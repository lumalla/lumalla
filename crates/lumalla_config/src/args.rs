use std::path::PathBuf;

/// Arguments provided at process start
#[derive(Debug, Default, Clone)]
pub struct Args {
    /// Path to log file
    pub log_file: Option<String>,
    /// Path to config file
    pub config_path: Option<PathBuf>,
}

impl Args {
    /// Parse arguments for the external config client.
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
                "--config" => {
                    if let Some(config_path) = args.next() {
                        global_args.config_path = Some(PathBuf::from(config_path));
                    }
                }
                "-h" | "--help" => {
                    print_help(&program_name);
                    return None;
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
}
