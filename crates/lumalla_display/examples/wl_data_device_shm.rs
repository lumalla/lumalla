//! Selection clipboard smoke client for `wl_data_device_manager`.
//!
//! Binds data device + seat + compositor/shm/shell, sets a `text/plain` selection,
//! receives it back through a pipe, and prints the bytes.
//!
//! DnD (`start_drag`) is covered by compositor unit tests; this example focuses on
//! the selection round-trip that a single client can exercise without compositor
//! button-grab injection.

use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    ptr,
};

use anyhow::{Context, ensure};
use lumalla_wayland_protocol::protocols::wayland::{
    WL_COMPOSITOR_CREATE_SURFACE_OPCODE, WL_DATA_DEVICE_MANAGER_CREATE_DATA_SOURCE_OPCODE,
    WL_DATA_DEVICE_MANAGER_GET_DATA_DEVICE_OPCODE, WL_DATA_DEVICE_SET_SELECTION_OPCODE,
    WL_DATA_OFFER_RECEIVE_OPCODE, WL_DATA_SOURCE_OFFER_OPCODE, WL_DISPLAY_GET_REGISTRY_OPCODE,
    WL_DISPLAY_SYNC_OPCODE, WL_REGISTRY_BIND_OPCODE, WL_SHELL_GET_SHELL_SURFACE_OPCODE,
    WL_SHELL_SURFACE_PONG_OPCODE, WL_SHELL_SURFACE_SET_TOPLEVEL_OPCODE, WL_SHM_CREATE_POOL_OPCODE,
    WL_SHM_FORMAT_XRGB8888, WL_SHM_POOL_CREATE_BUFFER_OPCODE, WL_SURFACE_ATTACH_OPCODE,
    WL_SURFACE_COMMIT_OPCODE, WL_SURFACE_DAMAGE_OPCODE,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;
const CLIPBOARD: &[u8] = b"hello from lumalla selection";

fn main() -> anyhow::Result<()> {
    let socket_path = socket_path()?;
    let mut stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("Failed to connect to {}", socket_path.display()))?;

    send(
        &mut stream,
        request(1, WL_DISPLAY_GET_REGISTRY_OPCODE, u32_arg(2)),
    )?;
    send(&mut stream, request(1, WL_DISPLAY_SYNC_OPCODE, u32_arg(3)))?;

    let mut globals = HashMap::new();
    loop {
        let event = read_event(&mut stream)?;
        if event.object_id == 2 && event.opcode == 0 {
            let (name, interface, version) = parse_global(&event.payload)?;
            globals.insert(interface, (name, version));
        } else if event.object_id == 3 && event.opcode == 0 {
            break;
        } else if event.object_id == 1 && event.opcode == 0 {
            anyhow::bail!("Compositor reported a protocol error");
        }
    }

    let compositor = bind(&mut stream, &globals, "wl_compositor", 4, 4)?;
    let shm = bind(&mut stream, &globals, "wl_shm", 1, 5)?;
    let shell = bind(&mut stream, &globals, "wl_shell", 1, 6)?;
    let seat = bind(&mut stream, &globals, "wl_seat", 5, 7)?;
    let data_mgr = bind(&mut stream, &globals, "wl_data_device_manager", 3, 8)?;

    send(
        &mut stream,
        request(compositor, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, u32_arg(9)),
    )?;

    let pixels = vec![0x40u8; (WIDTH * HEIGHT * 4) as usize];
    let file = memory_file(&pixels)?;
    let mut pool_payload = Vec::new();
    push_u32(&mut pool_payload, 10);
    push_i32(&mut pool_payload, pixels.len() as i32);
    send_with_fd(
        &mut stream,
        &request(shm, WL_SHM_CREATE_POOL_OPCODE, pool_payload),
        file.as_raw_fd(),
    )?;

    let mut buffer_payload = Vec::new();
    push_u32(&mut buffer_payload, 11);
    push_i32(&mut buffer_payload, 0);
    push_i32(&mut buffer_payload, WIDTH as i32);
    push_i32(&mut buffer_payload, HEIGHT as i32);
    push_i32(&mut buffer_payload, (WIDTH * 4) as i32);
    push_u32(&mut buffer_payload, WL_SHM_FORMAT_XRGB8888);
    send(
        &mut stream,
        request(10, WL_SHM_POOL_CREATE_BUFFER_OPCODE, buffer_payload),
    )?;

    let mut shell_surface_payload = Vec::new();
    push_u32(&mut shell_surface_payload, 12);
    push_u32(&mut shell_surface_payload, 9);
    send(
        &mut stream,
        request(
            shell,
            WL_SHELL_GET_SHELL_SURFACE_OPCODE,
            shell_surface_payload,
        ),
    )?;
    send(
        &mut stream,
        request(12, WL_SHELL_SURFACE_SET_TOPLEVEL_OPCODE, Vec::new()),
    )?;

    let mut attach_payload = Vec::new();
    push_u32(&mut attach_payload, 11);
    push_i32(&mut attach_payload, 0);
    push_i32(&mut attach_payload, 0);
    send(
        &mut stream,
        request(9, WL_SURFACE_ATTACH_OPCODE, attach_payload),
    )?;
    let mut damage_payload = Vec::new();
    push_i32(&mut damage_payload, 0);
    push_i32(&mut damage_payload, 0);
    push_i32(&mut damage_payload, WIDTH as i32);
    push_i32(&mut damage_payload, HEIGHT as i32);
    send(
        &mut stream,
        request(9, WL_SURFACE_DAMAGE_OPCODE, damage_payload),
    )?;
    send(
        &mut stream,
        request(9, WL_SURFACE_COMMIT_OPCODE, Vec::new()),
    )?;

    send(
        &mut stream,
        request(
            data_mgr,
            WL_DATA_DEVICE_MANAGER_CREATE_DATA_SOURCE_OPCODE,
            u32_arg(13),
        ),
    )?;
    let mut offer_payload = Vec::new();
    push_string(&mut offer_payload, "text/plain");
    send(
        &mut stream,
        request(13, WL_DATA_SOURCE_OFFER_OPCODE, offer_payload),
    )?;

    let mut device_payload = Vec::new();
    push_u32(&mut device_payload, 14);
    push_u32(&mut device_payload, seat);
    send(
        &mut stream,
        request(
            data_mgr,
            WL_DATA_DEVICE_MANAGER_GET_DATA_DEVICE_OPCODE,
            device_payload,
        ),
    )?;

    let mut selection_payload = Vec::new();
    push_u32(&mut selection_payload, 13);
    push_u32(&mut selection_payload, 1);
    send(
        &mut stream,
        request(14, WL_DATA_DEVICE_SET_SELECTION_OPCODE, selection_payload),
    )?;

    let mut selection_offer = None;
    let mut received_mimes = Vec::new();
    let mut got_selection = false;
    let mut pipe_read: Option<RawFd> = None;

    println!(
        "wl_data_device_shm selection round-trip on {}",
        socket_path.display()
    );

    loop {
        let (event, ancillary_fd) = read_event_with_fd(&mut stream)?;
        if event.object_id == 1 && event.opcode == 0 {
            anyhow::bail!("Compositor reported a protocol error");
        } else if event.object_id == 12 && event.opcode == 0 {
            let serial = read_u32(&event.payload, 0)?;
            send(
                &mut stream,
                request(12, WL_SHELL_SURFACE_PONG_OPCODE, u32_arg(serial)),
            )?;
        } else if event.object_id == 14 && event.opcode == 0 {
            // data_offer
            let offer = read_u32(&event.payload, 0)?;
            selection_offer = Some(offer);
            received_mimes.clear();
            println!("wl_data_device.data_offer id={offer:#x}");
        } else if event.object_id == 14 && event.opcode == 5 {
            // selection
            let offer = read_u32(&event.payload, 0)?;
            got_selection = true;
            println!("wl_data_device.selection offer={offer:#x}");
            if offer != 0 {
                selection_offer = Some(offer);
            }
            if let Some(offer) = selection_offer {
                if received_mimes.iter().any(|m| m == "text/plain") {
                    let mut fds = [0; 2];
                    ensure!(unsafe { libc::pipe(fds.as_mut_ptr()) } == 0, "pipe failed");
                    let (read_fd, write_fd) = (fds[0], fds[1]);
                    pipe_read = Some(read_fd);
                    let mut receive_payload = Vec::new();
                    push_string(&mut receive_payload, "text/plain");
                    send_with_fd(
                        &mut stream,
                        &request(offer, WL_DATA_OFFER_RECEIVE_OPCODE, receive_payload),
                        write_fd,
                    )?;
                    unsafe {
                        libc::close(write_fd);
                    }
                    println!("wl_data_offer.receive text/plain");
                }
            }
        } else if selection_offer == Some(event.object_id) && event.opcode == 0 {
            let mime = parse_string(&event.payload, 0)?;
            println!("wl_data_offer.offer {mime}");
            received_mimes.push(mime);
        } else if event.object_id == 13 && event.opcode == 1 {
            // wl_data_source.send
            let mime = parse_string(&event.payload, 0)?;
            let fd = ancillary_fd.context("wl_data_source.send missing fd")?;
            println!("wl_data_source.send mime={mime}");
            let mut file = unsafe { File::from_raw_fd(fd) };
            file.write_all(CLIPBOARD)?;
            drop(file);
            if let Some(read_fd) = pipe_read.take() {
                let mut file = unsafe { File::from_raw_fd(read_fd) };
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)?;
                println!("selection bytes: {}", String::from_utf8_lossy(&buf));
                ensure!(buf == CLIPBOARD, "clipboard round-trip mismatch");
                println!("selection round-trip ok");
                return Ok(());
            }
        }

        let _ = got_selection;
    }
}

fn socket_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::args_os().nth(1) {
        return Ok(path.into());
    }
    let display = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
    let display_path = PathBuf::from(display);
    if display_path.is_absolute() {
        return Ok(display_path);
    }
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(runtime_dir).join(display_path))
}

fn bind(
    stream: &mut UnixStream,
    globals: &HashMap<String, (u32, u32)>,
    interface: &str,
    version: u32,
    object_id: u32,
) -> anyhow::Result<u32> {
    let &(name, advertised_version) = globals
        .get(interface)
        .with_context(|| format!("Compositor does not advertise {interface}"))?;
    ensure!(advertised_version >= 1, "{interface} has invalid version 0");
    let version = version.min(advertised_version);
    let mut payload = Vec::new();
    push_u32(&mut payload, name);
    push_string(&mut payload, interface);
    push_u32(&mut payload, version);
    push_u32(&mut payload, object_id);
    send(stream, request(2, WL_REGISTRY_BIND_OPCODE, payload))?;
    Ok(object_id)
}

fn memory_file(bytes: &[u8]) -> anyhow::Result<File> {
    let fd = unsafe { libc::memfd_create(c"lumalla-wl-data-device".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create failed");
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.set_len(bytes.len() as u64)?;
    file.write_all(bytes)?;
    Ok(file)
}

fn request(object_id: u32, opcode: u16, payload: Vec<u8>) -> Vec<u8> {
    let size = 8 + payload.len();
    assert!(size <= u16::MAX as usize && size.is_multiple_of(4));
    let mut message = Vec::with_capacity(size);
    push_u32(&mut message, object_id);
    message.extend_from_slice(&opcode.to_ne_bytes());
    message.extend_from_slice(&(size as u16).to_ne_bytes());
    message.extend_from_slice(&payload);
    message
}

fn send(stream: &mut UnixStream, message: Vec<u8>) -> anyhow::Result<()> {
    stream.write_all(&message)?;
    Ok(())
}

fn send_with_fd(stream: &mut UnixStream, message: &[u8], fd: i32) -> anyhow::Result<()> {
    let mut iov = libc::iovec {
        iov_base: message.as_ptr().cast_mut().cast(),
        iov_len: message.len(),
    };
    let control_len = unsafe { libc::CMSG_SPACE(mem::size_of::<i32>() as u32) } as usize;
    let mut control = vec![0usize; control_len.div_ceil(mem::size_of::<usize>())];
    let mut header: libc::msghdr = unsafe { mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = control_len;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<i32>() as u32) as usize;
        ptr::write(libc::CMSG_DATA(cmsg).cast::<i32>(), fd);
    }
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &header, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error()).context("sendmsg failed");
    }
    stream.write_all(&message[sent as usize..])?;
    Ok(())
}

struct Event {
    object_id: u32,
    opcode: u16,
    payload: Vec<u8>,
}

fn read_event(stream: &mut UnixStream) -> anyhow::Result<Event> {
    let (event, _) = read_event_with_fd(stream)?;
    Ok(event)
}

fn read_event_with_fd(stream: &mut UnixStream) -> anyhow::Result<(Event, Option<RawFd>)> {
    let mut header = [0u8; 8];
    let control_len = unsafe { libc::CMSG_SPACE(mem::size_of::<i32>() as u32) } as usize;
    let mut control = vec![0usize; control_len.div_ceil(mem::size_of::<usize>())];
    let mut iov = libc::iovec {
        iov_base: header.as_mut_ptr().cast(),
        iov_len: header.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control_len;

    loop {
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("recvmsg failed");
        }
        if n == 0 {
            anyhow::bail!(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
        }
        ensure!(n as usize == 8, "expected Wayland header, got {n} bytes");
        break;
    }

    let mut fd = None;
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                fd = Some(ptr::read(libc::CMSG_DATA(cmsg).cast::<i32>()));
                break;
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    let object_id = u32::from_ne_bytes(header[0..4].try_into().unwrap());
    let opcode = u16::from_ne_bytes(header[4..6].try_into().unwrap());
    let size = u16::from_ne_bytes(header[6..8].try_into().unwrap()) as usize;
    ensure!(
        size >= 8 && size.is_multiple_of(4),
        "Invalid event size {size}"
    );
    let mut payload = vec![0; size - 8];
    if !payload.is_empty() {
        stream.read_exact(&mut payload)?;
    }
    Ok((
        Event {
            object_id,
            opcode,
            payload,
        },
        fd,
    ))
}

fn parse_global(payload: &[u8]) -> anyhow::Result<(u32, String, u32)> {
    ensure!(payload.len() >= 12, "Truncated wl_registry.global event");
    let name = read_u32(payload, 0)?;
    let (interface, next) = parse_string_at(payload, 4)?;
    let version = read_u32(payload, next)?;
    Ok((name, interface, version))
}

fn parse_string(payload: &[u8], offset: usize) -> anyhow::Result<String> {
    Ok(parse_string_at(payload, offset)?.0)
}

fn parse_string_at(payload: &[u8], offset: usize) -> anyhow::Result<(String, usize)> {
    let string_len = read_u32(payload, offset)? as usize;
    ensure!(string_len > 0, "null string");
    let string_end = offset
        .checked_add(4)
        .and_then(|o| o.checked_add(string_len))
        .context("string length overflow")?;
    ensure!(string_end <= payload.len(), "truncated string");
    ensure!(payload[string_end - 1] == 0, "string not terminated");
    let value = std::str::from_utf8(&payload[offset + 4..string_end - 1])?.to_owned();
    let next = (string_end + 3) & !3;
    Ok((value, next))
}

fn read_u32(bytes: &[u8], offset: usize) -> anyhow::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("Truncated Wayland uint")?
        .try_into()
        .unwrap();
    Ok(u32::from_ne_bytes(value))
}

fn u32_arg(value: u32) -> Vec<u8> {
    value.to_ne_bytes().to_vec()
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

fn push_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

fn push_string(bytes: &mut Vec<u8>, value: &str) {
    let length = value.len() + 1;
    push_u32(bytes, length as u32);
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    bytes.resize((bytes.len() + 3) & !3, 0);
}
