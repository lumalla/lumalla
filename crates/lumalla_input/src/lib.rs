//! Input handling for Lumalla via libinput.

use std::{io, pin::Pin};

use log::info;
use lumalla_seat::SeatState;
use lumalla_shared::Comms;
use mio::{Interest, Registry, Token, event::Source};

use crate::libinput::LibInput;

mod libinput;

pub struct InputState {
    _comms: Comms,
    libinput: LibInput,
}

impl InputState {
    pub fn new(comms: Comms, seat_state: Pin<&SeatState>) -> anyhow::Result<Self> {
        Ok(Self {
            _comms: comms,
            libinput: LibInput::new(seat_state)?,
        })
    }

    pub fn enable_seat(&mut self, seat_name: &str) -> anyhow::Result<()> {
        self.libinput.assign_seat(seat_name)?;
        self.libinput.resume()
    }

    pub fn disable_seat(&mut self) -> anyhow::Result<()> {
        self.libinput.suspend()
    }

    pub fn dispatch(&mut self) -> anyhow::Result<()> {
        self.libinput.dispatch()?;
        self.libinput
            .drain_events(|key, state| info!("key event: {key:?} {state:?}"));
        Ok(())
    }
}

impl Source for InputState {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.libinput.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.libinput.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        self.libinput.deregister(registry)
    }
}
