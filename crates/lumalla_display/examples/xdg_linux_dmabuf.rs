//! Minimal `xdg_wm_base` + stable `zwp_linux_dmabuf_v1` client for compositor smoke testing.

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
    linux_dmabuf::{
        ZWP_LINUX_BUFFER_PARAMS_V1_ADD_OPCODE, ZWP_LINUX_BUFFER_PARAMS_V1_CREATE_IMMED_OPCODE,
        ZWP_LINUX_DMABUF_V1_CREATE_PARAMS_OPCODE, ZWP_LINUX_DMABUF_V1_NAME,
    },
    wayland::{
        WL_COMPOSITOR_CREATE_SURFACE_OPCODE, WL_DISPLAY_GET_REGISTRY_OPCODE,
        WL_DISPLAY_SYNC_OPCODE, WL_REGISTRY_BIND_OPCODE, WL_SURFACE_ATTACH_OPCODE,
        WL_SURFACE_COMMIT_OPCODE, WL_SURFACE_DAMAGE_OPCODE,
    },
    xdg_shell::{
        XDG_SURFACE_ACK_CONFIGURE_OPCODE, XDG_SURFACE_GET_TOPLEVEL_OPCODE,
        XDG_WM_BASE_GET_XDG_SURFACE_OPCODE, XDG_WM_BASE_PONG_OPCODE,
    },
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
/// DRM fourcc XRGB8888 ('XR24').
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

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
    let xdg_wm_base = bind(&mut stream, &globals, "xdg_wm_base", 5)?;
    let dmabuf = bind(&mut stream, &globals, ZWP_LINUX_DMABUF_V1_NAME, 6)?;
    ensure!(
        globals
            .get(ZWP_LINUX_DMABUF_V1_NAME)
            .is_some_and(|(_, v)| *v >= 2),
        "zwp_linux_dmabuf_v1 must be at least version 2 for create_immed"
    );

    send(
        &mut stream,
        request(compositor, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, u32_arg(7)),
    )?;

    let mut xdg_surface_payload = Vec::new();
    push_u32(&mut xdg_surface_payload, 8);
    push_u32(&mut xdg_surface_payload, 7);
    send(
        &mut stream,
        request(
            xdg_wm_base,
            XDG_WM_BASE_GET_XDG_SURFACE_OPCODE,
            xdg_surface_payload,
        ),
    )?;
    send(
        &mut stream,
        request(8, XDG_SURFACE_GET_TOPLEVEL_OPCODE, u32_arg(9)),
    )?;

    let configure_serial;
    loop {
        let event = read_event(&mut stream)?;
        if event.object_id == 1 && event.opcode == 0 {
            anyhow::bail!("Compositor reported a protocol error");
        } else if event.object_id == xdg_wm_base && event.opcode == 0 {
            let serial = read_u32(&event.payload, 0)?;
            send(
                &mut stream,
                request(xdg_wm_base, XDG_WM_BASE_PONG_OPCODE, u32_arg(serial)),
            )?;
        } else if event.object_id == 8 && event.opcode == 0 {
            configure_serial = read_u32(&event.payload, 0)?;
            break;
        }
    }
    send(
        &mut stream,
        request(
            8,
            XDG_SURFACE_ACK_CONFIGURE_OPCODE,
            u32_arg(configure_serial),
        ),
    )?;

    let pixels = solid(0x40, 0xc0, 0x80);
    let file = memory_file(&pixels)?;
    send(
        &mut stream,
        request(
            dmabuf,
            ZWP_LINUX_DMABUF_V1_CREATE_PARAMS_OPCODE,
            u32_arg(10),
        ),
    )?;
    let mut add = Vec::new();
    push_u32(&mut add, 0); // plane_idx
    push_u32(&mut add, 0); // offset
    push_u32(&mut add, WIDTH * 4); // stride
    push_u32(&mut add, 0); // modifier_hi
    push_u32(&mut add, 0); // modifier_lo (LINEAR)
    send_with_fd(
        &mut stream,
        &request(10, ZWP_LINUX_BUFFER_PARAMS_V1_ADD_OPCODE, add),
        file.as_raw_fd(),
    )?;

    let mut create = Vec::new();
    push_u32(&mut create, 11); // buffer_id
    push_i32(&mut create, WIDTH as i32);
    push_i32(&mut create, HEIGHT as i32);
    push_u32(&mut create, DRM_FORMAT_XRGB8888);
    push_u32(&mut create, 0); // flags
    send(
        &mut stream,
        request(10, ZWP_LINUX_BUFFER_PARAMS_V1_CREATE_IMMED_OPCODE, create),
    )?;

    let mut attach = Vec::new();
    push_u32(&mut attach, 11);
    push_i32(&mut attach, 0);
    push_i32(&mut attach, 0);
    send(&mut stream, request(7, WL_SURFACE_ATTACH_OPCODE, attach))?;
    let mut damage = Vec::new();
    push_i32(&mut damage, 0);
    push_i32(&mut damage, 0);
    push_i32(&mut damage, WIDTH as i32);
    push_i32(&mut damage, HEIGHT as i32);
    send(&mut stream, request(7, WL_SURFACE_DAMAGE_OPCODE, damage))?;
    send(
        &mut stream,
        request(7, WL_SURFACE_COMMIT_OPCODE, Vec::new()),
    )?;

    // Drain a few events; fail fast on protocol error.
    stream.set_nonblocking(true)?;
    for _ in 0..8 {
        match read_event(&mut stream) {
            Ok(event) if event.object_id == 1 && event.opcode == 0 => {
                anyhow::bail!("Compositor reported a protocol error after dmabuf commit");
            }
            Ok(event) if event.object_id == xdg_wm_base && event.opcode == 0 => {
                let serial = read_u32(&event.payload, 0)?;
                send(
                    &mut stream,
                    request(xdg_wm_base, XDG_WM_BASE_PONG_OPCODE, u32_arg(serial)),
                )?;
            }
            Ok(_) => {}
            Err(err)
                if err
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
            {
                break;
            }
            Err(err) => return Err(err),
        }
    }

    println!(
        "Mapped xdg toplevel via {} on {}",
        ZWP_LINUX_DMABUF_V1_NAME,
        socket_path.display()
    );
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
    let version = advertised_version.min(3);
    let mut payload = Vec::new();
    push_u32(&mut payload, name);
    push_string(&mut payload, interface);
    push_u32(&mut payload, version);
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
    let fd = unsafe { libc::memfd_create(c"lumalla-dmabuf".as_ptr(), libc::MFD_CLOEXEC) };
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
