#!/usr/bin/env sh
set -eu

# Profile Lumalla memory with heaptrack, then open the recording in heaptrack_gui.
# Must be invoked with the repository root as the working directory.
#
# Usage:
#   ./mem-profiling.sh [scenario]
#
# Scenarios are Lua configs under profiling/scenarios/. The default is "manual".
# Set LUMALLA_PROFILE_SCENARIO to override without a positional argument.

if [ ! -f Cargo.toml ] || [ ! -f init.lua ]; then
  echo "error: run from the lumalla repository root (Cargo.toml and init.lua required)" >&2
  exit 1
fi

scenario="${LUMALLA_PROFILE_SCENARIO:-manual}"
if [ "$#" -gt 0 ]; then
  scenario="$1"
  shift
fi

config="./profiling/scenarios/${scenario}.lua"
if [ ! -f "$config" ]; then
  echo "error: unknown profiling scenario '${scenario}' (expected ${config})" >&2
  exit 1
fi

export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-C force-frame-pointers=yes"

cargo build -p lumalla_config --profile profiling
cargo build -p lumalla --profile profiling

outdir="${LUMALLA_PROFILE_DIR:-./profiling-out}"
mkdir -p "$outdir"
timestamp="$(date +%Y%m%d-%H%M%S)"
heap_output="${outdir}/mem-${scenario}-${timestamp}"

echo "Recording memory profile for scenario '${scenario}' to ${heap_output}"

status=0
heaptrack -o "$heap_output" \
  ./target/profiling/lumalla \
  --config "$config" \
  --config-command ./target/profiling/lumalla-config \
  "$@" || status=$?

# heaptrack may write the exact -o path or add a compression suffix.
heap_file=""
for candidate in "$heap_output" "$heap_output".gz "$heap_output".zst; do
  if [ -f "$candidate" ]; then
    heap_file="$candidate"
    break
  fi
done
if [ -z "$heap_file" ]; then
  for candidate in "$heap_output"*; do
    if [ -f "$candidate" ]; then
      heap_file="$candidate"
      break
    fi
  done
fi
if [ -z "$heap_file" ] || [ ! -f "$heap_file" ]; then
  echo "error: heaptrack did not produce a profile at ${heap_output}" >&2
  exit 1
fi

echo "Opening heaptrack_gui for ${heap_file}"
heaptrack_gui "$heap_file"
exit "$status"
