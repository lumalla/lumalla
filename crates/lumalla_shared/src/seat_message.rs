use std::path::PathBuf;

/// Represents the messages that can be sent to the seat thread
#[derive(Debug)]
pub enum SeatMessage {
    /// Requests the seat thread to shut down
    Shutdown,
    /// Notifies the seat thread that the seat has been enabled
    SeatEnabled,
    /// Notifies the seat thread that the seat has been disabled
    SeatDisabled,
    /// Request to open a device (e.g., DRM GPU device)
    /// The response will be sent as RendererMessage::FileOpenedInSession
    OpenDevice {
        /// The device path to open (e.g., /dev/dri/card0)
        path: PathBuf,
    },
}
