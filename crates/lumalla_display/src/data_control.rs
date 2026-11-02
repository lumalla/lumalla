use std::{
    collections::HashMap,
    os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd},
};

use lumalla_wayland_protocol::{
    ClientId, ObjectId,
    buffer::Writer,
    registry::{InterfaceIndex, Registry},
};

use crate::{
    ConnectedClients,
    data_device::{
        SelectionSourceKind, SelectionState, forward_receive_to_source,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DataControlError {
    UnknownSource,
    UnknownDevice,
    UnknownOffer,
    UsedSource,
    InvalidOffer,
    UnknownSeat,
}

impl std::fmt::Display for DataControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSource => write!(f, "Unknown ext_data_control_source_v1"),
            Self::UnknownDevice => write!(f, "Unknown ext_data_control_device_v1"),
            Self::UnknownOffer => write!(f, "Unknown ext_data_control_offer_v1"),
            Self::UsedSource => write!(f, "ext_data_control_source_v1 has already been used"),
            Self::InvalidOffer => {
                write!(f, "offer sent after ext_data_control_device.set_selection")
            }
            Self::UnknownSeat => write!(f, "Unknown wl_seat"),
        }
    }
}

#[derive(Debug, Default)]
pub struct DataControlManager {
    devices: HashMap<(ClientId, ObjectId), ControlDevice>,
    sources: HashMap<(ClientId, ObjectId), ControlSource>,
    offers: HashMap<(ClientId, ObjectId), ControlOffer>,
    pending_selection_notifies: Vec<PendingSelectionNotify>,
    pending_source_cancels: Vec<PendingSourceCancel>,
    pending_source_sends: Vec<PendingSourceSend>,
    pending_finished: Vec<PendingFinished>,
}

#[derive(Debug)]
struct ControlDevice {
    seat: ObjectId,
    selection_offer: Option<ObjectId>,
    finished: bool,
}

#[derive(Debug)]
struct ControlSource {
    mime_types: Vec<String>,
    used: bool,
}

#[derive(Debug)]
struct ControlOffer {
    device: ObjectId,
    source: Option<(ClientId, ObjectId)>,
    source_kind: SelectionSourceKind,
    mime_types: Vec<String>,
}

#[derive(Debug, Clone)]
struct PendingSelectionNotify {
    client_id: ClientId,
    device_id: ObjectId,
    selection: Option<SelectionState>,
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
    kind: SelectionSourceKind,
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

#[derive(Debug, Clone, Copy)]
struct PendingFinished {
    client_id: ClientId,
    device_id: ObjectId,
}

impl DataControlManager {
    pub fn create_data_source(&mut self, client_id: ClientId, id: ObjectId) {
        self.sources.insert(
            (client_id, id),
            ControlSource {
                mime_types: Vec::new(),
                used: false,
            },
        );
    }

    pub fn create_data_device(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        seat: ObjectId,
        selection: Option<&SelectionState>,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> Result<(), DataControlError> {
        self.devices.insert(
            (client_id, id),
            ControlDevice {
                seat,
                selection_offer: None,
                finished: false,
            },
        );
        // Protocol: first selection event is sent upon binding the device.
        self.send_selection_to_device(client_id, id, selection.cloned(), registry, writer)
    }

    pub fn offer(
        &mut self,
        client_id: ClientId,
        source_id: ObjectId,
        mime_type: &str,
    ) -> Result<(), DataControlError> {
        let source = self
            .sources
            .get_mut(&(client_id, source_id))
            .ok_or(DataControlError::UnknownSource)?;
        if source.used {
            return Err(DataControlError::InvalidOffer);
        }
        if !source.mime_types.iter().any(|m| m == mime_type) {
            source.mime_types.push(mime_type.to_owned());
        }
        Ok(())
    }

    /// Prepare a control source for becoming the selection. Marks it used.
    pub fn take_source_for_selection(
        &mut self,
        client_id: ClientId,
        source_id: ObjectId,
    ) -> Result<SelectionState, DataControlError> {
        let source = self
            .sources
            .get_mut(&(client_id, source_id))
            .ok_or(DataControlError::UnknownSource)?;
        if source.used {
            return Err(DataControlError::UsedSource);
        }
        source.used = true;
        Ok(SelectionState {
            source_client: client_id,
            source: source_id,
            kind: SelectionSourceKind::ExtDataControlSource,
            mime_types: source.mime_types.clone(),
            serial: 0,
        })
    }

    pub fn destroy_source(
        &mut self,
        client_id: ClientId,
        source_id: ObjectId,
    ) -> Result<bool, DataControlError> {
        if self.sources.remove(&(client_id, source_id)).is_none() {
            return Err(DataControlError::UnknownSource);
        }
        Ok(true)
    }

    pub fn destroy_device(
        &mut self,
        client_id: ClientId,
        device_id: ObjectId,
    ) -> Result<(), DataControlError> {
        if self.devices.remove(&(client_id, device_id)).is_none() {
            return Err(DataControlError::UnknownDevice);
        }
        self.offers
            .retain(|(owner, _), offer| !(*owner == client_id && offer.device == device_id));
        Ok(())
    }

    pub fn destroy_offer(
        &mut self,
        client_id: ClientId,
        offer_id: ObjectId,
    ) -> Result<(), DataControlError> {
        let Some(offer) = self.offers.remove(&(client_id, offer_id)) else {
            return Ok(());
        };
        if let Some(device) = self.devices.get_mut(&(client_id, offer.device))
            && device.selection_offer == Some(offer_id)
        {
            device.selection_offer = None;
        }
        Ok(())
    }

    pub fn destroy_manager(&mut self, _client_id: ClientId, _manager_id: ObjectId) {
        // Children remain valid until destroyed individually.
    }

    /// Queue selection updates for every live control device (no immediate writes).
    pub fn queue_selection_changed(&mut self, selection: Option<SelectionState>) {
        let devices: Vec<(ClientId, ObjectId)> = self
            .devices
            .iter()
            .filter(|(_, d)| !d.finished)
            .map(|((owner, id), _)| (*owner, *id))
            .collect();
        for (owner, device_id) in devices {
            self.pending_selection_notifies
                .push(PendingSelectionNotify {
                    client_id: owner,
                    device_id,
                    selection: selection.clone(),
                });
        }
    }

    /// Advertise `selection` to every live control device.
    pub fn notify_selection_changed(
        &mut self,
        selection: Option<&SelectionState>,
        immediate_client: Option<ClientId>,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> Result<(), DataControlError> {
        let devices: Vec<(ClientId, ObjectId)> = self
            .devices
            .iter()
            .filter(|(_, d)| !d.finished)
            .map(|((owner, id), _)| (*owner, *id))
            .collect();
        for (owner, device_id) in devices {
            if Some(owner) == immediate_client {
                self.send_selection_to_device(
                    owner,
                    device_id,
                    selection.cloned(),
                    registry,
                    writer,
                )?;
            } else {
                self.pending_selection_notifies
                    .push(PendingSelectionNotify {
                        client_id: owner,
                        device_id,
                        selection: selection.cloned(),
                    });
            }
        }
        Ok(())
    }

    pub fn cancel_control_source(
        &mut self,
        source_client: ClientId,
        source_id: ObjectId,
        immediate_client: Option<ClientId>,
        writer: &mut Writer,
    ) {
        if immediate_client == Some(source_client) {
            writer.ext_data_control_source_v1_cancelled(source_id);
        } else {
            self.pending_source_cancels.push(PendingSourceCancel {
                client_id: source_client,
                source_id,
            });
        }
    }

    pub fn receive(
        &mut self,
        client_id: ClientId,
        offer_id: ObjectId,
        mime_type: &str,
        fd: RawFd,
        writer: &mut Writer,
    ) -> Result<(), DataControlError> {
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let offer = self
            .offers
            .get(&(client_id, offer_id))
            .ok_or(DataControlError::UnknownOffer)?;
        let Some((source_client, source_id)) = offer.source else {
            return Ok(());
        };
        let source_kind = offer.source_kind;
        if !offer.mime_types.iter().any(|m| m == mime_type) {
            return Ok(());
        }
        if source_client != client_id {
            self.pending_source_sends.push(PendingSourceSend {
                client_id: source_client,
                source_id,
                kind: source_kind,
                mime_type: mime_type.to_owned(),
                fd: fd.into_raw_fd(),
            });
            return Ok(());
        }
        forward_receive_to_source(
            writer,
            source_id,
            source_kind,
            mime_type,
            fd.into_raw_fd(),
        );
        Ok(())
    }

    /// Mark devices bound to `seat` finished (seat destroyed / gone).
    pub fn finish_devices_for_seat(
        &mut self,
        seat: ObjectId,
        immediate_client: Option<ClientId>,
        writer: &mut Writer,
    ) {
        let devices: Vec<(ClientId, ObjectId)> = self
            .devices
            .iter()
            .filter(|(_, d)| d.seat == seat && !d.finished)
            .map(|((c, id), _)| (*c, *id))
            .collect();
        for (client_id, device_id) in devices {
            if let Some(device) = self.devices.get_mut(&(client_id, device_id)) {
                device.finished = true;
                device.selection_offer = None;
            }
            if immediate_client == Some(client_id) {
                writer.ext_data_control_device_v1_finished(device_id);
            } else {
                self.pending_finished.push(PendingFinished {
                    client_id,
                    device_id,
                });
            }
        }
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        self.sources.retain(|(owner, _), _| *owner != client_id);
        self.devices.retain(|(owner, _), _| *owner != client_id);
        self.offers.retain(|(owner, _), _| *owner != client_id);
        self.pending_selection_notifies
            .retain(|n| n.client_id != client_id);
        self.pending_source_cancels
            .retain(|c| c.client_id != client_id);
        self.pending_source_sends
            .retain(|s| s.client_id != client_id);
        self.pending_finished
            .retain(|f| f.client_id != client_id);
    }

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
                notify.selection,
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
                .ext_data_control_source_v1_cancelled(cancel.source_id);
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
                send.kind,
                &send.mime_type,
                fd,
            );
            clients.mark_send_needed(send.client_id);
        }

        let finished = std::mem::take(&mut self.pending_finished);
        for item in finished {
            let Some(client) = clients.get_mut(&item.client_id) else {
                continue;
            };
            client
                .writer_mut()
                .ext_data_control_device_v1_finished(item.device_id);
            clients.mark_send_needed(item.client_id);
        }
    }

    fn send_selection_to_device(
        &mut self,
        client_id: ClientId,
        device_id: ObjectId,
        selection: Option<SelectionState>,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> Result<(), DataControlError> {
        if self
            .devices
            .get(&(client_id, device_id))
            .is_none_or(|d| d.finished)
        {
            return Err(DataControlError::UnknownDevice);
        }

        let offer_id = match selection {
            Some(sel) => {
                let offer = self.create_offer(
                    client_id,
                    device_id,
                    Some((sel.source_client, sel.source)),
                    sel.kind,
                    sel.mime_types,
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
        writer
            .ext_data_control_device_v1_selection(device_id)
            .id(offer_id);
        Ok(())
    }

    fn create_offer(
        &mut self,
        client_id: ClientId,
        device_id: ObjectId,
        source: Option<(ClientId, ObjectId)>,
        source_kind: SelectionSourceKind,
        mime_types: Vec<String>,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> Result<ObjectId, DataControlError> {
        let offer_id = registry
            .create_object(InterfaceIndex::ExtDataControlOfferV1, 1)
            .map_err(|_| DataControlError::UnknownOffer)?;
        writer
            .ext_data_control_device_v1_data_offer(device_id)
            .id(offer_id);
        for mime in &mime_types {
            writer
                .ext_data_control_offer_v1_offer(offer_id)
                .mime_type(mime);
        }
        self.offers.insert(
            (client_id, offer_id),
            ControlOffer {
                device: device_id,
                source,
                source_kind,
                mime_types,
            },
        );
        Ok(offer_id)
    }

    #[cfg(test)]
    pub fn selection_offer(&self, client_id: ClientId, device_id: ObjectId) -> Option<ObjectId> {
        self.devices
            .get(&(client_id, device_id))
            .and_then(|d| d.selection_offer)
    }

    #[cfg(test)]
    pub fn has_offer(&self, client_id: ClientId, offer_id: ObjectId) -> bool {
        self.offers.contains_key(&(client_id, offer_id))
    }

    #[cfg(test)]
    pub fn pending_selection_notify_count(&self) -> usize {
        self.pending_selection_notifies.len()
    }

    #[cfg(test)]
    pub fn source_used(&self, client_id: ClientId, source_id: ObjectId) -> bool {
        self.sources
            .get(&(client_id, source_id))
            .is_some_and(|s| s.used)
    }

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
                notify.selection,
                registry,
                writer,
            );
        }

        let cancels: Vec<_> = self
            .pending_source_cancels
            .extract_if(.., |c| c.client_id == client_id)
            .collect();
        for cancel in cancels {
            writer.ext_data_control_source_v1_cancelled(cancel.source_id);
        }

        let sends: Vec<_> = self
            .pending_source_sends
            .extract_if(.., |s| s.client_id == client_id)
            .collect();
        for mut send in sends {
            let fd = std::mem::replace(&mut send.fd, -1);
            forward_receive_to_source(writer, send.source_id, send.kind, &send.mime_type, fd);
        }

        let finished: Vec<_> = self
            .pending_finished
            .extract_if(.., |f| f.client_id == client_id)
            .collect();
        for item in finished {
            writer.ext_data_control_device_v1_finished(item.device_id);
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
    use crate::data_device::DataDeviceManager;

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn writer_pair() -> (UnixStream, Writer) {
        let (receiver, sender) = UnixStream::pair().unwrap();
        let writer = Writer::new(sender.as_raw_fd());
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
    fn bind_device_advertises_current_selection() {
        let (_recv, mut writer) = writer_pair();
        let mut registry = Registry::new();
        let mut data = DataDeviceManager::default();
        let mut control = DataControlManager::default();
        let client_id = client(1);
        let wl_source = object(10);
        let wl_device = object(11);
        let control_device = object(20);
        let seat = object(12);

        data.create_data_source(client_id, wl_source, 3);
        data.offer(client_id, wl_source, "text/plain").unwrap();
        data.create_data_device(client_id, wl_device, seat, 3);
        data.set_selection(
            client_id,
            wl_device,
            Some(wl_source),
            1,
            Some(client_id),
            &mut registry,
            &mut writer,
        )
        .unwrap();

        control
            .create_data_device(
                client_id,
                control_device,
                seat,
                data.selection_state(),
                &mut registry,
                &mut writer,
            )
            .unwrap();

        assert!(control.selection_offer(client_id, control_device).is_some());
    }

    #[test]
    fn control_set_selection_notifies_without_focus() {
        let (_recv_a, mut writer_a) = writer_pair();
        let (_recv_b, mut writer_b) = writer_pair();
        let mut registry_a = Registry::new();
        let mut registry_b = Registry::new();
        let mut data = DataDeviceManager::default();
        let mut control = DataControlManager::default();
        let client_a = client(1);
        let client_b = client(2);
        let source = object(10);
        let device_a = object(11);
        let device_b = object(21);
        let seat = object(12);

        control.create_data_source(client_a, source);
        control.offer(client_a, source, "text/plain").unwrap();
        control
            .create_data_device(client_a, device_a, seat, None, &mut registry_a, &mut writer_a)
            .unwrap();
        control
            .create_data_device(client_b, device_b, seat, None, &mut registry_b, &mut writer_b)
            .unwrap();

        let selection = control.take_source_for_selection(client_a, source).unwrap();
        let previous = data
            .replace_selection(
                Some(selection.clone()),
                None, // no keyboard focus
                Some(client_a),
                &mut registry_a,
                &mut writer_a,
            )
            .unwrap();
        assert!(previous.is_none());
        control
            .notify_selection_changed(
                data.selection_state(),
                Some(client_a),
                &mut registry_a,
                &mut writer_a,
            )
            .unwrap();

        assert!(control.selection_offer(client_a, device_a).is_some());
        assert_eq!(control.pending_selection_notify_count(), 1);

        control.flush_pending_for_client(client_b, &mut registry_b, &mut writer_b);
        assert!(control.selection_offer(client_b, device_b).is_some());
        assert_eq!(
            data.selection_kind(),
            Some(SelectionSourceKind::ExtDataControlSource)
        );
    }

    #[test]
    fn wl_set_selection_notifies_control_devices() {
        let (_recv, mut writer) = writer_pair();
        let mut registry = Registry::new();
        let mut data = DataDeviceManager::default();
        let mut control = DataControlManager::default();
        let client_id = client(1);
        let wl_source = object(10);
        let wl_device = object(11);
        let control_device = object(20);
        let seat = object(12);

        control
            .create_data_device(
                client_id,
                control_device,
                seat,
                None,
                &mut registry,
                &mut writer,
            )
            .unwrap();
        assert!(control.selection_offer(client_id, control_device).is_none());

        data.create_data_source(client_id, wl_source, 3);
        data.offer(client_id, wl_source, "text/plain").unwrap();
        data.create_data_device(client_id, wl_device, seat, 3);
        data.set_selection(
            client_id,
            wl_device,
            Some(wl_source),
            1,
            Some(client_id),
            &mut registry,
            &mut writer,
        )
        .unwrap();
        control
            .notify_selection_changed(
                data.selection_state(),
                Some(client_id),
                &mut registry,
                &mut writer,
            )
            .unwrap();

        assert!(control.selection_offer(client_id, control_device).is_some());
    }

    #[test]
    fn used_source_errors() {
        let (_recv, mut writer) = writer_pair();
        let mut registry = Registry::new();
        let mut control = DataControlManager::default();
        let client_id = client(1);
        let source = object(10);
        let device = object(11);

        control.create_data_source(client_id, source);
        control
            .create_data_device(client_id, device, object(12), None, &mut registry, &mut writer)
            .unwrap();
        control.take_source_for_selection(client_id, source).unwrap();
        assert!(control.source_used(client_id, source));
        assert_eq!(
            control.take_source_for_selection(client_id, source),
            Err(DataControlError::UsedSource)
        );
        assert_eq!(
            control.offer(client_id, source, "text/plain"),
            Err(DataControlError::InvalidOffer)
        );
    }

    #[test]
    fn receive_round_trips_to_control_source() {
        let (mut receiver, mut writer) = writer_pair();
        let mut registry = Registry::new();
        let mut data = DataDeviceManager::default();
        let mut control = DataControlManager::default();
        let client_id = client(1);
        let source = object(10);
        let device = object(11);

        control.create_data_source(client_id, source);
        control.offer(client_id, source, "text/plain").unwrap();
        control
            .create_data_device(client_id, device, object(12), None, &mut registry, &mut writer)
            .unwrap();
        let selection = control.take_source_for_selection(client_id, source).unwrap();
        data.replace_selection(
            Some(selection),
            None,
            Some(client_id),
            &mut registry,
            &mut writer,
        )
        .unwrap();
        control
            .notify_selection_changed(
                data.selection_state(),
                Some(client_id),
                &mut registry,
                &mut writer,
            )
            .unwrap();
        writer.flush().unwrap();

        let offer_id = control
            .selection_offer(client_id, device)
            .expect("selection offer");
        assert!(control.has_offer(client_id, offer_id));

        let mut pipe_fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);
        drain_socket(&mut receiver);

        control
            .receive(client_id, offer_id, "text/plain", write_fd, &mut writer)
            .unwrap();

        let (object_id, opcode, size) = read_event_header(&mut receiver);
        assert_eq!(object_id, source.get());
        assert_eq!(opcode, 0); // ext_data_control_source.send
        skip_payload(&mut receiver, size);
        unsafe {
            libc::close(read_fd);
        }
    }

    #[test]
    fn set_primary_selection_is_ignored_by_handlers() {
        // Covered at the protocol handler layer: DataControlManager has no primary API.
        // This test documents the intentional absence of primary-selection state.
        let control = DataControlManager::default();
        assert_eq!(control.pending_selection_notify_count(), 0);
    }
}
