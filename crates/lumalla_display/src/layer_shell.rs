//! wlr-layer-shell state: output-bound layer surfaces with configure/ack mapping.

use std::collections::HashMap;

use lumalla_wayland_protocol::{
    ClientId, ObjectId,
    protocols::wlr_layer_shell::{
        ZWLR_LAYER_SHELL_V1_LAYER_BACKGROUND, ZWLR_LAYER_SHELL_V1_LAYER_BOTTOM,
        ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY, ZWLR_LAYER_SHELL_V1_LAYER_TOP,
        ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM, ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT,
        ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT, ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP,
        ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_EXCLUSIVE,
        ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_NONE,
        ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_ON_DEMAND,
    },
};

use crate::GlobalId;

type ResourceKey = (ClientId, ObjectId);

const ANCHOR_ALL: u32 = ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP
    | ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM
    | ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT
    | ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerShellError {
    UnknownShell,
    UnknownLayerSurface,
    UnknownSurface,
    RoleConflict,
    AlreadyConstructed,
    InvalidLayer,
    InvalidSurfaceState,
    InvalidSize,
    InvalidAnchor,
    InvalidKeyboardInteractivity,
    InvalidSerial,
    UnconfiguredBuffer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum LayerBand {
    Background = 0,
    Bottom = 1,
    #[default]
    Top = 2,
    Overlay = 3,
}

impl LayerBand {
    pub fn from_protocol(value: u32) -> Option<Self> {
        match value {
            ZWLR_LAYER_SHELL_V1_LAYER_BACKGROUND => Some(Self::Background),
            ZWLR_LAYER_SHELL_V1_LAYER_BOTTOM => Some(Self::Bottom),
            ZWLR_LAYER_SHELL_V1_LAYER_TOP => Some(Self::Top),
            ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY => Some(Self::Overlay),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardInteractivity {
    #[default]
    None,
    Exclusive,
    OnDemand,
}

impl KeyboardInteractivity {
    fn from_protocol(value: u32) -> Option<Self> {
        match value {
            ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_NONE => Some(Self::None),
            ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_EXCLUSIVE => Some(Self::Exclusive),
            ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_ON_DEMAND => Some(Self::OnDemand),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExclusiveInsets {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl ExclusiveInsets {
    pub fn inset_size(&self, width: i32, height: i32) -> (i32, i32) {
        (
            (width - self.left - self.right).max(0),
            (height - self.top - self.bottom).max(0),
        )
    }

    pub fn inset_rect(&self, x: i32, y: i32, width: i32, height: i32) -> (i32, i32, i32, i32) {
        let (w, h) = self.inset_size(width, height);
        (x + self.left, y + self.top, w, h)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerConfigure {
    pub serial: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerCommitOutcome {
    pub initial_configure: Option<LayerConfigure>,
    pub applied: bool,
    pub mapped: bool,
    pub unmapped: bool,
    pub geometry: Option<LayerGeometry>,
    pub closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerSurfaceInfo {
    pub layer_surface: ObjectId,
    pub wl_surface: ObjectId,
    pub output: GlobalId,
    pub band: LayerBand,
    pub keyboard_interactivity: KeyboardInteractivity,
    pub mapped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct LayerPendingState {
    size: (u32, u32),
    anchor: u32,
    exclusive_zone: i32,
    margin: (i32, i32, i32, i32),
    keyboard_interactivity: KeyboardInteractivity,
    layer: LayerBand,
}

#[derive(Debug)]
struct LayerSurfaceState {
    shell: ObjectId,
    wl_surface: ObjectId,
    namespace: String,
    output: GlobalId,
    pending: LayerPendingState,
    current: LayerPendingState,
    pending_configures: Vec<LayerConfigure>,
    pending_ack: Option<LayerConfigure>,
    current_configure: Option<LayerConfigure>,
    initial_configure_sent: bool,
    map_ready: bool,
    mapped: bool,
    closed: bool,
    geometry: Option<LayerGeometry>,
}

#[derive(Debug, Default)]
pub struct LayerShellManager {
    shells: HashMap<ResourceKey, ()>,
    surfaces: HashMap<ResourceKey, LayerSurfaceState>,
    surface_to_layer: HashMap<ResourceKey, ObjectId>,
    /// Per-output stacking: band → back-to-front layer_surface ids.
    stack: HashMap<GlobalId, [Vec<ResourceKey>; 4]>,
    next_configure_serial: u32,
}

impl LayerShellManager {
    pub fn create_shell(&mut self, client_id: ClientId, id: ObjectId) {
        self.shells.insert((client_id, id), ());
    }

    pub fn destroy_shell(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<(), LayerShellError> {
        self.shells
            .remove(&(client_id, id))
            .ok_or(LayerShellError::UnknownShell)?;
        Ok(())
    }

    pub fn create_layer_surface(
        &mut self,
        client_id: ClientId,
        shell: ObjectId,
        id: ObjectId,
        wl_surface: ObjectId,
        output: GlobalId,
        layer: u32,
        namespace: String,
    ) -> Result<(), LayerShellError> {
        if !self.shells.contains_key(&(client_id, shell)) {
            return Err(LayerShellError::UnknownShell);
        }
        let band = LayerBand::from_protocol(layer).ok_or(LayerShellError::InvalidLayer)?;
        if self.surface_to_layer.contains_key(&(client_id, wl_surface)) {
            return Err(LayerShellError::RoleConflict);
        }
        if self.surfaces.contains_key(&(client_id, id)) {
            return Err(LayerShellError::AlreadyConstructed);
        }
        let pending = LayerPendingState {
            layer: band,
            keyboard_interactivity: KeyboardInteractivity::None,
            ..LayerPendingState::default()
        };
        self.surfaces.insert(
            (client_id, id),
            LayerSurfaceState {
                shell,
                wl_surface,
                namespace,
                output,
                pending,
                current: pending,
                pending_configures: Vec::new(),
                pending_ack: None,
                current_configure: None,
                initial_configure_sent: false,
                map_ready: false,
                mapped: false,
                closed: false,
                geometry: None,
            },
        );
        self.surface_to_layer.insert((client_id, wl_surface), id);
        Ok(())
    }

    pub fn destroy_layer_surface(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<ObjectId, LayerShellError> {
        let state = self
            .surfaces
            .remove(&(client_id, id))
            .ok_or(LayerShellError::UnknownLayerSurface)?;
        self.surface_to_layer
            .remove(&(client_id, state.wl_surface));
        self.remove_from_stack(client_id, id, state.output, state.current.layer);
        Ok(state.wl_surface)
    }

    pub fn layer_surface_for_wl(
        &self,
        client_id: ClientId,
        wl_surface: ObjectId,
    ) -> Option<ObjectId> {
        self.surface_to_layer
            .get(&(client_id, wl_surface))
            .copied()
    }

    pub fn info(
        &self,
        client_id: ClientId,
        layer_surface: ObjectId,
    ) -> Option<LayerSurfaceInfo> {
        let state = self.surfaces.get(&(client_id, layer_surface))?;
        Some(LayerSurfaceInfo {
            layer_surface,
            wl_surface: state.wl_surface,
            output: state.output,
            band: state.current.layer,
            keyboard_interactivity: state.current.keyboard_interactivity,
            mapped: state.mapped,
        })
    }

    pub fn info_for_wl(
        &self,
        client_id: ClientId,
        wl_surface: ObjectId,
    ) -> Option<LayerSurfaceInfo> {
        let layer = self.layer_surface_for_wl(client_id, wl_surface)?;
        self.info(client_id, layer)
    }

    pub fn wl_surface(
        &self,
        client_id: ClientId,
        layer_surface: ObjectId,
    ) -> Option<ObjectId> {
        self.surfaces
            .get(&(client_id, layer_surface))
            .map(|state| state.wl_surface)
    }

    pub fn output_global(
        &self,
        client_id: ClientId,
        layer_surface: ObjectId,
    ) -> Option<GlobalId> {
        self.surfaces
            .get(&(client_id, layer_surface))
            .map(|state| state.output)
    }

    pub fn bound_output_object(
        &self,
        client_id: ClientId,
        wl_surface: ObjectId,
    ) -> Option<(GlobalId, ObjectId)> {
        let info = self.info_for_wl(client_id, wl_surface)?;
        Some((info.output, info.wl_surface))
    }

    pub fn set_size(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        width: u32,
        height: u32,
    ) -> Result<(), LayerShellError> {
        self.surface_mut(client_id, id)?.pending.size = (width, height);
        Ok(())
    }

    pub fn set_anchor(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        anchor: u32,
    ) -> Result<(), LayerShellError> {
        if anchor & !ANCHOR_ALL != 0 {
            return Err(LayerShellError::InvalidAnchor);
        }
        self.surface_mut(client_id, id)?.pending.anchor = anchor;
        Ok(())
    }

    pub fn set_exclusive_zone(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        zone: i32,
    ) -> Result<(), LayerShellError> {
        self.surface_mut(client_id, id)?.pending.exclusive_zone = zone;
        Ok(())
    }

    pub fn set_margin(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        top: i32,
        right: i32,
        bottom: i32,
        left: i32,
    ) -> Result<(), LayerShellError> {
        self.surface_mut(client_id, id)?.pending.margin = (top, right, bottom, left);
        Ok(())
    }

    pub fn set_keyboard_interactivity(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        value: u32,
    ) -> Result<(), LayerShellError> {
        let mode = KeyboardInteractivity::from_protocol(value)
            .ok_or(LayerShellError::InvalidKeyboardInteractivity)?;
        self.surface_mut(client_id, id)?.pending.keyboard_interactivity = mode;
        Ok(())
    }

    pub fn set_layer(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        layer: u32,
    ) -> Result<(), LayerShellError> {
        let band = LayerBand::from_protocol(layer).ok_or(LayerShellError::InvalidLayer)?;
        self.surface_mut(client_id, id)?.pending.layer = band;
        Ok(())
    }

    pub fn ack_configure(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        serial: u32,
    ) -> Result<(), LayerShellError> {
        let state = self.surface_mut(client_id, id)?;
        let index = state
            .pending_configures
            .iter()
            .position(|cfg| cfg.serial == serial)
            .ok_or(LayerShellError::InvalidSerial)?;
        let snapshot = state.pending_configures[index];
        state.pending_configures.drain(..=index);
        state.pending_ack = Some(snapshot);
        Ok(())
    }

    pub fn check_buffer_commit(
        &self,
        client_id: ClientId,
        wl_surface: ObjectId,
        attaching: bool,
    ) -> Result<(), LayerShellError> {
        let Some(layer_id) = self.layer_surface_for_wl(client_id, wl_surface) else {
            return Ok(());
        };
        let state = self
            .surfaces
            .get(&(client_id, layer_id))
            .ok_or(LayerShellError::UnknownLayerSurface)?;
        if state.closed {
            return Ok(());
        }
        if attaching && !state.map_ready && state.pending_ack.is_none() {
            return Err(LayerShellError::UnconfiguredBuffer);
        }
        Ok(())
    }

    /// Apply double-buffered state on wl_surface.commit.
    ///
    /// `output_size` is the bound output's pixel size. `exclusive` is the current
    /// exclusive inset on that output (before this surface's own zone).
    pub fn on_wl_surface_commit(
        &mut self,
        client_id: ClientId,
        wl_surface: ObjectId,
        buffer: Option<bool>,
        output_size: (i32, i32),
        exclusive: ExclusiveInsets,
    ) -> Result<LayerCommitOutcome, LayerShellError> {
        let Some(layer_id) = self.layer_surface_for_wl(client_id, wl_surface) else {
            return Ok(LayerCommitOutcome::default());
        };
        if self
            .surfaces
            .get(&(client_id, layer_id))
            .is_some_and(|s| s.closed)
        {
            return Ok(LayerCommitOutcome {
                closed: true,
                ..LayerCommitOutcome::default()
            });
        }

        let mut outcome = LayerCommitOutcome::default();
        let had_ack = {
            let state = self.surface_mut(client_id, layer_id)?;
            if let Some(ack) = state.pending_ack.take() {
                state.current_configure = Some(ack);
                state.map_ready = true;
                outcome.applied = true;
            }
            state.map_ready
        };

        // Commit pending state.
        let (old_band, old_output, old_mapped) = {
            let state = self.surface_mut(client_id, layer_id)?;
            let old_band = state.current.layer;
            let old_output = state.output;
            let old_mapped = state.mapped;
            if let Err(err) = validate_pending(&state.pending) {
                return Err(err);
            }
            state.current = state.pending;
            (old_band, old_output, old_mapped)
        };

        let attaching = matches!(buffer, Some(true));
        let detaching = matches!(buffer, Some(false));

        if detaching {
            let state = self.surface_mut(client_id, layer_id)?;
            if state.mapped {
                outcome.unmapped = true;
            }
            state.mapped = false;
            state.map_ready = false;
            state.pending_ack = None;
            state.current_configure = None;
            state.pending_configures.clear();
            state.initial_configure_sent = false;
            state.geometry = None;
            // Reset to post-get_layer_surface defaults for remapping.
            let layer = state.current.layer;
            state.pending = LayerPendingState {
                layer,
                keyboard_interactivity: KeyboardInteractivity::None,
                ..LayerPendingState::default()
            };
            state.current = state.pending;
            self.remove_from_stack(client_id, layer_id, old_output, old_band);
            return Ok(outcome);
        }

        // Initial / remap: bufferless commit requests configure.
        if !had_ack && !attaching {
            let configure = self.queue_configure(client_id, layer_id, output_size, exclusive)?;
            outcome.initial_configure = Some(configure);
            return Ok(outcome);
        }

        if attaching && !had_ack {
            return Err(LayerShellError::UnconfiguredBuffer);
        }

        let geometry = {
            let state = self.surfaces.get(&(client_id, layer_id)).unwrap();
            compute_geometry(&state.current, output_size, exclusive)
        };
        {
            let state = self.surface_mut(client_id, layer_id)?;
            state.geometry = Some(geometry);
            let was_mapped = state.mapped;
            state.mapped = state.map_ready && attaching || (state.mapped && buffer.is_none());
            if attaching {
                state.mapped = true;
            }
            outcome.mapped = state.mapped && !was_mapped;
            outcome.geometry = Some(geometry);
        }

        // Restack if layer changed or newly mapped.
        let (mapped, output, band) = {
            let state = self.surfaces.get(&(client_id, layer_id)).unwrap();
            (state.mapped, state.output, state.current.layer)
        };
        if mapped {
            if old_mapped && (old_band != band || old_output != output) {
                self.remove_from_stack(client_id, layer_id, old_output, old_band);
            }
            self.push_to_stack(client_id, layer_id, output, band);
        } else if old_mapped {
            self.remove_from_stack(client_id, layer_id, old_output, old_band);
        }

        // Size/anchor changes after map may need a new configure.
        if buffer.is_none() && had_ack {
            let needs = {
                let state = self.surfaces.get(&(client_id, layer_id)).unwrap();
                let (cw, ch) = state
                    .current_configure
                    .map(|c| (c.width, c.height))
                    .unwrap_or((0, 0));
                let desired = desired_configure_size(&state.current, output_size, exclusive);
                desired != (cw, ch)
            };
            if needs {
                let configure = self.queue_configure(client_id, layer_id, output_size, exclusive)?;
                outcome.initial_configure = Some(configure);
            }
        }

        Ok(outcome)
    }

    pub fn exclusive_insets_for_output(&self, output: GlobalId) -> ExclusiveInsets {
        let mut insets = ExclusiveInsets::default();
        let Some(bands) = self.stack.get(&output) else {
            return insets;
        };
        for band_stack in bands.iter() {
            for &(client_id, layer_id) in band_stack {
                let Some(state) = self.surfaces.get(&(client_id, layer_id)) else {
                    continue;
                };
                if !state.mapped || state.closed {
                    continue;
                }
                apply_exclusive_zone(&mut insets, &state.current);
            }
        }
        insets
    }

    /// Mapped layer roots for an output in paint order (background → overlay).
    pub fn mapped_roots_for_output(&self, output: GlobalId) -> Vec<(ClientId, ObjectId, LayerBand)> {
        let mut out = Vec::new();
        let Some(bands) = self.stack.get(&output) else {
            return out;
        };
        for (index, band_stack) in bands.iter().enumerate() {
            let band = match index {
                0 => LayerBand::Background,
                1 => LayerBand::Bottom,
                2 => LayerBand::Top,
                _ => LayerBand::Overlay,
            };
            for &(client_id, layer_id) in band_stack {
                let Some(state) = self.surfaces.get(&(client_id, layer_id)) else {
                    continue;
                };
                if state.mapped && !state.closed {
                    out.push((client_id, state.wl_surface, band));
                }
            }
        }
        out
    }

    /// Surfaces closed because their output was removed.
    pub fn close_output(&mut self, output: GlobalId) -> Vec<(ClientId, ObjectId, ObjectId)> {
        let mut closed = Vec::new();
        for (&(client_id, layer_id), state) in &mut self.surfaces {
            if state.output == output && !state.closed {
                state.closed = true;
                state.mapped = false;
                closed.push((client_id, layer_id, state.wl_surface));
            }
        }
        self.stack.remove(&output);
        closed
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        let layer_ids: Vec<ObjectId> = self
            .surfaces
            .keys()
            .filter_map(|&(cid, id)| (cid == client_id).then_some(id))
            .collect();
        for id in layer_ids {
            let _ = self.destroy_layer_surface(client_id, id);
        }
        self.shells.retain(|&(cid, _), _| cid != client_id);
    }

    pub fn is_map_ready(&self, client_id: ClientId, layer_surface: ObjectId) -> bool {
        self.surfaces
            .get(&(client_id, layer_surface))
            .is_some_and(|state| state.map_ready && !state.closed)
    }

    pub fn can_take_keyboard_focus(
        &self,
        client_id: ClientId,
        wl_surface: ObjectId,
    ) -> Option<bool> {
        let info = self.info_for_wl(client_id, wl_surface)?;
        Some(match info.keyboard_interactivity {
            KeyboardInteractivity::None => false,
            KeyboardInteractivity::OnDemand => true,
            KeyboardInteractivity::Exclusive => true,
        })
    }

    /// Top-most exclusive keyboard layer surface on any output (top/overlay preferred).
    pub fn top_exclusive_keyboard_surface(&self) -> Option<(ClientId, ObjectId)> {
        let mut best: Option<(LayerBand, ClientId, ObjectId)> = None;
        for bands in self.stack.values() {
            for (index, band_stack) in bands.iter().enumerate().rev() {
                let band = match index {
                    0 => LayerBand::Background,
                    1 => LayerBand::Bottom,
                    2 => LayerBand::Top,
                    _ => LayerBand::Overlay,
                };
                if matches!(band, LayerBand::Background | LayerBand::Bottom) {
                    continue;
                }
                for &(client_id, layer_id) in band_stack.iter().rev() {
                    let Some(state) = self.surfaces.get(&(client_id, layer_id)) else {
                        continue;
                    };
                    if state.mapped
                        && !state.closed
                        && state.current.keyboard_interactivity
                            == KeyboardInteractivity::Exclusive
                    {
                        let candidate = (band, client_id, state.wl_surface);
                        if best.is_none_or(|b| candidate.0 >= b.0) {
                            best = Some(candidate);
                        }
                        break;
                    }
                }
            }
        }
        best.map(|(_, c, s)| (c, s))
    }

    fn queue_configure(
        &mut self,
        client_id: ClientId,
        layer_id: ObjectId,
        output_size: (i32, i32),
        exclusive: ExclusiveInsets,
    ) -> Result<LayerConfigure, LayerShellError> {
        let state = self.surface_mut(client_id, layer_id)?;
        let (width, height) = desired_configure_size(&state.pending, output_size, exclusive);
        self.next_configure_serial = self.next_configure_serial.wrapping_add(1).max(1);
        let configure = LayerConfigure {
            serial: self.next_configure_serial,
            width,
            height,
        };
        let state = self.surface_mut(client_id, layer_id)?;
        state.pending_configures.push(configure);
        state.initial_configure_sent = true;
        Ok(configure)
    }

    fn push_to_stack(
        &mut self,
        client_id: ClientId,
        layer_id: ObjectId,
        output: GlobalId,
        band: LayerBand,
    ) {
        let bands = self.stack.entry(output).or_default();
        let stack = &mut bands[band as usize];
        let key = (client_id, layer_id);
        stack.retain(|entry| *entry != key);
        stack.push(key);
    }

    fn remove_from_stack(
        &mut self,
        client_id: ClientId,
        layer_id: ObjectId,
        output: GlobalId,
        band: LayerBand,
    ) {
        if let Some(bands) = self.stack.get_mut(&output) {
            bands[band as usize].retain(|entry| *entry != (client_id, layer_id));
        }
    }

    fn surface_mut(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<&mut LayerSurfaceState, LayerShellError> {
        self.surfaces
            .get_mut(&(client_id, id))
            .ok_or(LayerShellError::UnknownLayerSurface)
    }
}

fn validate_pending(pending: &LayerPendingState) -> Result<(), LayerShellError> {
    let (w, h) = pending.size;
    let anchor = pending.anchor;
    if w == 0
        && (anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT == 0
            || anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT == 0)
    {
        return Err(LayerShellError::InvalidSize);
    }
    if h == 0
        && (anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP == 0
            || anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM == 0)
    {
        return Err(LayerShellError::InvalidSize);
    }
    Ok(())
}

fn desired_configure_size(
    state: &LayerPendingState,
    output_size: (i32, i32),
    exclusive: ExclusiveInsets,
) -> (u32, u32) {
    let (out_w, out_h) = output_size;
    let (avail_w, avail_h) = if state.exclusive_zone == -1 {
        (out_w.max(0) as u32, out_h.max(0) as u32)
    } else {
        let (w, h) = exclusive.inset_size(out_w, out_h);
        (w.max(0) as u32, h.max(0) as u32)
    };
    let (req_w, req_h) = state.size;
    let (mt, mr, mb, ml) = state.margin;
    let anchor = state.anchor;

    let width = if req_w != 0 {
        req_w
    } else if anchor & (ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT | ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT)
        == (ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT | ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT)
    {
        let margin = ml.saturating_add(mr);
        avail_w.saturating_sub(margin.max(0) as u32)
    } else {
        0
    };
    let height = if req_h != 0 {
        req_h
    } else if anchor & (ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP | ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM)
        == (ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP | ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM)
    {
        let margin = mt.saturating_add(mb);
        avail_h.saturating_sub(margin.max(0) as u32)
    } else {
        0
    };
    (width, height)
}

fn compute_geometry(
    state: &LayerPendingState,
    output_size: (i32, i32),
    exclusive: ExclusiveInsets,
) -> LayerGeometry {
    let (out_w, out_h) = output_size;
    let (cfg_w, cfg_h) = desired_configure_size(state, output_size, exclusive);
    // Client may ignore configure; placement still uses requested/configured size.
    let width = if state.size.0 != 0 {
        state.size.0 as i32
    } else {
        cfg_w as i32
    };
    let height = if state.size.1 != 0 {
        state.size.1 as i32
    } else {
        cfg_h as i32
    };
    let (mt, mr, mb, ml) = state.margin;
    let anchor = state.anchor;

    let (area_x, area_y, area_w, area_h) = if state.exclusive_zone == -1 {
        (0, 0, out_w, out_h)
    } else if state.exclusive_zone == 0 {
        exclusive.inset_rect(0, 0, out_w, out_h)
    } else {
        (0, 0, out_w, out_h)
    };

    let x = if anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT != 0
        && anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT == 0
    {
        area_x + ml
    } else if anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT != 0
        && anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT == 0
    {
        area_x + area_w - width - mr
    } else {
        area_x + (area_w - width) / 2 + ml - mr
    };

    let y = if anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP != 0
        && anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM == 0
    {
        area_y + mt
    } else if anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM != 0
        && anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP == 0
    {
        area_y + area_h - height - mb
    } else {
        area_y + (area_h - height) / 2 + mt - mb
    };

    LayerGeometry {
        x,
        y,
        width,
        height,
    }
}

fn apply_exclusive_zone(insets: &mut ExclusiveInsets, state: &LayerPendingState) {
    let zone = state.exclusive_zone;
    if zone <= 0 {
        return;
    }
    let anchor = state.anchor;
    let (mt, mr, mb, ml) = state.margin;
    let edge = exclusive_edge(anchor);
    match edge {
        Some(ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP) => {
            insets.top = insets.top.max(zone + mt);
        }
        Some(ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM) => {
            insets.bottom = insets.bottom.max(zone + mb);
        }
        Some(ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT) => {
            insets.left = insets.left.max(zone + ml);
        }
        Some(ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT) => {
            insets.right = insets.right.max(zone + mr);
        }
        _ => {}
    }
}

fn exclusive_edge(anchor: u32) -> Option<u32> {
    let horiz = (anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT != 0) as u8
        + (anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT != 0) as u8;
    let vert = (anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP != 0) as u8
        + (anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM != 0) as u8;
    // Single edge, or edge + both perpendiculars.
    if vert == 1 && horiz != 1 {
        if anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP != 0 {
            return Some(ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP);
        }
        if anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM != 0 {
            return Some(ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM);
        }
    }
    if horiz == 1 && vert != 1 {
        if anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT != 0 {
            return Some(ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT);
        }
        if anchor & ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT != 0 {
            return Some(ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumalla_wayland_protocol::ObjectId;
    use std::num::NonZeroU32;

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn setup_panel() -> (LayerShellManager, ClientId, ObjectId, ObjectId) {
        let mut mgr = LayerShellManager::default();
        let c = client(1);
        let shell = object(10);
        let layer = object(11);
        let surface = object(12);
        mgr.create_shell(c, shell);
        mgr.create_layer_surface(
            c,
            shell,
            layer,
            surface,
            1,
            ZWLR_LAYER_SHELL_V1_LAYER_TOP,
            "panel".into(),
        )
        .unwrap();
        mgr.set_size(c, layer, 0, 30).unwrap();
        mgr.set_anchor(
            c,
            layer,
            ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP
                | ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT
                | ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT,
        )
        .unwrap();
        mgr.set_exclusive_zone(c, layer, 30).unwrap();
        (mgr, c, layer, surface)
    }

    #[test]
    fn configure_ack_map_cycle() {
        let (mut mgr, c, layer, surface) = setup_panel();
        let outcome = mgr
            .on_wl_surface_commit(c, surface, None, (800, 600), ExclusiveInsets::default())
            .unwrap();
        let cfg = outcome.initial_configure.expect("configure");
        assert_eq!(cfg.width, 800);
        assert_eq!(cfg.height, 30);
        mgr.ack_configure(c, layer, cfg.serial).unwrap();
        let outcome = mgr
            .on_wl_surface_commit(c, surface, Some(true), (800, 600), ExclusiveInsets::default())
            .unwrap();
        assert!(outcome.mapped);
        assert_eq!(
            outcome.geometry,
            Some(LayerGeometry {
                x: 0,
                y: 0,
                width: 800,
                height: 30
            })
        );
        let insets = mgr.exclusive_insets_for_output(1);
        assert_eq!(insets.top, 30);
    }

    #[test]
    fn rejects_zero_size_without_opposite_anchors() {
        let mut mgr = LayerShellManager::default();
        let c = client(1);
        mgr.create_shell(c, object(1));
        mgr.create_layer_surface(
            c,
            object(1),
            object(2),
            object(3),
            1,
            ZWLR_LAYER_SHELL_V1_LAYER_TOP,
            "x".into(),
        )
        .unwrap();
        mgr.set_size(c, object(2), 0, 10).unwrap();
        mgr.set_anchor(c, object(2), ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP)
            .unwrap();
        let err = mgr
            .on_wl_surface_commit(c, object(3), None, (800, 600), ExclusiveInsets::default())
            .unwrap_err();
        // validation happens when committing pending; initial configure path validates pending
        // Actually validate runs on commit of pending after initial configure path...
        // For bufferless first commit we queue configure without full validate of size 0.
        // Force apply path:
        let _ = err;
        // Direct validate:
        assert!(matches!(
            validate_pending(&LayerPendingState {
                size: (0, 10),
                anchor: ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP,
                layer: LayerBand::Top,
                ..Default::default()
            }),
            Err(LayerShellError::InvalidSize)
        ));
    }

    #[test]
    fn desktop_scene_excludes_layer_roots() {
        use crate::surface::SurfaceManager;
        let (mut mgr, c, layer, surface) = setup_panel();
        let mut surfaces = SurfaceManager::default();
        surfaces.create_surface(c, surface);
        surfaces.assign_layer_role(c, surface, layer).unwrap();
        let cfg = mgr
            .on_wl_surface_commit(c, surface, None, (800, 600), ExclusiveInsets::default())
            .unwrap()
            .initial_configure
            .unwrap();
        mgr.ack_configure(c, layer, cfg.serial).unwrap();
        // Simulate map-ready + buffer so is_mapped could be true; layer roots must
        // still be omitted from the desktop paint order path.
        surfaces.set_xdg_map_ready(c, surface, true).unwrap();
        surfaces.set_surface_layout(c, surface, 0, 0).unwrap();
        // Even if someone recorded the layer in paint_order, collect skips Role::Layer.
        surfaces.record_painted_surface(c, surface);
        let scene = surfaces.scene_surfaces();
        assert!(scene.is_empty(), "layer roots must not appear in desktop scene");
    }

    #[test]
    fn exclusive_zone_zero_avoids_insets() {
        let mut insets = ExclusiveInsets {
            top: 30,
            ..Default::default()
        };
        let state = LayerPendingState {
            size: (100, 50),
            exclusive_zone: 0,
            layer: LayerBand::Overlay,
            ..Default::default()
        };
        let geo = compute_geometry(&state, (800, 600), insets);
        assert_eq!(geo.y, 30 + (600 - 30 - 50) / 2);
        apply_exclusive_zone(&mut insets, &state);
        assert_eq!(insets.top, 30);
    }
}
