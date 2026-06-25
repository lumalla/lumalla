use crate::Output;

/// Messages handled by the compositor D-Bus thread.
#[derive(Debug)]
pub enum DbusMessage {
    /// Requests the D-Bus thread to shut down.
    Shutdown,
    /// Replaces the output list returned by `GetOutputs`.
    SetOutputs(Vec<Output>),
}
