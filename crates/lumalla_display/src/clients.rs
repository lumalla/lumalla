//! Owned Wayland client connections and cause-driven I/O arming.

use std::{
    collections::{HashMap, HashSet},
    ops::{Deref, DerefMut},
};

use log::error;
use lumalla_shared::EventLoop;
use lumalla_wayland_protocol::{ClientConnection, ClientId};

/// Collection of connected Wayland clients with recv/send arming helpers.
///
/// The main event loop should not scan this map each lap. Arm I/O at the sites
/// that create the need (accept, recv/send completions, protocol writes).
#[derive(Debug, Default)]
pub struct ConnectedClients {
    clients: HashMap<ClientId, ClientConnection>,
    /// Clients that need a SendMsg SQE when not already in flight.
    pending_send: HashSet<ClientId>,
}

impl ConnectedClients {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, client: ClientConnection) -> Option<ClientConnection> {
        let id = client.client_id();
        self.clients.insert(id, client)
    }

    pub fn remove(&mut self, client_id: ClientId) -> Option<ClientConnection> {
        self.pending_send.remove(&client_id);
        self.clients.remove(&client_id)
    }

    pub fn get(&self, client_id: &ClientId) -> Option<&ClientConnection> {
        self.clients.get(client_id)
    }

    pub fn get_mut(&mut self, client_id: &ClientId) -> Option<&mut ClientConnection> {
        self.clients.get_mut(client_id)
    }

    pub fn contains(&self, client_id: &ClientId) -> bool {
        self.clients.contains_key(client_id)
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Mark a client so [`Self::arm_pending_sends`] will submit SendMsg if needed.
    pub fn mark_send_needed(&mut self, client_id: ClientId) {
        if self.clients.contains_key(&client_id) {
            self.pending_send.insert(client_id);
        }
    }

    /// After a DisplayState batch that may have written to many clients, mark any
    /// client that currently has pending output.
    ///
    /// Call only at known write sites — not every main-loop lap.
    /// [`Self::arm_send`] / `prepare_send` no-op when a SendMsg SQE is already in flight.
    pub fn note_possible_output(&mut self) {
        for (id, client) in &self.clients {
            if !client.closing && client.has_pending_output() {
                self.pending_send.insert(*id);
            }
        }
    }

    /// Mark `client_id` for send if it has pending output (single-client path).
    pub fn note_client_output(&mut self, client_id: ClientId) {
        let Some(client) = self.clients.get(&client_id) else {
            return;
        };
        if !client.closing && client.has_pending_output() {
            self.pending_send.insert(client_id);
        }
    }

    /// Submit RecvMsg for `client_id` if not already in flight.
    ///
    /// Returns `Err(client_id)` when the client should be disconnected.
    pub fn arm_recv(
        &mut self,
        event_loop: &mut EventLoop,
        client_id: ClientId,
    ) -> Result<(), ClientId> {
        let id = client_id.get() as u64;
        let Some(client) = self.clients.get_mut(&client_id) else {
            return Ok(());
        };
        if client.closing || client.recv_in_flight() {
            return Ok(());
        }
        if client.should_disconnect() {
            return Err(client_id);
        }
        if client.recv_buffer_full() {
            error!(
                "Client {:?} filled the Wayland receive buffer; disconnecting",
                client_id
            );
            return Err(client_id);
        }
        let fd = client.as_raw_fd();
        let Some(msg) = client.prepare_recv() else {
            return Ok(());
        };
        let submit_result = unsafe { event_loop.submit_recvmsg(fd, msg, id) };
        if let Err(err) = submit_result {
            client.cancel_prepared_recv();
            error!(
                "Unable to submit recv for client {:?}: {err}; disconnecting",
                client_id
            );
            return Err(client_id);
        }
        Ok(())
    }

    /// Submit SendMsg for `client_id` if there is pending output and none in flight.
    ///
    /// Returns `Err(client_id)` when the client should be disconnected.
    pub fn arm_send(
        &mut self,
        event_loop: &mut EventLoop,
        client_id: ClientId,
    ) -> Result<(), ClientId> {
        let id = client_id.get() as u64;
        let Some(client) = self.clients.get_mut(&client_id) else {
            return Ok(());
        };
        if client.closing {
            return Ok(());
        }
        if client.should_disconnect() {
            return Err(client_id);
        }
        let fd = client.as_raw_fd();
        let Some(msg) = client.prepare_send() else {
            return Ok(());
        };
        let submit_result = unsafe { event_loop.submit_sendmsg(fd, msg, id) };
        if let Err(err) = submit_result {
            client.cancel_prepared_send();
            error!(
                "Unable to submit send for client {:?}: {err}; disconnecting",
                client_id
            );
            return Err(client_id);
        }
        Ok(())
    }

    /// Arm SendMsg for every client previously marked via [`Self::mark_send_needed`]
    /// / [`Self::note_possible_output`] / [`Self::note_client_output`].
    ///
    /// Returns client ids that must be disconnected.
    pub fn arm_pending_sends(&mut self, event_loop: &mut EventLoop) -> Vec<ClientId> {
        let pending: Vec<ClientId> = self.pending_send.drain().collect();
        let mut disconnect = Vec::new();
        for client_id in pending {
            if let Err(id) = self.arm_send(event_loop, client_id) {
                disconnect.push(id);
            }
        }
        disconnect
    }
}

impl Deref for ConnectedClients {
    type Target = HashMap<ClientId, ClientConnection>;

    fn deref(&self) -> &Self::Target {
        &self.clients
    }
}

impl DerefMut for ConnectedClients {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.clients
    }
}
