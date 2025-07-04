use std::sync::{Arc, mpsc};

use log::{error, warn};
use lumalla_shared::{Comms, DisplayMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner};
use mio::Poll;

pub struct DisplayState {
    _comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<DisplayMessage>,
    shutting_down: bool,
}

impl DisplayState {
    fn handle_message(&mut self, message: DisplayMessage) -> anyhow::Result<()> {
        match message {
            DisplayMessage::Shutdown => {
                self.shutting_down = true;
            }
            message => {
                warn!("Message not handled: {message:?}");
            }
        }

        Ok(())
    }
}

impl MessageRunner for DisplayState {
    type Message = DisplayMessage;

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
