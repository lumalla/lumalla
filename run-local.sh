#!/usr/bin/env sh
set -eu

# Run Lumalla from a source checkout with the local init.lua config.
# Must be invoked with the repository root as the working directory.

if [ ! -f Cargo.toml ] || [ ! -f init.lua ]; then
  echo "error: run from the lumalla repository root (Cargo.toml and init.lua required)" >&2
  exit 1
fi

cargo build -p lumalla_config
exec cargo run -- --config ./init.lua --config-command ./target/debug/lumalla-config "$@"
