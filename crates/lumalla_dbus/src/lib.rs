//! D-Bus service thread for the Lumalla compositor.

#![warn(missing_docs)]

mod types;

use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use anyhow::Context;
use log::{error, info};
use lumalla_ipc::{BUS_NAME, OBJECT_PATH};
use lumalla_shared::{
    Comms, DbusMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MainMessage, MessageRunner,
};
use mio::{Events, Poll};
use types::OutputInfo;
use zbus::{blocking::connection, interface};

struct WindowManager {
    comms: Comms,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
}

#[interface(name = "org.lumalla.WindowManager")]
impl WindowManager {
    /// Request a graceful compositor shutdown.
    fn quit(&mut self) -> zbus::fdo::Result<()> {
        info!("Quit requested over D-Bus");
        self.comms.main(MainMessage::Shutdown);
        Ok(())
    }

    /// Returns the current output layout.
    fn get_outputs(&self) -> zbus::fdo::Result<Vec<OutputInfo>> {
        Ok(self.outputs.lock().unwrap().clone())
    }
}

/// Holds the state of the D-Bus service thread.
pub struct DbusState {
    comms: Comms,
    channel: mpsc::Receiver<DbusMessage>,
    event_loop: Poll,
    shutting_down: bool,
    _connection: zbus::blocking::Connection,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
}

impl MessageRunner for DbusState {
    type Message = DbusMessage;

    fn new(
        comms: Comms,
        event_loop: Poll,
        channel: mpsc::Receiver<Self::Message>,
        _args: &'static GlobalArgs,
    ) -> anyhow::Result<Self> {
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let connection = connection::Builder::session()
            .context("Failed to connect to session bus")?
            .name(BUS_NAME)
            .context("Failed to acquire D-Bus name")?
            .serve_at(
                OBJECT_PATH,
                WindowManager {
                    comms: comms.clone(),
                    outputs: Arc::clone(&outputs),
                },
            )
            .context("Failed to register D-Bus object")?
            .build()
            .context("Failed to build D-Bus connection")?;
        info!("D-Bus service listening on {BUS_NAME}{OBJECT_PATH}");

        Ok(Self {
            comms,
            channel,
            event_loop,
            shutting_down: false,
            _connection: connection,
            outputs,
        })
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut events = Events::with_capacity(128);
        loop {
            if let Err(err) = self
                .event_loop
                .poll(&mut events, Some(Duration::from_millis(50)))
            {
                error!("Unable to poll D-Bus event loop: {err}");
            }

            for event in events.iter() {
                if event.token() == MESSAGE_CHANNEL_TOKEN {
                    while let Ok(message) = self.channel.try_recv() {
                        if let Err(err) = self.handle_message(message) {
                            error!("Unable to handle D-Bus message: {err}");
                        }
                    }
                }
            }

            if self.shutting_down {
                break;
            }
        }

        Ok(())
    }
}

impl DbusState {
    fn handle_message(&mut self, message: DbusMessage) -> anyhow::Result<()> {
        match message {
            DbusMessage::Shutdown => {
                self.shutting_down = true;
            }
            DbusMessage::SetOutputs(outputs) => {
                *self.outputs.lock().unwrap() = outputs.into_iter().map(OutputInfo::from).collect();
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumalla_shared::{ConfigMessage, message_loop_with_channel};

    #[test]
    fn dbus_state_registers_service() {
        let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
        let (_, dbus_channel, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();
        let comms = Comms::new(to_main, to_config, to_dbus);
        let (event_loop, _, _) = message_loop_with_channel::<DbusMessage>().unwrap();
        let args: &'static GlobalArgs = Box::leak(Box::new(GlobalArgs::default()));

        let state = DbusState::new(comms, event_loop, dbus_channel, args);
        if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
            // CI or headless environments may not have a session bus.
            assert!(state.is_err());
            return;
        }

        assert!(state.is_ok());
    }
}
