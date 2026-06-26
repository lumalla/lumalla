use std::{
    env::args,
    fs::OpenOptions,
    io::Write,
    process::{Child, Command},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::Context;
use env_logger::{Builder, Target};
use log::{error, info, warn};
use lumalla_dbus::{DbusService, run_thread as run_dbus_thread};
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, DbusMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MainMessage, MessageSender,
    message_loop_with_channel,
};
use mio::{Events, Interest, Token};
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
    config_child: Option<Child>,
    dbus_join_handle: JoinHandle<()>,
    shutting_down: bool,
    shutdown_timeout: Option<Instant>,
}

impl MainData {
    fn new(comms: Comms, config_child: Option<Child>, dbus_join_handle: JoinHandle<()>) -> Self {
        Self {
            comms,
            config_child,
            dbus_join_handle,
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

        for handle in [&self.dbus_join_handle] {
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
    let (dbus_event_loop, dbus_channel, to_dbus) = message_loop_with_channel::<DbusMessage>()?;
    let comms = Comms::new(to_main.clone(), to_dbus);

    let dbus_service =
        DbusService::register(comms.clone()).context("Failed to register D-Bus service")?;

    handle_signals(to_main.clone()).context("Failed to spawn signal handler thread")?;

    let config_child = if args.no_config {
        None
    } else {
        Some(spawn_config(args)?)
    };

    let dbus_join_handle =
        run_dbus_thread(to_main.clone(), dbus_event_loop, dbus_channel, dbus_service)
            .context("Unable to run D-Bus thread")?;

    comms.dbus(DbusMessage::EmitReady);

    let mut seat_state = SeatState::new(comms.clone())?;
    main_event_loop
        .registry()
        .register(&mut seat_state, LIBSEAT_TOKEN, Interest::READABLE)
        .context("Unable to listen on seat state")?;

    let mut data = MainData::new(comms.clone(), config_child, dbus_join_handle);

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
                    match msg {
                        MainMessage::SeatEnabled => {
                            seat_state.set_enabled(true);
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

fn spawn_config(args: &GlobalArgs) -> anyhow::Result<Child> {
    let command = args.config_command.as_deref().unwrap_or("lumalla-config");
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
