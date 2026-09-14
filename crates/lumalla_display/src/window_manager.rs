//! Compositor-side window registry and placement.

use std::collections::HashMap;

use lumalla_shared::{WindowGeometryUpdate, WindowRule, WindowState, Zone};
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
    zone: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowError {
    UnknownWindow(u32),
    NoFocusedWindow,
    UnknownZone(String),
}

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownWindow(id) => write!(f, "unknown window id {id}"),
            Self::NoFocusedWindow => write!(f, "no focused window"),
            Self::UnknownZone(name) => write!(f, "unknown zone {name}"),
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
    zones: Vec<Zone>,
    pending_configures: Vec<PendingConfigure>,
}

impl WindowManager {
    pub fn add_zone(&mut self, zone: Zone) {
        if zone.default {
            for existing in &mut self.zones {
                existing.default = false;
            }
        }
        if let Some(existing) = self.zones.iter_mut().find(|z| z.name == zone.name) {
            *existing = zone;
        } else {
            self.zones.push(zone);
        }
        self.ensure_default_zone();
    }

    pub fn remove_zone(&mut self, name: &str) -> bool {
        let Some(index) = self.zones.iter().position(|zone| zone.name == name) else {
            return false;
        };
        let was_default = self.zones[index].default;
        self.zones.remove(index);
        if was_default {
            self.ensure_default_zone();
        }
        true
    }

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

        let (zone_name, x, y, width, height) = if let Some(zone) = self.default_zone() {
            let (x, y, width, height) = zone.place();
            (Some(zone.name.clone()), x, y, width, height)
        } else {
            let (x, y) = self.next_cascade_position();
            (
                None,
                x,
                y,
                DEFAULT_WINDOW_WIDTH,
                DEFAULT_WINDOW_HEIGHT,
            )
        };

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
                zone: zone_name,
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

        let Some(rule) = self.matching_rule(&app_id).cloned() else {
            return Vec::new();
        };

        let mut changes = Vec::new();
        if let Some(zone_name) = rule.zone.as_deref()
            && let Ok(zone_changes) =
                self.assign_window_to_zone(id, zone_name, false, surface_manager, xdg_manager)
        {
            changes.extend(zone_changes);
        }

        let geometry = rule.geometry();
        if !geometry.is_empty() {
            changes.extend(self.apply_update(id, geometry, false, surface_manager, xdg_manager));
        }
        changes
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
        let target = self.resolve_window_id(id)?;
        Ok(self.apply_update(target, update, user_initiated, surface_manager, xdg_manager))
    }

    pub fn add_window_to_zone(
        &mut self,
        id: Option<u32>,
        zone_name: &str,
        surface_manager: &SurfaceManager,
        xdg_manager: &mut XdgManager,
    ) -> Result<Vec<WindowGeometryChange>, WindowError> {
        let target = self.resolve_window_id(id)?;
        self.assign_window_to_zone(target, zone_name, true, surface_manager, xdg_manager)
    }

    pub fn remove_window_from_zone(&mut self, id: Option<u32>) -> Result<(), WindowError> {
        let target = self.resolve_window_id(id)?;
        if let Some(window) = self.windows.get_mut(&target) {
            window.zone = None;
        }
        Ok(())
    }

    pub fn window_zone(&self, id: u32) -> Option<&str> {
        self.windows.get(&id).and_then(|window| window.zone.as_deref())
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

    /// Resolve a window id to its Wayland surface. `None` / `0` → focused window.
    pub fn resolve_surface(&self, id: Option<u32>) -> Result<(ClientId, ObjectId), WindowError> {
        let target = self.resolve_window_id(id)?;
        let window = self
            .windows
            .get(&target)
            .ok_or(WindowError::UnknownWindow(target))?;
        Ok((window.client_id, window.wl_surface))
    }

    fn assign_window_to_zone(
        &mut self,
        id: u32,
        zone_name: &str,
        user_initiated: bool,
        surface_manager: &SurfaceManager,
        xdg_manager: &mut XdgManager,
    ) -> Result<Vec<WindowGeometryChange>, WindowError> {
        let zone = self
            .zones
            .iter()
            .find(|zone| zone.name == zone_name)
            .cloned()
            .ok_or_else(|| WindowError::UnknownZone(zone_name.to_owned()))?;
        if let Some(window) = self.windows.get_mut(&id) {
            window.zone = Some(zone.name.clone());
        }
        let (x, y, width, height) = zone.place();
        Ok(self.apply_update(
            id,
            WindowGeometryUpdate {
                x: Some(x),
                y: Some(y),
                width: Some(width),
                height: Some(height),
            },
            user_initiated,
            surface_manager,
            xdg_manager,
        ))
    }

    fn resolve_window_id(&self, id: Option<u32>) -> Result<u32, WindowError> {
        match id {
            Some(id) if id != 0 => {
                if !self.windows.contains_key(&id) {
                    return Err(WindowError::UnknownWindow(id));
                }
                Ok(id)
            }
            _ => self.focused_id.ok_or(WindowError::NoFocusedWindow),
        }
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

    fn matching_rule(&self, app_id: &str) -> Option<&WindowRule> {
        self.rules.iter().find(|rule| rule.app_id == app_id)
    }

    fn matching_rule_geometry(&self, app_id: &str) -> WindowGeometryUpdate {
        self.matching_rule(app_id)
            .map(WindowRule::geometry)
            .unwrap_or_default()
    }

    fn default_zone(&self) -> Option<&Zone> {
        self.zones
            .iter()
            .find(|zone| zone.default)
            .or_else(|| self.zones.first())
    }

    fn ensure_default_zone(&mut self) {
        if self.zones.is_empty() {
            return;
        }
        if !self.zones.iter().any(|zone| zone.default) {
            self.zones[0].default = true;
        }
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
    use std::num::NonZeroU32;

    use super::*;
    use lumalla_shared::CompositionStrategy;

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn free_zone(name: &str, x: i32, y: i32, default: bool, w: i32, h: i32) -> Zone {
        Zone::new(
            name.to_owned(),
            x,
            y,
            default,
            CompositionStrategy::Free {
                default_width: w,
                default_height: h,
            },
        )
    }

    fn register_with_surface(
        wm: &mut WindowManager,
        surfaces: &mut SurfaceManager,
        xdg: &mut XdgManager,
    ) -> (i32, i32) {
        let c = client(1);
        let wl = object(12);
        let xdg_surface = object(11);
        let toplevel = object(10);
        surfaces.create_surface(c, wl);
        xdg.create_xdg_surface(c, xdg_surface, wl).unwrap();
        let (width, height) = wm.register_toplevel(c, toplevel, xdg_surface, wl, surfaces);
        xdg.create_toplevel(c, toplevel, xdg_surface, width, height)
            .unwrap();
        (width, height)
    }

    #[test]
    fn per_field_rule_merge_keeps_unspecified_fields() {
        let mut wm = WindowManager::default();
        wm.add_rule(WindowRule {
            app_id: String::from("app"),
            zone: None,
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

    #[test]
    fn add_and_remove_zone_promotes_default() {
        let mut wm = WindowManager::default();
        wm.add_zone(free_zone("a", 0, 0, true, 800, 600));
        wm.add_zone(free_zone("b", 100, 100, false, 640, 480));
        assert!(wm.remove_zone("a"));
        assert_eq!(wm.default_zone().map(|z| z.name.as_str()), Some("b"));
        assert!(wm.default_zone().unwrap().default);
    }

    #[test]
    fn register_toplevel_uses_default_zone() {
        let mut wm = WindowManager::default();
        let mut surfaces = SurfaceManager::default();
        let mut xdg = XdgManager::default();
        wm.add_zone(free_zone("main", 40, 50, true, 900, 700));

        let (width, height) = register_with_surface(&mut wm, &mut surfaces, &mut xdg);
        assert_eq!((width, height), (900, 700));
        assert_eq!(
            surfaces.surface_layout(client(1), object(12)),
            Some((40, 50))
        );
        assert_eq!(wm.window_zone(1), Some("main"));
    }

    #[test]
    fn register_toplevel_falls_back_to_cascade_without_zones() {
        let mut wm = WindowManager::default();
        let mut surfaces = SurfaceManager::default();
        let mut xdg = XdgManager::default();
        let (width, height) = register_with_surface(&mut wm, &mut surfaces, &mut xdg);
        assert_eq!(
            (width, height),
            (DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT)
        );
        assert_eq!(surfaces.surface_layout(client(1), object(12)), Some((0, 0)));
        assert_eq!(wm.window_zone(1), None);
    }

    #[test]
    fn add_and_remove_window_zone_membership() {
        let mut wm = WindowManager::default();
        let mut surfaces = SurfaceManager::default();
        let mut xdg = XdgManager::default();
        wm.add_zone(free_zone("main", 10, 20, true, 400, 300));
        wm.add_zone(free_zone("side", 500, 0, false, 200, 200));

        let _ = register_with_surface(&mut wm, &mut surfaces, &mut xdg);
        wm.focused_id = Some(1);

        let changes = wm
            .add_window_to_zone(None, "side", &surfaces, &mut xdg)
            .unwrap();
        assert_eq!(wm.window_zone(1), Some("side"));
        assert_eq!(changes[0].position, Some((500, 0)));
        assert_eq!(changes[0].size, Some((200, 200)));

        wm.remove_window_from_zone(None).unwrap();
        assert_eq!(wm.window_zone(1), None);
    }

    #[test]
    fn rule_with_zone_and_geometry_overlay() {
        let mut wm = WindowManager::default();
        let mut surfaces = SurfaceManager::default();
        let mut xdg = XdgManager::default();
        wm.add_zone(free_zone("main", 10, 20, true, 400, 300));
        wm.add_rule(WindowRule {
            app_id: String::from("app"),
            zone: Some(String::from("main")),
            x: None,
            y: None,
            width: Some(640),
            height: None,
        });

        let _ = register_with_surface(&mut wm, &mut surfaces, &mut xdg);
        wm.windows.get_mut(&1).unwrap().zone = None;
        let _ = surfaces.set_surface_layout(client(1), object(12), 0, 0);

        let changes =
            wm.on_app_id_set(client(1), object(10), String::from("app"), &surfaces, &mut xdg);
        assert_eq!(wm.window_zone(1), Some("main"));
        assert!(changes.iter().any(|c| c.position == Some((10, 20))));
        assert!(changes.iter().any(|c| c.size == Some((640, 300))));
    }
}
