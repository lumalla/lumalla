//! Per-output and per-plane presentation state.
//!
//! Hierarchy: DRM device topology (CRTCs/planes) → output (connector+CRTC) →
//! plane pipelines (primary / cursor / overlays), each with its own FB triple.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use io_uring::types::Timespec;
use lumalla_shared::EventLoop;

use crate::drm::{ConnectedOutput, ModeBlob};
use crate::scanout_pool::ScanoutBuffer;
use crate::scheduler::RenderScheduler;
use crate::scene_backing::{MAX_SCANOUT_BUFFER_AGE, UploadRect};

/// Stable output identity. Names alone can collide across GPUs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutputId {
    pub drm_path: PathBuf,
    /// `0` for virtual (non-KMS) outputs.
    pub connector_id: u32,
    pub name: String,
}

impl OutputId {
    pub fn physical(drm_path: PathBuf, connector_id: u32, name: impl Into<String>) -> Self {
        Self {
            drm_path,
            connector_id,
            name: name.into(),
        }
    }

    pub fn virtual_output(name: impl Into<String>) -> Self {
        Self {
            drm_path: PathBuf::from("virtual"),
            connector_id: 0,
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneKind {
    Primary,
    Cursor,
    Overlay,
}

/// Cached plane property IDs for atomic commits.
#[derive(Debug, Clone)]
pub struct PlaneAtomicProps {
    pub fb_id: u32,
    pub crtc_id: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub crtc_x: u32,
    pub crtc_y: u32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub zpos: Option<u32>,
}

/// Cached once per open DRM fd; invalidate on device remove / rebind.
#[derive(Debug, Clone)]
pub struct DrmPlaneInfo {
    pub plane_id: u32,
    pub kind: PlaneKind,
    /// Bitmask over card CRTC indices (`possible_crtcs`).
    pub possible_crtcs: u32,
    pub zpos: u32,
    pub zpos_mutable: bool,
    pub props: PlaneAtomicProps,
    /// DRM fourccs advertised on the plane.
    pub formats: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct DrmCrtcInfo {
    pub crtc_id: u32,
    /// Index into the card's CRTC list (for `possible_crtcs` bits).
    pub index: u32,
}

#[derive(Debug, Clone)]
pub struct DrmDeviceTopology {
    pub path: PathBuf,
    pub crtcs: Vec<DrmCrtcInfo>,
    pub planes: Vec<DrmPlaneInfo>,
    /// Overlay plane ids not currently leased to an output.
    pub free_overlays: Vec<u32>,
}

impl DrmDeviceTopology {
    pub fn plane(&self, plane_id: u32) -> Option<&DrmPlaneInfo> {
        self.planes.iter().find(|p| p.plane_id == plane_id)
    }

    pub fn primary_for_crtc(&self, crtc_index: u32) -> Option<&DrmPlaneInfo> {
        self.planes.iter().find(|p| {
            matches!(p.kind, PlaneKind::Primary) && p.possible_crtcs & (1 << crtc_index) != 0
        })
    }

    pub fn cursor_for_crtc(&self, crtc_index: u32) -> Option<&DrmPlaneInfo> {
        self.planes.iter().find(|p| {
            matches!(p.kind, PlaneKind::Cursor) && p.possible_crtcs & (1 << crtc_index) != 0
        })
    }
}

/// Where a plane is placed on its CRTC.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // geometry is written for introspection / future atomic bundling
pub struct PlaneGeometry {
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    /// Source rectangle in 16.16 fixed point.
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
}

impl PlaneGeometry {
    pub fn fullscreen(width: u32, height: u32) -> Self {
        Self {
            crtc_x: 0,
            crtc_y: 0,
            crtc_w: width,
            crtc_h: height,
            src_x: 0,
            src_y: 0,
            src_w: width << 16,
            src_h: height << 16,
        }
    }

    pub fn cursor(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            crtc_x: x,
            crtc_y: y,
            crtc_w: width,
            crtc_h: height,
            src_x: 0,
            src_y: 0,
            src_w: width << 16,
            src_h: height << 16,
        }
    }
}

/// Soft upper bound for HW cursor bitmaps (common amdgpu/i915 limit).
pub const HW_CURSOR_MAX_SIZE: u32 = 256;

/// Per-plane FB pipeline (`current` / `pending` / `queued`).
pub struct PlanePipeline {
    pub plane_id: u32,
    pub kind: PlaneKind,
    pub zpos: u32,
    pub geometry: PlaneGeometry,
    pub current: Option<ScanoutBuffer>,
    pub pending: Option<ScanoutBuffer>,
    pub queued: Option<ScanoutBuffer>,
    /// Soft path: content is composited into another plane (e.g. cursor into primary).
    pub software: bool,
    /// Property IDs when this plane is a real KMS object.
    pub props: Option<PlaneAtomicProps>,
}

impl PlanePipeline {
    pub fn primary(plane_id: u32, zpos: u32, geometry: PlaneGeometry) -> Self {
        Self {
            plane_id,
            kind: PlaneKind::Primary,
            zpos,
            geometry,
            current: None,
            pending: None,
            queued: None,
            software: false,
            props: None,
        }
    }

    pub fn software_cursor() -> Self {
        Self {
            plane_id: 0,
            kind: PlaneKind::Cursor,
            zpos: 0,
            geometry: PlaneGeometry::fullscreen(0, 0),
            current: None,
            pending: None,
            queued: None,
            software: true,
            props: None,
        }
    }

    pub fn hw_cursor(info: &DrmPlaneInfo) -> Self {
        Self {
            plane_id: info.plane_id,
            kind: PlaneKind::Cursor,
            zpos: info.zpos,
            geometry: PlaneGeometry::cursor(0, 0, 0, 0),
            current: None,
            pending: None,
            queued: None,
            software: false,
            props: Some(info.props.clone()),
        }
    }

    pub fn is_hw_cursor(&self) -> bool {
        matches!(self.kind, PlaneKind::Cursor) && !self.software && self.plane_id != 0
    }

    pub fn flip_busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Newest buffer content for this plane (queued → pending → current).
    pub fn newest_buffer(&self) -> Option<&ScanoutBuffer> {
        self.queued
            .as_ref()
            .or(self.pending.as_ref())
            .or(self.current.as_ref())
    }
}

pub struct OutputPlanes {
    pub primary: PlanePipeline,
    /// `None` or `software: true` ⇒ cursor is painted into the primary FB.
    pub cursor: Option<PlanePipeline>,
    pub overlays: Vec<PlanePipeline>,
}

impl OutputPlanes {
    pub fn with_primary_and_cursor(
        plane_id: u32,
        zpos: u32,
        width: u32,
        height: u32,
        cursor: Option<&DrmPlaneInfo>,
    ) -> Self {
        let cursor = match cursor {
            Some(info) => PlanePipeline::hw_cursor(info),
            None => PlanePipeline::software_cursor(),
        };
        Self {
            primary: PlanePipeline::primary(plane_id, zpos, PlaneGeometry::fullscreen(width, height)),
            cursor: Some(cursor),
            overlays: Vec::new(),
        }
    }

    pub fn hw_cursor_active(&self) -> bool {
        self.cursor.as_ref().is_some_and(|c| c.is_hw_cursor())
    }

    pub fn drain_buffers(self) -> Vec<ScanoutBuffer> {
        let mut out = Vec::new();
        let mut take_pipe = |pipe: PlanePipeline| {
            if let Some(b) = pipe.current {
                out.push(b);
            }
            if let Some(b) = pipe.pending {
                out.push(b);
            }
            if let Some(b) = pipe.queued {
                out.push(b);
            }
        };
        take_pipe(self.primary);
        if let Some(cursor) = self.cursor {
            take_pipe(cursor);
        }
        for overlay in self.overlays {
            take_pipe(overlay);
        }
        out
    }
}

/// KMS binding for a physical output.
pub struct PhysicalOutput {
    pub drm_path: PathBuf,
    pub output: ConnectedOutput,
    /// Keeps the CRTC MODE_ID blob alive while active.
    #[allow(dead_code)]
    pub mode_blob: ModeBlob,
}

impl PhysicalOutput {
    pub fn matches_target(&self, drm_path: &Path, output: &ConnectedOutput) -> bool {
        self.drm_path == drm_path
            && self.output.connector_id == output.connector_id
            && self.output.crtc_id == output.crtc_id
            && self.output.plane_id == output.plane_id
            && self.output.mode == output.mode
    }
}

/// Per-output dirty flags (planes may update independently).
#[derive(Debug, Clone, Default)]
pub struct OutputDirty {
    pub primary: bool,
    pub cursor_image: bool,
    pub cursor_pos: bool,
}

impl OutputDirty {
    pub fn mark_all(&mut self) {
        self.primary = true;
        self.cursor_image = true;
        self.cursor_pos = true;
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Per-output adaptive schedule + absolute wake timer state.
pub struct OutputPresentControl {
    pub scheduler: RenderScheduler,
    pub wake_token: u64,
    pub wake_ts: Box<Timespec>,
    pub wake_deadline: Option<(u64, u32)>,
    pub wake_armed: bool,
    /// Scene content not yet presented on this output.
    pub content_dirty: bool,
}

impl OutputPresentControl {
    pub fn new(wake_token: u64, refresh_mhz: i32) -> Self {
        Self {
            scheduler: RenderScheduler::new(refresh_mhz),
            wake_token,
            wake_ts: Box::new(Timespec::new()),
            wake_deadline: None,
            wake_armed: false,
            content_dirty: false,
        }
    }

    pub fn clear_wake(&mut self, event_loop: &mut EventLoop) -> io::Result<()> {
        if !self.wake_armed {
            self.wake_deadline = None;
            return Ok(());
        }
        event_loop.cancel_timeout(self.wake_token)?;
        self.wake_armed = false;
        self.wake_deadline = None;
        Ok(())
    }

    pub fn set_wake(&mut self, event_loop: &mut EventLoop, sec: u64, nsec: u32) -> io::Result<()> {
        if self.wake_armed && self.wake_deadline == Some((sec, nsec)) {
            return Ok(());
        }
        if self.wake_armed {
            event_loop.cancel_timeout(self.wake_token)?;
            self.wake_armed = false;
        }
        *self.wake_ts = Timespec::new().sec(sec).nsec(nsec);
        event_loop.submit_timeout_absolute(Pin::new(self.wake_ts.as_ref()), self.wake_token)?;
        self.wake_armed = true;
        self.wake_deadline = Some((sec, nsec));
        Ok(())
    }
}

/// Live state for one presentable output (physical or virtual).
pub struct OutputState {
    pub id: OutputId,
    /// `None` until the first successful modeset / virtual present.
    pub physical: Option<PhysicalOutput>,
    /// `None` until the first present creates plane pipelines.
    pub planes: Option<OutputPlanes>,
    pub present: OutputPresentControl,
    pub dirty: OutputDirty,
    /// Monotonic serial of the last successful GPU fill for this output.
    pub present_serial: u64,
    /// Coalesced damage from recent presents (for buffer-age partial repair).
    pub damage_history: OutputDamageHistory,
}

/// Recent per-output present damage for buffer-age partial repairs.
#[derive(Debug, Default, Clone)]
pub struct OutputDamageHistory {
    /// Newest at the back. `None` means that present was a full redraw.
    entries: VecDeque<Option<UploadRect>>,
}

impl OutputDamageHistory {
    pub fn push(&mut self, damage: Option<UploadRect>) {
        self.entries.push_back(damage);
        while self.entries.len() > MAX_SCANOUT_BUFFER_AGE as usize {
            self.entries.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn entries(&self) -> Vec<Option<UploadRect>> {
        self.entries.iter().copied().collect()
    }
}

impl OutputState {
    pub fn new(id: OutputId, present: OutputPresentControl) -> Self {
        Self {
            id,
            physical: None,
            planes: None,
            present,
            dirty: OutputDirty::default(),
            present_serial: 0,
            damage_history: OutputDamageHistory::default(),
        }
    }

    pub fn primary_flip_busy(&self) -> bool {
        self.planes
            .as_ref()
            .is_some_and(|p| p.primary.flip_busy())
    }

    pub fn primary(&self) -> Option<&PlanePipeline> {
        self.planes.as_ref().map(|p| &p.primary)
    }

    pub fn primary_mut(&mut self) -> Option<&mut PlanePipeline> {
        self.planes.as_mut().map(|p| &mut p.primary)
    }

    pub fn cursor(&self) -> Option<&PlanePipeline> {
        self.planes.as_ref().and_then(|p| p.cursor.as_ref())
    }

    pub fn cursor_mut(&mut self) -> Option<&mut PlanePipeline> {
        self.planes.as_mut().and_then(|p| p.cursor.as_mut())
    }

    pub fn hw_cursor_active(&self) -> bool {
        self.planes
            .as_ref()
            .is_some_and(|p| p.hw_cursor_active())
    }

    pub fn is_virtual(&self) -> bool {
        self.physical.is_none()
            && (self.id.connector_id == 0 || self.id.drm_path.as_os_str() == "virtual")
    }

    pub fn ensure_planes(
        &mut self,
        plane_id: u32,
        zpos: u32,
        width: u32,
        height: u32,
        cursor: Option<&DrmPlaneInfo>,
    ) -> &mut OutputPlanes {
        if self.planes.is_none() {
            self.planes = Some(OutputPlanes::with_primary_and_cursor(
                plane_id, zpos, width, height, cursor,
            ));
        }
        self.planes.as_mut().unwrap()
    }

    /// Backward-compatible helper: primary + software cursor.
    pub fn ensure_primary_planes(
        &mut self,
        plane_id: u32,
        zpos: u32,
        width: u32,
        height: u32,
    ) -> &mut OutputPlanes {
        self.ensure_planes(plane_id, zpos, width, height, None)
    }
}

/// Empty topology placeholder used until a card is probed.
pub fn empty_topology(path: PathBuf) -> DrmDeviceTopology {
    DrmDeviceTopology {
        path,
        crtcs: Vec::new(),
        planes: Vec::new(),
        free_overlays: Vec::new(),
    }
}
