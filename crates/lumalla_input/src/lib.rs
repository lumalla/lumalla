use std::sync::{Arc, mpsc};

use log::{error, warn};
use lumalla_shared::{Comms, GlobalArgs, InputMessage, MESSAGE_CHANNEL_TOKEN, MessageRunner};
use mio::Poll;

pub struct InputState {
    _comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<InputMessage>,
    shutting_down: bool,
}

impl InputState {
    fn handle_message(&mut self, message: InputMessage) -> anyhow::Result<()> {
        match message {
            InputMessage::Shutdown => {
                self.shutting_down = true;
            }
            _ => {
                warn!("Message not handled: {message:?}");
            }
        }

        Ok(())
    }
}

impl MessageRunner for InputState {
    type Message = InputMessage;

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
