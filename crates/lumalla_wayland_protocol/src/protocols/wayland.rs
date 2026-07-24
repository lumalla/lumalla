use lumalla_wayland_protocol_macros::wayland_protocol;

wayland_protocol!("src/protocols/wayland.xml");

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::Read,
        num::NonZeroU32,
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    use super::*;
    use crate::{
        Ctx, ObjectId,
        buffer::{MessageHeader, Writer},
        registry::Registry,
    };

    #[test]
    fn request_opcodes_are_zero_based() {
        assert_eq!(WL_DISPLAY_SYNC_OPCODE, 0);
        assert_eq!(WL_DISPLAY_GET_REGISTRY_OPCODE, 1);
        assert_eq!(WL_REGISTRY_BIND_OPCODE, 0);
        assert_eq!(WL_COMPOSITOR_CREATE_SURFACE_OPCODE, 0);
        assert_eq!(WL_SURFACE_DESTROY_OPCODE, 0);
        assert_eq!(WL_SURFACE_COMMIT_OPCODE, 6);
    }

    #[test]
    fn opcode_zero_dispatches_first_request() {
        struct Handler(bool);

        impl WlDisplay for Handler {
            fn sync(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlDisplaySync<'_>) {
                self.0 = true;
            }

            fn get_registry(
                &mut self,
                _ctx: &mut Ctx,
                _object_id: ObjectId,
                _params: &WlDisplayGetRegistry<'_>,
            ) {
            }
        }

        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut registry = Registry::new();
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut ctx = Ctx {
            registry: &mut registry,
            writer: &mut writer,
            client_id: crate::ClientId::new(NonZeroU32::new(1).unwrap()),
        };
        let header = MessageHeader {
            object_id: ObjectId::new(NonZeroU32::new(1).unwrap()),
            size: 12,
            opcode: WL_DISPLAY_SYNC_OPCODE,
        };
        let mut fds = VecDeque::new();
        let mut handler = Handler(false);

        WlDisplay::handle_request(
            &mut handler,
            &mut ctx,
            &header,
            &2u32.to_ne_bytes(),
            &mut fds,
            1,
        )
        .unwrap();
        assert!(handler.0);
    }

    #[test]
    fn rejects_requests_newer_than_bound_version() {
        struct Handler(bool);

        impl WlShm for Handler {
            fn create_pool(
                &mut self,
                _ctx: &mut Ctx,
                _object_id: ObjectId,
                _params: &WlShmCreatePool<'_>,
            ) {
            }

            fn release(
                &mut self,
                _ctx: &mut Ctx,
                _object_id: ObjectId,
                _params: &WlShmRelease<'_>,
            ) {
                self.0 = true;
            }
        }

        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut registry = Registry::new();
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut ctx = Ctx {
            registry: &mut registry,
            writer: &mut writer,
            client_id: crate::ClientId::new(NonZeroU32::new(1).unwrap()),
        };
        let header = MessageHeader {
            object_id: ObjectId::new(NonZeroU32::new(2).unwrap()),
            size: 8,
            opcode: WL_SHM_RELEASE_OPCODE,
        };
        let mut fds = VecDeque::new();
        let mut handler = Handler(false);

        assert!(WlShm::handle_request(&mut handler, &mut ctx, &header, &[], &mut fds, 1).is_err());
        assert!(!handler.0);
    }

    #[test]
    fn generated_array_events_include_array_contents() {
        let (mut receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let keyboard = ObjectId::new(NonZeroU32::new(4).unwrap());
        let surface = ObjectId::new(NonZeroU32::new(5).unwrap());

        writer
            .wl_keyboard_enter(keyboard)
            .serial(7)
            .surface(surface)
            .keys(&[1, 2, 3, 4]);
        writer.flush().unwrap();

        let mut bytes = [0u8; 24];
        receiver.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes[0..4], &4u32.to_ne_bytes());
        assert_eq!(&bytes[4..6], &1u16.to_ne_bytes());
        assert_eq!(&bytes[6..8], &24u16.to_ne_bytes());
        assert_eq!(&bytes[8..12], &7u32.to_ne_bytes());
        assert_eq!(&bytes[12..16], &5u32.to_ne_bytes());
        assert_eq!(&bytes[16..20], &4u32.to_ne_bytes());
        assert_eq!(&bytes[20..24], &[1, 2, 3, 4]);
    }
}
