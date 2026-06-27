use std::{
    pin::Pin,
    process::Child,
    sync::mpsc::Receiver,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::Context;
use log::{error, info, warn};
use lumalla_dbus::{DbusService, run_thread as run_dbus_thread};
use lumalla_input::InputState;
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, DbusMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MainMessage, MessageSender,
    message_loop_with_channel,
};
use mio::{Events, Interest, Poll, Token};

pub const LIBSEAT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);
pub const LIBINPUT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 2);

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
}

impl AppData {
    fn new(
        comms: Comms,
        config_child: Option<Child>,
        dbus_join_handle: JoinHandle<()>,
        seat_state: Pin<Box<SeatState>>,
        input_state: InputState,
    ) -> Self {
        Self {
            comms,
            config_child,
            dbus_join_handle,
            seat_state,
            input_state,
            shutting_down: false,
            shutdown_timeout: None,
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
        }
        Ok(())
    }

    fn handle_events(
        &mut self,
        events: &Events,
        main_channel: &Receiver<MainMessage>,
        _event_loop: &mut Poll,
    ) -> anyhow::Result<()> {
        for event in events {
            if event.token() == MESSAGE_CHANNEL_TOKEN {
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
            } else if event.token() == LIBSEAT_TOKEN {
                if let Err(err) = self.seat_state.dispatch() {
                    error!("Unable to dispatch seat events: {err}");
                }
            } else if event.token() == LIBINPUT_TOKEN {
                if let Err(err) = self.input_state.dispatch() {
                    error!("Unable to dispatch libinput events: {err}");
                }
            }
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
    _args: &'static GlobalArgs,
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
    let mut data = AppData::new(
        comms.clone(),
        config_child,
        dbus_join_handle,
        seat_state,
        input_state,
    );
    data.run_event_loop(&mut main_event_loop, main_channel)
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
