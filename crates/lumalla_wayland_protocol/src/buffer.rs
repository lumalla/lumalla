use std::{
    collections::VecDeque,
    io, mem,
    os::fd::{FromRawFd, OwnedFd, RawFd},
    ptr, slice,
};

use libc::{
    CMSG_DATA, CMSG_FIRSTHDR, CMSG_LEN, CMSG_NXTHDR, CMSG_SPACE, EAGAIN, EWOULDBLOCK, MSG_CTRUNC,
    MSG_NOSIGNAL, SCM_RIGHTS, SOL_SOCKET, cmsghdr, iovec, msghdr, recvmsg, sendmsg,
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
const SEND_CHUNK_SIZE: usize = 4096;
const MAX_SEND_BUFFERS: usize = 5;
const MAX_FDS_IN_CMSG: usize = 253;
/// `wl_display` is always object id 1; its `error` event is opcode 0.
const WL_DISPLAY_OBJECT_ID: u32 = 1;
const WL_DISPLAY_ERROR_OPCODE: Opcode = 0;
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
    fds: VecDeque<OwnedFd>,
    cmsg_buffer: Box<CmsgBuffer>,
    /// Stable iov/msghdr for an in-flight RecvMsg SQE.
    recv_iov: iovec,
    recv_msghdr: msghdr,
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
            recv_iov: iovec {
                iov_base: ptr::null_mut(),
                iov_len: 0,
            },
            recv_msghdr: unsafe { mem::zeroed() },
        }
    }

    fn compact_if_needed(&mut self) {
        if self.current_buffer_offset > 0 {
            self.buffer
                .copy_within(self.current_buffer_offset..self.bytes_in_buffer, 0);
            self.bytes_in_buffer -= self.current_buffer_offset;
            self.current_buffer_offset = 0;
        }
    }

    /// True when the receive buffer cannot accept more bytes (client stalled or flooding).
    pub fn recv_buffer_full(&mut self) -> bool {
        self.compact_if_needed();
        self.bytes_in_buffer == self.buffer.len()
    }

    /// Build a stable `msghdr` for `IORING_OP_RECVMSG`. Valid until the matching CQE is applied.
    pub fn prepare_recv_msghdr(&mut self) -> Option<*mut msghdr> {
        if self.recv_buffer_full() {
            error!("Wayland receive buffer is full");
            return None;
        }
        let usable = &mut self.buffer[self.bytes_in_buffer..];
        self.recv_iov = iovec {
            iov_base: usable.as_mut_ptr().cast(),
            iov_len: usable.len(),
        };
        self.recv_msghdr = msghdr {
            msg_name: ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut self.recv_iov as *mut _,
            msg_iovlen: 1,
            msg_control: self.cmsg_buffer.as_mut_ptr().cast(),
            msg_controllen: mem::size_of_val(self.cmsg_buffer.as_ref()),
            msg_flags: 0,
        };
        Some(&mut self.recv_msghdr as *mut msghdr)
    }

    /// Apply a RecvMsg / recvmsg result code.
    pub fn apply_recv_result(&mut self, received_bytes: i32) -> ReadResult {
        match received_bytes {
            0 => ReadResult::EndOfStream,
            -1 => ReadResult::NoMoreData,
            n if n < 0 => {
                let err = -n;
                if err == EWOULDBLOCK || err == EAGAIN {
                    ReadResult::NoMoreData
                } else {
                    error!("Error reading from socket: {}", err);
                    ReadResult::EndOfStream
                }
            }
            n => {
                let received_bytes = n as usize;
                self.bytes_in_buffer += received_bytes;
                let first_new_fd = self.fds.len();
                let msghdr = &self.recv_msghdr;
                unsafe {
                    let mut cmsg = CMSG_FIRSTHDR(msghdr);
                    while !cmsg.is_null() {
                        if (*cmsg).cmsg_level == SOL_SOCKET && (*cmsg).cmsg_type == SCM_RIGHTS {
                            if (*cmsg).cmsg_len < CMSG_LEN(0) as usize {
                                error!("Received malformed Wayland ancillary data");
                                return ReadResult::EndOfStream;
                            }
                            let data_ptr = CMSG_DATA(cmsg) as *const RawFd;
                            let data_len = (*cmsg).cmsg_len - CMSG_LEN(0) as usize;
                            let fd_count = data_len / mem::size_of::<RawFd>();

                            let fds = slice::from_raw_parts(data_ptr, fd_count);
                            for &fd in fds {
                                self.fds.push_back(OwnedFd::from_raw_fd(fd));
                            }
                        }
                        cmsg = CMSG_NXTHDR(msghdr, cmsg);
                    }
                }
                if msghdr.msg_flags & MSG_CTRUNC != 0 {
                    error!("Wayland ancillary data was truncated");
                    while self.fds.len() > first_new_fd {
                        self.fds.pop_back();
                    }
                    return ReadResult::EndOfStream;
                }
                ReadResult::ReadData
            }
        }
    }

    /// Synchronous recv (tests / fallback).
    #[must_use]
    pub fn read(&mut self) -> ReadResult {
        let Some(msg) = self.prepare_recv_msghdr() else {
            return ReadResult::EndOfStream;
        };
        let received_bytes = unsafe { recvmsg(self.fd, msg, 0) };
        if received_bytes < 0 {
            let err = unsafe { *libc::__errno_location() };
            return self.apply_recv_result(-err);
        }
        self.apply_recv_result(received_bytes as i32)
    }

    pub fn next(
        &mut self,
    ) -> anyhow::Result<Option<(MessageHeader, &[u8], &mut VecDeque<OwnedFd>)>> {
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
            self.current_buffer_offset = 0;
            self.bytes_in_buffer = 0;
        }
    }
}

#[derive(Debug)]
struct SendChunk {
    data: Box<[u8; SEND_CHUNK_SIZE]>,
    len: usize,
    send_offset: usize,
    fds: Vec<RawFd>,
}

impl SendChunk {
    fn new() -> Self {
        Self {
            data: Box::new([0; SEND_CHUNK_SIZE]),
            len: 0,
            send_offset: 0,
            fds: Vec::new(),
        }
    }

    fn remaining(&self) -> usize {
        SEND_CHUNK_SIZE - self.len
    }
}

#[derive(Debug)]
pub struct Writer {
    fd: RawFd,
    active: SendChunk,
    queue: VecDeque<SendChunk>,
    in_flight: Option<SendChunk>,
    message_start_offset: usize,
    last_err: Option<anyhow::Error>,
    send_buffer_limit_exceeded: bool,
    /// Set when `wl_display.error` is queued; the client must be disconnected.
    protocol_error: bool,
    send_cmsg: Box<CmsgBuffer>,
    send_iov: iovec,
    send_msghdr: msghdr,
}

impl Writer {
    pub fn new(fd: RawFd) -> Self {
        Self {
            fd,
            active: SendChunk::new(),
            queue: VecDeque::new(),
            in_flight: None,
            message_start_offset: 0,
            last_err: None,
            send_buffer_limit_exceeded: false,
            protocol_error: false,
            send_cmsg: unsafe { Box::new_uninit().assume_init() },
            send_iov: iovec {
                iov_base: ptr::null_mut(),
                iov_len: 0,
            },
            send_msghdr: unsafe { mem::zeroed() },
        }
    }

    pub fn last_err(&mut self) -> Option<anyhow::Error> {
        self.last_err.take()
    }

    pub fn has_write_error(&self) -> bool {
        self.last_err.is_some()
    }

    pub fn send_buffer_limit_exceeded(&self) -> bool {
        self.send_buffer_limit_exceeded
    }

    pub fn protocol_error(&self) -> bool {
        self.protocol_error
    }

    /// True when the writer is in a fatal state and the client should be disconnected.
    pub fn should_disconnect(&self) -> bool {
        self.protocol_error || self.send_buffer_limit_exceeded || self.last_err.is_some()
    }

    pub fn send_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    fn buffers_in_use(&self) -> usize {
        1 + self.queue.len() + usize::from(self.in_flight.is_some())
    }

    fn set_send_buffer_limit_exceeded(&mut self) {
        self.send_buffer_limit_exceeded = true;
        self.last_err = Some(anyhow::anyhow!(
            "Client exceeded {MAX_SEND_BUFFERS} send buffers (unresponsive reader)"
        ));
    }

    fn rotate_active(&mut self) -> bool {
        if self.buffers_in_use() >= MAX_SEND_BUFFERS {
            self.set_send_buffer_limit_exceeded();
            return false;
        }
        if self.active.len > 0 {
            self.queue
                .push_back(mem::replace(&mut self.active, SendChunk::new()));
        }
        true
    }

    fn pending_stream_bytes(&self) -> usize {
        self.queue.iter().map(|chunk| chunk.len).sum::<usize>() + self.active.len
    }

    fn patch_stream_u16(&mut self, offset: usize, value: u16) {
        let mut remaining = offset;
        for chunk in &mut self.queue {
            if remaining < chunk.len {
                chunk.data[remaining..remaining + mem::size_of::<u16>()]
                    .copy_from_slice(&value.to_ne_bytes());
                return;
            }
            remaining -= chunk.len;
        }
        debug_assert!(remaining < self.active.len);
        self.active.data[remaining..remaining + mem::size_of::<u16>()]
            .copy_from_slice(&value.to_ne_bytes());
    }

    fn ensure_room(&mut self, need: usize) -> bool {
        if self.last_err.is_some() {
            return false;
        }
        if need > SEND_CHUNK_SIZE {
            self.last_err = Some(anyhow::anyhow!(
                "Wayland write of {need} bytes exceeds send chunk size {SEND_CHUNK_SIZE}"
            ));
            return false;
        }
        while need > self.active.remaining() {
            if !self.rotate_active() {
                return false;
            }
        }
        true
    }

    #[inline]
    pub fn start_message(&mut self, object_id: ObjectId, opcode: Opcode) {
        if self.last_err.is_some() {
            return;
        }
        // Per the Wayland spec, wl_display.error is fatal: after sending it the
        // compositor must close the client connection.
        if object_id.get() == WL_DISPLAY_OBJECT_ID && opcode == WL_DISPLAY_ERROR_OPCODE {
            self.protocol_error = true;
        }
        if let Err(err) = self.flush_if_needed() {
            self.last_err = Some(err);
            return;
        }
        self.message_start_offset = self.pending_stream_bytes();
        self.write_u32(object_id.get());
        self.write_u16(opcode);
        self.write_u16(0);
    }

    #[inline]
    pub fn write_message_length(&mut self) {
        let message_length = self.pending_stream_bytes() - self.message_start_offset;
        if message_length > MAX_MESSAGE_SIZE || !message_length.is_multiple_of(4) {
            self.last_err = Some(anyhow::anyhow!(
                "Invalid outgoing Wayland message size {message_length}"
            ));
            return;
        }
        self.patch_stream_u16(self.message_start_offset + 6, message_length as u16);
    }

    #[inline]
    pub fn write_u16(&mut self, value: u16) {
        let size = mem::size_of::<u16>();
        if !self.ensure_room(size) {
            return;
        }
        let start = self.active.len;
        self.active.data[start..start + size].copy_from_slice(&value.to_ne_bytes());
        self.active.len += size;
    }

    #[inline]
    pub fn write_i32(&mut self, value: i32) {
        let size = mem::size_of::<i32>();
        if !self.ensure_room(size) {
            return;
        }
        let start = self.active.len;
        self.active.data[start..start + size].copy_from_slice(&value.to_ne_bytes());
        self.active.len += size;
    }

    #[inline]
    pub fn write_u32(&mut self, value: u32) {
        let size = mem::size_of::<u32>();
        if !self.ensure_room(size) {
            return;
        }
        let start = self.active.len;
        self.active.data[start..start + size].copy_from_slice(&value.to_ne_bytes());
        self.active.len += size;
    }

    #[inline]
    pub fn write_fixed(&mut self, value: f32) {
        self.write_i32(f32_to_fixed(value));
    }

    #[inline]
    pub fn write_str(&mut self, value: &str) {
        let bytes = value.as_bytes();
        if bytes.len() + 1 > MAX_STRING_LENGTH {
            self.last_err = Some(anyhow::anyhow!("Wayland string is too long"));
            return;
        }
        let len = bytes.len() + 1;
        let padded_len = (len + 3) & !3;
        if !self.ensure_room(padded_len) {
            return;
        }
        let len_index_start = self.active.len;
        let len_index_end = len_index_start + mem::size_of::<u32>();
        self.active.data[len_index_start..len_index_end]
            .copy_from_slice(&(len as u32).to_ne_bytes());
        let str_index_start = len_index_end;
        let str_index_end = str_index_start + bytes.len();
        self.active.data[str_index_start..str_index_end].copy_from_slice(bytes);
        self.active.data[str_index_end] = 0;
        self.active.data[str_index_end + 1..str_index_start + padded_len].fill(0);
        self.active.len = str_index_start + padded_len;
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
        let len = array.len();
        if !self.ensure_room(mem::size_of::<u32>()) {
            return;
        }
        let len_index_start = self.active.len;
        let len_index_end = len_index_start + mem::size_of::<u32>();
        self.active.data[len_index_start..len_index_end]
            .copy_from_slice(&(len as u32).to_ne_bytes());
        self.active.len = len_index_end;

        let mut offset = 0;
        while offset < len {
            if self.active.remaining() == 0 && !self.rotate_active() {
                return;
            }
            let to_write = len.saturating_sub(offset).min(self.active.remaining());
            let start = self.active.len;
            self.active.data[start..start + to_write]
                .copy_from_slice(&array[offset..offset + to_write]);
            self.active.len += to_write;
            offset += to_write;
        }

        let padded_len = (len + 3) & !3;
        let pad_remaining = padded_len - len;
        let mut padded = 0;
        while padded < pad_remaining {
            if self.active.remaining() == 0 && !self.rotate_active() {
                return;
            }
            let to_write = pad_remaining
                .saturating_sub(padded)
                .min(self.active.remaining());
            self.active.data[self.active.len..self.active.len + to_write].fill(0);
            self.active.len += to_write;
            padded += to_write;
        }
    }

    #[inline]
    pub fn write_fd(&mut self, fd: RawFd) {
        if self.active.fds.len() == MAX_FDS_IN_CMSG {
            self.last_err = Some(anyhow::anyhow!(
                "Too many file descriptors in Wayland message"
            ));
            return;
        }
        self.active.fds.push(fd);
    }

    fn seal_active_to_queue(&mut self) -> bool {
        if self.active.len == 0 {
            return true;
        }
        if self.buffers_in_use() >= MAX_SEND_BUFFERS {
            self.set_send_buffer_limit_exceeded();
            return false;
        }
        self.queue
            .push_back(mem::replace(&mut self.active, SendChunk::new()));
        true
    }

    fn promote_next_send_chunk(&mut self) -> Option<()> {
        if self.in_flight.is_some() {
            return Some(());
        }
        if let Some(chunk) = self.queue.pop_front() {
            debug_assert!(chunk.len > 0, "queued send chunk must not be empty");
            self.in_flight = Some(chunk);
            return Some(());
        }
        if self.active.len == 0 {
            return None;
        }
        if !self.seal_active_to_queue() {
            return None;
        }
        self.in_flight = self.queue.pop_front();
        self.in_flight.as_ref()?;
        Some(())
    }

    fn build_send_msghdr(&mut self) {
        let chunk = self.in_flight.as_mut().expect("in_flight chunk missing");
        let remaining = chunk.len - chunk.send_offset;
        self.send_iov = iovec {
            iov_base: chunk.data[chunk.send_offset..].as_mut_ptr().cast(),
            iov_len: remaining,
        };
        let (control_ptr, control_len) = if chunk.fds.is_empty() {
            (ptr::null_mut(), 0)
        } else {
            let payload_len = chunk.fds.len() * mem::size_of::<RawFd>();
            let cmsg = self.send_cmsg.as_mut_ptr().cast::<cmsghdr>();
            unsafe {
                (*cmsg).cmsg_level = SOL_SOCKET;
                (*cmsg).cmsg_type = SCM_RIGHTS;
                (*cmsg).cmsg_len = CMSG_LEN(payload_len as u32) as usize;
                ptr::copy_nonoverlapping(
                    chunk.fds.as_ptr().cast::<u8>(),
                    CMSG_DATA(cmsg),
                    payload_len,
                );
            }
            (self.send_cmsg.as_mut_ptr().cast(), unsafe {
                CMSG_SPACE(payload_len as u32) as usize
            })
        };
        self.send_msghdr = msghdr {
            msg_name: ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut self.send_iov as *mut _,
            msg_iovlen: 1,
            msg_control: control_ptr,
            msg_controllen: control_len,
            msg_flags: 0,
        };
    }

    /// Return a stable msghdr for SendMsg, if there is output to send.
    pub fn prepare_send_msghdr(&mut self) -> Option<*const msghdr> {
        if self.in_flight.is_some() {
            return Some(&self.send_msghdr as *const msghdr);
        }
        self.promote_next_send_chunk()?;
        self.build_send_msghdr();
        Some(&self.send_msghdr as *const msghdr)
    }

    /// Apply a SendMsg result. Returns whether the same chunk still needs sending.
    pub fn apply_send_result(&mut self, result: i32) -> anyhow::Result<bool> {
        let Some(chunk) = self.in_flight.as_mut() else {
            return Ok(false);
        };
        if result < 0 {
            let err = -result;
            if err == EWOULDBLOCK || err == EAGAIN {
                return Ok(true);
            }
            self.in_flight = None;
            return Err(io::Error::from_raw_os_error(err).into());
        }
        if result == 0 {
            self.in_flight = None;
            anyhow::bail!("Wayland socket write returned zero");
        }
        let written = result as usize;
        chunk.fds.clear();
        chunk.send_offset += written;
        if chunk.send_offset >= chunk.len {
            self.in_flight = None;
            Ok(false)
        } else {
            self.build_send_msghdr();
            Ok(true)
        }
    }

    #[inline]
    pub fn flush_if_needed(&mut self) -> anyhow::Result<()> {
        if self.active.fds.len() >= 100 {
            if self.active.len > 0 && !self.rotate_active() {
                return Err(self
                    .last_err
                    .take()
                    .unwrap_or_else(|| anyhow::anyhow!("send buffer limit exceeded")));
            }
        }
        if self.active.remaining() < HEADER_SIZE && !self.rotate_active() {
            return Err(self
                .last_err
                .take()
                .unwrap_or_else(|| anyhow::anyhow!("send buffer limit exceeded")));
        }
        Ok(())
    }

    fn send_chunk_sync(&mut self, chunk: &mut SendChunk) -> anyhow::Result<()> {
        while chunk.send_offset < chunk.len {
            let remaining = chunk.len - chunk.send_offset;
            self.send_iov = iovec {
                iov_base: chunk.data[chunk.send_offset..].as_mut_ptr().cast(),
                iov_len: remaining,
            };
            let (control_ptr, control_len) = if chunk.fds.is_empty() {
                (ptr::null_mut(), 0)
            } else {
                let payload_len = chunk.fds.len() * mem::size_of::<RawFd>();
                let cmsg = self.send_cmsg.as_mut_ptr().cast::<cmsghdr>();
                unsafe {
                    (*cmsg).cmsg_level = SOL_SOCKET;
                    (*cmsg).cmsg_type = SCM_RIGHTS;
                    (*cmsg).cmsg_len = CMSG_LEN(payload_len as u32) as usize;
                    ptr::copy_nonoverlapping(
                        chunk.fds.as_ptr().cast::<u8>(),
                        CMSG_DATA(cmsg),
                        payload_len,
                    );
                }
                (self.send_cmsg.as_mut_ptr().cast(), unsafe {
                    CMSG_SPACE(payload_len as u32) as usize
                })
            };
            self.send_msghdr = msghdr {
                msg_name: ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: &mut self.send_iov as *mut _,
                msg_iovlen: 1,
                msg_control: control_ptr,
                msg_controllen: control_len,
                msg_flags: 0,
            };
            let result = unsafe { sendmsg(self.fd, &self.send_msghdr, MSG_NOSIGNAL) };
            if result < 0 {
                let err = io::Error::last_os_error();
                if err
                    .raw_os_error()
                    .is_some_and(|code| code == EWOULDBLOCK || code == EAGAIN)
                {
                    return Ok(());
                }
                chunk.fds.clear();
                return Err(err.into());
            }
            if result == 0 {
                anyhow::bail!("Wayland socket write returned zero");
            }
            chunk.fds.clear();
            chunk.send_offset += result as usize;
        }
        Ok(())
    }

    /// Synchronous flush (tests). If a uring send is in flight, returns an error.
    #[inline]
    pub fn flush(&mut self) -> anyhow::Result<()> {
        if self.in_flight.is_some() {
            anyhow::bail!("Cannot sync-flush while SendMsg is in flight");
        }
        if !self.seal_active_to_queue() {
            return Err(self
                .last_err
                .take()
                .unwrap_or_else(|| anyhow::anyhow!("send buffer limit exceeded")));
        }
        while let Some(mut chunk) = self.queue.pop_front() {
            self.send_chunk_sync(&mut chunk)?;
            if chunk.send_offset < chunk.len {
                self.queue.push_front(chunk);
                break;
            }
        }
        Ok(())
    }

    pub fn has_pending_output(&self) -> bool {
        self.in_flight.is_some() || !self.queue.is_empty() || self.active.len > 0
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

    #[test]
    fn writer_rotates_chunks_and_sends_sequentially() {
        let socket = UnixStream::pair().unwrap();
        let mut writer = Writer::new(socket.1.as_raw_fd());

        writer.start_message(ObjectId::new(NonZeroU32::new(1).unwrap()), 0);
        let words_in_first_chunk = (SEND_CHUNK_SIZE - HEADER_SIZE) / mem::size_of::<u32>();
        for _ in 0..words_in_first_chunk {
            writer.write_u32(0xABAB_ABAB);
        }
        for _ in 0..20 {
            writer.write_u32(0xCDCD_CDFD);
        }
        writer.write_message_length();
        assert!(writer.has_pending_output());

        let total =
            HEADER_SIZE + words_in_first_chunk * mem::size_of::<u32>() + 20 * mem::size_of::<u32>();
        let msg = writer.prepare_send_msghdr().unwrap();
        let first = unsafe { sendmsg(socket.1.as_raw_fd(), &*msg, MSG_NOSIGNAL) };
        assert_eq!(first as usize, SEND_CHUNK_SIZE);
        assert!(!writer.apply_send_result(first as i32).unwrap());

        let msg = writer.prepare_send_msghdr().unwrap();
        let second = unsafe { sendmsg(socket.1.as_raw_fd(), &*msg, MSG_NOSIGNAL) };
        assert_eq!(second as usize, total - SEND_CHUNK_SIZE);
        assert!(!writer.apply_send_result(second as i32).unwrap());
        assert!(!writer.has_pending_output());
    }

    #[test]
    fn writer_disconnects_at_send_buffer_limit() {
        let socket = UnixStream::pair().unwrap();
        let mut writer = Writer::new(socket.1.as_raw_fd());

        let fill_chunk = |writer: &mut Writer| {
            // Use a non-error opcode so this only exercises the send buffer limit.
            writer.start_message(ObjectId::new(NonZeroU32::new(1).unwrap()), 1);
            let words = (SEND_CHUNK_SIZE - HEADER_SIZE) / mem::size_of::<u32>();
            for _ in 0..words {
                writer.write_u32(0);
            }
            writer.write_message_length();
        };

        for _ in 0..MAX_SEND_BUFFERS {
            fill_chunk(&mut writer);
        }
        assert!(!writer.send_buffer_limit_exceeded());
        assert!(!writer.protocol_error());

        writer.start_message(ObjectId::new(NonZeroU32::new(1).unwrap()), 1);
        assert!(writer.send_buffer_limit_exceeded());
        assert!(writer.should_disconnect());
    }

    #[test]
    fn writer_resumes_partial_chunk_send() {
        let socket = UnixStream::pair().unwrap();
        let mut writer = Writer::new(socket.1.as_raw_fd());

        writer.start_message(ObjectId::new(NonZeroU32::new(1).unwrap()), 1);
        writer.write_array(&vec![1u8; 32]);
        writer.write_message_length();
        let _ = writer.prepare_send_msghdr().unwrap();

        let total = HEADER_SIZE + mem::size_of::<u32>() + 32;
        let partial = (HEADER_SIZE + 16) as i32;
        assert!(writer.apply_send_result(partial).unwrap());
        assert!(writer.send_in_flight());

        let msg = writer.prepare_send_msghdr().unwrap();
        let rest = unsafe { sendmsg(socket.1.as_raw_fd(), &*msg, MSG_NOSIGNAL) };
        assert_eq!(rest as usize, total - partial as usize);
        assert!(!writer.apply_send_result(rest as i32).unwrap());
    }

    #[test]
    fn writer_marks_protocol_error_on_display_error_event() {
        let socket = UnixStream::pair().unwrap();
        let mut writer = Writer::new(socket.1.as_raw_fd());
        assert!(!writer.protocol_error());
        writer.start_message(ObjectId::new(NonZeroU32::new(1).unwrap()), 0);
        writer.write_u32(2);
        writer.write_u32(1);
        writer.write_str("boom");
        writer.write_message_length();
        assert!(writer.protocol_error());
        assert!(writer.should_disconnect());
    }

    #[test]
    fn writer_does_not_mark_protocol_error_on_delete_id() {
        let socket = UnixStream::pair().unwrap();
        let mut writer = Writer::new(socket.1.as_raw_fd());
        writer.start_message(ObjectId::new(NonZeroU32::new(1).unwrap()), 1);
        writer.write_u32(2);
        writer.write_message_length();
        assert!(!writer.protocol_error());
        assert!(!writer.should_disconnect());
    }

    #[test]
    fn reader_reports_recv_buffer_full() {
        let socket = UnixStream::pair().unwrap();
        let mut reader = Reader::new(socket.0.as_raw_fd());
        reader.bytes_in_buffer = BUFFER_SIZE;
        assert!(reader.recv_buffer_full());
        assert!(reader.prepare_recv_msghdr().is_none());
    }
}
