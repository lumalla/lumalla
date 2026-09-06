//! Smoke client: bind zwp_pointer_constraints_v1, map a shell surface, lock pointer on click.

use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    ptr,
};

use anyhow::{Context, ensure};
use lumalla_wayland_protocol::protocols::{
    pointer_constraints::{
        ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_ONESHOT, ZWP_POINTER_CONSTRAINTS_V1_LOCK_POINTER_OPCODE,
        ZWP_POINTER_CONSTRAINTS_V1_NAME,
    },
    wayland::{
        WL_COMPOSITOR_CREATE_SURFACE_OPCODE, WL_DISPLAY_GET_REGISTRY_OPCODE,
        WL_DISPLAY_SYNC_OPCODE, WL_POINTER_SET_CURSOR_OPCODE, WL_REGISTRY_BIND_OPCODE,
        WL_SEAT_GET_POINTER_OPCODE, WL_SHELL_GET_SHELL_SURFACE_OPCODE,
        WL_SHELL_SURFACE_PONG_OPCODE, WL_SHELL_SURFACE_SET_TOPLEVEL_OPCODE,
        WL_SHM_CREATE_POOL_OPCODE, WL_SHM_FORMAT_XRGB8888, WL_SHM_POOL_CREATE_BUFFER_OPCODE,
        WL_SURFACE_ATTACH_OPCODE, WL_SURFACE_COMMIT_OPCODE, WL_SURFACE_DAMAGE_OPCODE,
    },
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

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

    ensure!(
        globals.contains_key(ZWP_POINTER_CONSTRAINTS_V1_NAME),
        "compositor does not advertise zwp_pointer_constraints_v1"
    );

    let compositor = bind(&mut stream, &globals, "wl_compositor", 5, 4)?;
    let shm = bind(&mut stream, &globals, "wl_shm", 2, 5)?;
    let shell = bind(&mut stream, &globals, "wl_shell", 1, 6)?;
    let seat = bind(&mut stream, &globals, "wl_seat", 5, 7)?;
    let constraints = bind(&mut stream, &globals, ZWP_POINTER_CONSTRAINTS_V1_NAME, 1, 12)?;

    send(
        &mut stream,
        request(compositor, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, u32_arg(8)),
    )?;

    let pixels = solid(0x30, 0x70, 0xb0);
    let file = memory_file(&pixels)?;
    let mut pool_payload = Vec::new();
    push_u32(&mut pool_payload, 9);
    push_i32(&mut pool_payload, pixels.len() as i32);
    send_with_fd(
        &mut stream,
        &request(shm, WL_SHM_CREATE_POOL_OPCODE, pool_payload),
        file.as_raw_fd(),
    )?;

    let mut buffer_payload = Vec::new();
    push_u32(&mut buffer_payload, 10);
    push_i32(&mut buffer_payload, 0);
    push_i32(&mut buffer_payload, WIDTH as i32);
    push_i32(&mut buffer_payload, HEIGHT as i32);
    push_i32(&mut buffer_payload, (WIDTH * 4) as i32);
    push_u32(&mut buffer_payload, WL_SHM_FORMAT_XRGB8888);
    send(
        &mut stream,
        request(9, WL_SHM_POOL_CREATE_BUFFER_OPCODE, buffer_payload),
    )?;

    let mut shell_surface_payload = Vec::new();
    push_u32(&mut shell_surface_payload, 11);
    push_u32(&mut shell_surface_payload, 8);
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
        request(11, WL_SHELL_SURFACE_SET_TOPLEVEL_OPCODE, Vec::new()),
    )?;

    let mut attach = Vec::new();
    push_u32(&mut attach, 10);
    push_i32(&mut attach, 0);
    push_i32(&mut attach, 0);
    send(&mut stream, request(8, WL_SURFACE_ATTACH_OPCODE, attach))?;
    let mut damage = Vec::new();
    push_i32(&mut damage, 0);
    push_i32(&mut damage, 0);
    push_i32(&mut damage, WIDTH as i32);
    push_i32(&mut damage, HEIGHT as i32);
    send(&mut stream, request(8, WL_SURFACE_DAMAGE_OPCODE, damage))?;
    send(
        &mut stream,
        request(8, WL_SURFACE_COMMIT_OPCODE, Vec::new()),
    )?;

    send(
        &mut stream,
        request(seat, WL_SEAT_GET_POINTER_OPCODE, u32_arg(13)),
    )?;

    let mut locked = false;
    eprintln!("pointer-constraints smoke: move over the window and click to lock");

    loop {
        let event = read_event(&mut stream)?;
        if event.object_id == 1 && event.opcode == 0 {
            anyhow::bail!("Compositor reported a protocol error");
        }
        if event.object_id == 11 && event.opcode == 0 {
            // wl_shell_surface.ping
            let serial = u32::from_ne_bytes(event.payload[0..4].try_into()?);
            send(
                &mut stream,
                request(11, WL_SHELL_SURFACE_PONG_OPCODE, u32_arg(serial)),
            )?;
        }
        if event.object_id == 13 && event.opcode == 0 {
            // wl_pointer.enter
            let serial = u32::from_ne_bytes(event.payload[0..4].try_into()?);
            let mut set_cursor = Vec::new();
            push_u32(&mut set_cursor, serial);
            push_u32(&mut set_cursor, 0); // null cursor surface
            push_i32(&mut set_cursor, 0);
            push_i32(&mut set_cursor, 0);
            send(
                &mut stream,
                request(13, WL_POINTER_SET_CURSOR_OPCODE, set_cursor),
            )?;
        }
        if event.object_id == 13 && event.opcode == 3 && !locked {
            // wl_pointer.button pressed
            let state = u32::from_ne_bytes(event.payload[8..12].try_into()?);
            if state == 1 {
                let mut lock = Vec::new();
                push_u32(&mut lock, 14); // locked_pointer id
                push_u32(&mut lock, 8); // surface
                push_u32(&mut lock, 13); // pointer
                push_u32(&mut lock, 0); // null region
                push_u32(&mut lock, ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_ONESHOT);
                send(
                    &mut stream,
                    request(
                        constraints,
                        ZWP_POINTER_CONSTRAINTS_V1_LOCK_POINTER_OPCODE,
                        lock,
                    ),
                )?;
                locked = true;
                eprintln!("requested pointer lock");
            }
        }
        if event.object_id == 14 && event.opcode == 0 {
            eprintln!("received zwp_locked_pointer_v1.locked");
            break;
        }
    }

    eprintln!("pointer-constraints smoke ok");
    Ok(())
}

fn socket_path() -> anyhow::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR unset")?;
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    Ok(PathBuf::from(runtime).join(display))
}

fn bind(
    stream: &mut UnixStream,
    globals: &HashMap<String, (u32, u32)>,
    interface: &str,
    version: u32,
    id: u32,
) -> anyhow::Result<u32> {
    let (name, advertised_version) = globals
        .get(interface)
        .with_context(|| format!("missing global {interface}"))?;
    ensure!(*advertised_version >= 1, "{interface} has invalid version 0");
    let version = version.min(*advertised_version);
    let mut data = Vec::new();
    push_u32(&mut data, *name);
    push_string(&mut data, interface);
    push_u32(&mut data, version);
    push_u32(&mut data, id);
    send(stream, request(2, WL_REGISTRY_BIND_OPCODE, data))?;
    Ok(id)
}

fn solid(r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for chunk in pixels.chunks_exact_mut(4) {
        chunk[0] = b;
        chunk[1] = g;
        chunk[2] = r;
        chunk[3] = 0xff;
    }
    pixels
}

fn memory_file(bytes: &[u8]) -> anyhow::Result<File> {
    let fd = unsafe { libc::memfd_create(c"lumalla-pc-smoke".as_ptr(), libc::MFD_CLOEXEC) };
    ensure!(fd >= 0, "memfd_create failed");
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.set_len(bytes.len() as u64)?;
    file.write_all(bytes)?;
    Ok(file)
}

fn request(object_id: u32, opcode: u16, payload: Vec<u8>) -> Vec<u8> {
    let size = 8 + payload.len();
    let mut message = Vec::with_capacity(size);
    message.extend_from_slice(&object_id.to_ne_bytes());
    message.extend_from_slice(&opcode.to_ne_bytes());
    message.extend_from_slice(&(size as u16).to_ne_bytes());
    message.extend_from_slice(&payload);
    message
}

fn u32_arg(value: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    push_u32(&mut payload, value);
    payload
}

fn push_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_ne_bytes());
}

fn push_i32(buf: &mut Vec<u8>, value: i32) {
    buf.extend_from_slice(&value.to_ne_bytes());
}

fn push_string(buf: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    push_u32(buf, (bytes.len() + 1) as u32);
    buf.extend_from_slice(bytes);
    buf.push(0);
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
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
    let mut control = vec![0u8; control_len];
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
    ensure!(sent as usize == message.len(), "sendmsg failed");
    Ok(())
}

struct Event {
    object_id: u32,
    opcode: u16,
    payload: Vec<u8>,
}

fn read_event(stream: &mut UnixStream) -> anyhow::Result<Event> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header)?;
    let object_id = u32::from_ne_bytes(header[0..4].try_into()?);
    let opcode = u16::from_ne_bytes(header[4..6].try_into()?);
    let size = u16::from_ne_bytes(header[6..8].try_into()?) as usize;
    ensure!(size >= 8, "invalid wayland message size");
    let mut payload = vec![0u8; size - 8];
    if !payload.is_empty() {
        stream.read_exact(&mut payload)?;
    }
    Ok(Event {
        object_id,
        opcode,
        payload,
    })
}

fn parse_global(payload: &[u8]) -> anyhow::Result<(u32, String, u32)> {
    let name = u32::from_ne_bytes(payload[0..4].try_into()?);
    let str_len = u32::from_ne_bytes(payload[4..8].try_into()?) as usize;
    let interface = std::str::from_utf8(&payload[8..8 + str_len - 1])?.to_owned();
    let padded = (str_len + 3) & !3;
    let version = u32::from_ne_bytes(payload[8 + padded..12 + padded].try_into()?);
    Ok((name, interface, version))
}
