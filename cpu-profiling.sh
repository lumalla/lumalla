#!/usr/bin/env sh
set -eu

# Profile Lumalla CPU with perf, then open the recording in Hotspot.
# Must be invoked with the repository root as the working directory.

if [ ! -f Cargo.toml ] || [ ! -f init.lua ]; then
  echo "error: run from the lumalla repository root (Cargo.toml and init.lua required)" >&2
  exit 1
fi

export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-C force-frame-pointers=yes"

cargo build -p lumalla_config --profile profiling
cargo build -p lumalla --profile profiling

outdir="${LUMALLA_PROFILE_DIR:-./profiling-out}"
mkdir -p "$outdir"
timestamp="$(date +%Y%m%d-%H%M%S)"
perf_data="${outdir}/cpu-${timestamp}.perf.data"

echo "Recording CPU profile to ${perf_data}"
echo "Stop Lumalla (Ctrl+C) when you are done exercising the workload."

status=0
perf record -F 99 -g --call-graph dwarf -o "$perf_data" -- \
  ./target/profiling/lumalla \
  --config ./init.lua \
  --config-command ./target/profiling/lumalla-config \
  "$@" || status=$?

echo "Opening Hotspot for ${perf_data}"
hotspot "$perf_data"
exit "$status"
