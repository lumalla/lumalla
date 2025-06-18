use std::{env::args, fs::OpenOptions, io::Write, thread};

use anyhow::Context;
use env_logger::{Builder, Target};
use lumalla_shared::GlobalArgs;

fn main() -> anyhow::Result<()> {
    let Some(global_args) = GlobalArgs::parse(args()) else {
        return Ok(());
    };

    init_logger(global_args.log_file.as_deref())?;

    Ok(())
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
