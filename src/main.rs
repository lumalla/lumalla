use std::{
    env::args,
    fs::OpenOptions,
    io::Write,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::Context;
use calloop::{
    EventLoop, LoopHandle,
    channel::{self, Channel, Sender, channel},
    timer::{TimeoutAction, Timer},
};
use env_logger::{Builder, Target};
use log::{error, info, warn};
use lumalla_config::ConfigState;
use lumalla_display::DisplayState;
use lumalla_input::InputState;
use lumalla_rederer::RendererState;
use lumalla_shared::{
    Comms, ConfigMessage, DisplayMessage, GlobalArgs, InputMessage, MainMessage, MessageRunner,
    RendererMessage,
};

fn main() -> anyhow::Result<()> {
    let Some(global_args) = GlobalArgs::parse(args()) else {
        return Ok(());
    };

    init_logger(global_args.log_file.as_deref())?;

    run_app(Arc::new(global_args)).inspect_err(|err| error!("An error occurred: {err}"))
}

fn init_logger(log_file: Option<&str>) -> anyhow::Result<()> {
    println!("Logging initialized {:?}", log_file);
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
            "[{:<5}] {}: {}",
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
    loop_handle: LoopHandle<'static, MainData>,
    comms: Comms,
    config_join_handle: JoinHandle<()>,
    input_join_handle: JoinHandle<()>,
    display_join_handle: JoinHandle<()>,
    renderer_join_handle: JoinHandle<()>,
    shutting_down: bool,
    force_shutting_down: bool,
}

impl MainData {
    /// Creates a new instance of `MainData`
    fn new(
        loop_handle: LoopHandle<'static, MainData>,
        comms: Comms,
        config_join_handle: JoinHandle<()>,
        input_join_handle: JoinHandle<()>,
        display_join_handle: JoinHandle<()>,
        renderer_join_handle: JoinHandle<()>,
    ) -> Self {
        Self {
            loop_handle,
            comms,
            config_join_handle,
            input_join_handle,
            display_join_handle,
            renderer_join_handle,
            shutting_down: false,
            force_shutting_down: false,
        }
    }
}

/// Starts the application by creating the needed channels and starting the necessary threads. The
/// main thread will wait for the other threads to finish before exiting.
fn run_app(args: Arc<GlobalArgs>) -> anyhow::Result<()> {
    // Create the channels for communication between the threads
    let (to_main, main_channel) = channel();
    let (to_display, display_channel) = channel();
    let (to_renderer, renderer_channel) = channel();
    let (to_input, input_channel) = channel();
    let (to_config, config_channel) = channel();
    let comms = Comms::new(
        to_main.clone(),
        to_display,
        to_renderer,
        to_input,
        to_config,
    );

    let mut event_loop = EventLoop::<MainData>::try_new().context("Unable to create event loop")?;
    let signal = event_loop.get_signal();
    let loop_handle = event_loop.handle();

    if let Err(e) = loop_handle.insert_source(main_channel, |event, _, data| match event {
        calloop::channel::Event::Msg(msg) => match msg {
            MainMessage::Shutdown => {
                if !data.shutting_down {
                    data.shutting_down = true;
                    // Notify the other threads that the application is shutting down
                    data.comms.input(InputMessage::Shutdown);
                    data.comms.display(DisplayMessage::Shutdown);
                    data.comms.renderer(RendererMessage::Shutdown);
                    data.comms.config(ConfigMessage::Shutdown);
                    // Force shutdown after some time
                    if let Err(e) = data.loop_handle.insert_source(
                        Timer::from_duration(Duration::from_millis(1000)),
                        |_, _, data| {
                            info!("Force shutdown timeout reached. Shutting down now");
                            data.force_shutting_down = true;
                            TimeoutAction::Drop
                        },
                    ) {
                        warn!("Unable to insert timer to force shutdown ({e}). Shutting down now");
                        data.force_shutting_down = true;
                    }
                }
            }
        },
        calloop::channel::Event::Closed => (),
    }) {
        anyhow::bail!("Unable to insert main channel into event loop: {}", e);
    }

    // Spawn the config thread
    let config_join_handle = run_thread::<ConfigState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("config"),
        config_channel,
        args.clone(),
    )
    .context("Unable to run config thread")?;
    // Spawn the input thread
    let input_join_handle = run_thread::<InputState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("input"),
        input_channel,
        args.clone(),
    )
    .context("Unable to run input thread")?;
    // Spawn the renderer thread
    let renderer_join_handle = run_thread::<RendererState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("renderer"),
        renderer_channel,
        args.clone(),
    )
    .context("Unable to run renderer thread")?;
    // Spawn the display thread
    let display_join_handle = run_thread::<DisplayState, _>(
        comms.clone(),
        to_main.clone(),
        String::from("display"),
        display_channel,
        args,
    )
    .context("Unable to run display thread")?;

    let mut data = MainData::new(
        loop_handle,
        comms,
        config_join_handle,
        input_join_handle,
        display_join_handle,
        renderer_join_handle,
    );

    // Run the main loop
    event_loop
        .run(None, &mut data, |data| {
            if data.shutting_down
                && data.config_join_handle.is_finished()
                && data.input_join_handle.is_finished()
                && data.display_join_handle.is_finished()
                && data.renderer_join_handle.is_finished()
                || data.force_shutting_down
            {
                signal.stop();
            }
        })
        .context("Unable to run main loop")?;

    Ok(())
}

/// Spawns a new thread and runs the given function in it, returning a handle to the newly created
/// thread. The spawned thread is wrapped in a panic handler to gracefully handle any panics that
/// might occur.
fn run_thread<R, M>(
    comms: Comms,
    to_main: Sender<MainMessage>,
    name: String,
    channel: Channel<M>,
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
                if let Err(err) = run_message_loop::<R, M>(comms, channel, args) {
                    error!("Thread exited with an error: {err}");
                    false
                } else {
                    true
                }
            });
            match result {
                Ok(true) => {
                    info!("Thread exited normally");
                }
                Ok(false) => {
                    error!("Thread exited with an error");
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

/// Run the message loop with the runner type `R`. The message loop will exit when the channel to
/// the runner is closed.
fn run_message_loop<R, M>(
    comms: Comms,
    channel: Channel<M>,
    args: Arc<GlobalArgs>,
) -> anyhow::Result<()>
where
    R: MessageRunner<Message = M>,
    M: Send + 'static,
{
    let mut event_loop = EventLoop::<R>::try_new().context("Unable to create event loop")?;
    let signal = event_loop.get_signal();
    let loop_handle = event_loop.handle();

    if let Err(e) = loop_handle.insert_source(channel, move |event, _, data| match event {
        channel::Event::Msg(msg) => {
            if let Err(err) = data.handle_message(msg) {
                error!("Unable to handle message: {err}");
            }
        }
        channel::Event::Closed => {
            warn!("Channel closed, shutting down");
            signal.stop();
        }
    }) {
        anyhow::bail!("Unable to insert channel into event loop: {}", e);
    }

    let mut runner = R::new(comms, loop_handle, args).context("Unable to create runner")?;

    let signal = event_loop.get_signal();
    // Run the main loop f
    event_loop
        .run(None, &mut runner, |data| {
            data.on_dispatch_wait(&signal);
        })
        .context("Unable to run loop")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use calloop::LoopSignal;

    struct TestRunner;

    impl MessageRunner for TestRunner {
        type Message = ();

        fn new(
            _comms: Comms,
            _loop_handle: LoopHandle<'static, Self>,
            _args: Arc<GlobalArgs>,
        ) -> anyhow::Result<Self> {
            Ok(Self)
        }

        fn handle_message(&mut self, _message: Self::Message) -> anyhow::Result<()> {
            Ok(())
        }

        fn on_dispatch_wait(&mut self, signal: &LoopSignal) {
            signal.stop();
        }
    }

    #[test]
    fn run_thread_sends_shutdown_signal() {
        let (to_main, main_channel) = channel();
        let (to_display, _) = channel();
        let (to_renderer, _) = channel();
        let (to_input, _) = channel();
        let (to_config, _) = channel();
        let comms = Comms::new(
            to_main.clone(),
            to_display,
            to_renderer,
            to_input,
            to_config,
        );
        let args = Arc::new(GlobalArgs::default());
        let (_, test_channel) = channel::<()>();

        let join_handle = run_thread::<TestRunner, _>(
            comms,
            to_main,
            String::from("test_thread"),
            test_channel,
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

    #[test]
    fn run_thread_sends_shutdown_signal_on_panic() {
        let (to_main, main_channel) = channel();
        let (to_display, _) = channel();
        let (to_renderer, _) = channel();
        let (to_input, _) = channel();
        let (to_config, _) = channel();
        let comms = Comms::new(
            to_main.clone(),
            to_display,
            to_renderer,
            to_input,
            to_config,
        );
        let args = Arc::new(GlobalArgs::default());
        let (_, test_channel) = channel::<()>();

        let join_handle = run_thread::<TestRunner, _>(
            comms,
            to_main,
            String::from("test_thread"),
            test_channel,
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

    #[test]
    fn run_message_loop_forwards_messages_to_runner() {
        // TODO: fill body
    }

    #[test]
    fn run_message_loop_calls_on_dispatch_wait() {
        // TODO: fill body
    }

    #[test]
    fn run_message_loop_stops_on_channel_close() {
        // TODO: fill body
    }
}
