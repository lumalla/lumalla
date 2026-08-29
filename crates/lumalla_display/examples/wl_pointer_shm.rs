//! Smoke client: shell+shm surface, bind seat pointer, set_cursor, log pointer events.

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
    WL_POINTER_SET_CURSOR_OPCODE, WL_REGISTRY_BIND_OPCODE, WL_SEAT_GET_POINTER_OPCODE,
    WL_SHELL_GET_SHELL_SURFACE_OPCODE, WL_SHELL_SURFACE_PONG_OPCODE,
    WL_SHELL_SURFACE_SET_FULLSCREEN_OPCODE, WL_SHM_CREATE_POOL_OPCODE, WL_SHM_FORMAT_ARGB8888,
    WL_SHM_FORMAT_XRGB8888, WL_SHM_POOL_CREATE_BUFFER_OPCODE, WL_SURFACE_ATTACH_OPCODE,
    WL_SURFACE_COMMIT_OPCODE, WL_SURFACE_DAMAGE_OPCODE, WL_SURFACE_FRAME_OPCODE,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const CURSOR_SIZE: u32 = 16;

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

    let compositor = bind(&mut stream, &globals, "wl_compositor", 1, 4)?;
    let shm = bind(&mut stream, &globals, "wl_shm", 1, 5)?;
    let shell = bind(&mut stream, &globals, "wl_shell", 1, 6)?;
    let seat = bind(&mut stream, &globals, "wl_seat", 5, 7)?;

    send(
        &mut stream,
        request(compositor, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, u32_arg(8)),
    )?;

    let pixels = checkerboard();
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
        request(
            11,
            WL_SHELL_SURFACE_SET_FULLSCREEN_OPCODE,
            [0u32, 0, 0]
                .into_iter()
                .flat_map(u32::to_ne_bytes)
                .collect(),
        ),
    )?;

    let mut attach_payload = Vec::new();
    push_u32(&mut attach_payload, 10);
    push_i32(&mut attach_payload, 0);
    push_i32(&mut attach_payload, 0);
    send(
        &mut stream,
        request(8, WL_SURFACE_ATTACH_OPCODE, attach_payload),
    )?;
    let mut damage_payload = Vec::new();
    push_i32(&mut damage_payload, 0);
    push_i32(&mut damage_payload, 0);
    push_i32(&mut damage_payload, WIDTH as i32);
    push_i32(&mut damage_payload, HEIGHT as i32);
    send(
        &mut stream,
        request(8, WL_SURFACE_DAMAGE_OPCODE, damage_payload),
    )?;
    send(
        &mut stream,
        request(8, WL_SURFACE_FRAME_OPCODE, u32_arg(12)),
    )?;
    send(
        &mut stream,
        request(8, WL_SURFACE_COMMIT_OPCODE, Vec::new()),
    )?;

    // Cursor surface (object 14) with a small ARGB buffer.
    send(
        &mut stream,
        request(compositor, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, u32_arg(14)),
    )?;
    let cursor_pixels = solid_cursor();
    let cursor_file = memory_file(&cursor_pixels)?;
    let mut cursor_pool_payload = Vec::new();
    push_u32(&mut cursor_pool_payload, 15);
    push_i32(&mut cursor_pool_payload, cursor_pixels.len() as i32);
    send_with_fd(
        &mut stream,
        &request(shm, WL_SHM_CREATE_POOL_OPCODE, cursor_pool_payload),
        cursor_file.as_raw_fd(),
    )?;
    let mut cursor_buffer_payload = Vec::new();
    push_u32(&mut cursor_buffer_payload, 16);
    push_i32(&mut cursor_buffer_payload, 0);
    push_i32(&mut cursor_buffer_payload, CURSOR_SIZE as i32);
    push_i32(&mut cursor_buffer_payload, CURSOR_SIZE as i32);
    push_i32(&mut cursor_buffer_payload, (CURSOR_SIZE * 4) as i32);
    push_u32(&mut cursor_buffer_payload, WL_SHM_FORMAT_ARGB8888);
    send(
        &mut stream,
        request(15, WL_SHM_POOL_CREATE_BUFFER_OPCODE, cursor_buffer_payload),
    )?;
    let mut cursor_attach = Vec::new();
    push_u32(&mut cursor_attach, 16);
    push_i32(&mut cursor_attach, 0);
    push_i32(&mut cursor_attach, 0);
    send(
        &mut stream,
        request(14, WL_SURFACE_ATTACH_OPCODE, cursor_attach),
    )?;
    send(
        &mut stream,
        request(14, WL_SURFACE_COMMIT_OPCODE, Vec::new()),
    )?;

    send(
        &mut stream,
        request(seat, WL_SEAT_GET_POINTER_OPCODE, u32_arg(13)),
    )?;

    println!(
        "wl_pointer_shm ready on {}. Logging enter/motion/button; Ctrl-C to exit.",
        socket_path.display()
    );

    let mut cursor_set = false;
    loop {
        let event = match read_event(&mut stream) {
            Ok(event) => event,
            Err(error) if is_disconnect(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        if event.object_id == 1 && event.opcode == 0 {
            anyhow::bail!("Compositor reported a protocol error");
        } else if event.object_id == 11 && event.opcode == 0 {
            let serial = read_u32(&event.payload, 0)?;
            println!("Responding to wl_shell_surface.ping serial={serial}");
            if let Err(error) = send(
                &mut stream,
                request(11, WL_SHELL_SURFACE_PONG_OPCODE, u32_arg(serial)),
            ) {
                if is_disconnect(&error) {
                    return Ok(());
                }
                return Err(error);
            }
        } else if event.object_id == 13 {
            match event.opcode {
                0 => {
                    // enter
                    let serial = read_u32(&event.payload, 0)?;
                    let surface = read_u32(&event.payload, 4)?;
                    let sx = fixed_to_f32(read_i32(&event.payload, 8)?);
                    let sy = fixed_to_f32(read_i32(&event.payload, 12)?);
                    println!(
                        "wl_pointer.enter serial={serial} surface={surface} ({sx:.1},{sy:.1})"
                    );
                    if !cursor_set {
                        let mut payload = Vec::new();
                        push_u32(&mut payload, serial);
                        push_u32(&mut payload, 14);
                        push_i32(&mut payload, 0);
                        push_i32(&mut payload, 0);
                        send(
                            &mut stream,
                            request(13, WL_POINTER_SET_CURSOR_OPCODE, payload),
                        )?;
                        cursor_set = true;
                        println!("set_cursor with SHM cursor surface 14");
                    }
                }
                1 => {
                    let serial = read_u32(&event.payload, 0)?;
                    let surface = read_u32(&event.payload, 4)?;
                    println!("wl_pointer.leave serial={serial} surface={surface}");
                }
                2 => {
                    let time = read_u32(&event.payload, 0)?;
                    let sx = fixed_to_f32(read_i32(&event.payload, 4)?);
                    let sy = fixed_to_f32(read_i32(&event.payload, 8)?);
                    println!("wl_pointer.motion time={time} ({sx:.1},{sy:.1})");
                }
                3 => {
                    let serial = read_u32(&event.payload, 0)?;
                    let time = read_u32(&event.payload, 4)?;
                    let button = read_u32(&event.payload, 8)?;
                    let state = read_u32(&event.payload, 12)?;
                    println!(
                        "wl_pointer.button serial={serial} time={time} button={button} state={state}"
                    );
                }
                4 => {
                    let time = read_u32(&event.payload, 0)?;
                    let axis = read_u32(&event.payload, 4)?;
                    let value = fixed_to_f32(read_i32(&event.payload, 8)?);
                    println!("wl_pointer.axis time={time} axis={axis} value={value:.2}");
                }
                5 => println!("wl_pointer.frame"),
                _ => {}
            }
        }
    }
}

fn solid_cursor() -> Vec<u8> {
    let mut pixels = vec![0; (CURSOR_SIZE * CURSOR_SIZE * 4) as usize];
    for y in 0..CURSOR_SIZE {
        for x in 0..CURSOR_SIZE {
            let edge = x == 0 || y == 0 || x + 1 == CURSOR_SIZE || y + 1 == CURSOR_SIZE;
            let [b, g, r, a] = if edge {
                [0x00, 0x00, 0x00, 0xff]
            } else {
                [0xff, 0xff, 0xff, 0xff]
            };
            let offset = ((y * CURSOR_SIZE + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&[b, g, r, a]);
        }
    }
    pixels
}

fn fixed_to_f32(value: i32) -> f32 {
    value as f32 / 256.0
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

fn checkerboard() -> Vec<u8> {
    let mut pixels = vec![0; (WIDTH * HEIGHT * 4) as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let bright = ((x / 32) + (y / 32)).is_multiple_of(2);
            let [b, g, r] = if bright {
                [0x30, 0xd0, 0xff]
            } else {
                [0xb0, 0x30, 0x60]
            };
            let offset = ((y * WIDTH + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&[b, g, r, 0xff]);
        }
    }
    pixels
}

fn memory_file(bytes: &[u8]) -> anyhow::Result<File> {
    let fd = unsafe { libc::memfd_create(c"lumalla-wl-pointer-smoke".as_ptr(), libc::MFD_CLOEXEC) };
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

fn read_i32(bytes: &[u8], offset: usize) -> anyhow::Result<i32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("Truncated Wayland int")?
        .try_into()
        .unwrap();
    Ok(i32::from_ne_bytes(value))
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
