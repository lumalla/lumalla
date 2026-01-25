//! Wrapper around the `libseat` crate
//!
//! This module provides the same interface as `libseat.rs` but uses the
//! `libseat` crate instead of custom FFI bindings.

use std::cell::RefCell;
use std::ffi::CStr;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::path::Path;
use std::rc::Rc;

use libseat::{Device, Seat, SeatEvent};
use log::debug;
use lumalla_shared::{Comms, SeatMessage};

/// Pending events from the seat callback
#[derive(Clone)]
struct PendingEvents {
    events: Rc<RefCell<Vec<SeatEvent>>>,
}

impl PendingEvents {
    fn new() -> Self {
        Self {
            events: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn push(&self, event: SeatEvent) {
        self.events.borrow_mut().push(event);
    }

    fn drain(&self) -> Vec<SeatEvent> {
        self.events.borrow_mut().drain(..).collect()
    }
}

/// Safe wrapper around libseat using the `libseat` crate
pub struct LibSeat {
    seat: Seat,
    comms: Comms,
    pending_events: PendingEvents,
    /// Track opened devices so we can close them properly
    #[allow(dead_code)]
    opened_devices: Vec<Device>,
}

impl LibSeat {
    /// Open a new seat
    pub fn new(comms: Comms) -> anyhow::Result<Self> {
        let pending_events = PendingEvents::new();
        let pending_events_clone = pending_events.clone();

        let seat = Seat::open(move |_seat_ref, event| {
            debug!("Seat event received: {:?}", event);
            pending_events_clone.push(event);
        })
        .map_err(|e| anyhow::anyhow!("Failed to open seat: {}", e))?;

        Ok(Self {
            seat,
            comms,
            pending_events,
            opened_devices: Vec::new(),
        })
    }

    /// Get the file descriptor for the seat
    pub fn fd(&mut self) -> anyhow::Result<RawFd> {
        let borrowed_fd = self
            .seat
            .get_fd()
            .map_err(|e| anyhow::anyhow!("Failed to get seat fd: {}", e))?;
        Ok(borrowed_fd.as_raw_fd())
    }

    /// Dispatch all available seat events (non-blocking)
    pub fn dispatch(&mut self) -> anyhow::Result<()> {
        self.dispatch_timeout(0)?;
        Ok(())
    }

    /// Dispatch seat events with a timeout in milliseconds
    pub fn dispatch_timeout(&mut self, timeout_ms: i32) -> anyhow::Result<i32> {
        let count = self
            .seat
            .dispatch(timeout_ms)
            .map_err(|e| anyhow::anyhow!("Failed to dispatch seat events: {}", e))?;

        // Process any pending events that were collected during dispatch
        for event in self.pending_events.drain() {
            match event {
                SeatEvent::Enable => {
                    debug!("Processing seat enable event");
                    self.comms.seat(SeatMessage::SeatEnabled);
                }
                SeatEvent::Disable => {
                    debug!("Processing seat disable event");
                    self.comms.seat(SeatMessage::SeatDisabled);
                }
            }
        }

        Ok(count as i32)
    }

    /// Get the seat name
    pub fn seat_name(&mut self) -> anyhow::Result<String> {
        Ok(self.seat.name().to_string())
    }

    /// Disable the seat
    #[allow(dead_code)]
    pub fn disable_seat(&mut self) -> anyhow::Result<()> {
        self.seat
            .disable()
            .map_err(|e| anyhow::anyhow!("Failed to disable seat: {}", e))
    }

    /// Open a device. Returns (device_id, fd).
    /// The device_id is needed to close the device later.
    pub fn open_device(&mut self, path: &CStr) -> anyhow::Result<(i32, RawFd)> {
        let path_str = path.to_str().map_err(|_| anyhow::anyhow!("Invalid path"))?;
        let path = Path::new(path_str);

        let device = self
            .seat
            .open_device(&path)
            .map_err(|e| anyhow::anyhow!("Failed to open device {}: {}", path_str, e))?;

        let fd = device.as_fd().as_raw_fd();

        // Store the device so it doesn't get dropped (and the fd doesn't get closed)
        // Use the index as a synthetic device_id
        let device_id = self.opened_devices.len() as i32;
        self.opened_devices.push(device);

        Ok((device_id, fd))
    }

    /// Close a device by its device_id (returned from open_device)
    #[allow(dead_code)]
    pub fn close_device(&mut self, device_id: i32) -> anyhow::Result<()> {
        let idx = device_id as usize;
        if idx < self.opened_devices.len() {
            let device = self.opened_devices.remove(idx);
            self.seat
                .close_device(device)
                .map_err(|e| anyhow::anyhow!("Failed to close device: {}", e))?;
        }
        Ok(())
    }

    /// Switch session
    #[allow(dead_code)]
    pub fn switch_session(&mut self, session: i32) -> anyhow::Result<()> {
        self.seat
            .switch_session(session)
            .map_err(|e| anyhow::anyhow!("Failed to switch session: {}", e))
    }
}
