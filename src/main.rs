use std::{
    env::args,
    fs::OpenOptions,
    io::Write,
    process::{Child, Command},
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::Context;
use env_logger::{Builder, Target};
use log::{error, info, warn};
use lumalla_dbus::{DbusService, run_thread as run_dbus_thread};
use lumalla_display::DisplayState;
use lumalla_input::{OpenRequest, RestrictedDeviceOpener, set_device_opener, InputState};
use lumalla_renderer::RendererState;
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, DbusMessage, DisplayMessage, GlobalArgs, InputMessage, MESSAGE_CHANNEL_TOKEN,
    MainMessage, MessageRunner, MessageSender, RendererMessage, SeatMessage,
    message_loop_with_channel, message_sender_on_poll,
};
use mio::{Events, Interest, Poll, Token};
use signal_hook::{consts::SIGINT, iterator::Signals};

pub const LIBSEAT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);
pub const SEAT_CHANNEL_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 2);
pub const RESTRICTED_OPEN_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 3);

fn main() -> anyhow::Result<()> {
    let Some(global_args) = GlobalArgs::parse(args()) else {
        return Ok(());
    };
    init_logger(global_args.log_file.as_deref())?;
    let global_args: &'static GlobalArgs = Box::leak(Box::new(global_args));
    run_app(global_args)
}

fn init_logger(log_file: Option<&str>) -> anyhow::Result<()> {
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file.unwrap_or("log.txt"))
        .context("Failed to open log file")?;
    let mut builder = Builder::from_default_env();
    builder.target(Target::Pipe(Box::new(log_file)));
    builder.format(|buf, record| {
        writeln!(
            buf,
            "[{:<5}] {:<9}: {}",
            record.level(),
            thread::current().name().unwrap_or("<unnamed>"),
            record.args()
        )
    });
    builder.init();
    Ok(())
}

/// Represents the data for the main thread
struct MainData {
    comms: Comms,
    config_child: Option<Child>,
    dbus_join_handle: JoinHandle<()>,
    display_join_handle: JoinHandle<()>,
    input_join_handle: JoinHandle<()>,
    renderer_join_handle: JoinHandle<()>,
    shutting_down: bool,
    shutdown_timeout: Option<Instant>,
}

impl MainData {
    fn new(
        comms: Comms,
        config_child: Option<Child>,
        dbus_join_handle: JoinHandle<()>,
        display_join_handle: JoinHandle<()>,
        input_join_handle: JoinHandle<()>,
        renderer_join_handle: JoinHandle<()>,
    ) -> Self {
        Self {
            comms,
            config_child,
            dbus_join_handle,
            display_join_handle,
            input_join_handle,
            renderer_join_handle,
            shutting_down: false,
            shutdown_timeout: None,
        }
    }

    fn handle_message(&mut self, message: MainMessage) -> anyhow::Result<()> {
        match message {
            MainMessage::Shutdown => {
                if !self.shutting_down {
                    self.shutting_down = true;
                    self.comms.dbus(DbusMessage::Shutdown);
                    self.comms.display(DisplayMessage::Shutdown);
                    self.comms.input(InputMessage::Shutdown);
                    self.comms.renderer(RendererMessage::Shutdown);
                    if let Some(child) = &mut self.config_child {
                        if let Err(err) = child.kill() {
                            warn!("Failed to stop config process: {err}");
                        }
                    }
                    self.shutdown_timeout = Some(Instant::now() + Duration::from_millis(1000));
                }
            }
            MainMessage::SeatEnabled | MainMessage::SeatDisabled => {}
        }
        Ok(())
    }

    fn ready_for_shutdown(&mut self) -> bool {
        if !self.shutting_down {
            return false;
        }

        for handle in [
            &self.dbus_join_handle,
            &self.display_join_handle,
            &self.input_join_handle,
            &self.renderer_join_handle,
        ] {
            if !handle.is_finished() {
                return false;
            }
        }

        if let Some(child) = self.config_child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => return false,
                Err(_) => {}
            }
        }

        true
    }
}

fn run_app(args: &'static GlobalArgs) -> anyhow::Result<()> {
    let (mut main_event_loop, main_channel, to_main) = message_loop_with_channel::<MainMessage>()?;
    let (seat_channel, to_seat) =
        message_sender_on_poll::<SeatMessage>(&main_event_loop, SEAT_CHANNEL_TOKEN)?;
    let (dbus_event_loop, dbus_channel, to_dbus) =
        message_loop_with_channel::<DbusMessage>()?;
    let (display_event_loop, display_channel, to_display) =
        message_loop_with_channel::<DisplayMessage>()?;
    let (input_event_loop, input_channel, to_input) =
        message_loop_with_channel::<InputMessage>()?;
    let (renderer_event_loop, renderer_channel, to_renderer) =
        message_loop_with_channel::<RendererMessage>()?;
    let comms = Comms::new(
        to_main.clone(),
        to_dbus,
        to_display,
        to_input,
        to_renderer,
        to_seat,
    );

    let (restricted_open_tx, restricted_open_rx) = mpsc::channel::<OpenRequest>();
    let (restricted_notify_rx, restricted_notify_tx) =
        message_sender_on_poll::<()>(&main_event_loop, RESTRICTED_OPEN_TOKEN)?;
    set_device_opener(Arc::new(RestrictedDeviceOpener::new(
        restricted_open_tx,
        restricted_notify_tx,
    )));

    let dbus_service =
        DbusService::register(comms.clone()).context("Failed to register D-Bus service")?;

    handle_signals(to_main.clone()).context("Failed to spawn signal handler thread")?;

    let config_child = if args.no_config {
        None
    } else {
        Some(spawn_config(args)?)
    };

    let display_join_handle = run_thread::<DisplayState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("display"),
        display_event_loop,
        display_channel,
        args,
    )
    .context("Unable to run display thread")?;

    let input_join_handle = run_thread::<InputState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("input"),
        input_event_loop,
        input_channel,
        args,
    )
    .context("Unable to run input thread")?;

    let renderer_join_handle = run_thread::<RendererState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("renderer"),
        renderer_event_loop,
        renderer_channel,
        args,
    )
    .context("Unable to run renderer thread")?;

    let dbus_join_handle = run_dbus_thread(
        to_main.clone(),
        dbus_event_loop,
        dbus_channel,
        dbus_service,
    )
    .context("Unable to run D-Bus thread")?;

    comms.dbus(DbusMessage::EmitReady);

    let mut seat_state = SeatState::new(comms.clone())?;
    main_event_loop
        .registry()
        .register(&mut seat_state, LIBSEAT_TOKEN, Interest::READABLE)
        .context("Unable to listen on seat state")?;

    let mut data = MainData::new(
        comms.clone(),
        config_child,
        dbus_join_handle,
        display_join_handle,
        input_join_handle,
        renderer_join_handle,
    );

    let mut events = Events::with_capacity(1024);
    loop {
        process_restricted_opens(&mut seat_state, &restricted_open_rx);

        let event_loop_timeout = if let Some(timeout) = data.shutdown_timeout {
            let now = Instant::now();
            if now >= timeout {
                info!("Shutdown timeout reached. Shutting down now");
                break;
            }

            Some(timeout - now)
        } else {
            None
        };

        if let Err(err) = main_event_loop.poll(&mut events, event_loop_timeout) {
            warn!("Unable to poll event loop: {err}");
        }

        for event in &events {
            if event.token() == RESTRICTED_OPEN_TOKEN {
                while restricted_notify_rx.try_recv().is_ok() {}
                process_restricted_opens(&mut seat_state, &restricted_open_rx);
            } else if event.token() == MESSAGE_CHANNEL_TOKEN {
                while let Ok(msg) = main_channel.try_recv() {
                    match msg {
                        MainMessage::SeatEnabled => {
                            seat_state.set_enabled(true);
                            if let Err(err) = notify_seat_enabled(&comms, &seat_state) {
                                error!("Unable to notify seat enabled: {err}");
                            }
                        }
                        MainMessage::SeatDisabled => {
                            seat_state.set_enabled(false);
                        }
                        other => {
                            if let Err(err) = data.handle_message(other) {
                                error!("Unable to handle message: {err}");
                            }
                        }
                    }
                }
            } else if event.token() == SEAT_CHANNEL_TOKEN {
                while let Ok(msg) = seat_channel.try_recv() {
                    if let Err(err) = handle_seat_message(&comms, &mut seat_state, msg) {
                        error!("Unable to handle seat message: {err}");
                    }
                }
            } else if event.token() == LIBSEAT_TOKEN {
                if let Err(err) = seat_state.dispatch() {
                    error!("Unable to dispatch seat events: {err}");
                }
            }
        }

        if data.ready_for_shutdown() {
            break;
        }
    }

    Ok(())
}

fn notify_seat_enabled(comms: &Comms, seat_state: &SeatState) -> anyhow::Result<()> {
    let seat_name = seat_state.seat_name().context("Failed to get seat name")?;
    comms.display(DisplayMessage::ActivateSeat(seat_name.clone()));
    comms.renderer(RendererMessage::SeatSessionCreated {
        seat_name: seat_name.clone(),
    });
    comms.input(InputMessage::SeatEnabled { seat_name });
    Ok(())
}

fn process_restricted_opens(
    seat_state: &mut SeatState,
    restricted_open_rx: &mpsc::Receiver<OpenRequest>,
) {
    while let Ok(request) = restricted_open_rx.try_recv() {
        let result = seat_state
            .open_device(std::path::Path::new(&request.path))
            .with_context(|| format!("Failed to open {}", request.path));
        if request.response.send(result).is_err() {
            warn!(
                "Input thread dropped response channel for {}",
                request.path
            );
        }
    }
}

fn handle_seat_message(
    comms: &Comms,
    seat_state: &mut SeatState,
    message: SeatMessage,
) -> anyhow::Result<()> {
    match message {
        SeatMessage::Shutdown => {}
        SeatMessage::SeatEnabled => {
            seat_state.set_enabled(true);
            notify_seat_enabled(comms, seat_state)?;
        }
        SeatMessage::SeatDisabled => {
            seat_state.set_enabled(false);
        }
        SeatMessage::OpenDevice { path } => {
            let fd = seat_state.open_device(&path)?;
            comms.renderer(RendererMessage::FileOpenedInSession { path, fd });
        }
    }

    Ok(())
}

/// Spawns a new thread and runs the message runner in it, returning a handle to the newly created
/// thread. The spawned thread is wrapped in a panic handler to gracefully handle any panics that
/// might occur.
fn run_thread<R, M>(
    comms: Comms,
    to_main: MessageSender<MainMessage>,
    name: String,
    event_loop: Poll,
    channel: mpsc::Receiver<M>,
    args: &'static GlobalArgs,
) -> anyhow::Result<JoinHandle<()>>
where
    R: MessageRunner<Message = M>,
    M: Send + 'static,
{
    let join_handle = thread::Builder::new()
        .name(name)
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut runner = R::new(comms, event_loop, channel, args)?;
                runner.run().context("Message runner exited with an error")
            }));
            match result {
                Ok(Ok(())) => {
                    info!("Thread exited normally");
                }
                Ok(Err(ref err)) => {
                    error!("Thread exited with an error: {err}");
                }
                Err(ref err) => {
                    if let Some(err) = err.downcast_ref::<&str>() {
                        error!("Thread panicked: {err}");
                    } else if let Some(err) = err.downcast_ref::<String>() {
                        error!("Thread panicked: {err}");
                    } else {
                        error!("Thread panicked: {:?}", err);
                    }
                }
            }
            info!("Sending shutdown signal to main, because thread is about to exit");

            if let Err(err) = to_main.send(MainMessage::Shutdown) {
                warn!("Unable to send shutdown signal to main: {err}");
            }
        })
        .context("Unable to spawn thread")?;

    Ok(join_handle)
}

fn spawn_config(args: &GlobalArgs) -> anyhow::Result<Child> {
    let command = args
        .config_command
        .as_deref()
        .unwrap_or("lumalla-config");
    let mut cmd = Command::new(command);
    if let Some(config) = &args.config {
        cmd.arg("--config").arg(config);
    }
    if let Some(log_file) = &args.log_file {
        cmd.arg("--log-file").arg(log_file);
    }
    cmd.spawn()
        .with_context(|| format!("Failed to spawn config command `{command}`"))
}

/// Handles signals sent to the process, such as SIGINT (Ctrl+C).
/// When the signal is received, the main thread is notified to initiate a graceful shutdown.
fn handle_signals(to_main: MessageSender<MainMessage>) -> anyhow::Result<()> {
    thread::Builder::new()
        .name("signals".to_string())
        .spawn(move || {
            let mut signals = match Signals::new([SIGINT]) {
                Ok(signals) => signals,
                Err(e) => {
                    error!("Failed to register signal handler: {e}");
                    return;
                }
            };

            for signal in signals.forever() {
                match signal {
                    SIGINT => {
                        info!("Received SIGINT signal (Ctrl+C), initiating graceful shutdown");
                        if let Err(e) = to_main.send(MainMessage::Shutdown) {
                            error!("Failed to send shutdown message: {e}");
                        }
                        break;
                    }
                    _ => {
                        warn!("Received unexpected signal: {signal}");
                    }
                }
            }
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRunner;

    impl MessageRunner for TestRunner {
        type Message = ();

        fn new(
            _comms: Comms,
            _event_loop: Poll,
            _channel: mpsc::Receiver<Self::Message>,
            _args: &'static GlobalArgs,
        ) -> anyhow::Result<Self> {
            Ok(Self)
        }

        fn run(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn test_comms() -> (Comms, mpsc::Receiver<MainMessage>, MessageSender<MainMessage>) {
        let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, _, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();
        let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
        let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
        let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
        let (_, _, to_seat) = message_loop_with_channel::<SeatMessage>().unwrap();
        (
            Comms::new(
                to_main.clone(),
                to_dbus,
                to_display,
                to_input,
                to_renderer,
                to_seat,
            ),
            main_channel,
            to_main,
        )
    }

    #[test]
    fn main_is_shutdown_on_thread_exit() {
        let (comms, main_channel, to_main) = test_comms();
        let args: &'static GlobalArgs = Box::leak(Box::new(GlobalArgs::default()));
        let (runner_event_loop, runner_receiver, _) = message_loop_with_channel::<()>().unwrap();

        let join_handle = run_thread::<TestRunner, _>(
            comms,
            to_main,
            String::from("test_thread"),
            runner_event_loop,
            runner_receiver,
            args,
        );

        join_handle.unwrap().join().unwrap();

        assert!(matches!(
            main_channel.recv().unwrap(),
            MainMessage::Shutdown
        ));
        assert!(main_channel.try_recv().is_err());
    }

    struct PanickingTestRunner;

    impl MessageRunner for PanickingTestRunner {
        type Message = ();

        fn new(
            _comms: Comms,
            _event_loop: Poll,
            _channel: mpsc::Receiver<Self::Message>,
            _args: &'static GlobalArgs,
        ) -> anyhow::Result<Self> {
            Ok(Self)
        }

        fn run(&mut self) -> anyhow::Result<()> {
            panic!();
        }
    }

    #[test]
    fn main_is_shutdown_on_thread_panic() {
        let (comms, main_channel, to_main) = test_comms();
        let args: &'static GlobalArgs = Box::leak(Box::new(GlobalArgs::default()));
        let (runner_event_loop, runner_receiver, _) = message_loop_with_channel::<()>().unwrap();

        let join_handle = run_thread::<PanickingTestRunner, _>(
            comms,
            to_main,
            String::from("test_thread"),
            runner_event_loop,
            runner_receiver,
            args,
        );

        join_handle.unwrap().join().unwrap();

        assert!(matches!(
            main_channel.recv().unwrap(),
            MainMessage::Shutdown
        ));
        assert!(main_channel.try_recv().is_err());
    }
}
