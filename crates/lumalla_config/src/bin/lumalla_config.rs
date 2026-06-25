use std::env::args;

use anyhow::Context;
use env_logger::Builder;
use log::error;
use lumalla_config::ExternalConfig;
use lumalla_shared::GlobalArgs;

fn main() -> anyhow::Result<()> {
    Builder::from_default_env().init();

    let Some(args) = GlobalArgs::parse_config_client(args()) else {
        return Ok(());
    };

    let mut config = ExternalConfig::new(&args).context("Failed to start external config")?;
    if let Err(err) = config.run() {
        error!("External config exited with an error: {err}");
        return Err(err);
    }

    Ok(())
}
