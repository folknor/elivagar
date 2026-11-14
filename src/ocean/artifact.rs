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
use crate::pipeline::{ArtifactRefusal, KeyMismatch, PipelineError};
use crate::shortbread::Layer;
use crate::tiling::TileId;

/// The artifact is mapped, not read, so assembly borrows its tile blobs
/// without copying them.
type ArchiveView = Archive<MapSource>;

fn open_view(path: &std::path::Path) -> Result<ArchiveView, crate::DecodeError> {
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

fn artifact_read(reason: ArtifactRefusal, error: crate::DecodeError) -> PipelineError {
    match error.kind() {
        crate::DecodeErrorKind::Io | crate::DecodeErrorKind::Failed => error.into(),
        _ => PipelineError::artifact_refused(reason, "invalid ocean artifact").with_cause(error),
    }
}

fn check_key(
    declared: &OceanArtifactKey,
    expected: &OceanArtifactKey,
) -> Result<(), PipelineError> {
    let mut fields = Vec::new();
    macro_rules! compare {
            ($($field:ident),* $(,)?) => {
                $(if declared.$field != expected.$field {
                    fields.push(stringify!($field));
                })*
            };
        }
    compare!(
        full_shp_xxh128,
        full_shx_xxh128,
        simplified_shp_xxh128,
        simplified_shx_xxh128,
        min_zoom,
        max_zoom,
        policy_version
    );
    if declared.compression_level != expected.compression_level {
        fields.push("compression_level");
    }
    if !fields.is_empty() {
        let message = format!(
            "ocean artifact key does not match current inputs: {}",
            fields.join(", ")
        );
        return Err(PipelineError::artifact_refused(
            ArtifactRefusal::KeyMismatch(KeyMismatch { fields }),
            message,
        ));
    }
    Ok(())
}

/// First zoom of the artifact contract.
pub(crate) const ARTIFACT_MIN_ZOOM: u8 = 0;
/// Last zoom of the artifact contract.
pub(crate) const ARTIFACT_MAX_ZOOM: u8 = crate::tiling::MAX_ZOOM;

/// Whether a run window is the one the artifact serves.
pub(crate) fn artifact_serves_window(min_zoom: u8, max_zoom: u8) -> bool {
    min_zoom == ARTIFACT_MIN_ZOOM && max_zoom == ARTIFACT_MAX_ZOOM
}

fn declared_key_of(view: &ArchiveView) -> Result<OceanArtifactKey, crate::DecodeError> {
    let metadata: serde_json::Value = serde_json::from_str(&view.metadata()?)
        .map_err(|e| crate::DecodeError::caused("artifact metadata JSON", e))?;
    OceanArtifactKey::from_json(metadata.get("ocean_artifact").ok_or_else(|| {
        crate::DecodeError::malformed(
            "artifact metadata",
            "ocean artifact missing ocean_artifact metadata",
        )
    })?)
    .map_err(|e| crate::DecodeError::caused("artifact key", e))
}

/// The key an artifact file declares, without judging whether it can serve
/// anything. Retention of an outgoing artifact reads this, and must keep
/// working for artifacts the current inputs would refuse.
pub(crate) fn declared_key(path: &std::path::Path) -> std::io::Result<OceanArtifactKey> {
    // Identity only: no readability check, so retention can still read the
    // key of an artifact the current run could not serve.
    declared_key_of(&open_view(path)?).map_err(Into::into)
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
    ) -> Result<Self, PipelineError> {
        if let Err(error) = std::fs::metadata(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(PipelineError::io("could not stat ocean artifact", error));
            }
            return Err(PipelineError::artifact_refused(
                ArtifactRefusal::Missing,
                format!(
                    "configured ocean artifact does not exist: {}",
                    path.display()
                ),
            )
            .with_cause(error));
        }
        let view = open_view(path).map_err(|e| artifact_read(ArtifactRefusal::Layout, e))?;
        view.tile_decoder()
            .map_err(|e| artifact_read(ArtifactRefusal::Format, e))?;
        view.check_layout()
            .map_err(|e| artifact_read(ArtifactRefusal::Layout, e))?;
        let declared =
            declared_key_of(&view).map_err(|e| artifact_read(ArtifactRefusal::Metadata, e))?;
        if declared.compression_level != compression_level {
            return Err(PipelineError::artifact_refused(
                ArtifactRefusal::KeyMismatch(KeyMismatch {
                    fields: vec!["compression_level"],
                }),
                format!(
                    "ocean artifact {} was built at compression level {} but this run uses {}. Rebuild the artifact, match --compression-level, or drop the artifact from --ocean.",
                    path.display(),
                    declared.compression_level,
                    compression_level,
                ),
            ));
        }
        let header = view.header();
        if header.min_zoom != ARTIFACT_MIN_ZOOM || header.max_zoom != ARTIFACT_MAX_ZOOM {
            return Err(PipelineError::artifact_refused(
                ArtifactRefusal::Coverage,
                "ocean artifact must cover z0-14",
            ));
        }
        // A coverage inequality, not equality with a rounded world constant:
        // the artifact writes -85.05/85.05, inside the Mercator limit.
        let bounds = header.bounds;
        if bounds.west > -1_800_000_000
            || bounds.south > -850_500_000
            || bounds.east < 1_800_000_000
            || bounds.north < 850_500_000
        {
            return Err(PipelineError::artifact_refused(
                ArtifactRefusal::Coverage,
                "ocean artifact header bounds must cover the world",
            ));
        }
        let metadata: serde_json::Value = serde_json::from_str(
            &view
                .metadata()
                .map_err(|e| artifact_read(ArtifactRefusal::Metadata, e))?,
        )
        .map_err(|e| {
            PipelineError::artifact_refused(
                ArtifactRefusal::Metadata,
                "invalid ocean artifact metadata",
            )
            .with_cause(e)
        })?;
        let layers = metadata
            .get("vector_layers")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                PipelineError::artifact_refused(
                    ArtifactRefusal::Metadata,
                    "ocean artifact missing vector_layers",
                )
            })?;
        if layers.len() != 1
            || layers[0].get("id").and_then(serde_json::Value::as_str) != Some(Layer::Ocean.name())
        {
            return Err(PipelineError::artifact_refused(
                ArtifactRefusal::Metadata,
                "ocean artifact must declare exactly one ocean layer",
            ));
        }
        // Hash failures belong to the shapefile inputs, not the artifact.
        let expected = expected().map_err(PipelineError::from)?;
        check_key(&declared, &expected)?;
        Ok(Self {
            view,
            key: declared,
        })
    }

    pub(crate) fn key(&self) -> &OceanArtifactKey {
        &self.key
    }

    /// Read the directory and bind it to the extract's band for assembly.
    pub(crate) fn into_tiles(self, band: OceanBand) -> Result<OceanTiles, PipelineError> {
        let runs = self
            .view
            .read_all_runs()
            .map_err(|e| artifact_read(ArtifactRefusal::Directory, e))?;
        // Exclusive end: the first id past the top zoom is outside the
        // artifact contract.
        let profile_end = crate::tiling::PROFILE_ID_END;
        if runs.as_slice().iter().any(|run| run.end() > profile_end) {
            return Err(PipelineError::artifact_refused(
                ArtifactRefusal::Directory,
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

    pub(crate) fn raw_blob(
        &self,
        blob: BlobRef,
    ) -> Result<std::borrow::Cow<'_, [u8]>, PipelineError> {
        self.view
            .raw_blob(blob)
            .map_err(|e| artifact_read(ArtifactRefusal::Payload, e))
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
    fn key_mismatch_names_every_changed_field() {
        let source = OceanSourceKey::from_hashes(
            OceanShapefileHashes {
                shp_xxh128: Xxh128::from_u128(1),
                shx_xxh128: Xxh128::from_u128(2),
            },
            None,
            0,
            14,
        );
        let declared = OceanArtifactKey {
            source,
            compression_level: 6,
        };
        let mut expected = declared.clone();
        expected.policy_version += 1;
        expected.min_zoom = 1;
        expected.max_zoom = 13;
        expected.simplified_shp_xxh128 = Some(Xxh128::from_u128(3));
        expected.simplified_shx_xxh128 = Some(Xxh128::from_u128(4));
        expected.full_shp_xxh128 = Xxh128::from_u128(5);
        expected.full_shx_xxh128 = Xxh128::from_u128(6);
        expected.compression_level = 5;
        let error = check_key(&declared, &expected).expect_err("changed identity");
        assert_eq!(
            error.artifact_refusal(),
            Some(&ArtifactRefusal::KeyMismatch(KeyMismatch {
                fields: vec![
                    "full_shp_xxh128",
                    "full_shx_xxh128",
                    "simplified_shp_xxh128",
                    "simplified_shx_xxh128",
                    "min_zoom",
                    "max_zoom",
                    "policy_version",
                    "compression_level"
                ],
            }))
        );
        assert_eq!(error.kind().exit_code(), 4);
    }

    #[test]
    fn missing_and_malformed_artifacts_have_typed_reasons() {
        let dir = tempfile::tempdir().expect("artifact directory");
        let path = dir.path().join("artifact.pmtiles");
        let missing = OceanArtifact::admit(&path, 6, || panic!("must not hash"))
            .err()
            .expect("missing artifact");
        assert_eq!(missing.artifact_refusal(), Some(&ArtifactRefusal::Missing));
        std::fs::write(&path, b"bad archive").expect("write malformed artifact");
        let malformed = OceanArtifact::admit(&path, 6, || panic!("must not hash"))
            .err()
            .expect("malformed artifact");
        assert_eq!(malformed.artifact_refusal(), Some(&ArtifactRefusal::Layout));
        let operational = artifact_read(
            ArtifactRefusal::Directory,
            std::io::Error::from_raw_os_error(5).into(),
        );
        assert_eq!(operational.kind().exit_code(), 74);
        assert!(std::error::Error::source(&operational).is_some());
    }

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
                .add_run(
                    crate::tiling::TileId::new(start).expect("valid tile id"),
                    length,
                    &[0],
                )
                .expect("add directory run");
            writer.write_to(&path).expect("write artifact");
            let result = OceanArtifact::admit(&path, 6, || Ok(key.clone()))
                .and_then(|artifact| artifact.into_tiles(OceanBand::for_bounds(&world)));
            if accepted {
                assert!(result.is_ok(), "last z14 tile must be admitted");
            } else {
                let error = result.err().expect("z15 run must be refused");
                assert_eq!(error.artifact_refusal(), Some(&ArtifactRefusal::Directory));
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
        assert_eq!(
            level.artifact_refusal(),
            Some(&ArtifactRefusal::KeyMismatch(KeyMismatch {
                fields: vec!["compression_level"]
            }))
        );
        let mut other = key.clone();
        other.full_shp_xxh128 = Xxh128::from_u128(3);
        let mismatch = OceanArtifact::admit(&path, 6, || Ok(other))
            .err()
            .expect("key mismatch refused");
        assert_eq!(
            mismatch.artifact_refusal(),
            Some(&ArtifactRefusal::KeyMismatch(KeyMismatch {
                fields: vec!["full_shp_xxh128"]
            }))
        );
        let hashing = OceanArtifact::admit(&path, 6, || Err(std::io::Error::from_raw_os_error(13)))
            .err()
            .expect("hash failure");
        assert_eq!(hashing.kind(), crate::ErrorKind::Io);
        assert_eq!(hashing.artifact_refusal(), None);
        let opaque_hashing =
            OceanArtifact::admit(&path, 6, || Err(std::io::Error::other("hash failure")))
                .err()
                .expect("opaque hash failure");
        assert_eq!(opaque_hashing.kind(), crate::ErrorKind::Failed);
        assert_eq!(opaque_hashing.artifact_refusal(), None);
        assert_eq!(
            OceanArtifact::admit(&path, 6, || Ok(key.clone()))
                .expect("matching key admitted")
                .key(),
            &key
        );
        assert_eq!(declared_key(&path).expect("declared key"), key);
    }
}
