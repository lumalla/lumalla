use std::{
    collections::HashMap,
    io,
    num::NonZeroU32,
    os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    pin::Pin,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

use allocator_api2::vec::Vec as ArenaVec;
use anyhow::Context;
use io_uring::types::Timespec;
use libc::PIDFD_THREAD;
use log::{debug, error, info, warn};
use stumpalo::Arena;
use lumalla_dbus::{DbusService, run_thread as run_dbus_thread};
use lumalla_display::{
    ClientId, ConnectedClients, DisplayConfigHost, DisplayHandler, DisplayPresentationNotify,
    DisplayState, ReadResult, SeatInputHandler, Wayland, create_wayland_display,
};
use lumalla_input::{BTN_LEFT, InputState, SeatEvent};
use lumalla_renderer::{RendererState, is_present_wake_token};
use lumalla_screencast::{ScreencastManager, ScreencastWake};
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, Completion, CursorListenPolicy, CursorListenSink, DbusMessage, EventLoop, InjectedInput,
    Interest, MainMessage, MessageSender, OpKind, encode_user_data, message_loop_with_channel,
    monotonic_deadline_after, ring::MESSAGE_CHANNEL_TOKEN,
};

/// Cursor-listen side effects over dbus/config.
struct CommsCursorListen<'a> {
    comms: &'a Comms,
}

impl CursorListenSink for CommsCursorListen<'_> {
    fn cursor_moved(&mut self, x: f64, y: f64, dx: f64, dy: f64) {
        self.comms
            .dbus(DbusMessage::EmitCursorMoved { x, y, dx, dy });
    }

    fn cursor_clicked(&mut self, x: f64, y: f64, button: u32, pressed: bool) {
        self.comms.dbus(DbusMessage::EmitCursorClicked {
            x,
            y,
            button,
            pressed,
        });
    }

    fn cursor_scrolled(&mut self, x: f64, y: f64, axis: u32, value: f64) {
        self.comms
            .dbus(DbusMessage::EmitCursorScrolled { x, y, axis, value });
    }
}

use crate::args::Args;
use crate::screencast_capture::ScreencastCapturePhase;
use crate::screencast_lifecycle::{
    ScreencastLifecycleEffects, ScreencastSessionPeers, ScreencastSessionState,
};
use crate::seat_lifecycle::{SeatDisableOutcome, SeatEnableOutcome, SeatSessionPeers};

pub static SHUTDOWN_TIMEOUT_TIMESPEC: Timespec = Timespec::new().sec(1);
pub const LIBSEAT_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 1;
pub const LIBINPUT_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 2;
pub const UDEV_DRM_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 3;
pub const WAYLAND_ACCEPT_ID: u64 = MESSAGE_CHANNEL_TOKEN + 4;
pub const DBUS_THREAD_FINISHED_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 5;
pub const SHUTDOWN_TIMEOUT_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 6;
/// Short wake to poll in-flight screencast GPU fills without blocking the main loop.
pub const SCREENCAST_GPU_WAKE_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 7;
/// How soon to re-check screencast fence completion after submit.
const SCREENCAST_GPU_POLL_INTERVAL: Duration = Duration::from_millis(1);
/// DRM primary-node fds use this high token range to avoid Wayland client tokens.
pub const DRM_DEVICE_TOKEN_BASE: u64 = 1 << 16;

struct DrmDeviceRegistration {
    fd: RawFd,
    token: u64,
}

/// Main event-loop owner.
///
/// Display ↔ renderer collaboration is phase-local (`DisplayHandler` on client
/// dispatch, `DisplayPresentationNotify` on present/flip, `DisplayConfigHost`
/// for dmabuf / primary geometry). Seat input is phase-local via
/// `SeatInputHandler`. Session enable/disable is sequenced by
/// [`SeatSessionPeers`](crate::seat_lifecycle::SeatSessionPeers). Screencast
/// lifecycle/capture is sequenced by
/// [`ScreencastSessionPeers`](crate::screencast_lifecycle::ScreencastSessionPeers)
/// and [`ScreencastCapturePhase`](crate::screencast_capture::ScreencastCapturePhase)
/// (poll/dbus/present effects stay here).
struct AppData {
    comms: Comms,
    _dbus_thread_completion_fd: OwnedFd,
    // `seat_state` must outlive `input_state`; fields drop in reverse declaration order.
    seat_state: Pin<Box<SeatState>>,
    input_state: InputState,
    shutting_down: bool,
    shutdown_now: bool,
    dbus_thread_finished: bool,
    wayland: Wayland,
    clients: ConnectedClients,
    display_state: DisplayState,
    renderer_state: RendererState,
    screencast: ScreencastManager,
    screencast_session: ScreencastSessionState,
    /// Absolute timeout armed while screencast DMA fills are in flight.
    screencast_gpu_wake_armed: bool,
    screencast_gpu_wake_ts: Box<Timespec>,
    frame_clock: Instant,
    drm_device_poll: HashMap<PathBuf, DrmDeviceRegistration>,
    next_drm_device_token: usize,
    /// Config cursor-listen / consume policy for the seat-input phase.
    cursor_listen: CursorListenPolicy,
}

impl AppData {
    fn new(
        comms: Comms,
        _dbus_thread_completion_fd: OwnedFd,
        seat_state: Pin<Box<SeatState>>,
        input_state: InputState,
        wayland: Wayland,
        display_state: DisplayState,
        renderer_state: RendererState,
    ) -> Self {
        let screencast = ScreencastManager::new({
            let to_main = comms.main_sender();
            std::sync::Arc::new(move |wake| match wake {
                ScreencastWake::StreamReady { stream_id, result } => {
                    let _ = to_main.send(MainMessage::PipewireStreamReady { stream_id, result });
                }
                ScreencastWake::BlitNeeded => {
                    let _ = to_main.send(MainMessage::ScreencastBlitNeeded);
                }
            })
        });
        Self {
            comms,
            _dbus_thread_completion_fd,
            seat_state,
            input_state,
            shutting_down: false,
            shutdown_now: false,
            dbus_thread_finished: false,
            wayland,
            clients: ConnectedClients::new(),
            display_state,
            renderer_state,
            screencast,
            screencast_session: ScreencastSessionState::default(),
            screencast_gpu_wake_armed: false,
            screencast_gpu_wake_ts: Box::new(Timespec::new()),
            frame_clock: Instant::now(),
            drm_device_poll: HashMap::new(),
            next_drm_device_token: 0,
            cursor_listen: CursorListenPolicy::default(),
        }
    }

    fn run_event_loop(
        &mut self,
        event_loop: &mut EventLoop,
        main_channel: Receiver<MainMessage>,
    ) -> anyhow::Result<()> {
        let mut arena = Arena::with_capacity(64 * 1024);
        let mut completions = Vec::with_capacity(64);
        while !self.shutdown_now {
            if let Err(err) = event_loop.wait(&mut completions) {
                warn!("Unable to wait on event loop: {err}");
            }
            let now = Instant::now();
            for completion in completions.drain(..) {
                if let Err(err) =
                    self.handle_completion(event_loop, &main_channel, completion, now, &arena)
                {
                    error!("Unable to handle completion: {err:#}");
                }
                arena.clear();
            }
        }

        if let Err(err) = event_loop.shutdown_drain() {
            warn!("Unable to drain event loop during shutdown: {err}");
        }
        {
            let mut peers = SeatSessionPeers {
                seat: self.seat_state.as_ref().get_ref(),
                input: &mut self.input_state,
                display: &mut self.display_state,
                clients: &mut self.clients,
                render: &mut self.renderer_state,
            };
            if let Err(err) = peers.suspend_devices() {
                warn!("Unable to suspend libinput during shutdown: {err}");
            }
        }
        if let Err(err) = self.clear_drm_device_poll(event_loop) {
            warn!("Unable to deregister DRM device fds during shutdown: {err}");
        }
        Ok(())
    }

    fn handle_completion(
        &mut self,
        event_loop: &mut EventLoop,
        main_channel: &Receiver<MainMessage>,
        completion: Completion,
        _now: Instant,
        arena: &Arena,
    ) -> anyhow::Result<()> {
        match completion.kind {
            OpKind::Wake => {
                self.handle_channel_messages(main_channel, event_loop, arena)?;
                event_loop.rearm_waker()?;
            }
            OpKind::Timeout => {
                self.handle_timeout(event_loop, completion, arena);
            }
            OpKind::Cancel => {}
            OpKind::Accept => {
                self.handle_accept(event_loop, completion, arena);
            }
            OpKind::Recv => {
                self.handle_client_recv(event_loop, completion.id, completion.result, arena)?;
            }
            OpKind::Send => {
                self.handle_client_send(event_loop, completion.id, completion.result, arena)?;
            }
            OpKind::Poll => {
                self.handle_poll(event_loop, completion, arena)?;
            }
            OpKind::Waitid => {}
        }
        Ok(())
    }

    fn handle_poll(
        &mut self,
        event_loop: &mut EventLoop,
        completion: Completion,
        arena: &Arena,
    ) -> anyhow::Result<()> {
        let token = completion.id;
        let terminated = !completion.more();
        if completion.result == -libc::ECANCELED {
            self.rearm_poll_if_still_wanted(event_loop, token, terminated)?;
            return Ok(());
        }
        if completion.result < 0 {
            error!(
                "Poll for token {token} failed: {}",
                io::Error::from_raw_os_error(-completion.result)
            );
            self.rearm_poll_if_still_wanted(event_loop, token, terminated)?;
            return Ok(());
        }

        match token {
            DBUS_THREAD_FINISHED_TOKEN => {
                self.dbus_thread_finished = true;
                if self.shutting_down {
                    self.shutdown_now = true;
                }
            }
            LIBSEAT_TOKEN => {
                if let Err(err) = self.seat_state.dispatch() {
                    error!("Unable to dispatch seat events: {err}");
                }
            }
            LIBINPUT_TOKEN => {
                let mut events = ArenaVec::new_in(arena);
                if let Err(err) = self.input_state.dispatch(|event| events.push(event)) {
                    error!("Unable to dispatch libinput events: {err}");
                } else {
                    self.apply_seat_events(&events, event_loop, arena);
                }
            }
            UDEV_DRM_TOKEN => match self.renderer_state.dispatch() {
                Ok(result) if result.changed() => {
                    info!(
                        "DRM state updated (devices={}, connectors={}): {:?}",
                        result.devices_changed,
                        result.connectors_changed,
                        self.renderer_state.drm_device_states()
                    );
                    if self.seat_state.is_enabled() && self.seat_state.can_open_devices() {
                        if result.devices_changed {
                            if let Err(err) = self
                                .renderer_state
                                .reconcile_drm(self.seat_state.as_ref().get_ref())
                            {
                                error!("Unable to reconcile DRM devices: {err}");
                            }
                            if let Err(err) = self.sync_drm_device_poll(event_loop, arena) {
                                error!("Unable to refresh DRM device poll fds: {err}");
                            }
                            {
                                let mut host = DisplayConfigHost {
                                    state: &mut self.display_state,
                                    clients: &mut self.clients,
                                };
                                if let Err(err) =
                                    self.renderer_state.advertise_dmabuf_formats(&mut host)
                                {
                                    warn!(
                                        "Unable to refresh GPU dmabuf formats after DRM reconcile: {err:#}"
                                    );
                                }
                            }
                        }
                        self.sync_wayland_output_from_drm();
                        self.flush_client_sends(event_loop, arena);
                        self.renderer_state.mark_scene_dirty();
                        self.request_present_immediate(event_loop, arena);
                    }
                    self.comms.dbus(DbusMessage::EmitDrmDevicesChanged(
                        self.renderer_state.drm_device_states(),
                    ));
                }
                Ok(_) => {}
                Err(err) => {
                    error!("Unable to dispatch DRM udev events: {err}");
                }
            },
            token
                if self
                    .drm_device_poll
                    .values()
                    .any(|registration| registration.token == token) =>
            {
                self.handle_drm_device_events(event_loop, arena)?;
            }
            other => {
                debug!("Unexpected poll token: {other}");
            }
        }

        self.rearm_poll_if_still_wanted(event_loop, token, terminated)?;
        Ok(())
    }

    /// Multishot polls stay armed across CQEs with `IORING_CQE_F_MORE`.
    /// Re-submit only when the request actually terminated but we still want it.
    fn rearm_poll_if_still_wanted(
        &mut self,
        event_loop: &mut EventLoop,
        token: u64,
        terminated: bool,
    ) -> io::Result<()> {
        if !terminated || self.shutting_down {
            return Ok(());
        }
        match token {
            LIBSEAT_TOKEN => {
                if let Some(fd) = self.seat_state.poll_fd() {
                    event_loop.submit_poll(fd, Interest::READABLE, LIBSEAT_TOKEN)?;
                }
            }
            LIBINPUT_TOKEN => {
                event_loop.submit_poll(
                    self.input_state.as_raw_fd(),
                    Interest::READABLE,
                    LIBINPUT_TOKEN,
                )?;
            }
            UDEV_DRM_TOKEN => {
                event_loop.submit_poll(
                    self.renderer_state.udev_monitor_fd(),
                    Interest::READABLE,
                    UDEV_DRM_TOKEN,
                )?;
            }
            token => {
                if let Some(registration) = self
                    .drm_device_poll
                    .values()
                    .find(|registration| registration.token == token)
                    .map(|registration| (registration.fd, registration.token))
                {
                    event_loop.submit_poll(registration.0, Interest::READABLE, registration.1)?;
                }
            }
        }
        Ok(())
    }

    fn handle_client_recv(
        &mut self,
        event_loop: &mut EventLoop,
        client_id_raw: u64,
        result: i32,
        arena: &Arena,
    ) -> anyhow::Result<()> {
        let client_id = ClientId::new(
            NonZeroU32::new(client_id_raw as u32)
                .ok_or_else(|| anyhow::anyhow!("Invalid client id {client_id_raw}"))?,
        );
        let Some(client) = self.clients.get_mut(&client_id) else {
            debug!("Recv completion for unknown client {:?}", client_id);
            return Ok(());
        };

        if client.closing {
            client.complete_recv(result);
            self.try_finalize_client(client_id);
            return Ok(());
        }

        let read_result = client.complete_recv(result);
        match read_result {
            ReadResult::EndOfStream => {
                self.begin_client_disconnect(event_loop, client_id, arena);
            }
            ReadResult::NoMoreData => {
                self.arm_client_recv(event_loop, client_id, arena);
            }
            ReadResult::ReadData => {
                if let Err(err) = client.dispatch_pending(&mut DisplayHandler {
                    state: &mut self.display_state,
                    render: &mut self.renderer_state,
                }) {
                    error!(
                        "Unable to handle messages for client {:?}: {err}",
                        client_id
                    );
                    self.begin_client_disconnect(event_loop, client_id, arena);
                } else if client.should_disconnect() {
                    error!(
                        "Client {:?} entered fatal Wayland write/protocol state; disconnecting",
                        client_id
                    );
                    self.begin_client_disconnect(event_loop, client_id, arena);
                } else {
                    self.display_state
                        .flush_pending_keyboard_leaves(&mut self.clients);
                    // Pointer leaves before data-device drag enter so clients never
                    // see enter while still believing they own pointer focus.
                    self.display_state
                        .flush_pending_pointer_leaves(&mut self.clients);
                    self.display_state
                        .flush_pending_data_device(&mut self.clients);
                    self.display_state
                        .flush_pending_activation_configures(&mut self.clients);
                    let content_changed = self.display_state.take_render_content_changed();
                    // Mapping / get_pointer can change who should own the cursor
                    // without a motion event; sync enter/leave now.
                    self.display_state
                        .refresh_pointer_focus(&mut self.clients, arena);
                    let clients = &mut self.clients;
                    let layout_changed = self.display_state.with_render(
                        &mut self.renderer_state,
                        |display| display.drain_pending_geometry(clients),
                    );
                    if content_changed {
                        self.display_state.with_render(&mut self.renderer_state, |display| {
                            display.push_renderer_scenes();
                        });
                    }
                    if content_changed || layout_changed {
                        self.sync_windows_to_dbus();
                    }
                    self.sync_pointer_cursor(event_loop, arena);
                    if self.renderer_state.scene_dirty()
                        || self.display_state.pending_frame_callback_count() > 0
                        || self.display_state.pending_presentation_feedback_count() > 0
                    {
                        self.mark_present_dirty(event_loop, arena);
                    }
                    self.flush_client_sends(event_loop, arena);
                    self.arm_client_recv(event_loop, client_id, arena);
                }
            }
        }
        Ok(())
    }

    fn handle_client_send(
        &mut self,
        event_loop: &mut EventLoop,
        client_id_raw: u64,
        result: i32,
        arena: &Arena,
    ) -> anyhow::Result<()> {
        let client_id = ClientId::new(
            NonZeroU32::new(client_id_raw as u32)
                .ok_or_else(|| anyhow::anyhow!("Invalid client id {client_id_raw}"))?,
        );
        let Some(client) = self.clients.get_mut(&client_id) else {
            debug!("Send completion for unknown client {:?}", client_id);
            return Ok(());
        };

        if client.closing {
            let _ = client.complete_send(result);
            self.try_finalize_client(client_id);
            return Ok(());
        }

        match client.complete_send(result) {
            Ok(_) => {
                if client.should_disconnect() {
                    error!(
                        "Client {:?} entered fatal Wayland write/protocol state; disconnecting",
                        client_id
                    );
                    self.begin_client_disconnect(event_loop, client_id, arena);
                } else {
                    self.arm_client_send(event_loop, client_id, arena);
                }
            }
            Err(err) => {
                error!("Unable to send to client {:?}: {err}", client_id);
                self.begin_client_disconnect(event_loop, client_id, arena);
            }
        }
        Ok(())
    }

    fn begin_client_disconnect(&mut self, event_loop: &mut EventLoop, client_id: ClientId, arena: &Arena) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        if client.closing {
            return;
        }
        client.closing = true;
        // Best-effort delivery of queued events (especially wl_display.error)
        // before cancelling in-flight I/O and dropping the socket.
        if !client.send_in_flight() {
            if let Err(err) = client.flush() {
                debug!(
                    "Unable to flush pending output for disconnecting client {:?}: {err}",
                    client_id
                );
            }
        }
        let fd = client.as_raw_fd();
        if let Err(err) = event_loop.cancel_fd_all(fd) {
            error!("Unable to cancel I/O for client {:?}: {err}", client_id);
        }
        let clients = &mut self.clients;
        self.display_state.with_render(&mut self.renderer_state, |display| {
            display.remove_client(client_id, clients);
        });
        if self.renderer_state.scene_dirty() {
            self.mark_present_dirty(event_loop, arena);
        }
        self.sync_windows_to_dbus();
        // Selection/DnD cleanup may have written to remaining clients.
        self.flush_client_sends(event_loop, arena);
        self.try_finalize_client(client_id);
    }

    fn try_finalize_client(&mut self, client_id: ClientId) {
        let should_remove = self.clients.get(&client_id).is_some_and(|client| {
            client.closing && !client.recv_in_flight() && !client.send_in_flight()
        });
        if should_remove {
            self.clients.remove(client_id);
        }
    }

    fn handle_accept(&mut self, event_loop: &mut EventLoop, completion: Completion, arena: &Arena) {
        if completion.result >= 0 {
            if let Some(client) = self.wayland.client_from_accepted_fd(completion.result) {
                let client_id = client.client_id();
                info!("New client connected with id {:?}", client_id);
                self.clients.insert(client);
                self.arm_client_recv(event_loop, client_id, arena);
            }
        } else if completion.result != -libc::EAGAIN && completion.result != -libc::ECANCELED {
            warn!("Wayland accept failed: {}", completion.result);
        }

        if !completion.more() && !self.shutting_down {
            if let Err(err) = event_loop.submit_accept_multi(self.wayland.as_raw_fd(), WAYLAND_ACCEPT_ID)
            {
                warn!("Unable to re-arm Wayland accept: {err}");
            }
        }
    }

    fn arm_client_recv(&mut self, event_loop: &mut EventLoop, client_id: ClientId, arena: &Arena) {
        if let Err(id) = self.clients.arm_recv(event_loop, client_id) {
            self.begin_client_disconnect(event_loop, id, arena);
        }
    }

    fn arm_client_send(&mut self, event_loop: &mut EventLoop, client_id: ClientId, arena: &Arena) {
        if let Err(id) = self.clients.arm_send(event_loop, client_id) {
            self.begin_client_disconnect(event_loop, id, arena);
        }
    }

    /// After DisplayState (or other) writes that may enqueue Wayland events.
    fn flush_client_sends(&mut self, event_loop: &mut EventLoop, arena: &Arena) {
        self.clients.note_possible_output();
        for id in self.clients.arm_pending_sends(event_loop, arena) {
            self.begin_client_disconnect(event_loop, id, arena);
        }
    }

    fn handle_channel_messages(
        &mut self,
        main_channel: &Receiver<MainMessage>,
        event_loop: &mut EventLoop,
        arena: &Arena,
    ) -> anyhow::Result<()> {
        while let Ok(msg) = main_channel.try_recv() {
            match msg {
                MainMessage::MainSeatEnabled => {
                    let outcome = {
                        let mut peers = SeatSessionPeers {
                            seat: self.seat_state.as_ref().get_ref(),
                            input: &mut self.input_state,
                            display: &mut self.display_state,
                            clients: &mut self.clients,
                            render: &mut self.renderer_state,
                        };
                        peers.on_enabled()
                    };
                    match outcome {
                        SeatEnableOutcome::IgnoredStale
                        | SeatEnableOutcome::DrmActivateFailed => continue,
                        SeatEnableOutcome::Enabled(effects) => {
                            if effects.sync_drm_poll
                                && let Err(err) = self.sync_drm_device_poll(event_loop, arena)
                            {
                                error!("Unable to register DRM device poll fds: {err}");
                            }
                            if effects.outputs_changed {
                                self.emit_outputs_changed();
                            }
                            if effects.set_drm_devices {
                                self.comms.dbus(DbusMessage::SetDrmDevices(
                                    self.renderer_state.drm_device_states(),
                                ));
                            }
                            if effects.present_immediate {
                                self.request_present_immediate(event_loop, arena);
                            }
                            self.comms.dbus(DbusMessage::EmitReady);
                        }
                    }
                }
                MainMessage::MainSeatDisabled => {
                    let outcome = {
                        let mut peers = SeatSessionPeers {
                            seat: self.seat_state.as_ref().get_ref(),
                            input: &mut self.input_state,
                            display: &mut self.display_state,
                            clients: &mut self.clients,
                            render: &mut self.renderer_state,
                        };
                        peers.on_disabled()
                    };
                    if matches!(outcome, SeatDisableOutcome::Disabled)
                        && let Err(err) = self.clear_drm_device_poll(event_loop)
                    {
                        error!("Unable to deregister DRM device poll fds: {err}");
                    }
                }
                MainMessage::SwitchVt(vt) => {
                    info!("Switching to VT {vt}");
                    if let Err(err) = self.seat_state.switch_session(vt) {
                        error!("Unable to switch to VT {vt}: {err}");
                    }
                }
                MainMessage::AddKeymap {
                    key,
                    mods,
                    binding_id,
                    on_release,
                    consume,
                } => {
                    self.input_state
                        .add_keymap(key, mods, binding_id, on_release, consume);
                }
                MainMessage::ClearKeymaps => {
                    self.input_state.clear_keymaps();
                }
                MainMessage::RemoveKeymap { binding_id } => {
                    self.input_state.remove_keymap(&binding_id);
                }
                MainMessage::SetXkb(config) => {
                    if let Err(err) = self.input_state.set_xkb(config) {
                        error!("Unable to set XKB keymap: {err:#}");
                    } else {
                        match self.input_state.keymap_memfd() {
                            Ok(keymap) => {
                                let mods = self.input_state.modifiers();
                                self.display_state.set_keyboard_modifiers(mods);
                                if let Err(err) = self
                                    .display_state
                                    .update_keyboard_keymap(&mut self.clients, keymap)
                                {
                                    error!("Unable to advertise updated XKB keymap: {err:#}");
                                }
                            }
                            Err(err) => {
                                error!("Unable to serialize updated XKB keymap: {err:#}");
                            }
                        }
                    }
                }
                MainMessage::SetRenderDevice(path) => {
                    if let Err(err) = self.renderer_state.set_render_device(path) {
                        error!("Unable to set render device: {err:#}");
                    } else {
                        self.request_present_immediate(event_loop, arena);
                    }
                    self.comms.dbus(DbusMessage::SetDrmDevices(
                        self.renderer_state.drm_device_states(),
                    ));
                }
                MainMessage::SetOutputConfigs(configs) => {
                    if let Err(err) = self.renderer_state.set_output_configs(configs) {
                        error!("Unable to set output configs: {err:#}");
                    } else {
                        self.request_present_immediate(event_loop, arena);
                    }
                    self.comms.dbus(DbusMessage::SetDrmDevices(
                        self.renderer_state.drm_device_states(),
                    ));
                }
                MainMessage::AddOutput(output) => {
                    let name = output.name.clone();
                    let is_virtual = output.is_virtual;
                    if is_virtual {
                        let width = output.size.0.max(1) as u32;
                        let height = output.size.1.max(1) as u32;
                        if let Err(err) = self.renderer_state.add_virtual_output(
                            name.clone(),
                            width,
                            height,
                            output.refresh_mhz,
                        ) {
                            error!("Unable to register virtual output {name}: {err:#}");
                        } else {
                            self.sync_primary_output_geometry();
                            self.request_present_immediate(event_loop, arena);
                        }
                    }
                    if let Err(err) = self.display_state.add_output(
                        lumalla_display::OutputInfo::from(&output),
                        &mut self.clients,
                    ) {
                        error!("Unable to add output {name}: {err:#}");
                    } else {
                        self.renderer_state
                            .set_output_views(&name, output.views.clone());
                    }
                    self.emit_outputs_changed();
                }
                MainMessage::RemoveOutput { name } => {
                    self.renderer_state.remove_virtual_output(&name);
                    self.renderer_state.clear_output_views(&name);
                    if let Err(err) = self
                        .display_state
                        .remove_output(&name, &mut self.clients)
                    {
                        error!("Unable to remove output {name}: {err:#}");
                    }
                    self.sync_primary_output_geometry();
                    self.emit_outputs_changed();
                }
                MainMessage::AddView { output, view } => {
                    match self
                        .display_state
                        .add_view(&output, view.clone(), &mut self.clients)
                    {
                        Ok(true) => {
                            self.renderer_state
                                .set_output_views(&output, self.views_for_output(&output));
                            self.display_state
                                .refresh_pointer_focus(&mut self.clients, arena);
                            self.request_present_immediate(event_loop, arena);
                            self.emit_outputs_changed();
                        }
                        Ok(false) => {}
                        Err(err) => {
                            error!("Unable to add view {output}/{}: {err:#}", view.name);
                        }
                    }
                }
                MainMessage::RemoveView { output, view } => {
                    if let Err(err) =
                        self.display_state
                            .remove_view(&output, &view, &mut self.clients)
                    {
                        error!("Unable to remove view {output}/{view}: {err:#}");
                    } else {
                        self.renderer_state
                            .set_output_views(&output, self.views_for_output(&output));
                        self.display_state
                            .refresh_pointer_focus(&mut self.clients, arena);
                        self.request_present_immediate(event_loop, arena);
                        self.emit_outputs_changed();
                    }
                }
                MainMessage::AddZone(zone) => {
                    self.display_state.add_zone(zone);
                }
                MainMessage::RemoveZone { name } => {
                    if !self.display_state.remove_zone(&name) {
                        error!("Unable to remove unknown zone {name}");
                    }
                }
                MainMessage::AddGuide(guide) => {
                    self.renderer_state.add_guide(guide);
                    self.request_present_immediate(event_loop, arena);
                }
                MainMessage::RemoveGuide { name } => {
                    if !self.renderer_state.remove_guide(&name) {
                        error!("Unable to remove unknown guide {name}");
                    } else {
                        self.request_present_immediate(event_loop, arena);
                    }
                }
                MainMessage::ClearGuides => {
                    self.renderer_state.clear_guides();
                    self.request_present_immediate(event_loop, arena);
                }
                MainMessage::AddWindowToZone { window, zone } => {
                    let clients = &mut self.clients;
                    match self.display_state.with_render(&mut self.renderer_state, |display| {
                        display.add_window_to_zone(window, &zone, clients)
                    }) {
                        Ok(changed) => {
                            self.sync_windows_to_dbus();
                            if changed {
                                self.request_present_immediate(event_loop, arena);
                            }
                        }
                        Err(err) => error!("Unable to add window to zone {zone}: {err}"),
                    }
                }
                MainMessage::RemoveWindowFromZone { window } => {
                    match self.display_state.remove_window_from_zone(window) {
                        Ok(()) => {
                            self.sync_windows_to_dbus();
                        }
                        Err(err) => error!("Unable to remove window from zone: {err}"),
                    }
                }
                MainMessage::Shutdown => {
                    if !self.shutting_down {
                        let effects = self.screencast_peers().shutdown();
                        self.apply_screencast_lifecycle(effects, event_loop, arena);
                        self.init_shutdown(event_loop);
                    }
                }
                MainMessage::InjectInput(input) => {
                    if let Err(err) = self.inject_input(event_loop, input, arena) {
                        error!("Unable to inject input: {err:#}");
                    }
                }
                MainMessage::CaptureScreenshot {
                    request_id,
                    x,
                    y,
                    width,
                    height,
                } => {
                    let mut outputs = ArenaVec::new_in(arena);
                    outputs.extend(
                        self.display_state
                            .outputs()
                            .map(lumalla_shared::Output::from),
                    );
                    let result = self
                        .renderer_state
                        .capture_region(x, y, width, height, &outputs)
                        .map_err(|err| format!("{err:#}"));
                    self.comms
                        .dbus(DbusMessage::ScreenshotCaptured { request_id, result });
                }
                MainMessage::StartPipewireStream {
                    request_id,
                    x,
                    y,
                    width,
                    height,
                    name,
                    max_fps,
                } => {
                    let effects = self.screencast_peers().start_pipewire_region(
                        request_id, x, y, width, height, name, max_fps,
                    );
                    self.apply_screencast_lifecycle(effects, event_loop, arena);
                }
                MainMessage::StartPipewireWindowStream {
                    request_id,
                    window_id,
                    name,
                    max_fps,
                } => {
                    let effects = self.screencast_peers().start_pipewire_window(
                        request_id, window_id, name, max_fps,
                    );
                    self.apply_screencast_lifecycle(effects, event_loop, arena);
                }
                MainMessage::StopPipewireStream { stream_id } => {
                    let effects = self.screencast_peers().stop_pipewire(stream_id);
                    self.apply_screencast_lifecycle(effects, event_loop, arena);
                }
                MainMessage::StartMutterScreenCast {
                    mutter_stream_id,
                    session_id,
                    target,
                    cursor_mode,
                } => {
                    let effects = self.screencast_peers().start_mutter(
                        mutter_stream_id,
                        session_id,
                        target,
                        cursor_mode,
                    );
                    self.apply_screencast_lifecycle(effects, event_loop, arena);
                }
                MainMessage::StopMutterScreenCast { session_id } => {
                    let effects = self.screencast_peers().stop_mutter(session_id);
                    self.apply_screencast_lifecycle(effects, event_loop, arena);
                }
                MainMessage::InjectWaylandClient { fd } => {
                    let raw = fd.into_raw_fd();
                    if let Some(client) = self.wayland.client_from_accepted_fd(raw) {
                        let client_id = client.client_id();
                        info!("New service client connected with id {:?}", client_id);
                        self.clients.insert(client);
                        self.arm_client_recv(event_loop, client_id, arena);
                    }
                }
                MainMessage::PipewireStreamReady { stream_id, result } => {
                    let effects = self.screencast_peers().on_stream_ready(stream_id, result);
                    self.apply_screencast_lifecycle(effects, event_loop, arena);
                }
                MainMessage::ScreencastBlitNeeded => {
                    self.push_screencast_frames(event_loop, arena);
                }
                MainMessage::SetWindow {
                    id,
                    geometry,
                    user_initiated,
                } => {
                    let clients = &mut self.clients;
                    match self.display_state.with_render(&mut self.renderer_state, |display| {
                        display.set_window(id, geometry, user_initiated, clients)
                    }) {
                        Ok(changed) => {
                            self.sync_windows_to_dbus();
                            if changed {
                                self.request_present_immediate(event_loop, arena);
                            }
                        }
                        Err(err) => error!("Unable to set window geometry: {err}"),
                    }
                }
                MainMessage::FocusWindow { id, raise } => {
                    let clients = &mut self.clients;
                    match self.display_state.with_render(&mut self.renderer_state, |display| {
                        display.focus_window(id, raise, clients)
                    }) {
                        Ok(_raised) => {
                            self.sync_windows_to_dbus();
                            self.request_present_immediate(event_loop, arena);
                        }
                        Err(err) => error!("Unable to focus window: {err}"),
                    }
                }
                MainMessage::RaiseWindow { id } => {
                    match self.display_state.with_render(&mut self.renderer_state, |display| {
                        display.raise_window(id)
                    }) {
                        Ok(()) => {
                            self.request_present_immediate(event_loop, arena);
                        }
                        Err(err) => error!("Unable to raise window: {err}"),
                    }
                }
                MainMessage::CloseWindow { id } => {
                    if let Err(err) = self.display_state.close_window(id, &mut self.clients) {
                        error!("Unable to close window: {err}");
                    }
                }
                MainMessage::AddWindowRule(rule) => {
                    self.display_state.add_window_rule(rule);
                }
                MainMessage::ClearWindowRules => {
                    self.display_state.clear_window_rules();
                }
                MainMessage::SetCursorListening {
                    listen_move,
                    listen_click,
                    listen_scroll,
                    consume_move,
                    consume_click,
                    consume_scroll,
                    mods_move,
                    mods_click,
                    mods_scroll,
                } => {
                    self.cursor_listen.set_listening(
                        listen_move,
                        listen_click,
                        listen_scroll,
                        consume_move,
                        consume_click,
                        consume_scroll,
                        mods_move,
                        mods_click,
                        mods_scroll,
                    );
                }
            }
        }
        self.flush_client_sends(event_loop, arena);
        Ok(())
    }

    fn inject_input(
        &mut self,
        event_loop: &mut EventLoop,
        input: InjectedInput,
        arena: &Arena,
    ) -> anyhow::Result<()> {
        let mut events = ArenaVec::new_in(arena);
        let result = match input {
            InjectedInput::Key { name } => self
                .input_state
                .inject_key_name(&name, &mut |event| events.push(event)),
            InjectedInput::TypeText { text } => self
                .input_state
                .inject_type_text(&text, &mut |event| events.push(event)),
            InjectedInput::PointerMove { x, y } => {
                let (x, y) = self.display_state.map_scene_to_pointer(x, y);
                self.input_state
                    .inject_pointer_move(x, y, &mut |event| events.push(event));
                Ok(())
            }
            InjectedInput::PointerClick { x, y, button } => {
                let (x, y) = self.display_state.map_scene_to_pointer(x, y);
                self.input_state.inject_pointer_click(
                    x,
                    y,
                    if button == 0 { BTN_LEFT } else { button },
                    &mut |event| events.push(event),
                );
                Ok(())
            }
            InjectedInput::PointerButton { button, pressed } => {
                self.input_state.inject_pointer_button_state(
                    if button == 0 { BTN_LEFT } else { button },
                    pressed,
                    &mut |event| events.push(event),
                );
                Ok(())
            }
        };
        self.apply_seat_events(&events, event_loop, arena);
        result
    }

    /// Run a seat-input batch through [`SeatInputHandler`] (libinput or inject).
    fn apply_seat_events(
        &mut self,
        events: &[SeatEvent],
        event_loop: &mut EventLoop,
        arena: &Arena,
    ) {
        if events.is_empty() {
            return;
        }
        let pressed_mods = self.input_state.pressed_mods();
        let pointer_changed = {
            let mut cursor_notify = CommsCursorListen {
                comms: &self.comms,
            };
            let mut handler = SeatInputHandler {
                state: &mut self.display_state,
                clients: &mut self.clients,
                render: &mut self.renderer_state,
                listen: &mut self.cursor_listen,
                cursor_notify: &mut cursor_notify,
                pressed_mods,
            };
            let mut pointer_changed = false;
            for &event in events {
                pointer_changed |= handler.handle(event, arena);
            }
            if pointer_changed {
                handler.flush_cursor_moved();
                let _ = handler.sync_pointer_to_render();
            }
            pointer_changed
        };
        self.flush_client_sends(event_loop, arena);
        if pointer_changed && self.renderer_state.scene_dirty() {
            self.mark_present_dirty(event_loop, arena);
        }
    }

    fn emit_outputs_changed(&self) {
        let outputs = self
            .display_state
            .outputs()
            .map(lumalla_shared::Output::from)
            .collect();
        self.comms.dbus(DbusMessage::EmitOutputChanged(outputs));
    }

    fn views_for_output(&self, name: &str) -> Vec<lumalla_shared::View> {
        self.display_state
            .outputs()
            .find(|output| output.name == name)
            .map(|output| output.views.clone())
            .unwrap_or_default()
    }

    fn sync_wayland_output_from_drm(&mut self) {
        self.sync_primary_output_geometry();
    }

    /// Apply primary present-target geometry to input transform and display layout.
    fn sync_primary_output_geometry(&mut self) {
        let mut host = DisplayConfigHost {
            state: &mut self.display_state,
            clients: &mut self.clients,
        };
        let Some(effect) = self.renderer_state.sync_primary_output_geometry(&mut host) else {
            return;
        };
        self.input_state
            .set_output_geometry(effect.width, effect.height);
        if effect.outputs_changed {
            self.emit_outputs_changed();
        }
    }

    fn sync_pointer_cursor(&mut self, event_loop: &mut EventLoop, arena: &Arena) {
        self.display_state
            .with_render(&mut self.renderer_state, |display| {
                display.sync_pointer_cursor_to_render();
            });
        if self.renderer_state.scene_dirty() {
            self.mark_present_dirty(event_loop, arena);
        }
    }

    fn sync_windows_to_dbus(&mut self) {
        self.comms
            .dbus(DbusMessage::SetWindows(self.display_state.window_states()));
    }

    /// Mark all outputs dirty and arm their present wakes.
    fn mark_present_dirty(&mut self, event_loop: &mut EventLoop, arena: &Arena) {
        self.renderer_state.mark_dirty(Instant::now(), arena);
        self.arm_presents(event_loop, arena);
    }

    /// Request an immediate present on all outputs and arm their wakes.
    fn request_present_immediate(&mut self, event_loop: &mut EventLoop, arena: &Arena) {
        self.renderer_state.request_immediate(arena);
        self.arm_presents(event_loop, arena);
    }

    fn frame_time_msec(&self) -> u32 {
        self.frame_clock
            .elapsed()
            .as_millis()
            .min(u128::from(u32::MAX)) as u32
    }

    fn arm_presents(&mut self, event_loop: &mut EventLoop, arena: &Arena) {
        let seat_enabled = self.seat_state.is_enabled();
        let frame_time_msec = self.frame_time_msec();
        let mut notify = DisplayPresentationNotify {
            state: &mut self.display_state,
            clients: &mut self.clients,
        };
        match self.renderer_state.arm_presents(
            event_loop,
            &mut notify,
            seat_enabled,
            frame_time_msec,
            arena,
        ) {
            Ok(result) => {
                self.flush_client_sends(event_loop, arena);
                if !result.presented_outputs.is_empty() {
                    self.push_screencast_frames(event_loop, arena);
                }
            }
            Err(err) => warn!("Unable to arm present wakes: {err}"),
        }
    }

    fn handle_drm_device_events(
        &mut self,
        event_loop: &mut EventLoop,
        arena: &Arena,
    ) -> anyhow::Result<()> {
        let seat_enabled = self.seat_state.is_enabled();
        let frame_time_msec = self.frame_time_msec();
        let mut notify = DisplayPresentationNotify {
            state: &mut self.display_state,
            clients: &mut self.clients,
        };
        match self.renderer_state.on_drm_events(
            event_loop,
            &mut notify,
            seat_enabled,
            frame_time_msec,
            arena,
        ) {
            Ok(_effects) => {
                self.flush_client_sends(event_loop, arena);
            }
            Err(err) => error!("Unable to handle DRM device events: {err}"),
        }
        Ok(())
    }

    fn handle_timeout(
        &mut self,
        event_loop: &mut EventLoop,
        completion: Completion,
        arena: &Arena,
    ) {
        match completion.id {
            SHUTDOWN_TIMEOUT_TOKEN if self.shutting_down => {
                info!("Shutdown timeout reached. Shutting down now");
                self.shutdown_now = true;
            }
            token if is_present_wake_token(token) => {
                let seat_enabled = self.seat_state.is_enabled();
                let frame_time_msec = self.frame_time_msec();
                let mut notify = DisplayPresentationNotify {
                    state: &mut self.display_state,
                    clients: &mut self.clients,
                };
                match self.renderer_state.on_present_timeout(
                    event_loop,
                    token,
                    &mut notify,
                    seat_enabled,
                    frame_time_msec,
                ) {
                    Ok(result) => {
                        self.flush_client_sends(event_loop, arena);
                        if !result.presented_outputs.is_empty() {
                            self.push_screencast_frames(event_loop, arena);
                        }
                    }
                    Err(err) => warn!("Unable to handle present wake timeout: {err}"),
                }
            }
            SCREENCAST_GPU_WAKE_TOKEN => {
                self.screencast_gpu_wake_armed = false;
                self.screencast_capture().finish_ready_dma();
                self.arm_screencast_gpu_wake(event_loop);
            }
            other => {
                warn!("Ignoring unexpected timeout completion id={other}");
            }
        }
    }

    fn sync_drm_device_poll(
        &mut self,
        event_loop: &mut EventLoop,
        arena: &Arena,
    ) -> io::Result<()> {
        let mut opened = ArenaVec::new_in(arena);
        opened.extend(self.renderer_state.opened_drm_fds());

        let mut stale = ArenaVec::new_in(arena);
        for path in self.drm_device_poll.keys() {
            if !opened.iter().any(|(opened_path, _)| opened_path == path) {
                stale.push(path.clone());
            }
        }
        for path in stale {
            if let Some(registration) = self.drm_device_poll.remove(&path) {
                let poll_user_data = encode_user_data(OpKind::Poll, registration.token);
                event_loop.cancel_poll(poll_user_data)?;
            }
        }

        for (path, fd) in opened {
            if self.drm_device_poll.contains_key(&path) {
                continue;
            }
            let token = DRM_DEVICE_TOKEN_BASE + self.next_drm_device_token as u64;
            self.next_drm_device_token += 1;
            event_loop.submit_poll(fd, Interest::READABLE, token)?;
            self.drm_device_poll
                .insert(path, DrmDeviceRegistration { fd, token });
        }
        Ok(())
    }

    fn clear_drm_device_poll(&mut self, event_loop: &mut EventLoop) -> io::Result<()> {
        for (_, registration) in self.drm_device_poll.drain() {
            let poll_user_data = encode_user_data(OpKind::Poll, registration.token);
            event_loop.cancel_poll(poll_user_data)?;
        }
        Ok(())
    }

    fn init_shutdown(&mut self, event_loop: &mut EventLoop) {
        self.shutting_down = true;
        self.comms.dbus(DbusMessage::Shutdown);
        if self.dbus_thread_finished {
            self.shutdown_now = true;
            return;
        }
        // Drop any present-wake absolute timeouts so they cannot race with the
        // one-shot shutdown timeout below.
        if let Err(err) = self.renderer_state.clear_all_present_wakes(event_loop) {
            warn!("Unable to clear present wakes before shutdown timeout: {err}");
        }
        if self.screencast_gpu_wake_armed {
            let _ = event_loop.cancel_timeout(SCREENCAST_GPU_WAKE_TOKEN);
            self.screencast_gpu_wake_armed = false;
        }
        if let Err(err) =
            event_loop.submit_timeout(Pin::new(&SHUTDOWN_TIMEOUT_TIMESPEC), SHUTDOWN_TIMEOUT_TOKEN)
        {
            error!("Unable to schedule shutdown timeout: {err}. Shutting down now",);
            self.shutdown_now = true;
        }
    }

    fn screencast_peers(&mut self) -> ScreencastSessionPeers<'_> {
        ScreencastSessionPeers {
            state: &mut self.screencast_session,
            screencast: &mut self.screencast,
            render: &mut self.renderer_state,
            display: &self.display_state,
        }
    }

    fn screencast_capture(&mut self) -> ScreencastCapturePhase<'_> {
        ScreencastCapturePhase {
            session: &mut self.screencast_session,
            screencast: &mut self.screencast,
            render: &mut self.renderer_state,
            display: &self.display_state,
        }
    }

    fn apply_screencast_lifecycle(
        &mut self,
        effects: ScreencastLifecycleEffects,
        event_loop: &mut EventLoop,
        arena: &Arena,
    ) {
        for msg in effects.dbus {
            self.comms.dbus(msg);
        }
        if effects.mark_present_dirty {
            self.mark_present_dirty(event_loop, arena);
        }
        if effects.push_frames_now {
            self.push_screencast_frames(event_loop, arena);
        }
    }

    /// Arm a short absolute timeout while screencast GPU fills are in flight.
    fn arm_screencast_gpu_wake(&mut self, event_loop: &mut EventLoop) {
        if !self.renderer_state.has_pending_screencast_gpu() {
            if self.screencast_gpu_wake_armed {
                let _ = event_loop.cancel_timeout(SCREENCAST_GPU_WAKE_TOKEN);
                self.screencast_gpu_wake_armed = false;
            }
            return;
        }
        let (sec, nsec) = match monotonic_deadline_after(SCREENCAST_GPU_POLL_INTERVAL) {
            Ok(deadline) => deadline,
            Err(err) => {
                warn!("Unable to compute screencast GPU wake deadline: {err}");
                return;
            }
        };
        if self.screencast_gpu_wake_armed {
            let _ = event_loop.cancel_timeout(SCREENCAST_GPU_WAKE_TOKEN);
            self.screencast_gpu_wake_armed = false;
        }
        *self.screencast_gpu_wake_ts = Timespec::new().sec(sec).nsec(nsec);
        if let Err(err) = event_loop.submit_timeout_absolute(
            Pin::new(self.screencast_gpu_wake_ts.as_ref()),
            SCREENCAST_GPU_WAKE_TOKEN,
        ) {
            warn!("Unable to arm screencast GPU wake: {err}");
            return;
        }
        self.screencast_gpu_wake_armed = true;
    }

    fn push_screencast_frames(&mut self, event_loop: &mut EventLoop, arena: &Arena) {
        let effects = self.screencast_capture().push_frames(arena);
        for msg in effects.dbus {
            self.comms.dbus(msg);
        }
        if effects.arm_gpu_wake {
            self.arm_screencast_gpu_wake(event_loop);
        }
    }
}

pub(crate) fn run_app(
    args: Args,
    mut main_event_loop: EventLoop,
    main_channel: Receiver<MainMessage>,
    to_main: MessageSender<MainMessage>,
) -> anyhow::Result<()> {
    let (dbus_event_loop, dbus_channel, to_dbus) = message_loop_with_channel::<DbusMessage>()?;
    let comms = Comms::new(to_main.clone(), to_dbus);
    let seat_state =
        init_and_register_seat_state(comms.clone(), &mut main_event_loop, args.headless)?;
    let input_state =
        init_and_register_input_state(comms.clone(), &mut main_event_loop, seat_state.as_ref())?;
    let wayland = init_and_register_wayland_display(args.socket_path, &mut main_event_loop)?;
    let mut renderer_state = init_and_register_renderer_state(&mut main_event_loop)?;
    let display_state = init_display_state(&input_state, &mut renderer_state);
    let dbus_thread_completion_fd = start_dbus_service(
        comms.clone(),
        &mut main_event_loop,
        dbus_event_loop,
        dbus_channel,
        &mut renderer_state,
        &wayland,
        args.config_command,
        Some(args.config_args),
    )?;
    let mut data = AppData::new(
        comms,
        dbus_thread_completion_fd,
        seat_state,
        input_state,
        wayland,
        display_state,
        renderer_state,
    );
    data.run_event_loop(&mut main_event_loop, main_channel)
}

fn init_and_register_renderer_state(
    main_event_loop: &mut EventLoop,
) -> anyhow::Result<RendererState> {
    let renderer_state = RendererState::new()?;
    main_event_loop
        .submit_poll(
            renderer_state.udev_monitor_fd(),
            Interest::READABLE,
            UDEV_DRM_TOKEN,
        )
        .context("Unable to listen on DRM udev monitor")?;
    Ok(renderer_state)
}

fn init_and_register_wayland_display(
    socket_path: Option<PathBuf>,
    main_event_loop: &mut EventLoop,
) -> anyhow::Result<Wayland> {
    let wayland = create_wayland_display(socket_path)?;
    info!(
        "Created wayland display socket at: {:?}",
        wayland.socket_path()
    );
    main_event_loop
        .submit_accept_multi(wayland.as_raw_fd(), WAYLAND_ACCEPT_ID)
        .context("Unable to listen on wayland display socket")?;
    Ok(wayland)
}

fn set_wayland_display(service: &DbusService, wayland_socket_path: &Path) {
    let wayland_display = wayland_socket_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    service.set_wayland_display(wayland_display);
}

fn init_display_state(
    input_state: &InputState,
    renderer_state: &mut RendererState,
) -> DisplayState {
    let mut display_state = DisplayState::default();
    match input_state.keymap_memfd() {
        Ok(keymap) => {
            display_state.set_keyboard_keymap(keymap);
            display_state.set_keyboard_modifiers(input_state.modifiers());
        }
        Err(err) => error!("Unable to load xkb keymap for Wayland: {err}"),
    }
    let mut no_clients = ConnectedClients::new();
    {
        let mut host = DisplayConfigHost {
            state: &mut display_state,
            clients: &mut no_clients,
        };
        if let Err(err) = renderer_state.advertise_dmabuf_formats(&mut host) {
            warn!("Unable to query GPU dmabuf formats; using linear defaults: {err:#}");
        }
    }
    display_state
}

fn init_and_register_seat_state(
    comms: Comms,
    main_event_loop: &mut EventLoop,
    headless: bool,
) -> anyhow::Result<Pin<Box<SeatState>>> {
    let seat_state = Box::new(SeatState::new(comms, headless)?);
    if let Some(fd) = seat_state.poll_fd() {
        main_event_loop
            .submit_poll(fd, Interest::READABLE, LIBSEAT_TOKEN)
            .context("Unable to listen on seat state")?;
    }
    Ok(Box::into_pin(seat_state))
}

fn init_and_register_input_state(
    comms: Comms,
    main_event_loop: &mut EventLoop,
    seat_state: Pin<&SeatState>,
) -> anyhow::Result<InputState> {
    let input_state = InputState::new(comms.clone(), seat_state)?;
    main_event_loop
        .submit_poll(input_state.as_raw_fd(), Interest::READABLE, LIBINPUT_TOKEN)
        .context("Unable to poll libinput")?;
    Ok(input_state)
}

fn start_dbus_service(
    comms: Comms,
    main_event_loop: &mut EventLoop,
    dbus_event_loop: EventLoop,
    dbus_channel: Receiver<DbusMessage>,
    renderer_state: &mut RendererState,
    wayland: &Wayland,
    config_command: Option<String>,
    config_args: Option<Vec<String>>,
) -> anyhow::Result<OwnedFd> {
    let dbus_service =
        DbusService::register(comms.clone()).context("Failed to register D-Bus service")?;
    // Set before any Spawn so D-Bus method spawns never see an unset display.
    set_wayland_display(&dbus_service, wayland.socket_path());
    comms.dbus(DbusMessage::SetDrmDevices(
        renderer_state.drm_device_states(),
    ));
    if let Some(config_command) = config_command {
        comms.dbus(DbusMessage::Spawn {
            command: config_command,
            args: config_args.unwrap_or_default(),
        });
    }
    let dbus_thread_id = run_dbus_thread(comms, dbus_event_loop, dbus_channel, dbus_service)?;
    let thread_complete_fd =
        unsafe { libc::syscall(libc::SYS_pidfd_open, dbus_thread_id, PIDFD_THREAD) } as RawFd;
    if thread_complete_fd < 0 {
        return Err(io::Error::last_os_error()).context("Unable to open thread completion fd")?;
    }
    main_event_loop
        .submit_poll(
            thread_complete_fd,
            Interest::READABLE,
            DBUS_THREAD_FINISHED_TOKEN,
        )
        .context("Unable to poll dbus thread completion pid")?;
    Ok(unsafe { OwnedFd::from_raw_fd(thread_complete_fd) })
}
