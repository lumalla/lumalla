use std::sync::{Arc, mpsc};

use log::{error, warn};
use lumalla_shared::{Comms, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner, RendererMessage};
use mio::Poll;

pub struct RendererState {
    _comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<RendererMessage>,
    shutting_down: bool,
}

impl RendererState {
    fn handle_message(&mut self, message: RendererMessage) -> anyhow::Result<()> {
        match message {
            RendererMessage::Shutdown => {
                self.shutting_down = true;
            }
            message => {
                warn!("Message not handled: {message:?}");
            }
        }

        Ok(())
    }
}

impl MessageRunner for RendererState {
    type Message = RendererMessage;

    fn new(
        comms: Comms,
        event_loop: Poll,
        channel: mpsc::Receiver<Self::Message>,
        _args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            _comms: comms,
            event_loop,
            channel,
            shutting_down: false,
        })
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut events = mio::Events::with_capacity(128);
        loop {
            if let Err(err) = self.event_loop.poll(&mut events, None) {
                error!("Unable to poll event loop: {err}");
            }

            for event in events.iter() {
                match event.token() {
                    MESSAGE_CHANNEL_TOKEN => {
                        while let Ok(msg) = self.channel.try_recv() {
                            if let Err(err) = self.handle_message(msg) {
                                error!("Unable to handle message: {err}");
                            }
                        }
                    }
                    _ => {}
                }
            }

            if self.shutting_down {
                break;
            }
        }

        Ok(())
    }
}
