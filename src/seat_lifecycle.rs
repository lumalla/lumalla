//! Session enable/disable orchestration across seat, libinput, Wayland, and DRM.
//!
//! Event-loop poll registration and dbus emits stay in [`crate::app`]; this module
//! only sequences the peer crates and reports side effects.

use log::{debug, error, info, warn};
use lumalla_display::{ConnectedClients, DisplayConfigHost, DisplayState};
use lumalla_input::InputState;
use lumalla_renderer::RendererState;
use lumalla_seat::SeatState;

/// Peers required to bring the session up or tear DRM/libinput down.
pub struct SeatSessionPeers<'a> {
    pub seat: &'a SeatState,
    pub input: &'a mut InputState,
    pub display: &'a mut DisplayState,
    pub clients: &'a mut ConnectedClients,
    pub render: &'a mut RendererState,
}

/// Event-loop / dbus work the app must perform after a successful enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeatEnableEffects {
    pub sync_drm_poll: bool,
    pub set_drm_devices: bool,
    pub present_immediate: bool,
    pub outputs_changed: bool,
}

/// Result of handling [`lumalla_shared::MainMessage::MainSeatEnabled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatEnableOutcome {
    /// Seat already disabled again; ignore the stale message.
    IgnoredStale,
    /// DRM activate failed; wait for a later successful enable (no Ready).
    DrmActivateFailed,
    Enabled(SeatEnableEffects),
}

/// Result of handling [`lumalla_shared::MainMessage::MainSeatDisabled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatDisableOutcome {
    IgnoredStale,
    Disabled,
}

impl SeatSessionPeers<'_> {
    pub fn on_enabled(&mut self) -> SeatEnableOutcome {
        if !self.seat.is_enabled() {
            debug!("Ignoring stale MainSeatEnabled (seat disabled)");
            return SeatEnableOutcome::IgnoredStale;
        }

        if let Ok(seat_name) = self.seat.seat_name() {
            if self.seat.can_open_devices() {
                if let Err(err) = self.input.enable_seat(&seat_name) {
                    error!("Unable to enable libinput: {err}");
                }
            } else {
                info!(
                    "Skipping libinput seat assign (no session backend for device opens)"
                );
            }
            if let Err(err) = self.display.activate_main_seat(seat_name, self.clients) {
                error!("Unable to activate Wayland seat: {err}");
            }
        }

        if self.seat.can_open_devices() {
            if let Err(err) = self.render.activate_drm(self.seat) {
                error!("Unable to activate DRM devices: {err}");
                return SeatEnableOutcome::DrmActivateFailed;
            }
            {
                let mut host = DisplayConfigHost {
                    state: self.display,
                    clients: self.clients,
                };
                if let Err(err) = self.render.advertise_dmabuf_formats(&mut host) {
                    warn!(
                        "Unable to refresh GPU dmabuf formats after DRM activate: {err:#}"
                    );
                }
            }
            let outputs_changed = self.sync_geometry();
            self.render.mark_scene_dirty();
            SeatEnableOutcome::Enabled(SeatEnableEffects {
                sync_drm_poll: true,
                set_drm_devices: true,
                present_immediate: true,
                outputs_changed,
            })
        } else {
            info!("Skipping DRM activate (no session backend for device opens)");
            let outputs_changed = self.sync_geometry();
            SeatEnableOutcome::Enabled(SeatEnableEffects {
                sync_drm_poll: false,
                set_drm_devices: false,
                present_immediate: false,
                outputs_changed,
            })
        }
    }

    pub fn on_disabled(&mut self) -> SeatDisableOutcome {
        if self.seat.is_enabled() {
            debug!("Ignoring stale MainSeatDisabled (seat enabled)");
            return SeatDisableOutcome::IgnoredStale;
        }
        if let Err(err) = self.suspend_devices() {
            error!("Unable to disable libinput: {err}");
        }
        SeatDisableOutcome::Disabled
    }

    /// Unconditional libinput suspend + DRM deactivate (shutdown / disable paths).
    pub fn suspend_devices(&mut self) -> anyhow::Result<()> {
        let result = self.input.disable_seat();
        self.render.deactivate_drm(self.seat);
        result
    }

    fn sync_geometry(&mut self) -> bool {
        let mut host = DisplayConfigHost {
            state: self.display,
            clients: self.clients,
        };
        let Some(effect) = self.render.sync_primary_output_geometry(&mut host) else {
            return false;
        };
        self.input
            .set_output_geometry(effect.width, effect.height);
        effect.outputs_changed
    }
}
