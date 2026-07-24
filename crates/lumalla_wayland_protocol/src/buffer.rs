use std::{collections::VecDeque, io, mem, os::fd::RawFd, ptr};

use libc::{
    CMSG_DATA, CMSG_FIRSTHDR, CMSG_LEN, CMSG_NXTHDR, CMSG_SPACE, EAGAIN, EWOULDBLOCK, MSG_CTRUNC,
    MSG_NOSIGNAL, SCM_RIGHTS, SOL_SOCKET, close, cmsghdr, iovec, msghdr, recvmsg, sendmsg,
};
use log::error;

use crate::{ObjectId, Opcode};

#[derive(Debug)]
pub struct MessageHeader {
    pub object_id: ObjectId,
    pub size: u16,
    pub opcode: Opcode,
}

const HEADER_SIZE: usize = 8;
const MAX_MESSAGE_SIZE: usize = u16::MAX as usize;
const BUFFER_SIZE: usize = MAX_MESSAGE_SIZE * 2;
type Buffer = [u8; BUFFER_SIZE];
const MAX_FDS_IN_CMSG: usize = 253;
const CMSG_BUFFER_SIZE: usize =
    unsafe { CMSG_SPACE((MAX_FDS_IN_CMSG * mem::size_of::<RawFd>()) as u32) as usize };
const CMSG_BUFFER_WORDS: usize = CMSG_BUFFER_SIZE.div_ceil(mem::size_of::<usize>());
type CmsgBuffer = [usize; CMSG_BUFFER_WORDS];
const MAX_STRING_LENGTH: usize = 1_024 * 2;
const MAX_ARRAY_LENGTH: usize = MAX_STRING_LENGTH;

#[derive(Debug)]
pub struct Reader {
    fd: RawFd,
    buffer: Box<Buffer>,
    bytes_in_buffer: usize,
    current_buffer_offset: usize,
    fds: VecDeque<RawFd>,
    cmsg_buffer: Box<CmsgBuffer>,
}

#[derive(Debug, PartialEq)]
pub enum ReadResult {
    ReadData,
    NoMoreData,
    EndOfStream,
}

impl Reader {
    pub(crate) fn new(stream_fd: RawFd) -> Self {
        Self {
            fd: stream_fd,
            buffer: unsafe { Box::new_uninit().assume_init() },
            bytes_in_buffer: 0,
            current_buffer_offset: 0,
            fds: VecDeque::with_capacity(MAX_FDS_IN_CMSG),
            cmsg_buffer: unsafe { Box::new_uninit().assume_init() },
        }
    }

    #[must_use]
    pub fn read(&mut self) -> ReadResult {
        if self.current_buffer_offset > 0 {
            self.buffer
                .copy_within(self.current_buffer_offset..self.bytes_in_buffer, 0);
            self.bytes_in_buffer -= self.current_buffer_offset;
            self.current_buffer_offset = 0;
        }
        if self.bytes_in_buffer == self.buffer.len() {
            error!("Wayland receive buffer is full");
            return ReadResult::EndOfStream;
        }

        let usable_buffer = &mut self.buffer[self.bytes_in_buffer..];
        let mut iov = iovec {
            iov_base: usable_buffer.as_mut_ptr() as *mut _,
            iov_len: usable_buffer.len(),
        };
        let mut msghdr = msghdr {
            msg_name: ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut iov as *mut _,
            msg_iovlen: 1,
            msg_control: self.cmsg_buffer.as_mut_ptr().cast(),
            msg_controllen: mem::size_of_val(self.cmsg_buffer.as_ref()),
            msg_flags: 0,
        };
        let received_bytes = unsafe { recvmsg(self.fd, &mut msghdr as *mut _, 0) };
        match received_bytes {
            0 => ReadResult::EndOfStream,
            -1 => match unsafe { *libc::__errno_location() } {
                #[allow(unreachable_patterns)] // On some platforms these may have different values
                EWOULDBLOCK | EAGAIN => ReadResult::NoMoreData,
                err => {
                    error!("Error reading from socket: {}", err);
                    ReadResult::EndOfStream
                }
            },
            _ => {
                self.bytes_in_buffer += received_bytes as usize;
                let first_new_fd = self.fds.len();
                unsafe {
                    let mut cmsg = CMSG_FIRSTHDR(&msghdr);
                    while !cmsg.is_null() {
                        if (*cmsg).cmsg_level == SOL_SOCKET && (*cmsg).cmsg_type == SCM_RIGHTS {
                            if (*cmsg).cmsg_len < CMSG_LEN(0) as usize {
                                error!("Received malformed Wayland ancillary data");
                                return ReadResult::EndOfStream;
                            }
                            let data_ptr = CMSG_DATA(cmsg) as *const RawFd;
                            let data_len = (*cmsg).cmsg_len - CMSG_LEN(0) as usize;
                            let fd_count = data_len / mem::size_of::<RawFd>();

                            let fds = std::slice::from_raw_parts(data_ptr, fd_count);
                            for &fd in fds {
                                self.fds.push_back(fd);
                            }
                        }
                        cmsg = CMSG_NXTHDR(&msghdr, cmsg);
                    }
                }
                if msghdr.msg_flags & MSG_CTRUNC != 0 {
                    error!("Wayland ancillary data was truncated");
                    while self.fds.len() > first_new_fd {
                        if let Some(fd) = self.fds.pop_back() {
                            unsafe {
                                close(fd);
                            }
                        }
                    }
                    return ReadResult::EndOfStream;
                }
                ReadResult::ReadData
            }
        }
    }

    pub fn next(&mut self) -> anyhow::Result<Option<(MessageHeader, &[u8], &mut VecDeque<RawFd>)>> {
        let available_bytes = self.bytes_in_buffer - self.current_buffer_offset;
        if available_bytes < HEADER_SIZE {
            return Ok(None);
        }

        let start = self.current_buffer_offset;
        let object_id = u32::from_ne_bytes(self.buffer[start..start + 4].try_into().unwrap());
        let object_id = ObjectId::new(
            std::num::NonZeroU32::new(object_id)
                .ok_or_else(|| anyhow::anyhow!("Wayland message has object ID zero"))?,
        );
        let opcode = u16::from_ne_bytes(self.buffer[start + 4..start + 6].try_into().unwrap());
        let size = u16::from_ne_bytes(self.buffer[start + 6..start + 8].try_into().unwrap());
        let size = size as usize;
        anyhow::ensure!(
            (HEADER_SIZE..=MAX_MESSAGE_SIZE).contains(&size) && size.is_multiple_of(4),
            "Invalid Wayland message size {size}"
        );
        if size > available_bytes {
            return Ok(None);
        }

        let header = MessageHeader {
            object_id,
            size: size as u16,
            opcode,
        };
        Ok(Some((
            header,
            &self.buffer[start + HEADER_SIZE..start + size],
            &mut self.fds,
        )))
    }

    pub fn message_handled(&mut self, message_size: usize) {
        self.current_buffer_offset += message_size;
        if self.bytes_in_buffer == self.current_buffer_offset {
            // If we've read all the data in the buffer, reset the offset
            self.current_buffer_offset = 0;
            self.bytes_in_buffer = 0;
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        for fd in self.fds.drain(..) {
            unsafe {
                close(fd);
            }
        }
    }
}

#[derive(Debug)]
pub struct Writer {
    fd: RawFd,
    buffer: Box<Buffer>,
    bytes_in_buffer: usize,
    fds: Vec<RawFd>,
    message_start_index: usize,
    message_length_index: usize,
    last_err: Option<anyhow::Error>,
}

// TODO: change the writer to unchecked copies
impl Writer {
    pub fn new(fd: RawFd) -> Self {
        Self {
            fd,
            buffer: unsafe { Box::new_uninit().assume_init() },
            bytes_in_buffer: 0,
            fds: Vec::new(),
            message_start_index: 0,
            message_length_index: 0,
            last_err: None,
        }
    }

    pub fn last_err(&mut self) -> Option<anyhow::Error> {
        self.last_err.take()
    }

    #[inline]
    pub fn start_message(&mut self, object_id: ObjectId, opcode: Opcode) {
        if self.last_err.is_some() {
            return;
        }
        if let Err(err) = self.flush_if_needed() {
            self.last_err = Some(err);
            return;
        }
        self.message_start_index = self.bytes_in_buffer;
        self.write_u32(object_id.get());
        self.write_u16(opcode);
        self.message_length_index = self.bytes_in_buffer;
        self.write_u16(0);
    }

    #[inline]
    pub fn write_message_length(&mut self) {
        let message_length = self.bytes_in_buffer - self.message_start_index;
        if message_length > MAX_MESSAGE_SIZE || !message_length.is_multiple_of(4) {
            self.last_err = Some(anyhow::anyhow!(
                "Invalid outgoing Wayland message size {message_length}"
            ));
            return;
        }
        let index = self.message_length_index;
        self.buffer[index..index + mem::size_of::<u16>()]
            .copy_from_slice(&(message_length as u16).to_ne_bytes());
    }

    #[inline]
    pub fn write_u16(&mut self, value: u16) {
        self.buffer[self.bytes_in_buffer..self.bytes_in_buffer + mem::size_of::<u16>()]
            .copy_from_slice(&value.to_ne_bytes());
        self.bytes_in_buffer += mem::size_of::<u16>();
    }

    #[inline]
    pub fn write_i32(&mut self, value: i32) {
        self.buffer[self.bytes_in_buffer..self.bytes_in_buffer + mem::size_of::<i32>()]
            .copy_from_slice(&value.to_ne_bytes());
        self.bytes_in_buffer += mem::size_of::<i32>();
    }

    #[inline]
    pub fn write_u32(&mut self, value: u32) {
        self.buffer[self.bytes_in_buffer..self.bytes_in_buffer + mem::size_of::<u32>()]
            .copy_from_slice(&value.to_ne_bytes());
        self.bytes_in_buffer += mem::size_of::<u32>();
    }

    #[inline]
    pub fn write_fixed(&mut self, value: f32) {
        let fixed = f32_to_fixed(value);
        self.buffer[self.bytes_in_buffer..self.bytes_in_buffer + mem::size_of::<i32>()]
            .copy_from_slice(&fixed.to_ne_bytes());
        self.bytes_in_buffer += mem::size_of::<f32>();
    }

    #[inline]
    pub fn write_str(&mut self, value: &str) {
        let bytes = value.as_bytes();
        if bytes.len() + 1 > MAX_STRING_LENGTH {
            self.last_err = Some(anyhow::anyhow!("Wayland string is too long"));
            return;
        }
        let len = bytes.len() + 1;
        let len_index_start = self.bytes_in_buffer;
        let len_index_end = self.bytes_in_buffer + mem::size_of::<u32>();
        self.buffer[len_index_start..len_index_end].copy_from_slice(&(len as u32).to_ne_bytes());
        let str_index_start = len_index_end;
        let str_index_end = str_index_start + bytes.len();
        self.buffer[str_index_start..str_index_end].copy_from_slice(bytes);
        self.buffer[str_index_end] = 0;
        let padded_len = (len + 3) & !3;
        self.buffer[str_index_end + 1..str_index_start + padded_len].fill(0);
        self.bytes_in_buffer = str_index_start + padded_len;
    }

    #[inline]
    pub fn write_optional_str(&mut self, value: Option<&str>) {
        if let Some(value) = value {
            self.write_str(value);
        } else {
            self.write_u32(0);
        }
    }

    #[inline]
    pub fn write_array(&mut self, array: &[u8]) {
        if array.len() > MAX_ARRAY_LENGTH {
            self.last_err = Some(anyhow::anyhow!("Wayland array is too long"));
            return;
        }
        let bytes = array;
        let len = bytes.len();
        let len_index_start = self.bytes_in_buffer;
        let len_index_end = self.bytes_in_buffer + mem::size_of::<u32>();
        self.buffer[len_index_start..len_index_end].copy_from_slice(&(len as u32).to_ne_bytes());
        let val_index_start = len_index_end;
        let val_index_end = val_index_start + len;
        self.buffer[val_index_start..val_index_end].copy_from_slice(bytes);
        let padded_len = (len + 3) & !3;
        self.buffer[val_index_end..val_index_start + padded_len].fill(0);
        self.bytes_in_buffer = val_index_start + padded_len;
    }

    #[inline]
    pub fn write_fd(&mut self, fd: RawFd) {
        if self.fds.len() == MAX_FDS_IN_CMSG {
            self.last_err = Some(anyhow::anyhow!(
                "Too many file descriptors in Wayland message"
            ));
            return;
        }
        self.fds.push(fd);
    }

    #[inline]
    pub fn flush_if_needed(&mut self) -> anyhow::Result<()> {
        if self.bytes_in_buffer >= MAX_MESSAGE_SIZE || self.fds.len() >= 100 {
            self.flush()
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn flush(&mut self) -> anyhow::Result<()> {
        if self.bytes_in_buffer == 0 {
            return Ok(());
        }

        while self.bytes_in_buffer > 0 {
            let mut iov = iovec {
                iov_base: self.buffer.as_mut_ptr().cast(),
                iov_len: self.bytes_in_buffer,
            };
            let mut control = [0usize; CMSG_BUFFER_WORDS];
            let (control_ptr, control_len) = if self.fds.is_empty() {
                (ptr::null_mut(), 0)
            } else {
                let payload_len = self.fds.len() * mem::size_of::<RawFd>();
                let cmsg = control.as_mut_ptr().cast::<cmsghdr>();
                unsafe {
                    (*cmsg).cmsg_level = SOL_SOCKET;
                    (*cmsg).cmsg_type = SCM_RIGHTS;
                    (*cmsg).cmsg_len = CMSG_LEN(payload_len as u32) as usize;
                    ptr::copy_nonoverlapping(
                        self.fds.as_ptr().cast::<u8>(),
                        CMSG_DATA(cmsg),
                        payload_len,
                    );
                }
                (control.as_mut_ptr().cast(), unsafe {
                    CMSG_SPACE(payload_len as u32) as usize
                })
            };
            let msg = msghdr {
                msg_name: ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: &mut iov,
                msg_iovlen: 1,
                msg_control: control_ptr,
                msg_controllen: control_len,
                msg_flags: 0,
            };
            let result = unsafe { sendmsg(self.fd, &msg, MSG_NOSIGNAL) };
            if result < 0 {
                let err = io::Error::last_os_error();
                if err
                    .raw_os_error()
                    .is_some_and(|code| code == EWOULDBLOCK || code == EAGAIN)
                {
                    return Ok(());
                }
                return Err(err.into());
            }
            if result == 0 {
                anyhow::bail!("Wayland socket write returned zero");
            }

            let written = result as usize;
            self.fds.clear();
            self.buffer.copy_within(written..self.bytes_in_buffer, 0);
            self.bytes_in_buffer -= written;
        }
        Ok(())
    }

    pub fn has_pending_output(&self) -> bool {
        self.bytes_in_buffer != 0
    }
}

#[inline]
pub fn fixed_to_f32(value: i32) -> f32 {
    value as f32 / 256.0
}

#[inline]
pub fn f32_to_fixed(value: f32) -> i32 {
    (value * 256.0).round() as i32
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        num::NonZeroU32,
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    use super::*;

    #[test]
    fn read_and_write_unix_stream() {
        let socket = UnixStream::pair().unwrap();
        let mut reader = Reader::new(socket.0.as_raw_fd());
        let mut writer = Writer::new(socket.1.as_raw_fd());

        let str = "Hello, world!";
        let array = [1, 2, 3, 4, 5];
        writer.start_message(ObjectId::new(NonZeroU32::new(1).unwrap()), 2);
        writer.write_i32(-2);
        writer.write_u32(3);
        writer.write_fixed(4.3);
        writer.write_str(str);
        writer.write_array(&array);
        writer.write_fd(socket.1.as_raw_fd());
        writer.write_message_length();
        writer.flush().unwrap();

        assert_eq!(reader.read(), ReadResult::ReadData);
        let (header, data, fds) = reader.next().unwrap().unwrap();
        assert_eq!(header.object_id.get(), 1);
        assert_eq!(header.opcode, 2);
        assert_eq!(data.len(), 44);
        assert_eq!(header.size as usize, data.len() + HEADER_SIZE);
        let start_index = 0;
        let end_index = start_index + mem::size_of::<i32>();
        assert_eq!(data[start_index..end_index], (-2i32).to_ne_bytes());
        let start_index = end_index;
        let end_index = start_index + mem::size_of::<u32>();
        assert_eq!(data[start_index..end_index], 3u32.to_ne_bytes());
        let start_index = end_index;
        let end_index = start_index + mem::size_of::<i32>();
        assert_eq!(
            data[start_index..end_index],
            (f32_to_fixed(4.3).to_ne_bytes())
        );
        let start_index = end_index;
        let end_index = start_index + mem::size_of::<u32>();
        assert_eq!(
            data[start_index..end_index],
            ((str.len() + 1) as u32).to_ne_bytes()
        );
        let start_index = end_index;
        let end_index = start_index + str.bytes().len();
        assert_eq!(&data[start_index..end_index], str.as_bytes());
        assert_eq!(data[end_index], 0);
        let start_index = end_index + 3;
        let end_index = start_index + mem::size_of::<u32>();
        assert_eq!(
            data[start_index..end_index],
            (array.len() as u32).to_ne_bytes()
        );
        let start_index = end_index;
        let end_index = start_index + array.len();
        assert_eq!(&data[start_index..end_index], array);
        assert_eq!(fds.len(), 1);
    }

    #[test]
    fn writer_matches_wayland_wire_format() {
        let (mut receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());

        writer.start_message(ObjectId::new(NonZeroU32::new(7).unwrap()), 3);
        writer.write_u32(0x1122_3344);
        writer.write_str("abc");
        writer.write_array(&[5, 6, 7]);
        writer.write_message_length();
        writer.flush().unwrap();

        let mut bytes = [0u8; 28];
        receiver.read_exact(&mut bytes).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&7u32.to_ne_bytes());
        expected.extend_from_slice(&3u16.to_ne_bytes());
        expected.extend_from_slice(&28u16.to_ne_bytes());
        expected.extend_from_slice(&0x1122_3344u32.to_ne_bytes());
        expected.extend_from_slice(&4u32.to_ne_bytes());
        expected.extend_from_slice(b"abc\0");
        expected.extend_from_slice(&3u32.to_ne_bytes());
        expected.extend_from_slice(&[5, 6, 7, 0]);
        assert_eq!(bytes.as_slice(), expected);
    }

    #[test]
    fn reader_preserves_partial_messages() {
        let (receiver, mut sender) = UnixStream::pair().unwrap();
        let mut reader = Reader::new(receiver.as_raw_fd());
        let mut message = Vec::new();
        message.extend_from_slice(&9u32.to_ne_bytes());
        message.extend_from_slice(&1u16.to_ne_bytes());
        message.extend_from_slice(&12u16.to_ne_bytes());
        message.extend_from_slice(&42u32.to_ne_bytes());

        sender.write_all(&message[..6]).unwrap();
        assert_eq!(reader.read(), ReadResult::ReadData);
        assert!(reader.next().unwrap().is_none());
        sender.write_all(&message[6..]).unwrap();
        assert_eq!(reader.read(), ReadResult::ReadData);

        let (header, data, _) = reader.next().unwrap().unwrap();
        assert_eq!(header.object_id.get(), 9);
        assert_eq!(header.opcode, 1);
        assert_eq!(header.size, 12);
        assert_eq!(data, 42u32.to_ne_bytes());
    }

    #[test]
    fn convert_f32_to_fixed_and_back() {
        let values = [0.0, 1.0, 8.8, 27.27, 255.0, 256.0, 257.0];
        for value in values {
            let fixed = f32_to_fixed(value);
            let back = fixed_to_f32(fixed);
            assert!((value - back).abs() < 0.001);
        }

        for value in values.iter().map(|v| -v) {
            let fixed = f32_to_fixed(value);
            let back = fixed_to_f32(fixed);
            assert!((value - back).abs() < 0.001);
        }
    }
}
