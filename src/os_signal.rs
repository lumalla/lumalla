use std::thread;

use log::{error, info, warn};
use lumalla_shared::{MainMessage, MessageSender};
use signal_hook::{consts::SIGINT, iterator::Signals};

/// Handles signals sent to the process, such as SIGINT (Ctrl+C).
/// When the signal is received, the main thread is notified to initiate a graceful shutdown.
pub(crate) fn handle_signals(to_main: MessageSender<MainMessage>) -> anyhow::Result<()> {
    thread::Builder::new()
        .name("signals".to_string())
        .spawn(move || {
            let mut signals = match Signals::new([SIGINT]) {
                Ok(signals) => signals,
                Err(e) => {
                    error!("Failed to register signal handler: {e}");
                    return;
                }
            };

            for signal in signals.forever() {
                match signal {
                    SIGINT => {
                        info!("Received SIGINT signal (Ctrl+C), initiating graceful shutdown");
                        if let Err(e) = to_main.send(MainMessage::Shutdown) {
                            error!("Failed to send shutdown message: {e}");
                        }
                        break;
                    }
                    _ => {
                        warn!("Received unexpected signal: {signal}");
                    }
                }
            }
        })?;

    Ok(())
}
