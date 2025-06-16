use env_logger::{Builder, Target};

fn main() {
    init_logger();
}

fn init_logger() {
    let mut builder = Builder::from_default_env();
    builder.target(Target::Stdout);
    builder.init();
}
