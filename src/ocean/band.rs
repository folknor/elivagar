//! Which side serves a tile's ocean when an extract runs against the world
//! artifact: the extract's own computed pass (the boundary band near its data
//! bounds) or the artifact (the interior).
//!
//! Every reader of that answer goes through [`OceanBand`]: the producer's
//! pyramid traversal filter and record prefilter, assembly's splice decision,
//! and assembly's run-copy ranges, which are [`OceanBand::artifact_tile_rect`]
//! - the exact complement of [`OceanBand::computed_owns`] at one zoom.

use super::plan::OceanPass;
use super::{merc_ceil, merc_floor, rect_contains};
use crate::geometry::int_ocean::{IntRect, TILE_BUFFER_I32, TILE_EXTENT_I32};
use crate::geometry::projection::MercBbox;
use crate::geometry::pyramid::{PyramidCell, buffered_cell_rect_base, tile_cover};
use crate::geometry::tiles::grid::EXTENT_LOG2;

/// One pass's integer grid for ownership: the data bounds snapped inward onto
/// the pass's base pixel grid, and the world on that grid.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OceanPassGrid {
    pub(crate) max_zoom: u8,
    pub(crate) inner_rect: IntRect,
    pub(crate) world_rect: IntRect,
}

impl OceanPassGrid {
    pub(crate) fn for_bounds(data_bounds: &MercBbox, max_zoom: u8) -> Self {
        let scale = Self::scale_for(max_zoom);
        let world = i32::try_from(scale).expect("world grid fits i32");
        Self {
            max_zoom,
            inner_rect: IntRect {
                min_x: merc_ceil(data_bounds.min_x, scale),
                min_y: merc_ceil(data_bounds.min_y, scale),
                max_x: merc_floor(data_bounds.max_x, scale),
                max_y: merc_floor(data_bounds.max_y, scale),
            },
            world_rect: IntRect {
                min_x: 0,
                min_y: 0,
                max_x: world,
                max_y: world,
            },
        }
    }

    /// Base units across the world on a `max_zoom` grid.
    pub(crate) fn scale_for(max_zoom: u8) -> i64 {
        1_i64 << (u32::from(max_zoom) + EXTENT_LOG2)
    }

    fn covers_world(&self) -> bool {
        rect_contains(self.inner_rect, self.world_rect)
    }
}

/// Whether the computed pass on `pass` owns this tile rather than the
/// artifact.
///
/// Non-ownership is downward closed within one grid: a child's buffered cell
/// lies inside its parent's, so a tile the artifact owns has only artifact-owned
/// descendants. Pyramid traversal prunes on that.
pub(crate) fn ocean_band_tile(z: u8, tx: u32, ty: u32, pass: &OceanPassGrid) -> bool {
    if z > pass.max_zoom {
        return true;
    }
    // A regional extract touching the antimeridian has no representable
    // neighbour beyond x=0/x=max. Keep that column computed unless this pass
    // is the complete world, so a clipped world-edge fragment never leaks in
    // from the durable artifact.
    let max_tile = (1_u32 << z) - 1;
    if !pass.covers_world()
        && ((tx == 0 && pass.inner_rect.min_x == pass.world_rect.min_x)
            || (tx == max_tile && pass.inner_rect.max_x == pass.world_rect.max_x))
    {
        return true;
    }
    let buffered = buffered_cell_rect_base(pass.max_zoom, PyramidCell { z, tx, ty });
    // Keep the former world clamp and pass-world clipping around the shared
    // unbounded buffer so antimeridian and edge ownership stay unchanged.
    let world_limit = i32::try_from(OceanPassGrid::scale_for(pass.max_zoom))
        .expect("ocean world coordinate fits i32");
    let clamp_to_world = |value: i32| value.clamp(0, world_limit);
    let buffered = IntRect {
        min_x: clamp_to_world(buffered.min_x),
        min_y: clamp_to_world(buffered.min_y),
        max_x: clamp_to_world(buffered.max_x),
        max_y: clamp_to_world(buffered.max_y),
    };
    let clipped = IntRect {
        min_x: buffered.min_x.max(pass.world_rect.min_x),
        min_y: buffered.min_y.max(pass.world_rect.min_y),
        max_x: buffered.max_x.min(pass.world_rect.max_x),
        max_y: buffered.max_y.min(pass.world_rect.max_y),
    };
    !rect_contains(pass.inner_rect, clipped)
}

/// A half-open rectangle of tile coordinates at one zoom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TileRect {
    pub(crate) min_x: u32,
    pub(crate) min_y: u32,
    pub(crate) max_x: u32,
    pub(crate) max_y: u32,
}

/// Ownership of every tile between an extract's computed passes and the world
/// artifact, built once from the data bounds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OceanBand {
    low: OceanPassGrid,
    high: OceanPassGrid,
}

impl OceanBand {
    pub(crate) fn for_bounds(data_bounds: &MercBbox) -> Self {
        Self {
            low: OceanPassGrid::for_bounds(data_bounds, OceanPass::Low.grid_zoom()),
            high: OceanPassGrid::for_bounds(data_bounds, OceanPass::High.grid_zoom()),
        }
    }

    pub(crate) fn grid(&self, pass: OceanPass) -> &OceanPassGrid {
        match pass {
            OceanPass::Low => &self.low,
            OceanPass::High => &self.high,
        }
    }

    /// True when the data bounds cover the world, so the artifact serves
    /// every tile it holds and the computed passes have nothing to do.
    /// Inward snapping cannot make a smaller rectangle contain the world on
    /// any grid, so the two grids always agree here.
    pub(crate) fn is_empty(&self) -> bool {
        self.low.covers_world() && self.high.covers_world()
    }

    /// Whether the computed pass owns this tile's ocean.
    pub(crate) fn computed_owns(&self, z: u8, tx: u32, ty: u32) -> bool {
        !self.is_empty() && ocean_band_tile(z, tx, ty, self.grid(OceanPass::for_zoom(z)))
    }

    /// The tiles at `z` the artifact owns, as the exact rectangle
    /// `computed_owns` leaves (world-edge columns removed), or `None` when it
    /// owns none.
    pub(crate) fn artifact_tile_rect(&self, z: u8) -> Option<TileRect> {
        let side = 1_u32 << z;
        if self.is_empty() {
            return Some(TileRect {
                min_x: 0,
                min_y: 0,
                max_x: side,
                max_y: side,
            });
        }
        let grid = self.grid(OceanPass::for_zoom(z));
        let shift = u32::from(grid.max_zoom - z);
        let cell = i64::from(TILE_EXTENT_I32) << shift;
        let buffer = i64::from(TILE_BUFFER_I32) << shift;
        let limit = OceanPassGrid::scale_for(grid.max_zoom);
        let mut min_x = contained_axis_start(grid.inner_rect.min_x, cell, buffer, side);
        let min_y = contained_axis_start(grid.inner_rect.min_y, cell, buffer, side);
        let mut max_x = contained_axis_end(grid.inner_rect.max_x, cell, buffer, limit, side);
        let max_y = contained_axis_end(grid.inner_rect.max_y, cell, buffer, limit, side);
        if grid.inner_rect.min_x == grid.world_rect.min_x {
            min_x = min_x.max(1);
        }
        if grid.inner_rect.max_x == grid.world_rect.max_x {
            max_x = max_x.min(side - 1);
        }
        if min_x >= max_x || min_y >= max_y {
            return None;
        }
        Some(TileRect {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }
}

fn contained_axis_start(inner_min: i32, cell: i64, buffer: i64, side: u32) -> u32 {
    let inner_min = i64::from(inner_min);
    if inner_min <= 0 {
        return 0;
    }
    let numerator = inner_min + buffer;
    let quotient = numerator.div_euclid(cell);
    let rounded = quotient
        + if numerator.rem_euclid(cell) == 0 {
            0
        } else {
            1
        };
    u32::try_from(rounded.clamp(0, i64::from(side))).expect("tile coordinate fits u32")
}

fn contained_axis_end(inner_max: i32, cell: i64, buffer: i64, limit: i64, side: u32) -> u32 {
    let inner_max = i64::from(inner_max);
    if inner_max >= limit {
        return side;
    }
    let end = (inner_max - buffer).div_euclid(cell);
    u32::try_from(end.clamp(0, i64::from(side))).expect("tile coordinate fits u32")
}

/// Whether any root cell a shapefile record could select on `pass` is
/// computed-owned, so the record must be parsed.
///
/// This record-level reject runs before the polygon is decoded, so it uses
/// the shapefile's raw bbox, snapped and clamped. Pyramid root selection
/// later uses the bounds of the normalized quantized geometry; the header bbox
/// encloses those vertices and rounding is monotone, so this is conservative,
/// never stricter than the pyramid. Root selection itself is shared through
/// `tile_cover`, at the pass's canonical first zoom.
pub(crate) fn pass_owns_root_range(
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    band: &OceanBand,
    pass: OceanPass,
) -> bool {
    let grid = band.grid(pass);
    let scale = OceanPassGrid::scale_for(grid.max_zoom);
    let snap = |value: f64| -> i32 {
        #[allow(clippy::cast_possible_truncation)]
        let value = (value * scale as f64).round() as i64;
        i32::try_from(value.clamp(0, scale)).expect("ocean base coordinate fits i32")
    };
    let bbox = IntRect {
        min_x: snap(min_x),
        min_y: snap(min_y),
        max_x: snap(max_x),
        max_y: snap(max_y),
    };
    let z_top = pass.first_zoom();
    let (tx0, tx1, ty0, ty1) = tile_cover(bbox, grid.max_zoom, z_top, 0);
    for ty in ty0..=ty1 {
        for tx in tx0..=tx1 {
            if band.computed_owns(z_top, tx, ty) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> MercBbox {
        MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        }
    }

    #[test]
    fn ocean_band_tile_world_has_no_band_at_edges_or_corners() {
        let band = OceanBand::for_bounds(&world());
        assert!(band.is_empty());
        for z in [0, 7, 8, 14] {
            let edge = (1_u32 << z) - 1;
            for (x, y) in [(0, 0), (edge, 0), (0, edge), (edge, edge)] {
                assert!(!ocean_band_tile(z, x, y, band.grid(OceanPass::for_zoom(z))));
                assert!(!band.computed_owns(z, x, y));
            }
        }
    }

    #[test]
    fn ocean_band_tile_inward_sliver_and_world_edge_are_safe_band() {
        let bounds = MercBbox {
            min_x: 0.000_000_01,
            min_y: 0.2,
            max_x: 0.8,
            max_y: 0.8,
        };
        let grid = OceanPassGrid::for_bounds(&bounds, 14);
        assert!(ocean_band_tile(14, 0, 4_000, &grid));
        let edge_bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.2,
            max_x: 0.8,
            max_y: 0.8,
        };
        let edge = OceanPassGrid::for_bounds(&edge_bounds, 14);
        assert!(ocean_band_tile(14, 0, 4_000, &edge));
    }

    #[test]
    fn ocean_band_tile_respects_the_128_unit_inner_edge_buffer() {
        let cell = 4096;
        let world = IntRect {
            min_x: 0,
            min_y: 0,
            max_x: 1 << 26,
            max_y: 1 << 26,
        };
        // Tile (14, 100, 200) has a buffered footprint starting at
        // 100*cell - 128. Containment (non-band) requires the inner rect to
        // reach at or below that edge; one unit short of the buffer leaves
        // the tile band.
        let just_inside = OceanPassGrid {
            max_zoom: 14,
            inner_rect: IntRect {
                min_x: 100 * cell - 127,
                min_y: 200 * cell - 128,
                max_x: world.max_x,
                max_y: world.max_y,
            },
            world_rect: world,
        };
        let at_buffer = OceanPassGrid {
            inner_rect: IntRect {
                min_x: 100 * cell - 128,
                ..just_inside.inner_rect
            },
            ..just_inside
        };
        assert!(ocean_band_tile(14, 100, 200, &just_inside));
        assert!(!ocean_band_tile(14, 100, 200, &at_buffer));
    }

    #[test]
    fn ocean_band_tile_keeps_the_right_world_edge_computed() {
        let grid = OceanPassGrid::for_bounds(
            &MercBbox {
                min_x: 0.2,
                min_y: 0.2,
                max_x: 1.0,
                max_y: 0.8,
            },
            14,
        );
        assert!(ocean_band_tile(14, (1 << 14) - 1, 4_000, &grid));
    }

    #[test]
    fn each_zoom_is_owned_on_its_own_pass_grid() {
        // A z7 tile is judged on the z7 grid and a z8 tile on the z14 grid,
        // whatever spelling named the shapefiles.
        let band = OceanBand::for_bounds(&MercBbox {
            min_x: 0.3,
            min_y: 0.3,
            max_x: 0.7,
            max_y: 0.7,
        });
        assert_eq!(band.grid(OceanPass::for_zoom(7)).max_zoom, 7);
        assert_eq!(band.grid(OceanPass::for_zoom(8)).max_zoom, 14);
        assert_eq!(band.grid(OceanPass::for_zoom(14)).max_zoom, 14);
        assert!(!band.computed_owns(14, 8_192, 8_192));
        assert!(band.computed_owns(14, 0, 8_192));
    }

    #[test]
    fn ocean_band_tile_clamps_polar_edges() {
        let grid = OceanPassGrid::for_bounds(
            &MercBbox {
                min_x: 0.2,
                min_y: 0.0,
                max_x: 0.8,
                max_y: 0.7,
            },
            14,
        );
        // Bounds that REACH the world top edge behave like the world there:
        // the buffered footprint is clipped by the world rect before the
        // containment test, and the computed clip at y=0 is identical to the
        // world build's - so the top row is NOT band.
        assert!(!ocean_band_tile(14, 5_000, 0, &grid));
        // The bottom row lies entirely OUTSIDE the data bounds: it must be
        // band (computed owns it and emits nothing), or the artifact would
        // inject ocean beyond the extract's coverage.
        assert!(ocean_band_tile(14, 5_000, (1 << 14) - 1, &grid));
    }

    /// Bounds that exercise the interior, every world edge, the antimeridian
    /// columns and the world itself.
    fn ownership_bounds() -> [MercBbox; 5] {
        [
            MercBbox {
                min_x: 0.31,
                min_y: 0.28,
                max_x: 0.62,
                max_y: 0.71,
            },
            world(),
            MercBbox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 0.62,
                max_y: 0.71,
            },
            MercBbox {
                min_x: 0.31,
                min_y: 0.28,
                max_x: 1.0,
                max_y: 1.0,
            },
            MercBbox {
                min_x: 0.0,
                min_y: 0.4,
                max_x: 1.0,
                max_y: 0.45,
            },
        ]
    }

    #[test]
    fn artifact_rect_is_the_exact_complement_and_ownership_is_downward_closed() {
        for bounds in ownership_bounds() {
            let band = OceanBand::for_bounds(&bounds);
            for z in 0..=9_u8 {
                let rect = band.artifact_tile_rect(z);
                let side = 1_u32 << z;
                for ty in 0..side {
                    for tx in 0..side {
                        let in_rect = rect.is_some_and(|r| {
                            r.min_x <= tx && tx < r.max_x && r.min_y <= ty && ty < r.max_y
                        });
                        let computed = band.computed_owns(z, tx, ty);
                        assert_ne!(
                            computed, in_rect,
                            "tile z{z}/{tx}/{ty} in {bounds:?} must have one ocean owner"
                        );
                        // Traversal prunes an artifact-owned cell's subtree,
                        // so within one pass its children must be
                        // artifact-owned too.
                        if !computed && z < OceanPass::for_zoom(z).last_zoom() {
                            for (cx, cy) in [
                                (2 * tx, 2 * ty),
                                (2 * tx + 1, 2 * ty),
                                (2 * tx, 2 * ty + 1),
                                (2 * tx + 1, 2 * ty + 1),
                            ] {
                                assert!(
                                    !band.computed_owns(z + 1, cx, cy),
                                    "child of artifact-owned z{z}/{tx}/{ty} in {bounds:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
