//! Input handling for Lumalla via libinput.

use std::collections::HashMap;
use std::sync::{Arc, mpsc};

use anyhow::Context;
use log::{error, info, warn};
use lumalla_shared::{
    Comms, DbusMessage, GlobalArgs, InputMessage, MESSAGE_CHANNEL_TOKEN, MessageRunner, Mods,
};
use mio::{Events, Interest, Poll, Token};

mod libinput;
mod restricted;

pub use restricted::{OpenRequest, RestrictedDeviceOpener};

pub const LIBINPUT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);

pub struct InputState {
    comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<InputMessage>,
    shutting_down: bool,
    keymaps: HashMap<(String, Mods), usize>,
    mods: Mods,
    libinput: Option<libinput::LibInput>,
    device_opener: Arc<RestrictedDeviceOpener>,
}

impl InputState {
    fn handle_message(&mut self, message: InputMessage) -> anyhow::Result<()> {
        match message {
            InputMessage::Shutdown => {
                self.shutting_down = true;
            }
            InputMessage::SeatEnabled { seat_name } => {
                self.enable_seat(&seat_name)?;
            }
            InputMessage::ClearKeymaps => {
                self.keymaps.clear();
            }
            InputMessage::Keymap {
                key_name,
                mods,
                callback,
            } => {
                self.keymaps
                    .insert((normalize_key_name(&key_name), mods), callback.callback_id);
            }
            InputMessage::OpenFileInSessionForRenderer { path } => {
                warn!(
                    "OpenFileInSessionForRenderer not implemented: {}",
                    path.display()
                );
            }
        }

        Ok(())
    }

    fn enable_seat(&mut self, seat_name: &str) -> anyhow::Result<()> {
        if self.libinput.is_some() {
            return Ok(());
        }

        let mut libinput = libinput::LibInput::new(self.device_opener.clone())?;
        libinput.assign_seat(seat_name)?;
        self.event_loop
            .registry()
            .register(&mut libinput, LIBINPUT_TOKEN, Interest::READABLE)
            .context("Unable to poll libinput")?;
        self.libinput = Some(libinput);
        info!("libinput ready for seat `{seat_name}`");
        Ok(())
    }

    fn handle_libinput_event(&mut self) -> anyhow::Result<()> {
        let Some(libinput) = self.libinput.as_ref() else {
            return Ok(());
        };

        libinput.dispatch()?;

        let mut key_events = Vec::new();
        libinput.drain_events(|key, state| key_events.push((key, state)))?;
        for (key, state) in key_events {
            self.handle_key_event(key, state);
        }

        Ok(())
    }

    fn handle_key_event(&mut self, key: u32, state: u32) {
        if libinput::is_modifier_key(key) {
            let pressed = state == libinput::KEY_STATE_PRESSED;
            libinput::update_modifier(key, pressed, &mut self.mods);
            return;
        }

        if state != libinput::KEY_STATE_PRESSED {
            return;
        }

        let Some(key_name) = libinput::key_to_name(key) else {
            return;
        };
        self.try_activate_binding(&key_name);
    }

    fn try_activate_binding(&self, key_name: &str) {
        let Some(callback_id) = self
            .keymaps
            .get(&(key_name.to_string(), self.mods))
            .copied()
        else {
            return;
        };

        info!(
            "Activating binding {callback_id} for {key_name} with mods {:?}",
            self.mods
        );
        self.comms
            .dbus(DbusMessage::EmitBindingActivated(callback_id.to_string()));
    }
}

impl MessageRunner for InputState {
    type Message = InputMessage;

    fn new(
        comms: Comms,
        event_loop: Poll,
        channel: mpsc::Receiver<Self::Message>,
        _args: &'static GlobalArgs,
    ) -> anyhow::Result<Self> {
        let device_opener = DEVICE_OPENER
            .get()
            .cloned()
            .context("Restricted device opener was not configured")?;

        Ok(Self {
            comms,
            event_loop,
            channel,
            shutting_down: false,
            keymaps: HashMap::new(),
            mods: Mods::default(),
            libinput: None,
            device_opener,
        })
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut events = Events::with_capacity(128);
        loop {
            if let Err(err) = self.event_loop.poll(&mut events, None) {
                error!("Unable to poll event loop: {err}");
            }

            for event in events.iter() {
                if event.token() == MESSAGE_CHANNEL_TOKEN {
                    while let Ok(msg) = self.channel.try_recv() {
                        if let Err(err) = self.handle_message(msg) {
                            error!("Unable to handle message: {err}");
                        }
                    }
                } else if event.token() == LIBINPUT_TOKEN {
                    if let Err(err) = self.handle_libinput_event() {
                        error!("Unable to handle libinput event: {err}");
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

static DEVICE_OPENER: std::sync::OnceLock<Arc<RestrictedDeviceOpener>> = std::sync::OnceLock::new();

fn normalize_key_name(key_name: &str) -> String {
    key_name.to_ascii_lowercase()
}
