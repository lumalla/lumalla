use std::env::args;

use env_logger::{Builder, Target};
use lumalla_shared::GlobalArgs;

fn main() {
    let Some(_) = GlobalArgs::parse(args()) else {
        return;
    };

    init_logger();
}

fn init_logger() {
    let mut builder = Builder::from_default_env();
    builder.target(Target::Stdout);
    builder.init();
}
