use std::sync::Arc;

use anyhow::Context;
use calloop::{LoopSignal, PostAction};
use log::warn;
use lumalla_shared::{Comms, GlobalArgs, InputMessage, MessageRunner, MioLoopHandle, MioLoopSignal};

pub struct InputRunner {
    comms: Comms,
    loop_handle: MioLoopHandle,
}

impl MessageRunner for InputRunner {
    type Message = InputMessage;

    fn new(
        comms: Comms,
        loop_handle: MioLoopHandle,
        args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self> {
        Ok(Self { comms, loop_handle })
    }

    fn handle_message(&mut self, message: InputMessage) -> anyhow::Result<()> {
        match message {
            InputMessage::Shutdown => {
                warn!("Input module shutting down");
            }
        }
        Ok(())
    }

    fn on_dispatch_wait(&mut self, _signal: &MioLoopSignal) {
        // Nothing to do during wait
    }
}
