use std::{
    collections::HashMap,
    io,
    num::NonZeroU32,
    os::fd::RawFd,
    path::PathBuf,
    pin::Pin,
    process::{Child, Command},
    sync::mpsc::Receiver,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::Context;
use log::{debug, error, info, warn};
use lumalla_dbus::{DbusService, run_thread as run_dbus_thread};
use lumalla_display::{
    ClientConnection, ClientId, DisplayState, KeyboardModifiers, OutputInfo, ReadResult,
    SurfaceUpdate, Wayland, create_wayland_display,
};
use lumalla_input::{InputState, KeyboardEvent, PointerEvent, SeatEvent, TouchEvent};
use lumalla_renderer::{
    CursorFrame, DmabufAttachment, OutputDamageRect, PresentStatus, RenderScheduler, RendererState,
    SOLID_CLEAR_COLOR, SurfaceFrame,
};
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, Completion, DbusMessage, EventLoop, GlobalArgs, Interest, MESSAGE_CHANNEL_TOKEN,
    MainMessage, MessageSender, OpKind, encode_user_data, message_loop_with_channel,
    monotonic_deadline_after,
};

pub const LIBSEAT_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 1;
pub const LIBINPUT_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 2;
pub const UDEV_DRM_TOKEN: u64 = MESSAGE_CHANNEL_TOKEN + 3;
pub const WAYLAND_ACCEPT_ID: u64 = MESSAGE_CHANNEL_TOKEN + 4;
/// DRM primary-node fds use this high token range to avoid Wayland client tokens.
pub const DRM_DEVICE_TOKEN_BASE: u64 = 1 << 16;

struct DrmDeviceRegistration {
    fd: RawFd,
    token: u64,
}

/// Represents the data for the main app thread
struct AppData {
    comms: Comms,
    config_child: Option<Child>,
    startup_child: Option<Child>,
    dbus_join_handle: JoinHandle<()>,
    // `seat_state` must outlive `input_state`; fields drop in reverse declaration order.
    seat_state: Pin<Box<SeatState>>,
    input_state: InputState,
    shutting_down: bool,
    shutdown_timeout: Option<Instant>,
    wayland: Wayland,
    connected_clients: HashMap<ClientId, ClientConnection>,
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
        config_child: Option<Child>,
        startup_child: Option<Child>,
        dbus_join_handle: JoinHandle<()>,
        seat_state: Pin<Box<SeatState>>,
        input_state: InputState,
        wayland: Wayland,
        display_state: DisplayState,
        renderer_state: RendererState,
        render_scheduler: RenderScheduler,
    ) -> Self {
        Self {
            comms,
            config_child,
            startup_child,
            dbus_join_handle,
            seat_state,
            input_state,
            shutting_down: false,
            shutdown_timeout: None,
            wayland,
            connected_clients: HashMap::new(),
            display_state,
            renderer_state,
            render_scheduler,
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
        loop {
            let now = Instant::now();
            let (shutdown_now, event_loop_timeout) = self.check_for_shutdown();
            if shutdown_now {
                break;
            }
            let render_timeout = self.render_scheduler.poll_timeout(now);
            let poll_timeout = match (event_loop_timeout, render_timeout) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            match poll_timeout {
                Some(duration) => {
                    let (sec, nsec) = monotonic_deadline_after(duration)?;
                    event_loop.set_absolute_timeout_timespec(sec, nsec)?;
                }
                None => {
                    event_loop.clear_timeout()?;
                }
            }

            self.ensure_pending_io(event_loop)?;

            if let Err(err) = event_loop.wait(&mut completions) {
                warn!("Unable to wait on event loop: {err}");
            }

            for completion in completions.drain(..) {
                if let Err(err) = self.handle_completion(event_loop, &main_channel, completion) {
                    error!("Unable to handle completion: {err:#}");
                }
            }

            self.tick_render_scheduler();
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
    ) -> anyhow::Result<()> {
        match completion.kind {
            OpKind::Wake => {
                self.handle_channel_messages(main_channel, event_loop)?;
                event_loop.rearm_waker()?;
            }
            OpKind::Timeout => {}
            OpKind::Cancel => {}
            OpKind::Accept => {
                if completion.result >= 0 {
                    if let Some(client) = self.wayland.client_from_accepted_fd(completion.result) {
                        let client_id = client.client_id();
                        info!("New client connected with id {:?}", client_id);
                        self.connected_clients.insert(client_id, client);
                    }
                } else if completion.result != -libc::EAGAIN {
                    warn!("Wayland accept failed: {}", completion.result);
                }
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
            LIBSEAT_TOKEN => {
                if let Err(err) = self.seat_state.dispatch() {
                    error!("Unable to dispatch seat events: {err}");
                }
            }
            LIBINPUT_TOKEN => {
                let AppData {
                    input_state,
                    display_state,
                    connected_clients,
                    renderer_state,
                    ..
                } = self;
                let mut pointer_changed = false;
                if let Err(err) = input_state.dispatch(|event| match event {
                    SeatEvent::Keyboard(KeyboardEvent::Key {
                        time_msec,
                        key,
                        pressed,
                    }) => {
                        display_state.handle_keyboard_key(
                            connected_clients,
                            time_msec,
                            key,
                            pressed,
                        );
                    }
                    SeatEvent::Keyboard(KeyboardEvent::Modifiers(modifiers)) => {
                        display_state.handle_keyboard_modifiers(
                            connected_clients,
                            KeyboardModifiers {
                                depressed: modifiers.depressed,
                                latched: modifiers.latched,
                                locked: modifiers.locked,
                                group: modifiers.group,
                            },
                        );
                    }
                    SeatEvent::Pointer(PointerEvent::Motion { time_msec, dx, dy }) => {
                        pointer_changed = true;
                        display_state.handle_pointer_motion(connected_clients, time_msec, dx, dy);
                    }
                    SeatEvent::Pointer(PointerEvent::Absolute { time_msec, x, y }) => {
                        pointer_changed = true;
                        display_state.handle_pointer_absolute(connected_clients, time_msec, x, y);
                    }
                    SeatEvent::Pointer(PointerEvent::Button {
                        time_msec,
                        button,
                        pressed,
                    }) => {
                        display_state.handle_pointer_button(
                            connected_clients,
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
                        display_state.handle_pointer_axis(
                            connected_clients,
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
                        display_state.handle_touch_down(connected_clients, time_msec, id, x, y);
                    }
                    SeatEvent::Touch(TouchEvent::Up { time_msec, id }) => {
                        display_state.handle_touch_up(connected_clients, time_msec, id);
                    }
                    SeatEvent::Touch(TouchEvent::Motion {
                        time_msec,
                        id,
                        x,
                        y,
                    }) => {
                        display_state.handle_touch_motion(connected_clients, time_msec, id, x, y);
                    }
                    SeatEvent::Touch(TouchEvent::Frame) => {
                        display_state.handle_touch_frame(connected_clients);
                    }
                    SeatEvent::Touch(TouchEvent::Cancel) => {
                        display_state.handle_touch_cancel(connected_clients);
                    }
                }) {
                    error!("Unable to dispatch libinput events: {err}");
                } else if pointer_changed {
                    if let Err(err) = renderer_state.update_pointer_position(
                        display_state.pointer_position().0.round() as i32,
                        display_state.pointer_position().1.round() as i32,
                    ) {
                        error!("Unable to update pointer position: {err:#}");
                    } else if renderer_state.scene_dirty() {
                        self.render_scheduler.mark_dirty(Instant::now());
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
                    if self.seat_state.is_enabled() {
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
                                &mut self.connected_clients,
                            ) {
                                warn!(
                                    "Unable to refresh GPU dmabuf formats after DRM reconcile: {err:#}"
                                );
                            }
                        }
                        self.sync_wayland_output_from_drm();
                        self.render_scheduler.request_immediate();
                        self.renderer_state.mark_scene_dirty();
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
                self.handle_drm_device_events()?;
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
                event_loop.submit_poll(
                    self.seat_state.as_raw_fd(),
                    Interest::READABLE,
                    LIBSEAT_TOKEN,
                )?;
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
        let Some(client) = self.connected_clients.get_mut(&client_id) else {
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
            ReadResult::NoMoreData => {}
            ReadResult::ReadData => {
                if let Err(err) = client.dispatch_pending(&mut self.display_state) {
                    error!(
                        "Unable to handle messages for client {:?}: {err}",
                        client_id
                    );
                    self.begin_client_disconnect(event_loop, client_id);
                } else if client.send_buffer_limit_exceeded() {
                    error!(
                        "Client {:?} exceeded send buffer limit (unresponsive reader)",
                        client_id
                    );
                    self.begin_client_disconnect(event_loop, client_id);
                } else {
                    self.submit_committed_frames();
                    self.sync_pointer_cursor();
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
        let Some(client) = self.connected_clients.get_mut(&client_id) else {
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
                if client.send_buffer_limit_exceeded() {
                    error!(
                        "Client {:?} exceeded send buffer limit (unresponsive reader)",
                        client_id
                    );
                    self.begin_client_disconnect(event_loop, client_id);
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
        let Some(client) = self.connected_clients.get_mut(&client_id) else {
            return;
        };
        if client.closing {
            return;
        }
        client.closing = true;
        let fd = client.as_raw_fd();
        if let Err(err) = event_loop.cancel_fd_all(fd) {
            error!("Unable to cancel I/O for client {:?}: {err}", client_id);
        }
        self.display_state.remove_client(client_id);
        if let Err(err) = self.renderer_state.remove_client_frames(client_id.get()) {
            error!("Unable to clear frames for disconnected client: {err:#}");
        } else if self.renderer_state.scene_dirty() {
            self.render_scheduler.mark_dirty(Instant::now());
        }
        self.try_finalize_client(client_id);
    }

    fn try_finalize_client(&mut self, client_id: ClientId) {
        let should_remove = self
            .connected_clients
            .get(&client_id)
            .is_some_and(|client| {
                client.closing && !client.recv_in_flight() && !client.send_in_flight()
            });
        if should_remove {
            self.connected_clients.remove(&client_id);
        }
    }

    fn ensure_pending_io(&mut self, event_loop: &mut EventLoop) -> io::Result<()> {
        // Seat/input/udev/DRM use multishot POLL_ADD (armed once at register / hotplug).
        event_loop.submit_accept(
            self.wayland.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            WAYLAND_ACCEPT_ID,
        )?;

        self.ensure_client_io(event_loop)
    }

    fn ensure_client_io(&mut self, event_loop: &mut EventLoop) -> io::Result<()> {
        let client_ids: Vec<ClientId> = self.connected_clients.keys().copied().collect();
        for client_id in client_ids {
            let id = client_id.get() as u64;
            let (fd, closing, needs_recv, needs_send) = {
                let Some(client) = self.connected_clients.get(&client_id) else {
                    continue;
                };
                (
                    client.as_raw_fd(),
                    client.closing,
                    !client.recv_in_flight(),
                    !client.send_in_flight() && client.has_pending_output(),
                )
            };
            if closing {
                continue;
            }
            if needs_recv {
                if let Some(client) = self.connected_clients.get_mut(&client_id) {
                    if let Some(msg) = client.prepare_recv() {
                        unsafe {
                            event_loop.submit_recvmsg(fd, msg, id)?;
                        }
                    }
                }
            }
            if needs_send {
                if let Some(client) = self.connected_clients.get_mut(&client_id) {
                    if let Some(msg) = client.prepare_send() {
                        unsafe {
                            event_loop.submit_sendmsg(fd, msg, id)?;
                        }
                    }
                }
            }
        }
        Ok(())
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
                        if let Err(err) = self.input_state.enable_seat(&seat_name) {
                            error!("Unable to enable libinput: {err}");
                        }
                        if let Err(err) = self
                            .display_state
                            .activate_main_seat(seat_name, self.connected_clients.values_mut())
                        {
                            error!("Unable to activate Wayland seat: {err}");
                        }
                    }
                    if let Err(err) = self
                        .renderer_state
                        .activate_drm(self.seat_state.as_ref().get_ref())
                    {
                        error!("Unable to activate DRM devices: {err}");
                    } else {
                        if let Err(err) = self.sync_drm_device_poll(event_loop) {
                            error!("Unable to register DRM device poll fds: {err}");
                        }
                        if let Err(err) = configure_dmabuf_formats(
                            &mut self.display_state,
                            &mut self.renderer_state,
                            &mut self.connected_clients,
                        ) {
                            warn!(
                                "Unable to refresh GPU dmabuf formats after DRM activate: {err:#}"
                            );
                        }
                        self.sync_wayland_output_from_drm();
                        self.comms.dbus(DbusMessage::EmitDrmDevicesChanged(
                            self.renderer_state.drm_device_states(),
                        ));
                        self.render_scheduler.request_immediate();
                        self.renderer_state.mark_scene_dirty();
                        self.comms.dbus(DbusMessage::EmitReady);
                    }
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
                } => {
                    self.input_state.add_keymap(key, mods, binding_id);
                }
                MainMessage::ClearKeymaps => {
                    self.input_state.clear_keymaps();
                }
                MainMessage::SetRenderDevice(path) => {
                    if let Err(err) = self.renderer_state.set_render_device(path) {
                        error!("Unable to set render device: {err:#}");
                    } else {
                        self.render_scheduler.request_immediate();
                    }
                    self.comms.dbus(DbusMessage::EmitDrmDevicesChanged(
                        self.renderer_state.drm_device_states(),
                    ));
                }
                MainMessage::SetOutputConfigs(configs) => {
                    if let Err(err) = self.renderer_state.set_output_configs(configs) {
                        error!("Unable to set output configs: {err:#}");
                    } else {
                        self.render_scheduler.request_immediate();
                    }
                    self.comms.dbus(DbusMessage::EmitDrmDevicesChanged(
                        self.renderer_state.drm_device_states(),
                    ));
                }
                MainMessage::AddOutput(output) => {
                    let name = output.name.clone();
                    if let Err(err) = self.display_state.add_output(
                        lumalla_display::OutputInfo::from(&output),
                        self.connected_clients.values_mut(),
                    ) {
                        error!("Unable to add output {name}: {err:#}");
                    }
                    self.emit_outputs_changed();
                }
                MainMessage::RemoveOutput { name } => {
                    if let Err(err) = self
                        .display_state
                        .remove_output(&name, self.connected_clients.values_mut())
                    {
                        error!("Unable to remove output {name}: {err:#}");
                    }
                    self.emit_outputs_changed();
                }
                MainMessage::Shutdown => {
                    if !self.shutting_down {
                        self.init_shutdown();
                    }
                }
            }
        }
        Ok(())
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
            .update_primary_output(info, &mut self.connected_clients);
    }

    fn sync_pointer_cursor(&mut self) {
        if let Some(active) = self.display_state.active_cursor() {
            let key = (active.client_id.get(), active.surface_id.get());
            if self.renderer_state.cursor_surface_key() == Some(key) {
                if let Err(err) = self
                    .renderer_state
                    .update_cursor_hotspot(active.hotspot_x, active.hotspot_y)
                {
                    error!("Unable to update cursor hotspot: {err:#}");
                } else if self.renderer_state.scene_dirty() {
                    self.render_scheduler.mark_dirty(Instant::now());
                }
            }
        } else if self.renderer_state.cursor_surface_key().is_some() {
            if let Err(err) = self.renderer_state.clear_cursor_frame() {
                error!("Unable to clear client cursor frame: {err:#}");
            } else if self.renderer_state.scene_dirty() {
                self.render_scheduler.mark_dirty(Instant::now());
            }
        }
    }

    fn submit_committed_frames(&mut self) {
        let updates: Vec<_> = self.display_state.take_surface_updates().collect();
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
        if self.renderer_state.scene_dirty()
            || self.display_state.pending_frame_callback_count() > 0
        {
            self.render_scheduler.mark_dirty(Instant::now());
        }
    }

    fn tick_render_scheduler(&mut self) {
        let now = Instant::now();
        let scene_dirty = self.renderer_state.scene_dirty();
        let pending_callbacks = self.display_state.pending_frame_callback_count() > 0;
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
                self.maybe_complete_frame_callbacks(outcome.status);
            }
            Err(err) => {
                self.render_scheduler.on_present_started(now);
                error!("Unable to present outputs: {err:#}");
            }
        }
    }

    fn handle_drm_device_events(&mut self) -> anyhow::Result<()> {
        match self.renderer_state.dispatch_page_flips() {
            Ok(outcome) => {
                let now = Instant::now();
                if !outcome.completed.is_empty() {
                    self.render_scheduler.after_flip(
                        now,
                        self.renderer_state.scene_dirty(),
                        self.display_state.pending_frame_callback_count() > 0,
                    );
                }
                self.maybe_complete_frame_callbacks(outcome.status);
                self.tick_render_scheduler();
            }
            Err(err) => error!("Unable to dispatch DRM page-flip events: {err:#}"),
        }
        Ok(())
    }

    fn maybe_complete_frame_callbacks(&mut self, status: PresentStatus) {
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
            .complete_frame_callbacks(&mut self.connected_clients, time_msec);
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

    fn init_shutdown(&mut self) {
        self.shutting_down = true;
        self.comms.dbus(DbusMessage::Shutdown);
        if let Some(child) = &mut self.config_child {
            if let Err(err) = child.kill() {
                warn!("Failed to stop config process: {err}");
            }
        }
        self.shutdown_timeout = Some(Instant::now() + Duration::from_millis(1000));
    }

    fn check_for_shutdown(&mut self) -> (bool, Option<Duration>) {
        let startup_finished =
            self.startup_child
                .as_mut()
                .is_some_and(|child| match child.try_wait() {
                    Ok(Some(status)) => {
                        info!("Startup command exited with {status}");
                        true
                    }
                    Ok(None) => false,
                    Err(err) => {
                        warn!("Unable to query startup command: {err}");
                        true
                    }
                });
        if startup_finished {
            self.startup_child = None;
        }
        if !self.shutting_down {
            return (false, None);
        }
        let event_loop_timeout = if let Some(timeout) = self.shutdown_timeout {
            let now = Instant::now();
            if now >= timeout {
                info!("Shutdown timeout reached. Shutting down now");
                return (true, None);
            }

            Some(timeout - now)
        } else {
            None
        };
        if !self.dbus_join_handle.is_finished() {
            return (false, event_loop_timeout);
        }
        if let Some(child) = self.config_child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => return (false, event_loop_timeout),
                Err(_) => {}
            }
        }
        (true, event_loop_timeout)
    }
}

pub(crate) fn run_app(
    args: &'static GlobalArgs,
    mut main_event_loop: EventLoop,
    main_channel: Receiver<MainMessage>,
    to_main: MessageSender<MainMessage>,
    config_child: Option<Child>,
) -> anyhow::Result<()> {
    let (dbus_event_loop, dbus_channel, to_dbus) = message_loop_with_channel::<DbusMessage>()?;
    let comms = Comms::new(to_main.clone(), to_dbus);
    let dbus_join_handle = start_dbus_service(comms.clone(), dbus_event_loop, dbus_channel)?;
    let seat_state = init_and_register_seat_state(comms.clone(), &mut main_event_loop)?;
    let input_state =
        init_and_register_input_state(comms.clone(), &mut main_event_loop, seat_state.as_ref())?;
    let wayland =
        init_and_register_wayland_display(args.socket_path.clone(), &mut main_event_loop)?;
    let wayland_display = wayland_display_env(wayland.socket_path())?;
    comms.dbus(DbusMessage::SetWaylandDisplay(wayland_display.clone()));
    let mut display_state = DisplayState::new(comms.clone())?;
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
    let mut renderer_state = init_and_register_renderer_state(&mut main_event_loop)?;
    let mut no_clients = HashMap::new();
    if let Err(err) =
        configure_dmabuf_formats(&mut display_state, &mut renderer_state, &mut no_clients)
    {
        warn!("Unable to query GPU dmabuf formats; using linear defaults: {err:#}");
    }
    let render_scheduler = RenderScheduler::default();
    comms.dbus(DbusMessage::SetDrmDevices(
        renderer_state.drm_device_states(),
    ));
    let startup_child = spawn_startup_command(&args.startup_command, &wayland_display)?;
    let mut data = AppData::new(
        comms.clone(),
        config_child,
        startup_child,
        dbus_join_handle,
        seat_state,
        input_state,
        wayland,
        display_state,
        renderer_state,
        render_scheduler,
    );
    data.run_event_loop(&mut main_event_loop, main_channel)
}

fn wayland_display_env(wayland_socket: &str) -> anyhow::Result<String> {
    let wayland_display = if wayland_socket.starts_with('/') {
        wayland_socket.to_owned()
    } else {
        std::env::current_dir()?
            .join(wayland_socket)
            .to_string_lossy()
            .into_owned()
    };
    Ok(wayland_display)
}

fn spawn_startup_command(
    startup_command: &[String],
    wayland_display: &str,
) -> anyhow::Result<Option<Child>> {
    let Some((program, program_args)) = startup_command.split_first() else {
        return Ok(None);
    };
    info!("Spawning startup command `{program}` with WAYLAND_DISPLAY={wayland_display}");
    let child = Command::new(program)
        .args(program_args)
        .env("WAYLAND_DISPLAY", wayland_display)
        .spawn()
        .with_context(|| format!("Failed to spawn startup command `{program}`"))?;
    Ok(Some(child))
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
    clients: &mut std::collections::HashMap<
        lumalla_display::ClientId,
        lumalla_display::ClientConnection,
    >,
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
    socket_path: Option<String>,
    main_event_loop: &mut EventLoop,
) -> anyhow::Result<Wayland> {
    let wayland = create_wayland_display(socket_path)?;
    info!(
        "Created wayland display socket at: {}",
        wayland.socket_path()
    );
    main_event_loop
        .submit_accept(
            wayland.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            WAYLAND_ACCEPT_ID,
        )
        .context("Unable to listen on wayland display socket")?;
    Ok(wayland)
}

fn init_and_register_seat_state(
    comms: Comms,
    main_event_loop: &mut EventLoop,
) -> anyhow::Result<Pin<Box<SeatState>>> {
    let seat_state = Box::new(SeatState::new(comms)?);
    main_event_loop
        .submit_poll(seat_state.as_raw_fd(), Interest::READABLE, LIBSEAT_TOKEN)
        .context("Unable to listen on seat state")?;
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
    dbus_event_loop: EventLoop,
    dbus_channel: Receiver<DbusMessage>,
) -> anyhow::Result<JoinHandle<()>> {
    let dbus_service =
        DbusService::register(comms.clone()).context("Failed to register D-Bus service")?;
    run_dbus_thread(comms, dbus_event_loop, dbus_channel, dbus_service)
        .context("Unable to run D-Bus thread")
}
