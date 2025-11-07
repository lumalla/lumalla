/// Represents the messages that can be sent to the seat thread
#[derive(Debug)]
pub enum SeatMessage {
    /// Requests the seat thread to shut down
    Shutdown,
}
