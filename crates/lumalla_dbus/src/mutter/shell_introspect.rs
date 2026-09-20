//! Minimal `org.gnome.Shell.Introspect` for portal window pickers.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use lumalla_shared::WindowState;
use zbus::{fdo, interface, zvariant::Value};

/// Session-bus shim so xdg-desktop-portal-gnome can list windows.
#[derive(Clone)]
pub(crate) struct ShellIntrospect {
    windows: Arc<Mutex<Vec<WindowState>>>,
}

impl ShellIntrospect {
    pub(crate) fn new(windows: Arc<Mutex<Vec<WindowState>>>) -> Self {
        Self { windows }
    }
}

#[interface(name = "org.gnome.Shell.Introspect")]
impl ShellIntrospect {
    /// Returns `a{ta{sv}}` keyed by Lumalla window id cast to `u64`.
    fn get_windows(&self) -> fdo::Result<HashMap<u64, HashMap<String, Value<'static>>>> {
        let windows = self.windows.lock().unwrap();
        let mut out = HashMap::with_capacity(windows.len());
        for window in windows.iter() {
            let mut props = HashMap::new();
            props.insert(String::from("title"), Value::from(window.title.clone()));
            props.insert(String::from("app-id"), Value::from(window.app_id.clone()));
            props.insert(String::from("wm-class"), Value::from(window.app_id.clone()));
            out.insert(u64::from(window.id), props);
        }
        Ok(out)
    }

    #[zbus(property)]
    fn animations_enabled(&self) -> bool {
        true
    }
}
