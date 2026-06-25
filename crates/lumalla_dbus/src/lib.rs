//! D-Bus service thread for the Lumalla compositor.

#![warn(missing_docs)]

mod types;

use std::{
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::Context;
use log::{error, info};
use lumalla_ipc::{BUS_NAME, OBJECT_PATH};
use lumalla_shared::{
    Comms, DbusMessage, MESSAGE_CHANNEL_TOKEN, MainMessage, MessageSender,
};
use mio::{Events, Poll};
use types::OutputInfo;
use zbus::{Error as ZbusError, blocking::connection, interface};

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

/// A registered D-Bus service that must be kept alive for the lifetime of the compositor.
pub struct DbusService {
    _connection: zbus::blocking::Connection,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
}

impl DbusService {
    /// Connect to the session bus and acquire `org.lumalla.wm`.
    ///
    /// Returns an error if another process already owns the name.
    pub fn register(comms: Comms) -> anyhow::Result<Self> {
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let connection = connection::Builder::session()
            .context("Failed to connect to session bus")?
            .name(BUS_NAME)
            .context("Invalid D-Bus name")?
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .serve_at(
                OBJECT_PATH,
                WindowManager {
                    comms: comms.clone(),
                    outputs: Arc::clone(&outputs),
                },
            )
            .context("Failed to register D-Bus object")?
            .build()
            .map_err(|err| -> anyhow::Error {
                if err == ZbusError::NameTaken {
                    anyhow::anyhow!("another process already owns the D-Bus name `{BUS_NAME}`")
                } else {
                    err.into()
                }
            })?;
        info!("D-Bus service listening on {BUS_NAME}{OBJECT_PATH}");

        Ok(Self {
            _connection: connection,
            outputs,
        })
    }
}

/// Holds the state of the D-Bus service thread.
struct DbusState {
    channel: mpsc::Receiver<DbusMessage>,
    event_loop: Poll,
    shutting_down: bool,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
}

impl DbusState {
    fn new(
        event_loop: Poll,
        channel: mpsc::Receiver<DbusMessage>,
        service: DbusService,
    ) -> Self {
        Self {
            channel,
            event_loop,
            shutting_down: false,
            outputs: service.outputs,
        }
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

/// Run the D-Bus message loop on a dedicated thread.
///
/// The service must already be registered via [`DbusService::register`].
pub fn run_thread(
    to_main: MessageSender<MainMessage>,
    event_loop: Poll,
    channel: mpsc::Receiver<DbusMessage>,
    service: DbusService,
) -> anyhow::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(String::from("dbus"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut state = DbusState::new(event_loop, channel, service);
                state.run().context("D-Bus thread exited with an error")
            }));
            match result {
                Ok(Ok(())) => {
                    info!("D-Bus thread exited normally");
                }
                Ok(Err(ref err)) => {
                    error!("D-Bus thread exited with an error: {err}");
                }
                Err(ref err) => {
                    if let Some(err) = err.downcast_ref::<&str>() {
                        error!("D-Bus thread panicked: {err}");
                    } else if let Some(err) = err.downcast_ref::<String>() {
                        error!("D-Bus thread panicked: {err}");
                    } else {
                        error!("D-Bus thread panicked: {:?}", err);
                    }
                }
            }

            if let Err(err) = to_main.send(MainMessage::Shutdown) {
                error!("Unable to send shutdown signal to main from D-Bus thread: {err}");
            }
        })
        .context("Unable to spawn D-Bus thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumalla_shared::{ConfigMessage, message_loop_with_channel};

    fn comms() -> Comms {
        let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
        let (_, _, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();
        Comms::new(to_main, to_config, to_dbus)
    }

    #[test]
    fn dbus_name_registration() {
        if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
            return;
        }

        let first = DbusService::register(comms()).expect("registration should succeed");
        drop(first);

        let holder = DbusService::register(comms()).expect("registration should succeed after release");
        let second = DbusService::register(comms());
        assert!(second.is_err(), "second registration should fail while name is held");
        let err = second.err().unwrap();
        assert!(
            format!("{err:#}").contains("already owns"),
            "unexpected error: {err:#}"
        );
        drop(holder);
    }
}
