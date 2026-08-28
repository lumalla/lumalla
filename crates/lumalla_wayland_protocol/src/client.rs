use log::debug;
use std::{
    io,
    num::NonZeroU32,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
};

use crate::{
    buffer::{ReadResult, Reader, Writer},
    protocols::wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
    registry::{InterfaceIndex, Registry, RequestHandler},
};

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(NonZeroU32);

impl ClientId {
    pub fn new(id: NonZeroU32) -> Self {
        Self(id)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

pub struct Ctx<'client> {
    pub registry: &'client mut Registry,
    pub writer: &'client mut Writer,
    pub client_id: ClientId,
}

#[derive(Debug)]
pub struct ClientConnection {
    stream: UnixStream,
    client_id: ClientId,
    registry: Registry,
    reader: Reader,
    writer: Writer,
    pub(crate) recv_in_flight: bool,
    pub(crate) send_in_flight: bool,
    /// Set when disconnecting; recv/send CQEs are drained before removal.
    pub closing: bool,
}

impl ClientConnection {
    pub(crate) fn new(stream: UnixStream, client_id: ClientId) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        let stream_fd = stream.as_raw_fd();

        debug!(
            "Created client connection with ID: {:?} (from {:?})",
            client_id,
            stream.peer_addr().ok()
        );

        Ok(Self {
            stream,
            client_id,
            registry: Registry::new(),
            reader: Reader::new(stream_fd),
            writer: Writer::new(stream_fd),
            recv_in_flight: false,
            send_in_flight: false,
            closing: false,
        })
    }

    /// Build a client from an fd accepted via io_uring (already nonblocking/CLOEXEC).
    pub(crate) fn from_accepted_fd(fd: OwnedFd, client_id: ClientId) -> io::Result<Self> {
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(true)?;
        Self::new(stream, client_id)
    }

    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn writer_mut(&mut self) -> &mut Writer {
        &mut self.writer
    }

    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    pub fn registry_and_writer_mut(&mut self) -> (&mut Registry, &mut Writer) {
        (&mut self.registry, &mut self.writer)
    }

    pub fn has_pending_output(&self) -> bool {
        self.writer.has_pending_output()
    }

    pub fn recv_in_flight(&self) -> bool {
        self.recv_in_flight
    }

    pub fn send_in_flight(&self) -> bool {
        self.send_in_flight || self.writer.send_in_flight()
    }

    pub fn send_buffer_limit_exceeded(&self) -> bool {
        self.writer.send_buffer_limit_exceeded()
    }

    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    pub fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    /// Prepare RecvMsg; marks recv in flight. Returns None if buffer is full.
    pub fn prepare_recv(&mut self) -> Option<*mut libc::msghdr> {
        if self.recv_in_flight {
            return None;
        }
        let msg = self.reader.prepare_recv_msghdr()?;
        self.recv_in_flight = true;
        Some(msg)
    }

    pub fn complete_recv(&mut self, result: i32) -> ReadResult {
        self.recv_in_flight = false;
        self.reader.apply_recv_result(result)
    }

    /// Prepare SendMsg if there is pending output and none in flight.
    pub fn prepare_send(&mut self) -> Option<*const libc::msghdr> {
        if self.send_in_flight {
            return None;
        }
        let msg = self.writer.prepare_send_msghdr()?;
        self.send_in_flight = true;
        Some(msg)
    }

    /// Returns whether the caller should re-submit SendMsg.
    pub fn complete_send(&mut self, result: i32) -> anyhow::Result<bool> {
        let more = self.writer.apply_send_result(result)?;
        self.send_in_flight = more;
        Ok(more)
    }

    pub fn handle_messages(&mut self, handler: &mut impl RequestHandler) -> anyhow::Result<()> {
        match self.reader.read() {
            ReadResult::EndOfStream => {
                anyhow::bail!("Client {:?} disconnected", self.client_id);
            }
            ReadResult::NoMoreData => {
                debug!("Client {:?} did not read any data", self.client_id);
            }
            ReadResult::ReadData => {
                self.dispatch_pending(handler)?;
            }
        }
        Ok(())
    }

    /// Parse and dispatch buffered Wayland messages after an async recv.
    pub fn dispatch_pending(&mut self, handler: &mut impl RequestHandler) -> anyhow::Result<()> {
        while let Some((header, data, fds)) = self.reader.next()? {
            let Some(object) = self.registry.object_metadata(header.object_id) else {
                self.writer
                    .wl_display_error(crate::registry::DISPLAY_OBJECT_ID)
                    .object_id(header.object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                    .message("Invalid object ID");
                anyhow::bail!(
                    "Received request for unknown object ID {:?}. Disconnecting client {:?}",
                    header.object_id,
                    self.client_id
                );
            };
            let result = handler.handle_request(
                object,
                &mut Ctx {
                    registry: &mut self.registry,
                    writer: &mut self.writer,
                    client_id: self.client_id,
                },
                &header,
                data,
                fds,
            );
            let message_size = header.size as usize;
            self.reader.message_handled(message_size);
            if result.is_err() {
                return result;
            }
        }
        Ok(())
    }

    pub fn flush(&mut self) -> anyhow::Result<()> {
        if let Some(err) = self.writer.last_err() {
            return Err(err);
        }
        self.writer.flush()
    }

    pub fn broadcast_global(
        &mut self,
        global_id: u32,
        interface_index: InterfaceIndex,
        version: u32,
    ) {
        for registry_object_id in self
            .registry
            .iter_object_ids_of_interface(InterfaceIndex::WlRegistry)
        {
            self.writer
                .wl_registry_global(registry_object_id)
                .name(global_id)
                .interface(interface_index.interface_name())
                .version(version);
        }
    }

    pub fn broadcast_global_remove(&mut self, global_id: u32) {
        for registry_object_id in self
            .registry
            .iter_object_ids_of_interface(InterfaceIndex::WlRegistry)
        {
            self.writer
                .wl_registry_global_remove(registry_object_id)
                .name(global_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Read, num::NonZeroU32, os::unix::net::UnixStream};

    use super::*;
    use crate::{NewObjectId, ObjectId};

    #[test]
    fn broadcast_global_remove_writes_registry_event() {
        let (mut receiver, sender) = UnixStream::pair().unwrap();
        let mut client = ClientConnection::new(sender, ClientId::new(NonZeroU32::new(1).unwrap()))
            .unwrap();
        client
            .registry
            .register_client_object_with_version(
                NewObjectId::new(ObjectId::new(NonZeroU32::new(2).unwrap())),
                InterfaceIndex::WlRegistry,
                1,
            )
            .unwrap();
        client.broadcast_global_remove(7);
        client.flush().unwrap();
        drop(client);

        let mut bytes = Vec::new();
        receiver.read_to_end(&mut bytes).unwrap();
        assert!(bytes.windows(12).any(|w| {
            u32::from_ne_bytes(w[0..4].try_into().unwrap()) == 2
                && u16::from_ne_bytes(w[4..6].try_into().unwrap()) == 1
                && u32::from_ne_bytes(w[8..12].try_into().unwrap()) == 7
        }));
    }
}
