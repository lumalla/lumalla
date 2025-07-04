use log::warn;
use mio::{Poll, Waker};
use std::sync::{Arc, mpsc};

use crate::{
    ConfigMessage, DisplayMessage, InputMessage, MESSAGE_CHANNEL_TOKEN, MainMessage,
    RendererMessage,
};

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
    to_display: MessageSender<DisplayMessage>,
    to_renderer: MessageSender<RendererMessage>,
    to_input: MessageSender<InputMessage>,
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
        to_display: MessageSender<DisplayMessage>,
        to_renderer: MessageSender<RendererMessage>,
        to_input: MessageSender<InputMessage>,
        to_config: MessageSender<ConfigMessage>,
    ) -> Self {
        Comms {
            to_main,
            to_display,
            to_renderer,
            to_input,
            to_config,
        }
    }

    /// Sends a message to the main thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
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
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
    /// let sender = comms.main_sender();
    /// sender.send(MainMessage::Shutdown).unwrap();
    /// assert!(matches!(main_channel.recv().unwrap(), MainMessage::Shutdown));
    /// ```
    pub fn main_sender(&self) -> MessageSender<MainMessage> {
        self.to_main.clone()
    }

    /// Sends a message to the display thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, display_channel, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
    /// comms.display(DisplayMessage::Shutdown);
    /// assert!(matches!(display_channel.recv().unwrap(), DisplayMessage::Shutdown));
    /// ```
    pub fn display(&self, message: DisplayMessage) {
        if let Err(e) = self.to_display.send(message) {
            warn!("Lost connection to display ({e}). Requesting shutdown");
            self.to_main
                .send(MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }

    /// Get a message sender for sending messages to the display thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, display_channel, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
    /// let sender = comms.display_sender();
    /// sender.send(DisplayMessage::Shutdown).unwrap();
    /// assert!(matches!(display_channel.recv().unwrap(), DisplayMessage::Shutdown));
    /// ```
    pub fn display_sender(&self) -> MessageSender<DisplayMessage> {
        self.to_display.clone()
    }

    /// Sends a message to the renderer thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, renderer_channel, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
    /// comms.renderer(RendererMessage::Shutdown);
    /// assert!(matches!(renderer_channel.recv().unwrap(), RendererMessage::Shutdown));
    /// ```
    pub fn renderer(&self, message: RendererMessage) {
        if let Err(e) = self.to_renderer.send(message) {
            warn!("Lost connection to renderer ({e}). Requesting shutdown");
            self.to_main
                .send(MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }

    /// Get a message sender for sending messages to the renderer thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, renderer_channel, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
    /// let sender = comms.renderer_sender();
    /// sender.send(RendererMessage::Shutdown).unwrap();
    /// assert!(matches!(renderer_channel.recv().unwrap(), RendererMessage::Shutdown));
    /// ```
    pub fn renderer_sender(&self) -> MessageSender<RendererMessage> {
        self.to_renderer.clone()
    }

    /// Sends a message to the input thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, input_channel, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
    /// comms.input(InputMessage::Shutdown);
    /// assert!(matches!(input_channel.recv().unwrap(), InputMessage::Shutdown));
    /// ```
    pub fn input(&self, message: InputMessage) {
        if let Err(e) = self.to_input.send(message) {
            warn!("Lost connection to input ({e}). Requesting shutdown");
            self.to_main
                .send(MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }

    /// Get a message sender for sending messages to the input thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, input_channel, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, _, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
    /// let sender = comms.input_sender();
    /// sender.send(InputMessage::Shutdown).unwrap();
    /// assert!(matches!(input_channel.recv().unwrap(), InputMessage::Shutdown));
    /// ```
    pub fn input_sender(&self) -> MessageSender<InputMessage> {
        self.to_input.clone()
    }

    /// Sends a message to the config thread.
    ///
    /// # Example
    /// ```
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, config_channel, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
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
    /// # use lumalla_shared::{Comms, MainMessage, DisplayMessage, RendererMessage, InputMessage, ConfigMessage, message_loop_with_channel};
    /// # let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
    /// # let (_, _, to_display) = message_loop_with_channel::<DisplayMessage>().unwrap();
    /// # let (_, _, to_renderer) = message_loop_with_channel::<RendererMessage>().unwrap();
    /// # let (_, _, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
    /// # let (_, config_channel, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();
    /// # let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);
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
        display: mpsc::Receiver<DisplayMessage>,
        renderer: mpsc::Receiver<RendererMessage>,
        input: mpsc::Receiver<InputMessage>,
        config: mpsc::Receiver<ConfigMessage>,
    }

    fn comms() -> (Comms, Receivers) {
        let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, display_channel, to_display) =
            message_loop_with_channel::<DisplayMessage>().unwrap();
        let (_, renderer_channel, to_renderer) =
            message_loop_with_channel::<RendererMessage>().unwrap();
        let (_, input_channel, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
        let (_, config_channel, to_config) = message_loop_with_channel::<ConfigMessage>().unwrap();

        let comms = Comms::new(to_main, to_display, to_renderer, to_input, to_config);

        (
            comms,
            Receivers {
                main: main_channel,
                display: display_channel,
                renderer: renderer_channel,
                input: input_channel,
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
    fn to_display_sends_shutdown_to_main_on_lost_connection_to_display() {
        let (comms, receivers) = comms();

        // Close the channel to the display thread
        drop(receivers.display);

        comms.display(DisplayMessage::Shutdown);
        assert!(matches!(
            receivers.main.recv().unwrap(),
            MainMessage::Shutdown
        ));
    }

    #[test]
    #[should_panic]
    fn to_display_panics_on_lost_connection_to_display_and_main() {
        let (comms, receivers) = comms();

        // Close the display and main channels
        drop(receivers.display);
        drop(receivers.main);

        comms.display(DisplayMessage::Shutdown);
    }

    #[test]
    fn to_renderer_sends_shutdown_to_main_on_lost_connection_to_renderer() {
        let (comms, receivers) = comms();

        // Close the renderer channel
        drop(receivers.renderer);

        comms.renderer(RendererMessage::Shutdown);
        assert!(matches!(
            receivers.main.recv().unwrap(),
            MainMessage::Shutdown
        ));
    }

    #[test]
    #[should_panic]
    fn to_renderer_panics_on_lost_connection_to_renderer_and_main() {
        let (comms, receivers) = comms();

        // Close the renderer and main channels
        drop(receivers.renderer);
        drop(receivers.main);

        comms.renderer(RendererMessage::Shutdown);
    }

    #[test]
    fn to_input_sends_shutdown_to_main_on_lost_connection_to_input() {
        let (comms, receivers) = comms();

        // Close the input channel
        drop(receivers.input);

        comms.input(InputMessage::Shutdown);
        assert!(matches!(
            receivers.main.recv().unwrap(),
            MainMessage::Shutdown
        ));
    }

    #[test]
    #[should_panic]
    fn to_input_panics_on_lost_connection_to_input_and_main() {
        let (comms, receivers) = comms();

        // Close the input and main channels
        drop(receivers.input);
        drop(receivers.main);

        comms.input(InputMessage::Shutdown);
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
