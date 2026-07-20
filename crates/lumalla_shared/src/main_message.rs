use crate::Mods;
use crate::OutputConfig;
use std::path::PathBuf;

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
    /// Select the Vulkan render device by DRM primary path (`None` = auto).
    SetRenderDevice(Option<PathBuf>),
    /// Merge per-connector output configuration (enabled / mode).
    SetOutputConfigs(Vec<OutputConfig>),
}
