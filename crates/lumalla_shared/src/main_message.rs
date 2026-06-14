/// Represents the messages that can be sent to the main thread
pub enum MainMessage {
    /// Requests the application to shut down
    Shutdown,
    /// Notifies that the seat has been enabled
    SeatEnabled,
    /// Notifies that the seat has been disabled
    SeatDisabled,
}
