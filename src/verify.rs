//! PMTiles output verification.
//!
//! Validates a PMTiles archive end-to-end: container integrity, metadata schema,
//! tile decompression, MVT payload structure, and layer coverage.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::Path;

use crate::pmtiles_reader::{self, PmtilesReader};
use crate::pmtiles_writer::tile_id_to_zxy;
use crate::shortbread::Layer;
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
    pub tile_errors: Vec<String>,
    pub layers_observed: HashSet<String>,
    pub layers_declared: Vec<String>,
    pub passed: bool,
}

impl VerifyReport {
    /// Print a human-readable summary to stdout.
    pub fn print_summary(&self) {
        if self.passed {
            println!("PASS  ({} tiles checked)", self.tiles_checked);
        } else {
            println!("FAIL  ({} tiles checked)", self.tiles_checked);
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
        let shortbread_names: HashSet<&str> =
            Layer::ALL.iter().map(|l| l.name()).collect();

        let declared_set: HashSet<&str> =
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
            println!(
                "Warning: declared layers not in Shortbread schema: {non_shortbread:?}"
            );
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
    check_section_bounds("root_dir", reader.root_dir_offset(), reader.root_dir_length(), file_size)?;
    check_section_bounds("metadata", reader.metadata_offset(), reader.metadata_length(), file_size)?;
    check_section_bounds("leaf_dirs", reader.leaf_dirs_offset(), reader.leaf_dirs_length(), file_size)?;
    check_section_bounds("tile_data", reader.data_offset(), reader.data_length(), file_size)?;

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
    let entries = reader.read_all_entries()?;

    let mut tiles_checked: u64 = 0;
    let mut tile_errors: Vec<String> = Vec::new();
    let mut layers_observed: HashSet<String> = HashSet::new();

    for entry in &entries {
        if tile_errors.len() >= MAX_TILE_ERRORS {
            break;
        }

        let (z, x, y) = tile_id_to_zxy(entry.tile_id);

        // Read and decompress.
        let decompressed = match reader.read_tile(entry) {
            Ok(data) => data,
            Err(e) => {
                tile_errors.push(format!("z{z}/{x}/{y}: decompression failed: {e}"));
                tiles_checked += 1;
                continue;
            }
        };

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
        if tile_errors.len() < MAX_TILE_ERRORS
            && let Err(msg) = validate_mvt_geometry(&decompressed, z, x)
        {
            tile_errors.push(format!("z{z}/{x}/{y}: {msg}"));
        }

        tiles_checked += 1;
    }

    let passed = tile_errors.is_empty();

    Ok(VerifyReport {
        tiles_checked,
        tile_errors,
        layers_observed,
        layers_declared,
        passed,
    })
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
    while let Ok(Some((field, wire_type))) = layer_cursor.read_tag() {
        if field == 2 && wire_type == WIRE_LEN {
            let feature = layer_cursor
                .read_len_delimited()
                .map_err(|e| format!("feature decode failed: {e}"))?;
            validate_mvt_feature_geometry(feature, z, x)?;
        } else {
            layer_cursor
                .skip_field(wire_type)
                .map_err(|e| format!("layer parse failed: {e}"))?;
        }
    }
    Ok(())
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
        return Err("feature missing geometry".to_string());
    };
    let commands = decode_packed_varints(geom_bytes)?;
    validate_geometry_commands(&commands, geom_type, z, x)
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

fn validate_geometry_commands(
    commands: &[u32],
    geom_type: u64,
    z: u8,
    x: u32,
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

    let mut i = 0usize;
    let mut cx: i64 = 0;
    let mut cy: i64 = 0;
    let mut ring_points = 0usize;
    while i < commands.len() {
        let op = commands[i];
        i += 1;
        let id = op & 0x7;
        let count = op >> 3;
        if count == 0 {
            return Err("geometry command with zero repeat count".to_string());
        }
        match id {
            1 | 2 => {
                if geom_type == 3 && id == 1 && count != 1 {
                    return Err("polygon ring MoveTo count must be 1".to_string());
                }
                for n in 0..count {
                    if i + 1 >= commands.len() {
                        return Err("geometry command missing parameters".to_string());
                    }
                    let dx = zigzag_decode_u32(commands[i]);
                    let dy = zigzag_decode_u32(commands[i + 1]);
                    i += 2;
                    if dx.abs() > delta_limit || dy.abs() > delta_limit {
                        return Err(format!(
                            "suspicious geometry delta ({dx},{dy}) exceeds limit {delta_limit}"
                        ));
                    }
                    cx += dx;
                    cy += dy;
                    if cx.abs() > MVT_COORD_ABS_LIMIT || cy.abs() > MVT_COORD_ABS_LIMIT {
                        return Err(format!(
                            "geometry coordinate ({cx},{cy}) exceeds absolute limit {MVT_COORD_ABS_LIMIT}"
                        ));
                    }
                    if geom_type == 3 {
                        if id == 1 && n == 0 {
                            ring_points = 1;
                        } else if id == 2 {
                            ring_points += 1;
                        }
                    }
                }
            }
            7 => {
                if geom_type != 3 {
                    return Err("ClosePath in non-polygon geometry".to_string());
                }
                for _ in 0..count {
                    if ring_points < 3 {
                        return Err("polygon ClosePath without enough ring points".to_string());
                    }
                    ring_points = 0;
                }
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

fn extract_declared_layers(
    parsed: &serde_json::Value,
) -> Result<Vec<String>, VerifyError> {
    let vector_layers = parsed
        .get("vector_layers")
        .ok_or_else(|| VerifyError::Metadata("missing 'vector_layers' key".to_string()))?
        .as_array()
        .ok_or_else(|| {
            VerifyError::Metadata("'vector_layers' is not an array".to_string())
        })?;

    let mut names = Vec::with_capacity(vector_layers.len());
    for layer in vector_layers {
        let id = layer
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                VerifyError::Metadata(
                    "vector_layers entry missing 'id' string".to_string(),
                )
            })?;
        names.push(id.to_string());
    }
    Ok(names)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
        let err = validate_geometry_commands(&[cmd(4, 1)], 2, 10, 1)
            .expect_err("unknown command id should fail");
        assert!(err.contains("unknown geometry command id 4"));
    }

    #[test]
    fn geometry_rejects_zero_repeat_count() {
        let err = validate_geometry_commands(&[cmd(1, 0)], 2, 10, 1)
            .expect_err("zero repeat count should fail");
        assert!(err.contains("zero repeat count"));
    }

    #[test]
    fn geometry_rejects_polygon_moveto_count_not_one() {
        let commands = vec![
            cmd(1, 2),
            zz(0),
            zz(0),
            zz(1),
            zz(1),
        ];
        let err = validate_geometry_commands(&commands, 3, 10, 1)
            .expect_err("polygon MoveTo count != 1 should fail");
        assert!(err.contains("MoveTo count must be 1"));
    }

    #[test]
    fn geometry_rejects_closepath_in_non_polygon() {
        let commands = vec![
            cmd(1, 1),
            zz(0),
            zz(0),
            cmd(7, 1),
        ];
        let err = validate_geometry_commands(&commands, 2, 10, 1)
            .expect_err("ClosePath in non-polygon should fail");
        assert!(err.contains("ClosePath in non-polygon"));
    }

    #[test]
    fn geometry_rejects_polygon_closepath_without_enough_points() {
        let commands = vec![
            cmd(1, 1),
            zz(0),
            zz(0),
            cmd(7, 1),
        ];
        let err = validate_geometry_commands(&commands, 3, 10, 1)
            .expect_err("polygon ClosePath without enough points should fail");
        assert!(err.contains("without enough ring points"));
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
        let err = validate_geometry_commands(&commands, 3, 10, 1)
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
        let err = validate_geometry_commands(&commands, 2, 10, 1)
            .expect_err("absolute coordinate limit should fail");
        assert!(err.contains("exceeds absolute limit"));
    }

    #[test]
    fn geometry_seam_tile_uses_stricter_delta_limit() {
        // 20k delta is > seam limit (16384) but < non-seam limit (65536).
        let commands = vec![
            cmd(1, 1),
            zz(0),
            zz(0),
            cmd(2, 1),
            zz(20_000),
            zz(0),
        ];

        // Non-seam tile passes.
        validate_geometry_commands(&commands, 2, 4, 1)
            .expect("non-seam tile should allow 20k delta");

        // Seam tile fails.
        let err = validate_geometry_commands(&commands, 2, 4, 0)
            .expect_err("seam tile should reject 20k delta");
        assert!(err.contains("suspicious geometry delta"));
        assert!(err.contains(&MVT_DELTA_LIMIT_SEAM.to_string()));
    }
}
