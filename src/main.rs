use std::{
    env::args,
    fs::OpenOptions,
    io::{Write, stderr},
    os::fd::AsRawFd,
    panic, thread,
};

use anyhow::Context;
use env_logger::{Builder, Target};
use lumalla_shared::{MainMessage, message_loop_with_channel};

use crate::{app::run_app, args::Args, os_signal::handle_signals};

mod app;
mod args;
mod os_signal;

fn main() -> anyhow::Result<()> {
    let Some(args) = Args::parse(args()) else {
        return Ok(());
    };
    init_logger(args.log_file.as_deref())?;
    let (main_event_loop, main_channel, to_main) = message_loop_with_channel::<MainMessage>()?;
    handle_signals(to_main.clone()).context("Failed to spawn signal handler thread")?;
    run_app(args, main_event_loop, main_channel, to_main)
}

fn init_logger(log_file: Option<&str>) -> anyhow::Result<()> {
    let path = log_file.unwrap_or("log.txt").to_owned();

    // Panics, Rust abort messages, and child stderr (e.g. lumalla-config) otherwise
    // only hit the TTY — which is useless once the compositor owns the seat.
    redirect_stderr_to_log(&path)?;

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
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

    install_panic_hook(path);
    Ok(())
}

fn redirect_stderr_to_log(path: &str) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("Failed to open log file for stderr redirect")?;
    let rc = unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error())
            .context("Failed to redirect stderr to log file");
    }
    // `file` may close its fd on drop; STDERR_FILENO still refers to the open file.
    Ok(())
}

fn install_panic_hook(log_path: String) {
    panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let thread = thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let payload = panic_payload(info);
        let location = info
            .location()
            .map(|location| location.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let message = format!(
            "[PANIC] {name}: {payload}\n  location: {location}\n{backtrace}\n"
        );

        // stderr is the log file after redirect; flush so a following abort keeps the dump.
        let _ = eprint!("{message}");
        let _ = stderr().flush();

        // Fresh open as a fallback if stderr redirect failed or was later replaced.
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = write!(file, "{message}");
            let _ = file.flush();
        }
    }));
}

fn panic_payload(info: &panic::PanicHookInfo<'_>) -> String {
    if let Some(payload) = info.payload_as_str() {
        return payload.to_owned();
    }
    if let Some(payload) = info.payload().downcast_ref::<String>() {
        return payload.clone();
    }
    if let Some(payload) = info.payload().downcast_ref::<&str>() {
        return (*payload).to_owned();
    }
    "Box<dyn Any>".to_owned()
}
