use calloop::{EventSource, Poll, PostAction, Readiness, Token, TokenFactory};
use log::{debug, error, trace};
use std::{
    io::{self, Read},
    os::unix::net::UnixStream,
};

use crate::{header::read_header, registry::Registry};

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

    fn read_data<F>(&mut self, mut callback: F) -> io::Result<()>
    where
        F: FnMut(ClientEvent, &mut ()),
    {
        loop {
            match self.stream.read(&mut self.buffer[self.bytes_in_buffer..]) {
                Ok(0) => {
                    // Client disconnected
                    debug!("Client {} disconnected", self.client_id);
                    callback(
                        ClientEvent::Disconnected {
                            client_id: self.client_id,
                        },
                        &mut (),
                    );
                    return Ok(());
                }
                Ok(bytes_read) => {
                    trace!(
                        "Received {} bytes from client {}",
                        bytes_read, self.client_id
                    );
                    self.bytes_in_buffer += bytes_read;
                    let success = self.read_requests();
                    if !success {
                        callback(
                            ClientEvent::Disconnected {
                                client_id: self.client_id,
                            },
                            &mut (),
                        );
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // No more data to read
                    break;
                }
                Err(e) => {
                    error!("Error reading from client {}: {}", self.client_id, e);
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    #[must_use]
    fn read_requests(&mut self) -> bool {
        let mut current_buffer_offset = 0;
        loop {
            let available_bytes = self.bytes_in_buffer - current_buffer_offset;
            let Some(header) = read_header(&self.buffer[current_buffer_offset..], available_bytes)
            else {
                break;
            };

            if header.size as usize > available_bytes {
                break;
            }

            let success = self.registry.handle_request(
                header.object_id,
                header.opcode,
                &self.buffer[current_buffer_offset..current_buffer_offset + header.size as usize],
            );

            if !success {
                error!(
                    "Failed to handle request. Disconnecting client {}",
                    self.client_id
                );
                return false;
            }

            current_buffer_offset += header.size as usize;
        }

        true
    }
}

impl EventSource for ClientConnection {
    type Event = ClientEvent;
    type Metadata = ();
    type Ret = ();
    type Error = io::Error;

    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        _token: Token,
        callback: F,
    ) -> Result<PostAction, Self::Error>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        if readiness.readable {
            self.read_data(callback)?;
        }

        Ok(PostAction::Continue)
    }

    fn register(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        // SAFETY: Stream is unregistered
        unsafe {
            poll.register(
                &self.stream,
                calloop::Interest::READ,
                calloop::Mode::Level,
                token_factory.token(),
            )
        }?;
        Ok(())
    }

    fn reregister(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        poll.reregister(
            &self.stream,
            calloop::Interest::READ,
            calloop::Mode::Level,
            token_factory.token(),
        )?;
        Ok(())
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        poll.unregister(&self.stream)?;
        Ok(())
    }
}
