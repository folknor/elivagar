//! Which ocean passes exist, which shapefile feeds each, and which grid each
//! zoom is constructed on.
//!
//! The answer is fixed, not configured. There are always two passes: the
//! low pass covers z0-z7 on a z7 grid and unions its source pieces before
//! descent, and the high pass covers z8-z14 on a z14 grid and descends per
//! piece. The low pass reads the simplified shapefile when one is named and
//! the full one otherwise, so `z0-z14:<full>` and the `z0-z7` + `z8-z14` pair
//! run the same pass structure. A run's zoom window only truncates emission:
//! each pass still quantizes, roots and descends on its canonical grid from
//! its own first zoom, so a narrowed run holds exactly the ocean tiles a full
//! build holds at those zooms. The world artifact, the extract's band
//! ownership, and the producer all read this one table.

use std::path::{Path, PathBuf};

use super::{OceanArtifactKey, OceanShapefileHashes, OceanSourceKey, ShapefilePair};
use crate::provenance::ActiveLowZoomSource;
use crate::tiling::MAX_ZOOM;

/// Last zoom of the low pass. The engine splits here and nowhere else.
const LOW_PASS_LAST_ZOOM: u8 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OceanPass {
    Low,
    High,
}

impl OceanPass {
    pub(crate) const ALL: [Self; 2] = [Self::Low, Self::High];

    pub(crate) const fn for_zoom(z: u8) -> Self {
        if z <= LOW_PASS_LAST_ZOOM {
            Self::Low
        } else {
            Self::High
        }
    }

    pub(crate) const fn first_zoom(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::High => LOW_PASS_LAST_ZOOM + 1,
        }
    }

    pub(crate) const fn last_zoom(self) -> u8 {
        match self {
            Self::Low => LOW_PASS_LAST_ZOOM,
            Self::High => MAX_ZOOM,
        }
    }

    /// The zoom whose pixel grid this pass quantizes and descends on.
    pub(crate) const fn grid_zoom(self) -> u8 {
        self.last_zoom()
    }

    /// Whether this pass unions its source pieces before descent. The source
    /// shapefiles arrive pre-split into cells comparable to low-zoom tile
    /// sizes, where per-piece descent renders seam wedges; at z8 and deeper
    /// the seam mismatch is sub-pixel and a world-scale union of the full
    /// polygons would dominate `ocean-build`.
    pub(crate) const fn unions(self) -> bool {
        matches!(self, Self::Low)
    }

    /// The part of this pass a run window `min_zoom..=max_zoom` emits, or
    /// `None` when the window misses the pass.
    pub(crate) fn emission(self, min_zoom: u8, max_zoom: u8) -> Option<PassWindow> {
        let first = self.first_zoom().max(min_zoom);
        let last = self.last_zoom().min(max_zoom);
        (first <= last).then_some(PassWindow {
            pass: self,
            first,
            last,
        })
    }
}

/// One pass restricted to the zooms a run emits. Construction still starts at
/// `pass.first_zoom()` on `pass.grid_zoom()`; `first..=last` only says which
/// of the constructed zooms are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PassWindow {
    pub(crate) pass: OceanPass,
    pub(crate) first: u8,
    pub(crate) last: u8,
}

/// The validated ocean inputs of one run.
///
/// The key records the files as the invocation named them, in their roles:
/// `z0-z7:<full> z8-z14:<full>` names the full file as the low-zoom source,
/// so its key carries simplified hashes and its contract reads `simplified`,
/// while `z0-z14:<full>` carries none and reads `full`. The two run identical
/// work, but only in that degenerate same-file case, and collapsing them would
/// change what `simplified_*_xxh128` means in every key already written.
#[derive(Clone, Debug)]
pub(crate) struct OceanPlan {
    full: ShapefilePair,
    simplified: Option<ShapefilePair>,
    artifact: Option<PathBuf>,
}

impl OceanPlan {
    /// `None` when no ocean input is named. A simplified shapefile or an
    /// artifact without the full shapefile is refused: the full file feeds the
    /// high pass and validates the artifact key, so either one alone would
    /// otherwise mean silently no ocean.
    pub(crate) fn new(
        full: Option<&Path>,
        simplified: Option<&Path>,
        artifact: Option<&Path>,
    ) -> Result<Option<Self>, String> {
        let Some(full) = full else {
            if simplified.is_some() {
                return Err(
                    "a simplified ocean shapefile needs the full shapefile, which feeds z8-z14"
                        .to_string(),
                );
            }
            if artifact.is_some() {
                return Err(
                    "ocean artifact requires the full shapefile for key validation".to_string(),
                );
            }
            return Ok(None);
        };
        Ok(Some(Self {
            full: ShapefilePair::from_shp(full),
            simplified: simplified.map(ShapefilePair::from_shp),
            artifact: artifact.map(Path::to_path_buf),
        }))
    }

    pub(crate) fn artifact(&self) -> Option<&Path> {
        self.artifact.as_deref()
    }

    /// The shapefile a pass reads.
    pub(crate) fn source(&self, pass: OceanPass) -> &ShapefilePair {
        match pass {
            OceanPass::Low => self.simplified.as_ref().unwrap_or(&self.full),
            OceanPass::High => &self.full,
        }
    }

    pub(crate) fn has_simplified(&self) -> bool {
        self.simplified.is_some()
    }

    /// Which file serves the low zooms a window emits. A window that emits
    /// no low zoom reports `Full`, as it always has on the wire.
    pub(crate) fn low_zoom_source(&self, min_zoom: u8) -> ActiveLowZoomSource {
        if self.simplified.is_some() && min_zoom <= LOW_PASS_LAST_ZOOM {
            ActiveLowZoomSource::Simplified
        } else {
            ActiveLowZoomSource::Full
        }
    }

    /// Hash the named files into the computed-ocean identity for a window.
    pub(crate) fn source_key(&self, min_zoom: u8, max_zoom: u8) -> std::io::Result<OceanSourceKey> {
        OceanSourceKey::from_inputs(&self.full, self.simplified.as_ref(), min_zoom, max_zoom)
    }

    /// The key a world artifact built from these files carries.
    pub(crate) fn artifact_key(&self, compression_level: u32) -> std::io::Result<OceanArtifactKey> {
        OceanArtifactKey::from_inputs(
            &self.full,
            self.simplified.as_ref(),
            super::artifact::ARTIFACT_MIN_ZOOM,
            super::artifact::ARTIFACT_MAX_ZOOM,
            compression_level,
        )
    }

    /// The hashes a pass must find in the buffers it opens.
    pub(crate) fn pass_hashes(
        &self,
        key: &OceanSourceKey,
        pass: OceanPass,
    ) -> Result<OceanShapefileHashes, String> {
        match pass {
            OceanPass::Low if self.simplified.is_some() => key
                .simplified_shapefile_hashes()
                .ok_or_else(|| "resolved ocean key has no simplified shapefile hashes".to_string()),
            OceanPass::Low | OceanPass::High => Ok(key.full_shapefile_hashes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_partition_the_profile_zooms_at_the_one_split() {
        for z in 0..=MAX_ZOOM {
            let pass = OceanPass::for_zoom(z);
            assert!(pass.first_zoom() <= z && z <= pass.last_zoom());
            assert_eq!(pass.grid_zoom(), pass.last_zoom());
        }
        assert_eq!(OceanPass::Low.last_zoom() + 1, OceanPass::High.first_zoom());
        assert_eq!(OceanPass::Low.first_zoom(), 0);
        assert_eq!(OceanPass::High.last_zoom(), MAX_ZOOM);
    }

    #[test]
    fn emission_truncates_without_moving_construction() {
        assert_eq!(OceanPass::Low.emission(8, 14), None);
        assert_eq!(OceanPass::High.emission(0, 7), None);
        let low = OceanPass::Low.emission(3, 5).expect("low window");
        assert_eq!((low.first, low.last), (3, 5));
        assert_eq!(low.pass.grid_zoom(), 7);
        let high = OceanPass::High.emission(5, 12).expect("high window");
        assert_eq!((high.first, high.last), (8, 12));
        assert_eq!(high.pass.grid_zoom(), 14);
    }

    #[test]
    fn a_single_full_spelling_feeds_both_passes_from_the_full_file() {
        let plan = OceanPlan::new(Some(Path::new("full.shp")), None, None)
            .expect("valid plan")
            .expect("ocean named");
        assert_eq!(plan.source(OceanPass::Low).shp(), Path::new("full.shp"));
        assert_eq!(plan.source(OceanPass::High).shp(), Path::new("full.shp"));
        assert_eq!(plan.low_zoom_source(0), ActiveLowZoomSource::Full);

        let split = OceanPlan::new(
            Some(Path::new("full.shp")),
            Some(Path::new("simplified.shp")),
            None,
        )
        .expect("valid plan")
        .expect("ocean named");
        assert_eq!(
            split.source(OceanPass::Low).shp(),
            Path::new("simplified.shp")
        );
        assert_eq!(split.low_zoom_source(7), ActiveLowZoomSource::Simplified);
        assert_eq!(split.low_zoom_source(8), ActiveLowZoomSource::Full);
    }

    #[test]
    fn inputs_without_the_full_shapefile_are_refused() {
        assert!(
            OceanPlan::new(None, None, None)
                .expect("no ocean")
                .is_none()
        );
        assert!(OceanPlan::new(None, Some(Path::new("s.shp")), None).is_err());
        assert!(OceanPlan::new(None, None, Some(Path::new("o.pmtiles"))).is_err());
    }
}
