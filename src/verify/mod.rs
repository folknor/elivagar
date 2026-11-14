//! PMTiles output verification.
//!
//! Validates a PMTiles archive end-to-end: container integrity, metadata schema,
//! tile decompression, MVT payload structure, and layer coverage.
//!
//! This module is the coordinator: container checks, run traversal, the
//! per-class outcome cache, the error cap and the report. Each tile is parsed
//! once strictly (`tile`) and the checks read that parse as plain functions -
//! `geometry` and `paint_order` per layer, `ocean` per tile. They are not a
//! trait-object list on purpose: the set is fixed and always runs, nothing
//! selects among them at runtime, and their outputs differ in kind (first
//! error over an interleaved layer walk, every ring problem, a statistics
//! accumulator), so what they share is the ordering and cap policy here, not
//! an interface.

mod geometry;
mod ocean;
mod paint_order;
mod tile;

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::Path;

use crate::archive::Archive;
use crate::shortbread::Layer;
use crate::tiling::{ARCHIVE_MAX_ZOOM, TileId};
use geometry::MVT_EXTENT;
use tile::{CheckedLayer, parse_checked_tile};

/// One slot per archive zoom.
pub const ARCHIVE_ZOOM_COUNT: usize = ARCHIVE_MAX_ZOOM as usize + 1;

/// Coordinates of an id inside a directory run. `read_all_runs` refuses runs
/// reaching past the last addressable tile, so every such id is an address.
fn tile_coord(tile_id: u64) -> (u8, u32, u32) {
    TileId::new(tile_id)
        .expect("checked directory runs hold addressable ids")
        .coord()
}

/// Maximum number of tile-level errors before aborting traversal.
const MAX_TILE_ERRORS: usize = 100;

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
    /// Every finding for the payload in report order: structure, then the
    /// first geometry or paint-order violation, then each ocean ring problem.
    findings: Vec<String>,
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
            // The cap truncates findings only; the tile is still accounted
            // for (stats, tiles_checked) and traversal stops before the next.
            let room = MAX_TILE_ERRORS.saturating_sub(tile_errors.len());
            tile_errors.extend(
                outcome
                    .findings
                    .iter()
                    .take(room)
                    .map(|problem| format!("z{z}/{x}/{y}: {problem}")),
            );

            if let (Some(stats), Some(delta)) = (stats.as_mut(), outcome.geometry_stats.as_ref()) {
                ocean::add_zoom_stats(stats.per_zoom.entry(z).or_default(), delta);
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
        findings: Vec::new(),
        geometry_stats: None,
    };
    // One strict parse feeds every check below. A tile that does not parse
    // reports only that: the checks have nothing well formed to read.
    let tile = match parse_checked_tile(data) {
        Ok(tile) => tile,
        Err(error) => {
            outcome.findings.push(format!("MVT decode failed: {error}"));
            return outcome;
        }
    };
    if tile.is_empty() {
        outcome.findings.push("no MVT layers".to_string());
    }
    for layer in &tile {
        if layer.message.name.is_empty() {
            outcome.findings.push("empty layer name".to_string());
        }
        outcome.layer_names.push(layer.message.name.to_string());
    }

    if let Err(error) = validate_tile_layers(&tile, class) {
        outcome.findings.push(error);
    }
    ocean::check_rings(&tile, &mut outcome.findings);
    if collect_stats {
        let mut collected = GeometryStats::default();
        ocean::collect_stats(&tile, class.z, &mut collected);
        outcome.geometry_stats = collected.per_zoom.remove(&class.z);
    }
    outcome
}

/// Geometry, extent and paint order, one layer at a time in wire order: the
/// first violation of either check in that walk is the tile's one finding.
/// Running the two checks as whole-tile passes instead would name a later
/// layer's geometry fault ahead of an earlier layer's order fault.
fn validate_tile_layers(tile: &[CheckedLayer<'_>], class: ValidationClass) -> Result<(), String> {
    for layer in tile {
        geometry::check_layer(layer, class)?;
        paint_order::check_layer(layer).map_err(|e| format!("[{}] {e}", layer.message.name))?;
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
    use super::geometry::MVT_DELTA_LIMIT_SEAM;
    use super::*;
    use crate::archive::{LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter};
    use crate::mvt::decode::{self, Words};
    use crate::mvt::{Feature, GeomType, LayerBuilder, Value, encode_tile};
    use flate2::Compression;

    fn validate_geometry_commands(
        commands: &[u32],
        geom_type: u8,
        class: ValidationClass,
        raw_byte_len: usize,
    ) -> Result<(), String> {
        geometry::validate_geometry(Words::slice(commands), geom_type, class, raw_byte_len)
    }

    fn validate_mvt_geometry(data: &[u8], class: ValidationClass) -> Result<(), String> {
        validate_tile_layers(&parse_checked_tile(data)?, class)
    }

    fn is_full_tile_rect(ring: &[(i32, i32)]) -> bool {
        ocean::is_full_tile_rect_with_extent(ring, MVT_EXTENT)
    }

    fn structure_findings(data: &[u8], class: ValidationClass) -> Vec<String> {
        build_tile_outcome(data, class, false).findings
    }

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
    fn layer_walk_reports_first_fault_across_both_checks() {
        // An order fault in the first layer and a geometry fault in the
        // second: the per-layer walk names the first layer's fault only.
        let mut land = LayerBuilder::new("land");
        let key = land.intern_key("kind");
        for kind in ["forest", "residential"] {
            let value = land.intern_value(Value::String(kind.to_string()));
            land.add_feature(Feature {
                id: None,
                geom_type: GeomType::Point,
                geometry: vec![9, 0, 0],
                tags: vec![(key, value)],
            });
        }
        let mut lines = LayerBuilder::new("water_lines");
        lines.add_feature(Feature {
            id: None,
            geom_type: GeomType::LineString,
            geometry: vec![cmd(1, 1), zz(0), zz(0), cmd(2, 1), zz(70_000), zz(0)],
            tags: Vec::new(),
        });
        let findings = structure_findings(
            &encode_tile(&[&land, &lines]),
            ValidationClass::for_tile(11, 1),
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].starts_with("[land] feat 1: paint rank"),
            "{findings:?}"
        );
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
        assert!(structure_findings(&clean, class).is_empty());
        // A field MVT does not name and a truncated tag both fail the tile.
        let mut foreign = clean.clone();
        foreign.extend_from_slice(&[8, 1]);
        let mut truncated = clean;
        truncated.push(0x80);
        for tile in [foreign, truncated] {
            let findings = structure_findings(&tile, class);
            assert_eq!(findings.len(), 1, "{tile:?}");
            assert!(findings[0].starts_with("MVT decode failed"));
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
