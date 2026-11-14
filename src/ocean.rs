// Ocean shapefile processing.
//
// Reads a water-polygons-split-3857 shapefile (mmap + .shx index), filters to
// data bounds, parses polygons, and processes them in parallel with rayon.
// Emits tiles through the shared int_ocean pyramid engine: quantize-early to
// max-zoom pixel space, then per-tile integer boolean clipping. No scanline
// fill, no point-in-polygon, no Sutherland-Hodgman, no LandMask.

use crate::geometry::int_ocean::{
    IntEmitScratch, IntRect, OCEAN_DP_TOL_PX, Shape, Shapes, intersect_rect_into, normalize_into,
    quantize_polygon, shape_bbox,
};
use crate::geometry::polygon::{Area, ExtentUnits, MIN_POLY_AREA_AREA};
use crate::geometry::pyramid::{
    PyramidCell, PyramidEmitKind, PyramidParams, PyramidScratch, Simplifier,
    buffered_cell_rect_base, emit_shape_pyramid, emit_shape_pyramid_cell, split_for_parallel,
    tile_cover,
};
use crate::geometry::tiles::grid::EXTENT_LOG2;
use crate::geometry::{
    self,
    projection::{MercBbox, Point},
};
use crate::mvt::GeomType;
use crate::pmtiles_reader::{ArchiveView, RawDirEntry};
#[cfg(test)]
use crate::pmtiles_writer;
use crate::shortbread::Layer;
use crate::sort::{self, SortWriter};
use crate::wire_format::{append_feature_data_with_attrs, encode_attrs_bytes};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

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
const FULL_FILL_FEATURE_ID: u64 = u64::MAX;

/// A canonical XXH3-128 digest: exactly 32 lowercase hexadecimal characters
/// whenever it is serialized or displayed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Xxh128(u128);

impl Xxh128 {
    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }

    pub const fn as_u128(self) -> u128 {
        self.0
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(xxhash_rust::xxh3::xxh3_128(bytes))
    }

    /// Stream a file through the shared digest implementation and return its
    /// byte length with the digest.
    pub fn hash_file(path: &std::path::Path) -> std::io::Result<(Self, u64)> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        let mut buf = vec![0_u8; 8 << 20];
        let mut len = 0_u64;
        loop {
            let read = file.read(&mut buf)?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            let read = u64::try_from(read)
                .map_err(|_| std::io::Error::other("read size does not fit a u64 byte count"))?;
            len = len
                .checked_add(read)
                .ok_or_else(|| std::io::Error::other("file length exceeds the u64 byte count"))?;
        }
        Ok((Self(hasher.digest128()), len))
    }
}

impl std::fmt::Display for Xxh128 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseXxh128Error;

impl std::fmt::Display for ParseXxh128Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("XXH3-128 must be 32 lowercase hexadecimal characters")
    }
}

impl std::error::Error for ParseXxh128Error {}

impl std::str::FromStr for Xxh128 {
    type Err = ParseXxh128Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ParseXxh128Error);
        }
        u128::from_str_radix(value, 16)
            .map(Self)
            .map_err(|_| ParseXxh128Error)
    }
}

impl From<u128> for Xxh128 {
    fn from(value: u128) -> Self {
        Self::from_u128(value)
    }
}

impl Serialize for Xxh128 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Xxh128 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// The `.shp` path and its matching `.shx` index, derived together so callers
/// cannot accidentally hash or open a mismatched pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapefilePair {
    shp: std::path::PathBuf,
    shx: std::path::PathBuf,
}

impl ShapefilePair {
    pub fn from_shp(shp: &std::path::Path) -> Self {
        Self {
            shp: shp.to_path_buf(),
            shx: shp.with_extension("shx"),
        }
    }

    pub fn shp(&self) -> &std::path::Path {
        &self.shp
    }

    pub fn shx(&self) -> &std::path::Path {
        &self.shx
    }
}

/// Identity of the shapefile inputs behind computed ocean sort chunks.
///
/// Deliberately NOT `OceanArtifactKey`, which carries `compression_level`.
/// This key exists to answer one question - may a `--skip-to` resume reuse
/// the ocean chunks already on disk - so it covers exactly what the ocean
/// phase bakes into those chunks: the source files and the zoom span. Chunks
/// hold geometry records, not encoded tiles, so compression level cannot
/// change them and gating on it would refuse a resume that is legitimate
/// (`--compression-level` is an assemble-side setting, and the checkpoint
/// producer config excludes those by design).
///
/// Without this the resume contract had a hole: a checkpoint recorded ocean
/// mode as the bare string "computed", so chunks built from one shapefile
/// could be reused by a resume naming another, and the archive's metadata
/// would then describe inputs that produced none of its ocean geometry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OceanSourceKey {
    pub full_shp_xxh128: Xxh128,
    pub full_shx_xxh128: Xxh128,
    pub simplified_shp_xxh128: Option<Xxh128>,
    pub simplified_shx_xxh128: Option<Xxh128>,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub policy_version: u32,
}

// Use a newtype deserializer rather than delegating directly to Option:
// serde treats a missing field as None when deserialize_option is called.
// These wire fields must be present, with explicit null for absent inputs.
#[derive(Deserialize)]
struct RequiredOption<T>(Option<T>);

#[derive(Deserialize)]
struct OceanSourceKeyWire {
    full_shp_xxh128: Xxh128,
    full_shx_xxh128: Xxh128,
    simplified_shp_xxh128: RequiredOption<Xxh128>,
    simplified_shx_xxh128: RequiredOption<Xxh128>,
    min_zoom: u8,
    max_zoom: u8,
    policy_version: u32,
}

impl<'de> Deserialize<'de> for OceanSourceKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = OceanSourceKeyWire::deserialize(deserializer)?;
        Ok(Self {
            full_shp_xxh128: wire.full_shp_xxh128,
            full_shx_xxh128: wire.full_shx_xxh128,
            simplified_shp_xxh128: wire.simplified_shp_xxh128.0,
            simplified_shx_xxh128: wire.simplified_shx_xxh128.0,
            min_zoom: wire.min_zoom,
            max_zoom: wire.max_zoom,
            policy_version: wire.policy_version,
        })
    }
}

impl OceanSourceKey {
    pub fn from_inputs(
        full: &ShapefilePair,
        simplified: Option<&ShapefilePair>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> std::io::Result<Self> {
        let (full_shp_xxh128, _) = Xxh128::hash_file(full.shp())?;
        let (full_shx_xxh128, _) = Xxh128::hash_file(full.shx())?;
        let (simplified_shp_xxh128, simplified_shx_xxh128) = if let Some(pair) = simplified {
            let (shp, _) = Xxh128::hash_file(pair.shp())?;
            let (shx, _) = Xxh128::hash_file(pair.shx())?;
            (Some(shp), Some(shx))
        } else {
            (None, None)
        };
        Ok(Self {
            full_shp_xxh128,
            full_shx_xxh128,
            simplified_shp_xxh128,
            simplified_shx_xxh128,
            min_zoom,
            max_zoom,
            policy_version: OCEAN_POLICY_VERSION,
        })
    }

    pub(crate) fn from_hashes(
        full: OceanShapefileHashes,
        simplified: Option<OceanShapefileHashes>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> Self {
        Self {
            full_shp_xxh128: full.shp_xxh128,
            full_shx_xxh128: full.shx_xxh128,
            simplified_shp_xxh128: simplified.map(|hashes| hashes.shp_xxh128),
            simplified_shx_xxh128: simplified.map(|hashes| hashes.shx_xxh128),
            min_zoom,
            max_zoom,
            policy_version: OCEAN_POLICY_VERSION,
        }
    }

    pub(crate) fn full_shapefile_hashes(&self) -> OceanShapefileHashes {
        OceanShapefileHashes {
            shp_xxh128: self.full_shp_xxh128,
            shx_xxh128: self.full_shx_xxh128,
        }
    }

    pub(crate) fn simplified_shapefile_hashes(&self) -> Option<OceanShapefileHashes> {
        Some(OceanShapefileHashes {
            shp_xxh128: self.simplified_shp_xxh128?,
            shx_xxh128: self.simplified_shx_xxh128?,
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("OceanSourceKey has an infallible JSON representation")
    }

    pub fn from_json(value: &serde_json::Value) -> std::io::Result<Self> {
        serde_json::from_value(value.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// The durable artifact invalidation key, encoded under `ocean_artifact`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OceanArtifactKey {
    pub source: OceanSourceKey,
    pub compression_level: u32,
}

impl OceanArtifactKey {
    pub fn from_inputs(
        full: &ShapefilePair,
        simplified: Option<&ShapefilePair>,
        min_zoom: u8,
        max_zoom: u8,
        compression_level: u32,
    ) -> std::io::Result<Self> {
        Ok(Self {
            source: OceanSourceKey::from_inputs(full, simplified, min_zoom, max_zoom)?,
            compression_level,
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("OceanArtifactKey has an infallible JSON representation")
    }

    pub fn json(&self) -> String {
        serde_json::to_string(self).expect("OceanArtifactKey has an infallible JSON representation")
    }

    pub fn from_json(value: &serde_json::Value) -> std::io::Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| std::io::Error::other("ocean_artifact must be an object"))?;
        for field in [
            "full_shp_xxh128",
            "full_shx_xxh128",
            "simplified_shp_xxh128",
            "simplified_shx_xxh128",
            "min_zoom",
            "max_zoom",
            "compression_level",
            "policy_version",
        ] {
            if !object.contains_key(field) {
                return Err(std::io::Error::other(format!(
                    "ocean_artifact missing {field}"
                )));
            }
        }
        serde_json::from_value(value.clone())
            .map_err(|e| std::io::Error::other(format!("invalid ocean_artifact: {e}")))
    }
}

impl std::ops::Deref for OceanArtifactKey {
    type Target = OceanSourceKey;

    fn deref(&self) -> &Self::Target {
        &self.source
    }
}

impl std::ops::DerefMut for OceanArtifactKey {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.source
    }
}

/// Flat durable wire shape. Keep this field order aligned with ocean metadata.
#[derive(Serialize)]
struct OceanArtifactKeyRef<'a> {
    full_shp_xxh128: &'a Xxh128,
    full_shx_xxh128: &'a Xxh128,
    simplified_shp_xxh128: &'a Option<Xxh128>,
    simplified_shx_xxh128: &'a Option<Xxh128>,
    min_zoom: u8,
    max_zoom: u8,
    compression_level: u32,
    policy_version: u32,
}

#[derive(Deserialize)]
struct OceanArtifactKeyWire {
    full_shp_xxh128: Xxh128,
    full_shx_xxh128: Xxh128,
    simplified_shp_xxh128: RequiredOption<Xxh128>,
    simplified_shx_xxh128: RequiredOption<Xxh128>,
    min_zoom: u8,
    max_zoom: u8,
    compression_level: u32,
    policy_version: u32,
}

impl Serialize for OceanArtifactKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        OceanArtifactKeyRef {
            full_shp_xxh128: &self.source.full_shp_xxh128,
            full_shx_xxh128: &self.source.full_shx_xxh128,
            simplified_shp_xxh128: &self.source.simplified_shp_xxh128,
            simplified_shx_xxh128: &self.source.simplified_shx_xxh128,
            min_zoom: self.source.min_zoom,
            max_zoom: self.source.max_zoom,
            compression_level: self.compression_level,
            policy_version: self.source.policy_version,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OceanArtifactKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = OceanArtifactKeyWire::deserialize(deserializer)?;
        Ok(Self {
            source: OceanSourceKey {
                full_shp_xxh128: wire.full_shp_xxh128,
                full_shx_xxh128: wire.full_shx_xxh128,
                simplified_shp_xxh128: wire.simplified_shp_xxh128.0,
                simplified_shx_xxh128: wire.simplified_shx_xxh128.0,
                min_zoom: wire.min_zoom,
                max_zoom: wire.max_zoom,
                policy_version: wire.policy_version,
            },
            compression_level: wire.compression_level,
        })
    }
}

// ---------------------------------------------------------------------------
// Ocean input stats
// ---------------------------------------------------------------------------
//
// Shapefile processing runs the low-zoom pass (simplified when configured,
// full-resolution otherwise) and the full-resolution z8+ pass. Its input-side
// counts accumulate into these process-global atomics and are drained at
// OCEAN_END by emit_ocean_counters.
// OceanAcc records its output at each spill flush because the phase12 sort
// snapshot predates the ocean pass. The output counters use
// payload bytes only, matching the other sort_layer_*_bytes counters; its
// accumulator byte budget also includes PayloadRecord storage and is not a
// comparable measure. The input side records how many shapefile shapes were
// read, how many overlapped the data bounds, how many polygon pieces they
// parsed into, and how many shapefile bytes were mapped. Ocean is ~30% of wall
// and a top allocator, so this is the phase most worth a look.

struct OceanStats {
    shapes: AtomicU64,
    shapes_hit: AtomicU64,
    pieces: AtomicU64,
    shapefile_bytes: AtomicU64,
    records: AtomicU64,
    payload_bytes: AtomicU64,
    zoom_records: [AtomicU64; ZOOM_COUNT],
    zoom_payload_bytes: [AtomicU64; ZOOM_COUNT],
}

static OCEAN_STATS: OceanStats = OceanStats {
    shapes: AtomicU64::new(0),
    shapes_hit: AtomicU64::new(0),
    pieces: AtomicU64::new(0),
    shapefile_bytes: AtomicU64::new(0),
    records: AtomicU64::new(0),
    payload_bytes: AtomicU64::new(0),
    zoom_records: [const { AtomicU64::new(0) }; ZOOM_COUNT],
    zoom_payload_bytes: [const { AtomicU64::new(0) }; ZOOM_COUNT],
};

use crate::tiling::ZOOM_COUNT;

static OCEAN_LAYER_STATS_ENABLED: OnceLock<bool> = OnceLock::new();

fn ocean_layer_stats_enabled() -> bool {
    *OCEAN_LAYER_STATS_ENABLED.get_or_init(|| std::env::var_os("ELIVAGAR_LAYER_STATS").is_some())
}

/// Integer grid describing one ocean pass for artifact ownership.
#[derive(Clone, Copy, Debug)]
pub struct OceanPassGrid {
    pub max_zoom: u8,
    pub inner_rect: IntRect,
    pub world_rect: IntRect,
}

impl OceanPassGrid {
    pub fn for_bounds(data_bounds: &MercBbox, max_zoom: u8) -> Self {
        let scale = 1_i64 << (u32::from(max_zoom) + EXTENT_LOG2);
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
                max_x: i32::try_from(scale).expect("world grid fits i32"),
                max_y: i32::try_from(scale).expect("world grid fits i32"),
            },
        }
    }

    pub fn band_is_empty(&self, min_zoom: u8, max_zoom: u8) -> bool {
        let _ = (min_zoom, max_zoom); // the containment condition is zoom-invariant.
        rect_contains(self.inner_rect, self.world_rect)
    }
}

/// Whether the computed pass owns this tile rather than the artifact.
pub fn ocean_band_tile(z: u8, tx: u32, ty: u32, pass: &OceanPassGrid) -> bool {
    if z > pass.max_zoom {
        return true;
    }
    // A regional extract touching the antimeridian has no representable
    // neighbour beyond x=0/x=max. Keep that column computed unless this pass
    // is the complete world, so a clipped world-edge fragment never leaks in
    // from the durable artifact.
    let max_tile = (1_u32 << z) - 1;
    if !rect_contains(pass.inner_rect, pass.world_rect)
        && ((tx == 0 && pass.inner_rect.min_x == pass.world_rect.min_x)
            || (tx == max_tile && pass.inner_rect.max_x == pass.world_rect.max_x))
    {
        return true;
    }
    let buffered = buffered_cell_rect_base(pass.max_zoom, PyramidCell { z, tx, ty });
    // Keep the former world clamp and pass-world clipping around the shared
    // unbounded buffer so antimeridian and edge ownership stay unchanged.
    let world_limit = i32::try_from(1_i64 << (u32::from(pass.max_zoom) + EXTENT_LOG2))
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

fn pass_owns_root_range(
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    pass: &OceanPassGrid,
    z_top: u8,
) -> bool {
    // This record-level reject runs before the polygon is decoded, so it uses
    // the shapefile's raw bbox. Pyramid root selection later uses normalized
    // quantized geometry bounds; the header bbox encloses those vertices and
    // rounding is monotone, so this remains conservative. Keep its existing
    // snap and pass the rect to tile_cover to preserve the previous ocean
    // root-range result while sharing the pyramid's index arithmetic.
    let scale = 1_i64 << (u32::from(pass.max_zoom) + EXTENT_LOG2);
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
    let (tx0, tx1, ty0, ty1) = tile_cover(bbox, pass.max_zoom, z_top, 0);
    for ty in ty0..=ty1 {
        for tx in tx0..=tx1 {
            if ocean_band_tile(z_top, tx, ty, pass) {
                return true;
            }
        }
    }
    false
}

/// Bound, mmap-backed ocean artifact shared by assembly workers.
pub struct OceanTiles {
    view: ArchiveView,
    runs: Vec<RawDirEntry>,
    grids: Vec<OceanPassGrid>,
}

impl OceanTiles {
    pub fn declared_key(path: &std::path::Path) -> std::io::Result<OceanArtifactKey> {
        let view = ArchiveView::open(path)?;
        let metadata: serde_json::Value = serde_json::from_str(&view.metadata()?)
            .map_err(|e| std::io::Error::other(format!("invalid ocean artifact metadata: {e}")))?;
        OceanArtifactKey::from_json(metadata.get("ocean_artifact").ok_or_else(|| {
            std::io::Error::other("ocean artifact missing ocean_artifact metadata")
        })?)
    }
    pub fn open(
        path: &std::path::Path,
        expected: &OceanArtifactKey,
        data_bounds: &MercBbox,
        pass_max_zooms: &[u8],
    ) -> std::io::Result<Self> {
        let view = ArchiveView::open(path)?;
        if view.min_zoom() != 0 || view.max_zoom() != crate::tiling::MAX_ZOOM {
            return Err(std::io::Error::other("ocean artifact must cover z0-14"));
        }
        let header = view.header();
        let min_lon = crate::pmtiles_reader::read_i32_le(header, 102);
        let min_lat = crate::pmtiles_reader::read_i32_le(header, 106);
        let max_lon = crate::pmtiles_reader::read_i32_le(header, 110);
        let max_lat = crate::pmtiles_reader::read_i32_le(header, 114);
        if min_lon > -1_800_000_000
            || min_lat > -850_500_000
            || max_lon < 1_800_000_000
            || max_lat < 850_500_000
        {
            return Err(std::io::Error::other(
                "ocean artifact header bounds must cover the world",
            ));
        }
        let metadata: serde_json::Value = serde_json::from_str(&view.metadata()?)
            .map_err(|e| std::io::Error::other(format!("invalid ocean artifact metadata: {e}")))?;
        let layers = metadata
            .get("vector_layers")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| std::io::Error::other("ocean artifact missing vector_layers"))?;
        if layers.len() != 1
            || layers[0].get("id").and_then(serde_json::Value::as_str) != Some(Layer::Ocean.name())
        {
            return Err(std::io::Error::other(
                "ocean artifact must declare exactly one ocean layer",
            ));
        }
        let key =
            OceanArtifactKey::from_json(metadata.get("ocean_artifact").ok_or_else(|| {
                std::io::Error::other("ocean artifact missing ocean_artifact metadata")
            })?)?;
        if &key != expected {
            return Err(std::io::Error::other(
                "ocean artifact key does not match current shapefile inputs",
            ));
        }
        let runs = view.read_all_runs()?;
        // Exclusive end: the first id past the top zoom is outside the
        // artifact contract.
        let profile_end = crate::tiling::PROFILE_ID_END;
        if runs.iter().any(|run| {
            run.tile_id >= profile_end
                || run
                    .tile_id
                    .checked_add(u64::from(run.run_length))
                    .is_none_or(|end| end > profile_end)
        }) {
            return Err(std::io::Error::other(
                "ocean artifact directory run lies outside z0-14 tile space",
            ));
        }
        let grids = pass_max_zooms
            .iter()
            .map(|&zoom| OceanPassGrid::for_bounds(data_bounds, zoom))
            .collect();
        Ok(Self { view, runs, grids })
    }

    pub fn grids(&self) -> &[OceanPassGrid] {
        &self.grids
    }
    /// Select the pass grid which owns a tile. Two configured grids split at
    /// z7/z8; one grid serves every requested zoom, regardless of source file.
    pub(crate) fn grid_for_zoom(&self, z: u8) -> Option<&OceanPassGrid> {
        selected_pass_grid(&self.grids, z)
    }

    pub fn band_tile(&self, z: u8, tx: u32, ty: u32) -> bool {
        let grid = self.grid_for_zoom(z);
        let Some(grid) = grid else {
            return false;
        };
        ocean_band_tile(z, tx, ty, grid)
    }
    pub fn runs_in(&self, start: u64, end: u64) -> &[RawDirEntry] {
        let first = self
            .runs
            .partition_point(|run| run.tile_id.saturating_add(u64::from(run.run_length)) <= start);
        let last = self.runs.partition_point(|run| run.tile_id < end);
        &self.runs[first..last]
    }
    pub fn run_covering(&self, tile_id: u64) -> Option<RawDirEntry> {
        let index = self.runs.partition_point(|run| run.tile_id <= tile_id);
        index
            .checked_sub(1)
            .and_then(|index| self.runs.get(index))
            .copied()
            .filter(|run| tile_id < run.tile_id.saturating_add(u64::from(run.run_length)))
    }
    pub fn raw_blob(&self, offset: u64, length: u32) -> std::io::Result<&[u8]> {
        self.view.raw_blob_at(offset, length)
    }
}

fn selected_pass_grid(grids: &[OceanPassGrid], z: u8) -> Option<&OceanPassGrid> {
    if grids.len() == 2 && z <= 7 {
        grids.first()
    } else {
        grids.last()
    }
}

/// Discard ocean totals left by a previous failed sequential pipeline run.
pub(crate) fn reset_ocean_counters() {
    for counter in [
        &OCEAN_STATS.shapes,
        &OCEAN_STATS.shapes_hit,
        &OCEAN_STATS.pieces,
        &OCEAN_STATS.shapefile_bytes,
        &OCEAN_STATS.records,
        &OCEAN_STATS.payload_bytes,
    ]
    .into_iter()
    .chain(OCEAN_STATS.zoom_records.iter())
    .chain(OCEAN_STATS.zoom_payload_bytes.iter())
    {
        counter.store(0, Ordering::Relaxed);
    }
}

/// Drain accumulated ocean input counters to the sidecar. Called once at
/// OCEAN_END; emits nothing when no shapefile records were processed.
pub(crate) fn emit_ocean_counters() {
    use crate::debug::emit_counter_u64;
    let shapes = OCEAN_STATS.shapes.swap(0, Ordering::Relaxed);
    let shapes_hit = OCEAN_STATS.shapes_hit.swap(0, Ordering::Relaxed);
    let pieces = OCEAN_STATS.pieces.swap(0, Ordering::Relaxed);
    let shapefile_bytes = OCEAN_STATS.shapefile_bytes.swap(0, Ordering::Relaxed);
    let records = OCEAN_STATS.records.swap(0, Ordering::Relaxed);
    let payload_bytes = OCEAN_STATS.payload_bytes.swap(0, Ordering::Relaxed);
    let zoom_records: [u64; ZOOM_COUNT] =
        std::array::from_fn(|z| OCEAN_STATS.zoom_records[z].swap(0, Ordering::Relaxed));
    let zoom_payload_bytes: [u64; ZOOM_COUNT] =
        std::array::from_fn(|z| OCEAN_STATS.zoom_payload_bytes[z].swap(0, Ordering::Relaxed));
    if shapes == 0 {
        return;
    }
    emit_counter_u64("ocean_shapes", shapes);
    emit_counter_u64("ocean_shapes_hit", shapes_hit);
    emit_counter_u64("ocean_pieces", pieces);
    emit_counter_u64("ocean_shapefile_bytes", shapefile_bytes);

    if records == 0 {
        return;
    }
    emit_counter_u64("sort_layer_ocean_records", records);
    emit_counter_u64("sort_layer_ocean_bytes", payload_bytes);
    if !ocean_layer_stats_enabled() {
        return;
    }
    for (z, (&records, &bytes)) in zoom_records.iter().zip(&zoom_payload_bytes).enumerate() {
        if records > 0 {
            emit_counter_u64(&format!("sort_layer_ocean_z{z}_records"), records);
        }
        if bytes > 0 {
            emit_counter_u64(&format!("sort_layer_ocean_z{z}_bytes"), bytes);
        }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

const LARGE_PIECE_VERTICES: usize = 1024;

/// Passes with `max_zoom` at or below this union their source pieces before
/// pyramid descent. The runtime always processes z0-z7 as its own pass,
/// sourcing it from the simplified file when configured and the full file
/// otherwise, so that pass unions in either input layout. The z8-z14 pass
/// stays per-piece.
const LOW_ZOOM_UNION_MAX_ZOOM: u8 = 7;

/// Union pre-split source pieces into seam-free shapes: every contour of
/// every piece goes into one soup, and the overlay engine's nonzero Simplify
/// re-derives ring roles geometrically. Quantization gives adjacent cells
/// bit-identical shared-edge coordinates, so interior cell edges cancel
/// exactly.
///
/// INVARIANT: nonzero composition of a contour soup equals set union only
/// when piece interiors are disjoint - the mosaic property the split source
/// data guarantees (cells partition the plane; each piece's holes lie inside
/// its own outer). If two records' interiors overlapped, an outer from one
/// could cancel against a hole from the other under summed winding where a
/// Boolean OR would stay filled. Overlapping source records are an input
/// defect, not a case this function handles.
fn union_pieces(pieces: Vec<Shape>) -> Vec<Shape> {
    if pieces.len() <= 1 {
        return pieces;
    }
    let contour_count: usize = pieces.iter().map(Vec::len).sum();
    let mut soup: Shape = Vec::with_capacity(contour_count);
    for mut piece in pieces {
        soup.append(&mut piece);
    }
    let mut scratch = IntEmitScratch::new();
    let mut merged: Shapes = Vec::new();
    normalize_into(&mut scratch, soup, Area::ZERO, &mut merged);
    merged
}
// Cap each parallel fold accumulator's in-flight payload well below the
// global sort_budget: with (piece x zoom) fan-out across many rayon workers,
// letting each balloon to the full budget before flushing would multiply peak
// RSS by the worker count. A flush is a memcpy into the shared spill
// coalescer (which owns chunk sizing), so small thresholds are cheap - the
// old direct-to-disk flushes at this size were the main source of the NA
// chunk fragmentation (2754 chunks, fan-in 1076; see sort::SpillCoalescer).
const OCEAN_CHUNK_SIZE_LIMIT: usize = 4 * 1024 * 1024;
const RING_READ_BUFFER_BYTES: usize = 64 * 1024;

struct ParsedOceanRecord {
    pieces: Vec<Shape>,
    shapes_hit: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OceanShapefileHashes {
    pub(crate) shp_xxh128: Xxh128,
    pub(crate) shx_xxh128: Xxh128,
}

#[derive(Clone, Copy)]
struct ShxRecord {
    index: usize,
    offset: usize,
    content_len: usize,
}

// Pyramid recursion scratch is built per work item, NOT pooled per thread: a
// thread-local fragment pool grows to the fattest piece each rayon thread ever
// descends and retains it for the whole phase. Only IntEmitScratch is reused by
// a rayon worker thread. Its overlay buffers are bounded by one operation, and
// root/rescale inputs are drawn from its ring and shape pools, so the pools do
// not grow once the worker is warm.
thread_local! {
    static OCEAN_INT_SCRATCH: std::cell::RefCell<IntEmitScratch> =
        std::cell::RefCell::new(IntEmitScratch::new());
}

enum OceanWorkKind {
    Whole(usize),
    Cell(PyramidCell, Shapes),
}

struct OceanWorkItem {
    feature_id: u64,
    kind: OceanWorkKind,
}

struct OceanAcc {
    records: Vec<sort::PayloadRecord>,
    payload: Vec<u8>,
    bytes: usize,
    count: u64,
}

impl OceanAcc {
    fn new() -> Self {
        Self {
            records: Vec::new(),
            payload: Vec::new(),
            bytes: 0,
            count: 0,
        }
    }

    fn flush(&mut self, spill: &sort::SpillCoalescer) {
        if self.records.is_empty() {
            return;
        }
        spill.append(&self.records, &self.payload);
        OCEAN_STATS
            .records
            .fetch_add(self.records.len() as u64, Ordering::Relaxed);
        OCEAN_STATS
            .payload_bytes
            .fetch_add(self.payload.len() as u64, Ordering::Relaxed);
        if ocean_layer_stats_enabled() {
            for &(key, _, len) in &self.records {
                // Keys are minted for profile zooms only (make_sort_key).
                let zoom = usize::from(sort::tile_id_from_key(key).zoom());
                OCEAN_STATS.zoom_records[zoom].fetch_add(1, Ordering::Relaxed);
                OCEAN_STATS.zoom_payload_bytes[zoom].fetch_add(len as u64, Ordering::Relaxed);
            }
        }
        self.count += self.records.len() as u64;
        self.records.clear();
        self.payload.clear();
        self.bytes = 0;
    }

    fn merge_from(&mut self, other: Self) {
        self.count += other.count;

        let payload_base = self.payload.len();
        self.payload.extend(other.payload);
        self.records.extend(
            other
                .records
                .into_iter()
                .map(|(key, offset, len)| (key, payload_base + offset, len)),
        );
        self.bytes += other.bytes;
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Process synthetic test inputs without a pre-resolved source key.
#[cfg(test)]
#[allow(clippy::too_many_lines, clippy::unwrap_in_result)]
#[hotpath::measure]
pub(crate) fn process_ocean_shapefile(
    path: &std::path::Path,
    data_bounds: &MercBbox,
    min_zoom: u8,
    max_zoom: u8,
    sort_writer: &mut SortWriter,
    pass_grid: Option<&OceanPassGrid>,
) -> Result<u64, std::io::Error> {
    process_ocean_shapefile_inner(
        path,
        data_bounds,
        min_zoom,
        max_zoom,
        sort_writer,
        pass_grid,
        false,
        None,
    )
    .map(|(count, _)| count)
}

/// Process a shapefile and return hashes of the source buffers used.
pub(crate) fn process_ocean_shapefile_with_hashes(
    path: &std::path::Path,
    data_bounds: &MercBbox,
    min_zoom: u8,
    max_zoom: u8,
    sort_writer: &mut SortWriter,
) -> Result<(u64, OceanShapefileHashes), std::io::Error> {
    let (count, hashes) = process_ocean_shapefile_inner(
        path,
        data_bounds,
        min_zoom,
        max_zoom,
        sort_writer,
        None,
        true,
        None,
    )?;
    let hashes = hashes.ok_or_else(|| {
        std::io::Error::other("ocean shapefile processing did not capture source hashes")
    })?;
    Ok((count, hashes))
}

/// Process a shapefile only if the buffers opened for this pass match the
/// source key resolved before phase12. The comparison happens before parsing
/// or emitting records, so a replaced input cannot stamp mixed chunks with
/// the earlier key.
#[hotpath::measure]
pub(crate) fn process_ocean_shapefile_checked(
    path: &std::path::Path,
    data_bounds: &MercBbox,
    min_zoom: u8,
    max_zoom: u8,
    sort_writer: &mut SortWriter,
    pass_grid: Option<&OceanPassGrid>,
    expected_hashes: OceanShapefileHashes,
) -> Result<u64, std::io::Error> {
    let (count, _) = process_ocean_shapefile_inner(
        path,
        data_bounds,
        min_zoom,
        max_zoom,
        sort_writer,
        pass_grid,
        true,
        Some(expected_hashes),
    )?;
    Ok(count)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::unwrap_in_result
)]
fn process_ocean_shapefile_inner(
    path: &std::path::Path,
    data_bounds: &MercBbox,
    min_zoom: u8,
    max_zoom: u8,
    sort_writer: &mut SortWriter,
    pass_grid: Option<&OceanPassGrid>,
    capture_hashes: bool,
    expected_hashes: Option<OceanShapefileHashes>,
) -> Result<(u64, Option<OceanShapefileHashes>), std::io::Error> {
    if pass_grid.is_some_and(|grid| grid.band_is_empty(min_zoom, max_zoom)) {
        eprintln!("  Ocean band is empty; skipping shapefile geometry");
        return Ok((0, None));
    }
    eprintln!("  Opening {}", path.display());

    // --- Read .shx index to get record offsets ---
    let files = ShapefilePair::from_shp(path);
    let shx_path = files.shx();
    let shx_data = std::fs::read(shx_path)?;

    if shx_data.len() < 100 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid .shx file {}: expected at least 100-byte header, got {} bytes",
                shx_path.display(),
                shx_data.len()
            ),
        ));
    }

    let index_bytes = shx_data.len() - 100;
    if index_bytes % 8 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid .shx file {}: {} trailing bytes after complete index records",
                shx_path.display(),
                index_bytes % 8
            ),
        ));
    }

    let shape_count = index_bytes / 8;
    let mut records: Vec<ShxRecord> = Vec::with_capacity(shape_count);
    for i in 0..shape_count {
        let base = 100 + i * 8;
        let offset_words = i32::from_be_bytes([
            shx_data[base],
            shx_data[base + 1],
            shx_data[base + 2],
            shx_data[base + 3],
        ]);
        let content_words = i32::from_be_bytes([
            shx_data[base + 4],
            shx_data[base + 5],
            shx_data[base + 6],
            shx_data[base + 7],
        ]);
        if offset_words < 0 || content_words < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid .shx record {} in {}: negative offset or content length",
                    i + 1,
                    shx_path.display()
                ),
            ));
        }
        let offset = usize::try_from(offset_words)
            .ok()
            .and_then(|words| words.checked_mul(2))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "invalid .shx record {} in {}: offset overflow",
                        i + 1,
                        shx_path.display()
                    ),
                )
            })?;
        let content_len = usize::try_from(content_words)
            .ok()
            .and_then(|words| words.checked_mul(2))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "invalid .shx record {} in {}: content length overflow",
                        i + 1,
                        shx_path.display()
                    ),
                )
            })?;
        records.push(ShxRecord {
            index: i + 1,
            offset,
            content_len,
        });
    }
    eprintln!("  Index: {shape_count} shapes");

    // --- Mmap the .shp file ---
    let shp_file = std::fs::File::open(files.shp())?;
    let shp_mmap = unsafe { memmap2::Mmap::map(&shp_file) }?;
    // Hash the exact mapped bytes and index buffer consumed here. Artifact
    // builds record them in the key; regular runs compare them with the key
    // resolved before phase12.
    let hashes = capture_hashes.then(|| OceanShapefileHashes {
        shp_xxh128: Xxh128::from_bytes(&shp_mmap),
        shx_xxh128: Xxh128::from_bytes(&shx_data),
    });
    if let Some(expected) = expected_hashes {
        let actual = hashes.expect("checked processing captures source hashes");
        if actual != expected {
            return Err(std::io::Error::other(format!(
                "ocean shapefile inputs changed after source-key resolution for {}: expected .shp/.shx hashes {}/{}, opened {}/{}",
                path.display(),
                expected.shp_xxh128,
                expected.shx_xxh128,
                actual.shp_xxh128,
                actual.shx_xxh128,
            )));
        }
    }
    #[cfg(unix)]
    shp_mmap.advise(memmap2::Advice::Random)?;
    eprintln!(
        "  Mmapped {:.1} MB",
        shp_mmap.len() as f64 / (1024.0 * 1024.0)
    );
    OCEAN_STATS.shapefile_bytes.fetch_add(
        u64::try_from(shp_mmap.len()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    let data_rect = data_bounds_rect(data_bounds, max_zoom);

    // --- Parse phase: extract and clip polygons in parallel ---
    use rayon::prelude::*;

    let parse_ctx = OceanRecordParseCtx {
        path,
        shp_mmap: &shp_mmap,
        shp_file: &shp_file,
        data_bounds,
        max_zoom,
        data_rect,
        pass_grid,
        min_zoom,
    };
    let parsed_records: Result<Vec<ParsedOceanRecord>, std::io::Error> = records
        .par_iter()
        .map_init(IntEmitScratch::new, |scratch, &record| {
            parse_ocean_record(record, &parse_ctx, scratch)
        })
        .collect();
    let parsed_records = parsed_records?;
    #[cfg(unix)]
    unsafe {
        shp_mmap.unchecked_advise(memmap2::UncheckedAdvice::DontNeed)?;
    }
    drop(shp_mmap);
    let shapes_hit: u64 = parsed_records.iter().map(|record| record.shapes_hit).sum();
    let parsed_pieces: usize = parsed_records
        .iter()
        .map(|record| record.pieces.len())
        .sum();

    let mut pieces: Vec<Shape> = Vec::with_capacity(parsed_pieces);
    for record in parsed_records {
        pieces.extend(record.pieces);
    }

    OCEAN_STATS.shapes.fetch_add(
        u64::try_from(shape_count).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    OCEAN_STATS
        .shapes_hit
        .fetch_add(shapes_hit, Ordering::Relaxed);
    // `ocean_pieces` keeps its documented meaning: parsed input pieces,
    // counted before the low-zoom union below.
    OCEAN_STATS.pieces.fetch_add(
        u64::try_from(pieces.len()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    // The source shapefiles arrive pre-split into grid cells, and each cell
    // used to descend the pyramid independently: adjacent cells simplified
    // their shared coastline into disagreeing shapes (seam wedges) and the
    // per-zoom min-area drop applied per cell fragment, deleting the smaller
    // half of any landform straddling a cell edge (half-peninsula spikes).
    // For the low-zoom pass the split grid is comparable to the tile size,
    // which made both artifacts obvious at z2, so its pieces are unioned into
    // seam-free shapes before descent. The z8-z14 full-polygons pass keeps
    // per-piece descent: its fixed pixel tolerances keep any seam mismatch
    // sub-pixel, and a world-scale union of the full polygons would dominate
    // `ocean-build`. Union input order is deterministic (rayon's indexed
    // collect preserves record order) and the overlay step is serial, so
    // emitted bytes stay deterministic.
    if max_zoom <= LOW_ZOOM_UNION_MAX_ZOOM {
        let union_start = std::time::Instant::now();
        let input_pieces = pieces.len();
        let input_vertices: usize = pieces.iter().map(total_vertices).sum();
        pieces = union_pieces(pieces);
        let output_vertices: usize = pieces.iter().map(total_vertices).sum();
        let union_elapsed = union_start.elapsed();
        eprintln!(
            "  Union: {input_pieces} pieces ({input_vertices} vertices) -> {} shapes ({output_vertices} vertices) in {union_elapsed:.2?}",
            pieces.len()
        );
        use crate::debug::emit_counter_u64;
        emit_counter_u64(
            "ocean_union_ns",
            u64::try_from(union_elapsed.as_nanos()).unwrap_or(u64::MAX),
        );
        emit_counter_u64(
            "ocean_union_input_pieces",
            u64::try_from(input_pieces).unwrap_or(u64::MAX),
        );
        emit_counter_u64(
            "ocean_union_input_vertices",
            u64::try_from(input_vertices).unwrap_or(u64::MAX),
        );
        emit_counter_u64(
            "ocean_union_shapes",
            u64::try_from(pieces.len()).unwrap_or(u64::MAX),
        );
        emit_counter_u64(
            "ocean_union_output_vertices",
            u64::try_from(output_vertices).unwrap_or(u64::MAX),
        );
    }

    let poly_count = pieces.len();
    eprintln!(
        "  {shape_count} shapes, {shapes_hit} in bounds, {poly_count} polygons - processing in parallel"
    );

    // --- Process phase: parallel with rayon, direct chunk flushing ---
    //
    // Each rayon worker accumulates records in a thread-local buffer and flushes
    // directly to a chunk file when the buffer exceeds chunk_size_bytes. This
    // avoids holding all ocean sort records in memory simultaneously - at planet
    // scale that could be 10-30 GB. The previous approach (par_iter().collect()
    // into Vec<Vec<SortRecord>> + serial push) was fine for regional extracts but
    // would blow memory and serialize sort+flush at planet scale.
    let ocean_layer = Layer::Ocean as u8;
    let mut empty_attrs_bytes = Vec::new();
    encode_attrs_bytes(&mut empty_attrs_bytes, &[], max_zoom);

    // Ocean chunks share the current partitioned filename grammar and chunk
    // ID sequence with PBF chunks, so resume scans find the complete set.
    let spill = sort::SpillCoalescer::new(sort_writer.chunk_sink(), sort_writer.chunk_size_bytes());
    let chunk_size = sort_writer.chunk_size_bytes().min(OCEAN_CHUNK_SIZE_LIMIT);

    let tile_filter = |z, tx, ty| pass_grid.is_none_or(|grid| ocean_band_tile(z, tx, ty, grid));
    let tile_filter_dyn: Option<&(dyn Fn(u8, u32, u32) -> bool + Sync)> =
        pass_grid.map(|_| &tile_filter as &(dyn Fn(u8, u32, u32) -> bool + Sync));
    let params = ocean_params(min_zoom, max_zoom, tile_filter_dyn);
    // Per-piece item target for the parallel frontier. Root cells of a
    // large piece usually exceed this on their own (the frontier loop is
    // then a no-op); it only forces expansion for single-root ranges.
    const SPLIT_ITEMS_PER_PIECE: usize = 16;
    // Split phase runs in parallel across pieces; each large piece emits
    // its above-frontier tiles into its own accumulator, merged (and
    // flushed) serially afterwards.
    let piece_prep: Vec<(Vec<OceanWorkItem>, Option<OceanAcc>)> = pieces
        .par_iter()
        .enumerate()
        .map(|(piece_idx, piece)| {
            let feature_id = piece_idx as u64;
            if total_vertices(piece) < LARGE_PIECE_VERTICES {
                return (
                    vec![OceanWorkItem {
                        feature_id,
                        kind: OceanWorkKind::Whole(piece_idx),
                    }],
                    None,
                );
            }
            let mut acc = OceanAcc::new();
            let cells = {
                let mut scratch = PyramidScratch::new();
                let mut sink = ocean_sink(feature_id, ocean_layer, &empty_attrs_bytes, &mut acc);
                split_for_parallel(
                    piece,
                    &params,
                    SPLIT_ITEMS_PER_PIECE,
                    &mut scratch,
                    &mut sink,
                )
            };
            if acc.bytes >= chunk_size {
                acc.flush(&spill);
            }
            let items = cells
                .into_iter()
                .map(|(cell, frag)| OceanWorkItem {
                    feature_id,
                    kind: OceanWorkKind::Cell(cell, frag),
                })
                .collect();
            (items, Some(acc))
        })
        .collect();

    let mut work_items = Vec::with_capacity(pieces.len());
    let mut pre_emit = OceanAcc::new();
    for (items, acc) in piece_prep {
        work_items.extend(items);
        if let Some(acc) = acc {
            pre_emit.merge_from(acc);
            if pre_emit.bytes >= chunk_size {
                pre_emit.flush(&spill);
            }
        }
    }

    let mut result = work_items
        .into_par_iter()
        .fold(OceanAcc::new, |mut acc, item| {
            emit_ocean_piece(
                item,
                &pieces,
                &params,
                ocean_layer,
                &empty_attrs_bytes,
                &mut acc,
            );
            if acc.bytes >= chunk_size {
                acc.flush(&spill);
            }
            acc
        })
        .reduce(OceanAcc::new, |mut a, b| {
            a.merge_from(b);
            if a.bytes >= chunk_size {
                a.flush(&spill);
            }
            a
        });

    result.merge_from(pre_emit);
    result.flush(&spill);
    sort_writer.adopt_chunk_files(spill.finish());
    let count = result.count;

    eprintln!("  {poly_count} polygons, {count} features");
    Ok((count, hashes))
}

struct OceanRecordParseCtx<'a> {
    path: &'a std::path::Path,
    shp_mmap: &'a memmap2::Mmap,
    shp_file: &'a std::fs::File,
    data_bounds: &'a MercBbox,
    max_zoom: u8,
    data_rect: IntRect,
    pass_grid: Option<&'a OceanPassGrid>,
    min_zoom: u8,
}

// Fixed-width field readers. Callers have already bounds-checked the slice;
// these are plain functions so the expect does not sit in a Result function.
fn field_bytes<const N: usize>(buf: &[u8], off: usize) -> [u8; N] {
    buf[off..off + N].try_into().expect("slice length equals N")
}

fn be_u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(field_bytes(buf, off))
}

fn le_i32_at(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(field_bytes(buf, off))
}

fn le_f64_at(buf: &[u8], off: usize) -> f64 {
    f64::from_le_bytes(field_bytes(buf, off))
}

fn u64_from_usize(v: usize) -> u64 {
    u64::try_from(v).expect("shapefile offset fits u64")
}

#[allow(clippy::too_many_lines)]
fn parse_ocean_record(
    record: ShxRecord,
    ctx: &OceanRecordParseCtx<'_>,
    scratch: &mut IntEmitScratch,
) -> Result<ParsedOceanRecord, std::io::Error> {
    let mut out = ParsedOceanRecord {
        pieces: Vec::new(),
        shapes_hit: 0,
    };

    let header_end = record.offset.checked_add(8).ok_or_else(|| {
        invalid_shapefile_record(ctx.path, record, "record header offset overflow")
    })?;
    let content_end = header_end
        .checked_add(record.content_len)
        .ok_or_else(|| invalid_shapefile_record(ctx.path, record, "content range overflow"))?;
    if record.offset < 100 || content_end > ctx.shp_mmap.len() {
        return Err(invalid_shapefile_record(
            ctx.path,
            record,
            "record content extends past the .shp file",
        ));
    }

    let declared_content_words = be_u32_at(&ctx.shp_mmap[..header_end], record.offset + 4);
    let declared_content_len = usize::try_from(declared_content_words)
        .ok()
        .and_then(|words| words.checked_mul(2))
        .ok_or_else(|| {
            invalid_shapefile_record(ctx.path, record, "record content length overflow")
        })?;
    if declared_content_len != record.content_len {
        return Err(invalid_shapefile_record(
            ctx.path,
            record,
            "record length differs from the .shx index",
        ));
    }
    if record.content_len < 4 {
        return Err(invalid_shapefile_record(
            ctx.path,
            record,
            "record is shorter than its shape type",
        ));
    }

    let rec = header_end;
    let shape_type = le_i32_at(&ctx.shp_mmap[..rec + 4], rec);
    if shape_type == 0 {
        return Ok(out);
    }
    if !matches!(shape_type, 5 | 15 | 25) {
        return Err(invalid_shapefile_record(
            ctx.path,
            record,
            "shape type is not Polygon, PolygonZ or PolygonM",
        ));
    }
    if record.content_len < 44 {
        return Err(invalid_shapefile_record(
            ctx.path,
            record,
            "polygon record is shorter than its fixed header",
        ));
    }

    let header = {
        let shp = &ctx.shp_mmap[..];
        let mut header = [0_u8; 44];
        header.copy_from_slice(&shp[rec..rec + 44]);
        header
    };

    let xmin = le_f64_at(&header, 4);
    let ymin = le_f64_at(&header, 12);
    let xmax = le_f64_at(&header, 20);
    let ymax = le_f64_at(&header, 28);
    if ![xmin, ymin, xmax, ymax]
        .iter()
        .all(|value| value.is_finite())
        || xmin > xmax
        || ymin > ymax
    {
        return Err(invalid_shapefile_record(
            ctx.path,
            record,
            "polygon bounds are non-finite or inverted",
        ));
    }

    let merc_min = geometry::projection::from_epsg3857(xmin, ymax);
    let merc_max = geometry::projection::from_epsg3857(xmax, ymin);

    if merc_max.x < ctx.data_bounds.min_x
        || merc_min.x > ctx.data_bounds.max_x
        || merc_max.y < ctx.data_bounds.min_y
        || merc_min.y > ctx.data_bounds.max_y
    {
        return Ok(out);
    }

    if let Some(pass) = ctx.pass_grid
        && !pass_owns_root_range(
            merc_min.x,
            merc_min.y,
            merc_max.x,
            merc_max.y,
            pass,
            ctx.min_zoom,
        )
    {
        return Ok(out);
    }

    out.shapes_hit = 1;

    let source = ShapeRecordSource {
        record,
        rec,
        shp_len: ctx.shp_mmap.len(),
        path: ctx.path,
        shp_file: ctx.shp_file,
        header: &header,
    };
    push_shape_record_pieces(
        &source,
        scratch,
        &mut out.pieces,
        ctx.max_zoom,
        ctx.data_rect,
    )?;

    Ok(out)
}

fn invalid_shapefile_record(
    path: &std::path::Path,
    record: ShxRecord,
    reason: &str,
) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "invalid shapefile record {} at offset {} in {}: {reason}",
            record.index,
            record.offset,
            path.display()
        ),
    )
}

struct ShapeRecordSource<'a> {
    record: ShxRecord,
    rec: usize,
    shp_len: usize,
    path: &'a std::path::Path,
    shp_file: &'a std::fs::File,
    header: &'a [u8; 44],
}

#[allow(clippy::too_many_lines)]
fn push_shape_record_pieces(
    source: &ShapeRecordSource<'_>,
    scratch: &mut IntEmitScratch,
    pieces: &mut Vec<Shape>,
    max_zoom: u8,
    data_rect: IntRect,
) -> Result<(), std::io::Error> {
    let num_parts_i32 = le_i32_at(source.header, 36);
    let num_points_i32 = le_i32_at(source.header, 40);
    if num_parts_i32 < 0 || num_points_i32 < 0 {
        return Err(invalid_shapefile_record(
            source.path,
            source.record,
            "negative part or point count",
        ));
    }
    let num_parts = usize::try_from(num_parts_i32)
        .map_err(|_| invalid_shapefile_record(source.path, source.record, "part count overflow"))?;
    let num_points = usize::try_from(num_points_i32).map_err(|_| {
        invalid_shapefile_record(source.path, source.record, "point count overflow")
    })?;

    let parts_start = 44usize;
    let parts_bytes = num_parts.checked_mul(4).ok_or_else(|| {
        invalid_shapefile_record(source.path, source.record, "part index range overflow")
    })?;
    let points_start = parts_start.checked_add(parts_bytes).ok_or_else(|| {
        invalid_shapefile_record(source.path, source.record, "point range offset overflow")
    })?;
    let points_bytes = num_points.checked_mul(16).ok_or_else(|| {
        invalid_shapefile_record(source.path, source.record, "point range overflow")
    })?;
    let record_end = points_start.checked_add(points_bytes).ok_or_else(|| {
        invalid_shapefile_record(source.path, source.record, "record content range overflow")
    })?;
    let absolute_end = source.rec.checked_add(record_end).ok_or_else(|| {
        invalid_shapefile_record(source.path, source.record, "record file range overflow")
    })?;
    if record_end > source.record.content_len || absolute_end > source.shp_len {
        return Err(invalid_shapefile_record(
            source.path,
            source.record,
            "part and point arrays extend past the record",
        ));
    }
    if num_parts == 0 || num_points == 0 {
        return Err(invalid_shapefile_record(
            source.path,
            source.record,
            "polygon record has no parts or points",
        ));
    }

    let mut ring_starts = read_ring_starts(source, num_parts)?;
    if ring_starts.first() != Some(&0)
        || ring_starts.last().is_some_and(|&last| last >= num_points)
        || ring_starts.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(invalid_shapefile_record(
            source.path,
            source.record,
            "part indices are not strictly increasing from zero within the point array",
        ));
    }
    ring_starts.push(num_points);

    let mut current_outer: Option<Vec<Point>> = None;
    let mut current_inners: Vec<Vec<Point>> = Vec::new();

    for (w, window) in ring_starts.windows(2).enumerate() {
        let ring = read_ring_points(source, points_start, window[0], window[1])?;
        // Ring interpretation follows the split-water input convention: the
        // first contour is outer, later nonnegative contours start outers,
        // and negative contours attach to the latest outer. The checked-in
        // full OSMCD input has four zero-area contours, one after the first;
        // keep zero as an outer to preserve the old >= 0 grouping and ocean
        // bytes. The low-zoom contour-soup union relies on each piece keeping
        // its own holes and having a disjoint interior, as documented at
        // union_pieces.
        let is_outer = if w == 0 {
            true
        } else {
            let signed_area = geometry::polygon::signed_area(&ring);
            match geometry::polygon::ring_role(signed_area) {
                Some(geometry::polygon::RingOrientation::Positive) => true,
                Some(geometry::polygon::RingOrientation::Negative) => false,
                None => signed_area == 0.0,
            }
        };

        if is_outer {
            if let Some(outer) = current_outer.take() {
                push_quantized_pieces(
                    scratch,
                    pieces,
                    &outer,
                    &std::mem::take(&mut current_inners),
                    max_zoom,
                    data_rect,
                );
            }
            current_outer = Some(ring);
        } else if current_outer.is_some() {
            current_inners.push(ring);
        }
    }

    if let Some(outer) = current_outer {
        push_quantized_pieces(
            scratch,
            pieces,
            &outer,
            &current_inners,
            max_zoom,
            data_rect,
        );
    }
    Ok(())
}

fn read_ring_starts(
    source: &ShapeRecordSource<'_>,
    num_parts: usize,
) -> Result<Vec<usize>, std::io::Error> {
    let mut parts = vec![0_u8; num_parts * 4];
    if !parts.is_empty() {
        let read_offset = u64_from_usize(source.rec + 44);
        if let Err(err) = source.shp_file.read_exact_at(&mut parts, read_offset) {
            return Err(std::io::Error::new(
                err.kind(),
                format!(
                    "invalid shapefile record {} at offset {} in {}: failed to read part indices: {err}",
                    source.record.index,
                    source.record.offset,
                    source.path.display()
                ),
            ));
        }
    }
    (0..num_parts)
        .map(|j| {
            let b = j * 4;
            let v = le_i32_at(&parts, b);
            usize::try_from(v).map_err(|_| {
                invalid_shapefile_record(source.path, source.record, "negative part index")
            })
        })
        .collect()
}

fn read_ring_points(
    source: &ShapeRecordSource<'_>,
    points_start: usize,
    start: usize,
    end: usize,
) -> Result<Vec<Point>, std::io::Error> {
    let point_count = end - start;
    if point_count < 4 {
        return Err(invalid_shapefile_record(
            source.path,
            source.record,
            "polygon ring has fewer than four points including closure",
        ));
    }
    let mut ring = Vec::with_capacity(point_count);
    let mut buf = vec![0_u8; RING_READ_BUFFER_BYTES];
    let mut points_read = 0usize;

    while points_read < point_count {
        let points_this_read = ((point_count - points_read) * 16).min(buf.len()) / 16;
        let byte_len = points_this_read * 16;
        let read_offset = u64_from_usize(source.rec + points_start + (start + points_read) * 16);
        if let Err(err) = source
            .shp_file
            .read_exact_at(&mut buf[..byte_len], read_offset)
        {
            return Err(std::io::Error::new(
                err.kind(),
                format!(
                    "invalid shapefile record {} at offset {} in {}: failed to read point coordinates: {err}",
                    source.record.index,
                    source.record.offset,
                    source.path.display()
                ),
            ));
        }
        for idx in 0..points_this_read {
            let b = idx * 16;
            let x = le_f64_at(&buf, b);
            let y = le_f64_at(&buf, b + 8);
            if !x.is_finite() || !y.is_finite() {
                return Err(invalid_shapefile_record(
                    source.path,
                    source.record,
                    "polygon point coordinate is non-finite",
                ));
            }
            ring.push(geometry::projection::from_epsg3857(x, y));
        }
        points_read += points_this_read;
    }
    // Shapefile closure repeats the endpoint exactly; it is not a proximity
    // check like the later geometry simplification tolerances.
    #[allow(clippy::float_cmp)]
    let closed = ring
        .first()
        .zip(ring.last())
        .is_some_and(|(first, last)| first.x == last.x && first.y == last.y);
    if !closed {
        return Err(invalid_shapefile_record(
            source.path,
            source.record,
            "polygon ring is not closed",
        ));
    }
    Ok(ring)
}

fn push_quantized_pieces(
    scratch: &mut IntEmitScratch,
    pieces: &mut Vec<Shape>,
    outer: &[Point],
    inners: &[Vec<Point>],
    max_zoom: u8,
    data_rect: IntRect,
) {
    let shape = quantize_polygon(outer, inners, max_zoom);
    if shape.is_empty() {
        return;
    }

    // The common case for an in-bounds extract: the shape lies entirely
    // inside the data bounds - the boolean is an expensive identity.
    if shape_bbox(&shape).is_some_and(|bb| rect_contains(data_rect, bb)) {
        pieces.push(shape);
        return;
    }

    let mut clipped = Vec::new();
    intersect_rect_into(scratch, &shape, data_rect, Area::ZERO, &mut clipped);
    for piece in clipped {
        pieces.push(piece);
    }
}

/// True if `outer` contains `inner` (closed containment).
fn rect_contains(outer: IntRect, inner: IntRect) -> bool {
    outer.min_x <= inner.min_x
        && outer.min_y <= inner.min_y
        && outer.max_x >= inner.max_x
        && outer.max_y >= inner.max_y
}

fn data_bounds_rect(data_bounds: &MercBbox, max_zoom: u8) -> IntRect {
    let scale = 1_i64 << (u32::from(max_zoom) + EXTENT_LOG2);
    IntRect {
        min_x: merc_floor(data_bounds.min_x, scale),
        min_y: merc_floor(data_bounds.min_y, scale),
        max_x: merc_ceil(data_bounds.max_x, scale),
        max_y: merc_ceil(data_bounds.max_y, scale),
    }
}

fn merc_floor(v: f64, scale: i64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let q = (v * scale as f64).floor() as i64;
    i32::try_from(q.clamp(0, scale)).expect("base ocean coordinate fits i32")
}

fn merc_ceil(v: f64, scale: i64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let q = (v * scale as f64).ceil() as i64;
    i32::try_from(q.clamp(0, scale)).expect("base ocean coordinate fits i32")
}

fn total_vertices(piece: &Shape) -> usize {
    piece.iter().map(Vec::len).sum()
}

/// Everything this block selects - simplifier, tolerance, minimum area, the
/// zoom span - determines the durable ocean tile bytes `ocean-build` bakes
/// into the world artifact. Changing any of it, or the engines it dispatches
/// into (the pyramid descent, the int_ocean simplifiers, the overlay boolean
/// engine, the MVT encoder), requires bumping OCEAN_POLICY_VERSION: without
/// the bump every existing artifact keeps key-validating and silently serves
/// the pre-change geometry. The VW simplifier switch shipped exactly that way
/// and served DP-era coastline spikes world-wide for three days.
fn ocean_params<'a>(
    min_zoom: u8,
    max_zoom: u8,
    tile_filter: Option<&'a (dyn Fn(u8, u32, u32) -> bool + Sync)>,
) -> PyramidParams<'a> {
    PyramidParams {
        maxz: max_zoom,
        z_top: min_zoom,
        z_bottom: max_zoom,
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

fn ocean_sink<'a>(
    feature_id: u64,
    layer_idx: u8,
    attrs_bytes: &'a [u8],
    acc: &'a mut OceanAcc,
) -> impl FnMut(u8, u32, u32, &[u32], PyramidEmitKind) + 'a {
    move |z: u8, tx: u32, ty: u32, geom: &[u32], kind: PyramidEmitKind| {
        let tile_id = crate::tiling::TileId::at(z, tx, ty);
        let key = sort::make_sort_key(tile_id, layer_idx, 0);
        let range = append_feature_data_with_attrs(
            &mut acc.payload,
            if kind == PyramidEmitKind::FullFill {
                FULL_FILL_FEATURE_ID
            } else {
                feature_id
            },
            GeomType::Polygon,
            geom,
            attrs_bytes,
        );
        acc.bytes += range.len() + std::mem::size_of::<sort::PayloadRecord>();
        acc.records.push((key, range.start, range.len()));
    }
}

#[hotpath::measure]
fn emit_ocean_piece(
    item: OceanWorkItem,
    pieces: &[Shape],
    params: &PyramidParams<'_>,
    layer_idx: u8,
    attrs_bytes: &[u8],
    acc: &mut OceanAcc,
) {
    OCEAN_INT_SCRATCH.with(|int_cell| {
        let mut int_scratch = int_cell.borrow_mut();
        let mut scratch = PyramidScratch::new();
        std::mem::swap(&mut scratch.int, &mut *int_scratch);
        let mut sink = ocean_sink(item.feature_id, layer_idx, attrs_bytes, acc);
        match item.kind {
            OceanWorkKind::Whole(piece_idx) => {
                emit_shape_pyramid(&pieces[piece_idx], params, &mut scratch, &mut sink);
            }
            OceanWorkKind::Cell(cell, frag) => {
                emit_shape_pyramid_cell(cell, frag, params, &mut scratch, &mut sink);
            }
        }
        std::mem::swap(&mut scratch.int, &mut *int_scratch);
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::geometry::int_ocean::IntPoint;
    use crate::geometry::projection::Point;
    use crate::sort::SortWriter;
    use std::fs;
    use std::path::Path;

    fn record_osm_id(data: &[u8]) -> u64 {
        // The sort record payload leads with the raw little-endian osm_id
        // (wire_format.rs layout), not a protobuf message.
        u64::from_le_bytes(data[0..8].try_into().expect("record osm_id"))
    }

    #[test]
    fn overlapping_piece_zero_fragment_and_full_fill_have_distinct_ids() {
        let cell = PyramidCell {
            z: 8,
            tx: 100,
            ty: 100,
        };
        let rect = buffered_cell_rect_base(8, cell);
        let rectangle = |rect: IntRect| {
            vec![vec![
                IntPoint::new(rect.min_x, rect.min_y),
                IntPoint::new(rect.max_x, rect.min_y),
                IntPoint::new(rect.max_x, rect.max_y),
                IntPoint::new(rect.min_x, rect.max_y),
            ]]
        };
        // Piece zero crosses the tile edge, while piece one overlaps it by
        // only a narrow strip and covers the complete buffered tile.
        let piece_zero = rectangle(IntRect {
            min_x: rect.min_x - 3_000,
            min_y: rect.min_y + 1_000,
            max_x: rect.min_x + 240,
            max_y: rect.min_y + 1_040,
        });
        let piece_one = rectangle(IntRect {
            min_x: rect.min_x - 2_048,
            min_y: rect.min_y - 2_048,
            max_x: rect.max_x + 2_048,
            max_y: rect.max_y + 2_048,
        });
        let params = ocean_params(8, 8, None);
        let mut attrs = Vec::new();
        encode_attrs_bytes(&mut attrs, &[], 8);
        let tile_id = crate::tiling::TileId::at(cell.z, cell.tx, cell.ty);

        let mut fragment_acc = OceanAcc::new();
        let mut fragment_kinds = Vec::new();
        let mut fragment_sink = ocean_sink(0, Layer::Ocean as u8, &attrs, &mut fragment_acc);
        let mut scratch = PyramidScratch::new();
        emit_shape_pyramid(
            &piece_zero,
            &params,
            &mut scratch,
            &mut |z, tx, ty, geom, kind| {
                if (z, tx, ty) == (cell.z, cell.tx, cell.ty) {
                    fragment_kinds.push(kind);
                }
                fragment_sink(z, tx, ty, geom, kind);
            },
        );
        drop(fragment_sink);

        let mut fill_acc = OceanAcc::new();
        let mut fill_kinds = Vec::new();
        let mut fill_sink = ocean_sink(1, Layer::Ocean as u8, &attrs, &mut fill_acc);
        let mut scratch = PyramidScratch::new();
        emit_shape_pyramid(
            &piece_one,
            &params,
            &mut scratch,
            &mut |z, tx, ty, geom, kind| {
                if (z, tx, ty) == (cell.z, cell.tx, cell.ty) {
                    fill_kinds.push(kind);
                }
                fill_sink(z, tx, ty, geom, kind);
            },
        );
        drop(fill_sink);

        assert!(fragment_kinds.contains(&PyramidEmitKind::Fragment));
        assert!(fill_kinds.contains(&PyramidEmitKind::FullFill));
        let id_for_tile = |acc: &OceanAcc| {
            acc.records.iter().find_map(|(key, offset, len)| {
                (sort::tile_id_from_key(*key) == tile_id)
                    .then(|| record_osm_id(&acc.payload[*offset..*offset + *len]))
            })
        };
        assert_eq!(id_for_tile(&fragment_acc), Some(0));
        assert_eq!(id_for_tile(&fill_acc), Some(FULL_FILL_FEATURE_ID));
    }

    #[test]
    fn full_fill_uses_canonical_id_and_fragment_keeps_piece_id() {
        let mut acc = OceanAcc::new();
        let attrs = Vec::new();
        let mut sink = ocean_sink(77, Layer::Ocean as u8, &attrs, &mut acc);
        sink(0, 0, 0, &[9, 0, 0], PyramidEmitKind::FullFill);
        sink(0, 0, 0, &[9, 0, 0], PyramidEmitKind::Fragment);
        drop(sink);
        let (_, first_start, first_len) = acc.records[0];
        assert_eq!(
            record_osm_id(&acc.payload[first_start..first_start + first_len]),
            FULL_FILL_FEATURE_ID
        );
        let (_, second_start, second_len) = acc.records[1];
        assert_eq!(
            record_osm_id(&acc.payload[second_start..second_start + second_len]),
            77
        );
        let mut sink = ocean_sink(88, Layer::Ocean as u8, &attrs, &mut acc);
        sink(0, 0, 0, &[9, 0, 0], PyramidEmitKind::FullFill);
        drop(sink);
        let (_, third_start, third_len) = acc.records[2];
        assert_eq!(
            &acc.payload[first_start..first_start + first_len],
            &acc.payload[third_start..third_start + third_len],
            "full fills from different pieces retain identical payloads"
        );
    }

    #[test]
    fn piece_zero_fragment_round_trips_as_explicit_some_zero_id() {
        let mut acc = OceanAcc::new();
        // The decode side expects the encoded attrs-table form, exactly as
        // process_ocean_shapefile builds it - a bare empty Vec parses as a
        // truncated record and add_feature_to_layer would emit nothing.
        let mut attrs = Vec::new();
        encode_attrs_bytes(&mut attrs, &[], 14);
        let mut sink = ocean_sink(0, Layer::Ocean as u8, &attrs, &mut acc);
        sink(0, 0, 0, &[9, 0, 0], PyramidEmitKind::Fragment);
        drop(sink);
        let (_, start, len) = acc.records[0];
        let data = &acc.payload[start..start + len];
        let mut layer = crate::mvt::LayerBuilder::new("ocean");
        let mut geom_pool = Vec::new();
        let mut tags_pool = Vec::new();
        crate::wire_format::add_feature_to_layer(&mut layer, data, &mut geom_pool, &mut tags_pool);
        assert_eq!(layer.features()[0].id, Some(0));

        let mut encoded = Vec::new();
        let mut scratch = crate::mvt::EncodeScratch::new();
        crate::mvt::encode_tile_into(&mut encoded, &[&layer], &mut scratch);
        // Feature id 0 is present as the explicit varint field form, not
        // elided as a protobuf default.
        assert!(encoded.windows(2).any(|bytes| bytes == [8, 0]));
    }

    fn merc_square(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<Point> {
        vec![
            Point { x: min_x, y: min_y },
            Point { x: max_x, y: min_y },
            Point { x: max_x, y: max_y },
            Point { x: min_x, y: max_y },
        ]
    }

    #[test]
    fn union_pieces_merges_adjacent_cells_and_cancels_shared_edge() {
        use crate::geometry::int_ocean::signed_area_2x;
        // Two cells sharing the x=0.5 edge, as the pre-split source data
        // delivers them. Quantization puts the shared edge on bit-identical
        // coordinates, so the union must merge them into one seam-free shape
        // whose area is exactly the sum.
        let a = quantize_polygon(&merc_square(0.25, 0.25, 0.5, 0.5), &[], 7);
        let b = quantize_polygon(&merc_square(0.5, 0.25, 0.75, 0.5), &[], 7);
        let sum =
            signed_area_2x(&a[0]).get().unsigned_abs() + signed_area_2x(&b[0]).get().unsigned_abs();
        let merged = union_pieces(vec![a, b]);
        assert_eq!(merged.len(), 1, "adjacent cells must merge into one shape");
        assert_eq!(merged[0].len(), 1, "no holes expected");
        assert_eq!(signed_area_2x(&merged[0][0]).get().unsigned_abs(), sum);
    }

    #[test]
    fn union_of_split_pieces_restores_unsplit_pyramid_emission() {
        // The half-peninsula defect, end to end through pyramid emission. A
        // bump straddles the split edge x=0.5, sized so each half is ~200
        // z0-px^2 (under ocean_min_area's 256) while the whole is ~400 (over):
        // per-piece descent drops both halves at z0, the unioned pieces must
        // emit exactly what the unsplit shape emits at every zoom.
        let maxz = 2_u8;
        let w = 20.0 / 4096.0; // 20 z0 pixels in merc units
        let half = w / 2.0;
        let p = |x: f64, y: f64| Point { x, y };
        let s_unsplit = quantize_polygon(
            &[
                p(0.25, 0.25),
                p(0.5 - half, 0.25),
                p(0.5 - half, 0.25 - w),
                p(0.5 + half, 0.25 - w),
                p(0.5 + half, 0.25),
                p(0.75, 0.25),
                p(0.75, 0.5),
                p(0.25, 0.5),
            ],
            &[],
            maxz,
        );
        let piece_a = quantize_polygon(
            &[
                p(0.25, 0.25),
                p(0.5 - half, 0.25),
                p(0.5 - half, 0.25 - w),
                p(0.5, 0.25 - w),
                p(0.5, 0.5),
                p(0.25, 0.5),
            ],
            &[],
            maxz,
        );
        let piece_b = quantize_polygon(
            &[
                p(0.5, 0.25 - w),
                p(0.5 + half, 0.25 - w),
                p(0.5 + half, 0.25),
                p(0.75, 0.25),
                p(0.75, 0.5),
                p(0.5, 0.5),
            ],
            &[],
            maxz,
        );

        type Emitted = Vec<(u8, u32, u32, Vec<u32>)>;
        let emit_shapes = |shapes: &[Shape]| -> Emitted {
            let params = ocean_params(0, maxz, None);
            let mut out: Emitted = Vec::new();
            let mut sink = |z: u8, tx: u32, ty: u32, geom: &[u32], _kind: PyramidEmitKind| {
                out.push((z, tx, ty, geom.to_vec()));
            };
            let mut scratch = PyramidScratch::new();
            for shape in shapes {
                emit_shape_pyramid(shape, &params, &mut scratch, &mut sink);
            }
            out
        };

        let reference = emit_shapes(&[s_unsplit]);
        let per_piece = emit_shapes(&[piece_a.clone(), piece_b.clone()]);
        let unioned = emit_shapes(&union_pieces(vec![piece_a, piece_b]));

        // Calibration, both directions: per-piece emission must actually
        // exhibit the defect at z0 (bump halves dropped), or this test proves
        // nothing about the fix.
        let z0 = |emitted: &Emitted| -> Vec<Vec<u32>> {
            emitted
                .iter()
                .filter(|(z, _, _, _)| *z == 0)
                .map(|(_, _, _, geom)| geom.clone())
                .collect()
        };
        assert_ne!(
            z0(&reference),
            z0(&per_piece),
            "per-piece z0 emission must differ from unsplit (the defect)"
        );
        assert_eq!(
            reference, unioned,
            "unioned pieces must emit exactly the unsplit shape's pyramid"
        );
    }

    #[test]
    fn union_pieces_partial_shared_edge_with_extra_segmentation() {
        use crate::geometry::int_ocean::signed_area_2x;
        // B shares only part of A's right edge and carries an extra collinear
        // vertex on the shared segment - the split source's real shape: cell
        // edges meet with differing vertex segmentation and partial extent.
        let a = quantize_polygon(&merc_square(0.25, 0.25, 0.5, 0.5), &[], 2);
        let b = quantize_polygon(
            &[
                Point { x: 0.5, y: 0.3 },
                Point { x: 0.75, y: 0.3 },
                Point { x: 0.75, y: 0.45 },
                Point { x: 0.5, y: 0.45 },
                Point { x: 0.5, y: 0.375 },
            ],
            &[],
            2,
        );
        let sum =
            signed_area_2x(&a[0]).get().unsigned_abs() + signed_area_2x(&b[0]).get().unsigned_abs();
        let merged = union_pieces(vec![a, b]);
        assert_eq!(merged.len(), 1, "partial-edge neighbours must merge");
        assert_eq!(merged[0].len(), 1);
        assert_eq!(signed_area_2x(&merged[0][0]).get().unsigned_abs(), sum);
    }

    #[test]
    fn union_pieces_reassembles_island_split_across_cells() {
        // An island straddling the cell edge arrives as boundary notches in
        // both cells' outer rings (a clipped hole touching the clip edge is a
        // notch, not a hole). The union must close it back into a real hole.
        let a = quantize_polygon(
            &[
                Point { x: 0.25, y: 0.25 },
                Point { x: 0.5, y: 0.25 },
                Point { x: 0.5, y: 0.35 },
                Point { x: 0.45, y: 0.35 },
                Point { x: 0.45, y: 0.4 },
                Point { x: 0.5, y: 0.4 },
                Point { x: 0.5, y: 0.5 },
                Point { x: 0.25, y: 0.5 },
            ],
            &[],
            2,
        );
        let b = quantize_polygon(
            &[
                Point { x: 0.5, y: 0.25 },
                Point { x: 0.75, y: 0.25 },
                Point { x: 0.75, y: 0.5 },
                Point { x: 0.5, y: 0.5 },
                Point { x: 0.5, y: 0.4 },
                Point { x: 0.55, y: 0.4 },
                Point { x: 0.55, y: 0.35 },
                Point { x: 0.5, y: 0.35 },
            ],
            &[],
            2,
        );
        let merged = union_pieces(vec![a, b]);
        assert_eq!(merged.len(), 1, "the two cells must merge into one shape");
        assert_eq!(
            merged[0].len(),
            2,
            "the split island must come back as a hole"
        );
    }

    #[test]
    fn union_pieces_keeps_holes_and_disjoint_pieces() {
        let holed = quantize_polygon(
            &merc_square(0.1, 0.1, 0.4, 0.4),
            &[merc_square(0.2, 0.2, 0.3, 0.3)],
            7,
        );
        assert_eq!(holed.len(), 2);
        let solo = quantize_polygon(&merc_square(0.6, 0.6, 0.7, 0.7), &[], 7);
        let merged = union_pieces(vec![holed, solo]);
        assert_eq!(merged.len(), 2, "disjoint pieces must stay separate");
        let mut ring_counts: Vec<usize> = merged.iter().map(Vec::len).collect();
        ring_counts.sort_unstable();
        assert_eq!(ring_counts, vec![1, 2], "the hole must survive the union");
    }

    #[test]
    fn ocean_artifact_wire_order_and_nulls_stay_flat() {
        let key = OceanArtifactKey {
            source: OceanSourceKey {
                full_shp_xxh128: Xxh128::from_u128(1),
                full_shx_xxh128: Xxh128::from_u128(2),
                simplified_shp_xxh128: None,
                simplified_shx_xxh128: None,
                min_zoom: 0,
                max_zoom: 14,
                policy_version: OCEAN_POLICY_VERSION,
            },
            compression_level: 6,
        };
        let expected = concat!(
            r#"{"full_shp_xxh128":"00000000000000000000000000000001","#,
            r#""full_shx_xxh128":"00000000000000000000000000000002","#,
            r#""simplified_shp_xxh128":null,"simplified_shx_xxh128":null,"#,
            r#""min_zoom":0,"max_zoom":14,"compression_level":6,"policy_version":POLICY}"#,
        )
        .replace("POLICY", &OCEAN_POLICY_VERSION.to_string());
        assert_eq!(key.json(), expected);
    }

    #[test]
    fn ocean_artifact_hashes_require_32_lowercase_hex_characters() {
        let valid = serde_json::json!({
            "full_shp_xxh128": "0123456789abcdef0123456789abcdef",
            "full_shx_xxh128": "fedcba9876543210fedcba9876543210",
            "simplified_shp_xxh128": null,
            "simplified_shx_xxh128": null,
            "min_zoom": 0,
            "max_zoom": 14,
            "compression_level": 6,
            "policy_version": OCEAN_POLICY_VERSION,
        });
        OceanArtifactKey::from_json(&valid).expect("valid artifact key");
        for invalid in [
            "0123456789abcdef0123456789abcde",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdef0",
        ] {
            let mut value = valid.clone();
            value["full_shp_xxh128"] = serde_json::Value::String(invalid.to_string());
            assert!(OceanArtifactKey::from_json(&value).is_err());
        }
    }

    #[test]
    fn ocean_keys_require_explicit_nulls_for_absent_inputs() {
        let source = OceanSourceKey {
            full_shp_xxh128: Xxh128::from_u128(1),
            full_shx_xxh128: Xxh128::from_u128(2),
            simplified_shp_xxh128: None,
            simplified_shx_xxh128: None,
            min_zoom: 0,
            max_zoom: 14,
            policy_version: OCEAN_POLICY_VERSION,
        };
        let artifact = OceanArtifactKey {
            source: source.clone(),
            compression_level: 6,
        };
        for field in ["simplified_shp_xxh128", "simplified_shx_xxh128"] {
            let mut value = source.to_json();
            OceanSourceKey::from_json(&value).expect("explicit nulls are valid");
            value.as_object_mut().expect("object").remove(field);
            assert!(OceanSourceKey::from_json(&value).is_err());

            let mut value = artifact.to_json();
            let _: OceanArtifactKey = serde_json::from_value(value.clone())
                .expect("explicit nulls are valid through serde");
            value.as_object_mut().expect("object").remove(field);
            assert!(serde_json::from_value::<OceanArtifactKey>(value).is_err());
        }
    }

    #[test]
    fn ocean_artifact_rejects_runs_reaching_z15() {
        let dir = tempfile::tempdir().expect("create artifact directory");
        let path = dir.path().join("ocean.pmtiles");
        let key = OceanArtifactKey {
            source: OceanSourceKey::from_hashes(
                OceanShapefileHashes {
                    shp_xxh128: Xxh128::from_u128(1),
                    shx_xxh128: Xxh128::from_u128(2),
                },
                None,
                0,
                14,
            ),
            compression_level: 6,
        };
        let world = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let first_z15 = crate::tiling::TileId::at(15, 0, 0).raw();
        for (start, length, accepted) in [
            (first_z15 - 1, 1, true),
            (first_z15, 1, false),
            (first_z15 - 1, 2, false),
        ] {
            let mut writer = pmtiles_writer::PmtilesWriter::new(pmtiles_writer::PmtilesConfig {
                min_zoom: 0,
                max_zoom: 14,
                bounds: (-180.0, -85.05, 180.0, 85.05),
                center: (0.0, 0.0, 2),
            });
            writer.set_ocean_only_metadata();
            writer
                .set_ocean_artifact_metadata(key.clone())
                .expect("set artifact metadata");
            // Admission reads directory ranges and metadata, not tile payloads.
            writer
                .add_run(crate::tiling::TileId::new(start).unwrap(), length, &[0])
                .expect("add directory run");
            writer.write_to(&path).expect("write artifact");
            let result = OceanTiles::open(&path, &key, &world, &[14]);
            if accepted {
                assert!(result.is_ok(), "last z14 tile must be admitted");
            } else {
                let error = result.err().expect("z15 run must be refused");
                assert!(error.to_string().contains("outside z0-14 tile space"));
            }
        }
    }

    #[test]
    fn ocean_band_tile_world_has_no_band_at_edges_or_corners() {
        let grid = OceanPassGrid::for_bounds(
            &MercBbox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 1.0,
                max_y: 1.0,
            },
            14,
        );
        for z in [0, 7, 8, 14] {
            let edge = (1_u32 << z) - 1;
            for (x, y) in [(0, 0), (edge, 0), (0, edge), (edge, edge)] {
                assert!(!ocean_band_tile(z, x, y, &grid));
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
    fn two_pass_grid_selection_keeps_z14_on_the_full_resolution_grid() {
        let low = OceanPassGrid::for_bounds(
            &MercBbox {
                min_x: 0.4,
                min_y: 0.4,
                max_x: 0.6,
                max_y: 0.6,
            },
            7,
        );
        let high = OceanPassGrid::for_bounds(
            &MercBbox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 1.0,
                max_y: 1.0,
            },
            14,
        );
        let grids = [low, high];
        assert_eq!(selected_pass_grid(&grids, 7).expect("z7 grid").max_zoom, 7);
        assert_eq!(
            selected_pass_grid(&grids, 14).expect("z14 grid").max_zoom,
            14
        );
        // This z14 interior tile is outside the low-resolution bounds, but
        // belongs to the full-world grid and must not be a computed band tile.
        assert!(ocean_band_tile(14, 2_000, 2_000, &grids[0]));
        assert!(!ocean_band_tile(
            14,
            2_000,
            2_000,
            selected_pass_grid(&grids, 14).expect("z14 grid")
        ));
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

    fn write_u32_be(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
    }

    fn write_u32_le(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn write_f64_le(buf: &mut [u8], off: usize, v: f64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn write_single_record_polygon_shapefile(path: &Path, parts: &[Vec<(f64, f64)>]) {
        let num_parts = parts.len();
        let num_points: usize = parts.iter().map(std::vec::Vec::len).sum();
        assert!(num_parts > 0);
        assert!(num_points > 0);

        let mut xmin = f64::INFINITY;
        let mut ymin = f64::INFINITY;
        let mut xmax = f64::NEG_INFINITY;
        let mut ymax = f64::NEG_INFINITY;
        for ring in parts {
            for &(x, y) in ring {
                xmin = xmin.min(x);
                ymin = ymin.min(y);
                xmax = xmax.max(x);
                ymax = ymax.max(y);
            }
        }

        // Record content length (bytes):
        // shape_type(4) + bbox(32) + num_parts(4) + num_points(4) + parts(num_parts*4) + points(num_points*16)
        let record_content_bytes = 4 + 32 + 4 + 4 + num_parts * 4 + num_points * 16;
        let record_content_words =
            u32::try_from(record_content_bytes / 2).expect("record content length fits u32");
        let shp_file_len_words =
            u32::try_from((100 + 8 + record_content_bytes) / 2).expect("shp length fits u32");
        let shx_file_len_words = ((100 + 8) / 2) as u32;

        let mut shp = vec![0u8; 100 + 8 + record_content_bytes];
        let mut shx = vec![0u8; 108];

        // Common 100-byte file header (shp + shx)
        // Big-endian section
        write_u32_be(&mut shp, 0, 9994);
        write_u32_be(&mut shp, 24, shp_file_len_words);
        write_u32_be(&mut shx, 0, 9994);
        write_u32_be(&mut shx, 24, shx_file_len_words);
        // Little-endian section
        write_u32_le(&mut shp, 28, 1000); // version
        write_u32_le(&mut shp, 32, 5); // Polygon
        write_u32_le(&mut shx, 28, 1000);
        write_u32_le(&mut shx, 32, 5);
        // Header bbox
        write_f64_le(&mut shp, 36, xmin);
        write_f64_le(&mut shp, 44, ymin);
        write_f64_le(&mut shp, 52, xmax);
        write_f64_le(&mut shp, 60, ymax);
        write_f64_le(&mut shx, 36, xmin);
        write_f64_le(&mut shx, 44, ymin);
        write_f64_le(&mut shx, 52, xmax);
        write_f64_le(&mut shx, 60, ymax);

        // shx index record
        write_u32_be(&mut shx, 100, 50); // .shp record offset in 16-bit words (100 bytes)
        write_u32_be(&mut shx, 104, record_content_words);

        // shp record header
        let rec_header = 100;
        write_u32_be(&mut shp, rec_header, 1); // record number
        write_u32_be(&mut shp, rec_header + 4, record_content_words);

        // shp record content
        let rec = rec_header + 8;
        write_u32_le(&mut shp, rec, 5); // Polygon
        write_f64_le(&mut shp, rec + 4, xmin); // xmin
        write_f64_le(&mut shp, rec + 12, ymin); // ymin
        write_f64_le(&mut shp, rec + 20, xmax); // xmax
        write_f64_le(&mut shp, rec + 28, ymax); // ymax
        write_u32_le(
            &mut shp,
            rec + 36,
            u32::try_from(num_parts).expect("part count fits u32"),
        ); // num_parts
        write_u32_le(
            &mut shp,
            rec + 40,
            u32::try_from(num_points).expect("point count fits u32"),
        ); // num_points
        let mut part_start = 0usize;
        for (i, ring) in parts.iter().enumerate() {
            write_u32_le(
                &mut shp,
                rec + 44 + i * 4,
                u32::try_from(part_start).expect("part start fits u32"),
            );
            part_start += ring.len();
        }

        let mut p = rec + 44 + num_parts * 4;
        for ring in parts {
            for &(x, y) in ring {
                write_f64_le(&mut shp, p, x);
                write_f64_le(&mut shp, p + 8, y);
                p += 16;
            }
        }

        fs::write(path, shp).unwrap();
        fs::write(path.with_extension("shx"), shx).unwrap();
    }

    fn write_test_polygon_shapefile(path: &Path) {
        // One world-sized square in EPSG:3857 meters.
        let half_c = 20_037_508.343;
        let outer = vec![
            (-half_c, -half_c),
            (half_c, -half_c),
            (half_c, half_c),
            (-half_c, half_c),
            (-half_c, -half_c),
        ];
        write_single_record_polygon_shapefile(path, &[outer]);
    }

    fn write_test_polygon_with_hole_shapefile(path: &Path) {
        let half_c = 20_037_508.343;
        let outer = vec![
            (-half_c, -half_c),
            (half_c, -half_c),
            (half_c, half_c),
            (-half_c, half_c),
            (-half_c, -half_c),
        ];
        // Clockwise inner ring so parser classifies it as a hole.
        let hole = vec![
            (-half_c * 0.5, -half_c * 0.5),
            (-half_c * 0.5, half_c * 0.5),
            (half_c * 0.5, half_c * 0.5),
            (half_c * 0.5, -half_c * 0.5),
            (-half_c * 0.5, -half_c * 0.5),
        ];
        write_single_record_polygon_shapefile(path, &[outer, hole]);
    }

    // -----------------------------------------------------------------------
    // point_in_polygon tests
    // -----------------------------------------------------------------------

    fn unit_square() -> Vec<Point> {
        vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 1.0, y: 0.0 },
            Point { x: 1.0, y: 1.0 },
            Point { x: 0.0, y: 1.0 },
        ]
    }

    #[test]
    fn pip_inside_square() {
        assert!(geometry::polygon::point_in_polygon(
            &Point::new(0.5, 0.5),
            &unit_square()
        ));
    }

    #[test]
    fn pip_outside_square() {
        assert!(!geometry::polygon::point_in_polygon(
            &Point::new(2.0, 0.5),
            &unit_square()
        ));
    }

    #[test]
    fn pip_on_edge() {
        // Edge behavior is implementation-defined for ray-casting;
        // just verify it does not panic.
        let _ = geometry::polygon::point_in_polygon(&Point::new(0.5, 0.0), &unit_square());
    }

    #[test]
    fn pip_inside_triangle() {
        let tri = vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 4.0, y: 0.0 },
            Point { x: 2.0, y: 3.0 },
        ];
        assert!(geometry::polygon::point_in_polygon(
            &Point::new(2.0, 1.0),
            &tri
        ));
    }

    #[test]
    fn pip_outside_triangle() {
        let tri = vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 4.0, y: 0.0 },
            Point { x: 2.0, y: 3.0 },
        ];
        assert!(!geometry::polygon::point_in_polygon(
            &Point::new(0.0, 3.0),
            &tri
        ));
    }

    #[test]
    fn pip_degenerate() {
        // Fewer than 3 points should return false
        assert!(!geometry::polygon::point_in_polygon(
            &Point::new(0.0, 0.0),
            &[]
        ));
        assert!(!geometry::polygon::point_in_polygon(
            &Point::new(0.0, 0.0),
            &[Point { x: 0.0, y: 0.0 }]
        ));
        assert!(!geometry::polygon::point_in_polygon(
            &Point::new(0.0, 0.0),
            &[Point { x: 0.0, y: 0.0 }, Point { x: 1.0, y: 1.0 }],
        ));
    }

    #[test]
    fn pip_concave() {
        // An L-shaped (concave) polygon:
        //
        //   (0,0)---(2,0)
        //     |        |
        //   (0,2)---(1,2)
        //            |
        //   (0,3)---(1,3)   <-- not connected; full shape below
        //
        // Vertices (counter-clockwise on a math-y axis):
        //   (0,0) (2,0) (2,1) (1,1) (1,3) (0,3)
        let l_shape = vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 2.0, y: 0.0 },
            Point { x: 2.0, y: 1.0 },
            Point { x: 1.0, y: 1.0 },
            Point { x: 1.0, y: 3.0 },
            Point { x: 0.0, y: 3.0 },
        ];

        // Inside the body of the L
        assert!(geometry::polygon::point_in_polygon(
            &Point::new(0.5, 0.5),
            &l_shape
        ));
        assert!(geometry::polygon::point_in_polygon(
            &Point::new(0.5, 2.0),
            &l_shape
        ));

        // Inside the concavity (the cut-out region) - should be false
        assert!(!geometry::polygon::point_in_polygon(
            &Point::new(1.5, 2.0),
            &l_shape
        ));
    }

    #[test]
    fn process_ocean_shapefile_emits_features_when_bbox_overlaps() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_test.shp");
        write_test_polygon_shapefile(&shp_path);

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };

        let emitted =
            process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None).unwrap();

        assert!(emitted > 0, "expected ocean features to be emitted");

        let mut reader = sort_writer.finish().unwrap();
        let mut count = 0u64;
        for partition in reader.take_partitions() {
            let mut pr =
                sort::SortPartitionReader::open(&partition, sort::ChunkCompression::None).unwrap();
            while pr.next().unwrap().is_some() {
                count += 1;
            }
        }
        assert_eq!(count, emitted, "emitted count should match sorted records");
    }

    #[test]
    fn process_ocean_shapefile_emits_nothing_when_bbox_disjoint() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_test_disjoint.shp");
        write_test_polygon_shapefile(&shp_path);

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        // Mercator data bounds outside [0,1] should not intersect any valid projected shape.
        let disjoint_bounds = MercBbox {
            min_x: 2.0,
            min_y: 2.0,
            max_x: 3.0,
            max_y: 3.0,
        };

        let emitted =
            process_ocean_shapefile(&shp_path, &disjoint_bounds, 0, 0, &mut sort_writer, None)
                .unwrap();

        assert_eq!(emitted, 0, "expected no ocean features for disjoint bounds");
    }

    #[test]
    fn process_ocean_shapefile_handles_polygon_with_hole() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_test_hole.shp");
        write_test_polygon_with_hole_shapefile(&shp_path);

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };

        let emitted =
            process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None).unwrap();
        assert!(emitted > 0, "expected ocean features to be emitted");

        let mut reader = sort_writer.finish().unwrap();
        let partition = reader
            .take_partitions()
            .into_iter()
            .next()
            .expect("expected at least one partition");
        let rec = sort::SortPartitionReader::open(&partition, sort::ChunkCompression::None)
            .unwrap()
            .next()
            .unwrap()
            .expect("expected at least one record");
        let cmd_count = u16::from_le_bytes(rec.data[9..11].try_into().unwrap());
        assert!(
            cmd_count >= 6,
            "polygon with hole should encode multiple rings (cmd_count={cmd_count})"
        );
    }

    #[test]
    fn process_ocean_shapefile_rejects_short_shx_header() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_bad_shx.shp");
        write_test_polygon_shapefile(&shp_path);
        fs::write(shp_path.with_extension("shx"), [0u8; 64]).unwrap();

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let err = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None)
            .expect_err("short .shx header should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("invalid .shx file"));
    }

    #[test]
    fn process_ocean_shapefile_errors_when_shx_missing() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_missing_shx.shp");
        write_test_polygon_shapefile(&shp_path);
        fs::remove_file(shp_path.with_extension("shx")).unwrap();

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let err = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None)
            .expect_err("missing .shx should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn process_ocean_shapefile_rejects_truncated_shp_record() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_truncated_shp.shp");
        write_test_polygon_shapefile(&shp_path);

        let mut shp_data = fs::read(&shp_path).unwrap();
        shp_data.truncate(120);
        fs::write(&shp_path, shp_data).unwrap();

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let err = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None)
            .expect_err("truncated record should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("extends past the .shp file"));
    }

    #[test]
    fn process_ocean_shapefile_rejects_negative_shx_offset() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_negative_offset.shp");
        write_test_polygon_shapefile(&shp_path);
        let mut shx = fs::read(shp_path.with_extension("shx")).unwrap();
        write_u32_be(&mut shx, 100, u32::MAX);
        fs::write(shp_path.with_extension("shx"), shx).unwrap();

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let err = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None)
            .expect_err("negative .shx offset should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("negative offset"));
    }

    #[test]
    fn process_ocean_shapefile_rejects_bad_part_index() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_bad_part_index.shp");
        write_test_polygon_shapefile(&shp_path);
        let mut shp = fs::read(&shp_path).unwrap();
        write_u32_le(&mut shp, 100 + 8 + 44, 6);
        fs::write(&shp_path, shp).unwrap();

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let err = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None)
            .expect_err("out-of-range part index should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("part indices"));
    }

    #[test]
    fn process_ocean_shapefile_hashes_the_bytes_it_reads() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("ocean_hash_input.shp");
        write_test_polygon_shapefile(&shp_path);

        let mut sort_writer =
            SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
        let bounds = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let (_, hashes) =
            process_ocean_shapefile_with_hashes(&shp_path, &bounds, 0, 0, &mut sort_writer)
                .unwrap();

        assert_eq!(hashes.shp_xxh128, Xxh128::hash_file(&shp_path).unwrap().0);
        assert_eq!(
            hashes.shx_xxh128,
            Xxh128::hash_file(&shp_path.with_extension("shx"))
                .unwrap()
                .0
        );
    }

    #[test]
    fn process_ocean_shapefile_rejects_malformed_polygon_geometry() {
        for defect in ["type", "bounds", "coordinate", "short_ring", "closure"] {
            let dir = tempfile::tempdir().unwrap();
            let shp_path = dir.path().join("malformed.shp");
            write_test_polygon_shapefile(&shp_path);
            let mut shp = fs::read(&shp_path).unwrap();
            let rec = 108;
            let points = rec + 48;
            match defect {
                "type" => write_u32_le(&mut shp, rec, 3),
                "bounds" => write_f64_le(&mut shp, rec + 4, f64::NAN),
                "coordinate" => write_f64_le(&mut shp, points + 16, f64::INFINITY),
                "short_ring" => write_u32_le(&mut shp, rec + 40, 3),
                "closure" => write_f64_le(&mut shp, points + 4 * 16, 0.0),
                _ => unreachable!(),
            }
            fs::write(&shp_path, shp).unwrap();
            let mut sort_writer =
                SortWriter::new(dir.path(), 1 << 20, sort::ChunkCompression::None).unwrap();
            let bounds = MercBbox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 1.0,
                max_y: 1.0,
            };
            let err = process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None)
                .expect_err(defect);
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{defect}");
        }
    }
}
