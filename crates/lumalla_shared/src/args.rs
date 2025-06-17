/// Global arguments provided at process start
pub struct GlobalArgs {
    /// Path to lua config file
    pub config: Option<String>,
}

impl GlobalArgs {
    /// Parse global arguments. `None` indicates that the program should exit.
    pub fn parse(mut args: impl Iterator<Item = String>) -> Option<Self> {
        if args.any(|arg| arg == "--help" || arg == "-h") {
            print_help();
            return None;
        }

        Some(Self {
            config: None,
        })
    }
}

fn print_help() {
    println!("Usage: lumalla [OPTIONS]");
    println!("Options:");
    println!("  -h, --help    Print this help message and exit");
}
