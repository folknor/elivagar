//! Data extent of the input: every node coordinate, plus inline way
//! coordinates in locations mode, folded into one bbox that bounds the ocean
//! band and is recorded in the checkpoint.

use crate::geometry::{self, projection::MercBbox};

const LON_E7_FULL_CIRCLE: i64 = 3_600_000_000;

/// Normalized-Mercator buffer added around the data extent for ocean overlap.
const DATA_BOUNDS_BUFFER: f64 = 0.01;

#[inline]
pub(in crate::pipeline) fn lon_e7_shifted_360(lon_e7: i32) -> i64 {
    let lon = i64::from(lon_e7);
    if lon < 0 {
        lon + LON_E7_FULL_CIRCLE
    } else {
        lon
    }
}

#[inline]
pub(in crate::pipeline) fn crosses_antimeridian(
    min_lon_e7: i32,
    max_lon_e7: i32,
    min_lon_shifted_e7: i64,
    max_lon_shifted_e7: i64,
) -> bool {
    let raw_span_e7 = i64::from(max_lon_e7) - i64::from(min_lon_e7);
    let shifted_span_e7 = max_lon_shifted_e7 - min_lon_shifted_e7;
    shifted_span_e7 < raw_span_e7
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::pipeline) struct CoordinateExtent {
    pub(in crate::pipeline) min_lat_e7: i32,
    pub(in crate::pipeline) max_lat_e7: i32,
    pub(in crate::pipeline) min_lon_e7: i32,
    pub(in crate::pipeline) max_lon_e7: i32,
    pub(in crate::pipeline) min_lon_shifted_e7: i64,
    pub(in crate::pipeline) max_lon_shifted_e7: i64,
}

impl Default for CoordinateExtent {
    fn default() -> Self {
        Self {
            min_lat_e7: i32::MAX,
            max_lat_e7: i32::MIN,
            min_lon_e7: i32::MAX,
            max_lon_e7: i32::MIN,
            min_lon_shifted_e7: i64::MAX,
            max_lon_shifted_e7: i64::MIN,
        }
    }
}

impl CoordinateExtent {
    pub(super) fn include(&mut self, lat_e7: i32, lon_e7: i32) {
        self.min_lat_e7 = self.min_lat_e7.min(lat_e7);
        self.max_lat_e7 = self.max_lat_e7.max(lat_e7);
        self.min_lon_e7 = self.min_lon_e7.min(lon_e7);
        self.max_lon_e7 = self.max_lon_e7.max(lon_e7);
        let shifted_lon_e7 = lon_e7_shifted_360(lon_e7);
        self.min_lon_shifted_e7 = self.min_lon_shifted_e7.min(shifted_lon_e7);
        self.max_lon_shifted_e7 = self.max_lon_shifted_e7.max(shifted_lon_e7);
    }

    pub(super) fn merge(&mut self, other: Self) {
        self.min_lat_e7 = self.min_lat_e7.min(other.min_lat_e7);
        self.max_lat_e7 = self.max_lat_e7.max(other.max_lat_e7);
        self.min_lon_e7 = self.min_lon_e7.min(other.min_lon_e7);
        self.max_lon_e7 = self.max_lon_e7.max(other.max_lon_e7);
        self.min_lon_shifted_e7 = self.min_lon_shifted_e7.min(other.min_lon_shifted_e7);
        self.max_lon_shifted_e7 = self.max_lon_shifted_e7.max(other.max_lon_shifted_e7);
    }

    fn crosses_antimeridian(&self) -> bool {
        crosses_antimeridian(
            self.min_lon_e7,
            self.max_lon_e7,
            self.min_lon_shifted_e7,
            self.max_lon_shifted_e7,
        )
    }

    /// The data extent in normalized Mercator with a buffer for ocean
    /// overlap. A dateline-crossing extent spans the full x range; an empty
    /// one (no coordinates at all) is the whole world.
    pub(super) fn data_bounds(&self) -> MercBbox {
        if self.min_lat_e7 > self.max_lat_e7 {
            return MercBbox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 1.0,
                max_y: 1.0,
            };
        }
        let crosses_dateline = self.crosses_antimeridian();
        let (west_lon_e7, east_lon_e7) = if crosses_dateline {
            (-1_800_000_000, 1_800_000_000)
        } else {
            (self.min_lon_e7, self.max_lon_e7)
        };
        let sw = geometry::projection::project_e7(self.min_lat_e7, west_lon_e7);
        let ne = geometry::projection::project_e7(self.max_lat_e7, east_lon_e7);
        let buf = DATA_BOUNDS_BUFFER;
        MercBbox {
            min_x: if crosses_dateline {
                0.0
            } else {
                (sw.x - buf).max(0.0)
            },
            min_y: (ne.y - buf).max(0.0), // ne.y < sw.y in Mercator [0,1]
            max_x: if crosses_dateline {
                1.0
            } else {
                (ne.x + buf).min(1.0)
            },
            max_y: (sw.y + buf).min(1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CoordinateExtent, crosses_antimeridian};

    #[test]
    fn coordinate_extent_merges_inline_way_coordinates() {
        let mut extent = CoordinateExtent::default();
        extent.include(100, 1_790_000_000);
        extent.include(-100, -1_790_000_000);

        assert_eq!(extent.min_lat_e7, -100);
        assert_eq!(extent.max_lat_e7, 100);
        assert_eq!(extent.min_lon_e7, -1_790_000_000);
        assert_eq!(extent.max_lon_e7, 1_790_000_000);
        assert!(crosses_antimeridian(
            extent.min_lon_e7,
            extent.max_lon_e7,
            extent.min_lon_shifted_e7,
            extent.max_lon_shifted_e7,
        ));

        let mut other = CoordinateExtent::default();
        other.include(50, -100_000_000);
        extent.merge(other);
        assert_eq!(extent.max_lat_e7, 100);
        assert_eq!(extent.max_lon_shifted_e7, 3_500_000_000);
    }

    #[test]
    fn empty_extent_bounds_the_whole_world() {
        let bounds = CoordinateExtent::default().data_bounds();
        assert_eq!(
            (bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y),
            (0.0, 0.0, 1.0, 1.0)
        );
    }

    #[test]
    fn dateline_extent_spans_full_x() {
        let mut extent = CoordinateExtent::default();
        extent.include(0, 1_790_000_000);
        extent.include(0, -1_790_000_000);
        let bounds = extent.data_bounds();
        assert_eq!((bounds.min_x, bounds.max_x), (0.0, 1.0));
    }
}
