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
    /// Broadcast absolute pointer position after motion (when listening).
    EmitCursorMoved {
        /// Absolute X in output-local (monitor) space.
        x: f64,
        /// Absolute Y in output-local (monitor) space.
        y: f64,
        /// Relative X delta for this input batch.
        dx: f64,
        /// Relative Y delta for this input batch.
        dy: f64,
    },
    /// Broadcast a pointer button press/release (when listening).
    EmitCursorClicked {
        /// X coordinate in output-local (monitor) space.
        x: f64,
        /// Y coordinate in output-local (monitor) space.
        y: f64,
        /// Button code (`0` = left; otherwise Linux `BTN_*`).
        button: u32,
        /// `true` on press, `false` on release.
        pressed: bool,
    },
    /// Broadcast a pointer scroll axis event (when listening).
    EmitCursorScrolled {
        /// X coordinate in output-local (monitor) space.
        x: f64,
        /// Y coordinate in output-local (monitor) space.
        y: f64,
        /// Axis: `0` = vertical, `1` = horizontal.
        axis: u32,
        /// Scroll delta (libinput scroll value; sign depends on axis direction).
        value: f64,
    },
    /// Set `WAYLAND_DISPLAY` used for processes spawned over D-Bus.
    SetWaylandDisplay(String),
    /// Spawn a process over D-Bus.
    Spawn {
        /// Program to spawn.
        command: String,
        /// Arguments to pass to the program.
        args: Vec<String>,
    },
    /// Replace the window list returned by `GetWindows`.
    SetWindows(Vec<WindowState>),
    /// Region capture finished; encode/write PNG on the D-Bus thread.
    ScreenshotCaptured {
        /// Matches the pending request id from [`crate::MainMessage::CaptureScreenshot`].
        request_id: usize,
        /// Captured pixels, or an error message.
        result: Result<CapturedImage, String>,
    },
    /// PipeWire stream start finished on the main thread.
    PipewireStreamStarted {
        /// Matches the pending request id from [`crate::MainMessage::StartPipewireStream`].
        request_id: usize,
        /// `(stream_id, node_id)` on success.
        result: Result<(u32, u32), String>,
    },
    /// Mutter ScreenCast stream is ready (or failed); emit `PipeWireStreamAdded` on success.
    MutterScreenCastStarted {
        /// Matches [`crate::MainMessage::StartMutterScreenCast::mutter_stream_id`].
        mutter_stream_id: u64,
        /// PipeWire node id on success.
        result: Result<u32, String>,
    },
}
