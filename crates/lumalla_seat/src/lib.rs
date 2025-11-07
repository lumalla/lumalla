use std::sync::{Arc, mpsc};

use log::error;
use lumalla_shared::{Comms, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner, SeatMessage};
use mio::Poll;

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    clippy::all
)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub use bindings::*;

pub struct SeatState {
    _comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<SeatMessage>,
    shutting_down: bool,
}

impl SeatState {
    fn handle_message(&mut self, message: SeatMessage) -> anyhow::Result<()> {
        match message {
            SeatMessage::Shutdown => {
                self.shutting_down = true;
            }
        }

        Ok(())
    }
}

impl MessageRunner for SeatState {
    type Message = SeatMessage;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_loaded() {
        let _size = std::mem::size_of::<bindings::libseat>();
    }
}
