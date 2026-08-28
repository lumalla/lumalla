use std::sync::mpsc;

use crate::{GlobalArgs, comms::Comms, ring::EventLoop};

pub use crate::ring::MESSAGE_CHANNEL_TOKEN;

/// A trait for running a message loop.
pub trait MessageRunner {
    /// The message type that this runner handles.
    type Message;
    /// Creates a new instance of the runner.
    fn new(
        comms: Comms,
        event_loop: EventLoop,
        channel: mpsc::Receiver<Self::Message>,
        args: &'static GlobalArgs,
    ) -> anyhow::Result<Self>
    where
        Self: Sized;
    /// Run the message loop.
    fn run(&mut self) -> anyhow::Result<()>;
}
