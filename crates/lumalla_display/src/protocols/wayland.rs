use lumalla_wayland_protocol::{
    Ctx, ObjectId,
    protocols::{
        WlDisplay,
        wayland::{
            WlCompositor, WlCompositorCreateRegion, WlCompositorCreateSurface,
            WlDisplayGetRegistry, WlDisplaySync, WlRegistry, WlRegistryBind,
        },
    },
};

use crate::DisplayState;

impl WlDisplay for DisplayState {
    fn sync(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlDisplaySync) {
        todo!()
    }

    fn get_registry(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlDisplayGetRegistry) {
        todo!()
    }
}

impl WlRegistry for DisplayState {
    fn bind(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlRegistryBind) {
        todo!()
    }
}

impl WlCompositor for DisplayState {
    fn create_surface(
        &mut self,
        ctx: &Ctx,
        object_id: ObjectId,
        params: &WlCompositorCreateSurface,
    ) {
        todo!()
    }

    fn create_region(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlCompositorCreateRegion) {
    }
}
