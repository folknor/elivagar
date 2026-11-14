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
use crate::pmtiles_reader::{ArchiveView, RawDirEntry};
use crate::shortbread::Layer;

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
    declared_key_of(&ArchiveView::open(path)?)
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
    /// `ArchiveView::open` refuses any header tile type or compression other
    /// than MVT + gzip, so the format half of the contract is enforced there.
    /// The declared compression level is compared before `expected` is
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
        let view = ArchiveView::open(path)?;
        let declared = declared_key_of(&view)?;
        if declared.compression_level != compression_level {
            return Err(std::io::Error::other(format!(
                "ocean artifact {} was built at compression level {} but this run uses {}. Rebuild the artifact, match --compression-level, or drop the artifact from --ocean.",
                path.display(),
                declared.compression_level,
                compression_level,
            )));
        }
        if view.min_zoom() != ARTIFACT_MIN_ZOOM || view.max_zoom() != ARTIFACT_MAX_ZOOM {
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
        if runs.iter().any(|run| {
            run.tile_id >= profile_end
                || run
                    .tile_id
                    .checked_add(u64::from(run.run_length))
                    .is_none_or(|end| end > profile_end)
        }) {
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
    runs: Vec<RawDirEntry>,
    band: OceanBand,
}

impl OceanTiles {
    pub(crate) fn band(&self) -> &OceanBand {
        &self.band
    }

    pub(crate) fn run_count(&self) -> usize {
        self.runs.len()
    }

    pub(crate) fn runs_in(&self, start: u64, end: u64) -> &[RawDirEntry] {
        let first = self
            .runs
            .partition_point(|run| run.tile_id.saturating_add(u64::from(run.run_length)) <= start);
        let last = self.runs.partition_point(|run| run.tile_id < end);
        &self.runs[first..last]
    }

    pub(crate) fn run_covering(&self, tile_id: u64) -> Option<RawDirEntry> {
        let index = self.runs.partition_point(|run| run.tile_id <= tile_id);
        index
            .checked_sub(1)
            .and_then(|index| self.runs.get(index))
            .copied()
            .filter(|run| tile_id < run.tile_id.saturating_add(u64::from(run.run_length)))
    }

    pub(crate) fn raw_blob(&self, offset: u64, length: u32) -> std::io::Result<&[u8]> {
        self.view.raw_blob_at(offset, length)
    }
}
