//! GPU / present failure classification and recovery guard state.
//!
//! Separates skip-frame (transient) errors from session DRM denial and Vulkan
//! device-lost, so recovery can take the right path for each class.

use std::io;
use std::time::{Duration, Instant};

use ash::vk;

/// Base delay before the first deferred Vulkan recreate attempt after device loss.
pub const DEVICE_LOST_RETRY_BASE: Duration = Duration::from_secs(1);
/// Cap for exponential recreate backoff.
pub const DEVICE_LOST_RETRY_CAP: Duration = Duration::from_secs(30);

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

/// Present-loop guard: running, halted on session denial, or recovering from GPU loss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentGuard {
    Running,
    /// Session DRM permission lost; cleared only after successful DRM activate.
    Halted(PresentHaltReason),
    /// Vulkan torn down; waiting to recreate with backoff.
    Recovering {
        detail: String,
        attempt: u32,
        next_retry: Instant,
    },
}

impl PresentGuard {
    /// Presents should not run (halted or waiting on recreate backoff / pending recreate).
    pub fn blocks_presents(&self) -> bool {
        !matches!(self, Self::Running)
    }

    pub fn is_recovering(&self) -> bool {
        matches!(self, Self::Recovering { .. })
    }

    pub fn is_session_denied(&self) -> bool {
        matches!(
            self,
            Self::Halted(PresentHaltReason::SessionDenied { .. })
        )
    }

    pub fn is_device_lost_halt(&self) -> bool {
        matches!(self, Self::Halted(PresentHaltReason::DeviceLost { .. }))
            || matches!(self, Self::Recovering { .. })
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Running => None,
            Self::Halted(reason) => Some(reason.detail()),
            Self::Recovering { detail, .. } => Some(detail.as_str()),
        }
    }

    /// Whether a deferred recreate attempt is due.
    pub fn recovery_due(&self, now: Instant) -> bool {
        match self {
            Self::Recovering { next_retry, .. } => now >= *next_retry,
            // Stale DeviceLost halt (e.g. from before recovery ran) should also retry.
            Self::Halted(PresentHaltReason::DeviceLost { .. }) => true,
            _ => false,
        }
    }
}

/// Exponential backoff for Vulkan recreate after device loss (`attempt` is 1-based).
pub fn device_lost_backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    DEVICE_LOST_RETRY_BASE
        .saturating_mul(1u32 << shift)
        .min(DEVICE_LOST_RETRY_CAP)
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
