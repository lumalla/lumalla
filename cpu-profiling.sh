#!/usr/bin/env sh
set -eu

# Profile Lumalla CPU with perf, then open the recording in Hotspot.
# Must be invoked with the repository root as the working directory.
#
# Usage:
#   ./cpu-profiling.sh [scenario]
#
# Scenarios are Lua configs under profiling/scenarios/. The default is "manual".
# Set LUMALLA_PROFILE_SCENARIO to override without a positional argument.
#
# Only the lumalla compositor process is sampled (--no-inherit); spawned apps
# such as qalculate-qt are excluded. Frame-pointer call graphs match the
# profiling RUSTFLAGS below and are lighter than DWARF under perf.

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
perf_data="${outdir}/cpu-${scenario}-${timestamp}.perf.data"

echo "Recording CPU profile for scenario '${scenario}' to ${perf_data}"

status=0
perf record --no-inherit --mmap-pages=32 -F 99 -g --call-graph fp -o "$perf_data" -- \
  ./target/profiling/lumalla \
  -- ./target/profiling/lumalla-config \
  --config "$config" \
  "$@" || status=$?

echo "Opening Hotspot for ${perf_data}"
hotspot "$perf_data"
exit "$status"
