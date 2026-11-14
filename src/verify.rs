//! PMTiles output verification.
//!
//! Validates a PMTiles archive end-to-end: container integrity, metadata schema,
//! tile decompression, MVT payload structure, and layer coverage.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::Path;

use crate::archive::Archive;
use crate::mvt::decode::{
    self, Command, CommandKind, CommandSink, GeometryError, Grammar, Point, Words,
};
use crate::shortbread::{Layer, RankAttributes, RankError};
use crate::tiling::{ARCHIVE_MAX_ZOOM, TileId};

/// One slot per archive zoom.
pub const ARCHIVE_ZOOM_COUNT: usize = ARCHIVE_MAX_ZOOM as usize + 1;

/// Coordinates of an id inside a directory run. `read_all_runs` refuses runs
/// reaching past the last addressable tile, so every such id is an address.
fn tile_coord(tile_id: u64) -> (u8, u32, u32) {
    TileId::new(tile_id)
        .expect("checked directory runs hold addressable ids")
        .coord()
}
use crate::mvt::message::{self, ValueMessage};
use crate::tile_detail::Strictness;

/// Maximum number of tile-level errors before aborting traversal.
const MAX_TILE_ERRORS: usize = 100;
const MVT_EXTENT: i64 = crate::geometry::tiles::grid::EXTENT as i64;
const MVT_COORD_ABS_LIMIT: i64 = MVT_EXTENT * 32; // 131072
const MVT_DELTA_LIMIT: i64 = MVT_EXTENT * 16; // 65536
const MVT_DELTA_LIMIT_SEAM: i64 = MVT_EXTENT * 4; // 16384

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ValidationClass {
    z: u8,
    seam: bool,
    extent: i64,
}

impl ValidationClass {
    fn for_tile(z: u8, x: u32) -> Self {
        let max_x = (1_u32 << z).saturating_sub(1);
        Self {
            z,
            seam: x == 0 || x == max_x,
            extent: MVT_EXTENT,
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum VerifyError {
    Io(io::Error),
    Container(String),
    Metadata(String),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::Io(e) => write!(f, "I/O error: {e}"),
            VerifyError::Container(msg) => write!(f, "container error: {msg}"),
            VerifyError::Metadata(msg) => write!(f, "metadata error: {msg}"),
        }
    }
}

impl From<io::Error> for VerifyError {
    fn from(e: io::Error) -> Self {
        VerifyError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

pub struct VerifyReport {
    pub tiles_checked: u64,
    /// Number of distinct stored payloads in --unique-payloads mode.
    pub unique_blobs: u64,
    /// Number of PMTiles directory tile runs traversed.
    pub directory_runs: u64,
    /// Addressed tiles, grouped by archive zoom (0 through 31: verify reads
    /// any archive, not only profile-range ones), retained even when shared
    /// payloads are checked only once.
    pub addressed_per_zoom: [u64; ARCHIVE_ZOOM_COUNT],
    pub unique_payloads: bool,
    pub tile_errors: Vec<String>,
    /// BTreeSet, not a hash set: this is iterated to build the "undeclared
    /// layers" warning, so the report order must not depend on hash order.
    pub layers_observed: BTreeSet<String>,
    pub layers_declared: Vec<String>,
    pub passed: bool,
    pub geometry_stats: Option<GeometryStats>,
}

/// Per-zoom ocean-layer geometry statistics (--geometry-stats).
#[derive(Default)]
pub struct ZoomGeomStats {
    pub features: u64,
    pub rings: u64,
    /// Sum of encoded ocean layer protobuf bytes for tiles at this zoom.
    pub encoded_bytes: u64,
    /// One entry per ring: vertex count (closed ring, incl. closing vertex).
    pub ring_vertex_counts: Vec<u32>,
    /// Adjacent equal vertex pairs within rings (excludes ring closure).
    pub consecutive_dups: u64,
    /// Single-ring features whose ring is a 4-corner rectangle covering the
    /// full [0, 4096] extent (buffered full-tile ocean fills).
    pub full_tile_features: u64,
}

struct CachedTileOutcome {
    layer_names: Vec<String>,
    structure_errors: Vec<String>,
    geometry_error: Option<String>,
    ocean_errors: Vec<String>,
    geometry_stats: Option<ZoomGeomStats>,
}

#[derive(Default)]
pub struct GeometryStats {
    pub per_zoom: std::collections::BTreeMap<u8, ZoomGeomStats>,
}

impl GeometryStats {
    /// Print a per-zoom table to stdout.
    pub fn print_summary(&self) {
        println!();
        println!("Ocean geometry stats:");
        println!(
            "{:>4}  {:>9}  {:>9}  {:>13}  {:>9}  {:>9}  {:>11}  {:>9}",
            "zoom",
            "features",
            "rings",
            "encoded_bytes",
            "max_verts",
            "p99_verts",
            "consec_dups",
            "full_tile"
        );
        for (z, s) in &self.per_zoom {
            let mut counts = s.ring_vertex_counts.clone();
            counts.sort_unstable();
            let max = counts.last().copied().unwrap_or(0);
            let p99 = if counts.is_empty() {
                0
            } else {
                counts[(counts.len() - 1).min(counts.len() * 99 / 100)]
            };
            println!(
                "{:>4}  {:>9}  {:>9}  {:>13}  {:>9}  {:>9}  {:>11}  {:>9}",
                z,
                s.features,
                s.rings,
                s.encoded_bytes,
                max,
                p99,
                s.consecutive_dups,
                s.full_tile_features
            );
        }
    }
}

impl VerifyReport {
    /// Print a human-readable summary to stdout.
    pub fn print_summary(&self) {
        if self.passed {
            println!("PASS  ({} tiles checked)", self.tiles_checked);
        } else {
            println!("FAIL  ({} tiles checked)", self.tiles_checked);
        }

        if self.unique_payloads {
            println!(
                "Unique payloads: {} blobs across {} directory runs",
                self.unique_blobs, self.directory_runs
            );
            for (z, addressed) in self.addressed_per_zoom.iter().enumerate() {
                if *addressed != 0 {
                    println!("  z{z}: {addressed} addressed tiles");
                }
            }
        }

        if !self.tile_errors.is_empty() {
            println!();
            println!("Tile errors ({}):", self.tile_errors.len());
            for err in &self.tile_errors {
                println!("  {err}");
            }
            if self.tile_errors.len() >= MAX_TILE_ERRORS {
                println!("  ... (stopped after {MAX_TILE_ERRORS} errors)");
            }
        }

        // Layer coverage summary.
        let shortbread_names: FxHashSet<&str> = Layer::ALL.iter().map(|l| l.name()).collect();

        let declared_set: FxHashSet<&str> =
            self.layers_declared.iter().map(String::as_str).collect();

        // Layers observed in tiles but not declared in metadata.
        let undeclared: Vec<&str> = self
            .layers_observed
            .iter()
            .filter(|l| !declared_set.contains(l.as_str()))
            .map(String::as_str)
            .collect();

        // Layers declared but never seen in any tile.
        let unseen: Vec<&str> = self
            .layers_declared
            .iter()
            .filter(|l| !self.layers_observed.contains(l.as_str()))
            .map(String::as_str)
            .collect();

        // Declared layer names that aren't valid Shortbread layer names.
        let non_shortbread: Vec<&str> = self
            .layers_declared
            .iter()
            .filter(|l| !shortbread_names.contains(l.as_str()))
            .map(String::as_str)
            .collect();

        if !undeclared.is_empty() {
            println!();
            println!(
                "Warning: layers observed in tiles but not declared in metadata: {undeclared:?}"
            );
        }
        if !unseen.is_empty() {
            println!();
            println!("Info: layers declared in metadata but not observed in any tile: {unseen:?}");
        }
        if !non_shortbread.is_empty() {
            println!();
            println!("Warning: declared layers not in Shortbread schema: {non_shortbread:?}");
        }

        println!();
        println!(
            "Layers: {} observed, {} declared",
            self.layers_observed.len(),
            self.layers_declared.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Verify a PMTiles archive. Returns a report on success, or a fatal error
/// if the container or metadata is unreadable.
pub fn verify(path: &Path) -> Result<VerifyReport, VerifyError> {
    verify_opts(path, false)
}

/// Verify with options: `geometry_stats` additionally collects per-zoom
/// ocean-layer geometry statistics into the report.
#[allow(clippy::too_many_lines)]
pub fn verify_opts(path: &Path, geometry_stats: bool) -> Result<VerifyReport, VerifyError> {
    verify_opts_unique(path, geometry_stats, false)
}

/// Verify with optional unique-payload traversal. The unique mode validates a
/// blob once per zoom and seam class while retaining addressed-tile accounting.
#[allow(clippy::too_many_lines)]
pub fn verify_opts_unique(
    path: &Path,
    geometry_stats: bool,
    unique_payloads: bool,
) -> Result<VerifyReport, VerifyError> {
    // -- Open and validate header --
    let reader = Archive::open(path)?;
    let header = *reader.header();

    reader
        .tile_decoder()
        .map_err(|error| VerifyError::Container(error.to_string()))?;

    // Zoom sanity.
    if header.min_zoom > header.max_zoom {
        return Err(VerifyError::Container(format!(
            "min_zoom ({}) > max_zoom ({})",
            header.min_zoom, header.max_zoom
        )));
    }

    // -- Section bounds --
    reader
        .check_layout()
        .map_err(|error| VerifyError::Container(error.to_string()))?;

    // Dedup invariant: unique <= addressed.
    if header.tile_contents > header.addressed_tiles {
        return Err(VerifyError::Container(format!(
            "num_unique ({}) > num_addressed ({})",
            header.tile_contents, header.addressed_tiles
        )));
    }

    // -- Metadata --
    let metadata_json = reader.metadata()?;
    let parsed: serde_json::Value = serde_json::from_str(&metadata_json)
        .map_err(|e| VerifyError::Metadata(format!("invalid JSON: {e}")))?;

    let layers_declared = extract_declared_layers(&parsed)?;

    // -- Tile traversal --
    let runs = reader.read_all_runs()?;

    let mut tiles_checked: u64 = 0;
    let mut tile_errors: Vec<String> = Vec::new();
    let mut layers_observed: BTreeSet<String> = BTreeSet::new();
    let mut stats: Option<GeometryStats> = geometry_stats.then(GeometryStats::default);
    let mut seen_payloads: FxHashSet<(u64, u32, ValidationClass)> = FxHashSet::default();
    let mut unique_blobs: FxHashSet<(u64, u32)> = FxHashSet::default();
    let mut addressed_per_zoom = [0_u64; ARCHIVE_ZOOM_COUNT];

    for run in runs.as_slice() {
        if tile_errors.len() >= MAX_TILE_ERRORS {
            break;
        }
        let run_start = run.start().raw();
        let run_end = run.end();
        let blob = run.blob();
        let mut unique_tiles = Vec::new();
        for tile_id in run_start..run_end {
            let (z, x, y) = tile_coord(tile_id);
            addressed_per_zoom[usize::from(z)] += 1;
            if unique_payloads {
                let class = ValidationClass::for_tile(z, x);
                if seen_payloads.insert((blob.offset, blob.length, class)) {
                    unique_blobs.insert((blob.offset, blob.length));
                    unique_tiles.push((tile_id, z, x, y, class));
                }
            }
        }
        if unique_payloads && unique_tiles.is_empty() {
            continue;
        }

        // Every addressed tile in a run has the same blob. Read and decompress
        // it once, then retain per-tile report attribution below.
        let decompressed = match reader.read_tile(blob) {
            Ok(data) => data,
            Err(e) => {
                let error_tiles: Box<dyn Iterator<Item = (u64, u8, u32, u32)>> = if unique_payloads
                {
                    Box::new(
                        unique_tiles
                            .iter()
                            .map(|&(tile_id, z, x, y, _class)| (tile_id, z, x, y)),
                    )
                } else {
                    Box::new((run_start..run_end).map(|tile_id| {
                        let (z, x, y) = tile_coord(tile_id);
                        (tile_id, z, x, y)
                    }))
                };
                for (_tile_id, z, x, y) in error_tiles {
                    if tile_errors.len() >= MAX_TILE_ERRORS {
                        break;
                    }
                    tile_errors.push(format!("z{z}/{x}/{y}: decompression failed: {e}"));
                    tiles_checked += 1;
                }
                continue;
            }
        };

        // Cache the complete outcome per class within this run. Keeping the
        // cache run-local avoids retaining layer strings and ring statistics
        // for every distinct payload in a large archive.
        let mut payload_outcomes: FxHashMap<ValidationClass, CachedTileOutcome> =
            FxHashMap::default();
        let validation_tiles: Box<dyn Iterator<Item = (u64, u8, u32, u32, ValidationClass)>> =
            if unique_payloads {
                Box::new(unique_tiles.into_iter())
            } else {
                Box::new((run_start..run_end).map(|tile_id| {
                    let (z, x, y) = tile_coord(tile_id);
                    (tile_id, z, x, y, ValidationClass::for_tile(z, x))
                }))
            };
        for (_tile_id, z, x, y, class) in validation_tiles {
            if tile_errors.len() >= MAX_TILE_ERRORS {
                break;
            }
            let uncached_outcome =
                unique_payloads.then(|| build_tile_outcome(&decompressed, class, geometry_stats));
            let outcome: &CachedTileOutcome = if let Some(outcome) = uncached_outcome.as_ref() {
                outcome
            } else {
                payload_outcomes
                    .entry(class)
                    .or_insert_with(|| build_tile_outcome(&decompressed, class, geometry_stats))
            };

            layers_observed.extend(outcome.layer_names.iter().cloned());
            for problem in &outcome.structure_errors {
                tile_errors.push(format!("z{z}/{x}/{y}: {problem}"));
            }
            if tile_errors.len() < MAX_TILE_ERRORS
                && let Some(msg) = &outcome.geometry_error
            {
                tile_errors.push(format!("z{z}/{x}/{y}: {msg}"));
            }

            // Ocean polygon ring validity: check for self-intersections.
            if tile_errors.len() < MAX_TILE_ERRORS {
                for problem in &outcome.ocean_errors {
                    tile_errors.push(format!("z{z}/{x}/{y}: {problem}"));
                    if tile_errors.len() >= MAX_TILE_ERRORS {
                        break;
                    }
                }
            }

            if let (Some(stats), Some(delta)) = (stats.as_mut(), outcome.geometry_stats.as_ref()) {
                add_zoom_stats(stats.per_zoom.entry(z).or_default(), delta);
            }

            tiles_checked += 1;
        }
    }

    let passed = tile_errors.is_empty();

    Ok(VerifyReport {
        tiles_checked,
        unique_blobs: u64::try_from(unique_blobs.len()).unwrap_or(u64::MAX),
        directory_runs: u64::try_from(runs.len()).unwrap_or(u64::MAX),
        addressed_per_zoom,
        unique_payloads,
        tile_errors,
        layers_observed,
        layers_declared,
        passed,
        geometry_stats: stats,
    })
}

fn build_tile_outcome(
    data: &[u8],
    class: ValidationClass,
    collect_stats: bool,
) -> CachedTileOutcome {
    let mut outcome = CachedTileOutcome {
        layer_names: Vec::new(),
        structure_errors: Vec::new(),
        geometry_error: None,
        ocean_errors: Vec::new(),
        geometry_stats: None,
    };
    // One strict parse feeds every check below. A tile that does not parse
    // reports only that: the checks have nothing well formed to read.
    let tile = match parse_checked_tile(data) {
        Ok(tile) => tile,
        Err(error) => {
            outcome
                .structure_errors
                .push(format!("MVT decode failed: {error}"));
            return outcome;
        }
    };
    if tile.is_empty() {
        outcome.structure_errors.push("no MVT layers".to_string());
    }
    for layer in &tile {
        if layer.message.name.is_empty() {
            outcome
                .structure_errors
                .push("empty layer name".to_string());
        }
        outcome.layer_names.push(layer.message.name.to_string());
    }

    outcome.geometry_error = validate_tile_geometry(&tile, class).err();
    outcome.ocean_errors = validate_ocean_rings(&tile);
    if collect_stats {
        let mut collected = GeometryStats::default();
        collect_ocean_geometry_stats(&tile, class.z, &mut collected);
        outcome.geometry_stats = collected.per_zoom.remove(&class.z);
    }
    outcome
}

/// One layer as verify checks it: the envelope plus every feature parsed,
/// every value decoded and every tag index resolved against the tables.
struct CheckedLayer<'a> {
    message: message::LayerMessage<'a>,
    values: Vec<ValueMessage<'a>>,
    features: Vec<CheckedFeature<'a>>,
}

struct CheckedFeature<'a> {
    message: message::FeatureMessage<'a>,
    tags: Vec<(usize, usize)>,
}

/// Parse a tile under the message reader's `Strict` policy and go further
/// than its envelope: every feature and every value-table entry, referenced
/// or not, must decode, and every tag index must name a key and a value.
/// Verify reads elivagar's own output, which writes no field MVT does not
/// name and no field twice.
fn parse_checked_tile(data: &[u8]) -> Result<Vec<CheckedLayer<'_>>, String> {
    let strict = Strictness::Strict;
    let mut checked = Vec::new();
    for message in message::parse_tile(data, strict)? {
        let name = message.name;
        let values = message
            .values
            .iter()
            .enumerate()
            .map(|(idx, bytes)| {
                message::decode_value(bytes, strict)
                    .map_err(|e| format!("[{name}] value {idx}: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut features = Vec::with_capacity(message.features.len());
        for (idx, bytes) in message.features.iter().enumerate() {
            let feature = message::parse_feature(bytes, strict)
                .map_err(|e| format!("[{name}] feat {idx}: {e}"))?;
            let tags = message::tag_pairs(&feature.tags)
                .map_err(|e| format!("[{name}] feat {idx}: {e}"))?;
            if let Some(&(key, value)) = tags
                .iter()
                .find(|&&(key, value)| key >= message.keys.len() || value >= values.len())
            {
                return Err(format!(
                    "[{name}] feat {idx}: tag ({key}, {value}) out of bounds"
                ));
            }
            features.push(CheckedFeature {
                message: feature,
                tags,
            });
        }
        checked.push(CheckedLayer {
            message,
            values,
            features,
        });
    }
    Ok(checked)
}

/// The ocean layers of a tile, each with its extent.
fn ocean_layers<'t, 'a>(
    tile: &'t [CheckedLayer<'a>],
) -> impl Iterator<Item = &'t CheckedLayer<'a>> {
    tile.iter()
        .filter(|layer| layer.message.name == Layer::Ocean.name())
}

/// A feature's polygon geometry bytes, None for any other type.
fn polygon_geometry<'f>(feature: &'f CheckedFeature<'_>) -> Option<&'f [u8]> {
    (feature.message.geom_type == 3)
        .then_some(feature.message.geometry.as_deref())
        .flatten()
}

fn add_zoom_stats(target: &mut ZoomGeomStats, delta: &ZoomGeomStats) {
    target.features += delta.features;
    target.rings += delta.rings;
    target.encoded_bytes += delta.encoded_bytes;
    target.consecutive_dups += delta.consecutive_dups;
    target.full_tile_features += delta.full_tile_features;
    target
        .ring_vertex_counts
        .extend_from_slice(&delta.ring_vertex_counts);
}

/// Decode the ocean layer of one tile and accumulate geometry statistics.
fn collect_ocean_geometry_stats(tile: &[CheckedLayer<'_>], z: u8, stats: &mut GeometryStats) {
    for layer in ocean_layers(tile) {
        // A zero extent is reported by validate_tile_geometry.
        if layer.message.extent == 0 {
            continue;
        }
        collect_ocean_layer_stats(layer, z, stats);
    }
}

#[allow(clippy::cast_possible_truncation)]
fn collect_ocean_layer_stats(layer: &CheckedLayer<'_>, z: u8, stats: &mut GeometryStats) {
    let extent = i64::from(layer.message.extent);
    let zs = stats.per_zoom.entry(z).or_default();
    zs.encoded_bytes += layer.message.encoded_len as u64;
    for feature in &layer.features {
        let Some(gb) = polygon_geometry(feature) else {
            continue;
        };
        // A polygon that does not decode is reported by validate_tile_geometry
        // and validate_ocean_rings on the same tile; statistics skip it.
        let Ok(rings) = decode::decode_polygon_rings(Words::packed(gb)) else {
            continue;
        };
        if rings.is_empty() {
            continue;
        }

        zs.features += 1;
        zs.rings += rings.len() as u64;
        for ring in &rings {
            zs.ring_vertex_counts.push(ring.len() as u32);
            // Adjacent equal pairs; the closing vertex duplicates the FIRST
            // vertex (not its predecessor), so closure never counts.
            for pair in ring.windows(2) {
                if pair[0] == pair[1] {
                    zs.consecutive_dups += 1;
                }
            }
        }
        if rings.len() == 1 && is_full_tile_rect_with_extent(&rings[0], extent) {
            zs.full_tile_features += 1;
        }
    }
}

/// A closed 4-corner rectangle ring covering the full [0, 4096] extent
/// (i.e. a buffered full-tile ocean fill).
#[cfg(test)]
fn is_full_tile_rect(ring: &[(i32, i32)]) -> bool {
    is_full_tile_rect_with_extent(ring, MVT_EXTENT)
}

fn is_full_tile_rect_with_extent(ring: &[(i32, i32)], ext: i64) -> bool {
    let n = if ring.first() == ring.last() && ring.len() > 1 {
        ring.len() - 1
    } else {
        ring.len()
    };
    if n != 4 {
        return false;
    }
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    for &(x, y) in &ring[..n] {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
        // Every vertex must be a corner of the bbox - checked below.
    }
    let corners = [
        (min_x, min_y),
        (max_x, min_y),
        (max_x, max_y),
        (min_x, max_y),
    ];
    let all_corners = ring[..n].iter().all(|v| corners.contains(v));
    all_corners && min_x <= 0 && min_y <= 0 && i64::from(max_x) >= ext && i64::from(max_y) >= ext
}

/// Geometry limits, extent and paint order over every layer of a tile.
fn validate_tile_geometry(tile: &[CheckedLayer<'_>], class: ValidationClass) -> Result<(), String> {
    for layer in tile {
        validate_layer_geometry(layer, class)?;
    }
    Ok(())
}

fn validate_layer_geometry(
    layer: &CheckedLayer<'_>,
    mut class: ValidationClass,
) -> Result<(), String> {
    let layer_name = layer.message.name;
    if layer.message.extent == 0 {
        return Err(format!("[{layer_name}] layer extent is zero"));
    }
    class.extent = i64::from(layer.message.extent);
    for (feat_idx, feature) in layer.features.iter().enumerate() {
        // The strict parse guarantees a geometry field.
        let geometry = feature.message.geometry.as_deref().unwrap_or_default();
        validate_geometry(
            Words::packed(geometry),
            u64::from(feature.message.geom_type),
            class,
            geometry.len(),
        )
        .map_err(|e| format!("[{layer_name}] feat {feat_idx}: {e}"))?;
    }
    validate_paint_order(layer).map_err(|e| format!("[{layer_name}] {e}"))?;
    Ok(())
}

#[cfg(test)]
fn validate_mvt_geometry(data: &[u8], class: ValidationClass) -> Result<(), String> {
    validate_tile_geometry(&parse_checked_tile(data)?, class)
}

fn validate_paint_order(layer: &CheckedLayer<'_>) -> Result<(), String> {
    let Some(rank_layer) = Layer::from_name(layer.message.name).filter(|l| l.has_paint_order())
    else {
        return Ok(());
    };
    let mut previous = None;
    for (feature_idx, feature) in layer.features.iter().enumerate() {
        let attrs = paint_attrs(layer, feature).map_err(|e| format!("feat {feature_idx}: {e}"))?;
        let kind = attrs
            .kind
            .ok_or_else(|| format!("feat {feature_idx}: missing kind attribute"))?;
        let rank = match rank_layer.paint_rank_for(&RankAttributes {
            kind,
            link: attrs.link,
            tunnel: attrs.tunnel,
            bridge: attrs.bridge,
        }) {
            Ok(Some(rank)) => rank,
            Ok(None) => unreachable!("layer was filtered to ordered layers"),
            Err(RankError::UnknownLandKind) => {
                return Err(format!("feat {feature_idx}: unknown land kind {kind:?}"));
            }
            Err(RankError::UnknownStreetKind) => {
                return Err(format!("feat {feature_idx}: unknown street kind {kind:?}"));
            }
        };
        if let Some(previous) = previous
            && rank < previous
        {
            return Err(format!(
                "feat {feature_idx}: paint rank {rank} follows {previous}"
            ));
        }
        previous = Some(rank);
    }
    Ok(())
}

#[derive(Default)]
struct PaintAttrs<'a> {
    kind: Option<&'a str>,
    link: bool,
    tunnel: bool,
    bridge: bool,
    seen_link: bool,
    seen_tunnel: bool,
    seen_bridge: bool,
}

/// The paint-rank attributes of one feature; `parse_checked_tile` has
/// already bounds-checked every tag index.
fn paint_attrs<'a>(
    layer: &CheckedLayer<'a>,
    feature: &CheckedFeature<'a>,
) -> Result<PaintAttrs<'a>, String> {
    let mut attrs = PaintAttrs::default();
    for &(key_idx, value_idx) in &feature.tags {
        let key = &layer.message.keys[key_idx];
        let value = layer.values[value_idx];
        match *key {
            "kind" => match value {
                ValueMessage::String(kind) if attrs.kind.replace(kind).is_none() => {}
                ValueMessage::String(_) => {
                    return Err("duplicate kind attribute".to_string());
                }
                _ => return Err("kind attribute is not a string".to_string()),
            },
            "link" | "tunnel" | "bridge" => {
                let flag = match value {
                    ValueMessage::Bool(flag) => flag,
                    _ => return Err(format!("{key} attribute is not a bool")),
                };
                let (slot, seen) = match *key {
                    "link" => (&mut attrs.link, &mut attrs.seen_link),
                    "tunnel" => (&mut attrs.tunnel, &mut attrs.seen_tunnel),
                    "bridge" => (&mut attrs.bridge, &mut attrs.seen_bridge),
                    _ => unreachable!(),
                };
                if *seen {
                    return Err(format!("duplicate {key} attribute"));
                }
                *slot = flag;
                *seen = true;
            }
            _ => {}
        }
    }
    Ok(attrs)
}

/// verify's checks on top of the shared decoder and its spec grammar: the
/// vtzero count cap and the extent-scaled delta and absolute limits.
struct GeometryLimits {
    max_count: u32,
    delta_limit: i64,
    coord_limit: i64,
}

impl CommandSink for GeometryLimits {
    fn command(&mut self, command: Command) -> Result<(), GeometryError> {
        let name = match command.kind {
            CommandKind::MoveTo => "MoveTo",
            CommandKind::LineTo => "LineTo",
            CommandKind::ClosePath => return Ok(()),
        };
        if command.count > self.max_count {
            return Err(GeometryError::Consumer(format!(
                "{name} count too large ({} > {})",
                command.count, self.max_count
            )));
        }
        Ok(())
    }

    fn point(&mut self, point: Point) -> Result<(), GeometryError> {
        let (dx, dy) = (i64::from(point.dx), i64::from(point.dy));
        if dx.abs() > self.delta_limit || dy.abs() > self.delta_limit {
            return Err(GeometryError::Consumer(format!(
                "suspicious geometry delta ({dx},{dy}) exceeds limit {}",
                self.delta_limit
            )));
        }
        if point.x.abs() > self.coord_limit || point.y.abs() > self.coord_limit {
            return Err(GeometryError::Consumer(format!(
                "geometry coordinate ({},{}) exceeds absolute limit {}",
                point.x, point.y, self.coord_limit
            )));
        }
        Ok(())
    }
}

fn validate_geometry(
    words: Words<'_>,
    geom_type: u64,
    class: ValidationClass,
    raw_byte_len: usize,
) -> Result<(), String> {
    let delta_limit = if class.seam {
        MVT_DELTA_LIMIT_SEAM / MVT_EXTENT * class.extent
    } else {
        MVT_DELTA_LIMIT / MVT_EXTENT * class.extent
    };
    // Maximum valid count: vtzero uses raw_byte_len / 2 (each varint is >=1
    // byte, each point needs 2 varints). This is a generous upper bound that
    // avoids false positives on large but valid geometries.
    let mut limits = GeometryLimits {
        max_count: u32::try_from(raw_byte_len / 2).unwrap_or(u32::MAX),
        delta_limit,
        coord_limit: MVT_COORD_ABS_LIMIT / MVT_EXTENT * class.extent,
    };
    decode::decode(words, Some(Grammar::for_geom_type(geom_type)), &mut limits)?;
    Ok(())
}

#[cfg(test)]
fn validate_geometry_commands(
    commands: &[u32],
    geom_type: u64,
    class: ValidationClass,
    raw_byte_len: usize,
) -> Result<(), String> {
    validate_geometry(Words::slice(commands), geom_type, class, raw_byte_len)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate ocean polygon rings for self-intersections.
/// Decodes MVT protobuf → finds ocean layer → decodes polygon geometry → checks each ring.
fn validate_ocean_rings(tile: &[CheckedLayer<'_>]) -> Vec<String> {
    let mut problems = Vec::new();
    for layer in ocean_layers(tile) {
        validate_ocean_layer_rings(layer, &mut problems);
    }
    problems
}

fn validate_ocean_layer_rings(layer: &CheckedLayer<'_>, problems: &mut Vec<String>) {
    for (fi, feature) in layer.features.iter().enumerate() {
        let Some(gb) = polygon_geometry(feature) else {
            continue;
        };

        let rings = match decode::decode_polygon_rings(Words::packed(gb)) {
            Ok(rings) => rings,
            Err(error) => {
                problems.push(format!("ocean feat {fi} polygon decode failed: {error}"));
                continue;
            }
        };
        for (ri, ring) in rings.iter().enumerate() {
            if ring.len() >= 4 && !crate::geometry::polygon::ring_is_simple(ring) {
                problems.push(format!(
                    "ocean feat {fi} ring {ri} is self-intersecting ({} verts)",
                    ring.len()
                ));
            }
        }
    }
}

fn extract_declared_layers(parsed: &serde_json::Value) -> Result<Vec<String>, VerifyError> {
    let vector_layers = parsed
        .get("vector_layers")
        .ok_or_else(|| VerifyError::Metadata("missing 'vector_layers' key".to_string()))?
        .as_array()
        .ok_or_else(|| VerifyError::Metadata("'vector_layers' is not an array".to_string()))?;

    let mut names = Vec::with_capacity(vector_layers.len());
    for layer in vector_layers {
        let id = layer.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
            VerifyError::Metadata("vector_layers entry missing 'id' string".to_string())
        })?;
        names.push(id.to_string());
    }
    Ok(names)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::archive::{LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter};
    use crate::mvt::{Feature, GeomType, LayerBuilder, Value, encode_tile};
    use flate2::Compression;

    fn xy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
        TileId::at(z, x, y).raw()
    }
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn land_tile(kinds: &[&str]) -> Vec<u8> {
        let mut layer = LayerBuilder::new("land");
        let key = layer.intern_key("kind");
        for (id, kind) in kinds.iter().enumerate() {
            let value = layer.intern_value(Value::String((*kind).to_string()));
            layer.add_feature(Feature {
                id: Some(id as u64),
                geom_type: GeomType::Point,
                geometry: vec![9, 0, 0],
                tags: vec![(key, value)],
            });
        }
        encode_tile(&[&layer])
    }

    #[test]
    fn full_tile_rect_detects_buffered_fill() {
        let ring = vec![
            (-128, -128),
            (4224, -128),
            (4224, 4224),
            (-128, 4224),
            (-128, -128),
        ];
        assert!(is_full_tile_rect(&ring));
    }

    #[test]
    fn full_tile_rect_rejects_partial_and_non_rect() {
        // Covers extent but 5 distinct vertices (not a pure rectangle).
        let ring = vec![
            (-128, -128),
            (4224, -128),
            (4224, 4224),
            (2000, 4224),
            (-128, 4000),
            (-128, -128),
        ];
        assert!(!is_full_tile_rect(&ring));
        // Rectangle but does not cover the full extent.
        let ring = vec![(0, 0), (2048, 0), (2048, 2048), (0, 2048), (0, 0)];
        assert!(!is_full_tile_rect(&ring));
    }

    #[inline]
    fn cmd(id: u32, count: u32) -> u32 {
        id | (count << 3)
    }

    #[inline]
    fn zz(v: i64) -> u32 {
        let n = ((v << 1) ^ (v >> 63)).cast_unsigned();
        u32::try_from(n).expect("zigzag value should fit in u32 for test cases")
    }

    #[test]
    fn geometry_rejects_unknown_command_id() {
        let err =
            validate_geometry_commands(&[cmd(4, 1)], 2, ValidationClass::for_tile(10, 1), 1000)
                .expect_err("unknown command id should fail");
        assert!(err.contains("unknown geometry command id 4"));
    }

    #[test]
    fn geometry_rejects_zero_repeat_count() {
        let err =
            validate_geometry_commands(&[cmd(1, 0)], 2, ValidationClass::for_tile(10, 1), 1000)
                .expect_err("zero repeat count should fail");
        assert!(err.contains("zero repeat count"));
    }

    #[test]
    fn geometry_rejects_linestring_line_to_before_move_to() {
        let commands = vec![cmd(2, 1), zz(1), zz(0)];
        let err = validate_geometry_commands(&commands, 2, ValidationClass::for_tile(10, 1), 1000)
            .expect_err("LineTo without MoveTo should fail");
        assert!(err.contains("LineTo must follow MoveTo"));
    }

    #[test]
    fn geometry_rejects_linestring_moveto_count_not_one() {
        let commands = vec![cmd(1, 2), zz(0), zz(0), zz(1), zz(1)];
        let err = validate_geometry_commands(&commands, 2, ValidationClass::for_tile(10, 1), 1000)
            .expect_err("linestring MoveTo count other than one should fail");
        assert!(err.contains("MoveTo count must be 1"));
    }

    #[test]
    fn geometry_accepts_repeated_linestring_command_sequences() {
        let commands = vec![
            cmd(1, 1),
            zz(0),
            zz(0),
            cmd(2, 1),
            zz(1),
            zz(0),
            cmd(1, 1),
            zz(1),
            zz(1),
            cmd(2, 1),
            zz(0),
            zz(1),
        ];
        validate_geometry_commands(&commands, 2, ValidationClass::for_tile(10, 1), 1000)
            .expect("multilinestring command sequences should pass");
    }

    #[test]
    fn checked_polygon_decoder_rejects_coordinate_overflow() {
        let commands = vec![
            cmd(1, 1),
            u32::MAX - 1,
            zz(0),
            cmd(2, 2),
            zz(1),
            zz(0),
            zz(1),
            zz(0),
        ];
        let err = decode::decode_polygon_rings(Words::slice(&commands))
            .expect_err("coordinate addition overflow should fail");
        assert!(err.to_string().contains("coordinate overflows i32"));
    }

    #[test]
    fn geometry_rejects_polygon_moveto_count_not_one() {
        let commands = vec![cmd(1, 2), zz(0), zz(0), zz(1), zz(1)];
        let err = validate_geometry_commands(&commands, 3, ValidationClass::for_tile(10, 1), 1000)
            .expect_err("polygon MoveTo count != 1 should fail");
        assert!(err.contains("MoveTo count must be 1"));
    }

    #[test]
    fn geometry_rejects_closepath_in_non_polygon() {
        let commands = vec![cmd(1, 1), zz(0), zz(0), cmd(7, 1)];
        let err = validate_geometry_commands(&commands, 2, ValidationClass::for_tile(10, 1), 1000)
            .expect_err("ClosePath in non-polygon should fail");
        assert!(err.contains("ClosePath in non-polygon"));
    }

    #[test]
    fn geometry_rejects_polygon_closepath_without_enough_points() {
        let commands = vec![cmd(1, 1), zz(0), zz(0), cmd(7, 1)];
        let err = validate_geometry_commands(&commands, 3, ValidationClass::for_tile(10, 1), 1000)
            .expect_err("polygon ClosePath without enough points should fail");
        assert!(err.contains("too few points"));
    }

    #[test]
    fn geometry_rejects_polygon_missing_closepath() {
        let commands = vec![
            cmd(1, 1),
            zz(0),
            zz(0),
            cmd(2, 2),
            zz(1),
            zz(0),
            zz(0),
            zz(1),
        ];
        let err = validate_geometry_commands(&commands, 3, ValidationClass::for_tile(10, 1), 1000)
            .expect_err("polygon without ClosePath should fail");
        assert!(err.contains("missing ClosePath"));
    }

    #[test]
    fn geometry_rejects_absolute_coordinate_limit_exceeded() {
        // Three deltas at +65536 each stay within per-step delta limit but exceed
        // absolute coordinate guard on the third step.
        let commands = vec![
            cmd(1, 1),
            zz(0),
            zz(0),
            cmd(2, 3),
            zz(65_536),
            zz(0),
            zz(65_536),
            zz(0),
            zz(65_536),
            zz(0),
        ];
        let err = validate_geometry_commands(&commands, 2, ValidationClass::for_tile(10, 1), 1000)
            .expect_err("absolute coordinate limit should fail");
        assert!(err.contains("exceeds absolute limit"));
    }

    #[test]
    fn geometry_seam_tile_uses_stricter_delta_limit() {
        // 20k delta is > seam limit (16384) but < non-seam limit (65536).
        let commands = vec![cmd(1, 1), zz(0), zz(0), cmd(2, 1), zz(20_000), zz(0)];

        // Non-seam tile passes.
        validate_geometry_commands(&commands, 2, ValidationClass::for_tile(4, 1), 1000)
            .expect("non-seam tile should allow 20k delta");

        // Seam tile fails.
        let err = validate_geometry_commands(&commands, 2, ValidationClass::for_tile(4, 0), 1000)
            .expect_err("seam tile should reject 20k delta");
        assert!(err.contains("suspicious geometry delta"));
        assert!(err.contains(&MVT_DELTA_LIMIT_SEAM.to_string()));
    }

    #[test]
    fn verify_counts_run_tiles_and_groups_seam_geometry_errors() {
        let mut coordinates: Vec<(u32, u32)> =
            (0..4).flat_map(|x| (0..4).map(move |y| (x, y))).collect();
        coordinates.sort_unstable_by_key(|&(x, y)| xy_to_tile_id(2, x, y));
        let pair = coordinates
            .windows(2)
            .find(|pair| {
                let seam = |x| x == 0 || x == 3;
                seam(pair[0].0) != seam(pair[1].0)
            })
            .expect("z2 Hilbert order must cross a seam boundary");

        // Not a paint-order-checked layer: the delta must be the only error.
        let mut layer = LayerBuilder::new("water_lines");
        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::LineString,
            geometry: vec![cmd(1, 1), zz(0), zz(0), cmd(2, 1), zz(20_000), zz(0)],
            tags: Vec::new(),
        });
        let tile = encode_tile(&[&layer]);
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(&tile).expect("gzip tile");
        let gzip = gzip.finish().expect("finish gzip tile");

        let mut writer = PmtilesWriter::new(
            PmtilesConfig::new(
                2,
                2,
                LonLatBbox {
                    west: -180.0,
                    south: -85.0,
                    east: 180.0,
                    north: 85.0,
                },
                MapCenter {
                    lon: 0.0,
                    lat: 0.0,
                    zoom: 2,
                },
            )
            .expect("valid archive config"),
        );
        for &(x, y) in pair {
            writer.add_tile(2, x, y, &gzip).expect("add tile");
        }
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("mixed-seam-run.pmtiles");
        writer.write_to(&path).expect("write archive");

        let report = verify(&path).expect("verify archive");
        assert_eq!(report.tiles_checked, 2);
        assert_eq!(report.tile_errors.len(), 1);
        let seam_tile = pair
            .iter()
            .find(|&&(x, _)| x == 0 || x == 3)
            .expect("pair includes a seam tile");
        assert!(report.tile_errors[0].starts_with(&format!(
            "z2/{}/{}: [water_lines] feat 0: suspicious geometry delta",
            seam_tile.0, seam_tile.1
        )));
    }

    #[test]
    fn verify_replays_shared_geometry_error_for_every_run_tile() {
        // Two consecutive-Hilbert interior tiles (same z, same non-seam
        // group) sharing one bad blob: the cached validation outcome must be
        // replayed per tile, exactly like the old per-tile loop.
        let mut coordinates: Vec<(u32, u32)> =
            (1..3).flat_map(|x| (0..4).map(move |y| (x, y))).collect();
        coordinates.sort_unstable_by_key(|&(x, y)| xy_to_tile_id(2, x, y));
        let pair = coordinates
            .windows(2)
            .find(|pair| {
                xy_to_tile_id(2, pair[1].0, pair[1].1) == xy_to_tile_id(2, pair[0].0, pair[0].1) + 1
            })
            .expect("interior z2 tiles include a consecutive pair");

        // Delta above even the non-seam limit, so the shared group errors.
        let mut layer = LayerBuilder::new("water_lines");
        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::LineString,
            geometry: vec![cmd(1, 1), zz(0), zz(0), cmd(2, 1), zz(70_000), zz(0)],
            tags: Vec::new(),
        });
        let tile = encode_tile(&[&layer]);
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(&tile).expect("gzip tile");
        let gzip = gzip.finish().expect("finish gzip tile");

        let mut writer = PmtilesWriter::new(
            PmtilesConfig::new(
                2,
                2,
                LonLatBbox {
                    west: -180.0,
                    south: -85.0,
                    east: 180.0,
                    north: 85.0,
                },
                MapCenter {
                    lon: 0.0,
                    lat: 0.0,
                    zoom: 2,
                },
            )
            .expect("valid archive config"),
        );
        for &(x, y) in pair {
            writer.add_tile(2, x, y, &gzip).expect("add tile");
        }
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("shared-error-run.pmtiles");
        writer.write_to(&path).expect("write archive");

        let report = verify(&path).expect("verify archive");
        assert_eq!(report.tiles_checked, 2);
        assert_eq!(report.tile_errors.len(), 2);
        for (error, &(x, y)) in report.tile_errors.iter().zip(pair) {
            assert!(error.starts_with(&format!(
                "z2/{x}/{y}: [water_lines] feat 0: suspicious geometry delta"
            )));
        }
    }

    #[test]
    fn order_check_flags_out_of_order_land_kinds() {
        let tile = land_tile(&["forest", "residential"]);
        let err = validate_mvt_geometry(&tile, ValidationClass::for_tile(11, 1))
            .expect_err("forest before residential must fail paint order validation");
        assert!(err.contains("paint rank 0 follows 5"));
    }

    #[test]
    fn order_check_uses_street_details_without_every_boolean_key() {
        for name in ["streets", "street_polygons"] {
            for bridge_first in [false, true] {
                let mut layer = LayerBuilder::new(name);
                let kind_key = layer.intern_key("kind");
                let kind = layer.intern_string_value("service");
                let bridge_key = layer.intern_key("bridge");
                let yes = layer.intern_value(Value::Bool(true));
                for bridge in [bridge_first, !bridge_first] {
                    let mut tags = vec![(kind_key, kind)];
                    if bridge {
                        tags.push((bridge_key, yes));
                    }
                    layer.add_feature(Feature {
                        id: None,
                        geom_type: GeomType::Point,
                        geometry: vec![9, 0, 0],
                        tags,
                    });
                }
                let tile = encode_tile(&[&layer]);
                let result = validate_mvt_geometry(&tile, ValidationClass::for_tile(14, 1));
                assert_eq!(result.is_err(), bridge_first, "{name}: {result:?}");
            }
        }
    }

    #[test]
    fn payload_structure_is_checked_strictly() {
        let class = ValidationClass::for_tile(11, 1);
        let clean = land_tile(&["residential"]);
        assert!(
            build_tile_outcome(&clean, class, false)
                .structure_errors
                .is_empty()
        );
        // A field MVT does not name and a truncated tag both fail the tile.
        let mut foreign = clean.clone();
        foreign.extend_from_slice(&[8, 1]);
        let mut truncated = clean;
        truncated.push(0x80);
        for tile in [foreign, truncated] {
            let outcome = build_tile_outcome(&tile, class, false);
            assert_eq!(outcome.structure_errors.len(), 1, "{tile:?}");
            assert!(outcome.structure_errors[0].starts_with("MVT decode failed"));
        }
    }

    #[test]
    fn order_check_fails_closed_on_undecodable_kind() {
        let mut layer = LayerBuilder::new("land");
        let key = layer.intern_key("kind");
        let value = layer.intern_value(Value::Bool(true));
        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::Point,
            geometry: vec![9, 0, 0],
            tags: vec![(key, value)],
        });
        let tile = encode_tile(&[&layer]);
        let err = validate_mvt_geometry(&tile, ValidationClass::for_tile(11, 1))
            .expect_err("mistyped kind must fail closed");
        assert!(err.contains("kind attribute is not a string"));
    }
}
