//! Mutter-compatible D-Bus interfaces for xdg-desktop-portal-gnome.

mod display_config;
pub(crate) mod screen_cast;

pub(crate) use display_config::DisplayConfig;
pub(crate) use screen_cast::{ScreenCast, complete_mutter_stream};
