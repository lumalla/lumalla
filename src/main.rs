use std::{
    env::args,
    fs::OpenOptions,
    io::Write,
    process::{Child, Command},
    thread,
};

use anyhow::Context;
use env_logger::{Builder, Target};
use lumalla_shared::{GlobalArgs, MainMessage, message_loop_with_channel};

use crate::{app::run_app, os_signal::handle_signals};

mod app;
mod os_signal;

fn main() -> anyhow::Result<()> {
    let Some(global_args) = GlobalArgs::parse(args()) else {
        return Ok(());
    };
    init_logger(global_args.log_file.as_deref())?;
    let (main_event_loop, main_channel, to_main) = message_loop_with_channel::<MainMessage>()?;
    handle_signals(to_main.clone()).context("Failed to spawn signal handler thread")?;
    let args: &'static GlobalArgs = Box::leak(Box::new(global_args));
    let config_child = run_config(&args)?;
    run_app(args, main_event_loop, main_channel, to_main, config_child)
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

fn run_config(args: &GlobalArgs) -> anyhow::Result<Option<Child>> {
    if args.no_config {
        Ok(None)
    } else {
        Ok(Some(spawn_config(&args)?))
    }
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
