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
use crate::tiling::TileId;

/// The artifact is mapped, not read, so assembly borrows its tile blobs
/// without copying them.
type ArchiveView = Archive<MapSource>;

fn open_view(path: &std::path::Path) -> std::io::Result<ArchiveView> {
    // SAFETY: this assumes nothing modifies or truncates the artifact's inode
    // while the run holds it. This crate's archive writes honor that by
    // construction: archive publication (`archive::writer`, both modes)
    // renames a fresh `create_new` sibling over the path after closing it,
    // and retention only hard-links and unlinks - so `ocean-build`, or a run
    // whose `-o` names the artifact, replaces the directory entry and leaves
    // the mapped inode intact. What remains is an input-immutability
    // contract on everything else: a process (or safe Rust in this one)
    // writing the file in place - `cp` over it, `rsync --inplace`,
    // `truncate` - is undefined behavior here, typically SIGBUS. That is
    // accepted as the price of the zero-copy splice, the same contract the
    // shapefile mapping already rests on. An advisory lock or a size/mtime
    // recheck would not discharge it (locks bind only cooperating writers;
    // a recheck races the write), and copying blobs on read is the only
    // in-process way to drop the assumption. Every other archive reader
    // goes through the positional-read source instead.
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
            key: self.key,
        })
    }
}

/// Bound, mmap-backed ocean artifact shared by assembly workers, with the
/// band that decides which of its tiles this run serves.
pub(crate) struct OceanTiles {
    view: ArchiveView,
    runs: TileRuns,
    band: OceanBand,
    key: OceanArtifactKey,
}

impl OceanTiles {
    /// The admitted key, so assembly can check it against the provenance
    /// it records.
    pub(crate) fn key(&self) -> &OceanArtifactKey {
        &self.key
    }

    pub(crate) fn band(&self) -> &OceanBand {
        &self.band
    }

    pub(crate) fn run_count(&self) -> usize {
        self.runs.len()
    }

    pub(crate) fn runs_in(&self, start: u64, end: u64) -> &[TileRun] {
        self.runs.intersecting(start, end)
    }

    pub(crate) fn run_covering(&self, tile_id: TileId) -> Option<TileRun> {
        self.runs.covering(tile_id).copied()
    }

    pub(crate) fn raw_blob(&self, blob: BlobRef) -> std::io::Result<std::borrow::Cow<'_, [u8]>> {
        self.view.raw_blob(blob)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::key::{OceanShapefileHashes, OceanSourceKey, Xxh128};
    use super::*;
    use crate::archive;
    use crate::geometry::projection::MercBbox;

    #[test]
    fn ocean_artifact_rejects_runs_reaching_z15() {
        let dir = tempfile::tempdir().expect("create artifact directory");
        let path = dir.path().join("ocean.pmtiles");
        let key = OceanArtifactKey {
            source: OceanSourceKey::from_hashes(
                OceanShapefileHashes {
                    shp_xxh128: Xxh128::from_u128(1),
                    shx_xxh128: Xxh128::from_u128(2),
                },
                None,
                0,
                14,
            ),
            compression_level: 6,
        };
        let world = MercBbox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1.0,
            max_y: 1.0,
        };
        let first_z15 = crate::tiling::TileId::at(15, 0, 0).raw();
        for (start, length, accepted) in [
            (first_z15 - 1, 1, true),
            (first_z15, 1, false),
            (first_z15 - 1, 2, false),
        ] {
            let mut writer = archive::PmtilesWriter::new(
                archive::PmtilesConfig::new(
                    0,
                    14,
                    archive::LonLatBbox {
                        west: -180.0,
                        south: -85.05,
                        east: 180.0,
                        north: 85.05,
                    },
                    archive::MapCenter {
                        lon: 0.0,
                        lat: 0.0,
                        zoom: 2,
                    },
                )
                .expect("valid archive config"),
            );
            writer.set_ocean_only_metadata();
            writer
                .set_ocean_artifact_metadata(key.clone())
                .expect("set artifact metadata");
            // Admission reads directory ranges and metadata, not tile payloads.
            writer
                .add_run(crate::tiling::TileId::new(start).unwrap(), length, &[0])
                .expect("add directory run");
            writer.write_to(&path).expect("write artifact");
            let result = OceanArtifact::admit(&path, 6, || Ok(key.clone()))
                .and_then(|artifact| artifact.into_tiles(OceanBand::for_bounds(&world)));
            if accepted {
                assert!(result.is_ok(), "last z14 tile must be admitted");
            } else {
                let error = result.err().expect("z15 run must be refused");
                assert!(error.to_string().contains("outside z0-14 tile space"));
            }
        }
    }

    #[test]
    fn ocean_artifact_admission_refuses_a_level_or_key_mismatch_before_reading_runs() {
        let dir = tempfile::tempdir().expect("create artifact directory");
        let path = dir.path().join("ocean.pmtiles");
        let key = OceanArtifactKey {
            source: OceanSourceKey::from_hashes(
                OceanShapefileHashes {
                    shp_xxh128: Xxh128::from_u128(1),
                    shx_xxh128: Xxh128::from_u128(2),
                },
                None,
                0,
                14,
            ),
            compression_level: 6,
        };
        let mut writer = archive::PmtilesWriter::new(
            archive::PmtilesConfig::new(
                0,
                14,
                archive::LonLatBbox {
                    west: -180.0,
                    south: -85.05,
                    east: 180.0,
                    north: 85.05,
                },
                archive::MapCenter {
                    lon: 0.0,
                    lat: 0.0,
                    zoom: 2,
                },
            )
            .expect("valid archive config"),
        );
        writer.set_ocean_only_metadata();
        writer
            .set_ocean_artifact_metadata(key.clone())
            .expect("set artifact metadata");
        writer
            .add_run(crate::tiling::TileId::ZERO, 1, &[0])
            .expect("add directory run");
        writer.write_to(&path).expect("write artifact");

        let level = OceanArtifact::admit(&path, 5, || panic!("level is checked before hashing"))
            .err()
            .expect("level mismatch refused");
        assert!(level.to_string().contains("compression level 6"));
        let mut other = key.clone();
        other.full_shp_xxh128 = Xxh128::from_u128(3);
        let mismatch = OceanArtifact::admit(&path, 6, || Ok(other))
            .err()
            .expect("key mismatch refused");
        assert!(mismatch.to_string().contains("does not match"));
        assert_eq!(
            OceanArtifact::admit(&path, 6, || Ok(key.clone()))
                .expect("matching key admitted")
                .key(),
            &key
        );
        assert_eq!(declared_key(&path).expect("declared key"), key);
    }
}
