use std::sync::{Arc, mpsc};

use mio::Poll;

use crate::{GlobalArgs, comms::Comms};

pub const MESSAGE_CHANNEL_TOKEN: mio::Token = mio::Token(0);

/// A trait for running a message loop
pub trait MessageRunner {
    /// The message type that this runner handles
    type Message;
    /// Creates a new instance of the runner
    fn new(
        comms: Comms,
        event_loop: Poll,
        channel: mpsc::Receiver<Self::Message>,
        args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self>
    where
        Self: Sized;
    /// Run the message loop
    fn run(&mut self) -> anyhow::Result<()>;
}
