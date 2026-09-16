//! `org.gnome.Mutter.ScreenCast` — enough for xdg-desktop-portal-gnome.

use std::{
    collections::HashMap,
    mem,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use log::{debug, warn};
use lumalla_ipc::types::OutputInfo;
use lumalla_shared::{Comms, MainMessage};
use zbus::{
    fdo,
    interface,
    object_server::SignalEmitter,
    zvariant::{DeserializeDict, OwnedObjectPath, SerializeDict, Type, Value},
    blocking::Connection,
};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

/// Shared handles so the D-Bus thread can emit `PipeWireStreamAdded`.
#[derive(Clone)]
pub(crate) struct MutterStreamRegistry {
    /// mutter_stream_id → object path
    paths: Arc<Mutex<HashMap<u64, OwnedObjectPath>>>,
    connection: Connection,
}

impl MutterStreamRegistry {
    fn new(connection: Connection) -> Self {
        Self {
            paths: Arc::new(Mutex::new(HashMap::new())),
            connection,
        }
    }

    fn insert(&self, id: u64, path: OwnedObjectPath) {
        self.paths.lock().unwrap().insert(id, path);
    }

    fn remove(&self, id: u64) {
        self.paths.lock().unwrap().remove(&id);
    }

    fn take_path(&self, id: u64) -> Option<OwnedObjectPath> {
        self.paths.lock().unwrap().remove(&id)
    }
}

/// Emit `PipeWireStreamAdded` (or log failure) after the main thread starts PipeWire.
pub(crate) fn complete_mutter_stream(
    registry: &MutterStreamRegistry,
    mutter_stream_id: u64,
    result: Result<u32, String>,
) {
    let Some(path) = registry.take_path(mutter_stream_id) else {
        warn!("Mutter stream reply for unknown id={mutter_stream_id}");
        return;
    };

    match result {
        Ok(node_id) => {
            if let Err(err) = registry.connection.emit_signal(
                None::<()>,
                path.as_str(),
                "org.gnome.Mutter.ScreenCast.Stream",
                "PipeWireStreamAdded",
                &(node_id,),
            ) {
                warn!("Failed to emit PipeWireStreamAdded: {err}");
            } else {
                debug!("Emitted PipeWireStreamAdded node_id={node_id} on {path}");
            }
            // Keep path available only until signal; re-insert so Stop can find nothing.
            // Session Stop removes objects via object_server; path map entry is gone.
        }
        Err(err) => {
            warn!("Mutter ScreenCast start failed for stream {mutter_stream_id}: {err}");
        }
    }
}

#[derive(Clone)]
pub(crate) struct ScreenCast {
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    comms: Comms,
    connection: Connection,
    registry: MutterStreamRegistry,
}

impl ScreenCast {
    pub(crate) fn new(
        outputs: Arc<Mutex<Vec<OutputInfo>>>,
        comms: Comms,
        connection: Connection,
    ) -> (Self, MutterStreamRegistry) {
        let registry = MutterStreamRegistry::new(connection.clone());
        (
            Self {
                outputs,
                comms,
                connection,
                registry: registry.clone(),
            },
            registry,
        )
    }
}

#[derive(Debug, Default, DeserializeDict, Type, Clone, Copy, PartialEq, Eq)]
#[zvariant(signature = "dict")]
struct RecordMonitorProperties {
    #[zvariant(rename = "cursor-mode")]
    _cursor_mode: Option<u32>,
    #[zvariant(rename = "is-recording")]
    _is_recording: Option<bool>,
}

#[derive(Debug, SerializeDict, Type, Value)]
#[zvariant(signature = "dict")]
struct StreamParameters {
    position: (i32, i32),
    size: (i32, i32),
    #[zvariant(rename = "output-name")]
    output_name: String,
}

#[derive(Clone)]
struct Session {
    id: u64,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    comms: Comms,
    connection: Connection,
    registry: MutterStreamRegistry,
    streams: Arc<Mutex<Vec<(Stream, OwnedObjectPath)>>>,
    stopped: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Stream {
    id: u64,
    session_id: u64,
    connector: String,
    position: (i32, i32),
    size: (i32, i32),
    was_started: Arc<AtomicBool>,
    comms: Comms,
}

#[interface(name = "org.gnome.Mutter.ScreenCast")]
impl ScreenCast {
    fn create_session(
        &self,
        properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        if properties.contains_key("remote-desktop-session-id") {
            return Err(fdo::Error::Failed(
                "remote desktop sessions are not supported".into(),
            ));
        }

        let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{session_id}");
        let path = OwnedObjectPath::try_from(path)
            .map_err(|err| fdo::Error::Failed(format!("invalid session path: {err}")))?;

        let session = Session {
            id: session_id,
            outputs: Arc::clone(&self.outputs),
            comms: self.comms.clone(),
            connection: self.connection.clone(),
            registry: self.registry.clone(),
            streams: Arc::new(Mutex::new(Vec::new())),
            stopped: Arc::new(AtomicBool::new(false)),
        };

        match self.connection.object_server().at(&path, session) {
            Ok(true) => Ok(path),
            Ok(false) => Err(fdo::Error::Failed("session path already exists".into())),
            Err(err) => Err(fdo::Error::Failed(format!(
                "error creating session object: {err}"
            ))),
        }
    }

    #[zbus(property)]
    fn version(&self) -> i32 {
        4
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    fn start(&self) {
        debug!("Mutter ScreenCast session {} start", self.id);
        for (stream, _) in self.streams.lock().unwrap().iter() {
            stream.start();
        }
    }

    fn stop(&self, #[zbus(signal_context)] ctxt: SignalEmitter<'_>) {
        debug!("Mutter ScreenCast session {} stop", self.id);
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }

        let _ = self.connection.emit_signal(
            None::<()>,
            ctxt.path().as_str(),
            "org.gnome.Mutter.ScreenCast.Session",
            "Closed",
            &(),
        );

        self.comms.main(MainMessage::StopMutterScreenCast {
            session_id: self.id,
        });

        let streams = mem::take(&mut *self.streams.lock().unwrap());
        for (stream, path) in streams {
            self.registry.remove(stream.id);
            let _ = self
                .connection
                .object_server()
                .remove::<Stream, _>(path.as_str());
        }

        let _ = self
            .connection
            .object_server()
            .remove::<Session, _>(ctxt.path());
    }

    fn record_monitor(
        &mut self,
        connector: &str,
        _properties: RecordMonitorProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        debug!("Mutter RecordMonitor connector={connector}");

        let output = {
            let outputs = self.outputs.lock().unwrap();
            outputs.iter().find(|o| o.name == connector).cloned()
        };
        let Some(output) = output else {
            return Err(fdo::Error::Failed(format!("no such monitor: {connector}")));
        };
        if output.width <= 0 || output.height <= 0 {
            return Err(fdo::Error::Failed("monitor has invalid size".into()));
        }

        let stream_id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{stream_id}");
        let path = OwnedObjectPath::try_from(path)
            .map_err(|err| fdo::Error::Failed(format!("invalid stream path: {err}")))?;

        let stream = Stream {
            id: stream_id,
            session_id: self.id,
            connector: connector.to_string(),
            position: (output.x, output.y),
            size: (output.width, output.height),
            was_started: Arc::new(AtomicBool::new(false)),
            comms: self.comms.clone(),
        };

        match self.connection.object_server().at(&path, stream.clone()) {
            Ok(true) => {
                self.registry.insert(stream_id, path.clone());
                self.streams.lock().unwrap().push((stream, path.clone()));
                Ok(path)
            }
            Ok(false) => Err(fdo::Error::Failed("stream path already exists".into())),
            Err(err) => Err(fdo::Error::Failed(format!(
                "error creating stream object: {err}"
            ))),
        }
    }

    fn record_window(
        &mut self,
        _properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        Err(fdo::Error::Failed(
            "RecordWindow is not supported yet".into(),
        ))
    }

    fn record_area(
        &mut self,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
        _properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        Err(fdo::Error::Failed("RecordArea is not supported yet".into()))
    }

    fn record_virtual(
        &mut self,
        _properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        Err(fdo::Error::Failed(
            "RecordVirtual is not supported yet".into(),
        ))
    }

    #[zbus(signal)]
    async fn closed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
impl Stream {
    #[zbus(signal)]
    async fn pipe_wire_stream_added(ctxt: &SignalEmitter<'_>, node_id: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn parameters(&self) -> StreamParameters {
        StreamParameters {
            position: self.position,
            size: self.size,
            output_name: self.connector.clone(),
        }
    }
}

impl Stream {
    fn start(&self) {
        if self.was_started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.comms.main(MainMessage::StartMutterScreenCast {
            mutter_stream_id: self.id,
            session_id: self.session_id,
            connector: self.connector.clone(),
        });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            self.comms.main(MainMessage::StopMutterScreenCast {
                session_id: self.id,
            });
        }
    }
}
