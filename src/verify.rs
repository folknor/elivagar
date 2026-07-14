//! PMTiles output verification.
//!
//! Validates a PMTiles archive end-to-end: container integrity, metadata schema,
//! tile decompression, MVT payload structure, and layer coverage.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::Path;

use crate::pmtiles_reader::{self, PmtilesReader};
use crate::pmtiles_writer::tile_id_to_zxy;
use crate::shortbread::{Layer, paint_order};
use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

/// Maximum number of tile-level errors before aborting traversal.
const MAX_TILE_ERRORS: usize = 100;
const MVT_EXTENT: i64 = 4096;
const MVT_COORD_ABS_LIMIT: i64 = MVT_EXTENT * 32; // 131072
const MVT_DELTA_LIMIT: i64 = MVT_EXTENT * 16; // 65536
const MVT_DELTA_LIMIT_SEAM: i64 = MVT_EXTENT * 4; // 16384

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
    /// Addressed tiles, grouped by zoom, retained even when shared payloads
    /// are checked only once.
    pub addressed_per_zoom: [u64; 15],
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
/// blob once per `(offset, length, zoom, seam)` while retaining addressed-tile
/// accounting.
#[allow(clippy::too_many_lines)]
pub fn verify_opts_unique(
    path: &Path,
    geometry_stats: bool,
    unique_payloads: bool,
) -> Result<VerifyReport, VerifyError> {
    // -- Open and validate header --
    let mut reader = PmtilesReader::open(path)?;
    let file_size = reader.file_size()?;

    // Tile type must be MVT.
    if reader.tile_type() != 1 {
        return Err(VerifyError::Container(format!(
            "expected tile type MVT (1), got {}",
            reader.tile_type()
        )));
    }

    // Tile compression must be gzip.
    if reader.tile_compression() != 2 {
        return Err(VerifyError::Container(format!(
            "expected tile compression gzip (2), got {}",
            reader.tile_compression()
        )));
    }

    // Zoom sanity.
    if reader.min_zoom() > reader.max_zoom() {
        return Err(VerifyError::Container(format!(
            "min_zoom ({}) > max_zoom ({})",
            reader.min_zoom(),
            reader.max_zoom()
        )));
    }

    // -- Section bounds --
    check_section_bounds(
        "root_dir",
        reader.root_dir_offset(),
        reader.root_dir_length(),
        file_size,
    )?;
    check_section_bounds(
        "metadata",
        reader.metadata_offset(),
        reader.metadata_length(),
        file_size,
    )?;
    check_section_bounds(
        "leaf_dirs",
        reader.leaf_dirs_offset(),
        reader.leaf_dirs_length(),
        file_size,
    )?;
    check_section_bounds(
        "tile_data",
        reader.data_offset(),
        reader.data_length(),
        file_size,
    )?;

    // Dedup invariant: unique <= addressed.
    if reader.num_unique() > reader.num_addressed() {
        return Err(VerifyError::Container(format!(
            "num_unique ({}) > num_addressed ({})",
            reader.num_unique(),
            reader.num_addressed()
        )));
    }

    // -- Metadata --
    let metadata_json = reader.read_metadata()?;
    let parsed: serde_json::Value = serde_json::from_str(&metadata_json)
        .map_err(|e| VerifyError::Metadata(format!("invalid JSON: {e}")))?;

    let layers_declared = extract_declared_layers(&parsed)?;

    // -- Tile traversal --
    let runs = reader.read_all_runs()?;

    let mut tiles_checked: u64 = 0;
    let mut tile_errors: Vec<String> = Vec::new();
    let mut layers_observed: BTreeSet<String> = BTreeSet::new();
    let mut stats: Option<GeometryStats> = geometry_stats.then(GeometryStats::default);
    let mut seen_payloads: FxHashSet<(u64, u32, u8, bool)> = FxHashSet::default();
    let mut unique_blobs: FxHashSet<(u64, u32)> = FxHashSet::default();
    let mut addressed_per_zoom = [0_u64; 15];

    for run in &runs {
        if tile_errors.len() >= MAX_TILE_ERRORS {
            break;
        }
        let run_end = run
            .tile_id
            .checked_add(u64::from(run.run_length))
            .ok_or_else(|| VerifyError::Container("tile run overflows".to_string()))?;
        let mut unique_tiles = Vec::new();
        for tile_id in run.tile_id..run_end {
            let (z, x, y) = tile_id_to_zxy(tile_id);
            if let Some(addressed) = addressed_per_zoom.get_mut(z as usize) {
                *addressed += 1;
            }
            let seam = x == 0 || x == (1_u32 << z).saturating_sub(1);
            if unique_payloads && seen_payloads.insert((run.offset, run.length, z, seam)) {
                unique_blobs.insert((run.offset, run.length));
                unique_tiles.push((tile_id, z, x, y));
            }
        }
        if unique_payloads && unique_tiles.is_empty() {
            continue;
        }
        let representative = pmtiles_reader::TileEntry {
            tile_id: run.tile_id,
            offset: run.offset,
            length: run.length,
        };

        // Every addressed tile in a run has the same blob. Read and decompress
        // it once, then retain per-tile report attribution below.
        let decompressed = match reader.read_tile(&representative) {
            Ok(data) => data,
            Err(e) => {
                let error_tiles: Box<dyn Iterator<Item = (u64, u8, u32, u32)>> = if unique_payloads
                {
                    Box::new(unique_tiles.iter().copied())
                } else {
                    Box::new((run.tile_id..run_end).map(|tile_id| {
                        let (z, x, y) = tile_id_to_zxy(tile_id);
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

        // Geometry validation of the shared blob depends only on (z, seam).
        // Default traversal replays that outcome for every addressed tile;
        // unique mode selects one representative of each validation group.
        let mut geometry_outcomes: FxHashMap<(u8, bool), Option<String>> = FxHashMap::default();
        let validation_tiles: Box<dyn Iterator<Item = (u64, u8, u32, u32)>> = if unique_payloads {
            Box::new(unique_tiles.into_iter())
        } else {
            Box::new((run.tile_id..run_end).map(|tile_id| {
                let (z, x, y) = tile_id_to_zxy(tile_id);
                (tile_id, z, x, y)
            }))
        };
        for (_tile_id, z, x, y) in validation_tiles {
            if tile_errors.len() >= MAX_TILE_ERRORS {
                break;
            }
            // Decode MVT structure.
            match pmtiles_reader::decode_mvt_layers(&decompressed) {
                Ok(layers) => {
                    if layers.is_empty() {
                        tile_errors.push(format!("z{z}/{x}/{y}: no MVT layers"));
                    }
                    for layer in &layers {
                        if layer.name.is_empty() {
                            tile_errors.push(format!("z{z}/{x}/{y}: empty layer name"));
                        }
                        layers_observed.insert(layer.name.clone());
                    }
                }
                Err(e) => {
                    tile_errors.push(format!("z{z}/{x}/{y}: MVT decode failed: {e}"));
                }
            }
            let seam = x == 0 || x == (1u32 << z).saturating_sub(1);
            if tile_errors.len() < MAX_TILE_ERRORS {
                let outcome = geometry_outcomes
                    .entry((z, seam))
                    .or_insert_with(|| validate_mvt_geometry(&decompressed, z, x).err());
                if let Some(msg) = outcome {
                    tile_errors.push(format!("z{z}/{x}/{y}: {msg}"));
                }
            }

            // Ocean polygon ring validity: check for self-intersections.
            if tile_errors.len() < MAX_TILE_ERRORS {
                let ocean_problems = validate_ocean_rings(&decompressed);
                for problem in ocean_problems {
                    tile_errors.push(format!("z{z}/{x}/{y}: {problem}"));
                    if tile_errors.len() >= MAX_TILE_ERRORS {
                        break;
                    }
                }
            }

            if let Some(stats) = stats.as_mut() {
                collect_ocean_geometry_stats(&decompressed, z, stats);
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

/// Decode the ocean layer of one tile and accumulate geometry statistics.
fn collect_ocean_geometry_stats(data: &[u8], z: u8, stats: &mut GeometryStats) {
    let mut tile_cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = tile_cursor.read_tag() {
        if field == 3 && wire_type == WIRE_LEN {
            if let Ok(layer_data) = tile_cursor.read_len_delimited() {
                collect_ocean_layer_stats(layer_data, z, stats);
            }
        } else if tile_cursor.skip_field(wire_type).is_err() {
            break;
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn collect_ocean_layer_stats(layer_data: &[u8], z: u8, stats: &mut GeometryStats) {
    let mut name = String::new();
    let mut feature_blobs: Vec<&[u8]> = Vec::new();
    let mut cursor = Cursor::new(layer_data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if wire_type == WIRE_LEN {
            if let Ok(sub) = cursor.read_len_delimited() {
                match field {
                    1 => name = String::from_utf8_lossy(sub).to_string(),
                    2 => feature_blobs.push(sub),
                    _ => {}
                }
            }
        } else if cursor.skip_field(wire_type).is_err() {
            break;
        }
    }
    if name != "ocean" {
        return;
    }

    let zs = stats.per_zoom.entry(z).or_default();
    zs.encoded_bytes += layer_data.len() as u64;
    for feat_data in &feature_blobs {
        let mut geom_type: u64 = 0;
        let mut geom_bytes: Option<&[u8]> = None;
        let mut fc = Cursor::new(feat_data);
        while let Ok(Some((ff, fw))) = fc.read_tag() {
            match (ff, fw) {
                (3, WIRE_VARINT) => {
                    if let Ok(gt) = fc.read_varint() {
                        geom_type = gt;
                    }
                }
                (4, WIRE_LEN) => {
                    geom_bytes = fc.read_len_delimited().ok();
                }
                _ => {
                    drop(fc.skip_field(fw));
                }
            }
        }
        if geom_type != 3 {
            continue;
        }
        let Some(gb) = geom_bytes else {
            continue;
        };
        let Ok(commands) = decode_packed_varints(gb) else {
            continue;
        };
        let rings = crate::geometry::decode_mvt_polygon(&commands);
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
        if rings.len() == 1 && is_full_tile_rect(&rings[0]) {
            zs.full_tile_features += 1;
        }
    }
}

/// A closed 4-corner rectangle ring covering the full [0, 4096] extent
/// (i.e. a buffered full-tile ocean fill).
fn is_full_tile_rect(ring: &[(i32, i32)]) -> bool {
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
    #[allow(clippy::cast_possible_truncation)]
    let ext = crate::geometry::EXTENT as i32;
    all_corners && min_x <= 0 && min_y <= 0 && max_x >= ext && max_y >= ext
}

fn validate_mvt_geometry(data: &[u8], z: u8, x: u32) -> Result<(), String> {
    let mut tile_cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = tile_cursor.read_tag() {
        if field == 3 && wire_type == WIRE_LEN {
            let layer = tile_cursor
                .read_len_delimited()
                .map_err(|e| format!("layer decode failed: {e}"))?;
            validate_mvt_layer_geometry(layer, z, x)?;
        } else {
            tile_cursor
                .skip_field(wire_type)
                .map_err(|e| format!("tile parse failed: {e}"))?;
        }
    }
    Ok(())
}

fn validate_mvt_layer_geometry(layer: &[u8], z: u8, x: u32) -> Result<(), String> {
    let mut layer_cursor = Cursor::new(layer);
    let mut layer_name = None;
    let mut features = Vec::new();
    let mut keys = Vec::new();
    let mut values: Vec<&[u8]> = Vec::new();
    while let Ok(Some((field, wire_type))) = layer_cursor.read_tag() {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let bytes = layer_cursor
                    .read_len_delimited()
                    .map_err(|e| format!("layer name decode failed: {e}"))?;
                layer_name = Some(
                    std::str::from_utf8(bytes)
                        .map_err(|_| "layer name is not UTF-8".to_string())?,
                );
            }
            (2, WIRE_LEN) => {
                let feature = layer_cursor
                    .read_len_delimited()
                    .map_err(|e| format!("feature decode failed: {e}"))?;
                features.push(feature);
            }
            (3, WIRE_LEN) => {
                let bytes = layer_cursor
                    .read_len_delimited()
                    .map_err(|e| format!("layer key decode failed: {e}"))?;
                keys.push(
                    std::str::from_utf8(bytes).map_err(|_| "layer key is not UTF-8".to_string())?,
                );
            }
            // Retain the raw value bytes; decoding is deferred to the paint-order
            // check so it only ever runs on values a targeted layer references,
            // never on every value of every layer.
            (4, WIRE_LEN) => {
                let bytes = layer_cursor
                    .read_len_delimited()
                    .map_err(|e| format!("layer value decode failed: {e}"))?;
                values.push(bytes);
            }
            _ => layer_cursor
                .skip_field(wire_type)
                .map_err(|e| format!("layer parse failed: {e}"))?,
        }
    }
    let layer_name = layer_name.unwrap_or("");
    for (feat_idx, feature) in features.iter().enumerate() {
        validate_mvt_feature_geometry(feature, z, x)
            .map_err(|e| format!("[{layer_name}] feat {feat_idx}: {e}"))?;
    }
    validate_paint_order(layer_name, &features, &keys, &values, z)
        .map_err(|e| format!("[{layer_name}] {e}"))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum MvtValue<'a> {
    String(&'a str),
    Bool(bool),
    Other,
}

fn decode_mvt_value(value: &[u8]) -> Result<MvtValue<'_>, String> {
    let mut cursor = Cursor::new(value);
    let mut decoded = MvtValue::Other;
    let mut seen = false;
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|e| format!("value tag decode failed: {e}"))?
    {
        let next = match (field, wire_type) {
            (1, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("value string decode failed: {e}"))?;
                MvtValue::String(
                    std::str::from_utf8(bytes)
                        .map_err(|_| "value string is not UTF-8".to_string())?,
                )
            }
            (7, WIRE_VARINT) => MvtValue::Bool(
                cursor
                    .read_varint()
                    .map_err(|e| format!("value bool decode failed: {e}"))?
                    != 0,
            ),
            _ => {
                cursor
                    .skip_field(wire_type)
                    .map_err(|e| format!("value parse failed: {e}"))?;
                MvtValue::Other
            }
        };
        if seen {
            return Err("value message has multiple fields".to_string());
        }
        decoded = next;
        seen = true;
    }
    Ok(decoded)
}

fn validate_paint_order<'a>(
    layer: &str,
    features: &[&'a [u8]],
    keys: &[&'a str],
    values: &[&'a [u8]],
    z: u8,
) -> Result<(), String> {
    let target = matches!(layer, "land" | "streets" | "street_polygons");
    if !target {
        return Ok(());
    }
    let mut previous = None;
    for (feature_idx, feature) in features.iter().enumerate() {
        let attrs = decode_feature_attrs(feature, keys, values)
            .map_err(|e| format!("feat {feature_idx}: {e}"))?;
        let kind = attrs
            .kind
            .ok_or_else(|| format!("feat {feature_idx}: missing kind attribute"))?;
        let rank = match layer {
            "land" => {
                if !paint_order::is_known_land_kind(kind) {
                    return Err(format!("feat {feature_idx}: unknown land kind {kind:?}"));
                }
                paint_order::land_paint_rank(kind)
            }
            "streets" if z < 11 => {
                if !paint_order::is_known_street_kind(kind) {
                    return Err(format!("feat {feature_idx}: unknown street kind {kind:?}"));
                }
                paint_order::street_class_rank(kind)
            }
            "streets" | "street_polygons" => {
                if !paint_order::is_known_street_kind(kind) {
                    return Err(format!("feat {feature_idx}: unknown street kind {kind:?}"));
                }
                paint_order::street_paint_rank(kind, attrs.link, attrs.tunnel, attrs.bridge)
            }
            _ => unreachable!(),
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

fn decode_feature_attrs<'a>(
    feature: &'a [u8],
    keys: &[&'a str],
    values: &[&'a [u8]],
) -> Result<PaintAttrs<'a>, String> {
    let mut cursor = Cursor::new(feature);
    let mut attrs = PaintAttrs::default();
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|e| format!("feature tag decode failed: {e}"))?
    {
        if field != 2 || wire_type != WIRE_LEN {
            cursor
                .skip_field(wire_type)
                .map_err(|e| format!("feature parse failed: {e}"))?;
            continue;
        }
        let packed = cursor
            .read_len_delimited()
            .map_err(|e| format!("feature tags decode failed: {e}"))?;
        let pairs = decode_packed_varints(packed)?;
        if pairs.len() % 2 != 0 {
            return Err("feature tags have an odd index count".to_string());
        }
        for pair in pairs.as_chunks::<2>().0 {
            let key_idx = usize::try_from(pair[0])
                .map_err(|_| "tag key index overflows usize".to_string())?;
            let value_idx = usize::try_from(pair[1])
                .map_err(|_| "tag value index overflows usize".to_string())?;
            let key = keys
                .get(key_idx)
                .ok_or_else(|| format!("tag key index {key_idx} out of bounds"))?;
            let raw_value = values
                .get(value_idx)
                .ok_or_else(|| format!("tag value index {value_idx} out of bounds"))?;
            match *key {
                "kind" => match decode_mvt_value(raw_value)? {
                    MvtValue::String(kind) if attrs.kind.replace(kind).is_none() => {}
                    MvtValue::String(_) => return Err("duplicate kind attribute".to_string()),
                    _ => return Err("kind attribute is not a string".to_string()),
                },
                "link" | "tunnel" | "bridge" => {
                    let flag = match decode_mvt_value(raw_value)? {
                        MvtValue::Bool(flag) => flag,
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
    }
    Ok(attrs)
}

fn validate_mvt_feature_geometry(feature: &[u8], z: u8, x: u32) -> Result<(), String> {
    let mut geom_type: u64 = 0;
    let mut geom_bytes: Option<&[u8]> = None;
    let mut feature_cursor = Cursor::new(feature);
    while let Ok(Some((field, wire_type))) = feature_cursor.read_tag() {
        match (field, wire_type) {
            (3, WIRE_VARINT) => {
                geom_type = feature_cursor
                    .read_varint()
                    .map_err(|e| format!("feature type decode failed: {e}"))?;
            }
            (4, WIRE_LEN) => {
                geom_bytes = Some(
                    feature_cursor
                        .read_len_delimited()
                        .map_err(|e| format!("feature geometry decode failed: {e}"))?,
                );
            }
            _ => {
                feature_cursor
                    .skip_field(wire_type)
                    .map_err(|e| format!("feature parse failed: {e}"))?;
            }
        }
    }

    let Some(geom_bytes) = geom_bytes else {
        // Features without geometry are valid in MVT (e.g. metadata features).
        // Only flag polygon features that claim a type but lack geometry data.
        if geom_type != 0 {
            return Err(format!(
                "feature has geom_type={geom_type} but missing geometry data"
            ));
        }
        return Ok(());
    };
    let raw_byte_len = geom_bytes.len();
    let commands = decode_packed_varints(geom_bytes)?;
    validate_geometry_commands(&commands, geom_type, z, x, raw_byte_len)
}

fn decode_packed_varints(data: &[u8]) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let mut shift = 0u32;
        let mut value: u64 = 0;
        loop {
            if i >= data.len() {
                return Err("geometry varint truncated".to_string());
            }
            let b = data[i];
            i += 1;
            value |= u64::from(b & 0x7f) << shift;
            if (b & 0x80) == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return Err("geometry varint too long".to_string());
            }
        }
        let v = u32::try_from(value).map_err(|_| "geometry command > u32".to_string())?;
        out.push(v);
    }
    Ok(out)
}

fn zigzag_decode_u32(v: u32) -> i64 {
    i64::from(v >> 1) ^ -i64::from(v & 1)
}

/// Decode and validate a single point (dx,dy pair) from the command stream,
/// advancing the cursor and accumulating absolute coordinates.
fn consume_geometry_point(
    commands: &[u32],
    i: &mut usize,
    cx: &mut i64,
    cy: &mut i64,
    delta_limit: i64,
) -> Result<(), String> {
    if *i + 1 >= commands.len() {
        return Err("too few points in geometry".to_string());
    }
    let dx = zigzag_decode_u32(commands[*i]);
    let dy = zigzag_decode_u32(commands[*i + 1]);
    *i += 2;
    if dx.abs() > delta_limit || dy.abs() > delta_limit {
        return Err(format!(
            "suspicious geometry delta ({dx},{dy}) exceeds limit {delta_limit}"
        ));
    }
    *cx += dx;
    *cy += dy;
    if cx.abs() > MVT_COORD_ABS_LIMIT || cy.abs() > MVT_COORD_ABS_LIMIT {
        return Err(format!(
            "geometry coordinate ({cx},{cy}) exceeds absolute limit {MVT_COORD_ABS_LIMIT}"
        ));
    }
    Ok(())
}

fn validate_geometry_commands(
    commands: &[u32],
    geom_type: u64,
    z: u8,
    x: u32,
    raw_byte_len: usize,
) -> Result<(), String> {
    if commands.is_empty() {
        return Err("feature has empty geometry command stream".to_string());
    }
    let seam_tile = {
        let max_x = (1u32 << z).saturating_sub(1);
        x == 0 || x == max_x
    };
    let delta_limit = if seam_tile {
        MVT_DELTA_LIMIT_SEAM
    } else {
        MVT_DELTA_LIMIT
    };

    // Maximum valid count: vtzero uses raw_byte_len / 2 (each varint is ≥1 byte,
    // each point needs 2 varints). This is a generous upper bound that avoids
    // false positives on large but valid geometries.
    let max_count = u32::try_from(raw_byte_len / 2).unwrap_or(u32::MAX);

    let mut i = 0usize;
    let mut cx: i64 = 0;
    let mut cy: i64 = 0;
    let mut ring_points = 0usize;
    let mut expect_moveto = true; // first command must be MoveTo
    while i < commands.len() {
        let op = commands[i];
        i += 1;
        let id = op & 0x7;
        let count = op >> 3;
        if count == 0 {
            return Err("geometry command with zero repeat count".to_string());
        }
        match id {
            1 => {
                // MoveTo
                if count > max_count {
                    return Err(format!("MoveTo count too large ({count} > {max_count})"));
                }
                if geom_type == 3 && count != 1 {
                    return Err("polygon ring MoveTo count must be 1 (spec 4.3.4.4)".to_string());
                }
                if geom_type == 2 && count != 1 && !expect_moveto {
                    // Multi-linestring: subsequent MoveTo must also be 1
                    return Err("linestring MoveTo count must be 1 (spec 4.3.4.3)".to_string());
                }
                expect_moveto = false;
                for _ in 0..count {
                    consume_geometry_point(commands, &mut i, &mut cx, &mut cy, delta_limit)?;
                    if geom_type == 3 {
                        ring_points = 1;
                    }
                }
            }
            2 => {
                // LineTo
                if count > max_count {
                    return Err(format!("LineTo count too large ({count} > {max_count})"));
                }
                for _ in 0..count {
                    consume_geometry_point(commands, &mut i, &mut cx, &mut cy, delta_limit)?;
                    if geom_type == 3 {
                        ring_points += 1;
                    }
                }
            }
            7 => {
                // ClosePath
                if geom_type != 3 {
                    return Err("ClosePath in non-polygon geometry".to_string());
                }
                if count != 1 {
                    return Err("ClosePath command count is not 1 (spec 4.3.3.3)".to_string());
                }
                if ring_points < 3 {
                    return Err(format!(
                        "polygon ring has too few points ({ring_points}, need ≥3)"
                    ));
                }
                ring_points = 0;
            }
            _ => return Err(format!("unknown geometry command id {id}")),
        }
    }

    if geom_type == 3 && ring_points != 0 {
        return Err("polygon ring missing ClosePath".to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate ocean polygon rings for self-intersections.
/// Decodes MVT protobuf → finds ocean layer → decodes polygon geometry → checks each ring.
fn validate_ocean_rings(data: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut tile_cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = tile_cursor.read_tag() {
        if field == 3 && wire_type == WIRE_LEN {
            if let Ok(layer_data) = tile_cursor.read_len_delimited() {
                validate_ocean_layer_rings(layer_data, &mut problems);
            }
        } else if tile_cursor.skip_field(wire_type).is_err() {
            break;
        }
    }
    problems
}

fn validate_ocean_layer_rings(layer_data: &[u8], problems: &mut Vec<String>) {
    let mut name = String::new();
    let mut feature_blobs: Vec<&[u8]> = Vec::new();
    let mut cursor = Cursor::new(layer_data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if wire_type == WIRE_LEN {
            if let Ok(sub) = cursor.read_len_delimited() {
                match field {
                    1 => name = String::from_utf8_lossy(sub).to_string(),
                    2 => feature_blobs.push(sub),
                    _ => {}
                }
            }
        } else if cursor.skip_field(wire_type).is_err() {
            break;
        }
    }
    if name != "ocean" {
        return;
    }

    for (fi, feat_data) in feature_blobs.iter().enumerate() {
        let mut geom_type: u64 = 0;
        let mut geom_bytes: Option<&[u8]> = None;
        let mut fc = Cursor::new(feat_data);
        while let Ok(Some((ff, fw))) = fc.read_tag() {
            match (ff, fw) {
                (3, WIRE_VARINT) => {
                    if let Ok(gt) = fc.read_varint() {
                        geom_type = gt;
                    }
                }
                (4, WIRE_LEN) => {
                    geom_bytes = fc.read_len_delimited().ok();
                }
                _ => {
                    drop(fc.skip_field(fw));
                }
            }
        }
        if geom_type != 3 {
            continue;
        } // polygon only
        let Some(gb) = geom_bytes else {
            continue;
        };

        // Decode packed varints to u32 commands
        let Ok(commands) = decode_packed_varints(gb) else {
            continue;
        };
        let rings = crate::geometry::decode_mvt_polygon(&commands);
        for (ri, ring) in rings.iter().enumerate() {
            if ring.len() >= 4 && !crate::geometry::ring_is_simple(ring) {
                problems.push(format!(
                    "ocean feat {fi} ring {ri} is self-intersecting ({} verts)",
                    ring.len()
                ));
            }
        }
    }
}

fn check_section_bounds(
    name: &str,
    offset: u64,
    length: u64,
    file_size: u64,
) -> Result<(), VerifyError> {
    if let Some(end) = offset.checked_add(length) {
        if end > file_size {
            return Err(VerifyError::Container(format!(
                "{name} section exceeds file bounds: offset {offset} + length {length} > file size {file_size}"
            )));
        }
    } else {
        return Err(VerifyError::Container(format!(
            "{name} section offset+length overflows u64"
        )));
    }
    Ok(())
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
    use crate::mvt::{Feature, GeomType, LayerBuilder, Value, encode_tile};
    use crate::pmtiles_writer::{PmtilesConfig, PmtilesWriter, xy_to_tile_id};
    use flate2::Compression;
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
        let err = validate_geometry_commands(&[cmd(4, 1)], 2, 10, 1, 1000)
            .expect_err("unknown command id should fail");
        assert!(err.contains("unknown geometry command id 4"));
    }

    #[test]
    fn geometry_rejects_zero_repeat_count() {
        let err = validate_geometry_commands(&[cmd(1, 0)], 2, 10, 1, 1000)
            .expect_err("zero repeat count should fail");
        assert!(err.contains("zero repeat count"));
    }

    #[test]
    fn geometry_rejects_polygon_moveto_count_not_one() {
        let commands = vec![cmd(1, 2), zz(0), zz(0), zz(1), zz(1)];
        let err = validate_geometry_commands(&commands, 3, 10, 1, 1000)
            .expect_err("polygon MoveTo count != 1 should fail");
        assert!(err.contains("MoveTo count must be 1"));
    }

    #[test]
    fn geometry_rejects_closepath_in_non_polygon() {
        let commands = vec![cmd(1, 1), zz(0), zz(0), cmd(7, 1)];
        let err = validate_geometry_commands(&commands, 2, 10, 1, 1000)
            .expect_err("ClosePath in non-polygon should fail");
        assert!(err.contains("ClosePath in non-polygon"));
    }

    #[test]
    fn geometry_rejects_polygon_closepath_without_enough_points() {
        let commands = vec![cmd(1, 1), zz(0), zz(0), cmd(7, 1)];
        let err = validate_geometry_commands(&commands, 3, 10, 1, 1000)
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
        let err = validate_geometry_commands(&commands, 3, 10, 1, 1000)
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
        let err = validate_geometry_commands(&commands, 2, 10, 1, 1000)
            .expect_err("absolute coordinate limit should fail");
        assert!(err.contains("exceeds absolute limit"));
    }

    #[test]
    fn geometry_seam_tile_uses_stricter_delta_limit() {
        // 20k delta is > seam limit (16384) but < non-seam limit (65536).
        let commands = vec![cmd(1, 1), zz(0), zz(0), cmd(2, 1), zz(20_000), zz(0)];

        // Non-seam tile passes.
        validate_geometry_commands(&commands, 2, 4, 1, 1000)
            .expect("non-seam tile should allow 20k delta");

        // Seam tile fails.
        let err = validate_geometry_commands(&commands, 2, 4, 0, 1000)
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

        let mut writer = PmtilesWriter::new(PmtilesConfig {
            min_zoom: 2,
            max_zoom: 2,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 2),
        });
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

        let mut writer = PmtilesWriter::new(PmtilesConfig {
            min_zoom: 2,
            max_zoom: 2,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 2),
        });
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
        let err = validate_mvt_geometry(&tile, 11, 1)
            .expect_err("forest before residential must fail paint order validation");
        assert!(err.contains("paint rank 0 follows 5"));
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
        let err = validate_mvt_geometry(&tile, 11, 1).expect_err("mistyped kind must fail closed");
        assert!(err.contains("kind attribute is not a string"));
    }
}
