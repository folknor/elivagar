//! Zooms, tile addresses and PMTiles Hilbert tile ids.
//!
//! Two zoom domains meet here and must not be confused:
//!
//! - The profile domain, `0..=MAX_ZOOM`. The Shortbread schema tops out at
//!   z14, every geometry and encoding policy that asks "is this the last
//!   zoom" asks it of [`MAX_ZOOM`], and per-zoom pipeline storage is sized by
//!   [`ZOOM_COUNT`]. A run's configured zoom range is a truncation window over
//!   this domain: construction follows each layer's own zoom range and only
//!   emission is restricted, so a narrowed archive holds exactly the tiles a
//!   full build holds at those zooms. Computed ocean constructs on its fixed
//!   z7 and z14 pass grids for the same reason.
//! - The archive domain, `0..=ARCHIVE_MAX_ZOOM`. A PMTiles archive addresses
//!   tiles up to z31, and the tools that read foreign archives (`svg`,
//!   `diag`, `verify`, `inspect`) work in this domain.
//!
//! [`TileId`] is a valid archive address. Exclusive ends of id ranges are
//! not addresses (the end of a z31 range is one past the last z31 id), so
//! they stay raw `u64` boundaries.
//!
//! Zooms stay bare `u8` on purpose. A single `Zoom` type would have to pick
//! one bound, and the code meets at least four: the archive's 31, the
//! profile's 14, the ocean pass grids' 7 and 14, and the sort key's 48-bit id
//! field (z23). The checks live where a zoom enters a domain: `TileId`
//! construction for the archive, `SortKey::new` and `run`'s validation for
//! the profile. Per-zoom storage is likewise a plain `[T; ZOOM_COUNT]`: a
//! wrapper indexed by `u8` would check nothing the array index does not.
//! The same holds for per-layer `[T; Layer::count()]` arrays: every index is
//! a `Layer` cast, always in range, and the one place an unchecked layer
//! byte enters (a sort key read back from a chunk) is refused by
//! `SortKey::layer`, so a `LayerMap<T>` wrapper would only rename the cast.
//!
//! There is no `TileCoord` type either. A coordinate triple lives only
//! between a tile enumeration and `TileId::at`: the pyramid hands its sinks a
//! named `PyramidCell`, and everything downstream of a sink addresses tiles
//! by `TileId`. [`TileId::coord`] returns a `(z, x, y)` tuple for display and
//! for writers whose own API takes the three numbers.

use std::fmt;
use std::io;

/// The profile's top zoom: the last zoom of every geometry policy.
pub const MAX_ZOOM: u8 = 14;

/// Number of zooms in the profile domain, `0..=MAX_ZOOM`.
pub const ZOOM_COUNT: usize = MAX_ZOOM as usize + 1;

/// The highest zoom a PMTiles tile id can address in `u64`.
pub const ARCHIVE_MAX_ZOOM: u8 = 31;

/// `BASES[z]` is the first tile id of zoom `z`, `(4^z - 1) / 3`, for
/// `z <= 32`. Built as a running sum of `4^i` so no intermediate overflows:
/// `4^32` does not fit in `u64`, but `BASES[32]` does.
const BASES: [u64; ARCHIVE_MAX_ZOOM as usize + 2] = {
    let mut bases = [0u64; ARCHIVE_MAX_ZOOM as usize + 2];
    let mut z = 1;
    while z < bases.len() {
        bases[z] = bases[z - 1] + (1u64 << (2 * (z - 1)));
        z += 1;
    }
    bases
};

/// One past the last valid archive tile id (the first id of z32).
pub const ARCHIVE_ID_END: u64 = BASES[ARCHIVE_MAX_ZOOM as usize + 1];

/// One past the last tile id of the profile domain (the first id of z15).
pub const PROFILE_ID_END: u64 = BASES[ZOOM_COUNT];

/// The tile-id range of zoom `z` as `(first, end_exclusive)`.
///
/// Panics when `z` exceeds [`ARCHIVE_MAX_ZOOM`]; zooms reaching this are
/// already validated.
pub fn zoom_ids(z: u8) -> (u64, u64) {
    assert!(
        z <= ARCHIVE_MAX_ZOOM,
        "zoom {z} is beyond the PMTiles id space"
    );
    (BASES[z as usize], BASES[z as usize + 1])
}

/// A valid PMTiles Hilbert tile id: `raw < ARCHIVE_ID_END`, so its zoom and
/// coordinates are always defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct TileId(u64);

impl TileId {
    /// The single z0 tile.
    pub const ZERO: Self = Self(0);

    /// Accept a raw id read from an archive or a sort key.
    pub const fn new(raw: u64) -> Option<Self> {
        if raw < ARCHIVE_ID_END {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// Convert tile coordinates to a tile id. Refuses `z > 31` and
    /// coordinates outside the `2^z` by `2^z` matrix.
    pub fn from_coord(z: u8, x: u32, y: u32) -> io::Result<Self> {
        if z > ARCHIVE_MAX_ZOOM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PMTiles tile zoom must be at most 31",
            ));
        }
        let grid_size = 1_u32 << z;
        if x >= grid_size || y >= grid_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PMTiles tile coordinates are outside the zoom grid",
            ));
        }
        Ok(Self(BASES[z as usize] + hilbert_xy2d(grid_size, x, y)))
    }

    /// [`TileId::from_coord`] for coordinates the caller has already bounded,
    /// such as tiles enumerated from a zoom's own matrix. Panics otherwise.
    pub fn at(z: u8, x: u32, y: u32) -> Self {
        match Self::from_coord(z, x, y) {
            Ok(id) => id,
            Err(error) => panic!("z{z}/{x}/{y}: {error}"),
        }
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The zoom holding this id.
    #[allow(clippy::cast_possible_truncation)]
    pub fn zoom(self) -> u8 {
        // BASES is increasing and BASES[0] == 0, so the partition point is at
        // least 1 and at most 32 for a valid id.
        (BASES.partition_point(|&base| base <= self.0) - 1) as u8
    }

    /// Whether this id lies in the profile domain, `z <= MAX_ZOOM`.
    pub const fn is_profile(self) -> bool {
        self.0 < PROFILE_ID_END
    }

    /// `(z, x, y)` of this id.
    pub fn coord(self) -> (u8, u32, u32) {
        let z = self.zoom();
        let (x, y) = hilbert_d2xy(1_u32 << z, self.0 - BASES[z as usize]);
        (z, x, y)
    }
}

impl fmt::Display for TileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Hilbert curve within one zoom's tile matrix
// ---------------------------------------------------------------------------

fn hilbert_xy2d(n: u32, x: u32, y: u32) -> u64 {
    let mut d: u64 = 0;
    let (mut x, mut y) = (x, y);
    let mut s = n / 2;
    while s > 0 {
        let rx: u32 = u32::from((x & s) > 0);
        let ry: u32 = u32::from((y & s) > 0);
        d += (u64::from(s) * u64::from(s)) * u64::from((3 * rx) ^ ry);
        // xy2d reflects within the full n x n grid (coords hold full values here),
        // unlike d2xy which reflects within the current sub-square size `s`.
        hilbert_rot(n, &mut x, &mut y, rx, ry);
        s /= 2;
    }
    d
}

fn hilbert_d2xy(n: u32, d: u64) -> (u32, u32) {
    let mut x: u32 = 0;
    let mut y: u32 = 0;
    let mut d = d;
    let mut s: u32 = 1;
    while s < n {
        // Extract 2-bit quadrant value. Mapping of (3*rx)^ry:
        //   0 -> (rx=0, ry=0)
        //   1 -> (rx=0, ry=1)
        //   2 -> (rx=1, ry=1)
        //   3 -> (rx=1, ry=0)
        #[allow(clippy::cast_possible_truncation)]
        let val = (d & 3) as u32;
        let rx = u32::from(val >= 2);
        let ry = u32::from(val == 1 || val == 2);
        hilbert_rot(s, &mut x, &mut y, rx, ry);
        x += s * rx;
        y += s * ry;
        d >>= 2;
        s <<= 1;
    }
    (x, y)
}

fn hilbert_rot(n: u32, x: &mut u32, y: &mut u32, rx: u32, ry: u32) {
    if ry == 0 {
        if rx == 1 {
            *x = n - 1 - *x;
            *y = n - 1 - *y;
        }
        std::mem::swap(x, y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(z: u8, x: u32, y: u32) -> u64 {
        TileId::at(z, x, y).raw()
    }

    #[test]
    fn test_hilbert_z0() {
        assert_eq!(id(0, 0, 0), 0);
        assert_eq!(TileId::ZERO.coord(), (0, 0, 0));
    }

    #[test]
    fn checked_hilbert_conversion_rejects_invalid_coordinates() {
        assert!(TileId::from_coord(31, (1_u32 << 31) - 1, (1_u32 << 31) - 1).is_ok());
        for (z, x, y) in [(32, 0, 0), (64, 0, 0), (1, 2, 0), (1, 0, 2)] {
            let error =
                TileId::from_coord(z, x, y).expect_err("invalid coordinates must be refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert!(std::panic::catch_unwind(|| TileId::at(1, 2, 0)).is_err());
    }

    #[test]
    fn test_hilbert_z1_known_values() {
        assert_eq!(id(1, 0, 0), 1);
        assert_eq!(id(1, 0, 1), 2);
        assert_eq!(id(1, 1, 1), 3);
        assert_eq!(id(1, 1, 0), 4);
    }

    #[test]
    fn test_hilbert_z2_base() {
        assert_eq!(id(2, 0, 0), 5);
    }

    #[test]
    fn test_hilbert_roundtrip_sampled_zooms() {
        for z in [1_u8, 2, 5, 10] {
            let n = 1_u32 << z;
            // Exhaustive through z5, sampled above.
            let step: usize = if z <= 5 { 1 } else { 37 };
            for x in (0..n).step_by(step) {
                for y in (0..n).step_by(step) {
                    let tile = TileId::at(z, x, y);
                    assert_eq!(tile.coord(), (z, x, y), "z{z}/{x}/{y}");
                }
            }
        }
    }

    #[test]
    fn zoom_boundaries_are_exact_through_z31() {
        for z in 0..=ARCHIVE_MAX_ZOOM {
            let (start, end) = zoom_ids(z);
            assert_eq!(u128::from(start), ((1_u128 << (2 * u32::from(z))) - 1) / 3);
            assert_eq!(
                u128::from(end),
                ((1_u128 << (2 * (u32::from(z) + 1))) - 1) / 3
            );
            let first = TileId::new(start).expect("zoom start is an address");
            let last = TileId::new(end - 1).expect("zoom last id is an address");
            assert_eq!(first.zoom(), z);
            assert_eq!(last.zoom(), z);
            let n = 1_u32 << z;
            assert_eq!(TileId::at(z, n - 1, 0).zoom(), z);
        }
        assert_eq!(zoom_ids(ARCHIVE_MAX_ZOOM).1, ARCHIVE_ID_END);
        assert!(TileId::new(ARCHIVE_ID_END).is_none());
        assert!(TileId::new(u64::MAX).is_none());
    }

    #[test]
    fn profile_domain_ends_at_the_first_z15_id() {
        assert_eq!(PROFILE_ID_END, zoom_ids(MAX_ZOOM + 1).0);
        assert!(TileId::at(MAX_ZOOM, 0, 0).is_profile());
        assert!(!TileId::at(MAX_ZOOM + 1, 0, 0).is_profile());
    }
}
