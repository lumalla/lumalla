use log::debug;
use mio::{event::Source, unix::SourceFd};
use std::{
    io::{self},
    os::{fd::AsRawFd, unix::net::UnixStream},
};

use crate::{
    buffer::{ReadResult, Reader, Writer},
    protocols::wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
    registry::{InterfaceIndex, Registry, RequestHandler},
};

pub type ClientId = u32;

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
}

impl ClientConnection {
    pub(crate) fn new(stream: UnixStream, client_id: ClientId) -> io::Result<Self> {
        // Set the stream to non-blocking mode
        stream.set_nonblocking(true)?;

        debug!(
            "Created client connection with ID: {} (from {:?})",
            client_id,
            stream.peer_addr().ok()
        );

        Ok(Self {
            stream: stream.try_clone()?,
            client_id,
            registry: Registry::new(),
            reader: Reader::new(stream.as_raw_fd()),
            writer: Writer::new(stream.as_raw_fd()),
        })
    }

    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    pub fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    pub fn handle_messages(&mut self, handler: &mut impl RequestHandler) -> anyhow::Result<()> {
        match self.reader.read() {
            ReadResult::EndOfStream => {
                anyhow::bail!("Client {} disconnected", self.client_id);
            }
            ReadResult::NoMoreData => {
                debug!("Client {} did not read any data", self.client_id);
            }
            ReadResult::ReadData => {
                while let Some((header, data, fds)) = self.reader.next() {
                    let Some(interface_index) = self.registry.interface_index(header.object_id)
                    else {
                        self.writer
                            .wl_display_error(header.object_id)
                            .object_id(header.object_id)
                            .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                            .message("Invalid object ID");
                        anyhow::bail!(
                            "Received request for unknown object ID {}. Disconnecting client {}",
                            header.object_id,
                            self.client_id
                        );
                    };
                    let result = handler.handle_request(
                        interface_index,
                        &mut Ctx {
                            registry: &mut self.registry,
                            writer: &mut self.writer,
                            client_id: self.client_id,
                        },
                        header,
                        data,
                        fds,
                    );
                    let message_size = header.size as usize;
                    self.reader.message_handled(message_size);
                    if result.is_err() {
                        return result;
                    }
                }
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

    pub fn broadcast_global(&mut self, global_id: u32, interface_index: InterfaceIndex) {
        // TODO: If this is called a lot, we should probably cache the registry object ids
        for registry_object_id in self
            .registry
            .iter_object_ids_of_interface(InterfaceIndex::WlRegistry)
        {
            self.writer
                .wl_registry_global(registry_object_id)
                .name(global_id)
                .interface(interface_index.interface_name())
                .version(interface_index.interface_version());
        }
    }
}

impl Source for ClientConnection {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        SourceFd(&self.stream.as_raw_fd()).register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        SourceFd(&self.stream.as_raw_fd()).reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        SourceFd(&self.stream.as_raw_fd()).deregister(registry)
    }
}
