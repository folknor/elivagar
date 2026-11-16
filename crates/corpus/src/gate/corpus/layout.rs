//! Names and paths inside one committed SVG corpus dataset. The command
//! dispatch selects the dataset directory from the build root; this type owns
//! the files within that already-resolved directory.

use std::path::{Path, PathBuf};

pub(crate) const DIGEST_FILE: &str = "digest";
pub(crate) const CONTRACT_FILE: &str = "contract.json";
pub(crate) const MANIFEST_FILE: &str = "manifest.toml";
pub(crate) const LEAVES_FILE: &str = "leaves";
pub(crate) const TILES_DIR: &str = "tiles";

#[derive(Clone, Copy)]
pub(crate) struct CorpusLayout<'a> {
    root: &'a Path,
}

impl<'a> CorpusLayout<'a> {
    pub(crate) fn new(root: &'a Path) -> Self {
        Self { root }
    }

    pub(crate) fn digest(self) -> PathBuf {
        self.root.join(DIGEST_FILE)
    }

    pub(crate) fn contract(self) -> PathBuf {
        self.root.join(CONTRACT_FILE)
    }

    pub(crate) fn manifest(self) -> PathBuf {
        self.root.join(MANIFEST_FILE)
    }

    pub(crate) fn leaves(self) -> PathBuf {
        self.root.join(LEAVES_FILE)
    }

    pub(crate) fn tiles(self) -> PathBuf {
        self.root.join(TILES_DIR)
    }
}

/// The committed SVG name for a tile and optional layer-filtered render.
#[must_use]
pub(crate) fn tile_svg_file_name(z: u8, x: u32, y: u32, layers: &[String]) -> String {
    let base = format!("z{z}-x{x}-y{y}");
    if layers.is_empty() {
        format!("{base}.svg")
    } else {
        format!("{base}-{}.svg", layers.join("+"))
    }
}
