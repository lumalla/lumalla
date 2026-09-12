use std::path::PathBuf;

/// Arguments provided at process start
#[derive(Debug, Default, Clone)]
pub struct Args {
    /// Path to log file
    pub log_file: Option<String>,
    /// Path to config file
    pub config_path: Option<PathBuf>,
    /// Enable the Lua REPL over a Unix socket (default path if unset).
    pub repl: bool,
    /// Unix socket path for the Lua REPL. Implies [`Self::repl`].
    pub repl_socket: Option<PathBuf>,
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
                "--repl" => {
                    global_args.repl = true;
                }
                "--repl-socket" => {
                    if let Some(path) = args.next() {
                        global_args.repl = true;
                        global_args.repl_socket = Some(PathBuf::from(path));
                    } else {
                        eprintln!("--repl-socket requires a path argument");
                        print_help(&program_name);
                        return None;
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

    /// Resolved Unix socket path when the REPL is enabled.
    pub fn repl_socket_path(&self) -> anyhow::Result<Option<PathBuf>> {
        if !self.repl {
            return Ok(None);
        }
        if let Some(path) = &self.repl_socket {
            return Ok(Some(path.clone()));
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
            anyhow::anyhow!("XDG_RUNTIME_DIR not set; pass --repl-socket <PATH>")
        })?;
        Ok(Some(PathBuf::from(runtime).join(DEFAULT_REPL_SOCKET_NAME)))
    }
}

/// Default REPL socket file name under `$XDG_RUNTIME_DIR`.
///
/// Debug builds use a distinct name so a release config and a debug config can
/// expose REPLs side by side (same idea as [`lumalla_ipc::BUS_NAME`]).
const DEFAULT_REPL_SOCKET_NAME: &str = if cfg!(debug_assertions) {
    "lumalla-config.debug.sock"
} else {
    "lumalla-config.sock"
};

fn print_help(program_name: &str) {
    println!("Usage: {} [OPTIONS]", program_name);
    println!("Options:");
    println!("  -h, --help                  Print this help message and exit");
    println!("  -l, --log-file <FILE>       Path to log file");
    println!("  --config <FILE>             Path to lua config file");
    println!("  --repl                      Enable Lua REPL on a Unix socket");
    println!("  --repl-socket <PATH>        REPL socket path (implies --repl)");
    println!();
    println!("When --repl is set without --repl-socket, the socket defaults to");
    println!("$XDG_RUNTIME_DIR/{DEFAULT_REPL_SOCKET_NAME}");
}
