//! Domain state for `ext-image-copy-capture` / `ext-image-capture-source`.

use std::collections::{HashMap, HashSet};

use lumalla_wayland_protocol::{
    ClientId, ObjectId,
    buffer::Writer,
    protocols::{
        image_copy_capture::*,
        wayland::{WL_OUTPUT_TRANSFORM_NORMAL, WL_SHM_FORMAT_ARGB8888, WL_SHM_FORMAT_XRGB8888},
    },
};

type ResourceKey = (ClientId, ObjectId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSourceKind {
    Output { wl_output: ObjectId },
    Toplevel { window_id: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureBufferSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureDamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug)]
struct SourceState {
    kind: CaptureSourceKind,
    sessions: HashSet<ObjectId>,
}

#[derive(Debug)]
struct SessionState {
    /// Captured at session creation; survives source object destruction.
    kind: CaptureSourceKind,
    paint_cursors: bool,
    frame: Option<ObjectId>,
    stopped: bool,
    stopped_needs_emit: bool,
    /// At least one frame completed successfully in this session.
    had_ready: bool,
    /// Content serial when the last ready was emitted; used for change-wait.
    last_content_serial: u64,
    /// Constraints size last advertised.
    size: CaptureBufferSize,
    /// True when constraints must be re-sent before next capture can succeed.
    constraints_dirty: bool,
    /// Optional parent cursor session (if this session was created via get_capture_session).
    cursor_session: Option<ObjectId>,
}

#[derive(Debug)]
struct FrameState {
    session: ObjectId,
    buffer: Option<ObjectId>,
    damage: Vec<CaptureDamageRect>,
    capture_requested: bool,
}

#[derive(Debug)]
struct CursorSessionState {
    source: ObjectId,
    kind: CaptureSourceKind,
    pointer: ObjectId,
    capture_session: Option<ObjectId>,
    entered: bool,
}

/// A capture that has been requested and waits for post-present fulfillment.
#[derive(Debug, Clone)]
pub struct PendingCapture {
    pub client_id: ClientId,
    pub session: ObjectId,
    pub frame: ObjectId,
    pub buffer: ObjectId,
    pub source: CaptureSourceKind,
    pub paint_cursors: bool,
    pub client_damage: Vec<CaptureDamageRect>,
    pub first_frame: bool,
    pub wait_content_serial: u64,
    /// Cursor-only session (pointer image), when set.
    pub cursor_only: bool,
    pub cursor_entered: bool,
}

#[derive(Debug, Default)]
pub struct ImageCopyCaptureManager {
    sources: HashMap<ResourceKey, SourceState>,
    sessions: HashMap<ResourceKey, SessionState>,
    frames: HashMap<ResourceKey, FrameState>,
    cursor_sessions: HashMap<ResourceKey, CursorSessionState>,
    pending: Vec<PendingCapture>,
    /// Monotonic content serial bumped when scene/output content may have changed.
    content_serial: u64,
    /// DRM device bytes for dmabuf_device (dev_t), if known.
    dmabuf_device: Option<Vec<u8>>,
    /// Advertised dmabuf (fourcc, modifiers).
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
}

impl ImageCopyCaptureManager {
    pub fn set_dmabuf_advertise(&mut self, device: Option<Vec<u8>>, formats: Vec<(u32, Vec<u64>)>) {
        self.dmabuf_device = device;
        self.dmabuf_formats = formats;
    }

    pub fn bump_content_serial(&mut self) {
        self.content_serial = self.content_serial.saturating_add(1);
    }

    pub fn content_serial(&self) -> u64 {
        self.content_serial
    }

    pub fn create_source(
        &mut self,
        client_id: ClientId,
        source: ObjectId,
        kind: CaptureSourceKind,
    ) {
        self.sources.insert(
            (client_id, source),
            SourceState {
                kind,
                sessions: HashSet::new(),
            },
        );
    }

    pub fn source_kind(&self, client_id: ClientId, source: ObjectId) -> Option<CaptureSourceKind> {
        self.sources
            .get(&(client_id, source))
            .map(|s| s.kind)
    }

    pub fn destroy_source(&mut self, client_id: ClientId, source: ObjectId) {
        // Sessions keep a copy of the source kind and outlive the opaque source
        // object (clients such as grim destroy the source immediately after
        // create_session).
        let _ = self.sources.remove(&(client_id, source));
    }

    pub fn create_session(
        &mut self,
        client_id: ClientId,
        session: ObjectId,
        source: ObjectId,
        paint_cursors: bool,
        size: CaptureBufferSize,
        writer: &mut Writer,
    ) -> bool {
        let Some(source_state) = self.sources.get_mut(&(client_id, source)) else {
            return false;
        };
        let kind = source_state.kind;
        source_state.sessions.insert(session);
        self.sessions.insert(
            (client_id, session),
            SessionState {
                kind,
                paint_cursors,
                frame: None,
                stopped: false,
                stopped_needs_emit: false,
                had_ready: false,
                last_content_serial: 0,
                size,
                constraints_dirty: false,
                cursor_session: None,
            },
        );
        self.send_constraints(client_id, session, size, writer);
        true
    }

    pub fn create_cursor_session(
        &mut self,
        client_id: ClientId,
        cursor_session: ObjectId,
        source: ObjectId,
        pointer: ObjectId,
    ) -> bool {
        let Some(source_state) = self.sources.get(&(client_id, source)) else {
            return false;
        };
        let kind = source_state.kind;
        self.cursor_sessions.insert(
            (client_id, cursor_session),
            CursorSessionState {
                source,
                kind,
                pointer,
                capture_session: None,
                entered: false,
            },
        );
        true
    }

    pub fn get_cursor_capture_session(
        &mut self,
        client_id: ClientId,
        cursor_session: ObjectId,
        session: ObjectId,
        size: CaptureBufferSize,
        writer: &mut Writer,
    ) -> Result<(), u32> {
        let Some(cursor) = self.cursor_sessions.get_mut(&(client_id, cursor_session)) else {
            return Err(0);
        };
        if cursor.capture_session.is_some() {
            return Err(EXT_IMAGE_COPY_CAPTURE_CURSOR_SESSION_V1_ERROR_DUPLICATE_SESSION);
        }
        let kind = cursor.kind;
        let source = cursor.source;
        cursor.capture_session = Some(session);
        if let Some(source_state) = self.sources.get_mut(&(client_id, source)) {
            source_state.sessions.insert(session);
        }
        self.sessions.insert(
            (client_id, session),
            SessionState {
                kind,
                paint_cursors: false,
                frame: None,
                stopped: false,
                stopped_needs_emit: false,
                had_ready: false,
                last_content_serial: 0,
                size,
                constraints_dirty: false,
                cursor_session: Some(cursor_session),
            },
        );
        self.send_constraints(client_id, session, size, writer);
        Ok(())
    }

    pub fn destroy_cursor_session(&mut self, client_id: ClientId, cursor_session: ObjectId) {
        let Some(cursor) = self.cursor_sessions.remove(&(client_id, cursor_session)) else {
            return;
        };
        if let Some(session) = cursor.capture_session {
            self.destroy_session(client_id, session);
        }
    }

    fn send_constraints(
        &self,
        _client_id: ClientId,
        session: ObjectId,
        size: CaptureBufferSize,
        writer: &mut Writer,
    ) {
        writer
            .ext_image_copy_capture_session_v1_shm_format(session)
            .format(WL_SHM_FORMAT_ARGB8888);
        writer
            .ext_image_copy_capture_session_v1_shm_format(session)
            .format(WL_SHM_FORMAT_XRGB8888);
        if let Some(device) = self.dmabuf_device.as_ref() {
            writer
                .ext_image_copy_capture_session_v1_dmabuf_device(session)
                .device(device);
        }
        for (format, modifiers) in &self.dmabuf_formats {
            let mut bytes = Vec::with_capacity(modifiers.len() * 8);
            for modifier in modifiers {
                bytes.extend_from_slice(&modifier.to_ne_bytes());
            }
            writer
                .ext_image_copy_capture_session_v1_dmabuf_format(session)
                .format(*format)
                .modifiers(&bytes);
        }
        writer
            .ext_image_copy_capture_session_v1_buffer_size(session)
            .width(size.width)
            .height(size.height);
        writer.ext_image_copy_capture_session_v1_done(session);
    }

    pub fn update_session_size(
        &mut self,
        client_id: ClientId,
        session: ObjectId,
        size: CaptureBufferSize,
        writer: &mut Writer,
    ) {
        let Some(state) = self.sessions.get_mut(&(client_id, session)) else {
            return;
        };
        if state.stopped {
            return;
        }
        if state.size == size && !state.constraints_dirty {
            return;
        }
        state.size = size;
        state.constraints_dirty = false;
        self.send_constraints(client_id, session, size, writer);
    }

    pub fn destroy_session(&mut self, client_id: ClientId, session: ObjectId) {
        self.pending
            .retain(|p| !(p.client_id == client_id && p.session == session));
        let Some(state) = self.sessions.remove(&(client_id, session)) else {
            return;
        };
        for source in self.sources.values_mut() {
            source.sessions.remove(&session);
        }
        if let Some(frame) = state.frame {
            self.frames.remove(&(client_id, frame));
        }
        if let Some(cursor_session) = state.cursor_session {
            if let Some(cursor) = self.cursor_sessions.get_mut(&(client_id, cursor_session)) {
                cursor.capture_session = None;
            }
        }
    }

    pub fn stop_session(&mut self, client_id: ClientId, session: ObjectId) {
        let Some(state) = self.sessions.get_mut(&(client_id, session)) else {
            return;
        };
        if state.stopped {
            return;
        }
        state.stopped = true;
        state.stopped_needs_emit = true;
        self.pending
            .retain(|p| !(p.client_id == client_id && p.session == session));
    }

    pub fn create_frame(
        &mut self,
        client_id: ClientId,
        session: ObjectId,
        frame: ObjectId,
    ) -> Result<(), u32> {
        let Some(state) = self.sessions.get_mut(&(client_id, session)) else {
            return Err(0);
        };
        if state.frame.is_some() {
            return Err(EXT_IMAGE_COPY_CAPTURE_SESSION_V1_ERROR_DUPLICATE_FRAME);
        }
        state.frame = Some(frame);
        self.frames.insert(
            (client_id, frame),
            FrameState {
                session,
                buffer: None,
                damage: Vec::new(),
                capture_requested: false,
            },
        );
        Ok(())
    }

    pub fn destroy_frame(&mut self, client_id: ClientId, frame: ObjectId) {
        self.pending
            .retain(|p| !(p.client_id == client_id && p.frame == frame));
        let Some(state) = self.frames.remove(&(client_id, frame)) else {
            return;
        };
        if let Some(session) = self.sessions.get_mut(&(client_id, state.session)) {
            if session.frame == Some(frame) {
                session.frame = None;
            }
        }
    }

    pub fn attach_buffer(
        &mut self,
        client_id: ClientId,
        frame: ObjectId,
        buffer: ObjectId,
    ) -> Result<(), u32> {
        let Some(state) = self.frames.get_mut(&(client_id, frame)) else {
            return Err(0);
        };
        if state.capture_requested {
            return Err(EXT_IMAGE_COPY_CAPTURE_FRAME_V1_ERROR_ALREADY_CAPTURED);
        }
        state.buffer = Some(buffer);
        Ok(())
    }

    pub fn damage_buffer(
        &mut self,
        client_id: ClientId,
        frame: ObjectId,
        rect: CaptureDamageRect,
    ) -> Result<(), u32> {
        let Some(state) = self.frames.get_mut(&(client_id, frame)) else {
            return Err(0);
        };
        if state.capture_requested {
            return Err(EXT_IMAGE_COPY_CAPTURE_FRAME_V1_ERROR_ALREADY_CAPTURED);
        }
        if rect.x < 0 || rect.y < 0 || rect.width <= 0 || rect.height <= 0 {
            return Err(EXT_IMAGE_COPY_CAPTURE_FRAME_V1_ERROR_INVALID_BUFFER_DAMAGE);
        }
        state.damage.push(rect);
        Ok(())
    }

    pub fn request_capture(
        &mut self,
        client_id: ClientId,
        frame: ObjectId,
    ) -> Result<(), u32> {
        let Some(frame_state) = self.frames.get_mut(&(client_id, frame)) else {
            return Err(0);
        };
        if frame_state.capture_requested {
            return Err(EXT_IMAGE_COPY_CAPTURE_FRAME_V1_ERROR_ALREADY_CAPTURED);
        }
        let Some(buffer) = frame_state.buffer else {
            return Err(EXT_IMAGE_COPY_CAPTURE_FRAME_V1_ERROR_NO_BUFFER);
        };
        let session_id = frame_state.session;
        let damage = frame_state.damage.clone();
        frame_state.capture_requested = true;

        let Some(session) = self.sessions.get(&(client_id, session_id)) else {
            return Err(0);
        };
        if session.stopped {
            return Ok(());
        }
        let cursor_only = session.cursor_session.is_some();
        let cursor_entered = session
            .cursor_session
            .and_then(|id| self.cursor_sessions.get(&(client_id, id)))
            .map(|c| c.entered)
            .unwrap_or(true);
        let first_frame = !session.had_ready;
        let wait_serial = session.last_content_serial;
        self.pending.push(PendingCapture {
            client_id,
            session: session_id,
            frame,
            buffer,
            source: session.kind,
            paint_cursors: session.paint_cursors,
            client_damage: damage,
            first_frame,
            wait_content_serial: wait_serial,
            cursor_only,
            cursor_entered,
        });
        Ok(())
    }

    pub fn take_ready_pending(&mut self) -> Vec<PendingCapture> {
        let serial = self.content_serial;
        let mut ready = Vec::new();
        let mut rest = Vec::new();
        for pending in self.pending.drain(..) {
            let session_ok = self
                .sessions
                .get(&(pending.client_id, pending.session))
                .map(|s| !s.stopped)
                .unwrap_or(false);
            if !session_ok {
                ready.push(pending);
                continue;
            }
            if pending.cursor_only && !pending.cursor_entered {
                rest.push(pending);
                continue;
            }
            if pending.first_frame || serial != pending.wait_content_serial {
                ready.push(pending);
            } else {
                rest.push(pending);
            }
        }
        self.pending = rest;
        ready
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn mark_frame_ready(
        &mut self,
        client_id: ClientId,
        frame: ObjectId,
        damage: CaptureDamageRect,
        tv_sec: u64,
        tv_nsec: u32,
        writer: &mut Writer,
    ) {
        let Some(frame_state) = self.frames.get(&(client_id, frame)) else {
            return;
        };
        let session = frame_state.session;
        if let Some(state) = self.sessions.get_mut(&(client_id, session)) {
            state.had_ready = true;
            state.last_content_serial = self.content_serial;
        }
        writer
            .ext_image_copy_capture_frame_v1_transform(frame)
            .transform(WL_OUTPUT_TRANSFORM_NORMAL);
        writer
            .ext_image_copy_capture_frame_v1_damage(frame)
            .x(damage.x)
            .y(damage.y)
            .width(damage.width)
            .height(damage.height);
        writer
            .ext_image_copy_capture_frame_v1_presentation_time(frame)
            .tv_sec_hi((tv_sec >> 32) as u32)
            .tv_sec_lo(tv_sec as u32)
            .tv_nsec(tv_nsec);
        writer.ext_image_copy_capture_frame_v1_ready(frame);
    }

    pub fn mark_frame_failed(
        &mut self,
        client_id: ClientId,
        frame: ObjectId,
        reason: u32,
        writer: &mut Writer,
    ) {
        let _ = client_id;
        writer
            .ext_image_copy_capture_frame_v1_failed(frame)
            .reason(reason);
    }

    pub fn emit_stopped(&mut self, client_id: ClientId, session: ObjectId, writer: &mut Writer) {
        self.stop_session(client_id, session);
        writer.ext_image_copy_capture_session_v1_stopped(session);
    }

    pub fn session_size(&self, client_id: ClientId, session: ObjectId) -> Option<CaptureBufferSize> {
        self.sessions
            .get(&(client_id, session))
            .map(|s| s.size)
    }

    pub fn session_stopped(&self, client_id: ClientId, session: ObjectId) -> bool {
        self.sessions
            .get(&(client_id, session))
            .map(|s| s.stopped)
            .unwrap_or(true)
    }

    pub fn set_cursor_entered(
        &mut self,
        client_id: ClientId,
        cursor_session: ObjectId,
        entered: bool,
        writer: &mut Writer,
    ) {
        let Some(cursor) = self.cursor_sessions.get_mut(&(client_id, cursor_session)) else {
            return;
        };
        if cursor.entered == entered {
            return;
        }
        cursor.entered = entered;
        if entered {
            writer.ext_image_copy_capture_cursor_session_v1_enter(cursor_session);
        } else {
            writer.ext_image_copy_capture_cursor_session_v1_leave(cursor_session);
        }
    }

    pub fn emit_cursor_position(
        &mut self,
        client_id: ClientId,
        cursor_session: ObjectId,
        x: i32,
        y: i32,
        hotspot_x: i32,
        hotspot_y: i32,
        writer: &mut Writer,
    ) {
        let Some(cursor) = self.cursor_sessions.get(&(client_id, cursor_session)) else {
            return;
        };
        if !cursor.entered {
            return;
        }
        writer
            .ext_image_copy_capture_cursor_session_v1_position(cursor_session)
            .x(x)
            .y(y);
        writer
            .ext_image_copy_capture_cursor_session_v1_hotspot(cursor_session)
            .x(hotspot_x)
            .y(hotspot_y);
    }

    pub fn cursor_sessions_for_client(
        &self,
        client_id: ClientId,
    ) -> Vec<(ObjectId, ObjectId, CaptureSourceKind)> {
        self.cursor_sessions
            .iter()
            .filter(|((cid, _), _)| *cid == client_id)
            .map(|((_, cursor), state)| (*cursor, state.pointer, state.kind))
            .collect()
    }

    pub fn all_cursor_sessions(&self) -> Vec<(ClientId, ObjectId, CaptureSourceKind)> {
        self.cursor_sessions
            .iter()
            .map(|((cid, cursor), state)| (*cid, *cursor, state.kind))
            .collect()
    }

    pub fn stop_sources_for_output(&mut self, client_id: ClientId, wl_output: ObjectId) {
        let sessions: Vec<ObjectId> = self
            .sessions
            .iter()
            .filter(|((cid, _), s)| {
                *cid == client_id
                    && matches!(s.kind, CaptureSourceKind::Output { wl_output: o } if o == wl_output)
            })
            .map(|((_, sid), _)| *sid)
            .collect();
        for session in sessions {
            self.stop_session(client_id, session);
        }
    }

    pub fn stop_sources_for_window(&mut self, window_id: u32) {
        let keys: Vec<ResourceKey> = self.sessions.keys().copied().collect();
        for (client_id, session) in keys {
            let stop = self
                .sessions
                .get(&(client_id, session))
                .map(|s| {
                    matches!(
                        s.kind,
                        CaptureSourceKind::Toplevel {
                            window_id: id
                        } if id == window_id
                    )
                })
                .unwrap_or(false);
            if stop {
                self.stop_session(client_id, session);
            }
        }
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        self.pending.retain(|p| p.client_id != client_id);
        let frames: Vec<_> = self
            .frames
            .keys()
            .filter(|(cid, _)| *cid == client_id)
            .map(|(_, id)| *id)
            .collect();
        for frame in frames {
            self.destroy_frame(client_id, frame);
        }
        let sessions: Vec<_> = self
            .sessions
            .keys()
            .filter(|(cid, _)| *cid == client_id)
            .map(|(_, id)| *id)
            .collect();
        for session in sessions {
            self.destroy_session(client_id, session);
        }
        let cursors: Vec<_> = self
            .cursor_sessions
            .keys()
            .filter(|(cid, _)| *cid == client_id)
            .map(|(_, id)| *id)
            .collect();
        for cursor in cursors {
            self.destroy_cursor_session(client_id, cursor);
        }
        let sources: Vec<_> = self
            .sources
            .keys()
            .filter(|(cid, _)| *cid == client_id)
            .map(|(_, id)| *id)
            .collect();
        for source in sources {
            self.destroy_source(client_id, source);
        }
    }

    pub fn take_sessions_needing_stopped_event(&mut self) -> Vec<(ClientId, ObjectId)> {
        let mut out = Vec::new();
        for ((cid, sid), state) in &mut self.sessions {
            if state.stopped_needs_emit {
                state.stopped_needs_emit = false;
                out.push((*cid, *sid));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    use lumalla_wayland_protocol::buffer::Writer;

    fn client() -> ClientId {
        ClientId::new(NonZeroU32::new(1).unwrap())
    }

    fn oid(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn writer() -> (std::os::unix::net::UnixStream, Writer) {
        let (_receiver, sender) = std::os::unix::net::UnixStream::pair().unwrap();
        let writer = Writer::new(std::os::fd::AsRawFd::as_raw_fd(&sender));
        (sender, writer)
    }

    #[test]
    fn duplicate_frame_is_rejected() {
        let mut mgr = ImageCopyCaptureManager::default();
        let cid = client();
        let (_keep, mut w) = writer();
        mgr.create_source(
            cid,
            oid(10),
            CaptureSourceKind::Output {
                wl_output: oid(2),
            },
        );
        assert!(mgr.create_session(
            cid,
            oid(11),
            oid(10),
            false,
            CaptureBufferSize {
                width: 100,
                height: 100,
            },
            &mut w,
        ));
        assert!(mgr.create_frame(cid, oid(11), oid(12)).is_ok());
        assert_eq!(
            mgr.create_frame(cid, oid(11), oid(13)),
            Err(EXT_IMAGE_COPY_CAPTURE_SESSION_V1_ERROR_DUPLICATE_FRAME)
        );
    }

    #[test]
    fn capture_without_buffer_errors() {
        let mut mgr = ImageCopyCaptureManager::default();
        let cid = client();
        let (_keep, mut w) = writer();
        mgr.create_source(
            cid,
            oid(10),
            CaptureSourceKind::Toplevel { window_id: 1 },
        );
        assert!(mgr.create_session(
            cid,
            oid(11),
            oid(10),
            false,
            CaptureBufferSize {
                width: 10,
                height: 10,
            },
            &mut w,
        ));
        assert!(mgr.create_frame(cid, oid(11), oid(12)).is_ok());
        assert_eq!(
            mgr.request_capture(cid, oid(12)),
            Err(EXT_IMAGE_COPY_CAPTURE_FRAME_V1_ERROR_NO_BUFFER)
        );
    }

    #[test]
    fn invalid_damage_is_rejected() {
        let mut mgr = ImageCopyCaptureManager::default();
        let cid = client();
        let (_keep, mut w) = writer();
        mgr.create_source(
            cid,
            oid(10),
            CaptureSourceKind::Output {
                wl_output: oid(2),
            },
        );
        assert!(mgr.create_session(
            cid,
            oid(11),
            oid(10),
            false,
            CaptureBufferSize {
                width: 10,
                height: 10,
            },
            &mut w,
        ));
        assert!(mgr.create_frame(cid, oid(11), oid(12)).is_ok());
        assert_eq!(
            mgr.damage_buffer(
                cid,
                oid(12),
                CaptureDamageRect {
                    x: -1,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            ),
            Err(EXT_IMAGE_COPY_CAPTURE_FRAME_V1_ERROR_INVALID_BUFFER_DAMAGE)
        );
    }

    #[test]
    fn first_capture_is_ready_without_content_change() {
        let mut mgr = ImageCopyCaptureManager::default();
        let cid = client();
        let (_keep, mut w) = writer();
        mgr.create_source(
            cid,
            oid(10),
            CaptureSourceKind::Output {
                wl_output: oid(2),
            },
        );
        assert!(mgr.create_session(
            cid,
            oid(11),
            oid(10),
            false,
            CaptureBufferSize {
                width: 10,
                height: 10,
            },
            &mut w,
        ));
        assert!(mgr.create_frame(cid, oid(11), oid(12)).is_ok());
        assert!(mgr.attach_buffer(cid, oid(12), oid(13)).is_ok());
        assert!(mgr.request_capture(cid, oid(12)).is_ok());
        let ready = mgr.take_ready_pending();
        assert_eq!(ready.len(), 1);
        assert!(ready[0].first_frame);
    }
}
