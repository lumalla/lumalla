use std::{collections::VecDeque, mem, os::fd::RawFd, ptr};

use libc::{
    CMSG_DATA, CMSG_FIRSTHDR, CMSG_LEN, CMSG_NXTHDR, EAGAIN, EWOULDBLOCK, MSG_NOSIGNAL, SCM_RIGHTS,
    SOL_SOCKET, cmsghdr, iovec, msghdr, recvmsg, sendmsg,
};
use log::{debug, error};

use crate::{ObjectId, Opcode};

#[derive(Debug)]
#[repr(C)]
pub struct MessageHeader {
    pub object_id: ObjectId,
    pub size: u16,
    pub opcode: Opcode,
}

const MAX_MESSAGE_SIZE: usize = u16::MAX as usize;
const BUFFER_SIZE: usize = MAX_MESSAGE_SIZE * 2;
type Buffer = [u8; BUFFER_SIZE];
const MAX_FDS_IN_CMSG: usize = 253;
type CmsgBuffer = [u8; mem::size_of::<cmsghdr>() + MAX_FDS_IN_CMSG * mem::size_of::<RawFd>()];
const MAX_STRING_LENGTH: usize = 1_024 * 2;

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
        let usable_buffer = &mut self.buffer[self.current_buffer_offset..];
        let mut iov = iovec {
            iov_base: usable_buffer.as_mut_ptr() as *mut _,
            iov_len: usable_buffer.len(),
        };
        let mut msghdr = msghdr {
            msg_name: ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut iov as *mut _,
            msg_iovlen: 1,
            msg_control: self.cmsg_buffer.as_mut_ptr() as *mut _,
            msg_controllen: self.cmsg_buffer.len(),
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
                unsafe {
                    let mut cmsg = CMSG_FIRSTHDR(&msghdr);
                    while !cmsg.is_null() {
                        if (*cmsg).cmsg_level == SOL_SOCKET && (*cmsg).cmsg_type == SCM_RIGHTS {
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
                ReadResult::ReadData
            }
        }
    }

    pub fn next(
        &mut self,
    ) -> anyhow::Result<Option<(&MessageHeader, &[u8], &mut VecDeque<RawFd>)>> {
        let available_bytes = self.bytes_in_buffer - self.current_buffer_offset;
        if available_bytes < size_of::<MessageHeader>() {
            return Ok(None);
        }

        // Need to check if the object ID is all zeros, as that means the message is invalid
        // and would violate the cast to the MessageHeader
        if self.buffer
            [self.current_buffer_offset..self.current_buffer_offset + size_of::<ObjectId>()]
            .iter()
            .all(|v| *v == 0)
        {
            anyhow::bail!("Invalid message received, where the object ID is all zeros");
        }

        let header = unsafe {
            &*(self.buffer.as_ptr().add(self.current_buffer_offset) as *const MessageHeader)
        };
        if header.size as usize > available_bytes {
            return Ok(None);
        }

        Ok(Some((
            header,
            &self.buffer[self.current_buffer_offset + size_of::<MessageHeader>()
                ..self.current_buffer_offset + header.size as usize],
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
        if self.bytes_in_buffer == self.buffer.len() {
            // The buffer is full, copy the rest to the front
            debug!("Buffer is too small and needs copying",);
            self.buffer.copy_within(self.current_buffer_offset.., 0);
            self.bytes_in_buffer -= self.current_buffer_offset;
            self.current_buffer_offset = 0;
        }
    }
}

#[derive(Debug)]
pub struct Writer {
    fd: RawFd,
    buffer: Box<Buffer>,
    bytes_in_buffer: usize,
    fds: Box<CmsgBuffer>,
    bytes_in_fds: usize,
    message_length_index: usize,
    last_err: Option<anyhow::Error>,
}

// TODO: change the writer to unchecked copies
impl Writer {
    pub fn new(fd: RawFd) -> Self {
        let mut writer = Self {
            fd,
            buffer: unsafe { Box::new_uninit().assume_init() },
            bytes_in_buffer: 0,
            fds: unsafe { Box::new_uninit().assume_init() },
            bytes_in_fds: mem::size_of::<cmsghdr>(),
            message_length_index: 0,
            last_err: None,
        };
        let cmsghdr = unsafe { &mut *(writer.fds.as_mut_ptr() as *mut cmsghdr) };
        cmsghdr.cmsg_level = SOL_SOCKET;
        cmsghdr.cmsg_type = SCM_RIGHTS;
        writer
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
        self.write_u32(object_id.get());
        self.message_length_index = self.bytes_in_buffer;
        self.write_u16(0);
        self.write_u16(opcode);
    }

    #[inline]
    pub fn write_message_length(&mut self) {
        let index = self.message_length_index;
        self.buffer[index..index + mem::size_of::<u16>()].copy_from_slice(
            &((self.bytes_in_buffer - index + mem::size_of::<ObjectId>()) as u16).to_ne_bytes(),
        );
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
        let bytes = &bytes[0..bytes.len().min(MAX_STRING_LENGTH)];
        let len = bytes.len();
        let len_index_start = self.bytes_in_buffer;
        let len_index_end = self.bytes_in_buffer + mem::size_of::<u32>();
        self.buffer[len_index_start..len_index_end].copy_from_slice(&(len as u32).to_ne_bytes());
        let str_index_start = len_index_end;
        let str_index_end = str_index_start + len;
        self.buffer[str_index_start..str_index_end].copy_from_slice(bytes);
        self.buffer[str_index_end] = 0;
        // Pad to 32-bit boundary
        self.bytes_in_buffer =
            str_index_end + (mem::size_of::<u32>() - len % mem::size_of::<u32>());
    }

    #[inline]
    pub fn write_fd(&mut self, fd: RawFd) {
        let fd_index_start = self.bytes_in_fds;
        let fd_index_end = fd_index_start + mem::size_of::<RawFd>();
        self.fds[fd_index_start..fd_index_end].copy_from_slice(&fd.to_ne_bytes());
        self.bytes_in_fds += size_of::<RawFd>();
    }

    #[inline]
    pub fn flush_if_needed(&mut self) -> anyhow::Result<()> {
        if self.bytes_in_buffer >= MAX_MESSAGE_SIZE ||
            // This is just a guard against sending too many FDs in a single message,
            // since a single message should not contain more than 100 FDs
            self.bytes_in_fds > self.fds.len() / 2
        {
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

        let cmsghdr = unsafe { &mut *(self.fds.as_mut_ptr() as *mut cmsghdr) };
        cmsghdr.cmsg_len = self.bytes_in_fds;
        let msg = msghdr {
            msg_name: ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut iovec {
                iov_base: self.buffer.as_mut_ptr() as *mut _,
                iov_len: self.bytes_in_buffer.min(MAX_MESSAGE_SIZE),
            },
            msg_iovlen: 1,
            msg_control: self.fds.as_mut_ptr() as *mut _,
            msg_controllen: self.bytes_in_fds,
            msg_flags: 0,
        };
        let result = unsafe { sendmsg(self.fd, &msg as *const _, MSG_NOSIGNAL) };
        if result < 0 {
            anyhow::bail!("Error sending message: {}", unsafe {
                *libc::__errno_location()
            });
        }

        if self.bytes_in_buffer > MAX_MESSAGE_SIZE {
            self.buffer.copy_within(MAX_MESSAGE_SIZE.., 0);
            self.bytes_in_buffer -= MAX_MESSAGE_SIZE;
        } else {
            self.bytes_in_buffer = 0;
        }
        self.bytes_in_fds = mem::size_of::<cmsghdr>();
        Ok(())
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
        writer.start_message(ObjectId::new(NonZeroU32::new(1).unwrap()), 2);
        writer.write_u16(10);
        writer.write_i32(-2);
        writer.write_u32(3);
        writer.write_fixed(4.3);
        writer.write_str(str);
        writer.write_fd(socket.1.as_raw_fd());
        writer.write_message_length();
        writer.flush().unwrap();

        assert_eq!(reader.read(), ReadResult::ReadData);
        let (header, data, fds) = reader.next().unwrap().unwrap();
        assert_eq!(header.object_id.get(), 1);
        assert_eq!(header.opcode, 2);
        assert_eq!(data.len(), 34);
        assert_eq!(
            header.size as usize,
            data.len() + mem::size_of::<MessageHeader>()
        );
        let start_index = 0;
        let end_index = mem::size_of::<u16>();
        assert_eq!(data[start_index..end_index], 10u16.to_ne_bytes());
        let start_index = end_index;
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
            (str.len() as u32).to_ne_bytes()
        );
        let start_index = end_index;
        let end_index = start_index + str.bytes().len();
        assert_eq!(&data[start_index..end_index], str.as_bytes());
        assert_eq!(fds.len(), 1);
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
