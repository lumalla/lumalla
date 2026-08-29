use crate::{CapturedImage, DrmDeviceState, Output, WindowState};

/// Messages handled by the compositor D-Bus thread.
#[derive(Debug)]
pub enum DbusMessage {
    /// Requests the D-Bus thread to shut down.
    Shutdown,
    /// Replaces the output list returned by `GetOutputs` and used for layout resolution.
    SetOutputs(Vec<Output>),
    /// Replaces the DRM device list returned by `GetDrmDevices`.
    SetDrmDevices(Vec<DrmDeviceState>),
    /// Broadcast that the compositor is ready for configuration.
    EmitReady,
    /// Broadcast an output list change to config clients.
    EmitOutputChanged(Vec<Output>),
    /// Broadcast a DRM device list change to IPC clients.
    EmitDrmDevicesChanged(Vec<DrmDeviceState>),
    /// Broadcast that a custom key binding was activated.
    EmitBindingActivated(String),
    /// Set `WAYLAND_DISPLAY` used for processes spawned over D-Bus.
    SetWaylandDisplay(String),
    /// Replace the window list returned by `GetWindows`.
    SetWindows(Vec<WindowState>),
    /// Region capture finished; encode/write PNG on the D-Bus thread.
    ScreenshotCaptured {
        /// Matches the pending request id from [`crate::MainMessage::CaptureScreenshot`].
        request_id: usize,
        /// Captured pixels, or an error message.
        result: Result<CapturedImage, String>,
    },
}
