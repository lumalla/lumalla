//! Unix-socket Lua REPL for the config process.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use anyhow::Context;
use log::{info, warn};

/// A line of Lua submitted by a REPL client.
pub(crate) struct ReplRequest {
    pub chunk: String,
    pub reply: mpsc::Sender<ReplResponse>,
}

/// Result of evaluating a REPL chunk on the config main thread.
pub(crate) struct ReplResponse {
    pub ok: bool,
    pub text: String,
}

/// Bind a Unix socket and accept REPL clients on a background thread.
///
/// Each client line is forwarded as a [`ReplRequest`] for evaluation on the
/// config main thread. The socket is created with mode `0o600`.
pub(crate) fn start_repl_server(
    socket_path: PathBuf,
    request_tx: mpsc::Sender<ReplRequest>,
) -> anyhow::Result<()> {
    prepare_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Failed to bind REPL socket at {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .context("Failed to set REPL socket permissions to 0600")?;

    info!("Lua REPL listening on {}", socket_path.display());

    thread::Builder::new()
        .name(String::from("lumalla-config-repl"))
        .spawn(move || accept_loop(listener, request_tx))
        .context("Failed to spawn REPL accept thread")?;

    Ok(())
}

fn prepare_socket_path(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create REPL socket directory {}", parent.display()))?;
        }
    }
    if path.exists() {
        fs::remove_file(path).with_context(|| {
            format!("Failed to remove stale REPL socket at {}", path.display())
        })?;
    }
    Ok(())
}

fn accept_loop(listener: UnixListener, request_tx: mpsc::Sender<ReplRequest>) {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let tx = request_tx.clone();
                if let Err(err) = thread::Builder::new()
                    .name(String::from("lumalla-config-repl-client"))
                    .spawn(move || handle_client(stream, tx))
                {
                    warn!("Failed to spawn REPL client thread: {err}");
                }
            }
            Err(err) => {
                warn!("REPL accept failed: {err}");
                break;
            }
        }
    }
}

fn handle_client(stream: UnixStream, request_tx: mpsc::Sender<ReplRequest>) {
    let mut writer = match stream.try_clone() {
        Ok(clone) => clone,
        Err(err) => {
            warn!("Failed to clone REPL client stream: {err}");
            return;
        }
    };
    let reader = BufReader::new(stream);

    let _ = writeln!(
        writer,
        "lumalla config REPL — `lumalla` is preloaded; empty line ignored; Ctrl-D to exit"
    );
    let _ = write!(writer, "> ");
    let _ = writer.flush();

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                warn!("REPL client read error: {err}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            let _ = write!(writer, "> ");
            let _ = writer.flush();
            continue;
        }
        if matches!(trimmed, ".quit" | ".exit") {
            let _ = writeln!(writer, "bye");
            break;
        }

        let (reply_tx, reply_rx) = mpsc::channel();
        if request_tx
            .send(ReplRequest {
                chunk: line,
                reply: reply_tx,
            })
            .is_err()
        {
            let _ = writeln!(writer, "error: config process is shutting down");
            break;
        }

        match reply_rx.recv() {
            Ok(response) => {
                if response.ok {
                    if !response.text.is_empty() {
                        let _ = writeln!(writer, "{}", response.text);
                    }
                } else {
                    let _ = writeln!(writer, "error: {}", response.text);
                }
            }
            Err(_) => {
                let _ = writeln!(writer, "error: no response from config process");
                break;
            }
        }

        let _ = write!(writer, "> ");
        let _ = writer.flush();
    }
}
