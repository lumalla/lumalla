//! Smoke client for `wl_fixes.destroy_registry`.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use anyhow::{Context, ensure};
use lumalla_wayland_protocol::protocols::wayland::{
    WL_DISPLAY_GET_REGISTRY_OPCODE, WL_DISPLAY_SYNC_OPCODE, WL_FIXES_DESTROY_OPCODE,
    WL_FIXES_DESTROY_REGISTRY_OPCODE, WL_REGISTRY_BIND_OPCODE,
};

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

    let fixes = bind(&mut stream, &globals, "wl_fixes", 1, 4)?;
    // Second registry so we can destroy it without losing the one used for bind.
    send(
        &mut stream,
        request(1, WL_DISPLAY_GET_REGISTRY_OPCODE, u32_arg(5)),
    )?;
    send(
        &mut stream,
        request(fixes, WL_FIXES_DESTROY_REGISTRY_OPCODE, u32_arg(5)),
    )?;
    send(
        &mut stream,
        request(fixes, WL_FIXES_DESTROY_OPCODE, Vec::new()),
    )?;
    send(&mut stream, request(1, WL_DISPLAY_SYNC_OPCODE, u32_arg(6)))?;

    loop {
        let event = match read_event(&mut stream) {
            Ok(event) => event,
            Err(error) if is_disconnect(&error) => break,
            Err(error) => return Err(error),
        };
        if event.object_id == 1 && event.opcode == 0 {
            anyhow::bail!("Compositor reported a protocol error");
        }
        if event.object_id == 6 && event.opcode == 0 {
            break;
        }
    }

    println!(
        "Destroyed a wl_registry via wl_fixes on {}.",
        socket_path.display()
    );
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
    version: u32,
    object_id: u32,
) -> anyhow::Result<u32> {
    let &(name, advertised_version) = globals
        .get(interface)
        .with_context(|| format!("Compositor does not advertise {interface}"))?;
    ensure!(
        advertised_version >= version,
        "{interface} advertised version {advertised_version} < required {version}"
    );
    let mut payload = Vec::new();
    push_u32(&mut payload, name);
    push_string(&mut payload, interface);
    push_u32(&mut payload, version);
    push_u32(&mut payload, object_id);
    send(stream, request(2, WL_REGISTRY_BIND_OPCODE, payload))?;
    Ok(object_id)
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

fn push_string(bytes: &mut Vec<u8>, value: &str) {
    let length = value.len() + 1;
    push_u32(bytes, length as u32);
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    bytes.resize((bytes.len() + 3) & !3, 0);
}
