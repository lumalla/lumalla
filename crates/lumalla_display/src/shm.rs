use std::{
    collections::HashMap,
    ffi::c_void,
    fmt,
    mem::ManuallyDrop,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    rc::Rc,
};

use libc::{
    F_GET_SEALS, MAP_FAILED, MAP_PRIVATE, MAP_SHARED, PROT_READ, fcntl, fstat, mmap, munmap, stat,
};
use log::error;
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
pub struct ShmBufferSnapshot {
    pub pixels: Rc<Vec<u8>>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
}

/// Buffer-space rectangle used for damage-aware SHM captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmDamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Per-surface retained double-buffer for SHM pixel captures.
///
/// `front` is the last published frame (may still be held by the renderer via
/// `Arc`). `back` is scratch for the next commit; when `front` is uniquely owned
/// after publish it is recycled into `back`.
#[derive(Debug, Default)]
struct SurfaceShmBacking {
    front: Rc<Vec<u8>>,
    back: Vec<u8>,
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
}

#[derive(Debug, Default)]
pub struct ShmManager {
    pool_index: HashMap<ResourceKey, usize>,
    pools: Vec<Option<ShmPool>>,
    free_pool_indexes: Vec<usize>,
    buffers: HashMap<ResourceKey, ShmBuffer>,
    /// Retained CPU pixels keyed by wl_surface.
    surface_backing: HashMap<ResourceKey, SurfaceShmBacking>,
}

impl ShmManager {
    /// Create a pool, taking ownership of `fd` (closes it on all failure paths).
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
        // SAFETY: caller transfers ownership of a message SCM_RIGHTS fd.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if size <= 0 {
            close_owned_fd(fd);
            return Err(ShmError::new(
                ShmErrorKind::InvalidStride,
                "Shared-memory pool size must be positive",
            ));
        }
        let key = (client_id, object_id);
        if self.pool_index.contains_key(&key) {
            close_owned_fd(fd);
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

    /// Lookup SHM buffer dimensions without copying pixels.
    pub fn buffer_meta(
        &self,
        client_id: ClientId,
        buffer_id: ObjectId,
    ) -> Result<(usize, usize, usize, u32)> {
        let buffer = self.buffers.get(&(client_id, buffer_id)).ok_or_else(|| {
            ShmError::new(ShmErrorKind::InvalidObject, "Unknown shared-memory buffer")
        })?;
        Ok((
            buffer.width,
            buffer.height,
            buffer.width * 4,
            buffer.format,
        ))
    }

    /// Capture SHM pixels into a per-surface double-buffer.
    ///
    /// When `damage` is `Some` and the surface backing already matches this
    /// buffer's size/format, only the damaged region is read from the pool;
    /// undamaged pixels are copied from the previous front buffer. `damage:
    /// None` forces a full capture (also used on size/format changes).
    pub fn capture_surface_buffer(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        buffer_id: ObjectId,
        damage: Option<ShmDamageRect>,
    ) -> Result<ShmBufferSnapshot> {
        let buffer = *self.buffers.get(&(client_id, buffer_id)).ok_or_else(|| {
            ShmError::new(ShmErrorKind::InvalidObject, "Unknown shared-memory buffer")
        })?;
        let pool_index = buffer.pool_index;
        let packed_stride = buffer.width * 4;
        let needed = packed_stride
            .checked_mul(buffer.height)
            .ok_or_else(|| {
                ShmError::new(
                    ShmErrorKind::InvalidStride,
                    "Shared-memory buffer size overflows",
                )
            })?;

        let key = (client_id, surface_id);
        let mut backing = self.surface_backing.remove(&key).unwrap_or_default();
        let size_changed = backing.width != buffer.width
            || backing.height != buffer.height
            || backing.stride != packed_stride
            || backing.format != buffer.format
            || backing.front.len() != needed;
        let use_damage = match damage {
            Some(rect) if !size_changed && rect.width > 0 && rect.height > 0 => Some(rect),
            _ => None,
        };

        if backing.back.capacity() < needed {
            backing.back = Vec::with_capacity(needed);
        }
        backing.back.clear();

        let pool_bytes = self.pool(pool_index)?.bytes();
        if let Some(rect) = use_damage {
            backing.back.extend_from_slice(backing.front.as_slice());
            if backing.back.len() != needed {
                // Front was empty/mismatched despite size_changed check — fall back.
                backing.back.clear();
                copy_shm_rows(
                    &mut backing.back,
                    pool_bytes,
                    &buffer,
                    packed_stride,
                );
            } else {
                blit_shm_damage(
                    &mut backing.back,
                    packed_stride,
                    pool_bytes,
                    &buffer,
                    rect,
                );
            }
        } else {
            copy_shm_rows(
                &mut backing.back,
                pool_bytes,
                &buffer,
                packed_stride,
            );
        }

        debug_assert_eq!(backing.back.len(), needed);
        backing.width = buffer.width;
        backing.height = buffer.height;
        backing.stride = packed_stride;
        backing.format = buffer.format;

        let mut old_front = std::mem::replace(&mut backing.front, Rc::new(std::mem::take(&mut backing.back)));
        if let Some(unique) = Rc::get_mut(&mut old_front) {
            backing.back = std::mem::take(unique);
        } else {
            backing.back = Vec::with_capacity(needed);
        }

        let snapshot = ShmBufferSnapshot {
            pixels: Rc::clone(&backing.front),
            width: backing.width,
            height: backing.height,
            stride: backing.stride,
            format: backing.format,
        };
        self.surface_backing.insert(key, backing);
        Ok(snapshot)
    }

    /// Drop retained pixels for a surface (unmap / destroy).
    pub fn clear_surface_backing(&mut self, client_id: ClientId, surface_id: ObjectId) {
        self.surface_backing.remove(&(client_id, surface_id));
    }

    /// Full one-shot snapshot without updating per-surface backing (tests / debug).
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
        copy_shm_rows(&mut pixels, pool.bytes(), buffer, packed_stride);
        Ok(ShmBufferSnapshot {
            pixels: Rc::new(pixels),
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
        self.surface_backing
            .retain(|(owner, _), _| *owner != client_id);
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
    /// Wrapped so [`Drop`] can close without `OwnedFd`'s debug abort on EBADF.
    fd: ManuallyDrop<OwnedFd>,
    size: usize,
    address: *mut c_void,
    ref_count: usize,
}

impl ShmPool {
    fn new(fd: OwnedFd, size: usize) -> Result<Self> {
        if let Err(error) = ensure_file_size(&fd, size) {
            close_owned_fd(fd);
            return Err(error);
        }
        let address = match map_region(fd.as_raw_fd(), size) {
            Ok(address) => address,
            Err(error) => {
                close_owned_fd(fd);
                return Err(error);
            }
        };
        Ok(Self {
            fd: ManuallyDrop::new(fd),
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
            // Prefer quiet close(2): a double-closed fd must not abort the compositor.
            close_owned_fd(ManuallyDrop::take(&mut self.fd));
        }
    }
}

/// Close an [`OwnedFd`] without `debug_assert_fd_is_open` (which aborts on EBADF).
fn close_owned_fd(fd: OwnedFd) {
    let raw = fd.into_raw_fd();
    unsafe {
        libc::close(raw);
    }
}

#[derive(Debug, Clone, Copy)]
struct ShmBuffer {
    pool_index: usize,
    offset: usize,
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
}

fn copy_shm_rows(dst: &mut Vec<u8>, pool_bytes: &[u8], buffer: &ShmBuffer, packed_stride: usize) {
    dst.clear();
    dst.reserve(packed_stride * buffer.height);
    for row in 0..buffer.height {
        let start = buffer.offset + row * buffer.stride;
        dst.extend_from_slice(&pool_bytes[start..start + packed_stride]);
    }
}

fn blit_shm_damage(
    dst: &mut [u8],
    dst_stride: usize,
    pool_bytes: &[u8],
    buffer: &ShmBuffer,
    rect: ShmDamageRect,
) {
    let x0 = rect.x.max(0) as usize;
    let y0 = rect.y.max(0) as usize;
    let x1 = (rect.x.saturating_add(rect.width).max(0) as usize).min(buffer.width);
    let y1 = (rect.y.saturating_add(rect.height).max(0) as usize).min(buffer.height);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let row_bytes = (x1 - x0) * 4;
    for y in y0..y1 {
        let dst_start = y * dst_stride + x0 * 4;
        let src_start = buffer.offset + y * buffer.stride + x0 * 4;
        dst[dst_start..dst_start + row_bytes]
            .copy_from_slice(&pool_bytes[src_start..src_start + row_bytes]);
    }
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
        let errno = std::io::Error::last_os_error();
        let mut metadata = std::mem::MaybeUninit::<stat>::zeroed();
        let (mode, file_size, fstat_ok) = if unsafe { fstat(fd, metadata.as_mut_ptr()) } == 0 {
            let metadata = unsafe { metadata.assume_init() };
            (metadata.st_mode, metadata.st_size, true)
        } else {
            (0, -1, false)
        };
        let seals = unsafe { fcntl(fd, F_GET_SEALS) };
        let seals_desc = if seals < 0 {
            format!("errno={}", std::io::Error::last_os_error())
        } else {
            format!("{seals:#x}")
        };
        let path = std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|err| format!("<unreadable: {err}>"));
        // Diagnostic only: does MAP_PRIVATE succeed where MAP_SHARED failed?
        let private = unsafe { mmap(std::ptr::null_mut(), size, PROT_READ, MAP_PRIVATE, fd, 0) };
        let private_ok = private != MAP_FAILED;
        if private_ok {
            unsafe {
                munmap(private, size);
            }
        }
        let private_err = if private_ok {
            "ok".to_string()
        } else {
            format!("{}", std::io::Error::last_os_error())
        };
        error!(
            "wl_shm mmap(MAP_SHARED) failed: fd={fd} size={size} errno={} ({errno}); \
             fstat_ok={fstat_ok} mode={mode:#o} st_size={file_size} seals={seals_desc} \
             path={path}; MAP_PRIVATE probe={private_err}",
            errno.raw_os_error().unwrap_or(0),
        );
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
            snapshot.pixels.as_slice(),
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
                .pixels
                .as_slice(),
            [1, 2, 3, 4]
        );
        manager.delete_buffer(client(1), object(3));
        assert!(manager.pools.iter().all(Option::is_none));
    }

    #[test]
    fn capture_reuses_front_and_applies_damage() {
        let mut manager = ShmManager::default();
        // 2x2 buffer, stride 12 (4 bytes padding per row).
        let mut bytes = vec![0u8; 24];
        bytes[0..8].copy_from_slice(&[1, 1, 1, 1, 2, 2, 2, 2]);
        bytes[12..20].copy_from_slice(&[3, 3, 3, 3, 4, 4, 4, 4]);
        manager
            .create_pool(client(1), object(2), memory_file(&bytes, 24), 24)
            .unwrap();
        manager
            .create_buffer(
                client(1),
                object(2),
                object(3),
                0,
                2,
                2,
                12,
                WL_SHM_FORMAT_ARGB8888,
            )
            .unwrap();

        let first = manager
            .capture_surface_buffer(client(1), object(10), object(3), None)
            .unwrap();
        assert_eq!(
            first.pixels.as_slice(),
            [1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4]
        );
        let first_ptr = first.pixels.as_ptr();

        // New pool/buffer with only the top-left pixel changed.
        let mut updated = bytes.clone();
        updated[0..4].copy_from_slice(&[9, 9, 9, 9]);
        manager
            .create_pool(client(1), object(4), memory_file(&updated, 24), 24)
            .unwrap();
        manager
            .create_buffer(
                client(1),
                object(4),
                object(5),
                0,
                2,
                2,
                12,
                WL_SHM_FORMAT_ARGB8888,
            )
            .unwrap();

        // Keep `first` alive so the front Arc is shared (forces a second allocation).
        let second = manager
            .capture_surface_buffer(
                client(1),
                object(10),
                object(5),
                Some(ShmDamageRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                }),
            )
            .unwrap();
        assert_eq!(
            second.pixels.as_slice(),
            [9, 9, 9, 9, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4]
        );
        assert_ne!(second.pixels.as_ptr(), first_ptr);
        drop(first);

        let third = manager
            .capture_surface_buffer(client(1), object(10), object(5), None)
            .unwrap();
        assert_eq!(third.pixels.as_slice()[..4], [9, 9, 9, 9]);
        assert!(manager.surface_backing.contains_key(&(client(1), object(10))));
        manager.clear_surface_backing(client(1), object(10));
        assert!(!manager.surface_backing.contains_key(&(client(1), object(10))));
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
    }

    #[test]
    fn create_pool_with_non_mappable_fd_does_not_abort() {
        let mut manager = ShmManager::default();
        let mut fds = [0; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        // Keep fds[1] open so fds[0]'s number cannot be recycled by parallel tests
        // while create_pool inspects/closes it.
        let error = manager
            .create_pool(client(1), object(2), fds[0], 4)
            .unwrap_err();
        assert_eq!(error.kind(), ShmErrorKind::InvalidFd);
        assert!(manager.pools.iter().all(Option::is_none));
        assert!(manager.pool_index.is_empty());
        unsafe {
            libc::close(fds[1]);
        }
    }

    #[test]
    fn create_pool_rejects_non_positive_size_without_leaking_slot() {
        let mut manager = ShmManager::default();
        let fd = memory_file(&[1, 2, 3, 4], 4);
        let error = manager
            .create_pool(client(1), object(2), fd, 0)
            .unwrap_err();
        assert_eq!(error.kind(), ShmErrorKind::InvalidStride);
        assert!(manager.pools.iter().all(Option::is_none));
        assert!(manager.pool_index.is_empty());
    }
}
