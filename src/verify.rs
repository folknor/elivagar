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

/// Maximum number of tile-level errors before aborting traversal.
const MAX_TILE_ERRORS: usize = 100;

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
