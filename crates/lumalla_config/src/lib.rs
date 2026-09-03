//! The config module provides the external `lumalla-config` process and Lua bindings over D-Bus.

#![warn(missing_docs)]

mod args;
mod callback;
mod config_watcher;
mod dbus_lua;
mod external;

pub use args::Args;
pub use callback::CallbackState;
pub use external::ExternalConfig;
