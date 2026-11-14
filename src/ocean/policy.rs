// Ocean output policy: the artifact policy version and the pyramid
// parameters ocean emission selects.
//
// This module centralizes the selected policy; it is not the whole
// version-bump boundary. Anything that changes durable ocean tile bytes
// needs a bump, wherever it lives: the shapefile reader's projection and
// ring grouping, band rounding, the low-zoom union, piece and feature
// ordering, the pyramid descent, the overlay engine and the MVT encoder.

use crate::geometry::int_ocean::OCEAN_DP_TOL_PX;
use crate::geometry::polygon::{Area, ExtentUnits, MIN_POLY_AREA_AREA};
use crate::geometry::pyramid::{PyramidCell, PyramidParams, PyramidZooms, Simplifier};

/// Bump whenever a change affects durable ocean tile bytes.
///
/// v2: the ocean path's Visvalingam simplifier. The VW landing changed
/// durable ocean tile bytes without bumping this, so a DP-era artifact
/// kept key-validating and served the pre-VW coastline spikes on every
/// artifact-active run for three days before a human spotted one at
/// z5. The version is the code half of the artifact key: shapefile
/// hashes catch input rotations, this constant is the declared stand-in
/// for "the geometry pipeline changed", and forgetting it converts a
/// loud key mismatch into silently stale world geometry.
///
/// v3: low-zoom source pieces are unioned before descent. The source
/// shapefiles arrive pre-split into grid cells, and per-piece descent
/// simplified each cell's copy of a shared coastline independently and
/// applied the per-zoom min-area drop per cell fragment - at z2 that
/// rendered as seam wedges on every cell edge and half-peninsula spikes
/// where one side of a split landform fell under min-area (the corpus
/// z2-x2-y1 finding, 2026-07-19).
///
/// v4: polygons over the MapLibre 500-ring clamp are partitioned by rect
/// bisection; previously their smallest islands were invisible to
/// MapLibre-semantics consumers.
///
/// v5: the polygon engine picks up upstream i_overlay's 8.1.1 through 9.0.0
/// fixes. The fragment splitter no longer drops crossings and collinear
/// overlaps that round into a neighbouring column, and splits vertical
/// segments on column borders against the correct group. Collinear output
/// cleanup now runs after hole binding, so a hole can no longer be bound to
/// the wrong shape where two result shapes touch.
///
/// v6: rescaling, DP simplification and shape cleanup discard the whole
/// polygon when its original outer dies, rather than promoting a surviving
/// hole. Ocean uses this shared engine, including cleanup after overlay;
/// pre-split source cells and low-zoom union do not prove these paths
/// unreachable. Existing v5 artifacts must be rebuilt before use, followed
/// by rotation of the artifact-active corpus contract.
///
/// v7: rect-clip crossings round displacements from the endpoint with the
/// smaller clipped-axis coordinate. Opposite traversals of a shared edge
/// now cut at the same integer point instead of leaving a one-unit fill gap.
/// Existing artifacts must be rebuilt, then the artifact-active corpus
/// contract must be rotated with the newly adjudicated output.
///
/// v8: full-tile fills use a reserved canonical ID, distinct from source piece
/// IDs. Canonical zero IDs could alias piece 0 fragments when slightly
/// overlapping source cells contribute to the same tile. Existing artifacts
/// must be rebuilt before use.
pub const OCEAN_POLICY_VERSION: u32 = 8;

// Piece IDs are zero-based Vec indices, so they cannot reach u64::MAX.
// Keeping full fills identical across pieces preserves payload deduplication
// and long PMTiles runs in the world artifact.
pub(super) const FULL_FILL_FEATURE_ID: u64 = u64::MAX;

/// Everything this block selects - simplifier, tolerance, minimum area, the
/// zoom span - determines the durable ocean tile bytes `ocean-build` bakes
/// into the world artifact. Changing any of it, or the engines it dispatches
/// into (the pyramid descent, the int_ocean simplifiers, the overlay boolean
/// engine, the MVT encoder), requires bumping OCEAN_POLICY_VERSION: without
/// the bump every existing artifact keeps key-validating and silently serves
/// the pre-change geometry. The VW simplifier switch shipped exactly that way
/// and served DP-era coastline spikes world-wide for three days.
pub(super) fn ocean_params<'a>(
    zooms: PyramidZooms,
    tile_filter: Option<&'a (dyn Fn(PyramidCell) -> bool + Sync)>,
) -> PyramidParams<'a> {
    PyramidParams {
        zooms,
        dp_tol: &ocean_dp_tol,
        min_area: &ocean_min_area,
        pins: None,
        tile_filter,
        simplifier: Simplifier::Visvalingam,
    }
}

fn ocean_dp_tol(_z: u8) -> ExtentUnits {
    ExtentUnits::new(OCEAN_DP_TOL_PX)
}

fn ocean_min_area(_z: u8) -> Area {
    MIN_POLY_AREA_AREA
}
