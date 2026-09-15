//! PipeWire video source streams fed by compositor CPU captures.

#![warn(missing_docs)]

use std::{
    cell::RefCell,
    collections::HashMap,
    io::Cursor,
    rc::Rc,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow};
use log::{debug, error, info, warn};
use once_cell::sync::OnceCell;
use pipewire::{
    self as pw,
    context::ContextRc,
    core::CoreRc,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        self,
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{self, Pod, serialize::PodSerializer},
        utils::{Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{StreamFlags, StreamListener, StreamRc, StreamState},
};

static PW_INIT: OnceCell<()> = OnceCell::new();

/// RGBA8 frame pushed from the compositor main thread.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major RGBA8 pixels (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
}

#[derive(Debug)]
struct PendingStart {
    result: Mutex<Option<Result<u32, String>>>,
    done: Condvar,
}

enum PwCommand {
    Create {
        stream_id: u32,
        width: u32,
        height: u32,
        name: String,
        pending: Arc<PendingStart>,
    },
    PushFrame {
        stream_id: u32,
        frame: VideoFrame,
    },
    Destroy {
        stream_id: u32,
    },
    Shutdown,
}

struct StreamInner {
    latest: Option<VideoFrame>,
    width: u32,
    height: u32,
    format: VideoInfoRaw,
    pending_start: Option<Arc<PendingStart>>,
    started: bool,
}

struct StreamSlot {
    stream: StreamRc,
    _listener: StreamListener<Rc<RefCell<StreamInner>>>,
    inner: Rc<RefCell<StreamInner>>,
}

/// Region stream tracked on the compositor main thread.
#[derive(Debug)]
pub struct ActiveStream {
    /// Stable id returned to config clients.
    pub id: u32,
    /// PipeWire node id for consumers.
    pub node_id: u32,
    /// Capture region left edge in compositor space.
    pub x: i32,
    /// Capture region top edge in compositor space.
    pub y: i32,
    /// Capture region width in compositor space.
    pub width: i32,
    /// Capture region height in compositor space.
    pub height: i32,
    /// Maximum capture rate.
    pub max_fps: u32,
    /// Last successful capture time.
    pub last_capture: Option<Instant>,
}

impl ActiveStream {
    /// Whether a new frame should be captured at `now`.
    pub fn due_at(&self, now: Instant) -> bool {
        let min_interval = Duration::from_secs_f64(1.0 / f64::from(self.max_fps.max(1)));
        match self.last_capture {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= min_interval,
        }
    }
}

/// Owns the PipeWire thread and the set of active region streams.
pub struct ScreencastManager {
    next_id: u32,
    streams: HashMap<u32, ActiveStream>,
    cmd_tx: Option<pw::channel::Sender<PwCommand>>,
    thread: Option<JoinHandle<()>>,
}

impl Default for ScreencastManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreencastManager {
    /// Create a manager with no streams (PipeWire thread starts on first use).
    pub fn new() -> Self {
        Self {
            next_id: 1,
            streams: HashMap::new(),
            cmd_tx: None,
            thread: None,
        }
    }

    /// Whether any streams are currently active.
    pub fn has_streams(&self) -> bool {
        !self.streams.is_empty()
    }

    /// Borrow active streams.
    pub fn streams(&self) -> &HashMap<u32, ActiveStream> {
        &self.streams
    }

    /// Mutable borrow of active streams (for updating `last_capture`).
    pub fn streams_mut(&mut self) -> &mut HashMap<u32, ActiveStream> {
        &mut self.streams
    }

    fn ensure_thread(&mut self) -> anyhow::Result<&pw::channel::Sender<PwCommand>> {
        if self.cmd_tx.is_some() {
            return Ok(self.cmd_tx.as_ref().unwrap());
        }

        PW_INIT.get_or_init(|| {
            pw::init();
        });

        let (cmd_tx, cmd_rx) = pw::channel::channel::<PwCommand>();
        let thread = thread::Builder::new()
            .name(String::from("pipewire"))
            .spawn(move || {
                if let Err(err) = run_pipewire_thread(cmd_rx) {
                    error!("PipeWire thread exited with error: {err:#}");
                }
            })
            .context("failed to spawn PipeWire thread")?;

        self.cmd_tx = Some(cmd_tx);
        self.thread = Some(thread);
        Ok(self.cmd_tx.as_ref().unwrap())
    }

    /// Create a PipeWire output stream for a fixed-size region.
    ///
    /// Blocks until the node id is known (or an error occurs).
    pub fn start_stream(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        name: String,
        max_fps: u32,
    ) -> anyhow::Result<(u32, u32)> {
        anyhow::ensure!(width > 0 && height > 0, "stream region must be positive");
        let stream_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);

        let pending = Arc::new(PendingStart {
            result: Mutex::new(None),
            done: Condvar::new(),
        });

        let cmd_tx = self.ensure_thread()?;
        cmd_tx
            .send(PwCommand::Create {
                stream_id,
                width: width as u32,
                height: height as u32,
                name,
                pending: Arc::clone(&pending),
            })
            .map_err(|_| anyhow!("PipeWire thread is not accepting commands"))?;

        let mut guard = pending.result.lock().unwrap();
        while guard.is_none() {
            guard = pending.done.wait(guard).unwrap();
        }
        let node_id = guard.take().unwrap().map_err(|err| anyhow!(err))?;

        self.streams.insert(
            stream_id,
            ActiveStream {
                id: stream_id,
                node_id,
                x,
                y,
                width,
                height,
                max_fps: max_fps.max(1),
                last_capture: None,
            },
        );

        info!("Started PipeWire stream id={stream_id} node_id={node_id} {width}x{height}");
        Ok((stream_id, node_id))
    }

    /// Push a captured frame to an active stream.
    pub fn push_frame(&self, stream_id: u32, frame: VideoFrame) -> anyhow::Result<()> {
        let Some(cmd_tx) = self.cmd_tx.as_ref() else {
            return Ok(());
        };
        cmd_tx
            .send(PwCommand::PushFrame { stream_id, frame })
            .map_err(|_| anyhow!("PipeWire thread is not accepting commands"))?;
        Ok(())
    }

    /// Stop a stream. No-op if the id is unknown.
    pub fn stop_stream(&mut self, stream_id: u32) {
        if self.streams.remove(&stream_id).is_none() {
            return;
        }
        if let Some(cmd_tx) = self.cmd_tx.as_ref() {
            let _ = cmd_tx.send(PwCommand::Destroy { stream_id });
        }
        info!("Stopped PipeWire stream id={stream_id}");
    }

    /// Tear down all streams and stop the PipeWire thread.
    pub fn shutdown(&mut self) {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for id in ids {
            self.stop_stream(id);
        }
        if let Some(cmd_tx) = self.cmd_tx.take() {
            let _ = cmd_tx.send(PwCommand::Shutdown);
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ScreencastManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn complete_pending(pending: &PendingStart, result: Result<u32, String>) {
    {
        let mut guard = pending.result.lock().unwrap();
        if guard.is_none() {
            *guard = Some(result);
        }
    }
    pending.done.notify_one();
}

fn run_pipewire_thread(cmd_rx: pw::channel::Receiver<PwCommand>) -> anyhow::Result<()> {
    let mainloop = MainLoopRc::new(None).context("failed to create PipeWire MainLoop")?;
    let context = ContextRc::new(&mainloop, None).context("failed to create PipeWire Context")?;
    let core = context
        .connect_rc(None)
        .context("failed to connect PipeWire Core")?;

    let streams: Rc<RefCell<HashMap<u32, StreamSlot>>> = Rc::new(RefCell::new(HashMap::new()));
    let mainloop_quit = mainloop.clone();
    let streams_for_cmds = Rc::clone(&streams);
    let core_for_cmds = core.clone();

    let _attached = cmd_rx.attach(mainloop.loop_(), move |command| {
        match command {
            PwCommand::Create {
                stream_id,
                width,
                height,
                name,
                pending,
            } => match create_stream(
                &core_for_cmds,
                stream_id,
                width,
                height,
                &name,
                Arc::clone(&pending),
            ) {
                Ok(slot) => {
                    streams_for_cmds.borrow_mut().insert(stream_id, slot);
                }
                Err(err) => {
                    warn!("Failed to create PipeWire stream {stream_id}: {err:#}");
                    complete_pending(&pending, Err(format!("{err:#}")));
                }
            },
            PwCommand::PushFrame { stream_id, frame } => {
                let streams = streams_for_cmds.borrow();
                let Some(slot) = streams.get(&stream_id) else {
                    return;
                };
                {
                    let mut inner = slot.inner.borrow_mut();
                    inner.latest = Some(frame);
                }
                if let Err(err) = slot.stream.trigger_process() {
                    debug!("trigger_process failed for stream {stream_id}: {err}");
                }
            }
            PwCommand::Destroy { stream_id } => {
                streams_for_cmds.borrow_mut().remove(&stream_id);
            }
            PwCommand::Shutdown => {
                streams_for_cmds.borrow_mut().clear();
                mainloop_quit.quit();
            }
        }
    });

    let _core = core;
    mainloop.run();
    Ok(())
}

fn create_stream(
    core: &CoreRc,
    stream_id: u32,
    width: u32,
    height: u32,
    name: &str,
    pending: Arc<PendingStart>,
) -> anyhow::Result<StreamSlot> {
    let stream = StreamRc::new(
        core.clone(),
        name,
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::NODE_NAME => name,
            *pw::keys::NODE_DESCRIPTION => "Lumalla screen capture",
        },
    )
    .context("failed to create PipeWire stream")?;

    let inner = Rc::new(RefCell::new(StreamInner {
        latest: None,
        width,
        height,
        format: VideoInfoRaw::default(),
        pending_start: Some(Arc::clone(&pending)),
        started: false,
    }));

    let listener = stream
        .add_local_listener_with_user_data(Rc::clone(&inner))
        .state_changed({
            let stream = stream.clone();
            move |_stream, user_data, old, new| {
                debug!("PipeWire stream {stream_id} state {old:?} -> {new:?}");
                // Node id is available once the stream is paused or streaming; do not
                // wait for Streaming (that often requires a consumer to link).
                if matches!(new, StreamState::Paused | StreamState::Streaming) {
                    let mut inner = user_data.borrow_mut();
                    if !inner.started {
                        let node_id = stream.node_id();
                        if node_id != 0 {
                            inner.started = true;
                            if let Some(pending) = inner.pending_start.take() {
                                complete_pending(&pending, Ok(node_id));
                            }
                        }
                    }
                } else if matches!(new, StreamState::Error(_)) {
                    let mut inner = user_data.borrow_mut();
                    if let Some(pending) = inner.pending_start.take() {
                        complete_pending(
                            &pending,
                            Err(format!("PipeWire stream entered error state (was {old:?})")),
                        );
                    }
                }
            }
        })
        .param_changed(move |stream, user_data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != ParamType::Format.as_raw() {
                return;
            }

            let (media_type, media_subtype) = match spa::param::format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };
            if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                return;
            }

            let mut inner = user_data.borrow_mut();
            if let Err(err) = inner.format.parse(param) {
                warn!("Failed to parse PipeWire video format: {err}");
                return;
            }

            let width = inner.format.size().width.max(1);
            let height = inner.format.size().height.max(1);
            inner.width = width;
            inner.height = height;
            let stride = width.saturating_mul(4);
            let size = stride.saturating_mul(height);
            let memfd = 1 << spa::buffer::DataType::MemFd.as_raw();

            let buffers = pod::object!(
                SpaTypes::ObjectParamBuffers,
                ParamType::Buffers,
                pod::Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_buffers,
                    pod::Value::Choice(pod::ChoiceValue::Int(spa::utils::Choice(
                        spa::utils::ChoiceFlags::empty(),
                        spa::utils::ChoiceEnum::Range {
                            default: 4,
                            min: 2,
                            max: 8,
                        },
                    ))),
                ),
                pod::Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_blocks,
                    pod::Value::Int(1),
                ),
                pod::Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_size,
                    pod::Value::Int(size as i32),
                ),
                pod::Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_stride,
                    pod::Value::Int(stride as i32),
                ),
                pod::Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_dataType,
                    pod::Value::Choice(pod::ChoiceValue::Int(spa::utils::Choice(
                        spa::utils::ChoiceFlags::empty(),
                        spa::utils::ChoiceEnum::Flags {
                            default: memfd,
                            flags: vec![memfd],
                        },
                    ))),
                ),
            );

            let values: Vec<u8> = PodSerializer::serialize(
                Cursor::new(Vec::new()),
                &pod::Value::Object(buffers),
            )
            .expect("serialize buffer params")
            .0
            .into_inner();
            let mut params = [Pod::from_bytes(&values).expect("buffer params pod")];
            if let Err(err) = stream.update_params(&mut params) {
                warn!("Failed to update PipeWire buffer params: {err}");
            }
        })
        .process(move |stream, user_data| {
            let inner = user_data.borrow_mut();
            let Some(frame) = inner.latest.as_ref() else {
                return;
            };

            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }

            let data = &mut datas[0];
            let Some(slice) = data.data() else {
                return;
            };

            let need = (frame.width as usize)
                .saturating_mul(frame.height as usize)
                .saturating_mul(4);
            let copy_len = need.min(slice.len()).min(frame.rgba.len());
            slice[..copy_len].copy_from_slice(&frame.rgba[..copy_len]);

            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = (frame.width.saturating_mul(4)) as i32;
            *chunk.size_mut() = copy_len as u32;
        })
        .register()
        .context("failed to register PipeWire stream listener")?;

    let enum_format = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::RGBA),
        pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle { width, height }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 0, denom: 1 }
        ),
        pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: 60,
                denom: 1
            }
        ),
    );

    let values: Vec<u8> =
        PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(enum_format))
            .context("serialize enum format")?
            .0
            .into_inner();
    let mut params = [Pod::from_bytes(&values).context("enum format pod")?];

    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("failed to connect PipeWire stream")?;

    Ok(StreamSlot {
        stream,
        _listener: listener,
        inner,
    })
}
