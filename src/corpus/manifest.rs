use serde::Deserialize;
use std::collections::BTreeSet;
use std::io;
use std::path::Path;

#[derive(Deserialize)]
pub struct ManifestFile {
    #[serde(default)]
    pub tile: Vec<ManifestTile>,
}
#[derive(Deserialize)]
pub struct ManifestTile {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    #[serde(default)]
    pub layers: Vec<String>,
    #[serde(default)]
    pub note: String,
}
pub fn load(path: &Path) -> io::Result<ManifestFile> {
    let text = std::fs::read_to_string(path)?;
    let out: ManifestFile = toml::from_str(&text).map_err(io::Error::other)?;
    let mut tuples = BTreeSet::new();
    let mut names = BTreeSet::new();
    for tile in &out.tile {
        if tile.z > 14 || tile.x >= (1u32 << tile.z) || tile.y >= (1u32 << tile.z) {
            return Err(io::Error::other("manifest tile out of range"));
        }
        if !tile.layers.windows(2).all(|v| v[0] < v[1])
            || tile.layers.iter().any(|x| {
                x.is_empty()
                    || !x
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
            })
        {
            return Err(io::Error::other(
                "manifest layers must be strictly ascending [a-z0-9_]+",
            ));
        }
        let key = (tile.z, tile.x, tile.y, tile.layers.clone());
        if !tuples.insert(key) || !names.insert(file_name(tile)) {
            return Err(io::Error::other("duplicate manifest render target"));
        }
    }
    Ok(out)
}
pub fn file_name(tile: &ManifestTile) -> String {
    let base = format!("z{}-x{}-y{}", tile.z, tile.x, tile.y);
    if tile.layers.is_empty() {
        format!("{base}.svg")
    } else {
        format!("{base}-{}.svg", tile.layers.join("+"))
    }
}
