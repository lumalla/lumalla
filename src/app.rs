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
    ClientConnection, ClientId, DisplayState, KeyboardModifiers, OutputInfo, SurfaceUpdate,
    Wayland, create_wayland_display,
};
use lumalla_input::{InputState, KeyboardEvent, PointerEvent, SeatEvent, TouchEvent};
use lumalla_renderer::{CursorFrame, PresentStatus, RendererState, SOLID_CLEAR_COLOR, SurfaceFrame};
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, DbusMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MainMessage, MessageSender,
    message_loop_with_channel,
};
use mio::{Events, Interest, Poll, Registry, Token, event::Source, unix::SourceFd};

pub const LIBSEAT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);
pub const LIBINPUT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 2);
pub const UDEV_DRM_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 3);
pub const WAYLAND_SOCKET_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 4);
/// DRM primary-node fds use this high token range to avoid Wayland client tokens.
pub const DRM_DEVICE_TOKEN_BASE: Token = Token(1 << 16);

struct DrmDeviceRegistration {
    fd: RawFd,
    token: Token,
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
            frame_clock: Instant::now(),
            drm_device_poll: HashMap::new(),
            next_drm_device_token: 0,
        }
    }

    fn run_event_loop(
        &mut self,
        event_loop: &mut Poll,
        main_channel: Receiver<MainMessage>,
    ) -> anyhow::Result<()> {
        let mut events = Events::with_capacity(1024);
        loop {
            let (shutdown_now, event_loop_timeout) = self.check_for_shutdown();
            if shutdown_now {
                break;
            }
            if let Err(err) = event_loop.poll(&mut events, event_loop_timeout) {
                warn!("Unable to poll event loop: {err}");
            }
            self.handle_events(&events, &main_channel, event_loop)?;
            self.flush_clients(event_loop);
        }
        // Close seat devices while libseat is still valid. If we leave that to
        // LibInput/DrmDevice Drop during AppData teardown, close_restricted can
        // call libseat_close_device on a destroyed seat and SIGSEGV.
        if let Err(err) = self.input_state.disable_seat() {
            warn!("Unable to suspend libinput during shutdown: {err}");
        }
        if let Err(err) = self.clear_drm_device_poll(event_loop.registry()) {
            warn!("Unable to deregister DRM device fds during shutdown: {err}");
        }
        self.renderer_state
            .deactivate_drm(self.seat_state.as_ref().get_ref());
        Ok(())
    }

    fn handle_events(
        &mut self,
        events: &Events,
        main_channel: &Receiver<MainMessage>,
        event_loop: &mut Poll,
    ) -> anyhow::Result<()> {
        for event in events {
            match event.token() {
                MESSAGE_CHANNEL_TOKEN => {
                    self.handle_channel_messages(main_channel, event_loop)?;
                }
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
                            display_state.handle_pointer_motion(
                                connected_clients,
                                time_msec,
                                dx,
                                dy,
                            );
                        }
                        SeatEvent::Pointer(PointerEvent::Absolute { time_msec, x, y }) => {
                            pointer_changed = true;
                            display_state.handle_pointer_absolute(
                                connected_clients,
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
                            display_state.handle_touch_motion(
                                connected_clients,
                                time_msec,
                                id,
                                x,
                                y,
                            );
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
                        match renderer_state.update_pointer_position(
                            display_state.pointer_position().0.round() as i32,
                            display_state.pointer_position().1.round() as i32,
                        ) {
                            Ok(status) => self.maybe_complete_frame_callbacks(status),
                            Err(err) => error!("Unable to update pointer position: {err:#}"),
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
                                if let Err(err) = self.sync_drm_device_poll(event_loop.registry())
                                {
                                    error!("Unable to refresh DRM device poll fds: {err}");
                                }
                            }
                            self.sync_wayland_output_from_drm();
                            match self
                                .renderer_state
                                .present_enabled_outputs(SOLID_CLEAR_COLOR)
                            {
                                Ok(status) => self.maybe_complete_frame_callbacks(status),
                                Err(err) => {
                                    error!("Unable to present outputs after DRM change: {err:#}");
                                }
                            }
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
                WAYLAND_SOCKET_TOKEN => {
                    self.connect_client(event_loop);
                }
                token => {
                    if self
                        .drm_device_poll
                        .values()
                        .any(|registration| registration.token == token)
                    {
                        self.handle_drm_device_events()?;
                    } else {
                        self.handle_client_messages(token, event_loop)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_channel_messages(
        &mut self,
        main_channel: &Receiver<MainMessage>,
        event_loop: &mut Poll,
    ) -> anyhow::Result<()> {
        while let Ok(msg) = main_channel.try_recv() {
            match msg {
                MainMessage::MainSeatEnabled => {
                    // Callback already set the flag; ignore stale enables if we
                    // were disabled again before this message was processed.
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
                        if let Err(err) = self.sync_drm_device_poll(event_loop.registry()) {
                            error!("Unable to register DRM device poll fds: {err}");
                        }
                        self.sync_wayland_output_from_drm();
                        self.comms.dbus(DbusMessage::EmitDrmDevicesChanged(
                            self.renderer_state.drm_device_states(),
                        ));
                        match self
                            .renderer_state
                            .present_enabled_outputs(SOLID_CLEAR_COLOR)
                        {
                            Ok(status) => self.maybe_complete_frame_callbacks(status),
                            Err(err) => {
                                error!("Unable to present enabled outputs: {err:#}");
                            }
                        }
                        self.comms.dbus(DbusMessage::EmitReady);
                    }
                }
                MainMessage::MainSeatDisabled => {
                    // Callback already acknowledged libseat_disable_seat and cleared
                    // the flag. Ignore stale disables if we were re-enabled since.
                    if self.seat_state.is_enabled() {
                        debug!("Ignoring stale MainSeatDisabled (seat enabled)");
                        continue;
                    }
                    // Suspend input before releasing DRM; close may fail after disable.
                    if let Err(err) = self.input_state.disable_seat() {
                        error!("Unable to disable libinput: {err}");
                    }
                    if let Err(err) = self.clear_drm_device_poll(event_loop.registry()) {
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
                    match self.renderer_state.set_render_device(path) {
                        Ok(status) => self.maybe_complete_frame_callbacks(status),
                        Err(err) => error!("Unable to set render device: {err:#}"),
                    }
                    self.comms.dbus(DbusMessage::EmitDrmDevicesChanged(
                        self.renderer_state.drm_device_states(),
                    ));
                }
                MainMessage::SetOutputConfigs(configs) => {
                    match self.renderer_state.set_output_configs(configs) {
                        Ok(status) => self.maybe_complete_frame_callbacks(status),
                        Err(err) => error!("Unable to set output configs: {err:#}"),
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

    fn flush_clients(&mut self, event_loop: &mut Poll) {
        let mut clients_to_remove = Vec::new();
        for (&client_id, client) in self.connected_clients.iter_mut() {
            if let Err(err) = client.flush() {
                error!("Unable to flush client {:?}: {err}", client_id);
                if let Err(err) = event_loop.registry().deregister(client) {
                    error!("Unable to deregister client {:?}: {err}", client_id);
                }
                clients_to_remove.push(client_id);
            } else if let Err(err) = event_loop.registry().reregister(
                client,
                Token(WAYLAND_SOCKET_TOKEN.0 + client_id.get() as usize),
                client.interest(),
            ) {
                error!("Unable to update client {:?} interests: {err}", client_id);
                clients_to_remove.push(client_id);
            }
        }
        for client_id in clients_to_remove {
            self.display_state.remove_client(client_id);
            match self.renderer_state.remove_client_frames(client_id.get()) {
                Ok(status) => self.maybe_complete_frame_callbacks(status),
                Err(err) => error!("Unable to clear frames for disconnected client: {err:#}"),
            }
            self.connected_clients.remove(&client_id);
        }
    }

    fn handle_client_messages(
        &mut self,
        token: Token,
        event_loop: &mut Poll,
    ) -> anyhow::Result<()> {
        let client_id = ClientId::new(
            NonZeroU32::new((token.0 - WAYLAND_SOCKET_TOKEN.0) as u32)
                .ok_or(anyhow::anyhow!("Created invalid client id from token"))?,
        );
        if let Some(client) = self.connected_clients.get_mut(&client_id) {
            if let Err(err) = client.handle_messages(&mut self.display_state) {
                error!(
                    "Unable to handle messages for client {:?}: {err}",
                    client_id
                );
                if let Err(err) = client.flush() {
                    error!("Unable to flush client {:?}: {err}", client_id);
                }
                if let Err(err) = event_loop.registry().deregister(client) {
                    error!("Unable to deregister client {:?}: {err}", client_id);
                }
                self.display_state.remove_client(client_id);
                match self.renderer_state.remove_client_frames(client_id.get()) {
                    Ok(status) => self.maybe_complete_frame_callbacks(status),
                    Err(err) => {
                        error!("Unable to clear frames for disconnected client: {err:#}")
                    }
                }
                self.connected_clients.remove(&client_id);
            } else {
                self.submit_committed_frames();
                self.sync_pointer_cursor();
            }
        } else {
            debug!("Received message for unknown client {:?}", client_id);
        }
        Ok(())
    }

    fn sync_wayland_output_from_drm(&mut self) {
        let Some((name, width, height, refresh_mhz)) =
            self.renderer_state.primary_output_geometry()
        else {
            return;
        };
        let width_u = width.max(1) as u32;
        let height_u = height.max(1) as u32;
        self.input_state
            .set_output_geometry(width_u, height_u);
        self.display_state
            .set_output_geometry(width_u, height_u);
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
                match self.renderer_state.update_cursor_hotspot(
                    active.hotspot_x,
                    active.hotspot_y,
                ) {
                    Ok(status) => self.maybe_complete_frame_callbacks(status),
                    Err(err) => error!("Unable to update cursor hotspot: {err:#}"),
                }
            }
        } else if self.renderer_state.cursor_surface_key().is_some() {
            // Client hid its cursor; fall back to the compositor default.
            match self.renderer_state.clear_cursor_frame() {
                Ok(status) => self.maybe_complete_frame_callbacks(status),
                Err(err) => error!("Unable to clear client cursor frame: {err:#}"),
            }
        }
    }

    fn submit_committed_frames(&mut self) {
        let updates: Vec<_> = self.display_state.take_surface_updates().collect();
        let mut last_status = PresentStatus { idle: true };
        let mut had_updates = false;
        for update in updates {
            had_updates = true;
            match update {
                SurfaceUpdate::Frame(frame) => {
                    let frame = SurfaceFrame {
                        owner_id: frame.client_id.get(),
                        surface_id: frame.surface_id.get(),
                        pixels: frame.pixels,
                        width: frame.width,
                        height: frame.height,
                        stride: frame.stride,
                        format: frame.format,
                        x: frame.x,
                        y: frame.y,
                        buffer_scale: frame.buffer_scale,
                    };
                    match self.renderer_state.set_surface_frame(frame) {
                        Ok(status) => last_status = status,
                        Err(err) => error!("Unable to queue committed Wayland surface: {err:#}"),
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
                    let cursor = CursorFrame {
                        owner_id: frame.client_id.get(),
                        surface_id: frame.surface_id.get(),
                        pixels: frame.pixels,
                        width: frame.width,
                        height: frame.height,
                        stride: frame.stride,
                        format: frame.format,
                        hotspot_x: hotspot.0,
                        hotspot_y: hotspot.1,
                        buffer_scale: frame.buffer_scale,
                    };
                    match self.renderer_state.set_cursor_frame(cursor) {
                        Ok(status) => last_status = status,
                        Err(err) => error!("Unable to queue committed cursor surface: {err:#}"),
                    }
                }
                SurfaceUpdate::Unmapped {
                    client_id,
                    surface_id,
                } => match self
                    .renderer_state
                    .remove_surface_frame(client_id.get(), surface_id.get())
                {
                    Ok(status) => last_status = status,
                    Err(err) => error!("Unable to clear unmapped Wayland surface: {err:#}"),
                },
            }
        }
        if had_updates {
            self.maybe_complete_frame_callbacks(last_status);
        } else if self.display_state.pending_frame_callback_count() > 0 && last_status.idle {
            self.maybe_complete_frame_callbacks(last_status);
        }
    }

    fn handle_drm_device_events(&mut self) -> anyhow::Result<()> {
        match self.renderer_state.dispatch_page_flips() {
            Ok(status) => self.maybe_complete_frame_callbacks(status),
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
        // Avoid zero so clients that treat 0 as "unset" still see a clock.
        let time_msec = time_msec.max(1);
        self.display_state
            .complete_frame_callbacks(&mut self.connected_clients, time_msec);
    }

    fn sync_drm_device_poll(&mut self, registry: &Registry) -> io::Result<()> {
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
                let fd = registration.fd;
                let mut source = SourceFd(&fd);
                source.deregister(registry)?;
            }
        }

        for (path, fd) in opened {
            if self.drm_device_poll.contains_key(&path) {
                continue;
            }
            let token = Token(DRM_DEVICE_TOKEN_BASE.0 + self.next_drm_device_token);
            self.next_drm_device_token += 1;
            let mut source = SourceFd(&fd);
            source.register(registry, token, Interest::READABLE)?;
            self.drm_device_poll
                .insert(path, DrmDeviceRegistration { fd, token });
        }
        Ok(())
    }

    fn clear_drm_device_poll(&mut self, registry: &Registry) -> io::Result<()> {
        for (_, registration) in self.drm_device_poll.drain() {
            let fd = registration.fd;
            let mut source = SourceFd(&fd);
            source.deregister(registry)?;
        }
        Ok(())
    }

    fn connect_client(&mut self, event_loop: &mut Poll) {
        if let Some(mut client) = self.wayland.next_client() {
            let client_id = client.client_id();
            let interest = client.interest();
            info!("New client connected with id {:?}", client_id);
            if let Err(err) = event_loop.registry().register(
                &mut client,
                Token(WAYLAND_SOCKET_TOKEN.0 + client_id.get() as usize),
                interest,
            ) {
                error!(
                    "Unable to listen on client socket with client id {:?}: {err}",
                    client_id
                );
            } else {
                self.connected_clients.insert(client.client_id(), client);
            }
        }
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

    /// Returns whether the app should shut down now and the time until
    /// the next shutdown check should be performed.
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
    mut main_event_loop: Poll,
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
    let renderer_state = init_and_register_renderer_state(&mut main_event_loop)?;
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

fn init_and_register_renderer_state(main_event_loop: &mut Poll) -> anyhow::Result<RendererState> {
    let mut renderer_state = RendererState::new()?;
    main_event_loop
        .registry()
        .register(&mut renderer_state, UDEV_DRM_TOKEN, Interest::READABLE)
        .context("Unable to listen on DRM udev monitor")?;
    Ok(renderer_state)
}

fn init_and_register_wayland_display(
    socket_path: Option<String>,
    main_event_loop: &mut Poll,
) -> anyhow::Result<Wayland> {
    let mut wayland = create_wayland_display(socket_path)?;
    info!(
        "Created wayland display socket at: {}",
        wayland.socket_path()
    );
    main_event_loop
        .registry()
        .register(&mut wayland, WAYLAND_SOCKET_TOKEN, Interest::READABLE)
        .context("Unable to listen on wayland display socket")?;
    Ok(wayland)
}

fn init_and_register_seat_state(
    comms: Comms,
    main_event_loop: &mut Poll,
) -> anyhow::Result<Pin<Box<SeatState>>> {
    let mut seat_state = Box::new(SeatState::new(comms)?);
    main_event_loop
        .registry()
        .register(seat_state.as_mut(), LIBSEAT_TOKEN, Interest::READABLE)
        .context("Unable to listen on seat state")?;
    Ok(Box::into_pin(seat_state))
}

fn init_and_register_input_state(
    comms: Comms,
    main_event_loop: &mut Poll,
    seat_state: Pin<&SeatState>,
) -> anyhow::Result<InputState> {
    let mut input_state = InputState::new(comms.clone(), seat_state)?;
    main_event_loop
        .registry()
        .register(&mut input_state, LIBINPUT_TOKEN, Interest::READABLE)
        .context("Unable to poll libinput")?;
    Ok(input_state)
}

fn start_dbus_service(
    comms: Comms,
    dbus_event_loop: Poll,
    dbus_channel: Receiver<DbusMessage>,
) -> anyhow::Result<JoinHandle<()>> {
    let dbus_service =
        DbusService::register(comms.clone()).context("Failed to register D-Bus service")?;
    run_dbus_thread(comms, dbus_event_loop, dbus_channel, dbus_service)
        .context("Unable to run D-Bus thread")
}
