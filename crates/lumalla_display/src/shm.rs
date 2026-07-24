use std::{
    collections::HashMap,
    ffi::c_void,
    fmt,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

use libc::{MAP_FAILED, MAP_SHARED, PROT_READ, fstat, mmap, munmap, stat};
use lumalla_wayland_protocol::{
    ClientId, ObjectId,
    protocols::wayland::{WL_SHM_FORMAT_ARGB8888, WL_SHM_FORMAT_XRGB8888},
};

type ResourceKey = (ClientId, ObjectId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmErrorKind {
    InvalidFd,
    InvalidFormat,
    InvalidStride,
    InvalidObject,
}

#[derive(Debug)]
pub struct ShmError {
    kind: ShmErrorKind,
    message: &'static str,
}

impl ShmError {
    fn new(kind: ShmErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub fn kind(&self) -> ShmErrorKind {
        self.kind
    }
}

impl fmt::Display for ShmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ShmError {}

type Result<T> = std::result::Result<T, ShmError>;

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ShmBufferSnapshot {
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
}

#[derive(Debug, Default)]
pub struct ShmManager {
    pool_index: HashMap<ResourceKey, usize>,
    pools: Vec<Option<ShmPool>>,
    free_pool_indexes: Vec<usize>,
    buffers: HashMap<ResourceKey, ShmBuffer>,
}

impl ShmManager {
    pub fn create_pool(
        &mut self,
        client_id: ClientId,
        object_id: ObjectId,
        fd: RawFd,
        size: i32,
    ) -> Result<()> {
        if fd < 0 {
            return Err(ShmError::new(
                ShmErrorKind::InvalidFd,
                "Missing shared-memory file descriptor",
            ));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if size <= 0 {
            return Err(ShmError::new(
                ShmErrorKind::InvalidStride,
                "Shared-memory pool size must be positive",
            ));
        }
        let key = (client_id, object_id);
        if self.pool_index.contains_key(&key) {
            return Err(ShmError::new(
                ShmErrorKind::InvalidObject,
                "Shared-memory pool already exists",
            ));
        }

        let pool = ShmPool::new(fd, size as usize)?;
        let index = if let Some(index) = self.free_pool_indexes.pop() {
            self.pools[index] = Some(pool);
            index
        } else {
            self.pools.push(Some(pool));
            self.pools.len() - 1
        };
        self.pool_index.insert(key, index);
        Ok(())
    }

    pub fn delete_pool(&mut self, client_id: ClientId, object_id: ObjectId) {
        if let Some(index) = self.pool_index.remove(&(client_id, object_id)) {
            self.release_pool(index);
        }
    }

    pub fn resize_pool(
        &mut self,
        client_id: ClientId,
        object_id: ObjectId,
        size: i32,
    ) -> Result<()> {
        let Some(&index) = self.pool_index.get(&(client_id, object_id)) else {
            return Err(ShmError::new(
                ShmErrorKind::InvalidObject,
                "Unknown shared-memory pool",
            ));
        };
        if size <= 0 {
            return Err(ShmError::new(
                ShmErrorKind::InvalidStride,
                "Shared-memory pool size must be positive",
            ));
        }
        self.pool_mut(index)?.resize(size as usize)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_buffer(
        &mut self,
        client_id: ClientId,
        pool_id: ObjectId,
        buffer_id: ObjectId,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) -> Result<()> {
        if !matches!(format, WL_SHM_FORMAT_ARGB8888 | WL_SHM_FORMAT_XRGB8888) {
            return Err(ShmError::new(
                ShmErrorKind::InvalidFormat,
                "Unsupported shared-memory buffer format",
            ));
        }
        if offset < 0 || width <= 0 || height <= 0 || stride <= 0 {
            return Err(ShmError::new(
                ShmErrorKind::InvalidStride,
                "Shared-memory buffer dimensions must be positive",
            ));
        }

        let offset = offset as usize;
        let width = width as usize;
        let height = height as usize;
        let stride = stride as usize;
        let row_bytes = width.checked_mul(4).ok_or_else(|| {
            ShmError::new(
                ShmErrorKind::InvalidStride,
                "Shared-memory buffer width overflows",
            )
        })?;
        if stride < row_bytes {
            return Err(ShmError::new(
                ShmErrorKind::InvalidStride,
                "Shared-memory buffer stride is too small",
            ));
        }
        let last_row = (height - 1).checked_mul(stride).ok_or_else(|| {
            ShmError::new(
                ShmErrorKind::InvalidStride,
                "Shared-memory buffer height overflows",
            )
        })?;
        let end = offset
            .checked_add(last_row)
            .and_then(|end| end.checked_add(row_bytes))
            .ok_or_else(|| {
                ShmError::new(
                    ShmErrorKind::InvalidStride,
                    "Shared-memory buffer range overflows",
                )
            })?;

        let Some(&pool_index) = self.pool_index.get(&(client_id, pool_id)) else {
            return Err(ShmError::new(
                ShmErrorKind::InvalidObject,
                "Unknown shared-memory pool",
            ));
        };
        if end > self.pool(pool_index)?.size {
            return Err(ShmError::new(
                ShmErrorKind::InvalidStride,
                "Shared-memory buffer exceeds its pool",
            ));
        }
        let key = (client_id, buffer_id);
        if self.buffers.contains_key(&key) {
            return Err(ShmError::new(
                ShmErrorKind::InvalidObject,
                "Shared-memory buffer already exists",
            ));
        }

        self.pool_mut(pool_index)?.ref_count += 1;
        self.buffers.insert(
            key,
            ShmBuffer {
                pool_index,
                offset,
                width,
                height,
                stride,
                format,
            },
        );
        Ok(())
    }

    pub fn delete_buffer(&mut self, client_id: ClientId, buffer_id: ObjectId) {
        if let Some(buffer) = self.buffers.remove(&(client_id, buffer_id)) {
            self.release_pool(buffer.pool_index);
        }
    }

    #[allow(dead_code)]
    pub fn snapshot_buffer(
        &self,
        client_id: ClientId,
        buffer_id: ObjectId,
    ) -> Result<ShmBufferSnapshot> {
        let buffer = self.buffers.get(&(client_id, buffer_id)).ok_or_else(|| {
            ShmError::new(ShmErrorKind::InvalidObject, "Unknown shared-memory buffer")
        })?;
        let pool = self.pool(buffer.pool_index)?;
        let packed_stride = buffer.width * 4;
        let mut pixels = Vec::with_capacity(packed_stride * buffer.height);
        let bytes = pool.bytes();
        for row in 0..buffer.height {
            let start = buffer.offset + row * buffer.stride;
            pixels.extend_from_slice(&bytes[start..start + packed_stride]);
        }
        Ok(ShmBufferSnapshot {
            pixels,
            width: buffer.width,
            height: buffer.height,
            stride: packed_stride,
            format: buffer.format,
        })
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        let buffers: Vec<ObjectId> = self
            .buffers
            .keys()
            .filter_map(|(owner, id)| (*owner == client_id).then_some(*id))
            .collect();
        for buffer in buffers {
            self.delete_buffer(client_id, buffer);
        }
        let pools: Vec<ObjectId> = self
            .pool_index
            .keys()
            .filter_map(|(owner, id)| (*owner == client_id).then_some(*id))
            .collect();
        for pool in pools {
            self.delete_pool(client_id, pool);
        }
    }

    fn pool(&self, index: usize) -> Result<&ShmPool> {
        self.pools
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                ShmError::new(
                    ShmErrorKind::InvalidObject,
                    "Shared-memory pool is no longer alive",
                )
            })
    }

    fn pool_mut(&mut self, index: usize) -> Result<&mut ShmPool> {
        self.pools
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or_else(|| {
                ShmError::new(
                    ShmErrorKind::InvalidObject,
                    "Shared-memory pool is no longer alive",
                )
            })
    }

    fn release_pool(&mut self, index: usize) {
        let should_free = if let Some(pool) = self.pools.get_mut(index).and_then(Option::as_mut) {
            debug_assert!(pool.ref_count > 0);
            pool.ref_count -= 1;
            pool.ref_count == 0
        } else {
            false
        };
        if should_free {
            self.pools[index] = None;
            self.free_pool_indexes.push(index);
        }
    }
}

#[derive(Debug)]
struct ShmPool {
    fd: OwnedFd,
    size: usize,
    address: *mut c_void,
    ref_count: usize,
}

impl ShmPool {
    fn new(fd: OwnedFd, size: usize) -> Result<Self> {
        ensure_file_size(&fd, size)?;
        let address = map_region(fd.as_raw_fd(), size)?;
        Ok(Self {
            fd,
            size,
            address,
            ref_count: 1,
        })
    }

    fn resize(&mut self, size: usize) -> Result<()> {
        if size <= self.size {
            return Err(ShmError::new(
                ShmErrorKind::InvalidStride,
                "Shared-memory pools may only grow",
            ));
        }
        ensure_file_size(&self.fd, size)?;
        let address = map_region(self.fd.as_raw_fd(), size)?;
        unsafe {
            munmap(self.address, self.size);
        }
        self.address = address;
        self.size = size;
        Ok(())
    }

    #[allow(dead_code)]
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.address.cast(), self.size) }
    }
}

impl Drop for ShmPool {
    fn drop(&mut self) {
        unsafe {
            munmap(self.address, self.size);
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct ShmBuffer {
    pool_index: usize,
    offset: usize,
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
}

fn ensure_file_size(fd: &OwnedFd, size: usize) -> Result<()> {
    let mut metadata = std::mem::MaybeUninit::<stat>::zeroed();
    if unsafe { fstat(fd.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(ShmError::new(
            ShmErrorKind::InvalidFd,
            "Unable to inspect shared-memory file descriptor",
        ));
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_size < 0 || (metadata.st_size as u64) < size as u64 {
        return Err(ShmError::new(
            ShmErrorKind::InvalidFd,
            "Shared-memory file is smaller than the requested pool",
        ));
    }
    Ok(())
}

fn map_region(fd: RawFd, size: usize) -> Result<*mut c_void> {
    let address = unsafe { mmap(std::ptr::null_mut(), size, PROT_READ, MAP_SHARED, fd, 0) };
    if address == MAP_FAILED {
        return Err(ShmError::new(
            ShmErrorKind::InvalidFd,
            "Unable to map shared-memory file descriptor",
        ));
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{Seek, SeekFrom, Write},
        num::NonZeroU32,
        os::fd::{FromRawFd, IntoRawFd},
    };

    use super::*;

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn memory_file(bytes: &[u8], size: usize) -> RawFd {
        let fd = unsafe { libc::memfd_create(c"lumalla-shm-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size as u64).unwrap();
        file.write_all(bytes).unwrap();
        file.into_raw_fd()
    }

    #[test]
    fn snapshots_rows_without_stride_padding() {
        let mut manager = ShmManager::default();
        let mut bytes = vec![0u8; 32];
        bytes[4..12].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes[16..24].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
        manager
            .create_pool(client(1), object(2), memory_file(&bytes, 32), 32)
            .unwrap();
        manager
            .create_buffer(
                client(1),
                object(2),
                object(3),
                4,
                2,
                2,
                12,
                WL_SHM_FORMAT_ARGB8888,
            )
            .unwrap();

        let snapshot = manager.snapshot_buffer(client(1), object(3)).unwrap();
        assert_eq!(snapshot.width, 2);
        assert_eq!(snapshot.height, 2);
        assert_eq!(snapshot.stride, 8);
        assert_eq!(
            snapshot.pixels,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn buffer_keeps_destroyed_pool_alive() {
        let mut manager = ShmManager::default();
        manager
            .create_pool(client(1), object(2), memory_file(&[1, 2, 3, 4], 4), 4)
            .unwrap();
        manager
            .create_buffer(
                client(1),
                object(2),
                object(3),
                0,
                1,
                1,
                4,
                WL_SHM_FORMAT_XRGB8888,
            )
            .unwrap();

        manager.delete_pool(client(1), object(2));

        assert_eq!(
            manager
                .snapshot_buffer(client(1), object(3))
                .unwrap()
                .pixels,
            [1, 2, 3, 4]
        );
        manager.delete_buffer(client(1), object(3));
        assert!(manager.pools.iter().all(Option::is_none));
    }

    #[test]
    fn rejects_invalid_dimensions_formats_and_ranges() {
        let mut manager = ShmManager::default();
        manager
            .create_pool(client(1), object(2), memory_file(&[], 16), 16)
            .unwrap();

        for (offset, width, height, stride, format, kind) in [
            (0, 1, 1, 4, 99, ShmErrorKind::InvalidFormat),
            (
                0,
                0,
                1,
                4,
                WL_SHM_FORMAT_XRGB8888,
                ShmErrorKind::InvalidStride,
            ),
            (
                0,
                2,
                1,
                4,
                WL_SHM_FORMAT_XRGB8888,
                ShmErrorKind::InvalidStride,
            ),
            (
                12,
                2,
                1,
                8,
                WL_SHM_FORMAT_XRGB8888,
                ShmErrorKind::InvalidStride,
            ),
        ] {
            let error = manager
                .create_buffer(
                    client(1),
                    object(2),
                    object(3),
                    offset,
                    width,
                    height,
                    stride,
                    format,
                )
                .unwrap_err();
            assert_eq!(error.kind(), kind);
        }
    }

    #[test]
    fn resize_requires_a_larger_backing_file() {
        let mut manager = ShmManager::default();
        let fd = memory_file(&[], 8);
        let mut duplicate = unsafe { File::from_raw_fd(libc::dup(fd)) };
        manager.create_pool(client(1), object(2), fd, 8).unwrap();

        assert_eq!(
            manager
                .resize_pool(client(1), object(2), 16)
                .unwrap_err()
                .kind(),
            ShmErrorKind::InvalidFd
        );
        duplicate.set_len(16).unwrap();
        duplicate.seek(SeekFrom::Start(8)).unwrap();
        duplicate.write_all(&[0; 8]).unwrap();
        manager.resize_pool(client(1), object(2), 16).unwrap();
    }

    #[test]
    fn deleting_client_releases_all_resources() {
        let mut manager = ShmManager::default();
        let fd = memory_file(&[0; 4], 4);
        manager.create_pool(client(1), object(2), fd, 4).unwrap();
        manager
            .create_buffer(
                client(1),
                object(2),
                object(3),
                0,
                1,
                1,
                4,
                WL_SHM_FORMAT_ARGB8888,
            )
            .unwrap();

        manager.delete_client(client(1));

        assert!(manager.pool_index.is_empty());
        assert!(manager.buffers.is_empty());
        assert!(manager.pools.iter().all(Option::is_none));
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    }
}
