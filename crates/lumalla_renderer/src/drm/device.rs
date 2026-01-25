//! DRM device management

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use anyhow::Context;
use drm::Device;
use drm::control::Device as ControlDevice;
use log::{debug, info};

/// A DRM device wrapper that implements the drm-rs traits.
///
/// This wraps an `OwnedFd` received from libseat (which has DRM master privileges).
pub struct DrmDevice {
    fd: OwnedFd,
}

// Implement the drm-rs Device trait
impl Device for DrmDevice {}

impl AsFd for DrmDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

// Implement ControlDevice for modesetting operations
impl ControlDevice for DrmDevice {}

impl DrmDevice {
    /// Creates a new DRM device from an owned file descriptor.
    ///
    /// The fd should be obtained from libseat via `open_device()`,
    /// which grants DRM master privileges.
    pub fn from_fd(fd: OwnedFd) -> anyhow::Result<Self> {
        let device = Self { fd };

        // Verify we can access DRM resources
        let _resources = device
            .resource_handles()
            .context("Failed to get DRM resources - is this a valid DRM device?")?;

        // Check for atomic modesetting support
        device
            .set_client_capability(drm::ClientCapability::Atomic, true)
            .context("Failed to enable atomic modesetting - driver may not support it")?;

        // Enable universal planes (needed for atomic)
        device
            .set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .context("Failed to enable universal planes")?;

        info!("DRM device initialized with atomic modesetting support");

        Ok(device)
    }

    /// Returns the DRM device capabilities.
    pub fn get_capabilities(&self) -> anyhow::Result<DrmCapabilities> {
        let driver = self.get_driver().context("Failed to get DRM driver info")?;

        let name = driver.name().to_string_lossy().into_owned();
        let description = driver.description().to_string_lossy().into_owned();

        debug!("DRM driver: {} - {}", name, description);

        // Check various capabilities
        let has_dumb_buffer = self
            .get_driver_capability(drm::DriverCapability::DumbBuffer)
            .unwrap_or(0)
            != 0;

        let has_prime = self
            .get_driver_capability(drm::DriverCapability::Prime)
            .unwrap_or(0)
            != 0;

        let cursor_width = self
            .get_driver_capability(drm::DriverCapability::CursorWidth)
            .unwrap_or(64);

        let cursor_height = self
            .get_driver_capability(drm::DriverCapability::CursorHeight)
            .unwrap_or(64);

        Ok(DrmCapabilities {
            driver_name: name,
            driver_description: description,
            has_dumb_buffer,
            has_prime,
            cursor_width,
            cursor_height,
        })
    }

    /// Returns a reference to the underlying file descriptor.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Capabilities of a DRM device.
#[derive(Debug, Clone)]
pub struct DrmCapabilities {
    /// The driver name (e.g., "amdgpu", "i915", "nouveau")
    pub driver_name: String,
    /// The driver description
    pub driver_description: String,
    /// Whether the driver supports dumb buffers
    pub has_dumb_buffer: bool,
    /// Whether the driver supports PRIME (DMA-BUF sharing)
    pub has_prime: bool,
    /// Cursor width
    pub cursor_width: u64,
    /// Cursor height
    pub cursor_height: u64,
}

/// Finds available DRM render nodes.
///
/// Returns paths to `/dev/dri/card*` devices.
pub fn find_drm_devices() -> anyhow::Result<Vec<std::path::PathBuf>> {
    let dri_path = Path::new("/dev/dri");

    if !dri_path.exists() {
        anyhow::bail!("/dev/dri does not exist - is the DRM subsystem loaded?");
    }

    let mut devices = Vec::new();

    for entry in std::fs::read_dir(dri_path).context("Failed to read /dev/dri")? {
        let entry = entry?;
        let path = entry.path();

        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            // Look for card* devices (not renderD* which are render-only)
            if name.starts_with("card") {
                devices.push(path);
            }
        }
    }

    devices.sort();

    info!("Found {} DRM device(s): {:?}", devices.len(), devices);

    Ok(devices)
}
