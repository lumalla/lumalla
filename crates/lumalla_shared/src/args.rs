/// Global arguments provided at process start
#[derive(Debug, Default, Clone)]
pub struct GlobalArgs {
    /// Path to log file
    pub log_file: Option<String>,
    /// Path to lua config file
    pub config: Option<String>,
    /// Path to wayland socket
    pub socket_path: Option<String>,
    /// Use an external config process instead of the embedded config thread
    pub external_config: bool,
    /// Command used to spawn the external config process
    pub config_command: Option<String>,
    /// Do not start any configuration process
    pub no_config: bool,
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
                "--external-config" => {
                    global_args.external_config = true;
                }
                "--no-config" => {
                    global_args.no_config = true;
                }
                "--config-command" => {
                    if let Some(config_command) = args.next() {
                        global_args.config_command = Some(config_command);
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

    /// Parse arguments for the external config client.
    pub fn parse_config_client(mut args: impl Iterator<Item = String>) -> Option<Self> {
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
                    print_config_client_help(&program_name);
                    return None;
                }
                "--config" | "-c" => {
                    if let Some(config) = args.next() {
                        global_args.config = Some(config);
                    }
                }
                unknown => {
                    eprintln!("Unknown argument: {}", unknown);
                    print_config_client_help(&program_name);
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
    println!("      --external-config  Spawn lumalla-config instead of embedded config");
    println!("      --no-config        Do not start configuration");
    println!("      --config-command <CMD>  External config command (default: lumalla-config)");
}

fn print_config_client_help(program_name: &str) {
    println!("Usage: {} [OPTIONS]", program_name);
    println!("Options:");
    println!("  -h, --help             Print this help message and exit");
    println!("  -l, --log-file <FILE>  Path to log file");
    println!("  -c, --config <FILE>    Path to lua config file");
}
