//! Maps two non-overlapping-friendly xdg toplevels for pointer hit-test smoke.
//! Geometry hit-testing is covered by `surface::tests::pointer_target_uses_buffer_geometry`.

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
    wayland::{
        WL_COMPOSITOR_CREATE_SURFACE_OPCODE, WL_DISPLAY_GET_REGISTRY_OPCODE, WL_DISPLAY_SYNC_OPCODE,
        WL_REGISTRY_BIND_OPCODE, WL_SHM_CREATE_POOL_OPCODE, WL_SHM_FORMAT_XRGB8888,
        WL_SHM_POOL_CREATE_BUFFER_OPCODE, WL_SURFACE_ATTACH_OPCODE, WL_SURFACE_COMMIT_OPCODE,
        WL_SURFACE_DAMAGE_OPCODE,
    },
    xdg_shell::{
        XDG_SURFACE_ACK_CONFIGURE_OPCODE, XDG_SURFACE_GET_TOPLEVEL_OPCODE,
        XDG_WM_BASE_GET_XDG_SURFACE_OPCODE, XDG_WM_BASE_PONG_OPCODE,
    },
};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;

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
    let xdg_wm_base = bind(&mut stream, &globals, "xdg_wm_base", 6)?;

    map_toplevel(
        &mut stream,
        compositor,
        shm,
        xdg_wm_base,
        7,
        8,
        9,
        10,
        11,
        solid(0x20, 0x90, 0xff),
    )?;
    map_toplevel(
        &mut stream,
        compositor,
        shm,
        xdg_wm_base,
        12,
        13,
        14,
        15,
        16,
        solid(0xff, 0x60, 0x20),
    )?;

    println!(
        "Mapped two toplevels for hit-test smoke on {}. Visual: cascade offset, not fullscreen.",
        socket_path.display()
    );
    Ok(())
}

fn map_toplevel(
    stream: &mut UnixStream,
    compositor: u32,
    shm: u32,
    xdg_wm_base: u32,
    surface_id: u32,
    pool_id: u32,
    buffer_id: u32,
    xdg_surface_id: u32,
    toplevel_id: u32,
    pixels: Vec<u8>,
) -> anyhow::Result<()> {
    send(
        stream,
        request(
            compositor,
            WL_COMPOSITOR_CREATE_SURFACE_OPCODE,
            u32_arg(surface_id),
        ),
    )?;
    let file = memory_file(&pixels)?;
    let mut pool_payload = Vec::new();
    push_u32(&mut pool_payload, pool_id);
    push_i32(&mut pool_payload, pixels.len() as i32);
    send_with_fd(
        stream,
        &request(shm, WL_SHM_CREATE_POOL_OPCODE, pool_payload),
        file.as_raw_fd(),
    )?;
    let mut buffer_payload = Vec::new();
    push_u32(&mut buffer_payload, buffer_id);
    push_i32(&mut buffer_payload, 0);
    push_i32(&mut buffer_payload, WIDTH as i32);
    push_i32(&mut buffer_payload, HEIGHT as i32);
    push_i32(&mut buffer_payload, (WIDTH * 4) as i32);
    push_u32(&mut buffer_payload, WL_SHM_FORMAT_XRGB8888);
    send(
        stream,
        request(pool_id, WL_SHM_POOL_CREATE_BUFFER_OPCODE, buffer_payload),
    )?;

    let mut xdg_payload = Vec::new();
    push_u32(&mut xdg_payload, xdg_surface_id);
    push_u32(&mut xdg_payload, surface_id);
    send(
        stream,
        request(xdg_wm_base, XDG_WM_BASE_GET_XDG_SURFACE_OPCODE, xdg_payload),
    )?;
    send(
        stream,
        request(
            xdg_surface_id,
            XDG_SURFACE_GET_TOPLEVEL_OPCODE,
            u32_arg(toplevel_id),
        ),
    )?;

    let configure_serial;
    loop {
        let event = read_event(stream)?;
        if event.object_id == 1 && event.opcode == 0 {
            anyhow::bail!("Compositor reported a protocol error");
        } else if event.object_id == xdg_wm_base && event.opcode == 0 {
            let serial = read_u32(&event.payload, 0)?;
            send(
                stream,
                request(xdg_wm_base, XDG_WM_BASE_PONG_OPCODE, u32_arg(serial)),
            )?;
        } else if event.object_id == xdg_surface_id && event.opcode == 0 {
            configure_serial = read_u32(&event.payload, 0)?;
            break;
        }
    }
    send(
        stream,
        request(
            xdg_surface_id,
            XDG_SURFACE_ACK_CONFIGURE_OPCODE,
            u32_arg(configure_serial),
        ),
    )?;

    let mut attach = Vec::new();
    push_u32(&mut attach, buffer_id);
    push_i32(&mut attach, 0);
    push_i32(&mut attach, 0);
    send(stream, request(surface_id, WL_SURFACE_ATTACH_OPCODE, attach))?;
    let mut damage = Vec::new();
    push_i32(&mut damage, 0);
    push_i32(&mut damage, 0);
    push_i32(&mut damage, WIDTH as i32);
    push_i32(&mut damage, HEIGHT as i32);
    send(
        stream,
        request(surface_id, WL_SURFACE_DAMAGE_OPCODE, damage),
    )?;
    send(
        stream,
        request(surface_id, WL_SURFACE_COMMIT_OPCODE, Vec::new()),
    )?;
    Ok(())
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

fn solid(b: u8, g: u8, r: u8) -> Vec<u8> {
    let mut pixels = vec![0; (WIDTH * HEIGHT * 4) as usize];
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.copy_from_slice(&[b, g, r, 0xff]);
    }
    pixels
}

fn memory_file(bytes: &[u8]) -> anyhow::Result<File> {
    let fd = unsafe { libc::memfd_create(c"lumalla-positioned".as_ptr(), libc::MFD_CLOEXEC) };
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
    ensure!(size >= 8 && size.is_multiple_of(4), "Invalid event size {size}");
    let mut payload = vec![0; size - 8];
    stream.read_exact(&mut payload)?;
    Ok(Event {
        object_id,
        opcode,
        payload,
    })
}

fn parse_global(payload: &[u8]) -> anyhow::Result<(u32, String, u32)> {
    let name = read_u32(payload, 0)?;
    let (interface, after) = read_string(payload, 4)?;
    let version = read_u32(payload, after)?;
    Ok((name, interface, version))
}

fn u32_arg(value: u32) -> Vec<u8> {
    value.to_ne_bytes().to_vec()
}

fn push_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_ne_bytes());
}

fn push_i32(buf: &mut Vec<u8>, value: i32) {
    buf.extend_from_slice(&value.to_ne_bytes());
}

fn push_string(buf: &mut Vec<u8>, value: &str) {
    let len = value.len() + 1;
    push_u32(buf, len as u32);
    buf.extend_from_slice(value.as_bytes());
    buf.push(0);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

fn read_u32(payload: &[u8], offset: usize) -> anyhow::Result<u32> {
    let bytes: [u8; 4] = payload
        .get(offset..offset + 4)
        .context("Truncated u32")?
        .try_into()
        .unwrap();
    Ok(u32::from_ne_bytes(bytes))
}

fn read_string(payload: &[u8], offset: usize) -> anyhow::Result<(String, usize)> {
    let len = read_u32(payload, offset)? as usize;
    ensure!(len > 0, "Empty wayland string");
    let start = offset + 4;
    let end = start + len - 1;
    let bytes = payload.get(start..end).context("Truncated string")?;
    ensure!(payload.get(end) == Some(&0), "String missing NUL");
    let padded = (len + 3) & !3;
    Ok((
        String::from_utf8(bytes.to_vec()).context("String is not UTF-8")?,
        offset + 4 + padded,
    ))
}
