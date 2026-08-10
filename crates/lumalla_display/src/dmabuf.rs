use std::{
    collections::HashMap,
    fmt,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

use libc::{MAP_FAILED, MAP_SHARED, PROT_READ, fstat, mmap, munmap, stat};
use lumalla_wayland_protocol::{
    ClientId, ObjectId,
    protocols::wayland::{WL_SHM_FORMAT_ARGB8888, WL_SHM_FORMAT_XRGB8888},
};

type ResourceKey = (ClientId, ObjectId);

/// DRM fourcc: XRGB8888 ('XR24').
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// DRM fourcc: ARGB8888 ('AR24').
pub const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// DRM_FORMAT_MOD_LINEAR
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufErrorKind {
    InvalidFd,
    InvalidFormat,
    InvalidDimensions,
    Incomplete,
    AlreadyUsed,
    PlaneIdx,
    PlaneSet,
    OutOfBounds,
    InvalidObject,
}

#[derive(Debug)]
pub struct DmabufError {
    kind: DmabufErrorKind,
    message: &'static str,
}

impl DmabufError {
    fn new(kind: DmabufErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub fn kind(&self) -> DmabufErrorKind {
        self.kind
    }
}

impl fmt::Display for DmabufError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for DmabufError {}

type Result<T> = std::result::Result<T, DmabufError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmabufSnapshot {
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
}

/// DMA-BUF plane metadata with a compositor-owned FD duplicate (no CPU copy).
#[derive(Debug)]
pub struct ExportedDmabuf {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub drm_fourcc: u32,
    pub wl_format: u32,
}

#[derive(Debug)]
struct Plane {
    fd: OwnedFd,
    offset: u32,
    stride: u32,
    modifier: u64,
}

#[derive(Debug)]
struct Params {
    planes: [Option<Plane>; 4],
    used: bool,
}

#[derive(Debug)]
struct DmabufBuffer {
    planes: Vec<Plane>,
    width: u32,
    height: u32,
    format: u32,
    #[allow(dead_code)]
    flags: u32,
}

#[derive(Debug, Default)]
pub struct DmabufManager {
    params: HashMap<ResourceKey, Params>,
    buffers: HashMap<ResourceKey, DmabufBuffer>,
    /// Advertised `(drm_fourcc, modifier)` pairs. Empty means built-in linear defaults.
    supported: Vec<(u32, u64)>,
}

impl DmabufManager {
    pub fn supported_formats(&self) -> &[(u32, u64)] {
        if self.supported.is_empty() {
            static DEFAULTS: &[(u32, u64)] = &[
                (DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR),
                (DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_LINEAR),
            ];
            DEFAULTS
        } else {
            &self.supported
        }
    }

    pub fn set_supported_formats(&mut self, formats: Vec<(u32, u64)>) {
        self.supported = formats;
    }

    fn supports_format_modifier(&self, format: u32, modifier: u64) -> bool {
        self.supported_formats()
            .iter()
            .any(|(fmt, m)| *fmt == format && *m == modifier)
    }

    fn supports_format(&self, format: u32) -> bool {
        self.supported_formats()
            .iter()
            .any(|(fmt, _)| *fmt == format)
    }

    pub fn create_params(&mut self, client_id: ClientId, object_id: ObjectId) -> Result<()> {
        let key = (client_id, object_id);
        if self.params.contains_key(&key) {
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidObject,
                "dmabuf params object already exists",
            ));
        }
        self.params.insert(
            key,
            Params {
                planes: [None, None, None, None],
                used: false,
            },
        );
        Ok(())
    }

    pub fn destroy_params(&mut self, client_id: ClientId, object_id: ObjectId) {
        self.params.remove(&(client_id, object_id));
    }

    pub fn add_plane(
        &mut self,
        client_id: ClientId,
        params_id: ObjectId,
        fd: RawFd,
        plane_idx: u32,
        offset: u32,
        stride: u32,
        modifier_hi: u32,
        modifier_lo: u32,
    ) -> Result<()> {
        if fd < 0 {
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidFd,
                "Missing dmabuf file descriptor",
            ));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let params = self
            .params
            .get_mut(&(client_id, params_id))
            .ok_or_else(|| {
                DmabufError::new(DmabufErrorKind::InvalidObject, "Unknown dmabuf params")
            })?;
        if params.used {
            return Err(DmabufError::new(
                DmabufErrorKind::AlreadyUsed,
                "dmabuf params already used",
            ));
        }
        if plane_idx as usize >= params.planes.len() {
            return Err(DmabufError::new(
                DmabufErrorKind::PlaneIdx,
                "plane index out of bounds",
            ));
        }
        if params.planes[plane_idx as usize].is_some() {
            return Err(DmabufError::new(
                DmabufErrorKind::PlaneSet,
                "plane index already set",
            ));
        }
        let modifier = ((modifier_hi as u64) << 32) | (modifier_lo as u64);
        params.planes[plane_idx as usize] = Some(Plane {
            fd,
            offset,
            stride,
            modifier,
        });
        Ok(())
    }

    pub fn create_immed(
        &mut self,
        client_id: ClientId,
        params_id: ObjectId,
        buffer_id: ObjectId,
        width: i32,
        height: i32,
        format: u32,
        flags: u32,
    ) -> Result<()> {
        if width <= 0 || height <= 0 {
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidDimensions,
                "Invalid dmabuf dimensions",
            ));
        }
        if !self.supports_format(format) {
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidFormat,
                "Unsupported dmabuf format",
            ));
        }
        let buffer_key = (client_id, buffer_id);
        if self.buffers.contains_key(&buffer_key) {
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidObject,
                "wl_buffer already exists",
            ));
        }
        let modifier = {
            let params = self.params.get(&(client_id, params_id)).ok_or_else(|| {
                DmabufError::new(DmabufErrorKind::InvalidObject, "Unknown dmabuf params")
            })?;
            if params.used {
                return Err(DmabufError::new(
                    DmabufErrorKind::AlreadyUsed,
                    "dmabuf params already used",
                ));
            }
            let Some(plane) = params.planes[0].as_ref() else {
                return Err(DmabufError::new(
                    DmabufErrorKind::Incomplete,
                    "Missing plane 0",
                ));
            };
            if params.planes.iter().skip(1).any(|p| p.is_some()) {
                return Err(DmabufError::new(
                    DmabufErrorKind::Incomplete,
                    "Only single-planar formats are supported",
                ));
            }
            plane.modifier
        };
        if !self.supports_format_modifier(format, modifier) {
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidFormat,
                "Unsupported dmabuf format/modifier",
            ));
        }
        let params = self
            .params
            .get_mut(&(client_id, params_id))
            .ok_or_else(|| {
                DmabufError::new(DmabufErrorKind::InvalidObject, "Unknown dmabuf params")
            })?;
        let Some(plane) = params.planes[0].take() else {
            return Err(DmabufError::new(
                DmabufErrorKind::Incomplete,
                "Missing plane 0",
            ));
        };
        // Stride*height is only meaningful for linear layouts.
        if plane.modifier == DRM_FORMAT_MOD_LINEAR {
            let needed = (plane.offset as u64)
                .saturating_add((plane.stride as u64).saturating_mul(height as u64));
            let size = fd_size(plane.fd.as_raw_fd())?;
            if needed > size {
                return Err(DmabufError::new(
                    DmabufErrorKind::OutOfBounds,
                    "dmabuf plane is out of bounds",
                ));
            }
        }
        params.used = true;
        self.buffers.insert(
            buffer_key,
            DmabufBuffer {
                planes: vec![plane],
                width: width as u32,
                height: height as u32,
                format,
                flags,
            },
        );
        Ok(())
    }

    pub fn has_buffer(&self, client_id: ClientId, buffer_id: ObjectId) -> bool {
        self.buffers.contains_key(&(client_id, buffer_id))
    }

    pub fn delete_buffer(&mut self, client_id: ClientId, buffer_id: ObjectId) {
        self.buffers.remove(&(client_id, buffer_id));
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        self.params.retain(|(owner, _), _| *owner != client_id);
        self.buffers.retain(|(owner, _), _| *owner != client_id);
    }

    /// Finish `create` after the caller registered `buffer_id`.
    pub fn create_from_params(
        &mut self,
        client_id: ClientId,
        params_id: ObjectId,
        buffer_id: ObjectId,
        width: i32,
        height: i32,
        format: u32,
        flags: u32,
    ) -> Result<()> {
        self.create_immed(
            client_id, params_id, buffer_id, width, height, format, flags,
        )
    }

    pub fn snapshot_buffer(
        &self,
        client_id: ClientId,
        buffer_id: ObjectId,
    ) -> Result<DmabufSnapshot> {
        let buffer = self
            .buffers
            .get(&(client_id, buffer_id))
            .ok_or_else(|| DmabufError::new(DmabufErrorKind::InvalidObject, "Unknown dmabuf"))?;
        let plane = &buffer.planes[0];
        let width = buffer.width as usize;
        let height = buffer.height as usize;
        let stride = plane.stride as usize;
        let offset = plane.offset as usize;
        let size = fd_size(plane.fd.as_raw_fd())? as usize;
        let map_len = offset
            .checked_add(stride.checked_mul(height).ok_or_else(|| {
                DmabufError::new(DmabufErrorKind::OutOfBounds, "dmabuf size overflow")
            })?)
            .ok_or_else(|| {
                DmabufError::new(DmabufErrorKind::OutOfBounds, "dmabuf size overflow")
            })?;
        if map_len > size {
            return Err(DmabufError::new(
                DmabufErrorKind::OutOfBounds,
                "dmabuf plane is out of bounds",
            ));
        }
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                map_len,
                PROT_READ,
                MAP_SHARED,
                plane.fd.as_raw_fd(),
                0,
            )
        };
        if ptr == MAP_FAILED {
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidFd,
                "Failed to map dmabuf",
            ));
        }
        let mut pixels = vec![0u8; stride * height];
        unsafe {
            let src = (ptr as *const u8).add(offset);
            std::ptr::copy_nonoverlapping(src, pixels.as_mut_ptr(), pixels.len());
            munmap(ptr, map_len);
        }
        let format = match buffer.format {
            DRM_FORMAT_ARGB8888 => WL_SHM_FORMAT_ARGB8888,
            DRM_FORMAT_XRGB8888 => WL_SHM_FORMAT_XRGB8888,
            _ => {
                return Err(DmabufError::new(
                    DmabufErrorKind::InvalidFormat,
                    "Unsupported dmabuf format",
                ));
            }
        };
        Ok(DmabufSnapshot {
            pixels,
            width,
            height,
            stride,
            format,
        })
    }

    /// Duplicates the buffer FD for GPU import without a CPU copy.
    pub fn export_buffer(
        &self,
        client_id: ClientId,
        buffer_id: ObjectId,
    ) -> Result<ExportedDmabuf> {
        let buffer = self
            .buffers
            .get(&(client_id, buffer_id))
            .ok_or_else(|| DmabufError::new(DmabufErrorKind::InvalidObject, "Unknown dmabuf"))?;
        let plane = &buffer.planes[0];
        let fd = dup_fd(plane.fd.as_raw_fd())?;
        let wl_format = match buffer.format {
            DRM_FORMAT_ARGB8888 => WL_SHM_FORMAT_ARGB8888,
            DRM_FORMAT_XRGB8888 => WL_SHM_FORMAT_XRGB8888,
            _ => {
                return Err(DmabufError::new(
                    DmabufErrorKind::InvalidFormat,
                    "Unsupported dmabuf format",
                ));
            }
        };
        Ok(ExportedDmabuf {
            fd,
            width: buffer.width,
            height: buffer.height,
            stride: plane.stride,
            offset: plane.offset,
            modifier: plane.modifier,
            drm_fourcc: buffer.format,
            wl_format,
        })
    }
}

fn dup_fd(fd: RawFd) -> Result<OwnedFd> {
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(DmabufError::new(
            DmabufErrorKind::InvalidFd,
            "Failed to duplicate dmabuf fd",
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

fn fd_size(fd: RawFd) -> Result<u64> {
    unsafe {
        let mut st: stat = std::mem::zeroed();
        if fstat(fd, &mut st) != 0 {
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidFd,
                "Failed to fstat dmabuf",
            ));
        }
        Ok(st.st_size as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, num::NonZeroU32, os::fd::IntoRawFd};

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }
    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn memfd(bytes: &[u8]) -> RawFd {
        let fd = unsafe { libc::memfd_create(c"lumalla-dmabuf".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(bytes).unwrap();
        file.into_raw_fd()
    }

    #[test]
    fn create_immed_and_snapshot_linear_xrgb() {
        let mut manager = DmabufManager::default();
        let client_id = client(1);
        manager.create_params(client_id, object(2)).unwrap();
        let pixels = [0x11u8, 0x22, 0x33, 0xff, 0x44, 0x55, 0x66, 0xff];
        manager
            .add_plane(
                client_id,
                object(2),
                memfd(&pixels),
                0,
                0,
                8,
                0,
                DRM_FORMAT_MOD_LINEAR as u32,
            )
            .unwrap();
        manager
            .create_immed(
                client_id,
                object(2),
                object(3),
                2,
                1,
                DRM_FORMAT_XRGB8888,
                0,
            )
            .unwrap();
        let snap = manager.snapshot_buffer(client_id, object(3)).unwrap();
        assert_eq!(snap.width, 2);
        assert_eq!(snap.height, 1);
        assert_eq!(snap.stride, 8);
        assert_eq!(snap.format, WL_SHM_FORMAT_XRGB8888);
        assert_eq!(snap.pixels, pixels);
    }

    #[test]
    fn export_buffer_duplicates_fd_without_cpu_copy() {
        let mut manager = DmabufManager::default();
        let client_id = client(1);
        manager.create_params(client_id, object(2)).unwrap();
        let pixels = [0x11u8, 0x22, 0x33, 0xff, 0x44, 0x55, 0x66, 0xff];
        manager
            .add_plane(
                client_id,
                object(2),
                memfd(&pixels),
                0,
                0,
                8,
                0,
                DRM_FORMAT_MOD_LINEAR as u32,
            )
            .unwrap();
        manager
            .create_immed(
                client_id,
                object(2),
                object(3),
                2,
                1,
                DRM_FORMAT_XRGB8888,
                0,
            )
            .unwrap();
        let exported = manager.export_buffer(client_id, object(3)).unwrap();
        assert_eq!(exported.width, 2);
        assert_eq!(exported.height, 1);
        assert_eq!(exported.stride, 8);
        assert_eq!(exported.drm_fourcc, DRM_FORMAT_XRGB8888);
        let snap = manager.snapshot_buffer(client_id, object(3)).unwrap();
        assert_eq!(snap.pixels, pixels);
    }
}
