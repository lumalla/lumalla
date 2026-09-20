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
use lumalla_shared::{Comms, MainMessage, MutterScreenCastTarget, WindowState};
use zbus::{
    fdo,
    interface,
    object_server::SignalEmitter,
    zvariant::{DeserializeDict, OwnedObjectPath, SerializeDict, Type, Value},
    blocking::Connection,
};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

/// Keep portal stream `size` aligned with DMA-BUF PipeWire buffers.
fn fit_portal_size(width: i32, height: i32) -> (i32, i32) {
    // Match lumalla_screencast::fit_output_size (DMA max edge 7680).
    const MAX_EDGE: i32 = 7680;
    let width = width.max(1);
    let height = height.max(1);
    let longest = width.max(height);
    if longest <= MAX_EDGE {
        return (width, height);
    }
    let w = ((width as i64) * (MAX_EDGE as i64) / (longest as i64)).max(1) as i32;
    let h = ((height as i64) * (MAX_EDGE as i64) / (longest as i64)).max(1) as i32;
    (w, h)
}

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
        }
        Err(err) => {
            warn!("Mutter ScreenCast start failed for stream {mutter_stream_id}: {err}");
        }
    }
}

#[derive(Clone)]
pub(crate) struct ScreenCast {
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    windows: Arc<Mutex<Vec<WindowState>>>,
    comms: Comms,
    connection: Connection,
    registry: MutterStreamRegistry,
}

impl ScreenCast {
    pub(crate) fn new(
        outputs: Arc<Mutex<Vec<OutputInfo>>>,
        windows: Arc<Mutex<Vec<WindowState>>>,
        comms: Comms,
        connection: Connection,
    ) -> (Self, MutterStreamRegistry) {
        let registry = MutterStreamRegistry::new(connection.clone());
        (
            Self {
                outputs,
                windows,
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

#[derive(Debug, Default, DeserializeDict, Type, Clone, Copy, PartialEq, Eq)]
#[zvariant(signature = "dict")]
struct RecordWindowProperties {
    #[zvariant(rename = "window-id")]
    window_id: Option<u64>,
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
    windows: Arc<Mutex<Vec<WindowState>>>,
    comms: Comms,
    connection: Connection,
    registry: MutterStreamRegistry,
    streams: Arc<Mutex<Vec<(Stream, OwnedObjectPath)>>>,
    stopped: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
enum StreamTarget {
    Monitor { connector: String },
    Window { window_id: u32 },
}

#[derive(Clone)]
struct Stream {
    id: u64,
    session_id: u64,
    target: StreamTarget,
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
            windows: Arc::clone(&self.windows),
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

        let (out_w, out_h) = fit_portal_size(output.width, output.height);

        let stream = Stream {
            id: stream_id,
            session_id: self.id,
            target: StreamTarget::Monitor {
                connector: connector.to_string(),
            },
            position: (output.x, output.y),
            size: (out_w, out_h),
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
        properties: RecordWindowProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        let window_id = match properties.window_id {
            Some(id) => {
                if id > u64::from(u32::MAX) {
                    return Err(fdo::Error::Failed(format!(
                        "window-id {id} is out of range"
                    )));
                }
                let id = id as u32;
                let windows = self.windows.lock().unwrap();
                if !windows.iter().any(|w| w.id == id) {
                    return Err(fdo::Error::Failed(format!("no such window: {id}")));
                }
                id
            }
            None => {
                let windows = self.windows.lock().unwrap();
                windows
                    .iter()
                    .find(|w| w.focused)
                    .map(|w| w.id)
                    .ok_or_else(|| fdo::Error::Failed("no focused window".into()))?
            }
        };

        let (x, y, width, height) = {
            let windows = self.windows.lock().unwrap();
            let window = windows
                .iter()
                .find(|w| w.id == window_id)
                .ok_or_else(|| fdo::Error::Failed(format!("no such window: {window_id}")))?;
            (window.x, window.y, window.width, window.height)
        };
        if width <= 0 || height <= 0 {
            return Err(fdo::Error::Failed("window has invalid size".into()));
        }

        debug!("Mutter RecordWindow window-id={window_id}");

        let stream_id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{stream_id}");
        let path = OwnedObjectPath::try_from(path)
            .map_err(|err| fdo::Error::Failed(format!("invalid stream path: {err}")))?;

        let (out_w, out_h) = fit_portal_size(width, height);

        let stream = Stream {
            id: stream_id,
            session_id: self.id,
            target: StreamTarget::Window { window_id },
            position: (x, y),
            size: (out_w, out_h),
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
        let output_name = match &self.target {
            StreamTarget::Monitor { connector } => connector.clone(),
            StreamTarget::Window { window_id } => format!("window-{window_id}"),
        };
        StreamParameters {
            position: self.position,
            size: self.size,
            output_name,
        }
    }
}

impl Stream {
    fn start(&self) {
        if self.was_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let target = match &self.target {
            StreamTarget::Monitor { connector } => MutterScreenCastTarget::Monitor {
                connector: connector.clone(),
            },
            StreamTarget::Window { window_id } => MutterScreenCastTarget::Window {
                window_id: *window_id,
            },
        };
        self.comms.main(MainMessage::StartMutterScreenCast {
            mutter_stream_id: self.id,
            session_id: self.session_id,
            target,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_window_properties_parse_window_id() {
        let props = RecordWindowProperties {
            window_id: Some(42),
            _cursor_mode: None,
            _is_recording: Some(true),
        };
        assert_eq!(props.window_id, Some(42));
    }
}
