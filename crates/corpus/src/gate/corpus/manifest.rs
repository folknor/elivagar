//! The hard-tiles ledger: `corpus/<dataset>/manifest.toml`. Curated,
//! append-only, review-bearing baseline state (`brokkr.md`) - a file in the
//! corpus dir, not `brokkr.toml`. Ported verbatim so the committed manifests in
//! elivagar's repo parse unchanged.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use serde::Deserialize;

use super::super::constants::MAX_MANIFEST_ZOOM;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    #[serde(default)]
    pub tile: Vec<ManifestTile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestTile {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    #[serde(default)]
    pub layers: Vec<String>,
    /// Curator's rationale for the tile, retained in the ledger for review; not
    /// consumed by the gate.
    #[serde(default)]
    #[allow(dead_code)]
    pub note: String,
}

/// Every malformation is `InvalidData`, never `Other`: the gate folds
/// `InvalidData` into the baseline verdict (exit 3), and a malformed committed
/// manifest is baseline damage, not an operational failure.
pub fn load(path: &Path) -> io::Result<ManifestFile> {
    let invalid = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, msg.to_owned());
    let text = std::fs::read_to_string(path)?;
    let out: ManifestFile =
        toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut tuples = BTreeSet::new();
    let mut names = BTreeSet::new();
    for tile in &out.tile {
        if tile.z > MAX_MANIFEST_ZOOM || tile.x >= (1u32 << tile.z) || tile.y >= (1u32 << tile.z) {
            return Err(invalid("manifest tile out of range"));
        }
        if !tile.layers.windows(2).all(|v| v[0] < v[1])
            || tile.layers.iter().any(|x| {
                x.is_empty()
                    || !x
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
            })
        {
            return Err(invalid(
                "manifest layers must be strictly ascending [a-z0-9_]+",
            ));
        }
        let key = (tile.z, tile.x, tile.y, tile.layers.clone());
        if !tuples.insert(key) || !names.insert(file_name(tile)) {
            return Err(invalid("duplicate manifest render target"));
        }
    }
    Ok(out)
}

#[must_use]
pub fn file_name(tile: &ManifestTile) -> String {
    super::layout::tile_svg_file_name(tile.z, tile.x, tile.y, &tile.layers)
}
