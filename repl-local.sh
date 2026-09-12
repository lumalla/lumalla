#!/usr/bin/env sh
set -eu

# Connect to a debug-build lumalla-config Lua REPL.

if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
  echo "error: XDG_RUNTIME_DIR is not set" >&2
  exit 1
fi

socket="${XDG_RUNTIME_DIR}/lumalla-config.debug.sock"
if [ ! -S "$socket" ]; then
  echo "error: REPL socket not found at $socket" >&2
  echo "hint: start a debug lumalla-config with --repl (e.g. run-local.sh)" >&2
  exit 1
fi

exec socat - "UNIX-CONNECT:${socket}"
