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

Spawn that wrapper from Lua. In the log, look for sequences that match the symptom:

| Symptom | Protocol clues |
| --- | --- |
| Focus lost after typing | `wl_keyboard.leave` / `enter` around each key; focus moving onto a short-lived `wl_subsurface` or `xdg_popup` |
| Popup gone next frame | surface `attach(nil)` / destroy shortly after map; compositor `xdg_popup.popup_done` |
| Click hits wrong window | `wl_pointer.enter` / `leave` churn after commits (stacking / hit-test desync) |

Correlate those object ids with compositor policy in `crates/lumalla_display` (seat focus, `focus_newly_mapped_surface`, popup grabs, subsurface handling).

### 3. Narrow the compositor side

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
