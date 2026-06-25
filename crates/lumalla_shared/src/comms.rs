use log::warn;
use mio::{Poll, Token, Waker};
use std::sync::{Arc, mpsc};

use crate::{
    DbusMessage, DisplayMessage, InputMessage, MESSAGE_CHANNEL_TOKEN, MainMessage, RendererMessage,
    SeatMessage,
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

/// Attach a message channel to an existing event loop.
pub fn message_sender_on_poll<M>(
    poll: &Poll,
    token: Token,
) -> anyhow::Result<(mpsc::Receiver<M>, MessageSender<M>)> {
    let (sender, receiver) = mpsc::channel();
    let waker = Waker::new(poll.registry(), token)?;
    Ok((receiver, MessageSender::new(sender, Arc::new(waker))))
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
    to_dbus: MessageSender<DbusMessage>,
    to_display: MessageSender<DisplayMessage>,
    to_input: MessageSender<InputMessage>,
    to_renderer: MessageSender<RendererMessage>,
    to_seat: MessageSender<SeatMessage>,
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
        to_dbus: MessageSender<DbusMessage>,
        to_display: MessageSender<DisplayMessage>,
        to_input: MessageSender<InputMessage>,
        to_renderer: MessageSender<RendererMessage>,
        to_seat: MessageSender<SeatMessage>,
    ) -> Self {
        Comms {
            to_main,
            to_dbus,
            to_display,
            to_input,
            to_renderer,
            to_seat,
        }
    }

    /// Sends a message to the main thread.
    pub fn main(&self, message: MainMessage) {
        self.to_main
            .send(message)
            .expect("Lost connection to the main thread");
    }

    /// Get a message sender for sending messages to the main thread.
    pub fn main_sender(&self) -> MessageSender<MainMessage> {
        self.to_main.clone()
    }

    /// Sends a message to the D-Bus thread.
    pub fn dbus(&self, message: DbusMessage) {
        if let Err(e) = self.to_dbus.send(message) {
            warn!("Lost connection to D-Bus ({e}). Requesting shutdown");
            self.to_main
                .send(MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }

    /// Get a message sender for sending messages to the D-Bus thread.
    pub fn dbus_sender(&self) -> MessageSender<DbusMessage> {
        self.to_dbus.clone()
    }

    /// Sends a message to the seat handler on the main thread.
    pub fn seat(&self, message: SeatMessage) {
        if let Err(e) = self.to_seat.send(message) {
            warn!("Lost connection to seat ({e}). Requesting shutdown");
            self.to_main
                .send(MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }

    /// Sends a message to the display thread.
    pub fn display(&self, message: DisplayMessage) {
        if let Err(e) = self.to_display.send(message) {
            warn!("Lost connection to display ({e}). Requesting shutdown");
            self.to_main
                .send(MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }

    /// Sends a message to the input thread.
    pub fn input(&self, message: InputMessage) {
        if let Err(e) = self.to_input.send(message) {
            warn!("Lost connection to input ({e}). Requesting shutdown");
            self.to_main
                .send(MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }

    /// Sends a message to the renderer thread.
    pub fn renderer(&self, message: RendererMessage) {
        if let Err(e) = self.to_renderer.send(message) {
            warn!("Lost connection to renderer ({e}). Requesting shutdown");
            self.to_main
                .send(MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    struct Receivers {
        main: mpsc::Receiver<MainMessage>,
        dbus: mpsc::Receiver<DbusMessage>,
        display: mpsc::Receiver<DisplayMessage>,
        input: mpsc::Receiver<InputMessage>,
        renderer: mpsc::Receiver<RendererMessage>,
        seat: mpsc::Receiver<SeatMessage>,
    }

    fn comms() -> (Comms, Receivers) {
        let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, dbus_channel, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();
        let (_, display_channel, to_display) =
            message_loop_with_channel::<DisplayMessage>().unwrap();
        let (_, input_channel, to_input) = message_loop_with_channel::<InputMessage>().unwrap();
        let (_, renderer_channel, to_renderer) =
            message_loop_with_channel::<RendererMessage>().unwrap();
        let (_, seat_channel, to_seat) = message_loop_with_channel::<SeatMessage>().unwrap();

        let comms = Comms::new(
            to_main,
            to_dbus,
            to_display,
            to_input,
            to_renderer,
            to_seat,
        );

        (
            comms,
            Receivers {
                main: main_channel,
                dbus: dbus_channel,
                display: display_channel,
                input: input_channel,
                renderer: renderer_channel,
                seat: seat_channel,
            },
        )
    }

    #[test]
    #[should_panic]
    fn to_main_panics_on_lost_connection() {
        let (comms, receivers) = comms();

        drop(receivers.main);

        comms.main(MainMessage::Shutdown);
    }

    #[test]
    fn to_dbus_sends_shutdown_to_main_on_lost_connection_to_dbus() {
        let (comms, receivers) = comms();

        drop(receivers.dbus);

        comms.dbus(DbusMessage::Shutdown);
        assert!(matches!(
            receivers.main.recv().unwrap(),
            MainMessage::Shutdown
        ));
    }

    #[test]
    #[should_panic]
    fn to_dbus_panics_on_lost_connection_to_dbus_and_main() {
        let (comms, receivers) = comms();

        drop(receivers.dbus);
        drop(receivers.main);

        comms.dbus(DbusMessage::Shutdown);
    }
}
