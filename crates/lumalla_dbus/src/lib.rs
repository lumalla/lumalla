//! D-Bus service thread for the Lumalla compositor.

#![warn(missing_docs)]

mod iface;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::Context;
use iface::{emit_signal, ServiceState, WindowManager};
use log::{error, info};
use lumalla_ipc::{types::OutputInfo, BUS_NAME, OBJECT_PATH};
use lumalla_shared::{
    Comms, DbusMessage, MESSAGE_CHANNEL_TOKEN, MainMessage, MessageSender, Output,
};
use mio::{Events, Poll};
use zbus::{blocking::connection, Error as ZbusError};

/// A registered D-Bus service that must be kept alive for the lifetime of the compositor.
pub struct DbusService {
    connection: zbus::blocking::Connection,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    output_lookup: Arc<Mutex<HashMap<String, Output>>>,
}

impl DbusService {
    /// Connect to the session bus and acquire `org.lumalla.wm`.
    pub fn register(comms: Comms) -> anyhow::Result<Self> {
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let output_lookup = Arc::new(Mutex::new(HashMap::new()));
        let state = Arc::new(ServiceState {
            comms: comms.clone(),
            outputs: Arc::clone(&outputs),
            output_lookup: Arc::clone(&output_lookup),
            extra_env: Arc::new(Mutex::new(HashMap::new())),
            keymaps: Arc::new(Mutex::new(Vec::new())),
        });
        let connection = connection::Builder::session()
            .context("Failed to connect to session bus")?
            .name(BUS_NAME)
            .context("Invalid D-Bus name")?
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .serve_at(
                OBJECT_PATH,
                WindowManager {
                    state: Arc::clone(&state),
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
            connection,
            outputs,
            output_lookup,
        })
    }

    /// Notify config clients that the compositor is ready.
    pub fn emit_ready(&self) -> anyhow::Result<()> {
        emit_signal(&self.connection, "Ready", &())
    }
}

struct DbusState {
    channel: mpsc::Receiver<DbusMessage>,
    event_loop: Poll,
    shutting_down: bool,
    connection: zbus::blocking::Connection,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    output_lookup: Arc<Mutex<HashMap<String, Output>>>,
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
            connection: service.connection,
            outputs: service.outputs,
            output_lookup: service.output_lookup,
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
                self.update_outputs(outputs);
            }
            DbusMessage::EmitReady => {
                emit_signal(&self.connection, "Ready", &())?;
            }
            DbusMessage::EmitOutputChanged(outputs) => {
                let infos = self.update_outputs(outputs);
                emit_signal(&self.connection, "OutputChanged", &(&infos,))?;
            }
            DbusMessage::EmitBindingActivated(binding_id) => {
                emit_signal(
                    &self.connection,
                    "BindingActivated",
                    &(&binding_id,),
                )?;
            }
        }

        Ok(())
    }

    fn update_outputs(&self, outputs: Vec<Output>) -> Vec<OutputInfo> {
        let infos: Vec<OutputInfo> = outputs.iter().map(OutputInfo::from).collect();
        *self.outputs.lock().unwrap() = infos.clone();
        let mut lookup = self.output_lookup.lock().unwrap();
        lookup.clear();
        for output in outputs {
            lookup.insert(output.name.clone(), output);
        }
        infos
    }
}

/// Run the D-Bus message loop on a dedicated thread.
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
                Ok(Ok(())) => info!("D-Bus thread exited normally"),
                Ok(Err(ref err)) => error!("D-Bus thread exited with an error: {err}"),
                Err(ref err) => error!("D-Bus thread panicked: {err:?}"),
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
    use lumalla_shared::{
        DisplayMessage, InputMessage, RendererMessage, SeatMessage, message_loop_with_channel,
    };

    fn comms() -> Comms {
        let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, _, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();
        let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
        let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
        let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
        let (_, _, to_seat) = message_loop_with_channel::<SeatMessage>().unwrap();
        Comms::new(
            to_main,
            to_dbus,
            to_display,
            to_input,
            to_renderer,
            to_seat,
        )
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
