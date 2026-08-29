use crate::Mods;
use crate::Output;
use crate::OutputConfig;
use crate::WindowGeometryUpdate;
use crate::WindowRule;
use crate::XkbConfig;
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
    },
    /// Clears all compositor key bindings.
    ClearKeymaps,
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
}
