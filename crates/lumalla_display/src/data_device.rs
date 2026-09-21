use std::{collections::HashMap, os::unix::io::RawFd};

use lumalla_wayland_protocol::{
    ClientId, ObjectId,
    buffer::Writer,
    protocols::wayland::{
        WL_DATA_DEVICE_MANAGER_DND_ACTION_ASK, WL_DATA_DEVICE_MANAGER_DND_ACTION_COPY,
        WL_DATA_DEVICE_MANAGER_DND_ACTION_MOVE, WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE,
    },
    registry::{InterfaceIndex, Registry},
};

use crate::ConnectedClients;

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
    pending_selection_notifies: Vec<PendingSelectionNotify>,
    pending_source_cancels: Vec<PendingSourceCancel>,
    pending_source_sends: Vec<PendingSourceSend>,
    pending_source_events: Vec<PendingSourceEvent>,
    pending_drag_leaves: Vec<PendingDragLeave>,
    pending_drag_enters: Vec<PendingDragEnter>,
    pending_drag_motions: Vec<PendingDragMotion>,
    pending_drag_drops: Vec<PendingDragDrop>,
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
    origin_device: ObjectId,
    target_client: Option<ClientId>,
    target_device: Option<ObjectId>,
    target_surface: Option<ObjectId>,
    target_offer: Option<ObjectId>,
    drop_performed: bool,
}

#[derive(Debug, Clone)]
struct PendingSelectionNotify {
    client_id: ClientId,
    device_id: ObjectId,
    source: Option<(ClientId, ObjectId)>,
}

#[derive(Debug, Clone, Copy)]
struct PendingSourceCancel {
    client_id: ClientId,
    source_id: ObjectId,
}

#[derive(Debug)]
struct PendingSourceSend {
    client_id: ClientId,
    source_id: ObjectId,
    mime_type: String,
    fd: RawFd,
}

impl Drop for PendingSourceSend {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
            self.fd = -1;
        }
    }
}

#[derive(Debug, Clone)]
enum PendingSourceEvent {
    Target {
        client_id: ClientId,
        source_id: ObjectId,
        mime_type: Option<String>,
    },
    Action {
        client_id: ClientId,
        source_id: ObjectId,
        action: u32,
    },
    DndDropPerformed {
        client_id: ClientId,
        source_id: ObjectId,
    },
    DndFinished {
        client_id: ClientId,
        source_id: ObjectId,
    },
}

#[derive(Debug, Clone, Copy)]
struct PendingDragLeave {
    client_id: ClientId,
    device_id: ObjectId,
}

#[derive(Debug, Clone)]
struct PendingDragEnter {
    client_id: ClientId,
    device_id: ObjectId,
    surface: ObjectId,
    x: f32,
    y: f32,
    serial: u32,
    source: Option<(ClientId, ObjectId)>,
    source_actions: u32,
    mime_types: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct PendingDragMotion {
    client_id: ClientId,
    device_id: ObjectId,
    time_msec: u32,
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, Copy)]
struct PendingDragDrop {
    client_id: ClientId,
    device_id: ObjectId,
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
        registry: &mut Registry,
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
            self.advertise_selection(None, Some(client_id), registry, writer)?;
        }
        if let Some(drag) = self.drag.as_ref()
            && drag.source == Some((client_id, source_id))
        {
            self.cancel_drag(Some(client_id), writer);
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
            if !same {
                self.cancel_source(
                    previous.source_client,
                    previous.source,
                    Some(client_id),
                    writer,
                );
            }
        }

        self.selection = new_source.map(|(source_client, source_id)| Selection {
            source_client,
            source: source_id,
            serial,
        });

        self.advertise_selection(new_source, Some(client_id), registry, writer)?;
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
        target: Option<(ClientId, ObjectId)>,
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
            None => (None, WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE, Vec::new(), 1),
        };

        if self.drag.is_some() {
            self.cancel_drag(Some(client_id), writer);
        }

        let mut target_client = None;
        let mut target_device = None;
        let mut target_surface = None;
        let mut target_offer = None;

        if let Some((tid, surface)) = target {
            target_client = Some(tid);
            target_surface = Some(surface);
            let enter_serial = serial.wrapping_add(1).max(1);
            if tid == client_id {
                let device_version = self.devices[&(client_id, device_id)].version;
                let offer_version = device_version.min(source_version.max(1));
                let offer_id = if source_key.is_some() {
                    Some(self.create_offer(
                        client_id,
                        device_id,
                        source_key,
                        mime_types.clone(),
                        OfferKind::Drag,
                        source_actions,
                        offer_version,
                        registry,
                        writer,
                    )?)
                } else {
                    None
                };
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
                target_device = Some(device_id);
                target_offer = offer_id;
            } else if let Some(tdevice) = self.device_for_client(tid) {
                target_device = Some(tdevice);
                self.pending_drag_enters.push(PendingDragEnter {
                    client_id: tid,
                    device_id: tdevice,
                    surface,
                    x: pointer_x,
                    y: pointer_y,
                    serial: enter_serial,
                    source: source_key,
                    source_actions,
                    mime_types,
                });
                if let Some((_, source_id)) = source_key {
                    self.emit_source_action(client_id, source_id, writer);
                }
            }
        }

        self.drag = Some(ActiveDrag {
            source: source_key,
            origin_client: client_id,
            origin,
            icon: icon.map(|id| (client_id, id)),
            serial,
            source_actions,
            origin_device: device_id,
            target_client,
            target_device,
            target_surface,
            target_offer,
            drop_performed: false,
        });
        Ok(())
    }

    /// Drive drag motion from the compositor (has access to all client writers).
    pub fn drag_motion(
        &mut self,
        time_msec: u32,
        x: f32,
        y: f32,
        target: Option<(ClientId, ObjectId)>,
        clients: &mut ConnectedClients,
    ) {
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        if drag.drop_performed {
            return;
        }

        let same_surface = match (drag.target_client, drag.target_surface, target) {
            (Some(tc), Some(ts), Some((nc, ns))) => tc == nc && ts == ns,
            (None, None, None) => true,
            _ => false,
        };
        let source_key = drag.source;
        let source_actions = drag.source_actions;
        let serial = drag.serial;
        let old_target_client = drag.target_client;
        let old_target_device = drag.target_device;
        let old_offer = drag.target_offer;
        let had_target = drag.target_surface.is_some();

        if same_surface {
            if let (Some(tc), Some(tdevice)) = (old_target_client, old_target_device)
                && let Some(client) = clients.get_mut(&tc)
            {
                client
                    .writer_mut()
                    .wl_data_device_motion(tdevice)
                    .time(time_msec)
                    .x(x)
                    .y(y);
                clients.mark_send_needed(tc);
            }
            return;
        }

        if had_target
            && let (Some(tc), Some(tdevice)) = (old_target_client, old_target_device)
        {
            if let Some(client) = clients.get_mut(&tc) {
                client.writer_mut().wl_data_device_leave(tdevice);
                clients.mark_send_needed(tc);
            }
            if let Some(offer_id) = old_offer {
                self.offers.remove(&(tc, offer_id));
            }
            if let Some(drag) = self.drag.as_mut() {
                drag.target_client = None;
                drag.target_device = None;
                drag.target_surface = None;
                drag.target_offer = None;
            }
        }

        let Some((tid, surface)) = target else {
            return;
        };
        let Some(tdevice) = self.device_for_client(tid) else {
            return;
        };

        let mime_types = source_key
            .and_then(|key| self.sources.get(&key))
            .map(|s| s.mime_types.clone())
            .unwrap_or_default();
        let version = self
            .devices
            .get(&(tid, tdevice))
            .map(|d| d.version)
            .unwrap_or(1);
        let enter_serial = serial.wrapping_add(1).max(1);

        let Some(client) = clients.get_mut(&tid) else {
            return;
        };
        let (registry, writer) = client.registry_and_writer_mut();
        let offer_id = if source_key.is_some() {
            self.create_offer(
                tid,
                tdevice,
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
        writer
            .wl_data_device_enter(tdevice)
            .serial(enter_serial)
            .surface(surface)
            .x(x)
            .y(y)
            .id(offer_id);
        clients.mark_send_needed(tid);

        if let Some(drag) = self.drag.as_mut() {
            drag.target_client = Some(tid);
            drag.target_device = Some(tdevice);
            drag.target_surface = Some(surface);
            drag.target_offer = offer_id;
        }
    }

    pub fn drag_drop(&mut self, clients: &mut ConnectedClients) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if drag.drop_performed {
            return;
        }
        drag.drop_performed = true;
        let origin_client = drag.origin_client;
        let source = drag.source;
        let target_client = drag.target_client;
        let target_device = drag.target_device;
        let has_target = drag.target_surface.is_some();

        if has_target
            && let (Some(tc), Some(tdevice)) = (target_client, target_device)
            && let Some(client) = clients.get_mut(&tc)
        {
            client.writer_mut().wl_data_device_drop(tdevice);
            clients.mark_send_needed(tc);
        } else if let (Some(tc), Some(tdevice)) = (target_client, target_device)
            && let Some(client) = clients.get_mut(&tc)
        {
            client.writer_mut().wl_data_device_leave(tdevice);
            clients.mark_send_needed(tc);
        }

        if let Some((source_client, source_id)) = source {
            if let Some(source_state) = self.sources.get(&(source_client, source_id))
                && source_state.version >= 3
            {
                if source_client == origin_client {
                    if let Some(client) = clients.get_mut(&source_client) {
                        client
                            .writer_mut()
                            .wl_data_source_dnd_drop_performed(source_id);
                        clients.mark_send_needed(source_client);
                    }
                } else {
                    self.pending_source_events
                        .push(PendingSourceEvent::DndDropPerformed {
                            client_id: source_client,
                            source_id,
                        });
                }
            }
            if !has_target {
                if let Some(client) = clients.get_mut(&source_client) {
                    client.writer_mut().wl_data_source_cancelled(source_id);
                    clients.mark_send_needed(source_client);
                }
                self.drag = None;
            }
        } else if !has_target {
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
        {
            if source_client == client_id {
                writer.wl_data_source_target(source_id).mime_type(mime_type);
            } else {
                self.pending_source_events.push(PendingSourceEvent::Target {
                    client_id: source_client,
                    source_id,
                    mime_type: mime_type.map(str::to_owned),
                });
            }
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
            self.pending_source_sends.push(PendingSourceSend {
                client_id: source_client,
                source_id,
                mime_type: mime_type.to_owned(),
                fd,
            });
            return Ok(());
        }

        // Forward the destination FD directly to the source — correct Wayland
        // semantics while the FD lives in the compositor process.
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
            && drag.target_client == Some(client_id)
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
            && let Some(source) = self.sources.get(&(source_client, source_id))
            && source.version >= 3
        {
            if source_client == client_id {
                writer.wl_data_source_dnd_finished(source_id);
            } else {
                self.pending_source_events
                    .push(PendingSourceEvent::DndFinished {
                        client_id: source_client,
                        source_id,
                    });
            }
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
        offer.selected_action =
            negotiate_action(offer.source_actions, dnd_actions, preferred_action);
        let selected = offer.selected_action;
        let source = offer.source;
        if offer.version >= 3 {
            writer.wl_data_offer_action(offer_id).dnd_action(selected);
        }
        if let Some((source_client, source_id)) = source
            && let Some(source_state) = self.sources.get(&(source_client, source_id))
            && source_state.version >= 3
        {
            if source_client == client_id {
                writer.wl_data_source_action(source_id).dnd_action(selected);
            } else {
                self.pending_source_events.push(PendingSourceEvent::Action {
                    client_id: source_client,
                    source_id,
                    action: selected,
                });
            }
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
            && drag.origin_device == device_id
        {
            self.drag = None;
        }
        Ok(())
    }

    /// Drop a disconnected client and queue selection/drag cleanup for survivors.
    pub fn remove_client(&mut self, client_id: ClientId) {
        let was_selection_owner = self
            .selection
            .as_ref()
            .is_some_and(|s| s.source_client == client_id);
        let drag_target = self.drag.as_ref().and_then(|d| {
            (d.target_client == Some(client_id)).then_some((d.target_device, d.target_offer))
        });
        let was_drag_origin = self
            .drag
            .as_ref()
            .is_some_and(|d| d.origin_client == client_id);

        self.sources.retain(|(owner, _), _| *owner != client_id);
        self.devices.retain(|(owner, _), _| *owner != client_id);
        self.offers.retain(|(owner, _), _| *owner != client_id);

        self.pending_selection_notifies
            .retain(|n| n.client_id != client_id);
        self.pending_source_cancels
            .retain(|c| c.client_id != client_id);
        self.pending_source_sends
            .retain(|s| s.client_id != client_id);
        self.pending_source_events.retain(|e| match e {
            PendingSourceEvent::Target { client_id: c, .. }
            | PendingSourceEvent::Action { client_id: c, .. }
            | PendingSourceEvent::DndDropPerformed { client_id: c, .. }
            | PendingSourceEvent::DndFinished { client_id: c, .. } => *c != client_id,
        });
        self.pending_drag_leaves
            .retain(|l| l.client_id != client_id);
        self.pending_drag_enters
            .retain(|e| e.client_id != client_id);
        self.pending_drag_motions
            .retain(|m| m.client_id != client_id);
        self.pending_drag_drops
            .retain(|d| d.client_id != client_id);

        if was_selection_owner {
            self.selection = None;
            self.queue_selection_notifies(None);
        }

        if was_drag_origin {
            if let Some(drag) = self.drag.take()
                && let (Some(tc), Some(tdevice)) = (drag.target_client, drag.target_device)
                && tc != client_id
            {
                self.pending_drag_leaves.push(PendingDragLeave {
                    client_id: tc,
                    device_id: tdevice,
                });
            }
        } else if let Some((_tdevice, toffer)) = drag_target {
            if let Some(offer_id) = toffer {
                self.offers.remove(&(client_id, offer_id));
            }
            if let Some(drag) = self.drag.as_mut() {
                drag.target_client = None;
                drag.target_device = None;
                drag.target_surface = None;
                drag.target_offer = None;
            }
        }
    }

    /// Deliver queued cross-client data-device events once client writers are reachable.
    pub fn flush_pending(&mut self, clients: &mut ConnectedClients) {
        let selection_notifies = std::mem::take(&mut self.pending_selection_notifies);
        for notify in selection_notifies {
            let Some(client) = clients.get_mut(&notify.client_id) else {
                continue;
            };
            let (registry, writer) = client.registry_and_writer_mut();
            let _ = self.send_selection_to_device(
                notify.client_id,
                notify.device_id,
                notify.source,
                registry,
                writer,
            );
            clients.mark_send_needed(notify.client_id);
        }

        let cancels = std::mem::take(&mut self.pending_source_cancels);
        for cancel in cancels {
            let Some(client) = clients.get_mut(&cancel.client_id) else {
                continue;
            };
            client
                .writer_mut()
                .wl_data_source_cancelled(cancel.source_id);
            clients.mark_send_needed(cancel.client_id);
        }

        let sends = std::mem::take(&mut self.pending_source_sends);
        for mut send in sends {
            let Some(client) = clients.get_mut(&send.client_id) else {
                continue;
            };
            let fd = std::mem::replace(&mut send.fd, -1);
            forward_receive_to_source(
                client.writer_mut(),
                send.source_id,
                &send.mime_type,
                fd,
            );
            clients.mark_send_needed(send.client_id);
        }

        let source_events = std::mem::take(&mut self.pending_source_events);
        for event in source_events {
            match event {
                PendingSourceEvent::Target {
                    client_id,
                    source_id,
                    mime_type,
                } => {
                    let Some(client) = clients.get_mut(&client_id) else {
                        continue;
                    };
                    client
                        .writer_mut()
                        .wl_data_source_target(source_id)
                        .mime_type(mime_type.as_deref());
                    clients.mark_send_needed(client_id);
                }
                PendingSourceEvent::Action {
                    client_id,
                    source_id,
                    action,
                } => {
                    let Some(client) = clients.get_mut(&client_id) else {
                        continue;
                    };
                    client
                        .writer_mut()
                        .wl_data_source_action(source_id)
                        .dnd_action(action);
                    clients.mark_send_needed(client_id);
                }
                PendingSourceEvent::DndDropPerformed {
                    client_id,
                    source_id,
                } => {
                    let Some(client) = clients.get_mut(&client_id) else {
                        continue;
                    };
                    client
                        .writer_mut()
                        .wl_data_source_dnd_drop_performed(source_id);
                    clients.mark_send_needed(client_id);
                }
                PendingSourceEvent::DndFinished {
                    client_id,
                    source_id,
                } => {
                    let Some(client) = clients.get_mut(&client_id) else {
                        continue;
                    };
                    client
                        .writer_mut()
                        .wl_data_source_dnd_finished(source_id);
                    clients.mark_send_needed(client_id);
                }
            }
        }

        let leaves = std::mem::take(&mut self.pending_drag_leaves);
        for leave in leaves {
            let Some(client) = clients.get_mut(&leave.client_id) else {
                continue;
            };
            client
                .writer_mut()
                .wl_data_device_leave(leave.device_id);
            clients.mark_send_needed(leave.client_id);
        }

        let enters = std::mem::take(&mut self.pending_drag_enters);
        for enter in enters {
            let Some(client) = clients.get_mut(&enter.client_id) else {
                continue;
            };
            let (registry, writer) = client.registry_and_writer_mut();
            let version = self
                .devices
                .get(&(enter.client_id, enter.device_id))
                .map(|d| d.version)
                .unwrap_or(1);
            let offer_id = if enter.source.is_some() {
                self.create_offer(
                    enter.client_id,
                    enter.device_id,
                    enter.source,
                    enter.mime_types,
                    OfferKind::Drag,
                    enter.source_actions,
                    version,
                    registry,
                    writer,
                )
                .ok()
            } else {
                None
            };
            writer
                .wl_data_device_enter(enter.device_id)
                .serial(enter.serial)
                .surface(enter.surface)
                .x(enter.x)
                .y(enter.y)
                .id(offer_id);
            if let Some(drag) = self.drag.as_mut()
                && drag.target_client == Some(enter.client_id)
                && drag.target_device == Some(enter.device_id)
            {
                drag.target_offer = offer_id;
            }
            clients.mark_send_needed(enter.client_id);
        }

        let motions = std::mem::take(&mut self.pending_drag_motions);
        for motion in motions {
            let Some(client) = clients.get_mut(&motion.client_id) else {
                continue;
            };
            client
                .writer_mut()
                .wl_data_device_motion(motion.device_id)
                .time(motion.time_msec)
                .x(motion.x)
                .y(motion.y);
            clients.mark_send_needed(motion.client_id);
        }

        let drops = std::mem::take(&mut self.pending_drag_drops);
        for drop in drops {
            let Some(client) = clients.get_mut(&drop.client_id) else {
                continue;
            };
            client
                .writer_mut()
                .wl_data_device_drop(drop.device_id);
            clients.mark_send_needed(drop.client_id);
        }
    }

    /// Apply pending events for a single client (unit tests without ConnectedClients).
    #[cfg(test)]
    pub fn flush_pending_for_client(
        &mut self,
        client_id: ClientId,
        registry: &mut Registry,
        writer: &mut Writer,
    ) {
        let selection_notifies: Vec<_> = self
            .pending_selection_notifies
            .extract_if(.., |n| n.client_id == client_id)
            .collect();
        for notify in selection_notifies {
            let _ = self.send_selection_to_device(
                notify.client_id,
                notify.device_id,
                notify.source,
                registry,
                writer,
            );
        }

        let cancels: Vec<_> = self
            .pending_source_cancels
            .extract_if(.., |c| c.client_id == client_id)
            .collect();
        for cancel in cancels {
            writer.wl_data_source_cancelled(cancel.source_id);
        }

        let sends: Vec<_> = self
            .pending_source_sends
            .extract_if(.., |s| s.client_id == client_id)
            .collect();
        for mut send in sends {
            let fd = std::mem::replace(&mut send.fd, -1);
            forward_receive_to_source(writer, send.source_id, &send.mime_type, fd);
        }

        let events: Vec<_> = self
            .pending_source_events
            .extract_if(.., |e| match e {
                PendingSourceEvent::Target { client_id: c, .. }
                | PendingSourceEvent::Action { client_id: c, .. }
                | PendingSourceEvent::DndDropPerformed { client_id: c, .. }
                | PendingSourceEvent::DndFinished { client_id: c, .. } => *c == client_id,
            })
            .collect();
        for event in events {
            match event {
                PendingSourceEvent::Target {
                    source_id,
                    mime_type,
                    ..
                } => {
                    writer
                        .wl_data_source_target(source_id)
                        .mime_type(mime_type.as_deref());
                }
                PendingSourceEvent::Action {
                    source_id, action, ..
                } => {
                    writer.wl_data_source_action(source_id).dnd_action(action);
                }
                PendingSourceEvent::DndDropPerformed { source_id, .. } => {
                    writer.wl_data_source_dnd_drop_performed(source_id);
                }
                PendingSourceEvent::DndFinished { source_id, .. } => {
                    writer.wl_data_source_dnd_finished(source_id);
                }
            }
        }

        let leaves: Vec<_> = self
            .pending_drag_leaves
            .extract_if(.., |l| l.client_id == client_id)
            .collect();
        for leave in leaves {
            writer.wl_data_device_leave(leave.device_id);
        }

        let enters: Vec<_> = self
            .pending_drag_enters
            .extract_if(.., |e| e.client_id == client_id)
            .collect();
        for enter in enters {
            let version = self
                .devices
                .get(&(enter.client_id, enter.device_id))
                .map(|d| d.version)
                .unwrap_or(1);
            let offer_id = if enter.source.is_some() {
                self.create_offer(
                    enter.client_id,
                    enter.device_id,
                    enter.source,
                    enter.mime_types,
                    OfferKind::Drag,
                    enter.source_actions,
                    version,
                    registry,
                    writer,
                )
                .ok()
            } else {
                None
            };
            writer
                .wl_data_device_enter(enter.device_id)
                .serial(enter.serial)
                .surface(enter.surface)
                .x(enter.x)
                .y(enter.y)
                .id(offer_id);
            if let Some(drag) = self.drag.as_mut()
                && drag.target_client == Some(enter.client_id)
                && drag.target_device == Some(enter.device_id)
            {
                drag.target_offer = offer_id;
            }
        }

        let _ = writer.flush();
    }

    #[cfg(test)]
    pub fn selection_source(&self) -> Option<(ClientId, ObjectId)> {
        self.selection.as_ref().map(|s| (s.source_client, s.source))
    }

    #[cfg(test)]
    pub fn has_offer(&self, client_id: ClientId, offer_id: ObjectId) -> bool {
        self.offers.contains_key(&(client_id, offer_id))
    }

    #[cfg(test)]
    pub fn selection_offer(&self, client_id: ClientId, device_id: ObjectId) -> Option<ObjectId> {
        self.devices
            .get(&(client_id, device_id))
            .and_then(|d| d.selection_offer)
    }

    #[cfg(test)]
    pub fn pending_selection_notify_count(&self) -> usize {
        self.pending_selection_notifies.len()
    }

    #[cfg(test)]
    pub fn pending_source_cancel_count(&self) -> usize {
        self.pending_source_cancels.len()
    }

    #[cfg(test)]
    pub fn pending_source_send_count(&self) -> usize {
        self.pending_source_sends.len()
    }

    pub fn active_drag_icon(&self) -> Option<(ClientId, ObjectId)> {
        self.drag.as_ref().and_then(|d| d.icon)
    }

    fn device_for_client(&self, client_id: ClientId) -> Option<ObjectId> {
        self.devices
            .iter()
            .find_map(|((owner, id), _)| (*owner == client_id).then_some(*id))
    }

    fn cancel_source(
        &mut self,
        source_client: ClientId,
        source_id: ObjectId,
        immediate_client: Option<ClientId>,
        writer: &mut Writer,
    ) {
        if immediate_client == Some(source_client) {
            writer.wl_data_source_cancelled(source_id);
        } else {
            self.pending_source_cancels.push(PendingSourceCancel {
                client_id: source_client,
                source_id,
            });
        }
    }

    fn advertise_selection(
        &mut self,
        source: Option<(ClientId, ObjectId)>,
        immediate_client: Option<ClientId>,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> Result<(), DataDeviceError> {
        let devices: Vec<(ClientId, ObjectId)> = self
            .devices
            .keys()
            .map(|(owner, id)| (*owner, *id))
            .collect();
        for (owner, id) in devices {
            if Some(owner) == immediate_client {
                self.send_selection_to_device(owner, id, source, registry, writer)?;
            } else {
                self.pending_selection_notifies
                    .push(PendingSelectionNotify {
                        client_id: owner,
                        device_id: id,
                        source,
                    });
            }
        }
        Ok(())
    }

    fn queue_selection_notifies(&mut self, source: Option<(ClientId, ObjectId)>) {
        let devices: Vec<(ClientId, ObjectId)> = self
            .devices
            .keys()
            .map(|(owner, id)| (*owner, *id))
            .collect();
        for (owner, id) in devices {
            self.pending_selection_notifies
                .push(PendingSelectionNotify {
                    client_id: owner,
                    device_id: id,
                    source,
                });
        }
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
            negotiate_action(source_actions, source_actions, first_action(source_actions))
        } else {
            WL_DATA_DEVICE_MANAGER_DND_ACTION_NONE
        };
        if kind == OfferKind::Drag && version >= 3 {
            writer
                .wl_data_offer_source_actions(offer_id)
                .source_actions(source_actions);
            writer.wl_data_offer_action(offer_id).dnd_action(selected);
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
        writer.wl_data_source_action(source_id).dnd_action(action);
    }

    fn cancel_drag(&mut self, immediate_client: Option<ClientId>, writer: &mut Writer) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if let (Some(tc), Some(tdevice)) = (drag.target_client, drag.target_device) {
            if immediate_client == Some(tc) {
                writer.wl_data_device_leave(tdevice);
            } else {
                self.pending_drag_leaves.push(PendingDragLeave {
                    client_id: tc,
                    device_id: tdevice,
                });
            }
            if let Some(offer_id) = drag.target_offer {
                self.offers.remove(&(tc, offer_id));
            }
        }
        if let Some((source_client, source_id)) = drag.source {
            self.cancel_source(source_client, source_id, immediate_client, writer);
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

    use lumalla_wayland_protocol::{ClientId, ObjectId, buffer::Writer, registry::Registry};

    use super::*;

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn writer_pair() -> (UnixStream, Writer) {
        let (receiver, sender) = UnixStream::pair().unwrap();
        let writer = Writer::new(sender.as_raw_fd());
        // Keep sender alive via writer fd; leak sender by forgetting pair end ownership.
        std::mem::forget(sender);
        (receiver, writer)
    }

    fn drain_socket(receiver: &mut UnixStream) {
        let mut drain = [0u8; 4096];
        let _ = receiver.set_nonblocking(true);
        while receiver.read(&mut drain).is_ok() {}
        let _ = receiver.set_nonblocking(false);
    }

    fn read_event_header(receiver: &mut UnixStream) -> (u32, u16, usize) {
        let mut header = [0u8; 8];
        receiver.read_exact(&mut header).unwrap();
        let object_id = u32::from_ne_bytes(header[0..4].try_into().unwrap());
        let opcode = u16::from_ne_bytes(header[4..6].try_into().unwrap());
        let size = u16::from_ne_bytes(header[6..8].try_into().unwrap()) as usize;
        (object_id, opcode, size)
    }

    fn skip_payload(receiver: &mut UnixStream, size: usize) {
        if size > 8 {
            let mut payload = vec![0u8; size - 8];
            receiver.read_exact(&mut payload).unwrap();
        }
    }

    #[test]
    fn selection_round_trip_creates_offer_and_send() {
        let (mut receiver, mut writer) = writer_pair();
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

        assert_eq!(manager.selection_source(), Some((client_id, source)));
        let offer_id = manager
            .selection_offer(client_id, device)
            .expect("selection should create an offer");
        assert!(manager.has_offer(client_id, offer_id));

        let mut pipe_fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

        drain_socket(&mut receiver);

        manager
            .receive(client_id, offer_id, "text/plain", write_fd, &mut writer)
            .unwrap();

        let (object_id, opcode, size) = read_event_header(&mut receiver);
        assert_eq!(object_id, source.get());
        assert_eq!(opcode, 1); // send
        skip_payload(&mut receiver, size);
        unsafe {
            libc::close(read_fd);
        }
    }

    #[test]
    fn cross_client_set_selection_fans_out_on_flush() {
        let (_recv_a, mut writer_a) = writer_pair();
        let (_recv_b, mut writer_b) = writer_pair();
        let mut registry_a = Registry::new();
        let mut registry_b = Registry::new();
        let mut manager = DataDeviceManager::default();
        let client_a = client(1);
        let client_b = client(2);
        let source = object(10);
        let device_a = object(11);
        let device_b = object(21);
        let seat_a = object(12);
        let seat_b = object(22);

        manager.create_data_source(client_a, source, 3);
        manager.offer(client_a, source, "text/plain").unwrap();
        manager.create_data_device(client_a, device_a, seat_a, 3, &mut registry_a, &mut writer_a);
        manager.create_data_device(client_b, device_b, seat_b, 3, &mut registry_b, &mut writer_b);

        manager
            .set_selection(
                client_a,
                device_a,
                Some(source),
                1,
                &mut registry_a,
                &mut writer_a,
            )
            .unwrap();

        assert_eq!(manager.selection_source(), Some((client_a, source)));
        assert!(manager.selection_offer(client_a, device_a).is_some());
        assert!(manager.selection_offer(client_b, device_b).is_none());
        assert_eq!(manager.pending_selection_notify_count(), 1);

        manager.flush_pending_for_client(client_b, &mut registry_b, &mut writer_b);
        let offer_b = manager
            .selection_offer(client_b, device_b)
            .expect("B should receive selection offer after flush");
        assert!(manager.has_offer(client_b, offer_b));
    }

    #[test]
    fn cross_client_receive_sends_to_source_on_flush() {
        let (mut recv_a, mut writer_a) = writer_pair();
        let (_recv_b, mut writer_b) = writer_pair();
        let mut registry_a = Registry::new();
        let mut registry_b = Registry::new();
        let mut manager = DataDeviceManager::default();
        let client_a = client(1);
        let client_b = client(2);
        let source = object(10);
        let device_a = object(11);
        let device_b = object(21);
        let seat_a = object(12);
        let seat_b = object(22);

        manager.create_data_source(client_a, source, 3);
        manager.offer(client_a, source, "text/plain").unwrap();
        manager.create_data_device(client_a, device_a, seat_a, 3, &mut registry_a, &mut writer_a);
        manager.create_data_device(client_b, device_b, seat_b, 3, &mut registry_b, &mut writer_b);
        manager
            .set_selection(
                client_a,
                device_a,
                Some(source),
                1,
                &mut registry_a,
                &mut writer_a,
            )
            .unwrap();
        writer_a.flush().unwrap();
        manager.flush_pending_for_client(client_b, &mut registry_b, &mut writer_b);
        let offer_b = manager.selection_offer(client_b, device_b).unwrap();

        drain_socket(&mut recv_a);

        let mut pipe_fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

        manager
            .receive(client_b, offer_b, "text/plain", write_fd, &mut writer_b)
            .unwrap();
        assert_eq!(manager.pending_source_send_count(), 1);

        manager.flush_pending_for_client(client_a, &mut registry_a, &mut writer_a);

        let (object_id, opcode, size) = read_event_header(&mut recv_a);
        assert_eq!(object_id, source.get());
        assert_eq!(opcode, 1);
        skip_payload(&mut recv_a, size);
        unsafe {
            libc::close(read_fd);
        }
    }

    #[test]
    fn replacing_selection_cancels_previous_owner() {
        let (mut recv_a, mut writer_a) = writer_pair();
        let (_recv_b, mut writer_b) = writer_pair();
        let mut registry_a = Registry::new();
        let mut registry_b = Registry::new();
        let mut manager = DataDeviceManager::default();
        let client_a = client(1);
        let client_b = client(2);
        let source_a = object(10);
        let source_b = object(20);
        let device_a = object(11);
        let device_b = object(21);

        manager.create_data_source(client_a, source_a, 3);
        manager.offer(client_a, source_a, "text/plain").unwrap();
        manager.create_data_source(client_b, source_b, 3);
        manager.offer(client_b, source_b, "text/plain").unwrap();
        manager.create_data_device(client_a, device_a, object(12), 3, &mut registry_a, &mut writer_a);
        manager.create_data_device(client_b, device_b, object(22), 3, &mut registry_b, &mut writer_b);

        manager
            .set_selection(
                client_a,
                device_a,
                Some(source_a),
                1,
                &mut registry_a,
                &mut writer_a,
            )
            .unwrap();
        writer_a.flush().unwrap();
        manager.flush_pending_for_client(client_b, &mut registry_b, &mut writer_b);

        drain_socket(&mut recv_a);

        manager
            .set_selection(
                client_b,
                device_b,
                Some(source_b),
                2,
                &mut registry_b,
                &mut writer_b,
            )
            .unwrap();
        assert_eq!(manager.pending_source_cancel_count(), 1);
        assert_eq!(manager.selection_source(), Some((client_b, source_b)));

        manager.flush_pending_for_client(client_a, &mut registry_a, &mut writer_a);

        // Flush may emit selection(new offer) then cancelled; find cancelled on source_a.
        let mut found_cancel = false;
        for _ in 0..8 {
            let (object_id, opcode, size) = read_event_header(&mut recv_a);
            skip_payload(&mut recv_a, size);
            if object_id == source_a.get() && opcode == 2 {
                found_cancel = true;
                break;
            }
        }
        assert!(found_cancel, "expected wl_data_source.cancelled for previous owner");
        // Previous owner still learns about the new clipboard contents.
        let offer_a = manager
            .selection_offer(client_a, device_a)
            .expect("A should receive B's selection offer");
        assert!(manager.has_offer(client_a, offer_a));
    }

    #[test]
    fn owner_disconnect_clears_selection_for_others() {
        let (_recv_a, mut writer_a) = writer_pair();
        let (_recv_b, mut writer_b) = writer_pair();
        let mut registry_a = Registry::new();
        let mut registry_b = Registry::new();
        let mut manager = DataDeviceManager::default();
        let client_a = client(1);
        let client_b = client(2);
        let source = object(10);
        let device_a = object(11);
        let device_b = object(21);

        manager.create_data_source(client_a, source, 3);
        manager.offer(client_a, source, "text/plain").unwrap();
        manager.create_data_device(client_a, device_a, object(12), 3, &mut registry_a, &mut writer_a);
        manager.create_data_device(client_b, device_b, object(22), 3, &mut registry_b, &mut writer_b);
        manager
            .set_selection(
                client_a,
                device_a,
                Some(source),
                1,
                &mut registry_a,
                &mut writer_a,
            )
            .unwrap();
        manager.flush_pending_for_client(client_b, &mut registry_b, &mut writer_b);
        assert!(manager.selection_offer(client_b, device_b).is_some());

        manager.remove_client(client_a);
        assert!(manager.selection_source().is_none());
        assert_eq!(manager.pending_selection_notify_count(), 1);

        manager.flush_pending_for_client(client_b, &mut registry_b, &mut writer_b);
        assert!(manager.selection_offer(client_b, device_b).is_none());
    }

    #[test]
    fn start_drag_creates_enter_offer() {
        let (_receiver, mut writer) = writer_pair();
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
            .set_source_actions(client_id, source, WL_DATA_DEVICE_MANAGER_DND_ACTION_COPY)
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
                Some((client_id, target)),
                1.0,
                2.0,
                &mut registry,
                &mut writer,
            )
            .unwrap();
        assert!(manager.active_drag_icon().is_none());
        assert!(manager.drag.is_some());

        let mut clients = ConnectedClients::new();
        manager.drag_drop(&mut clients);
        assert!(manager.drag.as_ref().is_some_and(|d| d.drop_performed));
    }

    #[test]
    fn cross_client_start_drag_queues_enter() {
        let (_recv_a, mut writer_a) = writer_pair();
        let (_recv_b, mut writer_b) = writer_pair();
        let mut registry_a = Registry::new();
        let mut registry_b = Registry::new();
        let mut manager = DataDeviceManager::default();
        let client_a = client(1);
        let client_b = client(2);
        let source = object(10);
        let device_a = object(11);
        let device_b = object(21);
        let target = object(30);

        manager.create_data_source(client_a, source, 3);
        manager.offer(client_a, source, "text/plain").unwrap();
        manager
            .set_source_actions(client_a, source, WL_DATA_DEVICE_MANAGER_DND_ACTION_COPY)
            .unwrap();
        manager.create_data_device(client_a, device_a, object(12), 3, &mut registry_a, &mut writer_a);
        manager.create_data_device(client_b, device_b, object(22), 3, &mut registry_b, &mut writer_b);

        manager
            .start_drag(
                client_a,
                device_a,
                Some(source),
                object(20),
                None,
                7,
                Some((client_b, target)),
                3.0,
                4.0,
                &mut registry_a,
                &mut writer_a,
            )
            .unwrap();

        assert_eq!(manager.pending_drag_enters.len(), 1);
        manager.flush_pending_for_client(client_b, &mut registry_b, &mut writer_b);
        assert!(manager.drag.as_ref().is_some_and(|d| d.target_offer.is_some()));
    }
}
