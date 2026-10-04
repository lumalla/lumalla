//! Present/blit-phase screencast capture: DMA fills, MemFd readback, window teardown.

use std::time::Instant;

use allocator_api2::vec::Vec as ArenaVec;
use log::warn;
use lumalla_display::DisplayState;
use lumalla_renderer::RendererState;
use lumalla_screencast::{
    ScreencastManager, ScreencastSource, VideoFrame, fit_memfd_output_size,
};
use lumalla_shared::DbusMessage;
use stumpalo::Arena;

use crate::screencast_lifecycle::{ScreencastSessionState, take_window_gone_reply};

/// Peers for one capture/push batch after present or blit wake.
pub struct ScreencastCapturePhase<'a> {
    pub session: &'a mut ScreencastSessionState,
    pub screencast: &'a mut ScreencastManager,
    pub render: &'a mut RendererState,
    pub display: &'a DisplayState,
}

/// Event-loop work after a capture batch.
#[derive(Debug, Default)]
pub struct CaptureEffects {
    pub arm_gpu_wake: bool,
    pub dbus: Vec<DbusMessage>,
}

impl ScreencastCapturePhase<'_> {
    /// Queue DMA-BUF slots whose GPU fills have finished (non-blocking fence poll).
    pub fn finish_ready_dma(&mut self) {
        let ready = match self.render.poll_screencast_gpu() {
            Ok(ready) => ready,
            Err(err) => {
                warn!("Unable to poll screencast GPU fences: {err:#}");
                return;
            }
        };
        for (stream_id, index) in ready {
            if let Err(err) = self.screencast.queue_dma_buffer(stream_id, index) {
                warn!("Unable to queue DMA-BUF for stream {stream_id}: {err:#}");
                self.render.release_screencast_buffer(stream_id, index);
            }
        }
    }

    /// Push due frames for all active streams. Caller arms GPU wake from [`CaptureEffects`].
    pub fn push_frames(&mut self, arena: &Arena) -> CaptureEffects {
        let mut effects = CaptureEffects::default();
        if !self.screencast.has_streams() && !self.render.has_pending_screencast_gpu() {
            return effects;
        }

        self.finish_ready_dma();

        if !self.screencast.has_streams() {
            effects.arm_gpu_wake = true;
            return effects;
        }

        let now = Instant::now();
        let mut outputs = ArenaVec::new_in(arena);
        outputs.extend(self.display.outputs().map(lumalla_shared::Output::from));

        // Refresh window geometry / tear down destroyed window streams.
        let mut stop_ids = Vec::new();
        let all_ids: Vec<u32> = self.screencast.streams().keys().copied().collect();
        for stream_id in all_ids {
            let Some(source) = self.screencast.stream_source(stream_id) else {
                continue;
            };
            let ScreencastSource::Window { window_id } = source else {
                continue;
            };
            match self.display.window_capture_layers(Some(window_id)) {
                Some((_, x, y, width, height, _)) => {
                    if self
                        .screencast
                        .update_capture_geometry(stream_id, x, y, width, height)
                    {
                        self.render.invalidate_screencast_content(stream_id);
                    }
                }
                None => stop_ids.push(stream_id),
            }
        }
        for stream_id in stop_ids {
            warn!("Stopping PipeWire window stream {stream_id}: window gone");
            if let Some(msg) = take_window_gone_reply(self.session, stream_id) {
                effects.dbus.push(msg);
            }
            self.screencast.stop_stream(stream_id);
            self.render.free_screencast_buffers(stream_id);
        }

        // DMA-BUF path: submit GPU fills without waiting; queue when fences signal.
        let pending_blits = self.screencast.take_pending_blits();
        let mut deferred = Vec::new();
        for (stream_id, index) in pending_blits {
            let Some((x, y, width, height, out_w, out_h, uses_dmabuf)) =
                self.screencast.stream_capture_region(stream_id)
            else {
                let _ = self.screencast.queue_dma_buffer(stream_id, index);
                continue;
            };
            if !uses_dmabuf {
                deferred.push((stream_id, index));
                continue;
            }
            if let Some(stream) = self.screencast.streams().get(&stream_id)
                && !stream.due_at(now)
            {
                deferred.push((stream_id, index));
                continue;
            }

            let embed_cursor = self
                .screencast
                .streams()
                .get(&stream_id)
                .is_some_and(|s| s.embed_cursor());

            let content_serial = self.render.screencast_content_serial();
            if self
                .render
                .screencast_slot_content_serial(stream_id, index)
                == Some(content_serial)
            {
                if let Err(err) = self.screencast.queue_dma_buffer(stream_id, index) {
                    warn!(
                        "Unable to re-queue unchanged DMA-BUF for stream {stream_id}: {err:#}"
                    );
                    self.render.release_screencast_buffer(stream_id, index);
                } else if let Some(stream) = self.screencast.streams_mut().get_mut(&stream_id) {
                    stream.last_capture = Some(now);
                }
                continue;
            }

            let blit_result = match self.screencast.stream_source(stream_id) {
                Some(ScreencastSource::Window { window_id }) => {
                    match self.display.window_capture_layers(Some(window_id)) {
                        Some((_, ox, oy, w, h, layers)) => {
                            let keys: Vec<(u32, u32)> = layers
                                .iter()
                                .map(|s| (s.client_id.get(), s.surface_id.get()))
                                .collect();
                            self.render.composite_window_to_screencast_buffer(
                                stream_id, index, &keys, ox, oy, w, h, out_w, out_h, embed_cursor,
                            )
                        }
                        None => Err(anyhow::anyhow!("window {window_id} gone")),
                    }
                }
                _ => self.render.blit_region_to_screencast_buffer(
                    stream_id, index, x, y, width, height, out_w, out_h, &outputs, embed_cursor,
                ),
            };

            match blit_result {
                Ok(()) => {
                    if let Some(stream) = self.screencast.streams_mut().get_mut(&stream_id) {
                        stream.last_capture = Some(now);
                    }
                }
                Err(err) => {
                    warn!("Unable to fill PipeWire DMA buffer for stream {stream_id}: {err:#}");
                    if let Err(queue_err) = self.screencast.queue_dma_buffer(stream_id, index) {
                        warn!(
                            "Unable to recycle DMA-BUF after blit failure for stream {stream_id}: {queue_err:#}"
                        );
                        self.render.release_screencast_buffer(stream_id, index);
                    }
                }
            }
        }
        if !deferred.is_empty() {
            self.screencast.requeue_pending_blits_silent(deferred);
        }

        self.finish_ready_dma();

        // MemFd path: GPU-scale into the small screencast buffer, then read that back.
        let due: Vec<(u32, ScreencastSource, i32, i32, i32, i32, bool)> = self
            .screencast
            .streams()
            .values()
            .filter(|stream| !stream.uses_dmabuf() && stream.due_at(now))
            .map(|stream| {
                (
                    stream.id,
                    stream.source,
                    stream.x,
                    stream.y,
                    stream.width,
                    stream.height,
                    stream.embed_cursor(),
                )
            })
            .collect();

        for (stream_id, source, x, y, width, height, embed_cursor) in due {
            let (memfd_w, memfd_h) = fit_memfd_output_size(width as u32, height as u32);
            let capture_result = match source {
                ScreencastSource::Window { window_id } => {
                    match self.display.window_capture_layers(Some(window_id)) {
                        Some((_, ox, oy, w, h, layers)) => {
                            let keys: Vec<(u32, u32)> = layers
                                .iter()
                                .map(|s| (s.client_id.get(), s.surface_id.get()))
                                .collect();
                            let (mw, mh) = fit_memfd_output_size(w as u32, h as u32);
                            self.render.capture_window_for_screencast(
                                stream_id, &keys, ox, oy, w, h, mw, mh, embed_cursor,
                            )
                        }
                        None => Err(anyhow::anyhow!("window {window_id} gone")),
                    }
                }
                ScreencastSource::Region { .. } => self.render.capture_region_for_screencast(
                    stream_id, x, y, width, height, memfd_w, memfd_h, &outputs, embed_cursor,
                ),
            };
            match capture_result {
                Ok(image) => {
                    let frame = VideoFrame {
                        width: image.width,
                        height: image.height,
                        rgba: image.rgba,
                    };
                    if let Err(err) = self.screencast.push_memfd_frame(stream_id, frame) {
                        warn!("Unable to push PipeWire frame for stream {stream_id}: {err:#}");
                    } else if let Some(stream) = self.screencast.streams_mut().get_mut(&stream_id)
                    {
                        stream.last_capture = Some(now);
                    }
                }
                Err(err) => {
                    warn!("Unable to capture PipeWire frame for stream {stream_id}: {err:#}");
                }
            }
        }

        effects.arm_gpu_wake = true;
        effects
    }
}
