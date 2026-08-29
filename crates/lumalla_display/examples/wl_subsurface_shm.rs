//! Minimal `wl_subsurface` + `wl_shell` + `wl_shm` client for compositor smoke testing.
//!
//! Creates a parent shell surface and a sync subsurface child, commits the child first,
//! then the parent so both buffers are applied together.

use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    ptr,
};

use anyhow::{Context, ensure};
use lumalla_wayland_protocol::protocols::wayland::{
    WL_COMPOSITOR_CREATE_SURFACE_OPCODE, WL_DISPLAY_GET_REGISTRY_OPCODE, WL_DISPLAY_SYNC_OPCODE,
    WL_REGISTRY_BIND_OPCODE, WL_SHELL_GET_SHELL_SURFACE_OPCODE, WL_SHELL_SURFACE_PONG_OPCODE,
    WL_SHELL_SURFACE_SET_TOPLEVEL_OPCODE, WL_SHM_CREATE_POOL_OPCODE, WL_SHM_FORMAT_XRGB8888,
    WL_SHM_POOL_CREATE_BUFFER_OPCODE, WL_SUBCOMPOSITOR_GET_SUBSURFACE_OPCODE,
    WL_SUBSURFACE_SET_POSITION_OPCODE, WL_SUBSURFACE_SET_SYNC_OPCODE, WL_SURFACE_ATTACH_OPCODE,
    WL_SURFACE_COMMIT_OPCODE, WL_SURFACE_DAMAGE_OPCODE, WL_SURFACE_FRAME_OPCODE,
};

const PARENT_WIDTH: u32 = 320;
const PARENT_HEIGHT: u32 = 240;
const CHILD_WIDTH: u32 = 96;
const CHILD_HEIGHT: u32 = 96;

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

    let compositor = bind(&mut stream, &globals, "wl_compositor", 4)?;
    let shm = bind(&mut stream, &globals, "wl_shm", 5)?;
    let shell = bind(&mut stream, &globals, "wl_shell", 6)?;
    let subcompositor = bind(&mut stream, &globals, "wl_subcompositor", 7)?;

    // parent surface = 8, child surface = 9
    send(
        &mut stream,
        request(compositor, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, u32_arg(8)),
    )?;
    send(
        &mut stream,
        request(compositor, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, u32_arg(9)),
    )?;

    let parent_pixels = solid_color(PARENT_WIDTH, PARENT_HEIGHT, [0x40, 0x60, 0x90, 0xff]);
    let child_pixels = solid_color(CHILD_WIDTH, CHILD_HEIGHT, [0x30, 0xd0, 0xff, 0xff]);
    let mut pool_bytes = parent_pixels.clone();
    pool_bytes.extend_from_slice(&child_pixels);
    let file = memory_file(&pool_bytes)?;

    let mut pool_payload = Vec::new();
    push_u32(&mut pool_payload, 10);
    push_i32(&mut pool_payload, pool_bytes.len() as i32);
    send_with_fd(
        &mut stream,
        &request(shm, WL_SHM_CREATE_POOL_OPCODE, pool_payload),
        file.as_raw_fd(),
    )?;

    // parent buffer = 11
    let mut parent_buffer = Vec::new();
    push_u32(&mut parent_buffer, 11);
    push_i32(&mut parent_buffer, 0);
    push_i32(&mut parent_buffer, PARENT_WIDTH as i32);
    push_i32(&mut parent_buffer, PARENT_HEIGHT as i32);
    push_i32(&mut parent_buffer, (PARENT_WIDTH * 4) as i32);
    push_u32(&mut parent_buffer, WL_SHM_FORMAT_XRGB8888);
    send(
        &mut stream,
        request(10, WL_SHM_POOL_CREATE_BUFFER_OPCODE, parent_buffer),
    )?;

    // child buffer = 12
    let mut child_buffer = Vec::new();
    push_u32(&mut child_buffer, 12);
    push_i32(&mut child_buffer, parent_pixels.len() as i32);
    push_i32(&mut child_buffer, CHILD_WIDTH as i32);
    push_i32(&mut child_buffer, CHILD_HEIGHT as i32);
    push_i32(&mut child_buffer, (CHILD_WIDTH * 4) as i32);
    push_u32(&mut child_buffer, WL_SHM_FORMAT_XRGB8888);
    send(
        &mut stream,
        request(10, WL_SHM_POOL_CREATE_BUFFER_OPCODE, child_buffer),
    )?;

    let mut shell_surface_payload = Vec::new();
    push_u32(&mut shell_surface_payload, 13);
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
        request(13, WL_SHELL_SURFACE_SET_TOPLEVEL_OPCODE, Vec::new()),
    )?;

    // subsurface = 14 for child surface 9 under parent 8
    let mut subsurface_payload = Vec::new();
    push_u32(&mut subsurface_payload, 14);
    push_u32(&mut subsurface_payload, 9);
    push_u32(&mut subsurface_payload, 8);
    send(
        &mut stream,
        request(
            subcompositor,
            WL_SUBCOMPOSITOR_GET_SUBSURFACE_OPCODE,
            subsurface_payload,
        ),
    )?;
    send(
        &mut stream,
        request(14, WL_SUBSURFACE_SET_SYNC_OPCODE, Vec::new()),
    )?;
    let mut position_payload = Vec::new();
    push_i32(&mut position_payload, 40);
    push_i32(&mut position_payload, 40);
    send(
        &mut stream,
        request(14, WL_SUBSURFACE_SET_POSITION_OPCODE, position_payload),
    )?;

    // Commit child first (sync: cached until parent commit)
    attach_damage_commit(&mut stream, 9, 12, CHILD_WIDTH, CHILD_HEIGHT, Some(15))?;
    // Then commit parent (applies parent + synchronized child)
    attach_damage_commit(&mut stream, 8, 11, PARENT_WIDTH, PARENT_HEIGHT, Some(16))?;

    println!(
        "Presented a wl_shell parent with sync wl_subsurface child on {}. Press Ctrl-C to exit.",
        socket_path.display()
    );
    loop {
        let event = match read_event(&mut stream) {
            Ok(event) => event,
            Err(error) if is_disconnect(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        if event.object_id == 1 && event.opcode == 0 {
            anyhow::bail!("Compositor reported a protocol error");
        } else if event.object_id == 13 && event.opcode == 0 {
            let serial = read_u32(&event.payload, 0)?;
            println!("Responding to wl_shell_surface.ping serial={serial}");
            if let Err(error) = send(
                &mut stream,
                request(13, WL_SHELL_SURFACE_PONG_OPCODE, u32_arg(serial)),
            ) {
                if is_disconnect(&error) {
                    return Ok(());
                }
                return Err(error);
            }
        }
    }
}

fn attach_damage_commit(
    stream: &mut UnixStream,
    surface: u32,
    buffer: u32,
    width: u32,
    height: u32,
    frame: Option<u32>,
) -> anyhow::Result<()> {
    let mut attach_payload = Vec::new();
    push_u32(&mut attach_payload, buffer);
    push_i32(&mut attach_payload, 0);
    push_i32(&mut attach_payload, 0);
    send(
        stream,
        request(surface, WL_SURFACE_ATTACH_OPCODE, attach_payload),
    )?;

    let mut damage_payload = Vec::new();
    push_i32(&mut damage_payload, 0);
    push_i32(&mut damage_payload, 0);
    push_i32(&mut damage_payload, width as i32);
    push_i32(&mut damage_payload, height as i32);
    send(
        stream,
        request(surface, WL_SURFACE_DAMAGE_OPCODE, damage_payload),
    )?;
    if let Some(frame) = frame {
        send(
            stream,
            request(surface, WL_SURFACE_FRAME_OPCODE, u32_arg(frame)),
        )?;
    }
    send(
        stream,
        request(surface, WL_SURFACE_COMMIT_OPCODE, Vec::new()),
    )?;
    Ok(())
}

fn is_disconnect(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::WriteZero
        )
    })
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
    object_id: u32,
) -> anyhow::Result<u32> {
    let &(name, advertised_version) = globals
        .get(interface)
        .with_context(|| format!("Compositor does not advertise {interface}"))?;
    ensure!(advertised_version >= 1, "{interface} has invalid version 0");
    let mut payload = Vec::new();
    push_u32(&mut payload, name);
    push_string(&mut payload, interface);
    push_u32(&mut payload, 1);
    push_u32(&mut payload, object_id);
    send(stream, request(2, WL_REGISTRY_BIND_OPCODE, payload))?;
    Ok(object_id)
}

fn solid_color(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
    let mut pixels = vec![0; (width * height * 4) as usize];
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.copy_from_slice(&pixel);
    }
    pixels
}

fn memory_file(bytes: &[u8]) -> anyhow::Result<File> {
    let fd =
        unsafe { libc::memfd_create(c"lumalla-wl-subsurface-smoke".as_ptr(), libc::MFD_CLOEXEC) };
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
    let mut header = [0; 8];
    stream.read_exact(&mut header)?;
    let object_id = u32::from_ne_bytes(header[0..4].try_into().unwrap());
    let opcode = u16::from_ne_bytes(header[4..6].try_into().unwrap());
    let size = u16::from_ne_bytes(header[6..8].try_into().unwrap()) as usize;
    ensure!(
        size >= 8 && size.is_multiple_of(4),
        "Invalid event size {size}"
    );
    let mut payload = vec![0; size - 8];
    stream.read_exact(&mut payload)?;
    Ok(Event {
        object_id,
        opcode,
        payload,
    })
}

fn parse_global(payload: &[u8]) -> anyhow::Result<(u32, String, u32)> {
    ensure!(payload.len() >= 12, "Truncated wl_registry.global event");
    let name = read_u32(payload, 0)?;
    let string_len = read_u32(payload, 4)? as usize;
    ensure!(string_len > 0, "Global interface string is null");
    let string_end = 8usize
        .checked_add(string_len)
        .context("Global interface length overflows")?;
    ensure!(string_end <= payload.len(), "Truncated global interface");
    ensure!(
        payload[string_end - 1] == 0,
        "Global interface is not terminated"
    );
    let interface = std::str::from_utf8(&payload[8..string_end - 1])?.to_owned();
    let version_offset = (string_end + 3) & !3;
    let version = read_u32(payload, version_offset)?;
    Ok((name, interface, version))
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
