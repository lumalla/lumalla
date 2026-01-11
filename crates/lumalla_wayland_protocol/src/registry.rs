use std::{
    collections::{HashMap, VecDeque},
    num::NonZeroU32,
    os::fd::RawFd,
};

use crate::{
    NewObjectId, ObjectId,
    buffer::{MessageHeader, Writer},
    client::Ctx,
    protocols::{WaylandProtocol, WlDisplay, wayland::*},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InterfaceIndex {
    WlDisplay,
    WlRegistry,
    WlCallback,
    WlCompositor,
    WlShmPool,
    WlShm,
    WlBuffer,
    WlDataOffer,
    WlDataSource,
    WlDataDevice,
    WlDataDeviceManager,
    WlShell,
    WlShellSurface,
    WlSurface,
    WlSeat,
    WlPointer,
    WlKeyboard,
    WlTouch,
    WlOutput,
    WlRegion,
    WlSubcompositor,
    WlSubsurface,
    WlFixes,
}

impl InterfaceIndex {
    pub fn interface_name(&self) -> &'static str {
        match self {
            InterfaceIndex::WlDisplay => WL_DISPLAY_NAME,
            InterfaceIndex::WlRegistry => WL_REGISTRY_NAME,
            InterfaceIndex::WlCallback => WL_CALLBACK_NAME,
            InterfaceIndex::WlCompositor => WL_COMPOSITOR_NAME,
            InterfaceIndex::WlShmPool => WL_SHM_POOL_NAME,
            InterfaceIndex::WlShm => WL_SHM_NAME,
            InterfaceIndex::WlBuffer => WL_BUFFER_NAME,
            InterfaceIndex::WlDataOffer => WL_DATA_OFFER_NAME,
            InterfaceIndex::WlDataSource => WL_DATA_SOURCE_NAME,
            InterfaceIndex::WlDataDevice => WL_DATA_DEVICE_NAME,
            InterfaceIndex::WlDataDeviceManager => WL_DATA_DEVICE_MANAGER_NAME,
            InterfaceIndex::WlShell => WL_SHELL_NAME,
            InterfaceIndex::WlShellSurface => WL_SHELL_SURFACE_NAME,
            InterfaceIndex::WlSurface => WL_SURFACE_NAME,
            InterfaceIndex::WlSeat => WL_SEAT_NAME,
            InterfaceIndex::WlPointer => WL_POINTER_NAME,
            InterfaceIndex::WlKeyboard => WL_KEYBOARD_NAME,
            InterfaceIndex::WlTouch => WL_TOUCH_NAME,
            InterfaceIndex::WlOutput => WL_OUTPUT_NAME,
            InterfaceIndex::WlRegion => WL_REGION_NAME,
            InterfaceIndex::WlSubcompositor => WL_SUBCOMPOSITOR_NAME,
            InterfaceIndex::WlSubsurface => WL_SUBSURFACE_NAME,
            InterfaceIndex::WlFixes => WL_FIXES_NAME,
        }
    }

    pub fn interface_version(&self) -> u32 {
        match self {
            InterfaceIndex::WlDisplay => WL_DISPLAY_VERSION,
            InterfaceIndex::WlRegistry => WL_REGISTRY_VERSION,
            InterfaceIndex::WlCallback => WL_CALLBACK_VERSION,
            InterfaceIndex::WlCompositor => WL_COMPOSITOR_VERSION,
            InterfaceIndex::WlShmPool => WL_SHM_POOL_VERSION,
            InterfaceIndex::WlShm => WL_SHM_VERSION,
            InterfaceIndex::WlBuffer => WL_BUFFER_VERSION,
            InterfaceIndex::WlDataOffer => WL_DATA_OFFER_VERSION,
            InterfaceIndex::WlDataSource => WL_DATA_SOURCE_VERSION,
            InterfaceIndex::WlDataDevice => WL_DATA_DEVICE_VERSION,
            InterfaceIndex::WlDataDeviceManager => WL_DATA_DEVICE_MANAGER_VERSION,
            InterfaceIndex::WlShell => WL_SHELL_VERSION,
            InterfaceIndex::WlShellSurface => WL_SHELL_SURFACE_VERSION,
            InterfaceIndex::WlSurface => WL_SURFACE_VERSION,
            InterfaceIndex::WlSeat => WL_SEAT_VERSION,
            InterfaceIndex::WlPointer => WL_POINTER_VERSION,
            InterfaceIndex::WlKeyboard => WL_KEYBOARD_VERSION,
            InterfaceIndex::WlTouch => WL_TOUCH_VERSION,
            InterfaceIndex::WlOutput => WL_OUTPUT_VERSION,
            InterfaceIndex::WlRegion => WL_REGION_VERSION,
            InterfaceIndex::WlSubcompositor => WL_SUBCOMPOSITOR_VERSION,
            InterfaceIndex::WlSubsurface => WL_SUBSURFACE_VERSION,
            InterfaceIndex::WlFixes => WL_FIXES_VERSION,
        }
    }
}

const MIN_SERVER_OBJECT_ID: ObjectId =
    ObjectId::new(unsafe { NonZeroU32::new_unchecked(0xFF000000) });
pub const DISPLAY_OBJECT_ID: ObjectId = ObjectId::new(unsafe { NonZeroU32::new_unchecked(1) });

#[derive(Debug)]
pub struct Registry {
    objects: HashMap<ObjectId, InterfaceIndex>,
    next_object_id: ObjectId,
    freed_object_ids: Vec<ObjectId>,
}

impl Registry {
    pub fn new() -> Self {
        let mut registry = Self {
            objects: HashMap::new(),
            next_object_id: MIN_SERVER_OBJECT_ID,
            freed_object_ids: Vec::new(),
        };
        registry.register_object(
            NewObjectId::new(DISPLAY_OBJECT_ID),
            InterfaceIndex::WlDisplay,
        );
        registry
    }

    pub fn interface_index(&self, object_id: ObjectId) -> Option<InterfaceIndex> {
        self.objects.get(&object_id).copied()
    }

    pub fn register_object(&mut self, object_id: NewObjectId, interface_index: InterfaceIndex) {
        self.objects.insert(*object_id, interface_index);
    }

    pub fn create_object(&mut self, interface_index: InterfaceIndex) -> anyhow::Result<ObjectId> {
        let object_id = self.next_object_id()?;
        self.objects.insert(object_id, interface_index);
        Ok(object_id)
    }

    fn next_object_id(&mut self) -> anyhow::Result<ObjectId> {
        if let Some(object_id) = self.freed_object_ids.pop() {
            return Ok(object_id);
        }
        let object_id = self.next_object_id;
        let next_object_id = self.next_object_id.get() + 1;
        if next_object_id == 0 {
            anyhow::bail!("Ran out of object IDs");
        }
        self.next_object_id = ObjectId::new(
            NonZeroU32::new(next_object_id).ok_or(anyhow::anyhow!("Ran out of object ids"))?,
        );
        Ok(object_id)
    }

    pub fn free_object(&mut self, object_id: ObjectId, writer: &mut Writer) {
        self.objects.remove(&object_id);
        if object_id >= MIN_SERVER_OBJECT_ID {
            self.freed_object_ids.push(object_id);
        } else {
            // The spec says that only objects created by the client should be acknowledged
            writer
                .wl_display_delete_id(DISPLAY_OBJECT_ID)
                .id(object_id.get());
        }
    }

    pub fn iter_object_ids_of_interface(
        &self,
        interface: InterfaceIndex,
    ) -> impl Iterator<Item = ObjectId> {
        self.objects
            .iter()
            .filter(move |(_, interface_index)| interface == **interface_index)
            .map(|(object_id, _)| *object_id)
    }
}

pub trait RequestHandler {
    fn handle_request(
        &mut self,
        handler: InterfaceIndex,
        ctx: &mut Ctx,
        header: &MessageHeader,
        data: &[u8],
        fds: &mut VecDeque<RawFd>,
    ) -> anyhow::Result<()>;
}

impl<T> RequestHandler for T
where
    T: WaylandProtocol,
{
    fn handle_request(
        &mut self,
        handler: InterfaceIndex,
        ctx: &mut Ctx,
        header: &MessageHeader,
        data: &[u8],
        fds: &mut VecDeque<RawFd>,
    ) -> anyhow::Result<()> {
        match handler {
            InterfaceIndex::WlDisplay => WlDisplay::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlRegistry => WlRegistry::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlCallback => {
                write_invalid_method_error(ctx, header.object_id);
                anyhow::bail!("Invalid method");
            }
            InterfaceIndex::WlCompositor => {
                WlCompositor::handle_request(self, ctx, header, data, fds)
            }
            InterfaceIndex::WlShmPool => WlShmPool::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlShm => WlShm::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlBuffer => WlBuffer::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlDataOffer => {
                WlDataOffer::handle_request(self, ctx, header, data, fds)
            }
            InterfaceIndex::WlDataSource => {
                WlDataSource::handle_request(self, ctx, header, data, fds)
            }
            InterfaceIndex::WlDataDevice => {
                WlDataDevice::handle_request(self, ctx, header, data, fds)
            }
            InterfaceIndex::WlDataDeviceManager => {
                WlDataDeviceManager::handle_request(self, ctx, header, data, fds)
            }
            InterfaceIndex::WlShell => WlShell::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlShellSurface => {
                WlShellSurface::handle_request(self, ctx, header, data, fds)
            }
            InterfaceIndex::WlSurface => WlSurface::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlSeat => WlSeat::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlPointer => WlPointer::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlKeyboard => WlKeyboard::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlTouch => WlTouch::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlOutput => WlOutput::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlRegion => WlRegion::handle_request(self, ctx, header, data, fds),
            InterfaceIndex::WlSubcompositor => {
                WlSubcompositor::handle_request(self, ctx, header, data, fds)
            }
            InterfaceIndex::WlSubsurface => {
                WlSubsurface::handle_request(self, ctx, header, data, fds)
            }
            InterfaceIndex::WlFixes => WlFixes::handle_request(self, ctx, header, data, fds),
        }
    }
}

fn write_invalid_method_error(ctx: &mut Ctx, object_id: ObjectId) {
    ctx.writer
        .wl_display_error(object_id)
        .object_id(object_id)
        .code(WL_DISPLAY_ERROR_INVALID_METHOD)
        .message("Invalid method");
}
