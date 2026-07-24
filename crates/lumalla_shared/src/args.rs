/// Global arguments provided at process start
#[derive(Debug, Default, Clone)]
pub struct GlobalArgs {
    /// Path to log file
    pub log_file: Option<String>,
    /// Path to lua config file
    pub config: Option<String>,
    /// Path to wayland socket
    pub socket_path: Option<String>,
    /// Command used to spawn the external config process
    pub config_command: Option<String>,
    /// Do not start any configuration process
    pub no_config: bool,
    /// Program and arguments to spawn after `--`
    pub startup_command: Vec<String>,
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
                "--no-config" => {
                    global_args.no_config = true;
                }
                "--config-command" => {
                    if let Some(config_command) = args.next() {
                        global_args.config_command = Some(config_command);
                    }
                }
                "--" => {
                    global_args.startup_command.extend(args);
                    if global_args.startup_command.is_empty() {
                        eprintln!("Expected a command after --");
                        print_help(&program_name);
                        return None;
                    }
                    break;
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
    println!("Usage: {} [OPTIONS] [-- COMMAND [ARGS...]]", program_name);
    println!("Options:");
    println!("  -h, --help             Print this help message and exit");
    println!("  -l, --log-file <FILE>  Path to log file");
    println!("  -c, --config <FILE>    Path to lua config file");
    println!("  -s, --socket-path <PATH>");
    println!("      --no-config        Do not start configuration");
    println!("      --config-command <CMD>  Config command (default: lumalla-config)");
    println!("      -- COMMAND [ARGS...]     Spawn a program after Lumalla starts");
}

fn print_config_client_help(program_name: &str) {
    println!("Usage: {} [OPTIONS]", program_name);
    println!("Options:");
    println!("  -h, --help             Print this help message and exit");
    println!("  -l, --log-file <FILE>  Path to log file");
    println!("  -c, --config <FILE>    Path to lua config file");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_startup_command_after_separator() {
        let args = ["lumalla", "--no-config", "--", "program", "--flag", "value"]
            .into_iter()
            .map(String::from);

        let parsed = GlobalArgs::parse(args).unwrap();

        assert!(parsed.no_config);
        assert_eq!(
            parsed.startup_command,
            ["program", "--flag", "value"].map(String::from)
        );
    }

    #[test]
    fn rejects_empty_startup_command() {
        let args = ["lumalla", "--"].into_iter().map(String::from);
        assert!(GlobalArgs::parse(args).is_none());
    }
}
