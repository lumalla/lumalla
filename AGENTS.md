# Agents

## Cursor Cloud specific instructions

Lumalla is a Wayland window manager written in Rust. It is a single binary that requires Linux display/seat hardware at runtime (DRM, libseat, Vulkan). In cloud/headless VMs, compilation and tests work fully, but `cargo run` will exit immediately because no seat or `/dev/dri` device is available.

### Key commands

| Task | Command |
|------|---------|
| Build | `cargo build --workspace` |
| Test | `cargo test --workspace` |
| Lint | `cargo clippy --workspace` |
| Format check | `cargo fmt --check` |
| Run (with log) | `cargo run -- -l /tmp/lumalla.log` |

### System dependencies

The build requires `libseat-dev`, `libdrm-dev`, and `libgbm-dev` (Ubuntu packages). These are installed by the update script. `LIBCLANG_PATH` must point to the libclang `.so` directory for `bindgen` to work; it is set in `~/.bashrc` to `/usr/lib/llvm-18/lib`.

### Crate workspace

8 internal crates under `crates/` plus the root binary. See `Cargo.toml` workspace members for the full list. The `flake.nix` defines the canonical dev environment (Nix is not used in cloud VMs; we install apt packages instead).

### Runtime limitations in cloud VMs

- The binary requires DRM/KMS (`/dev/dri`) and a seat daemon (`seatd`) at runtime, so `cargo run` will fail with display/renderer errors. This is expected.
- All unit tests, doc-tests, and integration tests pass without display hardware.
- The `use-libseat-crate` cargo feature switches from custom FFI bindings to the `libseat` crate; default build uses custom bindings via `bindgen`.
