use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{
        ExtDataControlV1Protocol,
        data_control::{
            ExtDataControlDeviceV1, ExtDataControlDeviceV1Destroy,
            ExtDataControlDeviceV1SetPrimarySelection, ExtDataControlDeviceV1SetSelection,
            ExtDataControlManagerV1, ExtDataControlManagerV1CreateDataSource,
            ExtDataControlManagerV1Destroy, ExtDataControlManagerV1GetDataDevice,
            ExtDataControlOfferV1, ExtDataControlOfferV1Destroy, ExtDataControlOfferV1Receive,
            ExtDataControlSourceV1, ExtDataControlSourceV1Destroy, ExtDataControlSourceV1Offer,
            EXT_DATA_CONTROL_DEVICE_V1_ERROR_USED_SOURCE,
            EXT_DATA_CONTROL_SOURCE_V1_ERROR_INVALID_OFFER,
        },
        wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    DisplayState,
    data_control::DataControlError,
    data_device::SelectionSourceKind,
};

impl ExtDataControlV1Protocol for DisplayState {}

fn register_object(
    ctx: &mut Ctx,
    id: NewObjectId,
    interface: InterfaceIndex,
    version: u32,
) -> bool {
    if let Err(err) = ctx
        .registry
        .register_client_object_with_version(id, interface, version)
    {
        debug!("Failed to register {}: {err}", interface.interface_name());
        ctx.writer
            .wl_display_error(DISPLAY_OBJECT_ID)
            .object_id(*id)
            .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
            .message("Invalid or duplicate object ID");
        return false;
    }
    true
}

fn report_data_control_error(ctx: &mut Ctx, object_id: ObjectId, error: DataControlError) {
    let (code, message) = match error {
        DataControlError::UsedSource => (
            EXT_DATA_CONTROL_DEVICE_V1_ERROR_USED_SOURCE,
            "Source already used",
        ),
        DataControlError::InvalidOffer => (
            EXT_DATA_CONTROL_SOURCE_V1_ERROR_INVALID_OFFER,
            "offer sent after set_selection",
        ),
        DataControlError::UnknownSource
        | DataControlError::UnknownDevice
        | DataControlError::UnknownOffer
        | DataControlError::UnknownSeat => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown object"),
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

impl ExtDataControlManagerV1 for DisplayState {
    fn create_data_source(
        &mut self,
        ctx: &mut Ctx,
        _object_id: ObjectId,
        params: &ExtDataControlManagerV1CreateDataSource<'_>,
    ) {
        if !register_object(ctx, params.id(), InterfaceIndex::ExtDataControlSourceV1, 1) {
            return;
        }
        self.data_control_manager
            .create_data_source(ctx.client_id, *params.id());
    }

    fn get_data_device(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtDataControlManagerV1GetDataDevice<'_>,
    ) {
        let seat = params.seat();
        if ctx.registry.interface_index(seat) != Some(InterfaceIndex::WlSeat) {
            report_data_control_error(ctx, object_id, DataControlError::UnknownSeat);
            return;
        }
        if !register_object(ctx, params.id(), InterfaceIndex::ExtDataControlDeviceV1, 1) {
            return;
        }
        let selection = self.data_device_manager.selection_state().cloned();
        if let Err(error) = self.data_control_manager.create_data_device(
            ctx.client_id,
            *params.id(),
            seat,
            selection.as_ref(),
            ctx.registry,
            ctx.writer,
        ) {
            report_data_control_error(ctx, object_id, error);
        }
    }

    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtDataControlManagerV1Destroy<'_>,
    ) {
        self.data_control_manager
            .destroy_manager(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl ExtDataControlDeviceV1 for DisplayState {
    fn set_selection(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtDataControlDeviceV1SetSelection<'_>,
    ) {
        if let Some(source) = params.source()
            && ctx.registry.interface_index(source)
                != Some(InterfaceIndex::ExtDataControlSourceV1)
        {
            report_data_control_error(ctx, object_id, DataControlError::UnknownSource);
            return;
        }

        let new_selection = match params.source() {
            Some(source_id) => {
                match self
                    .data_control_manager
                    .take_source_for_selection(ctx.client_id, source_id)
                {
                    Ok(selection) => Some(selection),
                    Err(error) => {
                        report_data_control_error(ctx, object_id, error);
                        return;
                    }
                }
            }
            None => None,
        };

        let focused = self.seat_manager.focused_keyboard_surface().map(|(c, _)| c);
        let previous = match self.data_device_manager.replace_selection(
            new_selection,
            focused,
            Some(ctx.client_id),
            ctx.registry,
            ctx.writer,
        ) {
            Ok(previous) => previous,
            Err(error) => {
                log::error!("Failed to replace selection from data-control: {error}");
                return;
            }
        };

        if let Some(prev) = previous
            && prev.kind == SelectionSourceKind::ExtDataControlSource
        {
            self.data_control_manager.cancel_control_source(
                prev.source_client,
                prev.source,
                Some(ctx.client_id),
                ctx.writer,
            );
        }

        if let Err(error) = self.data_control_manager.notify_selection_changed(
            self.data_device_manager.selection_state(),
            Some(ctx.client_id),
            ctx.registry,
            ctx.writer,
        ) {
            report_data_control_error(ctx, object_id, error);
        }
    }

    fn set_primary_selection(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &ExtDataControlDeviceV1SetPrimarySelection<'_>,
    ) {
        // Primary selection is unsupported; protocol allows ignoring this request.
    }

    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtDataControlDeviceV1Destroy<'_>,
    ) {
        if let Err(error) = self
            .data_control_manager
            .destroy_device(ctx.client_id, object_id)
        {
            report_data_control_error(ctx, object_id, error);
            return;
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl ExtDataControlSourceV1 for DisplayState {
    fn offer(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtDataControlSourceV1Offer<'_>,
    ) {
        if let Err(error) =
            self.data_control_manager
                .offer(ctx.client_id, object_id, params.mime_type())
        {
            report_data_control_error(ctx, object_id, error);
        }
    }

    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtDataControlSourceV1Destroy<'_>,
    ) {
        let was_current = self.data_device_manager.selection_state().is_some_and(|s| {
            s.source_client == ctx.client_id
                && s.source == object_id
                && s.kind == SelectionSourceKind::ExtDataControlSource
        });
        if let Err(error) = self
            .data_control_manager
            .destroy_source(ctx.client_id, object_id)
        {
            report_data_control_error(ctx, object_id, error);
            return;
        }
        if was_current {
            let focused = self.seat_manager.focused_keyboard_surface().map(|(c, _)| c);
            let _ = self.data_device_manager.replace_selection(
                None,
                focused,
                Some(ctx.client_id),
                ctx.registry,
                ctx.writer,
            );
            let _ = self.data_control_manager.notify_selection_changed(
                None,
                Some(ctx.client_id),
                ctx.registry,
                ctx.writer,
            );
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl ExtDataControlOfferV1 for DisplayState {
    fn receive(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtDataControlOfferV1Receive<'_>,
    ) {
        if let Err(error) = self.data_control_manager.receive(
            ctx.client_id,
            object_id,
            params.mime_type(),
            params.fd(),
            ctx.writer,
        ) {
            report_data_control_error(ctx, object_id, error);
        }
    }

    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtDataControlOfferV1Destroy<'_>,
    ) {
        if let Err(error) = self
            .data_control_manager
            .destroy_offer(ctx.client_id, object_id)
        {
            report_data_control_error(ctx, object_id, error);
            return;
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }
}
