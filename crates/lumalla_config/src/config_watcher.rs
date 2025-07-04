use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
};

use log::error;
use mio::{Token, Waker};
use notify::{
    EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind, recommended_watcher,
};

pub struct ConfigWatcher {
    receiver: mpsc::Receiver<PathBuf>,
    waker: Arc<Waker>,
    _watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    pub fn new(path: PathBuf, waker: Arc<Waker>) -> Self {
        let (sender, receiver) = mpsc::channel();

        let waker_clone = waker.clone();
        let mut watcher =
            recommended_watcher(move |event_res: Result<notify::Event, notify::Error>| {
                match &event_res {
                    Ok(event) => {
                        match &event.kind {
                            EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)) => {
                                // No change to file contents
                                return;
                            }
                            _ => {}
                        }

                        for path in &event.paths {
                            if let Err(e) = sender.send(path.to_owned()) {
                                error!("Failed to send config change notification: {e}");
                                return;
                            }
                        }

                        // Wake up the event loop
                        if let Err(e) = waker_clone.wake() {
                            error!("Failed to wake event loop: {e}");
                        }
                    }
                    Err(err) => {
                        error!("File watcher had an error: {err}")
                    }
                }
            })
            .unwrap();

        if let Err(err) = watcher.watch(path.as_path(), RecursiveMode::NonRecursive) {
            error!("Unable to setup config file change watcher: {err}");
        }

        Self {
            receiver,
            waker,
            _watcher: watcher,
        }
    }

    /// Try to receive file change events
    pub fn try_recv(&self) -> Result<PathBuf, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Get the waker associated with this config watcher
    pub fn waker(&self) -> Arc<Waker> {
        self.waker.clone()
    }
}
