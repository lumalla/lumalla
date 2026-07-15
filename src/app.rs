use std::{
    collections::HashMap,
    num::NonZeroU32,
    pin::Pin,
    process::Child,
    sync::mpsc::Receiver,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::Context;
use log::{debug, error, info, warn};
use lumalla_dbus::{DbusService, run_thread as run_dbus_thread};
use lumalla_display::{ClientConnection, ClientId, DisplayState, Wayland, create_wayland_display};
use lumalla_input::InputState;
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, DbusMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MainMessage, MessageSender,
    message_loop_with_channel,
};
use mio::{Events, Interest, Poll, Token};

pub const LIBSEAT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);
pub const LIBINPUT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 2);
pub const WAYLAND_SOCKET_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 3);

/// Represents the data for the main app thread
struct AppData {
    comms: Comms,
    config_child: Option<Child>,
    dbus_join_handle: JoinHandle<()>,
    // `seat_state` must outlive `input_state`; fields drop in reverse declaration order.
    seat_state: Pin<Box<SeatState>>,
    input_state: InputState,
    shutting_down: bool,
    shutdown_timeout: Option<Instant>,
    wayland: Wayland,
    connected_clients: HashMap<ClientId, ClientConnection>,
    display_state: DisplayState,
}

impl AppData {
    fn new(
        comms: Comms,
        config_child: Option<Child>,
        dbus_join_handle: JoinHandle<()>,
        seat_state: Pin<Box<SeatState>>,
        input_state: InputState,
        wayland: Wayland,
        display_state: DisplayState,
    ) -> Self {
        Self {
            comms,
            config_child,
            dbus_join_handle,
            seat_state,
            input_state,
            shutting_down: false,
            shutdown_timeout: None,
            wayland,
            connected_clients: HashMap::new(),
            display_state,
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
                    self.handle_channel_messages(main_channel);
                }
                LIBSEAT_TOKEN => {
                    if let Err(err) = self.seat_state.dispatch() {
                        error!("Unable to dispatch seat events: {err}");
                    }
                }
                LIBINPUT_TOKEN => {
                    if let Err(err) = self.input_state.dispatch() {
                        error!("Unable to dispatch libinput events: {err}");
                    }
                }
                WAYLAND_SOCKET_TOKEN => {
                    self.connect_client(event_loop);
                }
                token => {
                    self.handle_client_messages(token, event_loop)?;
                }
            }
        }
        Ok(())
    }

    fn handle_channel_messages(&mut self, main_channel: &Receiver<MainMessage>) {
        while let Ok(msg) = main_channel.try_recv() {
            match msg {
                MainMessage::MainSeatEnabled => {
                    self.seat_state.enable_main_seat();
                    if let Ok(seat_name) = self.seat_state.seat_name() {
                        if let Err(err) = self.input_state.enable_seat(&seat_name) {
                            error!("Unable to enable libinput: {err}");
                        }
                    }
                }
                MainMessage::MainSeatDisabled => {
                    self.seat_state.disable_main_seat();
                }
                MainMessage::Shutdown => {
                    if !self.shutting_down {
                        self.init_shutdown();
                    }
                }
            }
        }
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
            }
        }
        for client_id in clients_to_remove {
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
                self.connected_clients.remove(&client_id);
            }
        } else {
            debug!("Received message for unknown client {:?}", client_id);
        }
        Ok(())
    }

    fn connect_client(&mut self, event_loop: &mut Poll) {
        if let Some(mut client) = self.wayland.next_client() {
            let client_id = client.client_id();
            info!("New client connected with id {:?}", client_id);
            if let Err(err) = event_loop.registry().register(
                &mut client,
                Token(WAYLAND_SOCKET_TOKEN.0 + client_id.get() as usize),
                Interest::READABLE,
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
    let display_state = DisplayState::new(comms.clone())?;
    let mut data = AppData::new(
        comms.clone(),
        config_child,
        dbus_join_handle,
        seat_state,
        input_state,
        wayland,
        display_state,
    );
    data.run_event_loop(&mut main_event_loop, main_channel)
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
