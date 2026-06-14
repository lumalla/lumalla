use log::warn;
use mio::{Poll, Waker};
use std::sync::{Arc, mpsc};

use crate::{ConfigMessage, MESSAGE_CHANNEL_TOKEN, MainMessage};

/// Create a new event loop with a message channel already set up
pub fn message_loop_with_channel<M>() -> anyhow::Result<(Poll, mpsc::Receiver<M>, MessageSender<M>)>
{
    let event_loop = mio::Poll::new()?;
    let (sender, receiver) = mpsc::channel();
    let waker = Waker::new(event_loop.registry(), MESSAGE_CHANNEL_TOKEN)?;
    Ok((
        event_loop,
        receiver,
        MessageSender::new(sender, Arc::new(waker)),
    ))
}

/// A sender that works with mio channels
#[derive(Debug)]
pub struct MessageSender<T> {
    sender: mpsc::Sender<T>,
    waker: std::sync::Arc<mio::Waker>,
}

impl<T> Clone for MessageSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            waker: self.waker.clone(),
        }
    }
}

impl<T> MessageSender<T> {
    /// Create a new MioSender
    pub fn new(sender: mpsc::Sender<T>, waker: std::sync::Arc<mio::Waker>) -> Self {
        Self { sender, waker }
    }

    /// Send a message and wake up the event loop
    pub fn send(&self, message: T) -> Result<(), mpsc::SendError<T>> {
        let result = self.sender.send(message);
        if result.is_ok() {
            let _ = self.waker.wake();
        }
        result
    }
}

/// Holds the channels for general communication and sending messages to the different threads.
/// Also, provides some convenience methods for interacting with other threads.
#[derive(Clone)]
pub struct Comms {
    to_main: MessageSender<MainMessage>,
    to_config: MessageSender<ConfigMessage>,
}

impl std::fmt::Debug for Comms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Comms").finish()
    }
}

impl Comms {
    /// Creates a new instance of `Comms` with the given channels.
    pub fn new(
        to_main: MessageSender<MainMessage>,
        to_config: MessageSender<ConfigMessage>,
    ) -> Self {
        Comms { to_main, to_config }
    }

    /// Sends a message to the main thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_config);
    /// comms.main(MainMessage::Shutdown);
    /// assert!(matches!(main_channel.recv().unwrap(), MainMessage::Shutdown));
    /// ```
    pub fn main(&self, message: MainMessage) {
        self.to_main
            .send(message)
            .expect("Lost connection to the main thread");
    }

    /// Get a message sender for sending messages to the main thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_config);
    /// let sender = comms.main_sender();
    /// sender.send(MainMessage::Shutdown).unwrap();
    /// assert!(matches!(main_channel.recv().unwrap(), MainMessage::Shutdown));
    /// ```
    pub fn main_sender(&self) -> MessageSender<MainMessage> {
        self.to_main.clone()
    }

    /// Sends a message to the config thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, config_channel, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_config);
    /// comms.config(ConfigMessage::Shutdown);
    /// assert!(matches!(config_channel.recv().unwrap(), ConfigMessage::Shutdown));
    /// ```
    pub fn config(&self, message: ConfigMessage) {
        if let Err(e) = self.to_config.send(message) {
            warn!("Lost connection to config ({e}). Requesting shutdown");
            self.to_main
                .send(MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }

    /// Get a message sender for sending messages to the config thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, config_channel, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_config);
    /// let sender = comms.config_sender();
    /// sender.send(ConfigMessage::Shutdown).unwrap();
    /// assert!(matches!(config_channel.recv().unwrap(), ConfigMessage::Shutdown));
    /// ```
    pub fn config_sender(&self) -> MessageSender<ConfigMessage> {
        self.to_config.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    struct Receivers {
        main: mpsc::Receiver<MainMessage>,
        config: mpsc::Receiver<ConfigMessage>,
    }

    fn comms() -> (Comms, Receivers) {
        let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, config_channel, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();

        let comms = Comms::new(to_main, to_config);

        (
            comms,
            Receivers {
                main: main_channel,
                config: config_channel,
            },
        )
    }

    #[test]
    #[should_panic]
    fn to_main_panics_on_lost_connection() {
        let (comms, receivers) = comms();

        // Close the channel to the main thread
        drop(receivers.main);

        comms.main(MainMessage::Shutdown);
    }

    #[test]
    fn to_config_sends_shutdown_to_main_on_lost_connection_to_config() {
        let (comms, receivers) = comms();

        // Close the config channel
        drop(receivers.config);

        comms.config(ConfigMessage::Shutdown);
        assert!(matches!(
            receivers.main.recv().unwrap(),
            MainMessage::Shutdown
        ));
    }

    #[test]
    #[should_panic]
    fn to_config_panics_on_lost_connection_to_config_and_main() {
        let (comms, receivers) = comms();

        // Close the config and main channels
        drop(receivers.config);
        drop(receivers.main);

        comms.config(ConfigMessage::Shutdown);
    }
}
