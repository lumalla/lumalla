use std::os::fd::{AsRawFd, OwnedFd, RawFd};

/// Sealed memfd containing a null-terminated xkb TEXT_V1 keymap.
#[derive(Debug)]
pub struct KeymapMemfd {
    fd: OwnedFd,
    size: u32,
}

impl KeymapMemfd {
    pub fn new(fd: OwnedFd, size: u32) -> Self {
        Self { fd, size }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}
