//! io_uring event loop used by the compositor threads.

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::Arc,
    time::Duration,
};

use io_uring::{
    IoUring, cqueue, opcode,
    types::{CancelBuilder, Fd, TimeoutFlags, Timespec},
};
use libc::{EFD_CLOEXEC, EFD_NONBLOCK, POLLIN, POLLOUT, eventfd};

/// Cross-thread / accept / library poll token for the message channel waker.
pub const MESSAGE_CHANNEL_TOKEN: u64 = 0;

/// Kind of in-flight SQE, stored in the high byte of `user_data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpKind {
    Wake = 0,
    Timeout = 1,
    Cancel = 2,
    Accept = 3,
    Recv = 4,
    Send = 5,
    Poll = 6,
}

impl OpKind {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Wake),
            1 => Some(Self::Timeout),
            2 => Some(Self::Cancel),
            3 => Some(Self::Accept),
            4 => Some(Self::Recv),
            5 => Some(Self::Send),
            6 => Some(Self::Poll),
            _ => None,
        }
    }
}

/// Encode op kind + 56-bit id into io_uring `user_data`.
#[inline]
pub fn encode_user_data(kind: OpKind, id: u64) -> u64 {
    debug_assert!(id < (1u64 << 56));
    ((kind as u64) << 56) | (id & ((1u64 << 56) - 1))
}

/// Decode `user_data` into op kind and id.
#[inline]
pub fn decode_user_data(user_data: u64) -> (OpKind, u64) {
    let kind = OpKind::from_u8((user_data >> 56) as u8).unwrap_or(OpKind::Cancel);
    let id = user_data & ((1u64 << 56) - 1);
    (kind, id)
}

/// A drained completion.
#[derive(Debug, Clone, Copy)]
pub struct Completion {
    pub kind: OpKind,
    pub id: u64,
    pub result: i32,
    pub flags: u32,
}

impl Completion {
    pub fn more(&self) -> bool {
        cqueue::more(self.flags)
    }
}

/// Read/write interest for POLL_ADD on library fds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest {
    pub readable: bool,
    pub writable: bool,
}

impl Interest {
    pub const READABLE: Self = Self {
        readable: true,
        writable: false,
    };
    pub const WRITABLE: Self = Self {
        readable: false,
        writable: true,
    };

    pub fn add(self, other: Self) -> Self {
        Self {
            readable: self.readable || other.readable,
            writable: self.writable || other.writable,
        }
    }

    fn poll_flags(self) -> u32 {
        let mut flags = 0u32;
        if self.readable {
            flags |= POLLIN as u32;
        }
        if self.writable {
            flags |= POLLOUT as u32;
        }
        flags
    }
}

/// io_uring-backed event loop.
pub struct EventLoop {
    ring: IoUring,
    inflight: u32,
    /// Stable storage for the absolute timeout timespec while the SQE is in flight.
    timeout_ts: Box<Timespec>,
    timeout_armed: bool,
    timeout_generation: u64,
    /// Last absolute deadline submitted (`None` if cleared / never armed).
    timeout_deadline: Option<(u64, u32)>,
    waker_fd: OwnedFd,
    /// Buffer for the permanent eventfd Read SQE.
    waker_buf: Box<u64>,
    waker_armed: bool,
    accepting: bool,
}

impl EventLoop {
    pub fn new(entries: u32) -> io::Result<Self> {
        let ring = IoUring::builder().dontfork().build(entries)?;
        let raw = unsafe { eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let waker_fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut loop_ = Self {
            ring,
            inflight: 0,
            timeout_ts: Box::new(Timespec::new()),
            timeout_armed: false,
            timeout_generation: 0,
            timeout_deadline: None,
            waker_fd,
            waker_buf: Box::new(0),
            waker_armed: false,
            accepting: false,
        };
        loop_.arm_waker_read()?;
        Ok(loop_)
    }

    pub fn inflight(&self) -> u32 {
        self.inflight
    }

    pub fn waker_fd(&self) -> RawFd {
        self.waker_fd.as_raw_fd()
    }

    /// Create a cloneable waker that writes to this loop's eventfd.
    pub fn waker(&self) -> Waker {
        Waker {
            fd: self.waker_fd.as_raw_fd(),
        }
    }

    fn push(&mut self, entry: io_uring::squeue::Entry) -> io::Result<()> {
        // Ensure there is room; submit pending SQEs if the SQ is full.
        loop {
            unsafe {
                if self.ring.submission().push(&entry).is_ok() {
                    self.inflight += 1;
                    return Ok(());
                }
            }
            self.ring.submit()?;
        }
    }

    pub fn submit(&mut self) -> io::Result<usize> {
        self.ring.submit()
    }

    fn arm_waker_read(&mut self) -> io::Result<()> {
        if self.waker_armed {
            return Ok(());
        }
        let fd = self.waker_fd.as_raw_fd();
        let buf = self.waker_buf.as_mut() as *mut u64 as *mut u8;
        let entry = opcode::Read::new(Fd(fd), buf, 8)
            .build()
            .user_data(encode_user_data(OpKind::Wake, MESSAGE_CHANNEL_TOKEN));
        self.push(entry)?;
        self.waker_armed = true;
        Ok(())
    }

    /// Submit an accept SQE. `addr`/`addrlen` may be null for anonymous accepts.
    pub fn submit_accept(
        &mut self,
        listen_fd: RawFd,
        addr: *mut libc::sockaddr,
        addrlen: *mut libc::socklen_t,
        id: u64,
    ) -> io::Result<()> {
        if self.accepting {
            return Ok(());
        }
        let entry = opcode::Accept::new(Fd(listen_fd), addr, addrlen)
            .flags(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
            .build()
            .user_data(encode_user_data(OpKind::Accept, id));
        self.push(entry)?;
        self.accepting = true;
        Ok(())
    }

    pub fn mark_accept_done(&mut self) {
        self.accepting = false;
    }

    /// Submit RecvMsg. Caller must keep `msg` valid until the matching CQE.
    pub unsafe fn submit_recvmsg(
        &mut self,
        fd: RawFd,
        msg: *mut libc::msghdr,
        id: u64,
    ) -> io::Result<()> {
        let entry = opcode::RecvMsg::new(Fd(fd), msg)
            .build()
            .user_data(encode_user_data(OpKind::Recv, id));
        self.push(entry)
    }

    /// Submit SendMsg. Caller must keep `msg` valid until the matching CQE.
    pub unsafe fn submit_sendmsg(
        &mut self,
        fd: RawFd,
        msg: *const libc::msghdr,
        id: u64,
    ) -> io::Result<()> {
        let entry = opcode::SendMsg::new(Fd(fd), msg)
            .flags(libc::MSG_NOSIGNAL as u32)
            .build()
            .user_data(encode_user_data(OpKind::Send, id));
        self.push(entry)
    }

    pub fn submit_poll(&mut self, fd: RawFd, interest: Interest, id: u64) -> io::Result<()> {
        let entry = opcode::PollAdd::new(Fd(fd), interest.poll_flags())
            .multi(true)
            .build()
            .user_data(encode_user_data(OpKind::Poll, id));
        self.push(entry)
    }

    pub fn cancel_user_data(&mut self, target: u64) -> io::Result<()> {
        let entry = opcode::AsyncCancel::new(target)
            .build()
            .user_data(encode_user_data(
                OpKind::Cancel,
                target & ((1u64 << 56) - 1),
            ));
        self.push(entry)
    }

    pub fn cancel_poll(&mut self, poll_user_data: u64) -> io::Result<()> {
        let entry = opcode::PollRemove::new(poll_user_data)
            .build()
            .user_data(encode_user_data(
                OpKind::Cancel,
                poll_user_data & ((1u64 << 56) - 1),
            ));
        self.push(entry)
    }

    pub fn cancel_fd_all(&mut self, fd: RawFd) -> io::Result<()> {
        let entry = opcode::AsyncCancel2::new(CancelBuilder::fd(Fd(fd)).all())
            .build()
            .user_data(encode_user_data(OpKind::Cancel, fd as u64));
        self.push(entry)
    }

    pub fn cancel_all(&mut self) -> io::Result<()> {
        let entry = opcode::AsyncCancel2::new(CancelBuilder::any().all())
            .build()
            .user_data(encode_user_data(OpKind::Cancel, 0));
        self.push(entry)
    }

    /// Arm or replace an absolute CLOCK_MONOTONIC timeout.
    ///
    /// `Some(duration)` is interpreted as a deadline `duration` from now.
    pub fn set_absolute_timeout(&mut self, deadline: Option<Duration>) -> io::Result<()> {
        let Some(deadline) = deadline else {
            return self.clear_timeout_deadline();
        };
        let (abs_sec, abs_nsec) = monotonic_deadline_after(deadline)?;
        self.set_absolute_timeout_timespec(abs_sec, abs_nsec)
    }

    fn clear_timeout_deadline(&mut self) -> io::Result<()> {
        if self.timeout_armed {
            let old = encode_user_data(OpKind::Timeout, self.timeout_generation);
            let entry = opcode::TimeoutRemove::new(old)
                .build()
                .user_data(encode_user_data(OpKind::Cancel, self.timeout_generation));
            self.push(entry)?;
            self.timeout_armed = false;
        }
        self.timeout_deadline = None;
        Ok(())
    }

    /// Absolute timeout from a monotonic timespec (sec, nsec).
    pub fn set_absolute_timeout_timespec(&mut self, sec: u64, nsec: u32) -> io::Result<()> {
        if self.timeout_armed && self.timeout_deadline == Some((sec, nsec)) {
            return Ok(());
        }
        if self.timeout_armed {
            let old = encode_user_data(OpKind::Timeout, self.timeout_generation);
            let entry = opcode::TimeoutRemove::new(old)
                .build()
                .user_data(encode_user_data(OpKind::Cancel, self.timeout_generation));
            self.push(entry)?;
            self.timeout_armed = false;
        }
        self.timeout_generation = self.timeout_generation.wrapping_add(1);
        *self.timeout_ts = Timespec::new().sec(sec).nsec(nsec);
        let ts = self.timeout_ts.as_ref() as *const Timespec;
        let entry = opcode::Timeout::new(ts)
            .flags(TimeoutFlags::ABS)
            .build()
            .user_data(encode_user_data(OpKind::Timeout, self.timeout_generation));
        self.push(entry)?;
        self.timeout_armed = true;
        self.timeout_deadline = Some((sec, nsec));
        Ok(())
    }

    pub fn clear_timeout(&mut self) -> io::Result<()> {
        self.clear_timeout_deadline()
    }

    /// Submit pending SQEs and wait for at least one CQE.
    pub fn submit_and_wait(&mut self, want: usize) -> io::Result<usize> {
        self.ring.submit_and_wait(want)
    }

    /// Submit pending SQEs and drain ready CQEs without blocking.
    ///
    /// Filters the same cancel/timeout-cancel noise as [`Self::wait`].
    pub fn submit_and_drain(&mut self, out: &mut Vec<Completion>) -> io::Result<()> {
        out.clear();
        self.ring.submit()?;
        self.drain_completions(out);
        out.retain(|completion| match completion.kind {
            OpKind::Cancel => false,
            OpKind::Timeout if completion.result == -libc::ECANCELED => false,
            _ => true,
        });
        Ok(())
    }

    /// Drain all currently available completions.
    ///
    /// Multishot requests (e.g. `POLL_ADD` with `multi`) only decrement
    /// [`Self::inflight`] on the final CQE (no `IORING_CQE_F_MORE`).
    pub fn drain_completions(&mut self, out: &mut Vec<Completion>) {
        let mut cq = self.ring.completion();
        cq.sync();
        for cqe in &mut cq {
            let flags = cqe.flags();
            if !cqueue::more(flags) && self.inflight > 0 {
                self.inflight -= 1;
            }
            let (kind, id) = decode_user_data(cqe.user_data());
            match kind {
                OpKind::Wake => {
                    self.waker_armed = false;
                }
                OpKind::Timeout => {
                    if id == self.timeout_generation {
                        self.timeout_armed = false;
                        self.timeout_deadline = None;
                    }
                }
                OpKind::Accept => {
                    self.accepting = false;
                }
                _ => {}
            }
            out.push(Completion {
                kind,
                id,
                result: cqe.result(),
                flags,
            });
        }
    }

    /// Wait until at least one meaningful CQE is available, then drain all.
    ///
    /// Completions from replacing/canceling in-flight ops (`Cancel`, and
    /// `Timeout` with `-ECANCELED`) are ignored so callers that refresh an
    /// absolute timeout every lap do not busy-spin on `TimeoutRemove`.
    pub fn wait(&mut self, out: &mut Vec<Completion>) -> io::Result<()> {
        out.clear();
        loop {
            if out.is_empty() {
                self.drain_completions(out);
            }
            if out.is_empty() {
                self.ring.submit_and_wait(1)?;
                self.drain_completions(out);
            }

            out.retain(|completion| match completion.kind {
                OpKind::Cancel => false,
                OpKind::Timeout if completion.result == -libc::ECANCELED => false,
                _ => true,
            });

            if !out.is_empty() {
                return Ok(());
            }
            if self.inflight == 0 {
                return Ok(());
            }
        }
    }

    /// Re-arm the waker read after a Wake completion was handled.
    pub fn rearm_waker(&mut self) -> io::Result<()> {
        self.arm_waker_read()
    }

    /// Cancel everything and wait until `inflight == 0`.
    pub fn shutdown_drain(&mut self) -> io::Result<()> {
        self.cancel_all()?;
        let mut buf = Vec::with_capacity(64);
        while self.inflight > 0 {
            self.ring.submit_and_wait(1)?;
            self.drain_completions(&mut buf);
            buf.clear();
        }
        Ok(())
    }
}

/// Thread-safe wake handle (writes to an eventfd).
#[derive(Debug, Clone)]
pub struct Waker {
    fd: RawFd,
}

impl Waker {
    pub fn wake(&self) -> io::Result<()> {
        let buf = 1u64.to_ne_bytes();
        loop {
            let ret = unsafe { libc::write(self.fd, buf.as_ptr().cast(), buf.len()) };
            if ret >= 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // EAGAIN means the counter is saturated; the waiter will still wake.
            if err.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(());
            }
            return Err(err);
        }
    }
}

/// Shared waker for [`crate::MessageSender`].
pub type SharedWaker = Arc<Waker>;

/// Read CLOCK_MONOTONIC as (sec, nsec).
pub fn monotonic_now() -> io::Result<(u64, u32)> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((ts.tv_sec as u64, ts.tv_nsec as u32))
}

/// Add a relative duration to a monotonic timespec.
pub fn monotonic_deadline_after(duration: Duration) -> io::Result<(u64, u32)> {
    let (mut sec, mut nsec) = monotonic_now()?;
    sec = sec.saturating_add(duration.as_secs());
    let sum = nsec as u64 + duration.subsec_nanos() as u64;
    sec = sec.saturating_add(sum / 1_000_000_000);
    nsec = (sum % 1_000_000_000) as u32;
    Ok((sec, nsec))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn user_data_roundtrip() {
        let ud = encode_user_data(OpKind::Recv, 42);
        let (k, id) = decode_user_data(ud);
        assert_eq!(k, OpKind::Recv);
        assert_eq!(id, 42);
    }

    #[test]
    fn waker_wakes_event_loop() {
        let mut loop_ = EventLoop::new(32).unwrap();
        let waker = loop_.waker();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            waker.wake().unwrap();
        });
        let mut completions = Vec::new();
        loop_.wait(&mut completions).unwrap();
        assert!(
            completions
                .iter()
                .any(|c| c.kind == OpKind::Wake && c.result >= 0)
        );
    }

    #[test]
    fn cancel_all_drains_inflight() {
        let mut loop_ = EventLoop::new(32).unwrap();
        // Arm a far-future timeout so something is in flight besides the waker.
        let (sec, nsec) = monotonic_deadline_after(Duration::from_secs(3600)).unwrap();
        loop_.set_absolute_timeout_timespec(sec, nsec).unwrap();
        assert!(loop_.inflight() >= 2);
        loop_.shutdown_drain().unwrap();
        assert_eq!(loop_.inflight(), 0);
    }

    #[test]
    fn absolute_timeout_fires() {
        let mut loop_ = EventLoop::new(32).unwrap();
        let (sec, nsec) = monotonic_deadline_after(Duration::from_millis(30)).unwrap();
        loop_.set_absolute_timeout_timespec(sec, nsec).unwrap();
        let mut completions = Vec::new();
        loop_.wait(&mut completions).unwrap();
        // May get wake or timeout; keep waiting until timeout if needed.
        for _ in 0..10 {
            if completions
                .iter()
                .any(|c| c.kind == OpKind::Timeout && c.result == -libc::ETIME)
            {
                return;
            }
            completions.clear();
            loop_.wait(&mut completions).unwrap();
        }
        panic!("timeout did not fire: {completions:?}");
    }

    #[test]
    fn replacing_timeout_each_wait_does_not_busy_spin() {
        use std::time::Instant;

        let mut loop_ = EventLoop::new(32).unwrap();
        let waker = loop_.waker();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            waker.wake().unwrap();
        });

        let started = Instant::now();
        let mut completions = Vec::new();
        loop {
            // Same pattern that previously spun the D-Bus thread: refresh an
            // absolute timeout every lap, then wait.
            let (sec, nsec) = monotonic_deadline_after(Duration::from_secs(3600)).unwrap();
            loop_.set_absolute_timeout_timespec(sec, nsec).unwrap();
            loop_.wait(&mut completions).unwrap();
            if completions.iter().any(|c| c.kind == OpKind::Wake && c.result >= 0) {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "wait returned without wake too many times: {completions:?}"
            );
            completions.clear();
        }

        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "returned too quickly; likely busy-spinning on TimeoutRemove"
        );
    }
}
