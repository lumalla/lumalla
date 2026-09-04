use std::path::PathBuf;

/// Arguments provided at process start
#[derive(Debug, Default, Clone)]
pub struct Args {
    /// Path to log file
    pub log_file: Option<String>,
    /// Path to wayland socket
    pub socket_path: Option<PathBuf>,
    /// Skip libseat and run without DRM/libinput device opens.
    pub headless: bool,
    /// Command used to spawn the external config process, specified after `--`
    pub config_command: Option<String>,
    /// Arguments to pass to the external config process
    pub config_args: Vec<String>,
}

impl Args {
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
                "--socket-path" | "-s" => {
                    if let Some(socket_path) = args.next() {
                        global_args.socket_path = Some(PathBuf::from(socket_path));
                    }
                }
                "--headless" => {
                    global_args.headless = true;
                }
                "--config-command" => {
                    if let Some(config_command) = args.next() {
                        global_args.config_command = Some(config_command);
                    }
                }
                "--" => {
                    let Some(config_command) = args.next() else {
                        eprintln!("Expected a command after --");
                        print_help(&program_name);
                        return None;
                    };
                    global_args.config_command = Some(config_command);
                    global_args.config_args.extend(&mut args);
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
    println!("Usage: {} [OPTIONS] [-- COMMAND [ARGS...]]", program_name);
    println!("Options:");
    println!("  -h, --help             Print this help message and exit");
    println!("  -l, --log-file <FILE>  Path to log file");
    println!("  -c, --config <FILE>    Path to lua config file");
    println!("  -s, --socket-path <PATH>");
    println!("      --headless         Run without libseat (no DRM/libinput device opens)");
    println!("      --no-config        Do not start configuration");
    println!("      --config-command <CMD>  Config command (default: lumalla-config)");
    println!("      -- COMMAND [ARGS...]     Spawn a program after Lumalla starts");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_command_after_separator() {
        let args = ["lumalla", "--", "lumalla-config", "--flag", "value"]
            .into_iter()
            .map(String::from);

        let parsed = Args::parse(args).unwrap();

        assert_eq!(parsed.config_command.as_deref(), Some("lumalla-config"));
        assert_eq!(parsed.config_args, ["--flag", "value"].map(String::from));
    }

    #[test]
    fn parses_headless_flag() {
        let args = ["lumalla", "--headless", "--", "lumalla-config"]
            .into_iter()
            .map(String::from);
        let parsed = Args::parse(args).unwrap();
        assert!(parsed.headless);
    }

    #[test]
    fn rejects_empty_startup_command() {
        let args = ["lumalla", "--"].into_iter().map(String::from);
        assert!(Args::parse(args).is_none());
    }
}
