//! Screencast session start/stop/complete orchestration across PW, renderer, and display.
//!
//! D-Bus emits and present dirty / push hooks stay in [`crate::app`]; this module
//! sequences peers and returns [`ScreencastLifecycleEffects`].

use std::collections::HashMap;

use lumalla_display::DisplayState;
use lumalla_renderer::{RendererState, ScreencastDmaExport};
use lumalla_screencast::{
    DmaBufferExport, FormatOffer, ScreencastManager, ScreencastSource, fit_output_size,
};
use lumalla_shared::{DbusMessage, MutterScreenCastTarget, ScreencastCursorMode};

/// D-Bus / portal reply waiting on an in-flight PipeWire stream start.
#[derive(Debug, Clone)]
pub enum PendingScreencastReply {
    Pipewire { request_id: usize },
    Mutter {
        mutter_stream_id: u64,
        session_id: u64,
    },
}

/// Long-lived maps correlating local stream ids with D-Bus / Mutter sessions.
#[derive(Debug, Default)]
pub struct ScreencastSessionState {
    pub pending_replies: HashMap<u32, PendingScreencastReply>,
    pub mutter_cast_streams: HashMap<u64, Vec<u32>>,
}

/// Event-loop / dbus work the app must perform after a lifecycle call.
#[derive(Debug, Default)]
pub struct ScreencastLifecycleEffects {
    pub mark_present_dirty: bool,
    pub push_frames_now: bool,
    pub dbus: Vec<DbusMessage>,
}

impl ScreencastLifecycleEffects {
    fn dbus(msg: DbusMessage) -> Self {
        Self {
            dbus: vec![msg],
            ..Self::default()
        }
    }
}

/// Peers required to start/stop screencast streams.
pub struct ScreencastSessionPeers<'a> {
    pub state: &'a mut ScreencastSessionState,
    pub screencast: &'a mut ScreencastManager,
    pub render: &'a mut RendererState,
    pub display: &'a DisplayState,
}

impl ScreencastSessionPeers<'_> {
    pub fn start_pipewire_region(
        &mut self,
        request_id: usize,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        name: String,
        max_fps: u32,
    ) -> ScreencastLifecycleEffects {
        let stream_id = self.screencast.peek_next_stream_id();
        match self.start_region_stream(
            stream_id,
            x,
            y,
            width,
            height,
            name,
            max_fps,
            FormatOffer::PreferMemFd,
            ScreencastCursorMode::Embedded,
        ) {
            Ok(stream_id) => {
                self.state.pending_replies.insert(
                    stream_id,
                    PendingScreencastReply::Pipewire { request_id },
                );
                ScreencastLifecycleEffects {
                    mark_present_dirty: true,
                    ..ScreencastLifecycleEffects::default()
                }
            }
            Err(err) => ScreencastLifecycleEffects::dbus(DbusMessage::PipewireStreamStarted {
                request_id,
                result: Err(err),
            }),
        }
    }

    pub fn start_pipewire_window(
        &mut self,
        request_id: usize,
        window_id: u32,
        name: String,
        max_fps: u32,
    ) -> ScreencastLifecycleEffects {
        match self.start_window_stream(
            window_id,
            name,
            max_fps,
            FormatOffer::PreferMemFd,
            ScreencastCursorMode::Embedded,
        ) {
            Ok(stream_id) => {
                self.state.pending_replies.insert(
                    stream_id,
                    PendingScreencastReply::Pipewire { request_id },
                );
                ScreencastLifecycleEffects {
                    mark_present_dirty: true,
                    ..ScreencastLifecycleEffects::default()
                }
            }
            Err(err) => ScreencastLifecycleEffects::dbus(DbusMessage::PipewireStreamStarted {
                request_id,
                result: Err(err),
            }),
        }
    }

    pub fn stop_pipewire(&mut self, stream_id: u32) -> ScreencastLifecycleEffects {
        let mut effects = ScreencastLifecycleEffects::default();
        if let Some(PendingScreencastReply::Pipewire { request_id }) =
            self.state.pending_replies.remove(&stream_id)
        {
            effects.dbus.push(DbusMessage::PipewireStreamStarted {
                request_id,
                result: Err(String::from("stream start was cancelled")),
            });
        }
        self.stop_stream(stream_id);
        effects
    }

    pub fn start_mutter(
        &mut self,
        mutter_stream_id: u64,
        session_id: u64,
        target: MutterScreenCastTarget,
        cursor_mode: ScreencastCursorMode,
    ) -> ScreencastLifecycleEffects {
        let start_result = (|| -> Result<u32, String> {
            match target {
                MutterScreenCastTarget::Monitor { connector } => {
                    let output = self
                        .display
                        .outputs()
                        .find(|o| o.name == connector)
                        .ok_or_else(|| format!("no such monitor: {connector}"))?;
                    let (x, y) = (output.x, output.y);
                    let (width, height) = (output.width, output.height);
                    if width <= 0 || height <= 0 {
                        return Err(format!("monitor '{connector}' has invalid size"));
                    }
                    let max_fps = if output.refresh_mhz > 0 {
                        ((output.refresh_mhz + 999) / 1000).max(1) as u32
                    } else {
                        60
                    };
                    let name = format!("Lumalla ScreenCast ({connector})");
                    let stream_id = self.screencast.peek_next_stream_id();
                    self.start_region_stream(
                        stream_id,
                        x,
                        y,
                        width,
                        height,
                        name,
                        max_fps,
                        FormatOffer::DmaOnly,
                        cursor_mode,
                    )
                }
                MutterScreenCastTarget::Window { window_id } => {
                    let name = format!("Lumalla ScreenCast (window {window_id})");
                    self.start_window_stream(
                        window_id,
                        name,
                        30,
                        FormatOffer::DmaOnly,
                        cursor_mode,
                    )
                }
            }
        })();
        match start_result {
            Ok(stream_id) => {
                self.state.pending_replies.insert(
                    stream_id,
                    PendingScreencastReply::Mutter {
                        mutter_stream_id,
                        session_id,
                    },
                );
                self.state
                    .mutter_cast_streams
                    .entry(session_id)
                    .or_default()
                    .push(stream_id);
                ScreencastLifecycleEffects {
                    mark_present_dirty: true,
                    ..ScreencastLifecycleEffects::default()
                }
            }
            Err(err) => ScreencastLifecycleEffects::dbus(DbusMessage::MutterScreenCastStarted {
                mutter_stream_id,
                result: Err(err),
            }),
        }
    }

    pub fn stop_mutter(&mut self, session_id: u64) -> ScreencastLifecycleEffects {
        let mut effects = ScreencastLifecycleEffects::default();
        let Some(stream_ids) = self.state.mutter_cast_streams.remove(&session_id) else {
            return effects;
        };
        for stream_id in stream_ids {
            if let Some(PendingScreencastReply::Mutter {
                mutter_stream_id, ..
            }) = self.state.pending_replies.remove(&stream_id)
            {
                effects.dbus.push(DbusMessage::MutterScreenCastStarted {
                    mutter_stream_id,
                    result: Err(String::from("stream start was cancelled")),
                });
            }
            self.stop_stream(stream_id);
        }
        effects
    }

    pub fn on_stream_ready(
        &mut self,
        stream_id: u32,
        result: Result<u32, String>,
    ) -> ScreencastLifecycleEffects {
        let reply = self.state.pending_replies.remove(&stream_id);
        let completed = self.screencast.complete_start(stream_id, result);
        let mut effects = ScreencastLifecycleEffects::default();
        if completed.is_err() {
            self.render.free_screencast_buffers(stream_id);
            if let Some(PendingScreencastReply::Mutter { session_id, .. }) = &reply {
                if let Some(ids) = self.state.mutter_cast_streams.get_mut(session_id) {
                    ids.retain(|id| *id != stream_id);
                    if ids.is_empty() {
                        self.state.mutter_cast_streams.remove(session_id);
                    }
                }
            }
        } else {
            effects.mark_present_dirty = true;
            effects.push_frames_now = true;
        }
        match reply {
            Some(PendingScreencastReply::Pipewire { request_id }) => {
                let result = completed.map(|node_id| (stream_id, node_id));
                effects
                    .dbus
                    .push(DbusMessage::PipewireStreamStarted { request_id, result });
            }
            Some(PendingScreencastReply::Mutter {
                mutter_stream_id, ..
            }) => {
                effects.dbus.push(DbusMessage::MutterScreenCastStarted {
                    mutter_stream_id,
                    result: completed,
                });
            }
            None => {
                if let Ok(_node_id) = completed {
                    // Orphan success (reply already cancelled): drop the stream.
                    self.stop_stream(stream_id);
                }
            }
        }
        effects
    }

    pub fn shutdown(&mut self) -> ScreencastLifecycleEffects {
        let mut effects = ScreencastLifecycleEffects::default();
        let mut ids: Vec<u32> = self.screencast.streams().keys().copied().collect();
        ids.extend(self.state.pending_replies.keys().copied());
        ids.sort_unstable();
        ids.dedup();
        for (_stream_id, reply) in self.state.pending_replies.drain() {
            match reply {
                PendingScreencastReply::Pipewire { request_id } => {
                    effects.dbus.push(DbusMessage::PipewireStreamStarted {
                        request_id,
                        result: Err(String::from("compositor shutting down")),
                    });
                }
                PendingScreencastReply::Mutter {
                    mutter_stream_id, ..
                } => {
                    effects.dbus.push(DbusMessage::MutterScreenCastStarted {
                        mutter_stream_id,
                        result: Err(String::from("compositor shutting down")),
                    });
                }
            }
        }
        self.state.mutter_cast_streams.clear();
        self.screencast.shutdown();
        for id in ids {
            self.render.free_screencast_buffers(id);
        }
        effects
    }

    /// Stop a stream and free GPU buffers (used by capture when a window disappears).
    pub fn stop_stream(&mut self, stream_id: u32) {
        self.screencast.stop_stream(stream_id);
        self.render.free_screencast_buffers(stream_id);
    }

    fn start_region_stream(
        &mut self,
        stream_id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        name: String,
        max_fps: u32,
        format_offer: FormatOffer,
        cursor_mode: ScreencastCursorMode,
    ) -> Result<u32, String> {
        let (out_w, out_h) = fit_output_size(width as u32, height as u32);
        let exports = self
            .render
            .alloc_screencast_buffers(
                stream_id,
                out_w,
                out_h,
                ScreencastManager::dma_buffer_count(),
            )
            .map_err(|err| format!("{err:#}"))?;
        let dma_exports = map_dma_exports(exports);
        self.screencast
            .start_stream(
                ScreencastSource::Region {
                    x,
                    y,
                    width,
                    height,
                },
                x,
                y,
                width,
                height,
                name,
                max_fps,
                dma_exports,
                format_offer,
                cursor_mode,
            )
            .map_err(|err| {
                self.render.free_screencast_buffers(stream_id);
                format!("{err:#}")
            })
    }

    fn start_window_stream(
        &mut self,
        window_id: u32,
        name: String,
        max_fps: u32,
        format_offer: FormatOffer,
        cursor_mode: ScreencastCursorMode,
    ) -> Result<u32, String> {
        let id = if window_id == 0 { None } else { Some(window_id) };
        let (resolved_id, x, y, width, height, _layers) =
            self.display.window_capture_layers(id).ok_or_else(|| {
                if window_id == 0 {
                    String::from("no focused window to capture")
                } else {
                    format!("no such window: {window_id}")
                }
            })?;
        let stream_id = self.screencast.peek_next_stream_id();
        let (out_w, out_h) = fit_output_size(width as u32, height as u32);
        let exports = self
            .render
            .alloc_screencast_buffers(
                stream_id,
                out_w,
                out_h,
                ScreencastManager::dma_buffer_count(),
            )
            .map_err(|err| format!("{err:#}"))?;
        let dma_exports = map_dma_exports(exports);
        self.screencast
            .start_stream(
                ScreencastSource::Window {
                    window_id: resolved_id,
                },
                x,
                y,
                width,
                height,
                name,
                max_fps,
                dma_exports,
                format_offer,
                cursor_mode,
            )
            .map_err(|err| {
                self.render.free_screencast_buffers(stream_id);
                format!("{err:#}")
            })
    }
}

fn map_dma_exports(exports: Vec<ScreencastDmaExport>) -> Vec<DmaBufferExport> {
    exports
        .into_iter()
        .map(|export| DmaBufferExport {
            index: export.index,
            fd: export.fd,
            width: export.width,
            height: export.height,
            stride: export.stride,
            offset: export.offset,
            size: export.size,
            modifier: export.modifier,
        })
        .collect()
}

/// Cancel a pending PipeWire reply when a window stream dies mid-capture.
///
/// Matches prior app behavior: only emits for [`PendingScreencastReply::Pipewire`].
pub fn take_window_gone_reply(
    state: &mut ScreencastSessionState,
    stream_id: u32,
) -> Option<DbusMessage> {
    if let Some(PendingScreencastReply::Pipewire { request_id }) =
        state.pending_replies.remove(&stream_id)
    {
        Some(DbusMessage::PipewireStreamStarted {
            request_id,
            result: Err(String::from("window was destroyed")),
        })
    } else {
        None
    }
}
