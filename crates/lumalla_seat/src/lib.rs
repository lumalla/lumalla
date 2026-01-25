use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, mpsc};

use anyhow::Context;
use log::{debug, error, info};
use lumalla_shared::{
    Comms, DisplayMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner, RendererMessage,
    SeatMessage,
};
use mio::{Events, Poll};

// Use the libseat crate wrapper when the feature is enabled,
// otherwise use the custom FFI bindings
#[cfg(feature = "use-libseat-crate")]
mod libseat_crate;
#[cfg(feature = "use-libseat-crate")]
use crate::libseat_crate::LibSeat;

#[cfg(not(feature = "use-libseat-crate"))]
mod libseat;
#[cfg(not(feature = "use-libseat-crate"))]
use crate::libseat::LibSeat;

// Note: We don't register the seat fd with mio - libseat handles its own polling

pub struct SeatState {
    comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<SeatMessage>,
    shutting_down: bool,
    seat: LibSeat,
    seat_enabled: bool,
}

impl SeatState {
    fn handle_message(&mut self, message: SeatMessage) -> anyhow::Result<()> {
        match message {
            SeatMessage::Shutdown => {
                self.shutting_down = true;
            }
            SeatMessage::SeatEnabled => {
                info!("Seat enabled");
                self.seat_enabled = true;

                let seat_name = self.seat.seat_name().context("Failed to get seat name")?;
                self.comms
                    .display(DisplayMessage::ActivateSeat(seat_name.clone()));
                self.comms
                    .renderer(RendererMessage::SeatSessionCreated { seat_name });
            }
            SeatMessage::SeatDisabled => {
                info!("Seat disabled");
                self.seat_enabled = false;
            }
            SeatMessage::OpenDevice { path } => {
                self.handle_open_device(path)?;
            }
        }

        Ok(())
    }

    fn handle_open_device(&mut self, path: std::path::PathBuf) -> anyhow::Result<()> {
        debug!(
            "handle_open_device called for {:?}, seat_enabled={}",
            path, self.seat_enabled
        );

        let path_str = path.to_str().context("Device path is not valid UTF-8")?;
        let c_path = CString::new(path_str).context("Device path contains null byte")?;

        debug!("Calling seat.open_device()");
        let (device_id, raw_fd) = self.seat.open_device(&c_path)?;
        debug!(
            "seat.open_device() returned device_id={}, fd={}",
            device_id, raw_fd
        );

        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        self.comms
            .renderer(RendererMessage::FileOpenedInSession { path, fd });

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
        // mut needed for libseat crate feature, not for custom FFI
        #[allow(unused_mut)]
        let mut seat = LibSeat::new(comms.clone()).context("Failed to create seat")?;

        // NOTE: We intentionally do NOT register the seat fd with mio.
        // Instead, we use libseat's dispatch_timeout() which handles its own polling.
        // This avoids potential conflicts between mio's epoll and libseat's internal handling.

        Ok(Self {
            comms,
            event_loop,
            channel,
            shutting_down: false,
            seat,
            seat_enabled: false,
        })
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut events = Events::with_capacity(128);
        loop {
            // FIRST: Dispatch seat events - this must happen before processing messages
            // to ensure the seat is in a consistent state
            match self.seat.dispatch_timeout(50) {
                Ok(n) if n > 0 => {
                    debug!("Dispatched {} seat event(s)", n);
                }
                Ok(_) => {}
                Err(err) => {
                    error!("Failed to dispatch seat events: {err}");
                }
            }

            // Use a short poll timeout
            let poll_timeout = Some(std::time::Duration::from_millis(10));
            if let Err(err) = self.event_loop.poll(&mut events, poll_timeout) {
                error!("Unable to poll event loop: {err}");
            }

            // Process channel messages
            for event in events.iter() {
                if event.token() == MESSAGE_CHANNEL_TOKEN {
                    while let Ok(msg) = self.channel.try_recv() {
                        debug!("Processing message: {:?}", msg);
                        if let Err(err) = self.handle_message(msg) {
                            error!("Unable to handle message: {err}");
                        }
                    }
                }
            }

            // Also check for messages that might have arrived without waking
            while let Ok(msg) = self.channel.try_recv() {
                debug!("Processing message (non-event): {:?}", msg);
                if let Err(err) = self.handle_message(msg) {
                    error!("Unable to handle message: {err}");
                }
            }

            if self.shutting_down {
                break;
            }
        }

        Ok(())
    }
}
