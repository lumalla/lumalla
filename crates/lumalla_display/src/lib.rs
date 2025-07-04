use std::sync::Arc;

use anyhow::Context;
use calloop::{LoopSignal, PostAction};
use log::warn;
use lumalla_shared::{Comms, DisplayMessage, GlobalArgs, MessageRunner, MioLoopHandle, MioLoopSignal};

pub struct DisplayRunner {
    comms: Comms,
    loop_handle: MioLoopHandle,
}

impl MessageRunner for DisplayRunner {
    type Message = DisplayMessage;

    fn new(
        comms: Comms,
        loop_handle: MioLoopHandle,
        args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self> {
        Ok(Self { comms, loop_handle })
    }

    fn handle_message(&mut self, message: DisplayMessage) -> anyhow::Result<()> {
        match message {
            DisplayMessage::Shutdown => {
                warn!("Display module shutting down");
            }
        }
        Ok(())
    }

    fn on_dispatch_wait(&mut self, _signal: &MioLoopSignal) {
        // Nothing to do during wait
    }
}
