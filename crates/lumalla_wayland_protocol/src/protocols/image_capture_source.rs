use super::wayland::WL_DISPLAY_ERROR_INVALID_METHOD;
use lumalla_wayland_protocol_macros::wayland_protocol;

wayland_protocol!("src/protocols/ext-image-capture-source-v1.xml");
