#!/usr/bin/env bash
#
# Ocean-coverage gate helper.
#
# brokkr has no `ocean-coverage` wrapper and no `--no-ocean-simplify` passthrough
# on `tilegen`, so the ocean coverage gate cannot be driven through brokkr. This
# script drives the `elivagar` binary directly. It:
#   1. builds a fresh elivagar release (so the binary is always up to date),
#   2. builds/caches a `--no-ocean-simplify` verbatim baseline for the dataset
#      (rebuilt when the binary or the PBF is newer than the cached baseline),
#   3. runs `elivagar ocean-coverage <archive> --baseline <ref>` for EACH archive
#      passed - so one invocation runs the whole comparison (e.g. the DP build,
#      which must FIRE, and the VW build, which must CLEAR).
#
# Usage:
#   scripts/ocean-coverage.sh <archive.pmtiles> [<archive.pmtiles> ...] [-- <ocean-coverage flags>]
#
# The full z1-6 gate (DP must fire large losses, VW must come back clean):
#   scripts/ocean-coverage.sh \
#     data/tilegen/norway-3f4ca38.pmtiles \
#     data/tilegen/norway-41d0227-vw.pmtiles
#
# The full-resolution false-positive floor (both should be near-zero):
#   scripts/ocean-coverage.sh \
#     data/tilegen/norway-3f4ca38.pmtiles \
#     data/tilegen/norway-41d0227-vw.pmtiles \
#     -- --zmin 7 --zmax 14
#
# With no `-- flags`, ocean-coverage's own defaults apply (zmin 1, zmax 6,
# threshold-2x 512). Dataset defaults to the norway locations extract; override
# any path via env: PBF=... OCEAN=... OCEAN_SIMPLIFIED=... REF=...
# (change REF when you change datasets - the baseline is dataset-specific).

set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${PBF:-data/norway-20260225-seq4709-locations-prepass.osm.pbf}"
OCEAN="${OCEAN:-data/water-polygons-split-3857/water_polygons.shp}"
OCEAN_SIMPLIFIED="${OCEAN_SIMPLIFIED:-data/simplified-water-polygons-split-3857/simplified_water_polygons.shp}"
REF="${REF:-data/tilegen/ocean-coverage-ref.pmtiles}"
BUILD_TMP="${BUILD_TMP:-data/tilegen_tmp}"

# Split args into archives (before `--`) and ocean-coverage flags (after `--`).
archives=()
while [ $# -gt 0 ] && [ "$1" != "--" ]; do
  archives+=("$1")
  shift
done
[ "${1:-}" = "--" ] && shift
cov_flags=("$@")

if [ ${#archives[@]} -eq 0 ]; then
  echo "usage: scripts/ocean-coverage.sh <archive.pmtiles> [<archive.pmtiles> ...] [-- <flags>]" >&2
  exit 2
fi

# 1. Fresh elivagar release build. Locate the binary via cargo metadata because
#    the target directory is not necessarily ./target on this machine.
echo "==> cargo build --release"
cargo build --release
target_dir="$(cargo metadata --format-version 1 --no-deps \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
elivagar="$target_dir/release/elivagar"
if [ ! -x "$elivagar" ]; then
  echo "error: elivagar binary not found at $elivagar" >&2
  exit 1
fi

# 2. Build the verbatim --no-ocean-simplify baseline if missing or stale.
if [ ! -f "$REF" ] || [ "$elivagar" -nt "$REF" ] || [ "$PBF" -nt "$REF" ]; then
  echo "==> building --no-ocean-simplify verbatim baseline -> $REF"
  "$elivagar" run "$PBF" -o "$REF" --tmp-dir "$BUILD_TMP" \
    --ocean "$OCEAN" --ocean-simplified "$OCEAN_SIMPLIFIED" --no-ocean-simplify
else
  echo "==> reusing cached baseline $REF"
fi

# 3. Coverage comparison against the baseline, once per archive. ocean-coverage
#    exits nonzero when a tile exceeds the threshold; DP is EXPECTED to fire and
#    VW is expected to clear, so we do not let a nonzero exit abort the loop -
#    each result is labelled and any failure is remembered for the exit code.
any_offenders=0
for archive in "${archives[@]}"; do
  echo
  echo "==================================================================="
  echo "==> ocean-coverage $archive --baseline $REF ${cov_flags[*]}"
  echo "==================================================================="
  if "$elivagar" ocean-coverage "$archive" --baseline "$REF" "${cov_flags[@]}"; then
    echo "--> CLEAN: no tile over threshold (exit 0)"
  else
    echo "--> OFFENDERS: tiles over threshold (nonzero exit) - see the 'lost' lines above"
    any_offenders=1
  fi
done

exit "$any_offenders"
