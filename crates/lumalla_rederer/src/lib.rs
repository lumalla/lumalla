use std::sync::Arc;

use anyhow::Context;
use calloop::{LoopSignal, PostAction};
use log::warn;
use lumalla_shared::{Comms, GlobalArgs, MessageRunner, MioLoopHandle, MioLoopSignal, RendererMessage};

pub struct RendererRunner {
    comms: Comms,
    loop_handle: MioLoopHandle,
}

impl MessageRunner for RendererRunner {
    type Message = RendererMessage;

    fn new(
        comms: Comms,
        loop_handle: MioLoopHandle,
        args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self> {
        Ok(Self { comms, loop_handle })
    }

    fn handle_message(&mut self, message: RendererMessage) -> anyhow::Result<()> {
        match message {
            RendererMessage::Shutdown => {
                warn!("Renderer module shutting down");
            }
        }
        Ok(())
    }

    fn on_dispatch_wait(&mut self, _signal: &MioLoopSignal) {
        // Nothing to do during wait
    }
}
