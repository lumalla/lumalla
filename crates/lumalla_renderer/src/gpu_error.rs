//! GPU / present failure classification.
//!
//! Separates skip-frame (transient) errors from session DRM denial and Vulkan
//! device-lost, so recovery can take the right path for each class.

use std::io;

use ash::vk;

/// How a present/GPU error should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentFailureClass {
    /// Skip this frame; keep presenting later.
    Transient,
    /// DRM master / permission lost — wait for seat DRM activate.
    SessionDenied,
    /// Vulkan device lost — tear down and recreate the GPU context.
    DeviceLost,
}

/// Why presents are currently stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentHaltReason {
    SessionDenied { detail: String },
    DeviceLost { detail: String },
}

impl PresentHaltReason {
    /// Build a halt reason when `err` is unrecoverable for the current frame loop.
    pub fn from_error(err: &anyhow::Error) -> Option<Self> {
        match classify_gpu_error(err) {
            PresentFailureClass::Transient => None,
            PresentFailureClass::SessionDenied => Some(Self::SessionDenied {
                detail: format!("{err:#}"),
            }),
            PresentFailureClass::DeviceLost => Some(Self::DeviceLost {
                detail: format!("{err:#}"),
            }),
        }
    }

    pub fn class(&self) -> PresentFailureClass {
        match self {
            Self::SessionDenied { .. } => PresentFailureClass::SessionDenied,
            Self::DeviceLost { .. } => PresentFailureClass::DeviceLost,
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::SessionDenied { detail } | Self::DeviceLost { detail } => detail,
        }
    }
}

/// Classify a nested `anyhow` error from Vulkan submit, fence wait, or DRM present.
pub fn classify_gpu_error(err: &anyhow::Error) -> PresentFailureClass {
    for cause in err.chain() {
        if let Some(vk_err) = cause.downcast_ref::<vk::Result>()
            && *vk_err == vk::Result::ERROR_DEVICE_LOST
        {
            return PresentFailureClass::DeviceLost;
        }
        if let Some(io_err) = cause.downcast_ref::<io::Error>()
            && io_err.kind() == io::ErrorKind::PermissionDenied
        {
            return PresentFailureClass::SessionDenied;
        }
    }

    // Fallback for drivers/wrappers that stringify without preserving `vk::Result`.
    let msg = format!("{err:#}");
    if msg.contains("ERROR_DEVICE_LOST") || msg.contains("device has been lost") {
        return PresentFailureClass::DeviceLost;
    }
    if msg.contains("Permission denied") {
        return PresentFailureClass::SessionDenied;
    }
    PresentFailureClass::Transient
}

/// Whether further presents should stop until session or GPU recovery.
pub fn is_unrecoverable_present_error(err: &anyhow::Error) -> bool {
    !matches!(classify_gpu_error(err), PresentFailureClass::Transient)
}
