use std::{
    env::args,
    fs::OpenOptions,
    io::Write,
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::Context;
use env_logger::{Builder, Target};
use log::{error, info, warn};
use lumalla_config::ConfigState;
use lumalla_display::DisplayState;
use lumalla_input::InputState;
use lumalla_renderer::RendererState;
use lumalla_seat::SeatState;
use lumalla_shared::{
    Comms, ConfigMessage, DisplayMessage, GlobalArgs, InputMessage, MESSAGE_CHANNEL_TOKEN,
    MainMessage, MessageRunner, MessageSender, RendererMessage, SeatMessage,
    message_loop_with_channel,
};
use mio::{Events, Poll};
use signal_hook::{consts::SIGINT, iterator::Signals};

fn main() -> anyhow::Result<()> {
    let Some(global_args) = GlobalArgs::parse(args()) else {
        return Ok(());
    };

    init_logger(global_args.log_file.as_deref())?;

    run_app(Arc::new(global_args)).inspect_err(|err| error!("An error occurred: {err}"))
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
    config_join_handle: JoinHandle<()>,
    input_join_handle: JoinHandle<()>,
    display_join_handle: JoinHandle<()>,
    renderer_join_handle: JoinHandle<()>,
    seat_join_handle: JoinHandle<()>,
    shutting_down: bool,
    shutdown_timeout: Option<Instant>,
}

impl MainData {
    /// Creates a new instance of `MainData`
    fn new(
        comms: Comms,
        config_join_handle: JoinHandle<()>,
        input_join_handle: JoinHandle<()>,
        display_join_handle: JoinHandle<()>,
        renderer_join_handle: JoinHandle<()>,
        seat_join_handle: JoinHandle<()>,
    ) -> Self {
        Self {
            comms,
            config_join_handle,
            input_join_handle,
            display_join_handle,
            renderer_join_handle,
            seat_join_handle,
            shutting_down: false,
            shutdown_timeout: None,
        }
    }

    fn handle_message(&mut self, message: MainMessage) -> anyhow::Result<()> {
        match message {
            MainMessage::Shutdown => {
                if !self.shutting_down {
                    self.shutting_down = true;
                    // Notify the other threads that the application is shutting down
                    self.comms.input(InputMessage::Shutdown);
                    self.comms.display(DisplayMessage::Shutdown);
                    self.comms.renderer(RendererMessage::Shutdown);
                    self.comms.config(ConfigMessage::Shutdown);
                    self.comms.seat(SeatMessage::Shutdown);
                    // Force shutdown after some time
                    self.shutdown_timeout = Some(Instant::now() + Duration::from_millis(1000));
                }
            }
        }
        Ok(())
    }
}

/// Starts the application by creating the needed channels and starting the necessary threads. The
/// main thread will wait for the other threads to finish before exiting.
fn run_app(args: Arc<GlobalArgs>) -> anyhow::Result<()> {
    // Create the channels for communication between the threads
    let (mut main_event_loop, main_channel, to_main) = message_loop_with_channel::<MainMessage>()?;
    let (display_event_loop, display_channel, to_display) =
        message_loop_with_channel::<DisplayMessage>()?;
    let (renderer_event_loop, renderer_channel, to_renderer) =
        message_loop_with_channel::<RendererMessage>()?;
    let (input_event_loop, input_channel, to_input) = message_loop_with_channel::<InputMessage>()?;
    let (config_event_loop, config_channel, to_config) =
        message_loop_with_channel::<ConfigMessage>()?;
    let (seat_event_loop, seat_channel, to_seat) = message_loop_with_channel::<SeatMessage>()?;
    let comms = Comms::new(
        to_main.clone(),
        to_display,
        to_renderer,
        to_input,
        to_config,
        to_seat,
    );

    handle_signals(to_main.clone()).context("Failed to spawn signal handler thread")?;

    // Spawn the config thread
    let config_join_handle = run_thread::<ConfigState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("config"),
        config_event_loop,
        config_channel,
        args.clone(),
    )
    .context("Unable to run config thread")?;
    // Spawn the input thread
    let input_join_handle = run_thread::<InputState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("input"),
        input_event_loop,
        input_channel,
        args.clone(),
    )
    .context("Unable to run input thread")?;
    // Spawn the renderer thread
    let renderer_join_handle = run_thread::<RendererState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("renderer"),
        renderer_event_loop,
        renderer_channel,
        args.clone(),
    )
    .context("Unable to run renderer thread")?;
    // Spawn the display thread
    let display_join_handle = run_thread::<DisplayState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("display"),
        display_event_loop,
        display_channel,
        args.clone(),
    )
    .context("Unable to run display thread")?;
    // Spawn the seat thread
    let seat_join_handle = run_thread::<SeatState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("seat"),
        seat_event_loop,
        seat_channel,
        args,
    )
    .context("Unable to run seat thread")?;

    let mut data = MainData::new(
        comms,
        config_join_handle,
        input_join_handle,
        display_join_handle,
        renderer_join_handle,
        seat_join_handle,
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
        }

        if data.shutting_down
            && data.config_join_handle.is_finished()
            && data.input_join_handle.is_finished()
            && data.display_join_handle.is_finished()
            && data.renderer_join_handle.is_finished()
            && data.seat_join_handle.is_finished()
        {
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
    args: Arc<GlobalArgs>,
) -> anyhow::Result<JoinHandle<()>>
where
    R: MessageRunner<Message = M>,
    M: Send + 'static,
{
    let join_handle = thread::Builder::new()
        .name(name)
        .spawn(move || {
            let result = std::panic::catch_unwind(move || {
                let mut runner = R::new(comms, event_loop, channel, args)?;
                runner.run().context("Message runner exited with an error")
            });
            match result {
                Ok(Ok(())) => {
                    info!("Thread exited normally");
                }
                Ok(Err(err)) => {
                    error!("Thread exited with an error: {err}");
                }
                Err(err) => {
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
            _args: Arc<GlobalArgs>,
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
        let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
        let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
        let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
        let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
        let (_, _, to_seat) = message_loop_with_channel::<SeatMessage>().unwrap();
        let comms = Comms::new(
            to_main.clone(),
            to_display,
            to_renderer,
            to_input,
            to_config,
            to_seat,
        );
        let args = Arc::new(GlobalArgs::default());
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
            _args: Arc<GlobalArgs>,
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
        let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
        let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
        let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
        let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
        let (_, _, to_seat) = message_loop_with_channel::<SeatMessage>().unwrap();
        let comms = Comms::new(
            to_main.clone(),
            to_display,
            to_renderer,
            to_input,
            to_config,
            to_seat,
        );
        let args = Arc::new(GlobalArgs::default());
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
