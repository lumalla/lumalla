//! Post-present fulfillment for `ext-image-copy-capture` sessions.

use log::warn;
use lumalla_display::{
    CaptureDamageRect, CaptureSourceKind, ClientId, ConnectedClients, DisplayState, ObjectId,
    PendingCapture,
};
use lumalla_renderer::RendererState;
use lumalla_shared::{CapturedImage, Output};

const FAILURE_UNKNOWN: u32 = 0;
const FAILURE_BUFFER_CONSTRAINTS: u32 = 1;
const FAILURE_STOPPED: u32 = 2;

const TEMP_CAPTURE_STREAM: u32 = 0xffff_fffe;

pub struct ImageCopyCapturePhase<'a> {
    pub display: &'a mut DisplayState,
    pub render: &'a mut RendererState,
    pub clients: &'a mut ConnectedClients,
}

impl ImageCopyCapturePhase<'_> {
    pub fn flush_stopped(&mut self) {
        let stopped = self
            .display
            .image_copy_capture
            .take_sessions_needing_stopped_event();
        for (client_id, session) in stopped {
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            client
                .writer_mut()
                .ext_image_copy_capture_session_v1_stopped(session);
        }
    }

    pub fn update_cursor_sessions(&mut self) {
        let sessions = self.display.image_copy_capture.all_cursor_sessions();
        let (px, py) = self.display.pointer_position();
        let (scene_x, scene_y) =
            lumalla_shared::map_dest_to_source(&self.display.pointer_views(), px, py);
        let pointer_x = scene_x.round() as i32;
        let pointer_y = scene_y.round() as i32;
        let (hotspot_x, hotspot_y) = self
            .display
            .active_cursor()
            .map(|c| (c.hotspot_x, c.hotspot_y))
            .unwrap_or((0, 0));

        for (client_id, cursor_session, kind) in sessions {
            let (entered, rel_x, rel_y) = match kind {
                CaptureSourceKind::Output { wl_output } => {
                    match self
                        .display
                        .output_rect_for_binding(client_id, wl_output)
                    {
                        Some((x, y, w, h)) => {
                            let entered = pointer_x >= x
                                && pointer_y >= y
                                && pointer_x < x.saturating_add(w)
                                && pointer_y < y.saturating_add(h);
                            (entered, pointer_x - x, pointer_y - y)
                        }
                        None => (false, 0, 0),
                    }
                }
                CaptureSourceKind::Toplevel { window_id } => {
                    match self.display.window_capture_layers(Some(window_id)) {
                        Some((_, x, y, w, h, _)) => {
                            let entered = pointer_x >= x
                                && pointer_y >= y
                                && pointer_x < x.saturating_add(w)
                                && pointer_y < y.saturating_add(h);
                            (entered, pointer_x - x, pointer_y - y)
                        }
                        None => (false, 0, 0),
                    }
                }
            };
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            self.display.image_copy_capture.set_cursor_entered(
                client_id,
                cursor_session,
                entered,
                client.writer_mut(),
            );
            if entered {
                self.display.image_copy_capture.emit_cursor_position(
                    client_id,
                    cursor_session,
                    rel_x,
                    rel_y,
                    hotspot_x,
                    hotspot_y,
                    client.writer_mut(),
                );
            }
        }
    }

    pub fn fulfill(&mut self) {
        self.flush_stopped();
        self.update_cursor_sessions();

        let pending = self.display.image_copy_capture.take_ready_pending();
        if pending.is_empty() {
            return;
        }

        let outputs: Vec<Output> = self.display.outputs().map(Output::from).collect();
        let (tv_sec, tv_usec) = monotonic_time_sec_usec();
        let tv_nsec = tv_usec.saturating_mul(1000);

        for capture in pending {
            if self
                .display
                .image_copy_capture
                .session_stopped(capture.client_id, capture.session)
            {
                self.fail(&capture, FAILURE_STOPPED);
                continue;
            }

            let result = if capture.cursor_only {
                self.render.capture_cursor_rgba().map_err(|err| {
                    warn!("cursor capture failed: {err:#}");
                    FAILURE_UNKNOWN
                })
            } else {
                self.capture_source_image(&capture, &outputs)
            };

            match result {
                Ok(image) => {
                    let xrgb = rgba_to_xrgb(&image.rgba);
                    if let Err(err) =
                        self.write_pixels(capture.client_id, capture.buffer, &xrgb, &image)
                    {
                        warn!("image-copy-capture write failed: {err:#}");
                        self.fail(&capture, FAILURE_BUFFER_CONSTRAINTS);
                        continue;
                    }
                    let damage = if capture.first_frame || capture.client_damage.is_empty() {
                        CaptureDamageRect {
                            x: 0,
                            y: 0,
                            width: image.width as i32,
                            height: image.height as i32,
                        }
                    } else {
                        capture.client_damage[0]
                    };
                    let Some(client) = self.clients.get_mut(&capture.client_id) else {
                        continue;
                    };
                    self.display.image_copy_capture.mark_frame_ready(
                        capture.client_id,
                        capture.frame,
                        damage,
                        tv_sec,
                        tv_nsec,
                        client.writer_mut(),
                    );
                }
                Err(reason) => self.fail(&capture, reason),
            }
        }
    }

    fn fail(&mut self, capture: &PendingCapture, reason: u32) {
        let Some(client) = self.clients.get_mut(&capture.client_id) else {
            return;
        };
        self.display.image_copy_capture.mark_frame_failed(
            capture.client_id,
            capture.frame,
            reason,
            client.writer_mut(),
        );
    }

    fn capture_source_image(
        &mut self,
        capture: &PendingCapture,
        outputs: &[Output],
    ) -> Result<CapturedImage, u32> {
        match capture.source {
            CaptureSourceKind::Output { wl_output } => {
                let Some((x, y, w, h)) = self
                    .display
                    .output_rect_for_binding(capture.client_id, wl_output)
                else {
                    return Err(FAILURE_STOPPED);
                };
                if capture.paint_cursors {
                    if let Err(err) = self.render.alloc_screencast_buffers(
                        TEMP_CAPTURE_STREAM,
                        w as u32,
                        h as u32,
                        1,
                    ) {
                        warn!("temp screencast alloc failed: {err:#}");
                        return self
                            .render
                            .capture_region(x, y, w, h, outputs)
                            .map_err(|err| {
                                warn!("capture_region failed: {err:#}");
                                FAILURE_UNKNOWN
                            });
                    }
                    let result = self.render.capture_region_for_screencast(
                        TEMP_CAPTURE_STREAM,
                        x,
                        y,
                        w,
                        h,
                        w as u32,
                        h as u32,
                        outputs,
                        true,
                    );
                    self.render.free_screencast_buffers(TEMP_CAPTURE_STREAM);
                    result.map_err(|err| {
                        warn!("capture_region_for_screencast failed: {err:#}");
                        FAILURE_UNKNOWN
                    })
                } else {
                    self.render
                        .capture_region(x, y, w, h, outputs)
                        .map_err(|err| {
                            warn!("capture_region failed: {err:#}");
                            FAILURE_UNKNOWN
                        })
                }
            }
            CaptureSourceKind::Toplevel { window_id } => {
                let Some((_, ox, oy, w, h, layers)) =
                    self.display.window_capture_layers(Some(window_id))
                else {
                    return Err(FAILURE_STOPPED);
                };
                let layer_keys: Vec<(u32, u32)> = layers
                    .iter()
                    .map(|s| (s.client_id.get(), s.surface_id.get()))
                    .collect();
                if let Err(err) = self.render.alloc_screencast_buffers(
                    TEMP_CAPTURE_STREAM,
                    w as u32,
                    h as u32,
                    1,
                ) {
                    warn!("temp screencast alloc failed: {err:#}");
                    return Err(FAILURE_UNKNOWN);
                }
                let result = self.render.capture_window_for_screencast(
                    TEMP_CAPTURE_STREAM,
                    &layer_keys,
                    ox,
                    oy,
                    w,
                    h,
                    w as u32,
                    h as u32,
                    capture.paint_cursors,
                );
                self.render.free_screencast_buffers(TEMP_CAPTURE_STREAM);
                result.map_err(|err| {
                    warn!("capture_window_for_screencast failed: {err:#}");
                    FAILURE_UNKNOWN
                })
            }
        }
    }

    fn write_pixels(
        &mut self,
        client_id: ClientId,
        buffer: ObjectId,
        xrgb: &[u8],
        image: &CapturedImage,
    ) -> anyhow::Result<()> {
        if self.display.shm_has_buffer(client_id, buffer) {
            let meta = self.display.shm_buffer_meta(client_id, buffer)?;
            if meta.0 != image.width as usize || meta.1 != image.height as usize {
                anyhow::bail!("SHM buffer size mismatch");
            }
            self.display
                .shm_write_buffer_pixels(client_id, buffer, xrgb)?;
            return Ok(());
        }
        if self.display.dmabuf_has_buffer(client_id, buffer) {
            self.display
                .dmabuf_write_linear_buffer_pixels(client_id, buffer, xrgb)?;
            return Ok(());
        }
        anyhow::bail!("buffer is neither SHM nor dmabuf")
    }
}

fn rgba_to_xrgb(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    out
}

fn monotonic_time_sec_usec() -> (u64, u32) {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64, (ts.tv_nsec / 1000) as u32)
}
