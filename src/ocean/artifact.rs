//! Admission of the durable world-ocean artifact and the view assembly reads.
//!
//! The artifact serves one tile contract: MVT + gzip over the whole profile
//! zoom range, at the compression level it was built with. A run that names
//! it is admitted once, at run entry and before phase12, by
//! [`OceanArtifact::admit`]; a run it cannot serve is an error, never a
//! silent fallback to computed ocean. The admitted view's directory is read
//! only when assembly binds it to the extract's band
//! ([`OceanArtifact::into_tiles`]), so the world directory (millions of runs)
//! is not held through phase12.

use super::OceanArtifactKey;
use super::band::OceanBand;
use crate::archive::{Archive, BlobRef, MapSource, TileRun, TileRuns};
use crate::shortbread::Layer;

/// The artifact is mapped, not read, so assembly borrows its tile blobs
/// without copying them.
type ArchiveView = Archive<MapSource>;

fn open_view(path: &std::path::Path) -> std::io::Result<ArchiveView> {
    // SAFETY: this assumes nothing modifies or truncates the artifact file
    // while the run holds it - the artifact is built once per shapefile
    // release and only replaced between runs. Nothing enforces that; a
    // concurrent rewrite of the file is undefined behavior here, which is the
    // price of the zero-copy splice. Every other reader goes through the
    // positional-read source instead.
    unsafe { Archive::open_mapped(path) }
}

/// First zoom of the artifact contract.
pub(crate) const ARTIFACT_MIN_ZOOM: u8 = 0;
/// Last zoom of the artifact contract.
pub(crate) const ARTIFACT_MAX_ZOOM: u8 = crate::tiling::MAX_ZOOM;

/// Whether a run window is the one the artifact serves.
pub(crate) fn artifact_serves_window(min_zoom: u8, max_zoom: u8) -> bool {
    min_zoom == ARTIFACT_MIN_ZOOM && max_zoom == ARTIFACT_MAX_ZOOM
}

fn declared_key_of(view: &ArchiveView) -> std::io::Result<OceanArtifactKey> {
    let metadata: serde_json::Value = serde_json::from_str(&view.metadata()?)
        .map_err(|e| std::io::Error::other(format!("invalid ocean artifact metadata: {e}")))?;
    OceanArtifactKey::from_json(
        metadata.get("ocean_artifact").ok_or_else(|| {
            std::io::Error::other("ocean artifact missing ocean_artifact metadata")
        })?,
    )
}

/// The key an artifact file declares, without judging whether it can serve
/// anything. Retention of an outgoing artifact reads this, and must keep
/// working for artifacts the current inputs would refuse.
pub(crate) fn declared_key(path: &std::path::Path) -> std::io::Result<OceanArtifactKey> {
    // Identity only: no readability check, so retention can still read the
    // key of an artifact the current run could not serve.
    declared_key_of(&open_view(path)?)
}

/// An artifact admitted for this run: every check that does not need the
/// directory has passed, and the mapped file is retained so assembly reads
/// the same inode that was admitted.
pub(crate) struct OceanArtifact {
    view: ArchiveView,
    key: OceanArtifactKey,
}

impl OceanArtifact {
    /// Admit `path` for a run whose inputs hash to `expected()`.
    ///
    /// The format half of the contract - MVT + gzip, which the raw splice and
    /// run-copy paths rely on - and the section layout are checked first. The
    /// declared compression level is compared before `expected` is
    /// computed, because computing it hashes the shapefiles and a level
    /// mismatch deserves its own message.
    pub(crate) fn admit(
        path: &std::path::Path,
        compression_level: u32,
        expected: impl FnOnce() -> std::io::Result<OceanArtifactKey>,
    ) -> std::io::Result<Self> {
        if !path.exists() {
            return Err(std::io::Error::other(format!(
                "configured ocean artifact does not exist: {}",
                path.display()
            )));
        }
        let view = open_view(path)?;
        view.tile_decoder()?;
        view.check_layout()?;
        let declared = declared_key_of(&view)?;
        if declared.compression_level != compression_level {
            return Err(std::io::Error::other(format!(
                "ocean artifact {} was built at compression level {} but this run uses {}. Rebuild the artifact, match --compression-level, or drop the artifact from --ocean.",
                path.display(),
                declared.compression_level,
                compression_level,
            )));
        }
        let header = view.header();
        if header.min_zoom != ARTIFACT_MIN_ZOOM || header.max_zoom != ARTIFACT_MAX_ZOOM {
            return Err(std::io::Error::other("ocean artifact must cover z0-14"));
        }
        // A coverage inequality, not equality with a rounded world constant:
        // the artifact writes -85.05/85.05, inside the Mercator limit.
        let bounds = header.bounds;
        if bounds.west > -1_800_000_000
            || bounds.south > -850_500_000
            || bounds.east < 1_800_000_000
            || bounds.north < 850_500_000
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
        if declared != expected()? {
            return Err(std::io::Error::other(
                "ocean artifact key does not match current shapefile inputs",
            ));
        }
        Ok(Self {
            view,
            key: declared,
        })
    }

    pub(crate) fn key(&self) -> &OceanArtifactKey {
        &self.key
    }

    /// Read the directory and bind it to the extract's band for assembly.
    pub(crate) fn into_tiles(self, band: OceanBand) -> std::io::Result<OceanTiles> {
        let runs = self.view.read_all_runs()?;
        // Exclusive end: the first id past the top zoom is outside the
        // artifact contract.
        let profile_end = crate::tiling::PROFILE_ID_END;
        if runs.as_slice().iter().any(|run| run.end() > profile_end) {
            return Err(std::io::Error::other(
                "ocean artifact directory run lies outside z0-14 tile space",
            ));
        }
        Ok(OceanTiles {
            view: self.view,
            runs,
            band,
        })
    }
}

/// Bound, mmap-backed ocean artifact shared by assembly workers, with the
/// band that decides which of its tiles this run serves.
pub(crate) struct OceanTiles {
    view: ArchiveView,
    runs: TileRuns,
    band: OceanBand,
}

impl OceanTiles {
    pub(crate) fn band(&self) -> &OceanBand {
        &self.band
    }

    pub(crate) fn run_count(&self) -> usize {
        self.runs.len()
    }

    pub(crate) fn runs_in(&self, start: u64, end: u64) -> &[TileRun] {
        self.runs.intersecting(start, end)
    }

    pub(crate) fn run_covering(&self, tile_id: u64) -> Option<TileRun> {
        self.runs.covering(tile_id).copied()
    }

    pub(crate) fn raw_blob(&self, blob: BlobRef) -> std::io::Result<std::borrow::Cow<'_, [u8]>> {
        self.view.raw_blob(blob)
    }
}
