use log::warn;
use std::sync::{Arc, mpsc};

use crate::ring::{EventLoop, SharedWaker};

/// Create a new event loop with a message channel already set up.
pub fn message_loop_with_channel<M>()
-> anyhow::Result<(EventLoop, mpsc::Receiver<M>, MessageSender<M>)> {
    let event_loop = EventLoop::new(1024)?;
    let (sender, receiver) = mpsc::channel();
    let waker = Arc::new(event_loop.waker());
    Ok((event_loop, receiver, MessageSender::new(sender, waker)))
}

/// A sender that wakes an [`EventLoop`] when a message is queued.
#[derive(Debug)]
pub struct MessageSender<T> {
    sender: mpsc::Sender<T>,
    waker: SharedWaker,
}

impl<T> Clone for MessageSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            waker: Arc::clone(&self.waker),
        }
    }
}

impl<T> MessageSender<T> {
    pub fn new(sender: mpsc::Sender<T>, waker: SharedWaker) -> Self {
        Self { sender, waker }
    }

    /// Send a message and wake up the event loop.
    pub fn send(&self, message: T) -> Result<(), mpsc::SendError<T>> {
        let result = self.sender.send(message);
        if result.is_ok() {
            let _ = self.waker.wake();
        }
        result
    }
}

/// Holds the channels for general communication and sending messages to the different threads.
#[derive(Clone)]
pub struct Comms {
    to_main: MessageSender<crate::MainMessage>,
    to_dbus: MessageSender<crate::DbusMessage>,
}

impl std::fmt::Debug for Comms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Comms").finish()
    }
}

impl Comms {
    pub fn new(
        to_main: MessageSender<crate::MainMessage>,
        to_dbus: MessageSender<crate::DbusMessage>,
    ) -> Self {
        Comms { to_main, to_dbus }
    }

    pub fn main(&self, message: crate::MainMessage) {
        self.to_main
            .send(message)
            .expect("Lost connection to the main thread");
    }

    pub fn main_sender(&self) -> MessageSender<crate::MainMessage> {
        self.to_main.clone()
    }

    pub fn dbus(&self, message: crate::DbusMessage) {
        if let Err(e) = self.to_dbus.send(message) {
            warn!("Lost connection to D-Bus ({e}). Requesting shutdown");
            self.to_main
                .send(crate::MainMessage::Shutdown)
                .expect("Lost connection to the main thread");
        }
    }

    pub fn dbus_sender(&self) -> MessageSender<crate::DbusMessage> {
        self.to_dbus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DbusMessage, MainMessage};
    use std::sync::mpsc;

    struct Receivers {
        main: mpsc::Receiver<MainMessage>,
        dbus: mpsc::Receiver<DbusMessage>,
    }

    fn comms() -> (Comms, Receivers) {
        let (_, main_channel, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, dbus_channel, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();

        let comms = Comms::new(to_main, to_dbus);

        (
            comms,
            Receivers {
                main: main_channel,
                dbus: dbus_channel,
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
