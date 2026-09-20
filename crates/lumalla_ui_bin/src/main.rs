//! `lumalla-ui` — small Wayland form helper for Lumalla config.

mod app;

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, bail};
use env_logger::Builder;
use log::error;
use lumalla_ui::FormSpec;

use crate::app::{Mode, run_form};

fn main() -> ExitCode {
    Builder::from_default_env().init();
    match run() {
        Ok(code) => code,
        Err(err) => {
            error!("{err:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let mut args = std::env::args().skip(1);
    let mut mode = Mode::Simple;
    let mut spec_path: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--simple" => mode = Mode::Simple,
            "--interactive" => mode = Mode::Interactive,
            "--spec" => {
                let path = args.next().context("--spec requires a path")?;
                spec_path = Some(PathBuf::from(path));
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(ExitCode::SUCCESS);
            }
            other => bail!("unknown argument: {other}"),
        }
    }

    let spec = load_spec(spec_path.as_deref())?;
    run_form(spec, mode)
}

fn load_spec(path: Option<&std::path::Path>) -> anyhow::Result<FormSpec> {
    let json = if let Some(path) = path {
        fs::read_to_string(path).with_context(|| format!("read form spec {}", path.display()))?
    } else {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .context("read form spec from stdin")?;
        buf
    };
    serde_json::from_str(&json).context("parse form spec JSON")
}

fn print_usage() {
    eprintln!(
        "Usage: lumalla-ui [--simple|--interactive] [--spec PATH]\n\
         \n\
         Reads a FormSpec JSON from --spec PATH, or stdin when --spec is omitted.\n\
         Simple mode prints a FormResult JSON on stdout and exits 0 on submit, 1 on cancel.\n\
         Interactive mode speaks NDJSON UiEvent/UiReply on stdout/stdin."
    );
}
