use std::path::Path;

use anyhow::Context;
use log::error;
use lumalla_shared::{ConfigMessage, MessageSender};
use notify::{
    EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind, recommended_watcher,
};

pub struct ConfigWatcher {
    watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    pub fn new(message_sender: MessageSender<ConfigMessage>) -> anyhow::Result<Self> {
        let watcher =
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
                            if let Err(e) =
                                message_sender.send(ConfigMessage::LoadConfig(path.to_owned()))
                            {
                                error!("Failed to send config change notification: {e}");
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        error!("File watcher had an error: {err}")
                    }
                }
            })
            .context("Failed to create file watcher")?;

        Ok(Self { watcher })
    }

    pub fn watch(&mut self, path: &Path) -> anyhow::Result<()> {
        self.watcher
            .watch(path, RecursiveMode::NonRecursive)
            .context("Failed to watch config file")
    }
}
