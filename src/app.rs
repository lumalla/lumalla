use std::{
    collections::HashMap,
    io,
    num::NonZeroU32,
    os::fd::{FromRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    pin::Pin,
    sync::mpsc::Receiver,
    time::Instant,
};

use anyhow::Context;
use io_uring::types::Timespec;
use libc::PIDFD_THREAD;
use log::{debug, error, info, warn};
use lumalla_dbus::{DbusService, run_thread as run_dbus_thread};
use lumalla_display::{
    ClientId, ConnectedClients, DisplayState, KeyboardModifiers, OutputInfo, PresentationFlipInfo,
    ReadResult, SurfaceUpdate, Wayland, create_wayland_display,
};
use lumalla_input::{BTN_LEFT, InputState, KeyboardEvent, PointerEvent, SeatEvent, TouchEvent};
use lumalla_renderer::{
    CursorFrame, DmabufAttachment, OutputDamageRect, PresentStatus, RenderScheduler, RendererState,
    SOLID_CLEAR_COLOR, SurfaceFrame,
};
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, Completion, DbusMessage, EventLoop, InjectedInput, Interest, MainMessage, MessageSender,
    OpKind, encode_user_data, message_loop_with_channel, monotonic_deadline_after,
    ring::MESSAGE_CHANNEL_TOKEN,
};

use crate::args::Args;

pub static SHUTDOWN_TIMEOUT_TIMESPEC: Timespec = Timespec::new().sec(1);
pub const LIBSEAT_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 1;
pub const LIBINPUT_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 2;
pub const UDEV_DRM_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 3;
pub const WAYLAND_ACCEPT_ID: u64 = MESSAGE_CHANNEL_TOKEN + 4;
pub const DBUS_THREAD_FINISHED_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 5;
pub const SHUTDOWN_TIMEOUT_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 6;
/// DRM primary-node fds use this high token range to avoid Wayland client tokens.
pub const DRM_DEVICE_TOKEN_BASE: u64 = 1 << 16;

struct DrmDeviceRegistration {
    fd: RawFd,
    token: u64,
}

/// Represents the data for the main app thread
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
    render_scheduler: RenderScheduler,
    frame_clock: Instant,
    drm_device_poll: HashMap<PathBuf, DrmDeviceRegistration>,
    next_drm_device_token: usize,
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
            render_scheduler: RenderScheduler::default(),
            frame_clock: Instant::now(),
            drm_device_poll: HashMap::new(),
            next_drm_device_token: 0,
        }
    }

    fn run_event_loop(
        &mut self,
        event_loop: &mut EventLoop,
        main_channel: Receiver<MainMessage>,
    ) -> anyhow::Result<()> {
        let mut completions = Vec::with_capacity(64);
        while !self.shutdown_now {
            if let Err(err) = event_loop.wait(&mut completions) {
                warn!("Unable to wait on event loop: {err}");
            }
            let now = Instant::now();
            for completion in completions.drain(..) {
                if let Err(err) = self.handle_completion(event_loop, &main_channel, completion, now)
                {
                    error!("Unable to handle completion: {err:#}");
                }
            }
        }

        if let Err(err) = event_loop.shutdown_drain() {
            warn!("Unable to drain event loop during shutdown: {err}");
        }
        if let Err(err) = self.input_state.disable_seat() {
            warn!("Unable to suspend libinput during shutdown: {err}");
        }
        if let Err(err) = self.clear_drm_device_poll(event_loop) {
            warn!("Unable to deregister DRM device fds during shutdown: {err}");
        }
        self.renderer_state
            .deactivate_drm(self.seat_state.as_ref().get_ref());
        Ok(())
    }

    fn handle_completion(
        &mut self,
        event_loop: &mut EventLoop,
        main_channel: &Receiver<MainMessage>,
        completion: Completion,
        _now: Instant,
    ) -> anyhow::Result<()> {
        match completion.kind {
            OpKind::Wake => {
                self.handle_channel_messages(main_channel, event_loop)?;
                event_loop.rearm_waker()?;
            }
            OpKind::Timeout => {
                self.handle_timeout(event_loop, completion);
            }
            OpKind::Cancel => {}
            OpKind::Accept => {
                self.handle_accept(event_loop, completion);
            }
            OpKind::Recv => {
                self.handle_client_recv(event_loop, completion.id, completion.result)?;
            }
            OpKind::Send => {
                self.handle_client_send(event_loop, completion.id, completion.result)?;
            }
            OpKind::Poll => {
                self.handle_poll(event_loop, completion)?;
            }
            OpKind::Waitid => {}
        }
        Ok(())
    }

    fn handle_poll(
        &mut self,
        event_loop: &mut EventLoop,
        completion: Completion,
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
                let mut events = Vec::new();
                if let Err(err) = self.input_state.dispatch(|event| events.push(event)) {
                    error!("Unable to dispatch libinput events: {err}");
                } else {
                    let mut pointer_changed = false;
                    for event in events {
                        pointer_changed |= self.handle_seat_event(event);
                    }
                    self.flush_client_sends(event_loop);
                    if pointer_changed {
                        if let Err(err) = self.renderer_state.update_pointer_position(
                            self.display_state.pointer_position().0.round() as i32,
                            self.display_state.pointer_position().1.round() as i32,
                        ) {
                            error!("Unable to update pointer position: {err:#}");
                        } else if self.renderer_state.scene_dirty() {
                            self.mark_present_dirty(event_loop);
                        }
                    }
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
                            if let Err(err) = self.sync_drm_device_poll(event_loop) {
                                error!("Unable to refresh DRM device poll fds: {err}");
                            }
                            if let Err(err) = configure_dmabuf_formats(
                                &mut self.display_state,
                                &mut self.renderer_state,
                                &mut self.clients,
                            ) {
                                warn!(
                                    "Unable to refresh GPU dmabuf formats after DRM reconcile: {err:#}"
                                );
                            }
                        }
                        self.sync_wayland_output_from_drm();
                        self.flush_client_sends(event_loop);
                        self.renderer_state.mark_scene_dirty();
                        self.request_present_immediate(event_loop);
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
                self.handle_drm_device_events(event_loop)?;
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
                self.begin_client_disconnect(event_loop, client_id);
            }
            ReadResult::NoMoreData => {
                self.arm_client_recv(event_loop, client_id);
            }
            ReadResult::ReadData => {
                if let Err(err) = client.dispatch_pending(&mut self.display_state) {
                    error!(
                        "Unable to handle messages for client {:?}: {err}",
                        client_id
                    );
                    self.begin_client_disconnect(event_loop, client_id);
                } else if client.should_disconnect() {
                    error!(
                        "Client {:?} entered fatal Wayland write/protocol state; disconnecting",
                        client_id
                    );
                    self.begin_client_disconnect(event_loop, client_id);
                } else {
                    self.display_state
                        .flush_pending_keyboard_leaves(&mut self.clients);
                    self.display_state
                        .flush_pending_activation_configures(&mut self.clients);
                    self.submit_committed_frames(event_loop);
                    // Mapping / get_pointer can change who should own the cursor
                    // without a motion event; sync enter/leave now.
                    self.display_state
                        .refresh_pointer_focus(&mut self.clients);
                    let layout_syncs = self
                        .display_state
                        .drain_pending_geometry(&mut self.clients);
                    self.apply_renderer_layout_syncs(event_loop, &layout_syncs);
                    if !layout_syncs.is_empty() {
                        self.sync_windows_to_dbus();
                    }
                    self.sync_pointer_cursor(event_loop);
                    self.flush_client_sends(event_loop);
                    self.arm_client_recv(event_loop, client_id);
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
                    self.begin_client_disconnect(event_loop, client_id);
                } else {
                    self.arm_client_send(event_loop, client_id);
                }
            }
            Err(err) => {
                error!("Unable to send to client {:?}: {err}", client_id);
                self.begin_client_disconnect(event_loop, client_id);
            }
        }
        Ok(())
    }

    fn begin_client_disconnect(&mut self, event_loop: &mut EventLoop, client_id: ClientId) {
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
        self.display_state.remove_client(client_id);
        if let Err(err) = self.renderer_state.remove_client_frames(client_id.get()) {
            error!("Unable to clear frames for disconnected client: {err:#}");
        } else if self.renderer_state.scene_dirty() {
            self.mark_present_dirty(event_loop);
        }
        self.sync_windows_to_dbus();
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

    fn handle_accept(&mut self, event_loop: &mut EventLoop, completion: Completion) {
        if completion.result >= 0 {
            if let Some(client) = self.wayland.client_from_accepted_fd(completion.result) {
                let client_id = client.client_id();
                info!("New client connected with id {:?}", client_id);
                self.clients.insert(client);
                self.arm_client_recv(event_loop, client_id);
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

    fn arm_client_recv(&mut self, event_loop: &mut EventLoop, client_id: ClientId) {
        if let Err(id) = self.clients.arm_recv(event_loop, client_id) {
            self.begin_client_disconnect(event_loop, id);
        }
    }

    fn arm_client_send(&mut self, event_loop: &mut EventLoop, client_id: ClientId) {
        if let Err(id) = self.clients.arm_send(event_loop, client_id) {
            self.begin_client_disconnect(event_loop, id);
        }
    }

    /// After DisplayState (or other) writes that may enqueue Wayland events.
    fn flush_client_sends(&mut self, event_loop: &mut EventLoop) {
        self.clients.note_possible_output();
        for id in self.clients.arm_pending_sends(event_loop) {
            self.begin_client_disconnect(event_loop, id);
        }
    }

    fn handle_channel_messages(
        &mut self,
        main_channel: &Receiver<MainMessage>,
        event_loop: &mut EventLoop,
    ) -> anyhow::Result<()> {
        while let Ok(msg) = main_channel.try_recv() {
            match msg {
                MainMessage::MainSeatEnabled => {
                    if !self.seat_state.is_enabled() {
                        debug!("Ignoring stale MainSeatEnabled (seat disabled)");
                        continue;
                    }
                    if let Ok(seat_name) = self.seat_state.seat_name() {
                        if self.seat_state.can_open_devices() {
                            if let Err(err) = self.input_state.enable_seat(&seat_name) {
                                error!("Unable to enable libinput: {err}");
                            }
                        } else {
                            info!(
                                "Skipping libinput seat assign (no session backend for device opens)"
                            );
                        }
                        if let Err(err) = self
                            .display_state
                            .activate_main_seat(seat_name, &mut self.clients)
                        {
                            error!("Unable to activate Wayland seat: {err}");
                        }
                    }
                    if self.seat_state.can_open_devices() {
                        if let Err(err) = self
                            .renderer_state
                            .activate_drm(self.seat_state.as_ref().get_ref())
                        {
                            error!("Unable to activate DRM devices: {err}");
                            // Keep waiting for a later successful activate; do not Ready yet.
                            continue;
                        }
                        if let Err(err) = self.sync_drm_device_poll(event_loop) {
                            error!("Unable to register DRM device poll fds: {err}");
                        }
                        if let Err(err) = configure_dmabuf_formats(
                            &mut self.display_state,
                            &mut self.renderer_state,
                            &mut self.clients,
                        ) {
                            warn!(
                                "Unable to refresh GPU dmabuf formats after DRM activate: {err:#}"
                            );
                        }
                        self.sync_wayland_output_from_drm();
                        self.comms.dbus(DbusMessage::EmitDrmDevicesChanged(
                            self.renderer_state.drm_device_states(),
                        ));
                        self.renderer_state.mark_scene_dirty();
                        self.request_present_immediate(event_loop);
                    } else {
                        info!("Skipping DRM activate (no session backend for device opens)");
                        self.sync_primary_output_geometry();
                    }
                    self.comms.dbus(DbusMessage::EmitReady);
                }
                MainMessage::MainSeatDisabled => {
                    if self.seat_state.is_enabled() {
                        debug!("Ignoring stale MainSeatDisabled (seat enabled)");
                        continue;
                    }
                    if let Err(err) = self.input_state.disable_seat() {
                        error!("Unable to disable libinput: {err}");
                    }
                    if let Err(err) = self.clear_drm_device_poll(event_loop) {
                        error!("Unable to deregister DRM device poll fds: {err}");
                    }
                    self.renderer_state
                        .deactivate_drm(self.seat_state.as_ref().get_ref());
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
                MainMessage::SetXkb(config) => {
                    if let Err(err) = self.input_state.set_xkb(config) {
                        error!("Unable to set XKB keymap: {err:#}");
                    } else {
                        match self.input_state.keymap_memfd() {
                            Ok(keymap) => {
                                let mods = self.input_state.modifiers();
                                self.display_state
                                    .set_keyboard_modifiers(KeyboardModifiers {
                                        depressed: mods.depressed,
                                        latched: mods.latched,
                                        locked: mods.locked,
                                        group: mods.group,
                                    });
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
                        self.request_present_immediate(event_loop);
                    }
                    self.comms.dbus(DbusMessage::EmitDrmDevicesChanged(
                        self.renderer_state.drm_device_states(),
                    ));
                }
                MainMessage::SetOutputConfigs(configs) => {
                    if let Err(err) = self.renderer_state.set_output_configs(configs) {
                        error!("Unable to set output configs: {err:#}");
                    } else {
                        self.request_present_immediate(event_loop);
                    }
                    self.comms.dbus(DbusMessage::EmitDrmDevicesChanged(
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
                            self.request_present_immediate(event_loop);
                        }
                    }
                    if let Err(err) = self.display_state.add_output(
                        lumalla_display::OutputInfo::from(&output),
                        &mut self.clients,
                    ) {
                        error!("Unable to add output {name}: {err:#}");
                    }
                    self.emit_outputs_changed();
                }
                MainMessage::RemoveOutput { name } => {
                    self.renderer_state.remove_virtual_output(&name);
                    if let Err(err) = self
                        .display_state
                        .remove_output(&name, &mut self.clients)
                    {
                        error!("Unable to remove output {name}: {err:#}");
                    }
                    self.sync_primary_output_geometry();
                    self.emit_outputs_changed();
                }
                MainMessage::Shutdown => {
                    if !self.shutting_down {
                        self.init_shutdown(event_loop);
                    }
                }
                MainMessage::InjectInput(input) => {
                    if let Err(err) = self.inject_input(event_loop, input) {
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
                    let outputs: Vec<_> = self
                        .display_state
                        .outputs()
                        .map(lumalla_shared::Output::from)
                        .collect();
                    let result = self
                        .renderer_state
                        .capture_region(x, y, width, height, &outputs)
                        .map_err(|err| format!("{err:#}"));
                    self.comms
                        .dbus(DbusMessage::ScreenshotCaptured { request_id, result });
                }
                MainMessage::SetWindow {
                    id,
                    geometry,
                    user_initiated,
                } => {
                    match self.display_state.set_window(
                        id,
                        geometry,
                        user_initiated,
                        &mut self.clients,
                    ) {
                        Ok(layout_syncs) => {
                            self.apply_renderer_layout_syncs(event_loop, &layout_syncs);
                            self.sync_windows_to_dbus();
                            self.request_present_immediate(event_loop);
                        }
                        Err(err) => error!("Unable to set window geometry: {err}"),
                    }
                }
                MainMessage::FocusWindow { id, raise } => {
                    match self
                        .display_state
                        .focus_window(id, raise, &mut self.clients)
                    {
                        Ok(raised) => {
                            if raised {
                                self.sync_renderer_scene();
                            }
                            self.sync_windows_to_dbus();
                            self.request_present_immediate(event_loop);
                        }
                        Err(err) => error!("Unable to focus window: {err}"),
                    }
                }
                MainMessage::RaiseWindow { id } => match self.display_state.raise_window(id) {
                    Ok(()) => {
                        self.sync_renderer_scene();
                        self.request_present_immediate(event_loop);
                    }
                    Err(err) => error!("Unable to raise window: {err}"),
                },
                MainMessage::AddWindowRule(rule) => {
                    self.display_state.add_window_rule(rule);
                }
                MainMessage::ClearWindowRules => {
                    self.display_state.clear_window_rules();
                }
            }
        }
        self.flush_client_sends(event_loop);
        Ok(())
    }

    fn inject_input(
        &mut self,
        event_loop: &mut EventLoop,
        input: InjectedInput,
    ) -> anyhow::Result<()> {
        let mut events = Vec::new();
        let result = match input {
            InjectedInput::Key { name } => self
                .input_state
                .inject_key_name(&name, &mut |event| events.push(event)),
            InjectedInput::TypeText { text } => self
                .input_state
                .inject_type_text(&text, &mut |event| events.push(event)),
            InjectedInput::PointerMove { x, y } => {
                self.input_state
                    .inject_pointer_move(x, y, &mut |event| events.push(event));
                Ok(())
            }
            InjectedInput::PointerClick { x, y, button } => {
                self.input_state.inject_pointer_click(
                    x,
                    y,
                    if button == 0 { BTN_LEFT } else { button },
                    &mut |event| events.push(event),
                );
                Ok(())
            }
        };
        let mut pointer_changed = false;
        for event in events {
            pointer_changed |= self.handle_seat_event(event);
        }
        self.flush_client_sends(event_loop);
        if pointer_changed {
            if let Err(err) = self.renderer_state.update_pointer_position(
                self.display_state.pointer_position().0.round() as i32,
                self.display_state.pointer_position().1.round() as i32,
            ) {
                error!("Unable to update pointer position after input injection: {err:#}");
            } else if self.renderer_state.scene_dirty() {
                self.mark_present_dirty(event_loop);
            }
        }
        result
    }

    fn handle_seat_event(&mut self, event: SeatEvent) -> bool {
        let mut pointer_changed = false;
        match event {
            SeatEvent::Keyboard(KeyboardEvent::Key {
                time_msec,
                key,
                pressed,
            }) => {
                self.display_state.handle_keyboard_key(
                    &mut self.clients,
                    time_msec,
                    key,
                    pressed,
                );
            }
            SeatEvent::Keyboard(KeyboardEvent::Modifiers(modifiers)) => {
                self.display_state.handle_keyboard_modifiers(
                    &mut self.clients,
                    KeyboardModifiers {
                        depressed: modifiers.depressed,
                        latched: modifiers.latched,
                        locked: modifiers.locked,
                        group: modifiers.group,
                    },
                );
            }
            SeatEvent::Pointer(PointerEvent::Motion {
                time_msec,
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
            }) => {
                pointer_changed = true;
                self.display_state.handle_pointer_motion(
                    &mut self.clients,
                    time_msec,
                    dx,
                    dy,
                    dx_unaccel,
                    dy_unaccel,
                );
            }
            SeatEvent::Pointer(PointerEvent::Absolute { time_msec, x, y }) => {
                pointer_changed = true;
                self.display_state.handle_pointer_absolute(
                    &mut self.clients,
                    time_msec,
                    x,
                    y,
                );
            }
            SeatEvent::Pointer(PointerEvent::Button {
                time_msec,
                button,
                pressed,
            }) => {
                self.display_state.handle_pointer_button(
                    &mut self.clients,
                    time_msec,
                    button,
                    pressed,
                );
            }
            SeatEvent::Pointer(PointerEvent::Axis {
                time_msec,
                axis,
                value,
            }) => {
                self.display_state.handle_pointer_axis(
                    &mut self.clients,
                    time_msec,
                    axis,
                    value,
                );
            }
            SeatEvent::Touch(TouchEvent::Down {
                time_msec,
                id,
                x,
                y,
            }) => {
                self.display_state.handle_touch_down(
                    &mut self.clients,
                    time_msec,
                    id,
                    x,
                    y,
                );
            }
            SeatEvent::Touch(TouchEvent::Up { time_msec, id }) => {
                self.display_state
                    .handle_touch_up(&mut self.clients, time_msec, id);
            }
            SeatEvent::Touch(TouchEvent::Motion {
                time_msec,
                id,
                x,
                y,
            }) => {
                self.display_state.handle_touch_motion(
                    &mut self.clients,
                    time_msec,
                    id,
                    x,
                    y,
                );
            }
            SeatEvent::Touch(TouchEvent::Frame) => {
                self.display_state
                    .handle_touch_frame(&mut self.clients);
            }
            SeatEvent::Touch(TouchEvent::Cancel) => {
                self.display_state
                    .handle_touch_cancel(&mut self.clients);
            }
        }
        pointer_changed
    }

    fn emit_outputs_changed(&self) {
        let outputs = self
            .display_state
            .outputs()
            .map(lumalla_shared::Output::from)
            .collect();
        self.comms.dbus(DbusMessage::EmitOutputChanged(outputs));
    }

    fn sync_wayland_output_from_drm(&mut self) {
        self.sync_primary_output_geometry();
    }

    /// Apply primary present-target geometry to input transform, display layout, and refresh.
    fn sync_primary_output_geometry(&mut self) {
        let Some((name, width, height, refresh_mhz)) =
            self.renderer_state.primary_output_geometry()
        else {
            return;
        };
        self.render_scheduler.set_refresh_rate(refresh_mhz);
        let width_u = width.max(1) as u32;
        let height_u = height.max(1) as u32;
        self.input_state.set_output_geometry(width_u, height_u);
        self.display_state.set_output_geometry(width_u, height_u);

        // Only rewrite the primary Wayland output when it already exists and is physical
        // (DRM hotplug sync). Virtual outputs are owned entirely by config `add_output`.
        let is_virtual_primary = self
            .display_state
            .outputs()
            .find(|o| o.name == name)
            .map(|o| o.is_virtual)
            .unwrap_or(true);
        if is_virtual_primary {
            return;
        }

        let info = OutputInfo {
            name: name.clone(),
            description: format!("Lumalla output {name}"),
            x: 0,
            y: 0,
            physical_width_mm: 300,
            physical_height_mm: 200,
            width,
            height,
            refresh_mhz,
            scale: 1,
            is_virtual: false,
        };
        self.display_state
            .update_primary_output(info, &mut self.clients);
    }

    fn sync_pointer_cursor(&mut self, event_loop: &mut EventLoop) {
        if let Some(active) = self.display_state.active_cursor() {
            let key = (active.client_id.get(), active.surface_id.get());
            if self.renderer_state.cursor_surface_key() == Some(key) {
                if let Err(err) = self
                    .renderer_state
                    .update_cursor_hotspot(active.hotspot_x, active.hotspot_y)
                {
                    error!("Unable to update cursor hotspot: {err:#}");
                } else if self.renderer_state.scene_dirty() {
                    self.mark_present_dirty(event_loop);
                }
            }
        } else if self.renderer_state.cursor_surface_key().is_some() {
            if let Err(err) = self.renderer_state.clear_cursor_frame() {
                error!("Unable to clear client cursor frame: {err:#}");
            } else if self.renderer_state.scene_dirty() {
                self.mark_present_dirty(event_loop);
            }
        }
    }

    fn apply_renderer_layout_syncs(
        &mut self,
        event_loop: &mut EventLoop,
        layout_syncs: &[lumalla_display::RendererLayoutSync],
    ) {
        for sync in layout_syncs {
            if let Err(err) = self.renderer_state.update_surface_frame_position(
                sync.owner_id,
                sync.surface_id,
                sync.x,
                sync.y,
            ) {
                error!("Unable to update surface frame position: {err:#}");
            }
        }
        self.sync_renderer_scene();
        if !layout_syncs.is_empty() && self.renderer_state.scene_dirty() {
            self.mark_present_dirty(event_loop);
        }
    }

    fn sync_windows_to_dbus(&mut self) {
        self.comms
            .dbus(DbusMessage::SetWindows(self.display_state.window_states()));
    }

    fn sync_renderer_scene(&mut self) {
        let scene: Vec<_> = self
            .display_state
            .scene_surfaces()
            .into_iter()
            .map(|surface| {
                (
                    surface.client_id.get(),
                    surface.surface_id.get(),
                    surface.x,
                    surface.y,
                )
            })
            .collect();
        self.renderer_state.sync_surface_scene(&scene);
    }

    fn submit_committed_frames(&mut self, event_loop: &mut EventLoop) {
        let updates: Vec<_> = self.display_state.take_surface_updates().collect();
        let had_updates = !updates.is_empty();
        for update in updates {
            match update {
                SurfaceUpdate::Frame(frame) => {
                    let dmabuf = frame.dmabuf.map(|exported| DmabufAttachment {
                        buffer_id: frame.buffer_id.get(),
                        fd: exported.fd,
                        drm_fourcc: exported.drm_fourcc,
                        offset: exported.offset,
                        modifier: exported.modifier,
                    });
                    let surface = SurfaceFrame {
                        owner_id: frame.client_id.get(),
                        surface_id: frame.surface_id.get(),
                        buffer_id: frame.buffer_id.get(),
                        pixels: frame.pixels,
                        width: frame.width,
                        height: frame.height,
                        stride: frame.stride,
                        format: frame.format,
                        x: frame.x,
                        y: frame.y,
                        buffer_scale: frame.buffer_scale,
                        buffer_transform: frame.buffer_transform,
                        surface_width: frame.surface_width,
                        surface_height: frame.surface_height,
                        viewport_src: frame.viewport_src,
                        dmabuf,
                        damage: frame
                            .damage
                            .into_iter()
                            .map(|rect| OutputDamageRect {
                                x: rect.x,
                                y: rect.y,
                                width: rect.width,
                                height: rect.height,
                            })
                            .collect(),
                        buffer_damage: frame
                            .buffer_damage
                            .into_iter()
                            .map(|rect| OutputDamageRect {
                                x: rect.x,
                                y: rect.y,
                                width: rect.width,
                                height: rect.height,
                            })
                            .collect(),
                        full_surface: frame.full_surface,
                    };
                    if let Err(err) = self.renderer_state.set_surface_frame(surface) {
                        error!("Unable to queue committed Wayland surface: {err:#}");
                    }
                }
                SurfaceUpdate::Cursor(frame) => {
                    let hotspot = self
                        .display_state
                        .active_cursor()
                        .filter(|cursor| {
                            cursor.client_id == frame.client_id
                                && cursor.surface_id == frame.surface_id
                        })
                        .map(|cursor| (cursor.hotspot_x, cursor.hotspot_y))
                        .unwrap_or((0, 0));
                    let dmabuf = frame.dmabuf.map(|exported| DmabufAttachment {
                        buffer_id: frame.buffer_id.get(),
                        fd: exported.fd,
                        drm_fourcc: exported.drm_fourcc,
                        offset: exported.offset,
                        modifier: exported.modifier,
                    });
                    let cursor = CursorFrame {
                        owner_id: frame.client_id.get(),
                        surface_id: frame.surface_id.get(),
                        buffer_id: frame.buffer_id.get(),
                        pixels: frame.pixels,
                        width: frame.width,
                        height: frame.height,
                        stride: frame.stride,
                        format: frame.format,
                        hotspot_x: hotspot.0,
                        hotspot_y: hotspot.1,
                        buffer_scale: frame.buffer_scale,
                        buffer_transform: frame.buffer_transform,
                        dmabuf,
                    };
                    if let Err(err) = self.renderer_state.set_cursor_frame(cursor) {
                        error!("Unable to queue committed cursor surface: {err:#}");
                    }
                }
                SurfaceUpdate::Unmapped {
                    client_id,
                    surface_id,
                } => {
                    if let Err(err) = self
                        .renderer_state
                        .remove_surface_frame(client_id.get(), surface_id.get())
                    {
                        error!("Unable to clear unmapped Wayland surface: {err:#}");
                    }
                }
            }
        }
        self.sync_renderer_scene();
        if had_updates {
            self.sync_windows_to_dbus();
        }
        if self.renderer_state.scene_dirty()
            || self.display_state.pending_frame_callback_count() > 0
            || self.display_state.pending_presentation_feedback_count() > 0
        {
            self.mark_present_dirty(event_loop);
        }
    }

    /// Arm (or run) the next present based on [`RenderScheduler::next_wake_at`].
    ///
    /// Future deadlines use the absolute io_uring timeout slot. Due-now work is
    /// presented inline — never arm a past/zero timeout.
    fn arm_present_wake(&mut self, event_loop: &mut EventLoop) -> io::Result<()> {
        let now = Instant::now();
        let wake_at = self.render_scheduler.next_wake_at(
            now,
            self.renderer_state.scene_dirty(),
            self.pending_present_work(),
            self.renderer_state.flip_idle(),
        );
        match wake_at {
            None => event_loop.clear_timeout(),
            Some(at) if at <= now => {
                event_loop.clear_timeout()?;
                self.tick_render_scheduler(event_loop);
                // After present, a flip is usually in flight (`None`). If a
                // future deadline remains, arm it without re-entering due-now.
                let now = Instant::now();
                let wake_at = self.render_scheduler.next_wake_at(
                    now,
                    self.renderer_state.scene_dirty(),
                    self.pending_present_work(),
                    self.renderer_state.flip_idle(),
                );
                match wake_at {
                    Some(at) if at > now => {
                        let remaining = at.saturating_duration_since(now);
                        let (sec, nsec) = monotonic_deadline_after(remaining)?;
                        event_loop.set_absolute_timeout_timespec(sec, nsec)
                    }
                    _ => Ok(()),
                }
            }
            Some(at) => {
                let remaining = at.saturating_duration_since(now);
                debug_assert!(!remaining.is_zero());
                let (sec, nsec) = monotonic_deadline_after(remaining)?;
                event_loop.set_absolute_timeout_timespec(sec, nsec)
            }
        }
    }

    fn mark_present_dirty(&mut self, event_loop: &mut EventLoop) {
        self.render_scheduler.mark_dirty(Instant::now());
        if let Err(err) = self.arm_present_wake(event_loop) {
            warn!("Unable to arm present wake: {err}");
        }
    }

    fn request_present_immediate(&mut self, event_loop: &mut EventLoop) {
        self.render_scheduler.request_immediate();
        if let Err(err) = self.arm_present_wake(event_loop) {
            warn!("Unable to arm present wake: {err}");
        }
    }

    fn tick_render_scheduler(&mut self, event_loop: &mut EventLoop) {
        if !self.seat_state.is_enabled() || self.renderer_state.presents_halted() {
            return;
        }

        let now = Instant::now();
        let scene_dirty = self.renderer_state.scene_dirty();
        let pending_callbacks = self.pending_present_work();
        let flip_idle = self.renderer_state.flip_idle();

        if !self
            .render_scheduler
            .should_present(now, scene_dirty, pending_callbacks, flip_idle)
        {
            return;
        }

        let force = pending_callbacks && !scene_dirty;
        match self.renderer_state.present(SOLID_CLEAR_COLOR, force) {
            Ok(outcome) => {
                if outcome.presented {
                    self.render_scheduler.on_present_started(now);
                    if let Some(timings) = outcome.timings {
                        self.render_scheduler
                            .on_present_finished(timings.render_duration);
                    }
                }
                self.maybe_complete_frame_callbacks(event_loop, outcome.status);
            }
            Err(err) => {
                self.render_scheduler.on_present_started(now);
                error!("Unable to present outputs: {err:#}");
            }
        }
    }

    fn handle_drm_device_events(&mut self, event_loop: &mut EventLoop) -> anyhow::Result<()> {
        match self.renderer_state.dispatch_page_flips() {
            Ok(outcome) => {
                let now = Instant::now();
                if !outcome.completed.is_empty() {
                    let refresh_ns = self
                        .render_scheduler
                        .frame_period()
                        .as_nanos()
                        .min(u128::from(u32::MAX)) as u32;
                    if let Some(flip) = outcome.completed.last() {
                        self.display_state.complete_presentation_feedbacks(
                            &mut self.clients,
                            PresentationFlipInfo {
                                tv_sec: flip.tv_sec,
                                tv_usec: flip.tv_usec,
                                sequence: flip.sequence,
                                refresh_ns,
                            },
                        );
                    }
                    self.render_scheduler.after_flip(
                        now,
                        self.renderer_state.scene_dirty(),
                        self.pending_present_work(),
                    );
                }
                self.maybe_complete_frame_callbacks(event_loop, outcome.status);
                self.flush_client_sends(event_loop);
                if let Err(err) = self.arm_present_wake(event_loop) {
                    warn!("Unable to arm present wake after page flip: {err}");
                }
            }
            Err(err) => error!("Unable to dispatch DRM page-flip events: {err:#}"),
        }
        Ok(())
    }

    fn handle_timeout(&mut self, event_loop: &mut EventLoop, completion: Completion) {
        if completion.id == SHUTDOWN_TIMEOUT_TOKEN {
            info!("Shutdown timeout reached. Shutting down now");
            self.shutdown_now = true;
            return;
        }
        // Absolute present-wake timeout (id is EventLoop timeout_generation).
        if let Err(err) = self.arm_present_wake(event_loop) {
            warn!("Unable to handle present wake timeout: {err}");
        }
    }

    fn maybe_complete_frame_callbacks(
        &mut self,
        event_loop: &mut EventLoop,
        status: PresentStatus,
    ) {
        if !status.idle || self.display_state.pending_frame_callback_count() == 0 {
            return;
        }
        let time_msec = self
            .frame_clock
            .elapsed()
            .as_millis()
            .min(u128::from(u32::MAX)) as u32;
        let time_msec = time_msec.max(1);
        self.display_state
            .complete_frame_callbacks(&mut self.clients, time_msec);
        self.flush_client_sends(event_loop);
    }

    fn sync_drm_device_poll(&mut self, event_loop: &mut EventLoop) -> io::Result<()> {
        let opened: HashMap<PathBuf, RawFd> =
            self.renderer_state.opened_drm_fds().into_iter().collect();

        let stale: Vec<PathBuf> = self
            .drm_device_poll
            .keys()
            .filter(|path| !opened.contains_key(*path))
            .cloned()
            .collect();
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
        if let Err(err) =
            event_loop.submit_timeout(Pin::new(&SHUTDOWN_TIMEOUT_TIMESPEC), SHUTDOWN_TIMEOUT_TOKEN)
        {
            error!("Unable to schedule shutdown timeout: {err}. Shutting down now",);
            self.shutdown_now = true;
        }
    }

    fn pending_present_work(&self) -> bool {
        self.display_state.pending_frame_callback_count() > 0
            || self.display_state.pending_presentation_feedback_count() > 0
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

fn configure_dmabuf_formats(
    display_state: &mut DisplayState,
    renderer_state: &mut RendererState,
    clients: &mut ConnectedClients,
) -> anyhow::Result<()> {
    let formats = renderer_state.supported_dmabuf_formats()?;
    let device_path = renderer_state.dmabuf_feedback_device_path();
    info!(
        "Advertising {} linux-dmabuf format/modifier pairs",
        formats.len()
    );
    display_state.set_dmabuf_formats(formats, device_path.as_deref(), clients);
    Ok(())
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
            let mods = input_state.modifiers();
            display_state.set_keyboard_modifiers(KeyboardModifiers {
                depressed: mods.depressed,
                latched: mods.latched,
                locked: mods.locked,
                group: mods.group,
            });
        }
        Err(err) => error!("Unable to load xkb keymap for Wayland: {err}"),
    }
    let mut no_clients = ConnectedClients::new();
    if let Err(err) = configure_dmabuf_formats(&mut display_state, renderer_state, &mut no_clients)
    {
        warn!("Unable to query GPU dmabuf formats; using linear defaults: {err:#}");
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
