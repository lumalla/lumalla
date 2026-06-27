/// Represents the messages that can be sent to the main thread
pub enum MainMessage {
    /// Requests the application to shut down
    Shutdown,
    /// Notifies that the main seat has been enabled
    MainSeatEnabled,
    /// Notifies that the main seat has been disabled
    MainSeatDisabled,
}
