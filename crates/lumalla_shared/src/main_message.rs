use crate::Mods;
use crate::Output;
use crate::OutputConfig;
use crate::View;
use crate::WindowGeometryUpdate;
use crate::WindowRule;
use crate::XkbConfig;
use crate::Zone;
use std::path::PathBuf;

/// Synthetic input requested by profiling or automation configs.
#[derive(Debug, Clone)]
pub enum InjectedInput {
    /// Press and release a named key (xkb keysym name).
    Key {
        /// Keysym name such as `"Return"` or `"a"`.
        name: String,
    },
    /// Type a UTF-8 string as key presses.
    TypeText {
        /// Text to type.
        text: String,
    },
    /// Move the pointer to absolute compositor coordinates.
    PointerMove {
        /// X coordinate in compositor space.
        x: f64,
        /// Y coordinate in compositor space.
        y: f64,
    },
    /// Click a pointer button at absolute compositor coordinates.
    PointerClick {
        /// X coordinate in compositor space.
        x: f64,
        /// Y coordinate in compositor space.
        y: f64,
        /// Linux input button code (defaults to left button).
        button: u32,
    },
}

/// Represents the messages that can be sent to the main thread
pub enum MainMessage {
    /// Requests the application to shut down
    Shutdown,
    /// Notifies that the main seat has been enabled
    MainSeatEnabled,
    /// Notifies that the main seat has been disabled
    MainSeatDisabled,
    /// Switch to the given VT/session (1-based).
    SwitchVt(i32),
    /// Registers a compositor key binding.
    AddKeymap {
        /// Linux input keycode.
        key: u32,
        /// Required modifiers.
        mods: Mods,
        /// Binding id forwarded in `BindingActivated` signals.
        binding_id: String,
        /// When true, fire on key release instead of press.
        on_release: bool,
        /// When true, stop further binding matches and do not forward the key to clients.
        consume: bool,
    },
    /// Clears all compositor key bindings.
    ClearKeymaps,
    /// Remove a single key binding by the id returned from config `map_key`.
    RemoveKeymap {
        /// Binding id previously passed to [`Self::AddKeymap`].
        binding_id: String,
    },
    /// Replace the XKB keymap from RMLVO names.
    SetXkb(XkbConfig),
    /// Select the Vulkan render device by DRM primary path (`None` = auto).
    SetRenderDevice(Option<PathBuf>),
    /// Merge per-connector output configuration (enabled / mode).
    SetOutputConfigs(Vec<OutputConfig>),
    /// Add a logical Wayland output (config-owned).
    AddOutput(Output),
    /// Remove a logical Wayland output by name.
    RemoveOutput {
        /// Output name previously passed to [`Self::AddOutput`].
        name: String,
    },
    /// Append a view to a named output (replaces an existing view with the same name).
    AddView {
        /// Output that owns the view.
        output: String,
        /// View to add or replace.
        view: View,
    },
    /// Remove a view from a named output by view name.
    RemoveView {
        /// Output that owns the view.
        output: String,
        /// View name previously passed to [`Self::AddView`].
        view: String,
    },
    /// Add or replace a zone by name.
    AddZone(Zone),
    /// Remove a zone by name.
    RemoveZone {
        /// Zone name previously passed to [`Self::AddZone`].
        name: String,
    },
    /// Assign a window to a zone and apply its composition strategy.
    /// `window == None` targets the focused window.
    AddWindowToZone {
        /// Window id, or `None` for the focused window.
        window: Option<u32>,
        /// Zone name.
        zone: String,
    },
    /// Clear a window's zone membership without changing geometry.
    /// `window == None` targets the focused window.
    RemoveWindowFromZone {
        /// Window id, or `None` for the focused window.
        window: Option<u32>,
    },
    /// Inject synthetic keyboard or pointer input.
    InjectInput(InjectedInput),
    /// Capture a rectangular region of the compositor for a screenshot request.
    CaptureScreenshot {
        /// Correlates the reply on the D-Bus thread (path string heap pointer).
        request_id: usize,
        /// Left edge in compositor space.
        x: i32,
        /// Top edge in compositor space.
        y: i32,
        /// Region width in compositor space.
        width: i32,
        /// Region height in compositor space.
        height: i32,
    },
    /// Start a PipeWire video stream of a compositor region.
    StartPipewireStream {
        /// Correlates the reply on the D-Bus thread.
        request_id: usize,
        /// Left edge in compositor space.
        x: i32,
        /// Top edge in compositor space.
        y: i32,
        /// Region width in compositor space.
        width: i32,
        /// Region height in compositor space.
        height: i32,
        /// PipeWire node name.
        name: String,
        /// Maximum capture rate (frames per second).
        max_fps: u32,
    },
    /// Stop a PipeWire video stream previously started via [`Self::StartPipewireStream`].
    StopPipewireStream {
        /// Stream id returned from start.
        stream_id: u32,
    },
    /// Start a Mutter ScreenCast stream for a named output (portal path).
    StartMutterScreenCast {
        /// Opaque stream id from the Mutter ScreenCast D-Bus object.
        mutter_stream_id: u64,
        /// Session that owns this stream (for stop grouping).
        session_id: u64,
        /// Output / connector name to capture.
        connector: String,
    },
    /// Stop all PipeWire streams belonging to a Mutter ScreenCast session.
    StopMutterScreenCast {
        /// Mutter session id.
        session_id: u64,
    },
    /// PipeWire finished creating a stream previously requested on the main thread.
    ///
    /// Carries the PipeWire node id on success. Delivered asynchronously so stream start
    /// never blocks the compositor event loop.
    PipewireStreamReady {
        /// Local stream id assigned when start was requested.
        stream_id: u32,
        /// PipeWire node id, or an error string.
        result: Result<u32, String>,
    },
    /// PipeWire dequeued a DMA-BUF that needs a GPU blit on the main thread.
    ScreencastBlitNeeded,
    /// Update window geometry. `id == None` targets the focused window.
    SetWindow {
        /// Window id, or `None` for the focused window.
        id: Option<u32>,
        /// Fields to update.
        geometry: WindowGeometryUpdate,
        /// When true, updated fields are marked as user-placed and won't be overwritten by rules.
        user_initiated: bool,
    },
    /// Register a default placement rule for matching app ids.
    AddWindowRule(WindowRule),
    /// Remove all window placement rules.
    ClearWindowRules,
    /// Focus a window (`id == None` → focused). Optionally raise it after focusing.
    FocusWindow {
        /// Window id, or `None` for the focused window.
        id: Option<u32>,
        /// When true, also raise the window to the top of paint order.
        raise: bool,
    },
    /// Raise a window in stacking order without changing keyboard focus.
    /// `id == None` targets the focused window.
    RaiseWindow {
        /// Window id, or `None` for the focused window.
        id: Option<u32>,
    },
    /// Enable or disable emitting cursor move / click / scroll signals to config.
    SetCursorListening {
        /// Emit [`crate::DbusMessage::EmitCursorMoved`] after pointer motion.
        listen_move: bool,
        /// Emit [`crate::DbusMessage::EmitCursorClicked`] on pointer buttons.
        listen_click: bool,
        /// Emit [`crate::DbusMessage::EmitCursorScrolled`] on pointer axis.
        listen_scroll: bool,
        /// Withhold matching motion events from Wayland clients.
        consume_move: bool,
        /// Withhold matching button events from Wayland clients.
        consume_click: bool,
        /// Withhold matching axis events from Wayland clients.
        consume_scroll: bool,
    },
}
