use std::{
    env::args,
    fs::OpenOptions,
    io::Write,
    process::{Child, Command},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::Context;
use env_logger::{Builder, Target};
use log::{error, info, warn};
use lumalla_config::ConfigState;
use lumalla_dbus::{DbusService, run_thread as run_dbus_thread};
use lumalla_display::DisplayState;
use lumalla_input::InputState;
use lumalla_renderer::RendererState;
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, ConfigMessage, DbusMessage, GlobalArgs, InputMessage, MESSAGE_CHANNEL_TOKEN,
    MainMessage, MessageRunner, MessageSender, RendererMessage, SeatMessage,
    message_loop_with_channel,
};
use mio::{Events, Interest, Poll, Token, unix::SourceFd};
use signal_hook::{consts::SIGINT, iterator::Signals};

pub const LIBSEAT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);

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
    config_join_handle: Option<JoinHandle<()>>,
    external_config_child: Option<Child>,
    dbus_join_handle: JoinHandle<()>,
    shutting_down: bool,
    shutdown_timeout: Option<Instant>,
    seat_enabled: bool,
}

impl MainData {
    fn new(
        comms: Comms,
        config_join_handle: Option<JoinHandle<()>>,
        external_config_child: Option<Child>,
        dbus_join_handle: JoinHandle<()>,
    ) -> Self {
        Self {
            comms,
            config_join_handle,
            external_config_child,
            dbus_join_handle,
            shutting_down: false,
            shutdown_timeout: None,
            seat_enabled: false,
        }
    }

    fn handle_message(&mut self, message: MainMessage) -> anyhow::Result<()> {
        match message {
            MainMessage::Shutdown => {
                if !self.shutting_down {
                    self.shutting_down = true;
                    // Notify the other threads that the application is shutting down
                    self.comms.config(ConfigMessage::Shutdown);
                    self.comms.dbus(DbusMessage::Shutdown);
                    // Force shutdown after some time
                    self.shutdown_timeout = Some(Instant::now() + Duration::from_millis(1000));
                }
            }
            MainMessage::SeatEnabled => {
                info!("Seat enabled");
                self.seat_enabled = true;
            }
            MainMessage::SeatDisabled => {
                info!("Seat disabled");
                self.seat_enabled = false;
            }
        }
        Ok(())
    }

    fn ready_for_shutdown(&mut self) -> bool {
        if !self.shutting_down || !self.dbus_join_handle.is_finished() {
            return false;
        }

        if let Some(config_join_handle) = &self.config_join_handle {
            if !config_join_handle.is_finished() {
                return false;
            }
        }

        if let Some(child) = self.external_config_child.as_mut() {
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
    let (config_event_loop, config_channel, to_config) =
        message_loop_with_channel::<ConfigMessage>()?;
    let (dbus_event_loop, dbus_channel, to_dbus) =
        message_loop_with_channel::<DbusMessage>()?;
    let comms = Comms::new(to_main.clone(), to_config, to_dbus);

    let dbus_service =
        DbusService::register(comms.clone()).context("Failed to register D-Bus service")?;

    handle_signals(to_main.clone()).context("Failed to spawn signal handler thread")?;

    let external_config_child = if args.external_config && !args.no_config {
        Some(spawn_external_config(args)?)
    } else {
        None
    };

    let config_join_handle = if args.no_config {
        Some(run_config_shutdown_listener(config_channel))
    } else if args.external_config {
        Some(run_config_shutdown_listener(config_channel))
    } else {
        Some(
            run_thread::<ConfigState, _>(
                comms.clone(),
                to_main.clone(),
                String::from("config"),
                config_event_loop,
                config_channel,
                args,
            )
            .context("Unable to run config thread")?,
        )
    };

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
        comms,
        config_join_handle,
        external_config_child,
        dbus_join_handle,
    );

    let mut events = Events::with_capacity(1024);
    loop {
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
            if event.token() == MESSAGE_CHANNEL_TOKEN {
                while let Ok(msg) = main_channel.try_recv() {
                    if let Err(err) = data.handle_message(msg) {
                        error!("Unable to handle message: {err}");
                    }
                }
            }
            if event.token() == LIBSEAT_TOKEN {
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
    let thread_name = name.clone();
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

            // The thread should only exit if the main thread has already sent a shutdown signal,
            // but in case something is wrong, we send a shutdown signal to the main thread anyway.
            if let Err(err) = to_main.send(MainMessage::Shutdown) {
                warn!("Unable to send shutdown signal to main: {err}");
            }
        })
        .context("Unable to spawn thread")?;

    Ok(join_handle)
}

fn spawn_external_config(args: &GlobalArgs) -> anyhow::Result<Child> {
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
        .with_context(|| format!("Failed to spawn external config command `{command}`"))
}

fn run_config_shutdown_listener(channel: mpsc::Receiver<ConfigMessage>) -> JoinHandle<()> {
    thread::Builder::new()
        .name(String::from("config-shutdown"))
        .spawn(move || {
            while let Ok(message) = channel.recv() {
                if matches!(message, ConfigMessage::Shutdown) {
                    break;
                }
            }
        })
        .expect("Failed to spawn config shutdown listener")
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

    #[test]
    fn main_is_shutdown_on_thread_exit() {
        let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
        let (_, _, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();
        let comms = Comms::new(to_main.clone(), to_config, to_dbus);
        let args: &'static GlobalArgs = Box::leak(Box::new(GlobalArgs::default()));
        let (test_event_loop, test_receiver, _) = message_loop_with_channel::<()>().unwrap();

        let join_handle = run_thread::<TestRunner, _>(
            comms,
            to_main,
            String::from("test_thread"),
            test_event_loop,
            test_receiver,
            args,
        );

        // Wait for the thread to finish
        join_handle.unwrap().join().unwrap();

        // Check if the main channel has received the shutdown signal
        assert!(matches!(
            main_channel.recv().unwrap(),
            MainMessage::Shutdown
        ));
        // No other messages should be received
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
        let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
        let (_, _, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();
        let comms = Comms::new(to_main.clone(), to_config, to_dbus);
        let args: &'static GlobalArgs = Box::leak(Box::new(GlobalArgs::default()));
        let (test_event_loop, test_receiver, _) = message_loop_with_channel::<()>().unwrap();

        let join_handle = run_thread::<PanickingTestRunner, _>(
            comms,
            to_main,
            String::from("test_thread"),
            test_event_loop,
            test_receiver,
            args,
        );

        // Wait for the thread to finish
        join_handle.unwrap().join().unwrap();

        // Check if the main channel has received the shutdown signal
        assert!(matches!(
            main_channel.recv().unwrap(),
            MainMessage::Shutdown
        ));
        // No other messages should be received
        assert!(main_channel.try_recv().is_err());
    }
}
