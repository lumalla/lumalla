use log::{debug, error};
use mio::{event::Source, unix::SourceFd};
use std::{
    io::{self},
    os::{fd::AsRawFd, unix::net::UnixStream},
};

use crate::{
    buffer::{ReadResult, Reader, Writer},
    registry::{Registry, RequestHandler},
};

pub type ClientId = u32;

pub struct Ctx<'client> {
    pub registry: &'client mut Registry,
    pub writer: &'client mut Writer,
}

#[derive(Debug)]
pub enum ClientEvent {
    MessageReceived { client_id: ClientId, data: Vec<u8> },
    Disconnected { client_id: ClientId },
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

    #[must_use]
    pub fn handle_messages(&mut self, handler: &mut impl RequestHandler) -> bool {
        match self.reader.read() {
            ReadResult::EndOfStream => {
                debug!("Client {} disconnected", self.client_id);
                return false;
            }
            ReadResult::NoMoreData => {
                debug!("Client {} did not read any data", self.client_id);
            }
            ReadResult::ReadData => {
                while let Some((header, data, fds)) = self.reader.next() {
                    let Some(interface_index) = self.registry.interface_index(header.object_id)
                    else {
                        error!(
                            "Received request for unknown object ID {}. Disconnecting client {}",
                            header.object_id, self.client_id
                        );
                        return false;
                    };
                    let success = handler.handle_request(
                        interface_index,
                        header.opcode,
                        Ctx {
                            registry: &mut self.registry,
                            writer: &mut self.writer,
                        },
                        data,
                        fds,
                    );
                    let message_size = header.size as usize;
                    self.reader.message_handled(message_size);
                    if !success {
                        return false;
                    }
                }
            }
        }

        true
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
