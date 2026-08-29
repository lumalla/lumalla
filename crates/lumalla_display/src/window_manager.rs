//! Compositor-side window registry and placement.

use std::collections::HashMap;

use lumalla_shared::{WindowGeometryUpdate, WindowRule, WindowState};
use lumalla_wayland_protocol::{ClientId, ObjectId};

use crate::surface::SurfaceManager;
use crate::xdg::XdgManager;

pub const DEFAULT_WINDOW_WIDTH: i32 = 800;
pub const DEFAULT_WINDOW_HEIGHT: i32 = 600;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UserPlaced {
    x: bool,
    y: bool,
    width: bool,
    height: bool,
}

#[derive(Debug, Clone)]
struct ManagedWindow {
    id: u32,
    client_id: ClientId,
    wl_surface: ObjectId,
    toplevel: ObjectId,
    xdg_surface: ObjectId,
    app_id: String,
    title: String,
    user_placed: UserPlaced,
}

/// Pending configure event that must be sent to a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingConfigure {
    pub client_id: ClientId,
    pub toplevel: ObjectId,
    pub xdg_surface: ObjectId,
}

/// Geometry change to apply in the display stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometryChange {
    pub window_id: u32,
    pub client_id: ClientId,
    pub wl_surface: ObjectId,
    pub toplevel: ObjectId,
    pub xdg_surface: ObjectId,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowError {
    UnknownWindow(u32),
    NoFocusedWindow,
}

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownWindow(id) => write!(f, "unknown window id {id}"),
            Self::NoFocusedWindow => write!(f, "no focused window"),
        }
    }
}

impl std::error::Error for WindowError {}

#[derive(Debug, Default)]
pub struct WindowManager {
    next_id: u32,
    next_cascade: i32,
    windows: HashMap<u32, ManagedWindow>,
    by_toplevel: HashMap<(ClientId, ObjectId), u32>,
    focused_id: Option<u32>,
    rules: Vec<WindowRule>,
    pending_configures: Vec<PendingConfigure>,
}

impl WindowManager {
    pub fn register_toplevel(
        &mut self,
        client_id: ClientId,
        toplevel: ObjectId,
        xdg_surface: ObjectId,
        wl_surface: ObjectId,
        surface_manager: &mut SurfaceManager,
    ) -> (i32, i32) {
        self.next_id = self.next_id.saturating_add(1).max(1);
        let id = self.next_id;
        let (x, y) = self.next_cascade_position();
        let (width, height) = (DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT);

        let _ = surface_manager.set_surface_layout(client_id, wl_surface, x, y);

        self.windows.insert(
            id,
            ManagedWindow {
                id,
                client_id,
                wl_surface,
                toplevel,
                xdg_surface,
                app_id: String::new(),
                title: String::new(),
                user_placed: UserPlaced::default(),
            },
        );
        self.by_toplevel.insert((client_id, toplevel), id);
        (width, height)
    }

    pub fn unregister_toplevel(&mut self, client_id: ClientId, toplevel: ObjectId) {
        let Some(id) = self.by_toplevel.remove(&(client_id, toplevel)) else {
            return;
        };
        self.windows.remove(&id);
        if self.focused_id == Some(id) {
            self.focused_id = None;
        }
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        let ids: Vec<u32> = self
            .windows
            .values()
            .filter(|window| window.client_id == client_id)
            .map(|window| window.id)
            .collect();
        for id in ids {
            if let Some(window) = self.windows.remove(&id) {
                self.by_toplevel
                    .remove(&(window.client_id, window.toplevel));
            }
            if self.focused_id == Some(id) {
                self.focused_id = None;
            }
        }
        self.pending_configures
            .retain(|pending| pending.client_id != client_id);
    }

    pub fn set_toplevel_title(&mut self, client_id: ClientId, toplevel: ObjectId, title: String) {
        let Some(id) = self.by_toplevel.get(&(client_id, toplevel)).copied() else {
            return;
        };
        if let Some(window) = self.windows.get_mut(&id) {
            window.title = title;
        }
    }

    pub fn on_app_id_set(
        &mut self,
        client_id: ClientId,
        toplevel: ObjectId,
        app_id: String,
        surface_manager: &SurfaceManager,
        xdg_manager: &mut XdgManager,
    ) -> Vec<WindowGeometryChange> {
        let Some(id) = self.by_toplevel.get(&(client_id, toplevel)).copied() else {
            return Vec::new();
        };
        {
            let Some(window) = self.windows.get_mut(&id) else {
                return Vec::new();
            };
            window.app_id = app_id.clone();
        }

        let update = self.matching_rule_geometry(&app_id);
        if update.is_empty() {
            return Vec::new();
        }
        self.apply_update(id, update, false, surface_manager, xdg_manager)
    }

    pub fn set_focus_from_surface(&mut self, client_id: ClientId, wl_surface: ObjectId) {
        let focused = self
            .windows
            .values()
            .find(|window| window.client_id == client_id && window.wl_surface == wl_surface)
            .map(|window| window.id);
        if let Some(id) = focused {
            self.focused_id = Some(id);
        }
    }

    pub fn add_rule(&mut self, rule: WindowRule) {
        self.rules.push(rule);
    }

    pub fn clear_rules(&mut self) {
        self.rules.clear();
    }

    pub fn set_window(
        &mut self,
        id: Option<u32>,
        update: WindowGeometryUpdate,
        user_initiated: bool,
        surface_manager: &SurfaceManager,
        xdg_manager: &mut XdgManager,
    ) -> Result<Vec<WindowGeometryChange>, WindowError> {
        if update.is_empty() {
            return Ok(Vec::new());
        }
        let target = match id {
            Some(id) if id != 0 => {
                if !self.windows.contains_key(&id) {
                    return Err(WindowError::UnknownWindow(id));
                }
                id
            }
            _ => self.focused_id.ok_or(WindowError::NoFocusedWindow)?,
        };
        Ok(self.apply_update(target, update, user_initiated, surface_manager, xdg_manager))
    }

    pub fn take_pending_configures(&mut self) -> Vec<PendingConfigure> {
        std::mem::take(&mut self.pending_configures)
    }

    pub fn window_states(
        &self,
        surface_manager: &SurfaceManager,
        xdg_manager: &XdgManager,
    ) -> Vec<WindowState> {
        let mut windows: Vec<WindowState> = self
            .windows
            .values()
            .map(|window| self.snapshot_window(window, surface_manager, xdg_manager))
            .collect();
        windows.sort_by_key(|window| window.id);
        windows
    }

    pub fn focused_window_id(&self) -> Option<u32> {
        self.focused_id
    }

    fn apply_update(
        &mut self,
        id: u32,
        update: WindowGeometryUpdate,
        user_initiated: bool,
        surface_manager: &SurfaceManager,
        xdg_manager: &mut XdgManager,
    ) -> Vec<WindowGeometryChange> {
        let Some(window) = self.windows.get(&id).cloned() else {
            return Vec::new();
        };

        let mut merged = WindowGeometryUpdate::default();
        if let Some(window) = self.windows.get_mut(&id) {
            if let Some(x) = update.x
                && (user_initiated || !window.user_placed.x)
            {
                merged.x = Some(x);
                if user_initiated {
                    window.user_placed.x = true;
                }
            }
            if let Some(y) = update.y
                && (user_initiated || !window.user_placed.y)
            {
                merged.y = Some(y);
                if user_initiated {
                    window.user_placed.y = true;
                }
            }
            if let Some(width) = update.width
                && (user_initiated || !window.user_placed.width)
            {
                merged.width = Some(width);
                if user_initiated {
                    window.user_placed.width = true;
                }
            }
            if let Some(height) = update.height
                && (user_initiated || !window.user_placed.height)
            {
                merged.height = Some(height);
                if user_initiated {
                    window.user_placed.height = true;
                }
            }
        }

        if merged.is_empty() {
            return Vec::new();
        }

        let position = match (merged.x, merged.y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => {
                let current = surface_manager
                    .surface_layout(window.client_id, window.wl_surface)
                    .unwrap_or((0, 0));
                let x = merged.x.unwrap_or(current.0);
                let y = merged.y.unwrap_or(current.1);
                if merged.x.is_some() || merged.y.is_some() {
                    Some((x, y))
                } else {
                    None
                }
            }
        };

        let size = match (merged.width, merged.height) {
            (Some(width), Some(height)) => Some((width, height)),
            _ => {
                let current = xdg_manager
                    .toplevel_configure_size(window.client_id, window.toplevel)
                    .unwrap_or((DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT));
                let width = merged.width.unwrap_or(current.0);
                let height = merged.height.unwrap_or(current.1);
                if merged.width.is_some() || merged.height.is_some() {
                    Some((width, height))
                } else {
                    None
                }
            }
        };

        if let Some((width, height)) = size {
            let _ = xdg_manager.set_toplevel_configure_size(
                window.client_id,
                window.toplevel,
                width,
                height,
            );
            self.pending_configures.push(PendingConfigure {
                client_id: window.client_id,
                toplevel: window.toplevel,
                xdg_surface: window.xdg_surface,
            });
        }

        vec![WindowGeometryChange {
            window_id: window.id,
            client_id: window.client_id,
            wl_surface: window.wl_surface,
            toplevel: window.toplevel,
            xdg_surface: window.xdg_surface,
            position,
            size,
        }]
    }

    fn matching_rule_geometry(&self, app_id: &str) -> WindowGeometryUpdate {
        for rule in &self.rules {
            if rule.app_id == app_id {
                return rule.geometry();
            }
        }
        WindowGeometryUpdate::default()
    }

    fn next_cascade_position(&mut self) -> (i32, i32) {
        let pos = self.next_cascade;
        self.next_cascade = self.next_cascade.wrapping_add(32);
        (pos, pos)
    }

    fn snapshot_window(
        &self,
        window: &ManagedWindow,
        surface_manager: &SurfaceManager,
        xdg_manager: &XdgManager,
    ) -> WindowState {
        let (x, y) = surface_manager
            .surface_layout(window.client_id, window.wl_surface)
            .unwrap_or((0, 0));
        let (width, height) = xdg_manager
            .toplevel_configure_size(window.client_id, window.toplevel)
            .unwrap_or((DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT));
        WindowState {
            id: window.id,
            app_id: window.app_id.clone(),
            title: window.title.clone(),
            x,
            y,
            width,
            height,
            focused: self.focused_id == Some(window.id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_field_rule_merge_keeps_unspecified_fields() {
        let mut wm = WindowManager::default();
        wm.add_rule(WindowRule {
            app_id: String::from("app"),
            x: None,
            y: None,
            width: Some(640),
            height: Some(480),
        });
        let geometry = wm.matching_rule_geometry("app");
        assert_eq!(geometry.width, Some(640));
        assert_eq!(geometry.height, Some(480));
        assert!(geometry.x.is_none());
        assert!(geometry.y.is_none());
    }
}
