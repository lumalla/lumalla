use std::sync::Arc;

use calloop::{LoopHandle, LoopSignal};

use crate::{GlobalArgs, comms::Comms};

/// A trait for running a message loop
pub trait MessageRunner {
    /// The message type that this runner handles
    type Message;
    /// Creates a new instance of the runner
    fn new(
        comms: Comms,
        loop_handle: LoopHandle<'static, Self>,
        args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self>
    where
        Self: Sized;
    /// Handle a message
    fn handle_message(&mut self, message: Self::Message) -> anyhow::Result<()>;
    /// Called when the loop is waiting for a new message. The provided [`LoopSignal`] can be used to stop
    /// the loop.
    fn on_dispatch_wait(&mut self, signal: &LoopSignal);
}
