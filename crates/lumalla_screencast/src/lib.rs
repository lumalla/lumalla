//! PipeWire video source streams fed by compositor captures (DMA-BUF or MemFd).

#![warn(missing_docs)]

use std::{
    cell::RefCell,
    collections::HashMap,
    io::Cursor,
    os::fd::{AsRawFd, OwnedFd, RawFd},
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
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
        buffer::DataType,
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            video::{VideoFlags, VideoFormat, VideoInfoRaw},
        },
        pod::{self, Pod, Property, PropertyFlags, serialize::PodSerializer},
        sys::{
            SPA_DATA_FLAG_READWRITE, SPA_PARAM_BUFFERS_blocks, SPA_PARAM_BUFFERS_buffers,
            SPA_PARAM_BUFFERS_dataType, SPA_PARAM_BUFFERS_size, SPA_PARAM_BUFFERS_stride,
        },
        utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{StreamFlags, StreamListener, StreamRc, StreamState},
    sys::pw_stream_queue_buffer,
};

static PW_INIT: OnceCell<()> = OnceCell::new();

const DMA_BUFFER_COUNT: usize = 4;

/// RGBA8 frame pushed from the compositor main thread (MemFd path).
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major RGBA8 pixels (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
}

/// DMA-BUF export metadata for one PipeWire buffer slot.
#[derive(Debug)]
pub struct DmaBufferExport {
    /// Slot index.
    pub index: usize,
    /// DMA-BUF fd (moved into the PipeWire thread).
    pub fd: OwnedFd,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: u32,
    /// Byte offset.
    pub offset: u32,
    /// DRM modifier.
    pub modifier: u64,
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
        dma_exports: Vec<DmaBufferExport>,
        pending: Arc<PendingStart>,
        dma_negotiated: Arc<AtomicBool>,
    },
    PushMemFdFrame {
        stream_id: u32,
        frame: VideoFrame,
    },
    /// Capture buffer `index` has been GPU-filled and is ready to queue.
    QueueDma {
        stream_id: u32,
        index: usize,
    },
    Destroy {
        stream_id: u32,
    },
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DmaSlotState {
    /// With PipeWire / consumer; not safe to blit.
    WithPw,
    /// Dequeued; waiting for the main thread to blit.
    NeedsBlit,
    /// Blit done; waiting to be queued to PipeWire.
    Ready,
}

struct DmaSlot {
    fd: RawFd,
    stride: u32,
    offset: u32,
    width: u32,
    height: u32,
    modifier: u64,
    /// Keeps the fd alive for the stream lifetime.
    _owned: OwnedFd,
    pw_buffer: Option<std::ptr::NonNull<pw::sys::pw_buffer>>,
    state: DmaSlotState,
}

struct StreamInner {
    latest_memfd: Option<VideoFrame>,
    width: u32,
    height: u32,
    format: VideoInfoRaw,
    pending_start: Option<Arc<PendingStart>>,
    started: bool,
    use_dmabuf: bool,
    dma_negotiated: Arc<AtomicBool>,
    dma_slots: Vec<DmaSlot>,
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
    /// Set by the PipeWire thread when DMA-BUF format is negotiated.
    pub dma_negotiated: Arc<AtomicBool>,
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

    /// Whether the consumer negotiated DMA-BUF.
    pub fn uses_dmabuf(&self) -> bool {
        self.dma_negotiated.load(Ordering::Acquire)
    }
}

/// Owns the PipeWire thread and the set of active region streams.
pub struct ScreencastManager {
    next_id: u32,
    streams: HashMap<u32, ActiveStream>,
    cmd_tx: Option<pw::channel::Sender<PwCommand>>,
    thread: Option<JoinHandle<()>>,
    /// `(stream_id, buffer_index)` slots waiting for a GPU blit.
    pending_blits: Arc<Mutex<Vec<(u32, usize)>>>,
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
            pending_blits: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Recommended DMA-BUF pool size for [`Self::start_stream`].
    pub fn dma_buffer_count() -> usize {
        DMA_BUFFER_COUNT
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

    /// Drain buffer indices that need a GPU blit before they can be queued.
    pub fn take_pending_blits(&self) -> Vec<(u32, usize)> {
        std::mem::take(&mut *self.pending_blits.lock().unwrap())
    }

    fn ensure_thread(&mut self) -> anyhow::Result<&pw::channel::Sender<PwCommand>> {
        if self.cmd_tx.is_some() {
            return Ok(self.cmd_tx.as_ref().unwrap());
        }

        PW_INIT.get_or_init(|| {
            pw::init();
        });

        let pending_blits = Arc::clone(&self.pending_blits);
        let (cmd_tx, cmd_rx) = pw::channel::channel::<PwCommand>();
        let thread = thread::Builder::new()
            .name(String::from("pipewire"))
            .spawn(move || {
                if let Err(err) = run_pipewire_thread(cmd_rx, pending_blits) {
                    error!("PipeWire thread exited with error: {err:#}");
                }
            })
            .context("failed to spawn PipeWire thread")?;

        self.cmd_tx = Some(cmd_tx);
        self.thread = Some(thread);
        Ok(self.cmd_tx.as_ref().unwrap())
    }

    /// Stream id that the next [`Self::start_stream`] will assign.
    pub fn peek_next_stream_id(&self) -> u32 {
        self.next_id
    }

    /// Create a PipeWire output stream for a fixed-size region.
    ///
    /// `dma_exports` should contain [`Self::dma_buffer_count`] LINEAR DMA-BUF exports
    /// allocated by the renderer. Blocks until the node id is known.
    pub fn start_stream(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        name: String,
        max_fps: u32,
        dma_exports: Vec<DmaBufferExport>,
    ) -> anyhow::Result<(u32, u32)> {
        anyhow::ensure!(width > 0 && height > 0, "stream region must be positive");
        anyhow::ensure!(
            dma_exports.len() == DMA_BUFFER_COUNT,
            "expected {DMA_BUFFER_COUNT} DMA-BUF exports, got {}",
            dma_exports.len()
        );
        let stream_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);

        let pending = Arc::new(PendingStart {
            result: Mutex::new(None),
            done: Condvar::new(),
        });
        let dma_negotiated = Arc::new(AtomicBool::new(false));

        let cmd_tx = self.ensure_thread()?;
        cmd_tx
            .send(PwCommand::Create {
                stream_id,
                width: width as u32,
                height: height as u32,
                name,
                dma_exports,
                pending: Arc::clone(&pending),
                dma_negotiated: Arc::clone(&dma_negotiated),
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
                dma_negotiated,
            },
        );

        info!("Started PipeWire stream id={stream_id} node_id={node_id} {width}x{height}");
        Ok((stream_id, node_id))
    }

    /// Push a MemFd/CPU frame.
    pub fn push_memfd_frame(&self, stream_id: u32, frame: VideoFrame) -> anyhow::Result<()> {
        let Some(cmd_tx) = self.cmd_tx.as_ref() else {
            return Ok(());
        };
        cmd_tx
            .send(PwCommand::PushMemFdFrame { stream_id, frame })
            .map_err(|_| anyhow!("PipeWire thread is not accepting commands"))?;
        Ok(())
    }

    /// Notify PipeWire that DMA-BUF slot `index` has been filled by the GPU.
    pub fn queue_dma_buffer(&self, stream_id: u32, index: usize) -> anyhow::Result<()> {
        let Some(cmd_tx) = self.cmd_tx.as_ref() else {
            return Ok(());
        };
        cmd_tx
            .send(PwCommand::QueueDma { stream_id, index })
            .map_err(|_| anyhow!("PipeWire thread is not accepting commands"))?;
        Ok(())
    }

    /// Stop a stream. No-op if the id is unknown.
    pub fn stop_stream(&mut self, stream_id: u32) {
        if self.streams.remove(&stream_id).is_none() {
            return;
        }
        self.pending_blits
            .lock()
            .unwrap()
            .retain(|(id, _)| *id != stream_id);
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

fn run_pipewire_thread(
    cmd_rx: pw::channel::Receiver<PwCommand>,
    pending_blits: Arc<Mutex<Vec<(u32, usize)>>>,
) -> anyhow::Result<()> {
    let mainloop = MainLoopRc::new(None).context("failed to create PipeWire MainLoop")?;
    let context = ContextRc::new(&mainloop, None).context("failed to create PipeWire Context")?;
    let core = context
        .connect_rc(None)
        .context("failed to connect PipeWire Core")?;

    let streams: Rc<RefCell<HashMap<u32, StreamSlot>>> = Rc::new(RefCell::new(HashMap::new()));
    let mainloop_quit = mainloop.clone();
    let streams_for_cmds = Rc::clone(&streams);
    let core_for_cmds = core.clone();
    let pending_blits_cmds = Arc::clone(&pending_blits);

    let _attached = cmd_rx.attach(mainloop.loop_(), move |command| {
        match command {
            PwCommand::Create {
                stream_id,
                width,
                height,
                name,
                dma_exports,
                pending,
                dma_negotiated,
            } => match create_stream(
                &core_for_cmds,
                stream_id,
                width,
                height,
                &name,
                dma_exports,
                Arc::clone(&pending),
                dma_negotiated,
                Arc::clone(&pending_blits_cmds),
            ) {
                Ok(slot) => {
                    streams_for_cmds.borrow_mut().insert(stream_id, slot);
                }
                Err(err) => {
                    warn!("Failed to create PipeWire stream {stream_id}: {err:#}");
                    complete_pending(&pending, Err(format!("{err:#}")));
                }
            },
            PwCommand::PushMemFdFrame { stream_id, frame } => {
                let streams = streams_for_cmds.borrow();
                let Some(slot) = streams.get(&stream_id) else {
                    return;
                };
                {
                    let mut inner = slot.inner.borrow_mut();
                    if inner.use_dmabuf {
                        return;
                    }
                    inner.latest_memfd = Some(frame);
                }
                if let Err(err) = slot.stream.trigger_process() {
                    debug!("trigger_process failed for stream {stream_id}: {err}");
                }
            }
            PwCommand::QueueDma { stream_id, index } => {
                let streams = streams_for_cmds.borrow();
                let Some(slot) = streams.get(&stream_id) else {
                    return;
                };
                let mut inner = slot.inner.borrow_mut();
                let Some(dma) = inner.dma_slots.get_mut(index) else {
                    return;
                };
                if dma.state != DmaSlotState::NeedsBlit {
                    return;
                }
                let Some(pw_buffer) = dma.pw_buffer else {
                    return;
                };
                unsafe {
                    let spa_buffer = (*pw_buffer.as_ptr()).buffer;
                    if spa_buffer.is_null() || (*spa_buffer).n_datas == 0 {
                        return;
                    }
                    let spa_data = (*spa_buffer).datas;
                    let chunk = (*spa_data).chunk;
                    (*chunk).offset = dma.offset;
                    (*chunk).stride = dma.stride as i32;
                    (*chunk).size = dma.stride.saturating_mul(dma.height);
                }
                dma.state = DmaSlotState::WithPw;
                unsafe {
                    pw_stream_queue_buffer(slot.stream.as_raw_ptr(), pw_buffer.as_ptr());
                }
            }
            PwCommand::Destroy { stream_id } => {
                streams_for_cmds.borrow_mut().remove(&stream_id);
                pending_blits_cmds
                    .lock()
                    .unwrap()
                    .retain(|(id, _)| *id != stream_id);
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

#[allow(clippy::too_many_arguments)]
fn create_stream(
    core: &CoreRc,
    stream_id: u32,
    width: u32,
    height: u32,
    name: &str,
    dma_exports: Vec<DmaBufferExport>,
    pending: Arc<PendingStart>,
    dma_negotiated: Arc<AtomicBool>,
    pending_blits: Arc<Mutex<Vec<(u32, usize)>>>,
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

    let dma_slots: Vec<DmaSlot> = dma_exports
        .into_iter()
        .map(|export| DmaSlot {
            fd: export.fd.as_raw_fd(),
            stride: export.stride,
            offset: export.offset,
            width: export.width,
            height: export.height,
            modifier: export.modifier,
            _owned: export.fd,
            pw_buffer: None,
            state: DmaSlotState::WithPw,
        })
        .collect();

    let inner = Rc::new(RefCell::new(StreamInner {
        latest_memfd: None,
        width,
        height,
        format: VideoInfoRaw::default(),
        pending_start: Some(Arc::clone(&pending)),
        started: false,
        use_dmabuf: false,
        dma_negotiated: Arc::clone(&dma_negotiated),
        dma_slots,
    }));

    let listener = stream
        .add_local_listener_with_user_data(Rc::clone(&inner))
        .state_changed({
            let stream = stream.clone();
            move |_stream, user_data, old, new| {
                debug!("PipeWire stream {stream_id} state {old:?} -> {new:?}");
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
        .param_changed({
            let pending_blits = Arc::clone(&pending_blits);
            move |stream, user_data, id, param| {
                let Some(param) = param else {
                    return;
                };
                if id != ParamType::Format.as_raw() {
                    return;
                }

                let (media_type, media_subtype) =
                    match spa::param::format_utils::parse_format(param) {
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

                let use_dmabuf = inner.format.flags().contains(VideoFlags::MODIFIER);
                inner.use_dmabuf = use_dmabuf;
                inner.dma_negotiated.store(use_dmabuf, Ordering::Release);
                info!(
                    "PipeWire stream {stream_id} negotiated {} path",
                    if use_dmabuf { "DMA-BUF" } else { "MemFd" }
                );

                let datatype = if use_dmabuf {
                    1 << DataType::DmaBuf.as_raw()
                } else {
                    1 << DataType::MemFd.as_raw()
                };
                let stride = if use_dmabuf {
                    inner
                        .dma_slots
                        .first()
                        .map(|s| s.stride)
                        .unwrap_or(width.saturating_mul(4))
                } else {
                    width.saturating_mul(4)
                };
                let size = stride.saturating_mul(height);

                let buffers = pod::object!(
                    SpaTypes::ObjectParamBuffers,
                    ParamType::Buffers,
                    Property::new(
                        SPA_PARAM_BUFFERS_buffers,
                        pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Range {
                                default: DMA_BUFFER_COUNT as i32,
                                min: 2,
                                max: DMA_BUFFER_COUNT as i32,
                            },
                        ))),
                    ),
                    Property::new(SPA_PARAM_BUFFERS_blocks, pod::Value::Int(1)),
                    Property::new(SPA_PARAM_BUFFERS_size, pod::Value::Int(size as i32)),
                    Property::new(SPA_PARAM_BUFFERS_stride, pod::Value::Int(stride as i32)),
                    Property::new(
                        SPA_PARAM_BUFFERS_dataType,
                        pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Flags {
                                default: datatype as i32,
                                flags: vec![datatype as i32],
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

                // Kick DRIVER scheduling once format is known.
                let _ = pending_blits;
                if let Err(err) = stream.trigger_process() {
                    debug!("trigger_process after format: {err}");
                }
            }
        })
        .add_buffer({
            move |_stream, user_data, buffer| {
                let mut inner = user_data.borrow_mut();
                if !inner.use_dmabuf {
                    if let Err(err) = unsafe { attach_memfd(buffer, inner.width, inner.height) } {
                        warn!("Failed to attach MemFd buffer: {err:#}");
                    }
                    return;
                }
                unsafe {
                    let spa_buffer = (*buffer).buffer;
                    if spa_buffer.is_null() || (*spa_buffer).n_datas == 0 {
                        return;
                    }
                    let spa_data = (*spa_buffer).datas;
                    // Find a slot not yet bound to a pw_buffer.
                    let Some(slot) = inner
                        .dma_slots
                        .iter_mut()
                        .find(|slot| slot.pw_buffer.is_none())
                    else {
                        warn!("No free DMA slot for add_buffer");
                        return;
                    };
                    (*spa_data).type_ = DataType::DmaBuf.as_raw();
                    (*spa_data).flags = SPA_DATA_FLAG_READWRITE;
                    (*spa_data).fd = slot.fd as i64;
                    (*spa_data).mapoffset = slot.offset;
                    (*spa_data).maxsize = slot.stride.saturating_mul(slot.height);
                    (*spa_data).data = std::ptr::null_mut();
                    let chunk = (*spa_data).chunk;
                    (*chunk).offset = slot.offset;
                    (*chunk).stride = slot.stride as i32;
                    (*chunk).size = 0;
                    slot.pw_buffer = std::ptr::NonNull::new(buffer);
                    slot.state = DmaSlotState::WithPw;
                }
            }
        })
        .remove_buffer(move |_stream, user_data, buffer| {
            let mut inner = user_data.borrow_mut();
            for slot in &mut inner.dma_slots {
                if slot.pw_buffer.map(std::ptr::NonNull::as_ptr) == Some(buffer) {
                    slot.pw_buffer = None;
                    slot.state = DmaSlotState::WithPw;
                }
            }
        })
        .process({
            let pending_blits = Arc::clone(&pending_blits);
            move |stream, user_data| {
                let mut inner = user_data.borrow_mut();
                if inner.use_dmabuf {
                    // Dequeue returned buffers and mark them for GPU fill on the main thread.
                    loop {
                        let ptr = unsafe { stream.dequeue_raw_buffer() };
                        let Some(pw_buffer) = std::ptr::NonNull::new(ptr) else {
                            break;
                        };
                        let fd = unsafe {
                            let spa_buffer = (*pw_buffer.as_ptr()).buffer;
                            if spa_buffer.is_null() || (*spa_buffer).n_datas == 0 {
                                pw_stream_queue_buffer(stream.as_raw_ptr(), pw_buffer.as_ptr());
                                continue;
                            }
                            (*(*spa_buffer).datas).fd as RawFd
                        };
                        if let Some((index, slot)) = inner
                            .dma_slots
                            .iter_mut()
                            .enumerate()
                            .find(|(_, slot)| slot.fd == fd)
                        {
                            slot.pw_buffer = Some(pw_buffer);
                            slot.state = DmaSlotState::NeedsBlit;
                            pending_blits.lock().unwrap().push((stream_id, index));
                        } else {
                            unsafe {
                                pw_stream_queue_buffer(stream.as_raw_ptr(), pw_buffer.as_ptr());
                            }
                        }
                    }
                    return;
                }

                let Some(frame) = inner.latest_memfd.as_ref() else {
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
            }
        })
        .register()
        .context("failed to register PipeWire stream listener")?;

    // Prefer DMA-BUF (BGRx + LINEAR), fall back to MemFd RGBA.
    let mut params_bytes: Vec<Vec<u8>> = Vec::new();
    params_bytes.push(serialize_enum_format(
        width,
        height,
        VideoFormat::BGRx,
        Some(0), // DRM_FORMAT_MOD_LINEAR
    )?);
    params_bytes.push(serialize_enum_format(
        width,
        height,
        VideoFormat::RGBA,
        None,
    )?);

    let mut params: Vec<&Pod> = params_bytes
        .iter()
        .map(|bytes| Pod::from_bytes(bytes).expect("enum format pod"))
        .collect();

    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
            &mut params,
        )
        .context("failed to connect PipeWire stream")?;

    Ok(StreamSlot {
        stream,
        _listener: listener,
        inner,
    })
}

fn serialize_enum_format(
    width: u32,
    height: u32,
    format: VideoFormat,
    modifier: Option<u64>,
) -> anyhow::Result<Vec<u8>> {
    let mut properties = vec![
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, format),
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
    ];
    if let Some(modifier) = modifier {
        properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY,
            value: pod::Value::Long(modifier as i64),
        });
    }
    let obj = pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties,
    };
    Ok(
        PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(obj))
            .context("serialize enum format")?
            .0
            .into_inner(),
    )
}

unsafe fn attach_memfd(
    buffer: *mut pw::sys::pw_buffer,
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    let spa_buffer = unsafe { (*buffer).buffer };
    anyhow::ensure!(!spa_buffer.is_null(), "spa_buffer is null");
    anyhow::ensure!(unsafe { (*spa_buffer).n_datas } > 0, "no spa datas");
    let size = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    let stride = width.saturating_mul(4);

    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"lumalla-pw-memfd".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    anyhow::ensure!(fd >= 0, "memfd_create failed");
    let fd = fd as RawFd;
    if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(anyhow!("ftruncate memfd failed: {err}"));
    }
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if map == libc::MAP_FAILED {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(anyhow!("mmap memfd failed: {err}"));
    }

    unsafe {
        let spa_data = (*spa_buffer).datas;
        (*spa_data).type_ = DataType::MemFd.as_raw();
        (*spa_data).flags = SPA_DATA_FLAG_READWRITE;
        (*spa_data).fd = fd as i64;
        (*spa_data).mapoffset = 0;
        (*spa_data).maxsize = size as u32;
        (*spa_data).data = map;
        let chunk = (*spa_data).chunk;
        (*chunk).offset = 0;
        (*chunk).stride = stride as i32;
        (*chunk).size = 0;
    }
    Ok(())
}
