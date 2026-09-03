use std::{env::args, fs::OpenOptions, io::Write, thread};

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
