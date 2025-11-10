/// Represents the messages that can be sent to the seat thread
#[derive(Debug)]
pub enum SeatMessage {
    /// Requests the seat thread to shut down
    Shutdown,
    /// Notifies the seat thread that the seat has been enabled
    SeatEnabled,
    /// Notifies the seat thread that the seat has been disabled
    SeatDisabled,
}
