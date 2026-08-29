use std::{
    collections::HashMap,
    fmt,
    io::Write,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    path::Path,
};

use libc::{MAP_FAILED, MAP_SHARED, PROT_READ, fstat, mmap, munmap, stat};
use lumalla_wayland_protocol::{
    ClientId, ObjectId,
    buffer::Writer,
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

/// Packed format+modifier entry for `zwp_linux_dmabuf_feedback_v1.format_table`.
const FORMAT_TABLE_ENTRY_SIZE: usize = 16;

#[derive(Debug)]
struct FormatTable {
    fd: OwnedFd,
    size: u32,
    /// Number of `(format, modifier)` rows in the table.
    entry_count: u32,
}

#[derive(Debug)]
struct FeedbackObject {
    version: u32,
}

#[derive(Debug, Default)]
pub struct DmabufManager {
    params: HashMap<ResourceKey, Params>,
    buffers: HashMap<ResourceKey, DmabufBuffer>,
    /// Advertised `(drm_fourcc, modifier)` pairs. Empty means built-in linear defaults.
    supported: Vec<(u32, u64)>,
    /// Cached read-only format table for feedback objects.
    format_table: Option<FormatTable>,
    /// DRM `dev_t` of the preferred main/sampling device (`st_rdev`).
    main_device: Option<libc::dev_t>,
    /// Active `zwp_linux_dmabuf_feedback_v1` objects.
    feedbacks: HashMap<ResourceKey, FeedbackObject>,
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

    /// Update advertised formats and optional main DRM device for feedback.
    ///
    /// Rebuilds the format table memfd. Call [`Self::send_all_feedback`] afterward
    /// if clients already hold feedback objects.
    pub fn set_supported_formats(&mut self, formats: Vec<(u32, u64)>, device_path: Option<&Path>) {
        self.supported = formats;
        self.main_device = device_path.and_then(device_rdev);
        self.format_table = match build_format_table(self.supported_formats()) {
            Ok(table) => Some(table),
            Err(err) => {
                log::warn!("Failed to build linux-dmabuf format table: {err}");
                None
            }
        };
    }

    pub fn create_feedback(
        &mut self,
        client_id: ClientId,
        object_id: ObjectId,
        version: u32,
    ) -> Result<()> {
        self.ensure_format_table();
        let key = (client_id, object_id);
        if self.feedbacks.contains_key(&key) {
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidObject,
                "dmabuf feedback object already exists",
            ));
        }
        self.feedbacks.insert(key, FeedbackObject { version });
        Ok(())
    }

    fn ensure_format_table(&mut self) {
        if self.format_table.is_some() {
            return;
        }
        match build_format_table(self.supported_formats()) {
            Ok(table) => self.format_table = Some(table),
            Err(err) => log::warn!("Failed to build linux-dmabuf format table: {err}"),
        }
    }

    pub fn destroy_feedback(&mut self, client_id: ClientId, object_id: ObjectId) {
        self.feedbacks.remove(&(client_id, object_id));
    }

    /// Send full feedback parameters for one object.
    pub fn send_feedback(&self, writer: &mut Writer, object_id: ObjectId, version: u32) {
        let Some(table) = self.format_table.as_ref() else {
            // Still finish the feedback sequence so clients do not hang.
            writer.zwp_linux_dmabuf_feedback_v1_done(object_id);
            return;
        };

        writer
            .zwp_linux_dmabuf_feedback_v1_format_table(object_id)
            .fd(table.fd.as_raw_fd())
            .size(table.size);

        let device = encode_dev_t(self.main_device.unwrap_or(0));
        // `main_device` is required below v6; deprecated (and unused) from v6 on.
        if version < 6 {
            writer
                .zwp_linux_dmabuf_feedback_v1_main_device(object_id)
                .device(&device);
        }

        writer
            .zwp_linux_dmabuf_feedback_v1_tranche_target_device(object_id)
            .device(&device);

        let mut flags = 0u32;
        if version >= 6 {
            flags |= lumalla_wayland_protocol::protocols::linux_dmabuf::ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_FLAGS_SAMPLING;
        }
        writer
            .zwp_linux_dmabuf_feedback_v1_tranche_flags(object_id)
            .flags(flags);

        let indices = tranche_indices(table.entry_count);
        // Protocol allows multiple tranche_formats; one event is enough for our table size.
        writer
            .zwp_linux_dmabuf_feedback_v1_tranche_formats(object_id)
            .indices(&indices);

        writer.zwp_linux_dmabuf_feedback_v1_tranche_done(object_id);
        writer.zwp_linux_dmabuf_feedback_v1_done(object_id);
    }

    /// Re-send feedback to every live feedback object (e.g. after format refresh).
    pub fn send_all_feedback<'a>(
        &self,
        clients: impl Iterator<Item = &'a mut lumalla_wayland_protocol::ClientConnection>,
    ) {
        let feedbacks: Vec<(ClientId, ObjectId, u32)> = self
            .feedbacks
            .iter()
            .map(|(&(client_id, object_id), feedback)| (client_id, object_id, feedback.version))
            .collect();
        if feedbacks.is_empty() {
            return;
        }

        let mut by_client: HashMap<ClientId, Vec<(ObjectId, u32)>> = HashMap::new();
        for (client_id, object_id, version) in feedbacks {
            by_client
                .entry(client_id)
                .or_default()
                .push((object_id, version));
        }

        for client in clients {
            let Some(objects) = by_client.get(&client.client_id()) else {
                continue;
            };
            let writer = client.writer_mut();
            for &(object_id, version) in objects {
                self.send_feedback(writer, object_id, version);
            }
        }
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
        // SAFETY: caller transfers ownership of a message SCM_RIGHTS fd.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let Some(params) = self.params.get_mut(&(client_id, params_id)) else {
            close_owned_fd(fd);
            return Err(DmabufError::new(
                DmabufErrorKind::InvalidObject,
                "Unknown dmabuf params",
            ));
        };
        if params.used {
            close_owned_fd(fd);
            return Err(DmabufError::new(
                DmabufErrorKind::AlreadyUsed,
                "dmabuf params already used",
            ));
        }
        if plane_idx as usize >= params.planes.len() {
            close_owned_fd(fd);
            return Err(DmabufError::new(
                DmabufErrorKind::PlaneIdx,
                "plane index out of bounds",
            ));
        }
        if params.planes[plane_idx as usize].is_some() {
            close_owned_fd(fd);
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

fn device_rdev(path: &Path) -> Option<libc::dev_t> {
    use std::os::unix::ffi::OsStrExt;
    unsafe {
        let mut st: stat = std::mem::zeroed();
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        if libc::stat(c_path.as_ptr(), &mut st) != 0 {
            return None;
        }
        Some(st.st_rdev)
    }
}

fn encode_dev_t(dev: libc::dev_t) -> Vec<u8> {
    let mut bytes = vec![0u8; std::mem::size_of::<libc::dev_t>()];
    bytes.copy_from_slice(unsafe {
        std::slice::from_raw_parts(
            (&raw const dev).cast::<u8>(),
            std::mem::size_of::<libc::dev_t>(),
        )
    });
    bytes
}

fn tranche_indices(entry_count: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(entry_count as usize * 2);
    for index in 0..entry_count {
        bytes.extend_from_slice(&(index as u16).to_ne_bytes());
    }
    bytes
}

fn build_format_table(formats: &[(u32, u64)]) -> std::io::Result<FormatTable> {
    let size = formats
        .len()
        .checked_mul(FORMAT_TABLE_ENTRY_SIZE)
        .ok_or_else(|| std::io::Error::other("format table too large"))?;
    let fd = unsafe {
        libc::memfd_create(
            c"lumalla-dmabuf-format-table".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut packed = Vec::with_capacity(size);
    for &(format, modifier) in formats {
        packed.extend_from_slice(&format.to_ne_bytes());
        packed.extend_from_slice(&0u32.to_ne_bytes());
        packed.extend_from_slice(&modifier.to_ne_bytes());
    }
    file.write_all(&packed)?;
    // Prevent later mutation; protocol forbids changing table contents after send.
    let raw = file.as_raw_fd();
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
    if unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, seals) } != 0 {
        // Sealing is best-effort; keep the table even if the kernel rejects seals.
        log::debug!(
            "Unable to seal dmabuf format table: {}",
            std::io::Error::last_os_error()
        );
    }
    let owned = OwnedFd::from(file);
    Ok(FormatTable {
        fd: owned,
        size: size as u32,
        entry_count: formats.len() as u32,
    })
}

fn close_owned_fd(fd: OwnedFd) {
    let raw = fd.into_raw_fd();
    unsafe {
        libc::close(raw);
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

    #[test]
    fn format_table_packs_format_modifier_pairs() {
        let mut manager = DmabufManager::default();
        manager.set_supported_formats(
            vec![
                (DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR),
                (DRM_FORMAT_ARGB8888, 0x0100_0000_0000_0001),
            ],
            None,
        );
        let table = manager.format_table.as_ref().expect("format table");
        assert_eq!(table.size, 32);
        assert_eq!(table.entry_count, 2);

        let mut mapped = vec![0u8; table.size as usize];
        unsafe {
            let len = libc::pread(
                table.fd.as_raw_fd(),
                mapped.as_mut_ptr().cast(),
                mapped.len(),
                0,
            );
            assert_eq!(len as usize, mapped.len());
        }
        assert_eq!(&mapped[0..4], &DRM_FORMAT_XRGB8888.to_ne_bytes());
        assert_eq!(&mapped[4..8], &0u32.to_ne_bytes());
        assert_eq!(&mapped[8..16], &DRM_FORMAT_MOD_LINEAR.to_ne_bytes());
        assert_eq!(&mapped[16..20], &DRM_FORMAT_ARGB8888.to_ne_bytes());
        assert_eq!(&mapped[24..32], &0x0100_0000_0000_0001u64.to_ne_bytes());

        let indices = tranche_indices(2);
        assert_eq!(indices, {
            let mut expected = Vec::new();
            expected.extend_from_slice(&0u16.to_ne_bytes());
            expected.extend_from_slice(&1u16.to_ne_bytes());
            expected
        });
    }
}
