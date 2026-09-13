# Agents

## Cursor Cloud specific instructions

### Overview

Lumalla is a Wayland window manager (compositor) for Linux, built as a Rust workspace with 8 internal crates plus the root binary. It uses Nix flakes for its development environment and CI.

### Development environment

All build commands **must** be run inside the Nix devShell to get the correct Rust nightly toolchain and native dependencies (libseat, libdrm, libgbm, Vulkan). The update script starts the Nix daemon automatically; all you need is the `nix develop --command ...` prefix.

### Common commands

| Task | Command |
|------|---------|
| Type-check | `nix develop --command cargo check --all-targets` |
| Format check | `nix develop --command cargo fmt --all -- --check` |
| Tests | `nix develop --command cargo test --workspace` |
| Build (dev) | `nix develop --command cargo build` |
| CI-equivalent | `nix flake check --print-build-logs --show-trace` |
| Run binary | `nix develop --command cargo run -- --help` |

### Gotchas

- **Edition 2024**: All crates use `edition = "2024"`, which requires Rust nightly. The system Rust toolchain (stable) will fail. Always use `nix develop --command ...` so the flake-provided nightly Rust is used.
- **Clippy**: The Nix devShell does not include `clippy`. Running `cargo clippy` inside `nix develop` will fall through to the system stable clippy, which cannot compile edition 2024 code. CI does not run clippy.
- **Nix daemon**: The update script starts `nix-daemon` in the background. If `nix develop` fails with a socket error, verify the daemon is running: `pgrep nix-daemon || (sudo nix-daemon &)`.
- **No GUI testing**: Lumalla is a Wayland compositor that requires DRM/KMS hardware access. It cannot be launched inside a Cloud Agent VM. Testing is limited to `cargo check`, `cargo test`, `cargo build`, and `nix flake check`.
