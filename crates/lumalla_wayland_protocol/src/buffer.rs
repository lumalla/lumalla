use std::{collections::VecDeque, mem, os::fd::RawFd, ptr};

use log::{debug, error};
use nix::{
    errno::Errno,
    libc::{
        CMSG_DATA, CMSG_FIRSTHDR, CMSG_LEN, CMSG_NXTHDR, SCM_RIGHTS, SOL_SOCKET, cmsghdr, iovec,
        msghdr, recvmsg, sendmsg,
    },
};

use crate::MessageHeader;

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

#[derive(Debug)]
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
        // TODO: check if there are any useful flags
        let received_bytes = unsafe { recvmsg(self.fd, &mut msghdr as *mut _, 0) };
        match received_bytes {
            0 => ReadResult::EndOfStream,
            -1 => match Errno::last() {
                Errno::EWOULDBLOCK => ReadResult::NoMoreData,
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

    pub fn next(&mut self) -> Option<(&MessageHeader, &[u8], &mut VecDeque<RawFd>)> {
        let available_bytes = self.bytes_in_buffer - self.current_buffer_offset;
        if available_bytes < size_of::<MessageHeader>() {
            return None;
        }

        let header = unsafe { &*(self.buffer.as_ptr() as *const MessageHeader) };
        if header.size as usize > available_bytes {
            return None;
        }

        Some((
            header,
            &self.buffer[self.current_buffer_offset + size_of::<MessageHeader>()
                ..self.current_buffer_offset + header.size as usize],
            &mut self.fds,
        ))
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
        };
        let cmsghdr = unsafe { &mut *(writer.fds.as_mut_ptr() as *mut cmsghdr) };
        cmsghdr.cmsg_level = SOL_SOCKET;
        cmsghdr.cmsg_type = SCM_RIGHTS;
        writer
    }

    pub fn write_i32(&mut self, value: i32) {
        self.buffer[self.bytes_in_buffer..self.bytes_in_buffer + mem::size_of::<i32>()]
            .copy_from_slice(&value.to_ne_bytes());
        self.bytes_in_buffer += mem::size_of::<i32>();
    }

    pub fn write_u32(&mut self, value: u32) {
        self.buffer[self.bytes_in_buffer..self.bytes_in_buffer + mem::size_of::<u32>()]
            .copy_from_slice(&value.to_ne_bytes());
        self.bytes_in_buffer += mem::size_of::<u32>();
    }

    pub fn write_fixed(&mut self, value: f32) {
        let fixed = f32_to_fixed(value);
        self.buffer[self.bytes_in_buffer..self.bytes_in_buffer + mem::size_of::<i32>()]
            .copy_from_slice(&fixed.to_ne_bytes());
        self.bytes_in_buffer += mem::size_of::<f32>();
    }

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

    pub fn write_fd(&mut self, fd: RawFd) {
        let fd_index_start = self.bytes_in_fds;
        let fd_index_end = fd_index_start + mem::size_of::<RawFd>();
        self.fds[fd_index_start..fd_index_end].copy_from_slice(&fd.to_ne_bytes());
        self.bytes_in_fds += size_of::<RawFd>();
    }

    #[must_use]
    pub fn flush_if_needed(&mut self) -> bool {
        if self.bytes_in_buffer >= MAX_MESSAGE_SIZE ||
            // This is just a guard against sending too many FDs in a single message,
            // since a single message should not contain more than 100 FDs
            self.bytes_in_fds > self.fds.len() / 2
        {
            self.flush()
        } else {
            true
        }
    }

    #[must_use]
    fn flush(&mut self) -> bool {
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
        // TODO: check if there are any useful flags
        let result = unsafe { sendmsg(self.fd, &msg as *const _, 0) };
        if result < 0 {
            error!("Error sending message: {}", Errno::last());
            return false;
        }

        if self.bytes_in_buffer > MAX_MESSAGE_SIZE {
            self.buffer.copy_within(MAX_MESSAGE_SIZE.., 0);
            self.bytes_in_buffer -= MAX_MESSAGE_SIZE;
        } else {
            self.bytes_in_buffer = 0;
        }
        self.bytes_in_fds = mem::size_of::<cmsghdr>();
        true
    }
}

pub fn fixed_to_f32(value: i32) -> f32 {
    value as f32 / 256.0
}

pub fn f32_to_fixed(value: f32) -> i32 {
    (value * 256.0).round() as i32
}
