// Ocean shapefile processing.
//
// Reads a water-polygons-split-3857 shapefile (mmap + .shx index), filters to
// data bounds, parses polygons, and processes them in parallel with rayon.
// Emits tiles through the shared int_ocean pyramid engine: quantize-early to
// max-zoom pixel space, then per-tile integer boolean clipping. No scanline
// fill, no point-in-polygon, no Sutherland-Hodgman, no LandMask.

use crate::geometry::int_ocean::{
    IntEmitScratch, IntRect, OCEAN_DP_TOL_PX, Shape, Shapes, intersect_rect_into, quantize_polygon,
    shape_bbox,
};
use crate::geometry::pyramid::{
    PyramidCell, PyramidEmitKind, PyramidParams, PyramidScratch, Simplifier, emit_shape_pyramid,
    emit_shape_pyramid_cell, split_for_parallel,
};
use crate::geometry::{self, MercBbox, Point};
use crate::mvt::GeomType;
use crate::pmtiles_reader::{ArchiveView, RawDirEntry};
use crate::pmtiles_writer;
use crate::shortbread::Layer;
use crate::sort::{self, SortWriter};
use crate::wire_format::{append_feature_data_with_attrs, encode_attrs_bytes};
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
pub const OCEAN_POLICY_VERSION: u32 = 2;

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OceanSourceKey {
    pub full_shp_xxh128: u128,
    pub full_shx_xxh128: u128,
    pub simplified_shp_xxh128: Option<u128>,
    pub simplified_shx_xxh128: Option<u128>,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub policy_version: u32,
}

impl OceanSourceKey {
    pub fn from_inputs(
        full_shp: &std::path::Path,
        simplified_shp: Option<&std::path::Path>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> std::io::Result<Self> {
        let simplified_shx = simplified_shp.map(|p| p.with_extension("shx"));
        Ok(Self {
            full_shp_xxh128: hash_file(full_shp)?,
            full_shx_xxh128: hash_file(&full_shp.with_extension("shx"))?,
            simplified_shp_xxh128: simplified_shp.map(hash_file).transpose()?,
            simplified_shx_xxh128: simplified_shx.as_deref().map(hash_file).transpose()?,
            min_zoom,
            max_zoom,
            policy_version: OCEAN_POLICY_VERSION,
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        fn hash(v: u128) -> serde_json::Value {
            serde_json::Value::String(format!("{v:032x}"))
        }
        fn optional(v: Option<u128>) -> serde_json::Value {
            v.map_or(serde_json::Value::Null, hash)
        }
        serde_json::json!({
            "full_shp_xxh128": hash(self.full_shp_xxh128),
            "full_shx_xxh128": hash(self.full_shx_xxh128),
            "simplified_shp_xxh128": optional(self.simplified_shp_xxh128),
            "simplified_shx_xxh128": optional(self.simplified_shx_xxh128),
            "min_zoom": self.min_zoom,
            "max_zoom": self.max_zoom,
            "policy_version": self.policy_version,
        })
    }

    pub fn from_json(value: &serde_json::Value) -> std::io::Result<Self> {
        let invalid =
            || std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid ocean source key");
        let parse = |field: &str| -> std::io::Result<u128> {
            u128::from_str_radix(
                value
                    .get(field)
                    .and_then(|v| v.as_str())
                    .ok_or_else(invalid)?,
                16,
            )
            .map_err(|_| invalid())
        };
        let parse_optional = |field: &str| -> std::io::Result<Option<u128>> {
            match value.get(field) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::String(s)) => {
                    u128::from_str_radix(s, 16).map(Some).map_err(|_| invalid())
                }
                Some(_) => Err(invalid()),
            }
        };
        let small = |field: &str| -> std::io::Result<u32> {
            u32::try_from(
                value
                    .get(field)
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(invalid)?,
            )
            .map_err(|_| invalid())
        };
        Ok(Self {
            full_shp_xxh128: parse("full_shp_xxh128")?,
            full_shx_xxh128: parse("full_shx_xxh128")?,
            simplified_shp_xxh128: parse_optional("simplified_shp_xxh128")?,
            simplified_shx_xxh128: parse_optional("simplified_shx_xxh128")?,
            min_zoom: u8::try_from(small("min_zoom")?).map_err(|_| invalid())?,
            max_zoom: u8::try_from(small("max_zoom")?).map_err(|_| invalid())?,
            policy_version: small("policy_version")?,
        })
    }
}

/// The durable artifact invalidation key, encoded under `ocean_artifact`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OceanArtifactKey {
    pub full_shp_xxh128: u128,
    pub full_shx_xxh128: u128,
    pub simplified_shp_xxh128: Option<u128>,
    pub simplified_shx_xxh128: Option<u128>,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub compression_level: u32,
    pub policy_version: u32,
}

impl OceanArtifactKey {
    pub fn from_inputs(
        full_shp: &std::path::Path,
        full_shx: &std::path::Path,
        simplified_shp: Option<&std::path::Path>,
        simplified_shx: Option<&std::path::Path>,
        min_zoom: u8,
        max_zoom: u8,
        compression_level: u32,
    ) -> std::io::Result<Self> {
        Ok(Self {
            full_shp_xxh128: hash_file(full_shp)?,
            full_shx_xxh128: hash_file(full_shx)?,
            simplified_shp_xxh128: simplified_shp.map(hash_file).transpose()?,
            simplified_shx_xxh128: simplified_shx.map(hash_file).transpose()?,
            min_zoom,
            max_zoom,
            compression_level,
            policy_version: OCEAN_POLICY_VERSION,
        })
    }

    pub fn json(&self) -> String {
        fn hash(v: u128) -> String {
            format!("{v:032x}")
        }
        fn optional(v: Option<u128>) -> String {
            v.map_or_else(|| "null".to_string(), |v| format!("\"{}\"", hash(v)))
        }
        format!(
            r#"{{"full_shp_xxh128":"{}","full_shx_xxh128":"{}","simplified_shp_xxh128":{},"simplified_shx_xxh128":{},"min_zoom":{},"max_zoom":{},"compression_level":{},"policy_version":{}}}"#,
            hash(self.full_shp_xxh128),
            hash(self.full_shx_xxh128),
            optional(self.simplified_shp_xxh128),
            optional(self.simplified_shx_xxh128),
            self.min_zoom,
            self.max_zoom,
            self.compression_level,
            self.policy_version,
        )
    }

    pub fn from_json(value: &serde_json::Value) -> std::io::Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| std::io::Error::other("ocean_artifact must be an object"))?;
        let parse_hash = |name: &str| -> std::io::Result<u128> {
            let value = object
                .get(name)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| std::io::Error::other(format!("ocean_artifact missing {name}")))?;
            if value.len() != 32
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(std::io::Error::other(format!(
                    "invalid ocean_artifact {name}"
                )));
            }
            u128::from_str_radix(value, 16)
                .map_err(|_| std::io::Error::other(format!("invalid ocean_artifact {name}")))
        };
        let parse_optional_hash = |name: &str| -> std::io::Result<Option<u128>> {
            match object.get(name) {
                Some(serde_json::Value::Null) => Ok(None),
                Some(value) => value
                    .as_str()
                    .ok_or_else(|| std::io::Error::other(format!("invalid ocean_artifact {name}")))
                    .and_then(|s| {
                        if s.len() != 32
                            || !s
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                        {
                            return Err(std::io::Error::other(format!(
                                "invalid ocean_artifact {name}"
                            )));
                        }
                        u128::from_str_radix(s, 16).map(Some).map_err(|_| {
                            std::io::Error::other(format!("invalid ocean_artifact {name}"))
                        })
                    }),
                None => Err(std::io::Error::other(format!(
                    "ocean_artifact missing {name}"
                ))),
            }
        };
        let number = |name: &str| -> std::io::Result<u64> {
            object
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| std::io::Error::other(format!("ocean_artifact missing {name}")))
        };
        Ok(Self {
            full_shp_xxh128: parse_hash("full_shp_xxh128")?,
            full_shx_xxh128: parse_hash("full_shx_xxh128")?,
            simplified_shp_xxh128: parse_optional_hash("simplified_shp_xxh128")?,
            simplified_shx_xxh128: parse_optional_hash("simplified_shx_xxh128")?,
            min_zoom: u8::try_from(number("min_zoom")?)
                .map_err(|_| std::io::Error::other("invalid ocean_artifact min_zoom"))?,
            max_zoom: u8::try_from(number("max_zoom")?)
                .map_err(|_| std::io::Error::other("invalid ocean_artifact max_zoom"))?,
            compression_level: u32::try_from(number("compression_level")?)
                .map_err(|_| std::io::Error::other("invalid ocean_artifact compression_level"))?,
            policy_version: u32::try_from(number("policy_version")?)
                .map_err(|_| std::io::Error::other("invalid ocean_artifact policy_version"))?,
        })
    }
}

fn hash_file(path: &std::path::Path) -> std::io::Result<u128> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.digest128())
}

// ---------------------------------------------------------------------------
// Ocean input stats
// ---------------------------------------------------------------------------
//
// process_ocean_shapefile runs up to twice per pipeline (simplified z0-7 plus
// full-resolution z8+), so its input-side counts accumulate into these
// process-global atomics and are flushed once at OCEAN_END by
// emit_ocean_counters. OceanAcc records its output at each spill flush because
// the phase12 sort snapshot predates the ocean pass. The output counters use
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
    zoom_records: [AtomicU64; 15],
    zoom_payload_bytes: [AtomicU64; 15],
}

static OCEAN_STATS: OceanStats = OceanStats {
    shapes: AtomicU64::new(0),
    shapes_hit: AtomicU64::new(0),
    pieces: AtomicU64::new(0),
    shapefile_bytes: AtomicU64::new(0),
    records: AtomicU64::new(0),
    payload_bytes: AtomicU64::new(0),
    zoom_records: [const { AtomicU64::new(0) }; 15],
    zoom_payload_bytes: [const { AtomicU64::new(0) }; 15],
};

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
        let scale = 1_i64 << (u32::from(max_zoom) + 12);
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
    let shift = u32::from(pass.max_zoom - z);
    let cell = 4096_i64 << shift;
    let buffer = 128_i64 << shift;
    let limit = 1_i64 << (u32::from(pass.max_zoom) + 12);
    let clamp = |v: i64| i32::try_from(v.clamp(0, limit)).expect("ocean base coordinate fits i32");
    let buffered = IntRect {
        min_x: clamp(i64::from(tx) * cell - buffer),
        min_y: clamp(i64::from(ty) * cell - buffer),
        max_x: clamp((i64::from(tx) + 1) * cell + buffer),
        max_y: clamp((i64::from(ty) + 1) * cell + buffer),
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
    // This deliberately follows pyramid::root_fragments rather than using the
    // raw shapefile bbox. Root selection snaps the quantized bbox outward to
    // z_top cells; a raw-bbox test can incorrectly discard a root touching the
    // computed band.
    let scale = 1_i64 << (u32::from(pass.max_zoom) + 12);
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
    let shift = u32::from(pass.max_zoom - z_top);
    let cell = 4096_i64 << shift;
    let max_tile = (1_u32 << z_top) - 1;
    let index = |v: i32| -> u32 {
        if v <= 0 {
            0
        } else {
            u32::try_from(i64::from(v).div_euclid(cell))
                .expect("ocean root tile fits u32")
                .min(max_tile)
        }
    };
    let tx0 = index(bbox.min_x);
    let tx1 = index(bbox.max_x);
    let ty0 = index(bbox.min_y);
    let ty1 = index(bbox.max_y);
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
        if view.tile_type() != 1 || view.tile_compression() != 2 {
            return Err(std::io::Error::other("ocean artifact must use MVT + gzip"));
        }
        if view.min_zoom() != 0 || view.max_zoom() != 14 {
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
            || layers[0].get("id").and_then(serde_json::Value::as_str) != Some("ocean")
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
        let z14_end = ((1_u64 << 30) - 1) / 3 + 1;
        if runs.iter().any(|run| {
            run.tile_id >= z14_end
                || run
                    .tile_id
                    .checked_add(u64::from(run.run_length))
                    .is_none_or(|end| end > z14_end)
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
    /// Select the one pass which owns a tile. A simplified z0-7 pass precedes
    /// the full-resolution pass when both shapefiles are configured.
    pub fn band_tile(&self, z: u8, tx: u32, ty: u32) -> bool {
        let grid = selected_pass_grid(&self.grids, z);
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

/// Flush accumulated ocean input counters to the sidecar. Called once at
/// OCEAN_END; a no-op when no shapefile was processed.
pub(crate) fn emit_ocean_counters() {
    use crate::debug::emit_counter_u64;
    let shapes = OCEAN_STATS.shapes.load(Ordering::Relaxed);
    if shapes == 0 {
        return;
    }
    emit_counter_u64("ocean_shapes", shapes);
    emit_counter_u64(
        "ocean_shapes_hit",
        OCEAN_STATS.shapes_hit.load(Ordering::Relaxed),
    );
    emit_counter_u64("ocean_pieces", OCEAN_STATS.pieces.load(Ordering::Relaxed));
    emit_counter_u64(
        "ocean_shapefile_bytes",
        OCEAN_STATS.shapefile_bytes.load(Ordering::Relaxed),
    );

    let records = OCEAN_STATS.records.load(Ordering::Relaxed);
    if records == 0 {
        return;
    }
    emit_counter_u64("sort_layer_ocean_records", records);
    emit_counter_u64(
        "sort_layer_ocean_bytes",
        OCEAN_STATS.payload_bytes.load(Ordering::Relaxed),
    );
    if !ocean_layer_stats_enabled() {
        return;
    }
    for z in 0..15 {
        let records = OCEAN_STATS.zoom_records[z].load(Ordering::Relaxed);
        let bytes = OCEAN_STATS.zoom_payload_bytes[z].load(Ordering::Relaxed);
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
    source_pieces: usize,
    shapes_hit: u64,
}

#[derive(Clone, Copy)]
struct ShxRecord {
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
                let zoom = sort::zoom_from_tile_id(sort::tile_id_from_key(key)) as usize;
                if zoom < 15 {
                    OCEAN_STATS.zoom_records[zoom].fetch_add(1, Ordering::Relaxed);
                    OCEAN_STATS.zoom_payload_bytes[zoom].fetch_add(len as u64, Ordering::Relaxed);
                }
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

/// Read ocean shapefile with mmap + bbox-first filtering, then process in parallel.
///
/// Mmaps the .shp file and reads the .shx index to get record offsets.
/// For each record, reads only the 32-byte bbox from the mmap. Only shapes
/// intersecting data_bounds get their full geometry parsed.
/// Then all polygons are processed in parallel with rayon.
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
    if pass_grid.is_some_and(|grid| grid.band_is_empty(min_zoom, max_zoom)) {
        eprintln!("  Ocean band is empty; skipping shapefile geometry");
        return Ok(0);
    }
    eprintln!("  Opening {}", path.display());

    // --- Read .shx index to get record offsets ---
    let shx_path = path.with_extension("shx");
    let shx_data = std::fs::read(&shx_path)?;

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

    let shape_count = (shx_data.len() - 100) / 8;
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
            continue;
        }
        #[allow(clippy::cast_sign_loss)]
        let offset = (offset_words as usize) * 2;
        #[allow(clippy::cast_sign_loss)]
        let content_len = (content_words as usize) * 2;
        records.push(ShxRecord {
            offset,
            content_len,
        });
    }
    eprintln!("  Index: {shape_count} shapes");

    // --- Mmap the .shp file ---
    let shp_file = std::fs::File::open(path)?;
    let shp_mmap = unsafe { memmap2::Mmap::map(&shp_file) }?;
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
        shp_mmap: &shp_mmap,
        shp_file: &shp_file,
        data_bounds,
        max_zoom,
        data_rect,
        pass_grid,
        min_zoom,
    };
    let parsed_records: Vec<ParsedOceanRecord> = records
        .par_iter()
        .map_init(IntEmitScratch::new, |scratch, &record| {
            parse_ocean_record(record, &parse_ctx, scratch)
        })
        .collect();
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

    let poly_count = pieces.len();
    eprintln!(
        "  {shape_count} shapes, {shapes_hit} in bounds, {poly_count} polygons - processing in parallel"
    );
    OCEAN_STATS.shapes.fetch_add(
        u64::try_from(shape_count).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    OCEAN_STATS
        .shapes_hit
        .fetch_add(shapes_hit, Ordering::Relaxed);
    OCEAN_STATS.pieces.fetch_add(
        u64::try_from(poly_count).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    // --- Process phase: parallel with rayon, direct chunk flushing ---
    //
    // Each rayon worker accumulates records in a thread-local buffer and flushes
    // directly to a chunk file when the buffer exceeds chunk_size_bytes. This
    // avoids holding all ocean sort records in memory simultaneously - at planet
    // scale that could be 10-30 GB. The previous approach (par_iter().collect()
    // into Vec<Vec<SortRecord>> + serial push) was fine for regional extracts but
    // would blow memory and serialize sort+flush at planet scale.
    use std::sync::atomic::AtomicUsize;

    let ocean_layer = Layer::Ocean as u8;
    let mut empty_attrs_bytes = Vec::new();
    encode_attrs_bytes(&mut empty_attrs_bytes, &[], max_zoom);

    // Ocean chunks use the same chunk_NNNN.bin naming (starting after PBF chunks)
    // so that --skip-to sort (SortReader::from_dir sequential scan) finds them.
    let chunk_id = std::sync::Arc::new(AtomicUsize::new(sort_writer.chunk_count()));
    let spill = sort::SpillCoalescer::new(
        sort_writer.tmp_dir().to_path_buf(),
        std::sync::Arc::clone(&chunk_id),
        sort_writer.chunk_size_bytes(),
        sort_writer.compression(),
    );
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
    Ok(count)
}

struct OceanRecordParseCtx<'a> {
    shp_mmap: &'a memmap2::Mmap,
    shp_file: &'a std::fs::File,
    data_bounds: &'a MercBbox,
    max_zoom: u8,
    data_rect: IntRect,
    pass_grid: Option<&'a OceanPassGrid>,
    min_zoom: u8,
}

fn parse_ocean_record(
    record: ShxRecord,
    ctx: &OceanRecordParseCtx<'_>,
    scratch: &mut IntEmitScratch,
) -> ParsedOceanRecord {
    let mut out = ParsedOceanRecord {
        pieces: Vec::new(),
        source_pieces: 0,
        shapes_hit: 0,
    };

    let rec = record.offset + 8;
    if record.content_len < 44 || rec + 44 > ctx.shp_mmap.len() {
        return out;
    }

    let header = {
        let shp = &ctx.shp_mmap[..];
        let mut header = [0_u8; 44];
        header.copy_from_slice(&shp[rec..rec + 44]);
        header
    };

    let xmin = f64::from_le_bytes(header[4..12].try_into().expect("shapefile field read"));
    let ymin = f64::from_le_bytes(header[12..20].try_into().expect("shapefile field read"));
    let xmax = f64::from_le_bytes(header[20..28].try_into().expect("shapefile field read"));
    let ymax = f64::from_le_bytes(header[28..36].try_into().expect("shapefile field read"));

    let merc_min = geometry::from_epsg3857(xmin, ymax);
    let merc_max = geometry::from_epsg3857(xmax, ymin);

    if merc_max.x < ctx.data_bounds.min_x
        || merc_min.x > ctx.data_bounds.max_x
        || merc_max.y < ctx.data_bounds.min_y
        || merc_min.y > ctx.data_bounds.max_y
    {
        return out;
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
        return out;
    }

    out.shapes_hit = 1;

    let source = ShapeRecordSource {
        record,
        rec,
        shp_len: ctx.shp_mmap.len(),
        shp_file: ctx.shp_file,
        header: &header,
    };
    out.source_pieces += push_shape_record_pieces(
        &source,
        scratch,
        &mut out.pieces,
        ctx.max_zoom,
        ctx.data_rect,
    );

    out
}

struct ShapeRecordSource<'a> {
    record: ShxRecord,
    rec: usize,
    shp_len: usize,
    shp_file: &'a std::fs::File,
    header: &'a [u8; 44],
}

fn push_shape_record_pieces(
    source: &ShapeRecordSource<'_>,
    scratch: &mut IntEmitScratch,
    pieces: &mut Vec<Shape>,
    max_zoom: u8,
    data_rect: IntRect,
) -> usize {
    let offset = source.record.offset;
    let num_parts_i32 = i32::from_le_bytes(
        source.header[36..40]
            .try_into()
            .expect("shapefile field read"),
    );
    let num_points_i32 = i32::from_le_bytes(
        source.header[40..44]
            .try_into()
            .expect("shapefile field read"),
    );
    if num_parts_i32 < 0 || num_points_i32 < 0 {
        eprintln!("  Warning: negative part/point count at offset {offset}, skipping record");
        return 0;
    }
    #[allow(clippy::cast_sign_loss)]
    let num_parts = num_parts_i32 as usize;
    #[allow(clippy::cast_sign_loss)]
    let num_points = num_points_i32 as usize;

    let parts_start = 44;
    let points_start = parts_start + num_parts * 4;
    let record_end = points_start + num_points * 16;
    if record_end > source.record.content_len || source.rec + record_end > source.shp_len {
        eprintln!("  Warning: shape record at offset {offset} extends past end of file, skipping");
        return 0;
    }

    let Some(mut ring_starts) = read_ring_starts(source, num_parts) else {
        return 0;
    };
    if ring_starts.iter().any(|&v| v > num_points) {
        eprintln!("  Warning: invalid part index at offset {offset}, skipping record");
        return 0;
    }
    ring_starts.push(num_points);

    let mut source_pieces = 0;
    let mut current_outer: Option<Vec<Point>> = None;
    let mut current_inners: Vec<Vec<Point>> = Vec::new();

    for (w, window) in ring_starts.windows(2).enumerate() {
        let Some(ring) = read_ring_points(source, points_start, window[0], window[1]) else {
            return source_pieces;
        };
        let is_outer = w == 0 || geometry::signed_area(&ring) >= 0.0;

        if is_outer {
            if let Some(outer) = current_outer.take() {
                source_pieces += push_quantized_pieces(
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
        source_pieces += push_quantized_pieces(
            scratch,
            pieces,
            &outer,
            &current_inners,
            max_zoom,
            data_rect,
        );
    }

    source_pieces
}

fn read_ring_starts(source: &ShapeRecordSource<'_>, num_parts: usize) -> Option<Vec<usize>> {
    let mut parts = vec![0_u8; num_parts * 4];
    if !parts.is_empty() {
        let read_offset = u64::try_from(source.rec + 44).expect("shapefile offset fits u64");
        if let Err(err) = source.shp_file.read_exact_at(&mut parts, read_offset) {
            eprintln!(
                "  Warning: failed to read shape parts at offset {}: {err}",
                source.record.offset
            );
            return None;
        }
    }
    Some(
        (0..num_parts)
            .map(|j| {
                let b = j * 4;
                let v =
                    i32::from_le_bytes(parts[b..b + 4].try_into().expect("shapefile field read"));
                if v < 0 {
                    usize::MAX
                } else {
                    #[allow(clippy::cast_sign_loss)]
                    {
                        v as usize
                    }
                }
            })
            .collect(),
    )
}

fn read_ring_points(
    source: &ShapeRecordSource<'_>,
    points_start: usize,
    start: usize,
    end: usize,
) -> Option<Vec<Point>> {
    let point_count = end - start;
    let mut ring = Vec::with_capacity(point_count);
    let mut buf = vec![0_u8; RING_READ_BUFFER_BYTES];
    let mut points_read = 0usize;

    while points_read < point_count {
        let points_this_read = ((point_count - points_read) * 16).min(buf.len()) / 16;
        let byte_len = points_this_read * 16;
        let read_offset = u64::try_from(source.rec + points_start + (start + points_read) * 16)
            .expect("shapefile offset fits u64");
        if let Err(err) = source
            .shp_file
            .read_exact_at(&mut buf[..byte_len], read_offset)
        {
            eprintln!(
                "  Warning: failed to read shape points at offset {}: {err}",
                source.record.offset
            );
            return None;
        }
        for idx in 0..points_this_read {
            let b = idx * 16;
            let x = f64::from_le_bytes(buf[b..b + 8].try_into().expect("shapefile field read"));
            let y =
                f64::from_le_bytes(buf[b + 8..b + 16].try_into().expect("shapefile field read"));
            ring.push(geometry::from_epsg3857(x, y));
        }
        points_read += points_this_read;
    }
    Some(ring)
}

fn push_quantized_pieces(
    scratch: &mut IntEmitScratch,
    pieces: &mut Vec<Shape>,
    outer: &[Point],
    inners: &[Vec<Point>],
    max_zoom: u8,
    data_rect: IntRect,
) -> usize {
    let shape = quantize_polygon(outer, inners, max_zoom);
    if shape.is_empty() {
        return 0;
    }

    // The common case for an in-bounds extract: the shape lies entirely
    // inside the data bounds - the boolean is an expensive identity.
    if shape_bbox(&shape).is_some_and(|bb| rect_contains(data_rect, bb)) {
        pieces.push(shape);
        return 1;
    }

    let mut clipped = Vec::new();
    intersect_rect_into(scratch, &shape, data_rect, 0, &mut clipped);
    let source_pieces = clipped.len();
    for piece in clipped {
        pieces.push(piece);
    }
    source_pieces
}

/// True if `outer` contains `inner` (closed containment).
fn rect_contains(outer: IntRect, inner: IntRect) -> bool {
    outer.min_x <= inner.min_x
        && outer.min_y <= inner.min_y
        && outer.max_x >= inner.max_x
        && outer.max_y >= inner.max_y
}

fn data_bounds_rect(data_bounds: &MercBbox, max_zoom: u8) -> IntRect {
    let scale = 1_i64 << (u32::from(max_zoom) + 12);
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

fn ocean_dp_tol(_z: u8) -> i64 {
    OCEAN_DP_TOL_PX
}

fn ocean_min_area(_z: u8) -> u64 {
    256
}

fn ocean_sink<'a>(
    feature_id: u64,
    layer_idx: u8,
    attrs_bytes: &'a [u8],
    acc: &'a mut OceanAcc,
) -> impl FnMut(u8, u32, u32, &[u32], PyramidEmitKind) + 'a {
    move |z: u8, tx: u32, ty: u32, geom: &[u32], kind: PyramidEmitKind| {
        let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
        let key = sort::make_sort_key(tile_id, layer_idx, 0);
        let range = append_feature_data_with_attrs(
            &mut acc.payload,
            if kind == PyramidEmitKind::FullFill {
                0
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
    use crate::geometry::Point;
    use crate::sort::SortWriter;
    use std::fs;
    use std::path::Path;

    fn record_osm_id(data: &[u8]) -> u64 {
        // The sort record payload leads with the raw little-endian osm_id
        // (wire_format.rs layout), not a protobuf message.
        u64::from_le_bytes(data[0..8].try_into().expect("record osm_id"))
    }

    #[test]
    fn full_fill_uses_canonical_zero_id_and_fragment_keeps_piece_id() {
        let mut acc = OceanAcc::new();
        let attrs = Vec::new();
        let mut sink = ocean_sink(77, Layer::Ocean as u8, &attrs, &mut acc);
        sink(0, 0, 0, &[9, 0, 0], PyramidEmitKind::FullFill);
        sink(0, 0, 0, &[9, 0, 0], PyramidEmitKind::Fragment);
        drop(sink);
        let (_, first_start, first_len) = acc.records[0];
        assert_eq!(
            record_osm_id(&acc.payload[first_start..first_start + first_len]),
            0
        );
        let (_, second_start, second_len) = acc.records[1];
        assert_eq!(
            record_osm_id(&acc.payload[second_start..second_start + second_len]),
            77
        );
    }

    #[test]
    fn full_fill_round_trips_as_explicit_some_zero_id() {
        let mut acc = OceanAcc::new();
        // The decode side expects the encoded attrs-table form, exactly as
        // process_ocean_shapefile builds it - a bare empty Vec parses as a
        // truncated record and add_feature_to_layer would emit nothing.
        let mut attrs = Vec::new();
        encode_attrs_bytes(&mut attrs, &[], 14);
        let mut sink = ocean_sink(77, Layer::Ocean as u8, &attrs, &mut acc);
        sink(0, 0, 0, &[9, 0, 0], PyramidEmitKind::FullFill);
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
        assert!(geometry::point_in_polygon(
            &Point::new(0.5, 0.5),
            &unit_square()
        ));
    }

    #[test]
    fn pip_outside_square() {
        assert!(!geometry::point_in_polygon(
            &Point::new(2.0, 0.5),
            &unit_square()
        ));
    }

    #[test]
    fn pip_on_edge() {
        // Edge behavior is implementation-defined for ray-casting;
        // just verify it does not panic.
        let _ = geometry::point_in_polygon(&Point::new(0.5, 0.0), &unit_square());
    }

    #[test]
    fn pip_inside_triangle() {
        let tri = vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 4.0, y: 0.0 },
            Point { x: 2.0, y: 3.0 },
        ];
        assert!(geometry::point_in_polygon(&Point::new(2.0, 1.0), &tri));
    }

    #[test]
    fn pip_outside_triangle() {
        let tri = vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 4.0, y: 0.0 },
            Point { x: 2.0, y: 3.0 },
        ];
        assert!(!geometry::point_in_polygon(&Point::new(0.0, 3.0), &tri));
    }

    #[test]
    fn pip_degenerate() {
        // Fewer than 3 points should return false
        assert!(!geometry::point_in_polygon(&Point::new(0.0, 0.0), &[]));
        assert!(!geometry::point_in_polygon(
            &Point::new(0.0, 0.0),
            &[Point { x: 0.0, y: 0.0 }]
        ));
        assert!(!geometry::point_in_polygon(
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
        assert!(geometry::point_in_polygon(&Point::new(0.5, 0.5), &l_shape));
        assert!(geometry::point_in_polygon(&Point::new(0.5, 2.0), &l_shape));

        // Inside the concavity (the cut-out region) - should be false
        assert!(!geometry::point_in_polygon(&Point::new(1.5, 2.0), &l_shape));
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
        while reader.next().unwrap().is_some() {
            count += 1;
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
        let rec = reader
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
    fn process_ocean_shapefile_skips_truncated_shp_record() {
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
        let emitted =
            process_ocean_shapefile(&shp_path, &bounds, 0, 0, &mut sort_writer, None).unwrap();
        assert_eq!(emitted, 0, "truncated record should be skipped");
    }
}
