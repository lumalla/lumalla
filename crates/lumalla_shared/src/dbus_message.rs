use crate::Output;

/// Messages handled by the compositor D-Bus thread.
#[derive(Debug)]
pub enum DbusMessage {
    /// Requests the D-Bus thread to shut down.
    Shutdown,
    /// Replaces the output list returned by `GetOutputs` and used for layout resolution.
    SetOutputs(Vec<Output>),
    /// Broadcast that the compositor is ready for configuration.
    EmitReady,
    /// Broadcast an output list change to config clients.
    EmitOutputChanged(Vec<Output>),
    /// Broadcast that a custom key binding was activated.
    EmitBindingActivated(String),
}
