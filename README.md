<div align="center">
  <a href="https://lumalla.org">
    <img src="assets/logo.svg" alt="Logo" width="80" height="80">
  </a>

  <h3 align="center">Lumalla</h3>

  <p align="center">
    Window manager for productivity and efficiency.
  </p>
</div>

## Requirements

- Linux with io_uring (kernel 5.19+ recommended for async cancel-by-fd; Accept / RecvMsg / SendMsg / Timeout ABS)

## NixOS and home-manager

The flake exposes modules that wire seatd, xdg-desktop-portal, packages, and a `start_lumalla` helper so you do not need to hand-roll session plumbing.

**NixOS** (portals + seatd):

```nix
{
  imports = [ inputs.lumalla.nixosModules.default ];
  programs.lumalla.enable = true;
}
```

**home-manager** (packages, `start_lumalla`, optional config install):

```nix
{
  imports = [ inputs.lumalla.homeManagerModules.default ];
  programs.lumalla.enable = true;
  programs.lumalla.configFile = ./lumalla-init.lua; # or configText = ''...'';
}
```

Import both when using home-manager as a NixOS module. After rebuild, log in on a TTY and run `start_lumalla`.

`start_lumalla` sets `XDG_CURRENT_DESKTOP=lumalla`, starts `nixos-fake-graphical-session.target`, and launches `lumalla -- lumalla-config --config ~/.config/lumalla/init.lua --repl`.

Your Lua config should still push compositor env into the user session once the compositor is up (portals need `WAYLAND_DISPLAY`), and optionally spawn `xwayland-satellite`:

```lua
lum.on_startup(function()
  lum.spawn({
    command = "dbus-update-activation-environment",
    args = {
      "--systemd",
      "WAYLAND_DISPLAY",
      "XDG_CURRENT_DESKTOP",
      "XDG_SESSION_TYPE",
      "XDG_SESSION_DESKTOP",
    },
  })
  lum.spawn({ command = "xwayland-satellite", args = { ":1" } })
  -- ...
end)
```

## Lua API

Configuration runs in a separate `lumalla_config` process. Scripts load the module with `local lum = require("lumalla")` and talk to the compositor over D-Bus.

Config files are passed with `--config <path>`, or loaded from `~/.config/lumalla/*.lua` when no path is given. See `init.lua` for a full example.

Default keymaps (not Lua-callable): Ctrl+Alt+Backspace quits; Ctrl+Alt+F1–F12 switch VTs.

### Lifecycle

| Function | Description |
| --- | --- |
| `on_startup(callback)` | Called once when the compositor is ready. `callback()` takes no args. Returns a callback id. |
| `on_connector_change(callback)` | Called once at Ready and on DRM hotplug. `callback(devices)` receives the same tables as `get_drm_devices`. Does **not** fire when the config adds/removes logical outputs or views, or when calling `set_output_configs` / `set_render_device`. Returns a callback id. |
| `on_cursor_move(callback \| {callback, consume?, mods?})` | Called when the pointer moves. `callback(x, y, dx, dy)` receives **monitor/output-local** absolute coordinates and the relative delta for that input batch. Emission is opt-in and coalesced per input batch. `consume` (default `false`) withholds the event from Wayland clients. `mods` is a pipe-separated string like `map_key` (empty = always). Fire and consume only while required mods are held. Returns a callback id. |
| `on_cursor_click(callback \| {callback, consume?, mods?})` | Called on pointer button press/release. `callback(x, y, button, pressed)` — coordinates are monitor/output-local; `button` is `0` for left (same as `click`), otherwise a Linux `BTN_*` code; `pressed` is `true` on press. `consume` defaults to `false`. `mods` as above. Returns a callback id. |
| `on_cursor_scroll(callback \| {callback, consume?, mods?})` | Called on pointer scroll. `callback(x, y, axis, value)` — coordinates are monitor/output-local; `axis` is `0` vertical / `1` horizontal; `value` is the libinput scroll delta. `consume` defaults to `false`. `mods` as above. Returns a callback id. |
| `off(callback_id)` | Stop any callback previously returned by `map_key`, `on_startup`, `on_cursor_*`, etc. |

### Session

| Function | Description |
| --- | --- |
| `quit()` / `shutdown()` | Shut down the compositor. |
| `toggle_debug_ui()` | Toggle the debug overlay. |
| `start_pipewire_stream({x, y, width, height, name?, max_fps?})` | Start a PipeWire stream of a region. Returns `{id, node_id}`. Empty `name` becomes `"Lumalla"`; `max_fps` of `0` defaults to 30. |
| `start_pipewire_stream_window({id?, name?, max_fps?})` | Start a PipeWire stream of one window's surfaces (isolated). `id` of `0`/omitted uses the focused window. Returns `{id, node_id}`. |
| `stop_pipewire_stream(stream_id)` | Stop a stream by id from `start_pipewire_stream` / `start_pipewire_stream_window`. |

### Keyboard

| Function | Description |
| --- | --- |
| `set_xkb({rules?, model?, layout?, variant?, options?})` | Set XKB RMLVO. Call before `map_key` so key names resolve against the active layout. |
| `map_key({key, mods?, on?, consume?/suppress?, callback})` | Bind a key. `mods` is a pipe-separated string: `"shift"`, `"ctrl"`, `"alt"`, `"logo"` / `"super"`. `on` is `"down"`/`"press"` (default) or `"up"`/`"release"`. `consume` / `suppress` default to `true`. Returns a callback id for `off`. |

### Outputs and views

| Function | Description |
| --- | --- |
| `get_outputs()` | Return current logical outputs. |
| `add_output(output)` | Add a logical output (no views). |
| `remove_output(name)` | Remove an output by name. |
| `add_view(output_name, view)` | Add or replace a view on an output. |
| `remove_view(output_name, view_name)` | Remove a view. |
| `add_output_with_view(output)` | `add_output` plus a `"main"` view mapping `x`/`y`/`width`/`height` as the source rectangle. |

**Output table:** `name`, `width`, `height`, and optionally `description`, `x`, `y`, `scale` (default 1), `refresh_mhz` (default 60000), `mm_width` / `physical_width_mm`, `mm_height` / `physical_height_mm`, `virtual` / `is_virtual`. Returned tables use `mm_width`, `mm_height`, and `virtual`.

**View table:** `name`, plus either nested `source` / `dest` `{x, y, width, height}` or flat `source_x`… / `dest_x`… fields.

### DRM

| Function | Description |
| --- | --- |
| `get_drm_devices()` | Return DRM primary nodes with connectors and modes. |
| `set_render_device(path)` | Select the Vulkan render device. `nil` or `""` selects automatically. |
| `set_output_configs({{name, enabled?, mode?}, ...})` | Enable/disable connectors and pick a mode by name. `enabled` defaults to `true`. |

**DRM device:** `path`, `selected_render_device`, `connectors[]` with `name`, `connector_id`, `connector_type`, `connected`, `mm_width`, `mm_height`, `modes[]` (`name`, `width`, `height`, `refresh_hz`, `preferred`).

### Windows, zones, and rules

| Function | Description |
| --- | --- |
| `get_windows()` | Return `{id, app_id, title, x, y, width, height, focused, zone?, stack}` for each window. `zone` is the zone name when assigned, otherwise `nil`. `stack` is paint order (higher = closer to top). |
| `get_focused_window()` | Focused window id, or `nil`. |
| `set_window({id?, x?, y?, width?, height?})` | Set geometry. Omitted fields stay unchanged; omit `id` (or pass `0`) for the focused window. |
| `focus_window({id?, raise?})` | Focus a window (`raise` defaults to `false`). |
| `raise_window({id?})` | Raise without changing focus. |
| `close_window({id?})` | Ask the client to close via `xdg_toplevel.close` (client may ignore / prompt). Omit `id` for the focused window. |
| `add_zone({name, x?, y?, default?, composition?, default_width?, default_height?})` | Add or replace a zone. `composition` defaults to `"free"`; size defaults to 800×600. |
| `remove_zone(name)` | Remove a zone. |
| `add_guide({name, kind?, layer?, ...})` | Add or replace a guide (helper line/box). See below. |
| `remove_guide(name)` | Remove a guide. |
| `clear_guides()` | Remove all guides. |
| `get_guides()` | Return current guides as tables. |
| `add_window_to_zone({id?, zone})` | Assign a window to a zone. |
| `remove_window_from_zone({id?})` | Clear zone membership. |
| `add_window_rule({app_id, zone?, x?, y?, width?, height?})` | Placement rule for an `app_id`. |
| `clear_window_rules()` | Clear all window rules. |

**Guide table:** `name` (required), `kind` (`"box"` default or `"line"`), `layer` (`"below"` default or `"above"`), `color` `{r,g,b,a}` (0–255; default amber), `stroke` (scene px, default 1), optional `fill` `{r,g,b,a}` (boxes), optional `label` (bitmap `A–Za–z0–9`; other chars including space are blank cells), optional `label_color`. Boxes use `x`,`y`,`width`,`height`. Lines use `x1`,`y1`,`x2`,`y2`. Coordinates are compositor/scene space.

### Spawn, forms, and input injection

| Function | Description |
| --- | --- |
| `spawn({command, args?})` | Spawn a process with the compositor’s Wayland environment (plus any vars from `set_extra_env`). |
| `ui(spec)` | Open a small form via the `lumalla-ui` helper (must be on `PATH`, or set `LUMALLA_UI`). Uses wgpu/Vulkan Wayland (not OpenGL). Returns immediately; results arrive in callbacks. See below. |
| `set_extra_env(name, value)` | Set an environment variable applied to all future `spawn` calls (e.g. `DISPLAY` for xwayland-satellite). |
| `sleep(seconds)` | Blocking sleep in the config process (no D-Bus round-trip). |
| `key(name)` | Press and release a named key. |
| `type(text)` | Type UTF-8 text as key presses. |
| `pointer_move(x, y)` | Absolute pointer move. `x`/`y` are global compositor (scene) coordinates; they are mapped through the active view onto the monitor. |
| `click(x, y, button?)` | Click at scene coordinates (`button` defaults to `0`). Mapped through the active view like `pointer_move`. |
| `screenshot(x, y, width, height, path)` | Capture a region to a PNG file. |

#### `lum.ui` form DSL

Opens a Wayland form window as a separate client. Only one form may be open at a time.

**Spec keys:** `title`, `fields`, `actions`, `on_submit(values, action)`, `on_cancel()`, and optionally `validate(values)`, `on_change(id, value, values)`, `on_action(action, values)`.

If any of `validate` / `on_change` / `on_action` is set, the helper runs in **interactive** mode (NDJSON over stdin/stdout). Otherwise it is **simple**: the process exits with a JSON result and `on_submit` / `on_cancel` run once.

**Field types:** `text` (`placeholder`, `default`, `password`, `focus`), `choice` (`options` as strings or `{id, label}`, `default`), `toggle` (`default`), `label` (static; omitted from `values`).

**Actions:** `{ id, label?, primary?, submit? }`. `submit = true` finishes the form. Escape / window close / a `cancel` action calls `on_cancel`.

```lua
lum.map_key({
  key = "n",
  mods = "logo",
  callback = function()
    lum.ui({
      title = "New view",
      fields = {
        { id = "name", type = "text", label = "Name", placeholder = "pip", focus = true },
      },
      actions = {
        { id = "cancel", label = "Cancel" },
        { id = "ok", label = "Create", primary = true, submit = true },
      },
      on_submit = function(values, action)
        if action ~= "ok" then return end
        -- use values.name with lum.add_view(...)
      end,
    })
  end,
})
```

`validate` may return `true`, `false, "message"`, or an update table `{ fields = { { id = "...", error = "..." } } }`. `on_change` / `on_action` use the same return shapes; `on_change` updates are pushed asynchronously to the helper.

## Debugging compositor issues

For bugs that only show up with real clients (focus loss, popups vanishing, bad hit-testing), prefer a **headless Lumalla + the offending app** over guessing from code alone.

### 1. Reproduce under a virtual output

Build and run headless so Lumalla does not need a seat/TTY. Use a Lua config that enables a virtual output and spawns the client:

```bash
cargo build -p lumalla -p lumalla_config --profile profiling

./target/profiling/lumalla --headless -- \
  ./target/profiling/lumalla_config \
  --config ./profiling/scenarios/manual.lua
```

`profiling/lib/base.lua` provides `enable_virtual_output()`. Spawn apps with `lum.spawn({ command = "...", args = { ... } })` so they inherit the compositor’s `WAYLAND_DISPLAY`.

Drive input from Lua when useful:

- `lum.click(x, y)` / `lum.pointer_move(x, y)`
- `lum.type("text")` or `lum.key("Return")` (letter-by-letter with `lum.sleep` if the bug is per-keystroke)
- `lum.screenshot(x, y, w, h, "/tmp/out.png")` to inspect UI state after each step

Example pattern: click the control that should keep focus, type one character at a time, screenshot after each key, then quit.

### 2. Capture Wayland protocol traffic from the client

Wrap the client so its stderr is logged with `WAYLAND_DEBUG=1`:

```bash
#!/usr/bin/env bash
export WAYLAND_DEBUG=1
exec brave --ozone-platform=wayland ... > /tmp/client-wayland.log 2>&1
```

**Electron / Chromium:** redirecting stderr alone often yields an empty protocol log — the GPU process swallows libwayland debug on an internal pipe. Run the whole tree under `script` (PTY) and prefer `--no-zygote` so child processes keep `WAYLAND_DEBUG`. See `profiling/scenarios/discord_wayland_debug.sh`.

Spawn that wrapper from Lua. In the log, look for sequences that match the symptom:

| Symptom | Protocol clues |
| --- | --- |
| Focus lost after typing | `wl_keyboard.leave` / `enter` around each key; focus moving onto a short-lived `wl_subsurface` or `xdg_popup` |
| Popup gone next frame | surface `attach(nil)` / destroy shortly after map; compositor `xdg_popup.popup_done` |
| Click hits wrong window | `wl_pointer.enter` / `leave` churn after commits (stacking / hit-test desync) |
| Splash maps, main UI never appears | One `xdg_toplevel` (updater/splash) only — no second `get_toplevel` / `set_title` for the main window. Often **client env**, not map/focus policy (see below). |

Correlate those object ids with compositor policy in `crates/lumalla_display` (seat focus, `focus_newly_mapped_surface`, popup grabs, subsurface handling).

### 3. Rule out client environment before deep compositor dives

Some “Wayland hangs” are still X11/env problems:

- **Electron apps (e.g. Discord)** may need a working `DISPLAY` (via `xwayland-satellite`) even with `--ozone-platform=wayland`. Without it they can sit on a splash forever while Lumalla looks fine. A/B: same repro with `unset DISPLAY` vs `DISPLAY=:N` after spawning satellite on the headless compositor.
- Check compositor logs for linux-dmabuf advertising (`Advertising N … format/modifier pairs`). Only a couple of linear formats usually means Vulkan was not queried yet (headless / before a render device) — GPU clients then look “broken” for unrelated reasons.
- When killing repro clients, match on the real binary (`readlink /proc/PID/exe`), not `pkill -f` patterns that also appear in the shell’s own argv (that kills the repro harness).

### 4. Narrow the compositor side

Once the protocol sequence is clear, search the display crate for the matching path (map → focus, null-buffer unmap → keyboard leave, grab dismiss, pointer refresh after client dispatch). Prefer fixes that match protocol intent: child surfaces (subsurfaces, non-grabbed popups) usually must not steal keyboard focus from the parent, and unmapping a focused child should restore the parent when appropriate.

Existing Lua scenarios under `profiling/scenarios/` are good templates for new repros.

## License

Except where noted, all code in this repository is dual-licensed under either:

* MIT License ([LICENSE-MIT](LICENSE-MIT) or [http://opensource.org/licenses/MIT](http://opensource.org/licenses/MIT))
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or [http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0))

at your option. This means you can select the license you prefer!

### Your contributions

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you,
as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
