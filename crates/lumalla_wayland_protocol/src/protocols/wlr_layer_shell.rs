use super::wayland::WL_DISPLAY_ERROR_INVALID_METHOD;
use lumalla_wayland_protocol_macros::wayland_protocol;

wayland_protocol!("src/protocols/wlr-layer-shell-unstable-v1.xml");
