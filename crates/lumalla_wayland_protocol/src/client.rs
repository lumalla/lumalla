use log::{debug, error, trace};
use mio::{event::Source, unix::SourceFd};
use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::net::UnixStream},
};

use crate::{
    header::read_header,
    registry::{Registry, RequestHandler},
};

pub type ClientId = u32;
pub type Buffer = [u8; u16::MAX as usize];

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
    buffer: Box<Buffer>,
    bytes_in_buffer: usize,
    current_buffer_offset: usize,
}

impl ClientConnection {
    pub(crate) fn new(stream: UnixStream, client_id: ClientId) -> io::Result<Self> {
        // Set the stream to non-blocking mode
        stream.set_nonblocking(true)?;

        debug!("Created client connection with ID: {}", client_id);

        Ok(Self {
            stream,
            client_id,
            registry: Registry::new(),
            buffer: Box::new([0u8; u16::MAX as usize]),
            bytes_in_buffer: 0,
            current_buffer_offset: 0,
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
    pub fn read_data(&mut self, handler: &mut impl RequestHandler) -> bool {
        loop {
            match self.stream.read(&mut self.buffer[self.bytes_in_buffer..]) {
                Ok(0) => {
                    // Client disconnected
                    debug!("Client {} disconnected", self.client_id);
                    return false;
                }
                Ok(bytes_read) => {
                    trace!(
                        "Received {} bytes from client {}",
                        bytes_read, self.client_id
                    );
                    self.bytes_in_buffer += bytes_read;
                    let success = self.read_requests(handler);
                    if !success {
                        return false;
                    }
                    if self.bytes_in_buffer == self.current_buffer_offset {
                        // If we've read all the data in the buffer, reset the offset
                        self.current_buffer_offset = 0;
                        self.bytes_in_buffer = 0;
                    }
                    if self.bytes_in_buffer == self.buffer.len() {
                        // The buffer is full, copy the rest to the front
                        debug!(
                            "Buffer is too small and needs copying for client {}",
                            self.client_id
                        );
                        self.buffer.copy_within(self.current_buffer_offset.., 0);
                        self.bytes_in_buffer -= self.current_buffer_offset;
                        self.current_buffer_offset = 0;
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // No more data to read
                    break;
                }
                Err(e) => {
                    error!("Error reading from client {}: {}", self.client_id, e);
                    return false;
                }
            }
        }

        true
    }

    #[must_use]
    fn read_requests(&mut self, handler: &mut impl RequestHandler) -> bool {
        loop {
            let available_bytes = self.bytes_in_buffer - self.current_buffer_offset;
            let Some(header) =
                read_header(&self.buffer[self.current_buffer_offset..], available_bytes)
            else {
                break;
            };

            if header.size as usize > available_bytes {
                break;
            }

            let Some(interface_index) = self.registry.interface_index(header.object_id) else {
                error!(
                    "Received request for unknown object ID {}. Disconnecting client {}",
                    header.object_id, self.client_id
                );
                return false;
            };

            let success = handler.handle_request(
                interface_index,
                header.opcode,
                &mut self.registry,
                &self.buffer
                    [self.current_buffer_offset..self.current_buffer_offset + header.size as usize],
            );

            if !success {
                error!(
                    "Failed to handle request. Disconnecting client {}",
                    self.client_id
                );
                return false;
            }

            self.current_buffer_offset += header.size as usize;
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
