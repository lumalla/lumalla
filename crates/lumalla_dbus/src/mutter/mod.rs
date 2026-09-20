//! Mutter-compatible D-Bus interfaces for xdg-desktop-portal-gnome.

mod display_config;
pub(crate) mod screen_cast;
mod shell_introspect;

pub(crate) use display_config::DisplayConfig;
pub(crate) use screen_cast::{ScreenCast, complete_mutter_stream};
pub(crate) use shell_introspect::ShellIntrospect;
