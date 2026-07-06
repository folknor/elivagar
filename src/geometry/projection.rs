use std::f64::consts::PI;
use std::sync::OnceLock;

use super::EXTENT;

/// Earth's equatorial circumference in meters.
pub(super) const EARTH_CIRCUMFERENCE: f64 = 40_075_016.686;

/// Maximum latitude for Web Mercator (beyond this, projection diverges).
#[cfg(test)]
pub(super) const MAX_LATITUDE: f64 = 85.051_129;

/// A 2D point in Mercator [0,1] space.
#[derive(Clone, Copy, Debug)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}
const _: () = assert!(std::mem::size_of::<Point>() == 16);

impl Point {
    #[inline]
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Axis-aligned bounding rectangle in Mercator [0,1] space.
#[derive(Clone, Copy, Debug)]
pub struct MercBbox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

// Latitude LUT for project_e7: 2^18 + 1 entries, ~2 MB. Linear interpolation
// gives ~0.00032° error = 0.03 pixels at z14 (imperceptible). Eliminates
// tan/cos/ln transcendentals (~250-400 cycles) from the hot path.
const LUT_BITS: u32 = 18;
const LUT_SIZE: usize = (1 << LUT_BITS) + 1; // 262_145
const LAT_E7_MIN: i64 = -850_511_290; // -MAX_LATITUDE in e7
const LAT_E7_MAX: i64 = 850_511_290; //  MAX_LATITUDE in e7
const LAT_E7_RANGE: f64 = (LAT_E7_MAX - LAT_E7_MIN) as f64;

static LAT_LUT: OnceLock<Box<[f64]>> = OnceLock::new();

fn init_lat_lut() -> Box<[f64]> {
    let mut table = vec![0.0f64; LUT_SIZE];
    let scale = 1.0 / (LUT_SIZE - 1) as f64;
    for (i, entry) in table.iter_mut().enumerate() {
        let lat_e7 = LAT_E7_MIN as f64 + (i as f64 * scale) * LAT_E7_RANGE;
        let lat_rad = lat_e7 * 1e-7 * PI / 180.0;
        *entry = 0.5 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / (2.0 * PI);
    }
    table.into_boxed_slice()
}

/// Project a single WGS84 coordinate (lat_deg, lon_deg) to Mercator [0,1].
/// Uses exact transcendentals - for tests and one-off calls. Hot path uses
/// `project_e7` which goes through the LUT.
#[cfg(test)]
#[inline]
pub fn project(lat_deg: f64, lon_deg: f64) -> Point {
    let lat_clamped = lat_deg.clamp(-MAX_LATITUDE, MAX_LATITUDE);
    let x = (lon_deg + 180.0) / 360.0;
    let lat_rad = lat_clamped * PI / 180.0;
    let y = 0.5 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / (2.0 * PI);
    Point::new(x, y)
}

/// Project from fixed-point e7 integers to Mercator [0,1] via LUT.
/// ~3-4 cycles (table lookup + lerp) vs ~250-400 cycles (transcendentals).
#[inline]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
pub fn project_e7(lat_e7: i32, lon_e7: i32) -> Point {
    let lut = LAT_LUT.get_or_init(init_lat_lut);
    let x = (f64::from(lon_e7) * 1e-7 + 180.0) / 360.0;

    let lat = i64::from(lat_e7).clamp(LAT_E7_MIN, LAT_E7_MAX);
    let frac = (lat - LAT_E7_MIN) as f64 / LAT_E7_RANGE;
    let idx_f = frac * (LUT_SIZE - 1) as f64;
    let idx = idx_f as usize;
    let t = idx_f - idx as f64;

    let y = if idx + 1 < LUT_SIZE {
        lut[idx] + t * (lut[idx + 1] - lut[idx])
    } else {
        lut[idx]
    };
    Point::new(x, y)
}

/// Convert EPSG:3857 (Web Mercator meters) to Mercator [0,1] coordinates.
#[inline]
pub fn from_epsg3857(x: f64, y: f64) -> Point {
    let half_c = EARTH_CIRCUMFERENCE / 2.0;
    Point::new(
        (x + half_c) / EARTH_CIRCUMFERENCE,
        (half_c - y) / EARTH_CIRCUMFERENCE,
    )
}

/// Inverse projection: Mercator y → latitude in degrees.
#[cfg(test)]
#[inline]
pub fn merc_y_to_lat(y: f64) -> f64 {
    let lat_rad = (PI * (1.0 - 2.0 * y)).sinh().atan();
    lat_rad * 180.0 / PI
}

/// Convert a Mercator [0,1] point to tile-local pixel coordinates.
///
/// Returns `(px_x, px_y)` as i32 suitable for MVT command encoding.
#[allow(clippy::cast_possible_truncation)]
pub fn merc_to_tile_px(p: &Point, tile_x: u32, tile_y: u32, zoom: u8) -> (i32, i32) {
    let z_scale = f64::from(1u32 << zoom);
    let px_x = (p.x * z_scale - f64::from(tile_x)) * EXTENT;
    let px_y = (p.y * z_scale - f64::from(tile_y)) * EXTENT;
    (px_x.round() as i32, px_y.round() as i32)
}

/// Project a WGS84 bbox to Mercator and return the `MercBbox`.
#[cfg(test)]
pub fn project_bbox(south: f64, west: f64, north: f64, east: f64) -> MercBbox {
    let sw = project(south, west);
    let ne = project(north, east);
    MercBbox {
        min_x: sw.x,
        min_y: ne.y, // In Mercator, north has smaller y
        max_x: ne.x,
        max_y: sw.y,
    }
}
