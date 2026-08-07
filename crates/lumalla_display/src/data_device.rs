use std::{
    collections::HashMap,
    os::unix::io::RawFd,
};

use lumalla_wayland_protocol::{
    ClientId, ObjectId,
    buffer::Writer,
    protocols::wayland::{
        WL_DATA_DEVICE_MANAGER_DND_ACTION_ASK, WL_DATA_DEVICE_MANAGER_DND_ACTION_COPY,
        WL_DATA_DEVICE_MANAGER_DND_ACTION_MOVE, WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE,
    },
    registry::{InterfaceIndex, Registry},
};

const VALID_ACTIONS_MASK: u32 = WL_DATA_DEVICE_MANAGER_DND_ACTION_COPY
    | WL_DATA_DEVICE_MANAGER_DND_ACTION_MOVE
    | WL_DATA_DEVICE_MANAGER_DND_ACTION_ASK;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDeviceError {
    UnknownSource,
    UnknownDevice,
    UnknownOffer,
    UsedSource,
    InvalidActionMask,
    InvalidAction,
    InvalidFinish,
    InvalidOffer,
    InvalidSource,
    UnknownSeat,
    UnknownSurface,
    RoleConflict,
}

impl std::fmt::Display for DataDeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSource => write!(f, "Unknown wl_data_source"),
            Self::UnknownDevice => write!(f, "Unknown wl_data_device"),
            Self::UnknownOffer => write!(f, "Unknown wl_data_offer"),
            Self::UsedSource => write!(f, "wl_data_source has already been used"),
            Self::InvalidActionMask => write!(f, "Invalid drag-and-drop action mask"),
            Self::InvalidAction => write!(f, "Invalid drag-and-drop action"),
            Self::InvalidFinish => write!(f, "finish called on a non-drag offer"),
            Self::InvalidOffer => write!(f, "Request not valid for this offer"),
            Self::InvalidSource => write!(f, "Request not valid for this source"),
            Self::UnknownSeat => write!(f, "Unknown wl_seat"),
            Self::UnknownSurface => write!(f, "Unknown wl_surface"),
            Self::RoleConflict => write!(f, "Surface already has a role"),
        }
    }
}

#[derive(Debug, Default)]
pub struct DataDeviceManager {
    sources: HashMap<(ClientId, ObjectId), DataSource>,
    devices: HashMap<(ClientId, ObjectId), DataDevice>,
    offers: HashMap<(ClientId, ObjectId), DataOffer>,
    selection: Option<Selection>,
    drag: Option<ActiveDrag>,
}

#[derive(Debug)]
struct DataSource {
    mime_types: Vec<String>,
    dnd_actions: u32,
    actions_set: bool,
    used: bool,
    version: u32,
}

#[derive(Debug)]
struct DataDevice {
    #[allow(dead_code)]
    seat: ObjectId,
    version: u32,
    selection_offer: Option<ObjectId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfferKind {
    Selection,
    Drag,
}

#[derive(Debug)]
struct DataOffer {
    device: ObjectId,
    source: Option<(ClientId, ObjectId)>,
    mime_types: Vec<String>,
    kind: OfferKind,
    accepted_mime: Option<Option<String>>,
    dnd_actions: u32,
    preferred_action: u32,
    selected_action: u32,
    source_actions: u32,
    finished: bool,
    version: u32,
}

#[derive(Debug, Clone)]
struct Selection {
    source_client: ClientId,
    source: ObjectId,
    #[allow(dead_code)]
    serial: u32,
}

#[derive(Debug)]
struct ActiveDrag {
    source: Option<(ClientId, ObjectId)>,
    origin_client: ClientId,
    #[allow(dead_code)]
    origin: ObjectId,
    icon: Option<(ClientId, ObjectId)>,
    #[allow(dead_code)]
    serial: u32,
    source_actions: u32,
    device: ObjectId,
    target_surface: Option<ObjectId>,
    target_offer: Option<ObjectId>,
    drop_performed: bool,
}

impl DataDeviceManager {
    pub fn create_data_source(&mut self, client_id: ClientId, id: ObjectId, version: u32) {
        self.sources.insert(
            (client_id, id),
            DataSource {
                mime_types: Vec::new(),
                dnd_actions: WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE,
                actions_set: false,
                used: false,
                version,
            },
        );
    }

    pub fn create_data_device(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        seat: ObjectId,
        version: u32,
        registry: &mut Registry,
        writer: &mut Writer,
    ) {
        self.devices.insert(
            (client_id, id),
            DataDevice {
                seat,
                version,
                selection_offer: None,
            },
        );
        if let Some(selection) = self.selection.clone() {
            let _ = self.send_selection_to_device(
                client_id,
                id,
                Some((selection.source_client, selection.source)),
                registry,
                writer,
            );
        }
    }

    pub fn offer(
        &mut self,
        client_id: ClientId,
        source_id: ObjectId,
        mime_type: &str,
    ) -> Result<(), DataDeviceError> {
        let source = self
            .sources
            .get_mut(&(client_id, source_id))
            .ok_or(DataDeviceError::UnknownSource)?;
        if !source.mime_types.iter().any(|m| m == mime_type) {
            source.mime_types.push(mime_type.to_owned());
        }
        Ok(())
    }

    pub fn set_source_actions(
        &mut self,
        client_id: ClientId,
        source_id: ObjectId,
        dnd_actions: u32,
    ) -> Result<(), DataDeviceError> {
        if !is_valid_action_mask(dnd_actions) {
            return Err(DataDeviceError::InvalidActionMask);
        }
        let source = self
            .sources
            .get_mut(&(client_id, source_id))
            .ok_or(DataDeviceError::UnknownSource)?;
        if source.used || source.actions_set {
            return Err(DataDeviceError::InvalidSource);
        }
        source.dnd_actions = dnd_actions;
        source.actions_set = true;
        Ok(())
    }

    pub fn destroy_source(
        &mut self,
        client_id: ClientId,
        source_id: ObjectId,
        writer: &mut Writer,
    ) -> Result<(), DataDeviceError> {
        if self.sources.remove(&(client_id, source_id)).is_none() {
            return Err(DataDeviceError::UnknownSource);
        }
        if self
            .selection
            .as_ref()
            .is_some_and(|s| s.source_client == client_id && s.source == source_id)
        {
            self.selection = None;
            self.clear_selection_offers(client_id, writer);
        }
        if let Some(drag) = self.drag.as_ref()
            && drag.source == Some((client_id, source_id))
        {
            self.cancel_drag(writer);
        }
        Ok(())
    }

    pub fn set_selection(
        &mut self,
        client_id: ClientId,
        device_id: ObjectId,
        source: Option<ObjectId>,
        serial: u32,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> Result<(), DataDeviceError> {
        if !self.devices.contains_key(&(client_id, device_id)) {
            return Err(DataDeviceError::UnknownDevice);
        }

        let new_source = match source {
            Some(source_id) => {
                let source_state = self
                    .sources
                    .get_mut(&(client_id, source_id))
                    .ok_or(DataDeviceError::UnknownSource)?;
                if source_state.used {
                    return Err(DataDeviceError::UsedSource);
                }
                if source_state.actions_set {
                    return Err(DataDeviceError::InvalidSource);
                }
                source_state.used = true;
                Some((client_id, source_id))
            }
            None => None,
        };

        if let Some(previous) = self.selection.take() {
            let same = new_source == Some((previous.source_client, previous.source));
            if !same && previous.source_client == client_id {
                writer.wl_data_source_cancelled(previous.source);
            }
        }

        self.selection = new_source.map(|(source_client, source_id)| Selection {
            source_client,
            source: source_id,
            serial,
        });

        let device_ids: Vec<ObjectId> = self
            .devices
            .iter()
            .filter_map(|((owner, id), _)| (*owner == client_id).then_some(*id))
            .collect();
        for id in device_ids {
            self.send_selection_to_device(client_id, id, new_source, registry, writer)?;
        }
        Ok(())
    }

    pub fn start_drag(
        &mut self,
        client_id: ClientId,
        device_id: ObjectId,
        source: Option<ObjectId>,
        origin: ObjectId,
        icon: Option<ObjectId>,
        serial: u32,
        target_surface: Option<ObjectId>,
        pointer_x: f32,
        pointer_y: f32,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> Result<(), DataDeviceError> {
        if !self.devices.contains_key(&(client_id, device_id)) {
            return Err(DataDeviceError::UnknownDevice);
        }

        let (source_key, source_actions, mime_types, source_version) = match source {
            Some(source_id) => {
                let source_state = self
                    .sources
                    .get_mut(&(client_id, source_id))
                    .ok_or(DataDeviceError::UnknownSource)?;
                if source_state.used {
                    return Err(DataDeviceError::UsedSource);
                }
                source_state.used = true;
                (
                    Some((client_id, source_id)),
                    source_state.dnd_actions,
                    source_state.mime_types.clone(),
                    source_state.version,
                )
            }
            None => (
                None,
                WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE,
                Vec::new(),
                1,
            ),
        };

        if self.drag.is_some() {
            self.cancel_drag(writer);
        }

        let device_version = self.devices[&(client_id, device_id)].version;
        let offer_version = device_version.min(source_version.max(1));
        let mut target_offer = None;
        if let Some(surface) = target_surface {
            let offer_id = if source_key.is_some() {
                Some(self.create_offer(
                    client_id,
                    device_id,
                    source_key,
                    mime_types,
                    OfferKind::Drag,
                    source_actions,
                    offer_version,
                    registry,
                    writer,
                )?)
            } else {
                None
            };
            let enter_serial = serial.wrapping_add(1).max(1);
            writer
                .wl_data_device_enter(device_id)
                .serial(enter_serial)
                .surface(surface)
                .x(pointer_x)
                .y(pointer_y)
                .id(offer_id);
            if let Some((_, source_id)) = source_key {
                self.emit_source_action(client_id, source_id, writer);
            }
            target_offer = offer_id;
        }

        self.drag = Some(ActiveDrag {
            source: source_key,
            origin_client: client_id,
            origin,
            icon: icon.map(|id| (client_id, id)),
            serial,
            source_actions,
            device: device_id,
            target_surface,
            target_offer,
            drop_performed: false,
        });
        Ok(())
    }

    pub fn drag_motion(
        &mut self,
        client_id: ClientId,
        time_msec: u32,
        x: f32,
        y: f32,
        surface: Option<ObjectId>,
        registry: &mut Registry,
        writer: &mut Writer,
    ) {
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        if drag.origin_client != client_id || drag.drop_performed {
            return;
        }
        let device_id = drag.device;
        let same_surface = drag.target_surface == surface;
        let source_key = drag.source;
        let source_actions = drag.source_actions;
        let serial = drag.serial;
        let old_offer = drag.target_offer;

        if same_surface {
            if surface.is_some() {
                writer
                    .wl_data_device_motion(device_id)
                    .time(time_msec)
                    .x(x)
                    .y(y);
            }
            return;
        }

        if drag.target_surface.is_some() {
            writer.wl_data_device_leave(device_id);
            if let Some(offer_id) = old_offer {
                self.offers.remove(&(client_id, offer_id));
            }
            if let Some(drag) = self.drag.as_mut() {
                drag.target_surface = None;
                drag.target_offer = None;
            }
        }

        let Some(surface) = surface else {
            return;
        };

        let mime_types = source_key
            .and_then(|key| self.sources.get(&key))
            .map(|s| s.mime_types.clone())
            .unwrap_or_default();
        let version = self
            .devices
            .get(&(client_id, device_id))
            .map(|d| d.version)
            .unwrap_or(1);
        let offer_id = if source_key.is_some() {
            self.create_offer(
                client_id,
                device_id,
                source_key,
                mime_types,
                OfferKind::Drag,
                source_actions,
                version,
                registry,
                writer,
            )
            .ok()
        } else {
            None
        };
        let enter_serial = serial.wrapping_add(1).max(1);
        writer
            .wl_data_device_enter(device_id)
            .serial(enter_serial)
            .surface(surface)
            .x(x)
            .y(y)
            .id(offer_id);
        if let Some(drag) = self.drag.as_mut() {
            drag.target_surface = Some(surface);
            drag.target_offer = offer_id;
        }
    }

    pub fn drag_drop(&mut self, client_id: ClientId, writer: &mut Writer) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if drag.origin_client != client_id || drag.drop_performed {
            return;
        }
        drag.drop_performed = true;
        let device_id = drag.device;
        if drag.target_surface.is_some() {
            writer.wl_data_device_drop(device_id);
        } else {
            writer.wl_data_device_leave(device_id);
        }
        if let Some((_, source_id)) = drag.source {
            if let Some(source) = self.sources.get(&(client_id, source_id)) {
                if source.version >= 3 {
                    writer.wl_data_source_dnd_drop_performed(source_id);
                }
            }
        }
        if drag.target_surface.is_none() {
            if let Some((_, source_id)) = drag.source {
                writer.wl_data_source_cancelled(source_id);
            }
            self.drag = None;
        }
    }

    pub fn accept(
        &mut self,
        client_id: ClientId,
        offer_id: ObjectId,
        _serial: u32,
        mime_type: Option<&str>,
        writer: &mut Writer,
    ) -> Result<(), DataDeviceError> {
        let offer = self
            .offers
            .get_mut(&(client_id, offer_id))
            .ok_or(DataDeviceError::UnknownOffer)?;
        if offer.finished {
            return Err(DataDeviceError::InvalidOffer);
        }
        offer.accepted_mime = Some(mime_type.map(str::to_owned));
        if offer.kind == OfferKind::Drag
            && let Some((source_client, source_id)) = offer.source
            && source_client == client_id
        {
            writer
                .wl_data_source_target(source_id)
                .mime_type(mime_type);
        }
        Ok(())
    }

    pub fn receive(
        &mut self,
        client_id: ClientId,
        offer_id: ObjectId,
        mime_type: &str,
        fd: RawFd,
        writer: &mut Writer,
    ) -> Result<(), DataDeviceError> {
        let offer = self
            .offers
            .get(&(client_id, offer_id))
            .ok_or(DataDeviceError::UnknownOffer)?;
        if offer.finished {
            unsafe {
                libc::close(fd);
            }
            return Err(DataDeviceError::InvalidOffer);
        }
        let Some((source_client, source_id)) = offer.source else {
            unsafe {
                libc::close(fd);
            }
            return Ok(());
        };
        if !offer.mime_types.iter().any(|m| m == mime_type) {
            unsafe {
                libc::close(fd);
            }
            return Ok(());
        }
        if source_client != client_id {
            // Cross-client send needs the source client's writer; drop the FD.
            unsafe {
                libc::close(fd);
            }
            return Ok(());
        }

        // Compositor-mediated pipe: write end to the source, and bridge the read
        // end into the FD supplied by the destination (`receive`). Without an
        // async pump we forward the destination FD directly — equivalent for the
        // single-client MVP and correct Wayland semantics.
        forward_receive_to_source(writer, source_id, mime_type, fd);
        Ok(())
    }

    pub fn destroy_offer(
        &mut self,
        client_id: ClientId,
        offer_id: ObjectId,
    ) -> Result<(), DataDeviceError> {
        let offer = self
            .offers
            .remove(&(client_id, offer_id))
            .ok_or(DataDeviceError::UnknownOffer)?;
        if let Some(device) = self.devices.get_mut(&(client_id, offer.device))
            && device.selection_offer == Some(offer_id)
        {
            device.selection_offer = None;
        }
        if let Some(drag) = self.drag.as_mut()
            && drag.target_offer == Some(offer_id)
        {
            drag.target_offer = None;
        }
        Ok(())
    }

    pub fn finish(
        &mut self,
        client_id: ClientId,
        offer_id: ObjectId,
        writer: &mut Writer,
    ) -> Result<(), DataDeviceError> {
        let offer = self
            .offers
            .get_mut(&(client_id, offer_id))
            .ok_or(DataDeviceError::UnknownOffer)?;
        if offer.kind != OfferKind::Drag {
            return Err(DataDeviceError::InvalidFinish);
        }
        if offer.finished {
            return Err(DataDeviceError::InvalidOffer);
        }
        if matches!(offer.accepted_mime, Some(None)) || offer.selected_action == 0 {
            return Err(DataDeviceError::InvalidFinish);
        }
        offer.finished = true;
        if let Some((source_client, source_id)) = offer.source
            && source_client == client_id
            && let Some(source) = self.sources.get(&(source_client, source_id))
            && source.version >= 3
        {
            writer.wl_data_source_dnd_finished(source_id);
        }
        self.drag = None;
        Ok(())
    }

    pub fn set_offer_actions(
        &mut self,
        client_id: ClientId,
        offer_id: ObjectId,
        dnd_actions: u32,
        preferred_action: u32,
        writer: &mut Writer,
    ) -> Result<(), DataDeviceError> {
        if !is_valid_action_mask(dnd_actions) {
            return Err(DataDeviceError::InvalidActionMask);
        }
        if !is_valid_single_action(preferred_action) {
            return Err(DataDeviceError::InvalidAction);
        }
        if preferred_action != WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE
            && (dnd_actions & preferred_action) == 0
        {
            return Err(DataDeviceError::InvalidAction);
        }
        let offer = self
            .offers
            .get_mut(&(client_id, offer_id))
            .ok_or(DataDeviceError::UnknownOffer)?;
        if offer.kind != OfferKind::Drag {
            return Err(DataDeviceError::InvalidOffer);
        }
        if offer.finished {
            return Err(DataDeviceError::InvalidOffer);
        }
        if preferred_action != WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE
            && (offer.source_actions & preferred_action) == 0
        {
            return Err(DataDeviceError::InvalidAction);
        }
        offer.dnd_actions = dnd_actions;
        offer.preferred_action = preferred_action;
        offer.selected_action = negotiate_action(offer.source_actions, dnd_actions, preferred_action);
        let selected = offer.selected_action;
        let source = offer.source;
        if offer.version >= 3 {
            writer
                .wl_data_offer_action(offer_id)
                .dnd_action(selected);
        }
        if let Some((source_client, source_id)) = source
            && source_client == client_id
            && let Some(source_state) = self.sources.get(&(source_client, source_id))
            && source_state.version >= 3
        {
            writer
                .wl_data_source_action(source_id)
                .dnd_action(selected);
        }
        Ok(())
    }

    pub fn release_device(
        &mut self,
        client_id: ClientId,
        device_id: ObjectId,
    ) -> Result<(), DataDeviceError> {
        if self.devices.remove(&(client_id, device_id)).is_none() {
            return Err(DataDeviceError::UnknownDevice);
        }
        self.offers
            .retain(|(owner, _), offer| !(*owner == client_id && offer.device == device_id));
        if let Some(drag) = &self.drag
            && drag.origin_client == client_id
            && drag.device == device_id
        {
            self.drag = None;
        }
        Ok(())
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        self.sources.retain(|(owner, _), _| *owner != client_id);
        self.devices.retain(|(owner, _), _| *owner != client_id);
        self.offers.retain(|(owner, _), _| *owner != client_id);
        if self
            .selection
            .as_ref()
            .is_some_and(|s| s.source_client == client_id)
        {
            self.selection = None;
        }
        if self
            .drag
            .as_ref()
            .is_some_and(|d| d.origin_client == client_id)
        {
            self.drag = None;
        }
    }

    #[cfg(test)]
    pub fn selection_source(&self) -> Option<(ClientId, ObjectId)> {
        self.selection
            .as_ref()
            .map(|s| (s.source_client, s.source))
    }

    #[cfg(test)]
    pub fn has_offer(&self, client_id: ClientId, offer_id: ObjectId) -> bool {
        self.offers.contains_key(&(client_id, offer_id))
    }

    #[cfg(test)]
    pub fn selection_offer(
        &self,
        client_id: ClientId,
        device_id: ObjectId,
    ) -> Option<ObjectId> {
        self.devices
            .get(&(client_id, device_id))
            .and_then(|d| d.selection_offer)
    }

    pub fn active_drag_icon(&self) -> Option<(ClientId, ObjectId)> {
        self.drag.as_ref().and_then(|d| d.icon)
    }

    fn send_selection_to_device(
        &mut self,
        client_id: ClientId,
        device_id: ObjectId,
        source: Option<(ClientId, ObjectId)>,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> Result<(), DataDeviceError> {
        let version = self
            .devices
            .get(&(client_id, device_id))
            .map(|d| d.version)
            .ok_or(DataDeviceError::UnknownDevice)?;

        let offer_id = match source {
            Some((source_client, source_id)) => {
                let mime_types = self
                    .sources
                    .get(&(source_client, source_id))
                    .map(|s| s.mime_types.clone())
                    .unwrap_or_default();
                let offer = self.create_offer(
                    client_id,
                    device_id,
                    Some((source_client, source_id)),
                    mime_types,
                    OfferKind::Selection,
                    WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE,
                    version,
                    registry,
                    writer,
                )?;
                if let Some(device) = self.devices.get_mut(&(client_id, device_id)) {
                    device.selection_offer = Some(offer);
                }
                Some(offer)
            }
            None => {
                if let Some(device) = self.devices.get_mut(&(client_id, device_id)) {
                    device.selection_offer = None;
                }
                None
            }
        };
        writer.wl_data_device_selection(device_id).id(offer_id);
        Ok(())
    }

    fn create_offer(
        &mut self,
        client_id: ClientId,
        device_id: ObjectId,
        source: Option<(ClientId, ObjectId)>,
        mime_types: Vec<String>,
        kind: OfferKind,
        source_actions: u32,
        version: u32,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> Result<ObjectId, DataDeviceError> {
        let offer_id = registry
            .create_object(InterfaceIndex::WlDataOffer, version.max(1))
            .map_err(|_| DataDeviceError::UnknownOffer)?;
        writer.wl_data_device_data_offer(device_id).id(offer_id);
        for mime in &mime_types {
            writer.wl_data_offer_offer(offer_id).mime_type(mime);
        }
        let selected = if kind == OfferKind::Drag {
            negotiate_action(
                source_actions,
                source_actions,
                first_action(source_actions),
            )
        } else {
            WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE
        };
        if kind == OfferKind::Drag && version >= 3 {
            writer
                .wl_data_offer_source_actions(offer_id)
                .source_actions(source_actions);
            writer
                .wl_data_offer_action(offer_id)
                .dnd_action(selected);
        }
        self.offers.insert(
            (client_id, offer_id),
            DataOffer {
                device: device_id,
                source,
                mime_types,
                kind,
                accepted_mime: None,
                dnd_actions: source_actions,
                preferred_action: first_action(source_actions),
                selected_action: selected,
                source_actions,
                finished: false,
                version,
            },
        );
        Ok(offer_id)
    }

    fn emit_source_action(&self, client_id: ClientId, source_id: ObjectId, writer: &mut Writer) {
        let Some(source) = self.sources.get(&(client_id, source_id)) else {
            return;
        };
        if source.version < 3 {
            return;
        }
        let action = first_action(source.dnd_actions);
        writer
            .wl_data_source_action(source_id)
            .dnd_action(action);
    }

    fn clear_selection_offers(&mut self, client_id: ClientId, writer: &mut Writer) {
        let device_ids: Vec<ObjectId> = self
            .devices
            .iter()
            .filter_map(|((owner, id), _)| (*owner == client_id).then_some(*id))
            .collect();
        for device_id in device_ids {
            if let Some(device) = self.devices.get_mut(&(client_id, device_id)) {
                device.selection_offer = None;
            }
            writer.wl_data_device_selection(device_id).id(None);
        }
    }

    fn cancel_drag(&mut self, writer: &mut Writer) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.target_surface.is_some() {
            writer.wl_data_device_leave(drag.device);
        }
        if let Some((source_client, source_id)) = drag.source
            && source_client == drag.origin_client
        {
            writer.wl_data_source_cancelled(source_id);
        }
    }
}

fn is_valid_action_mask(actions: u32) -> bool {
    actions & !VALID_ACTIONS_MASK == 0
}

fn is_valid_single_action(action: u32) -> bool {
    matches!(
        action,
        WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE
            | WL_DATA_DEVICE_MANAGER_DND_ACTION_COPY
            | WL_DATA_DEVICE_MANAGER_DND_ACTION_MOVE
            | WL_DATA_DEVICE_MANAGER_DND_ACTION_ASK
    )
}

fn first_action(actions: u32) -> u32 {
    for candidate in [
        WL_DATA_DEVICE_MANAGER_DND_ACTION_COPY,
        WL_DATA_DEVICE_MANAGER_DND_ACTION_MOVE,
        WL_DATA_DEVICE_MANAGER_DND_ACTION_ASK,
    ] {
        if actions & candidate != 0 {
            return candidate;
        }
    }
    WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE
}

fn negotiate_action(source_actions: u32, dest_actions: u32, preferred: u32) -> u32 {
    let available = source_actions & dest_actions;
    if preferred != WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE && available & preferred != 0 {
        return preferred;
    }
    first_action(available)
}

/// Forward a client-provided receive FD to the data source via `wl_data_source.send`.
pub fn forward_receive_to_source(
    writer: &mut Writer,
    source_id: ObjectId,
    mime_type: &str,
    fd: RawFd,
) {
    writer
        .wl_data_source_send(source_id)
        .mime_type(mime_type)
        .fd(fd);
    if writer.flush().is_ok() && !writer.has_pending_output() {
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Read,
        num::NonZeroU32,
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    use lumalla_wayland_protocol::{
        ClientId, ObjectId,
        buffer::Writer,
        registry::Registry,
    };

    use super::*;

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    #[test]
    fn selection_round_trip_creates_offer_and_send() {
        let (mut receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut registry = Registry::new();
        let mut manager = DataDeviceManager::default();
        let client_id = client(1);
        let source = object(10);
        let device = object(11);
        let seat = object(12);

        manager.create_data_source(client_id, source, 3);
        manager.offer(client_id, source, "text/plain").unwrap();
        manager.create_data_device(client_id, device, seat, 3, &mut registry, &mut writer);
        manager
            .set_selection(
                client_id,
                device,
                Some(source),
                1,
                &mut registry,
                &mut writer,
            )
            .unwrap();
        writer.flush().unwrap();

        assert_eq!(
            manager.selection_source(),
            Some((client_id, source))
        );
        let offer_id = manager
            .selection_offer(client_id, device)
            .expect("selection should create an offer");
        assert!(manager.has_offer(client_id, offer_id));

        let mut pipe_fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

        // Drain advertisement events so the next messages are the send event.
        let mut drain = [0u8; 4096];
        let _ = receiver.set_nonblocking(true);
        while receiver.read(&mut drain).is_ok() {}
        let _ = receiver.set_nonblocking(false);

        manager
            .receive(client_id, offer_id, "text/plain", write_fd, &mut writer)
            .unwrap();

        // wl_data_source.send: object=source, opcode=1, mime string + fd
        let mut header = [0u8; 8];
        receiver.read_exact(&mut header).unwrap();
        let object_id = u32::from_ne_bytes(header[0..4].try_into().unwrap());
        let opcode = u16::from_ne_bytes(header[4..6].try_into().unwrap());
        let size = u16::from_ne_bytes(header[6..8].try_into().unwrap()) as usize;
        assert_eq!(object_id, source.get());
        assert_eq!(opcode, 1); // send
        let mut payload = vec![0u8; size - 8];
        receiver.read_exact(&mut payload).unwrap();
        unsafe {
            libc::close(read_fd);
        }
    }

    #[test]
    fn start_drag_creates_enter_offer() {
        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut registry = Registry::new();
        let mut manager = DataDeviceManager::default();
        let client_id = client(1);
        let source = object(10);
        let device = object(11);
        let seat = object(12);
        let origin = object(20);
        let target = object(21);

        manager.create_data_source(client_id, source, 3);
        manager.offer(client_id, source, "text/plain").unwrap();
        manager
            .set_source_actions(
                client_id,
                source,
                WL_DATA_DEVICE_MANAGER_DND_ACTION_COPY,
            )
            .unwrap();
        manager.create_data_device(client_id, device, seat, 3, &mut registry, &mut writer);
        manager
            .start_drag(
                client_id,
                device,
                Some(source),
                origin,
                None,
                7,
                Some(target),
                1.0,
                2.0,
                &mut registry,
                &mut writer,
            )
            .unwrap();
        assert!(manager.active_drag_icon().is_none());
        assert!(manager.drag.is_some());
        manager.drag_drop(client_id, &mut writer);
        assert!(manager.drag.as_ref().is_some_and(|d| d.drop_performed));
    }
}
