/// Global arguments provided at process start
#[derive(Debug, Default)]
pub struct GlobalArgs {
    /// Path to log file
    pub log_file: Option<String>,
    /// Path to lua config file
    pub config: Option<String>,
    /// Path to wayland socket
    pub socket_path: Option<String>,
}

impl GlobalArgs {
    /// Parse global arguments. `None` indicates that the program should exit.
    pub fn parse(mut args: impl Iterator<Item = String>) -> Option<Self> {
        let Some(program_name) = args.next() else {
            eprintln!("No program name provided");
            return None;
        };

        let mut global_args = Self::default();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--log-file" | "-l" => {
                    if let Some(log_file) = args.next() {
                        global_args.log_file = Some(log_file);
                    }
                }
                "-h" | "--help" => {
                    print_help(&program_name);
                    return None;
                }
                "--config" | "-c" => {
                    if let Some(config) = args.next() {
                        global_args.config = Some(config);
                    }
                }
                "--socket-path" | "-s" => {
                    if let Some(socket_path) = args.next() {
                        global_args.socket_path = Some(socket_path);
                    }
                }
                unknown => {
                    eprintln!("Unknown argument: {}", unknown);
                    print_help(&program_name);
                    return None;
                }
            }
        }

        Some(global_args)
    }
}

fn print_help(program_name: &str) {
    println!("Usage: {} [OPTIONS]", program_name);
    println!("Options:");
    println!("  -h, --help             Print this help message and exit");
    println!("  -l, --log-file <FILE>  Path to log file");
    println!("  -c, --config <FILE>    Path to lua config file");
    println!("  -s, --socket-path <PATH>");
}
