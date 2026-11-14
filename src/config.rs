//! Checked caller-facing pipeline request values.
use crate::shortbread::Layer;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// Explicit ocean sources. The full-resolution source is always required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OceanInputs {
    pub full: PathBuf,
    pub simplified: Option<PathBuf>,
    pub artifact: Option<PathBuf>,
}

impl OceanInputs {
    /// Parse repeatable `--ocean` specs. An empty list disables ocean.
    pub fn parse_specs(specs: &[String]) -> Result<Option<Self>, String> {
        resolve_ocean_inputs(specs)
    }

    /// Parse shapefile specs for a world artifact build, rejecting artifacts.
    pub fn parse_build_specs(specs: &[String]) -> Result<Self, String> {
        let inputs = Self::parse_specs(specs)?
            .ok_or_else(|| "ocean-build needs a full-resolution shapefile".to_string())?;
        if inputs.artifact.is_some() {
            return Err("ocean-build --ocean takes shapefiles, not a .pmtiles artifact".into());
        }
        Ok(inputs)
    }
}

#[derive(Default)]
struct ParsedOceanInputs {
    low: Option<PathBuf>,
    high: Option<PathBuf>,
    artifact: Option<PathBuf>,
}

/// Finite polygon simplification multiplier in the inclusive range 0.1..=10.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimplifyFactor {
    pub(crate) value: f64,
}

impl SimplifyFactor {
    /// Check a multiplier without changing its floating-point representation.
    pub fn new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || !(0.1..=10.0).contains(&value) {
            return Err(format!(
                "--polygon-simplify-factor must be between 0.1 and 10.0, got {value}"
            ));
        }
        Ok(Self { value })
    }
    pub fn get(self) -> f64 {
        self.value
    }
}
impl Default for SimplifyFactor {
    fn default() -> Self {
        Self { value: 1.0 }
    }
}

/// Per-layer fanout limits. Zero means uncapped; only eligible layers accept caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanoutCaps {
    pub(crate) values: [Option<NonZeroU32>; Layer::count()],
}
impl Default for FanoutCaps {
    fn default() -> Self {
        Self {
            values: [None; Layer::count()],
        }
    }
}
impl FanoutCaps {
    /// Apply a default to eligible layers, followed by ordered `layer=N` overrides.
    pub fn parse_specs(default: Option<u32>, specs: &[String]) -> Result<Self, String> {
        let mut caps = Self::default();
        for layer in Layer::ALL {
            if layer.supports_fanout_cap() {
                caps.values[layer as usize] = default.and_then(NonZeroU32::new);
            }
        }
        for spec in specs {
            let (name, value) = spec.trim().split_once('=').ok_or_else(|| {
                format!("invalid --fanout-cap format, expected layer=N: '{spec}'")
            })?;
            let layer: Layer = name.trim().parse()?;
            let value = value
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid cap value in --fanout-cap: '{spec}'"))?;
            caps.set(layer, value)?;
        }
        Ok(caps)
    }
    /// Set a supported layer's cap; zero removes its cap.
    pub fn set(&mut self, layer: Layer, value: u32) -> Result<(), String> {
        if !layer.supports_fanout_cap() {
            return Err(format!(
                "layer '{}' does not support --fanout-cap",
                layer.name()
            ));
        }
        self.values[layer as usize] = NonZeroU32::new(value);
        Ok(())
    }
    pub fn get(&self, layer: Layer) -> Option<NonZeroU32> {
        self.values[layer as usize]
    }
    pub(crate) fn as_array(&self) -> &[Option<NonZeroU32>; Layer::count()] {
        &self.values
    }
}

/// Parse one `--ocean` value into either a zoom-ranged shapefile or an
/// artifact. Nothing here consults the filesystem: a spec means what it says,
/// and a path that does not exist is diagnosed later, by the code that opens
/// it, rather than silently changing what the run is.
fn parse_ocean_spec(spec: &str) -> Result<(Option<(u8, u8)>, PathBuf), String> {
    let Some((range, path)) = spec.split_once(':') else {
        // No range prefix: only the artifact is accepted bare, because it is
        // the one input whose zoom coverage is a property of the file.
        if Path::new(spec).extension().is_some_and(|e| e == "pmtiles") {
            return Ok((None, PathBuf::from(spec)));
        }
        return Err(format!(
            "'{spec}' needs a zoom range, e.g. z0-z14:{spec} (bare paths are accepted only for a .pmtiles artifact)"
        ));
    };
    let parse_z = |t: &str| -> Result<u8, String> {
        t.strip_prefix('z')
            .ok_or_else(|| format!("'{t}' is not a zoom (expected z0-z14)"))?
            .parse::<u8>()
            .map_err(|_| format!("'{t}' is not a zoom (expected z0-z14)"))
    };
    let (lo, hi) = range
        .split_once("-")
        .ok_or_else(|| format!("'{range}' is not a zoom range (expected z0-z7)"))?;
    let (lo, hi) = (parse_z(lo)?, parse_z(hi)?);
    if lo > hi || hi > crate::tiling::MAX_ZOOM {
        return Err(format!("'{range}' is not a zoom range within z0-z14"));
    }
    Ok((Some((lo, hi)), PathBuf::from(path)))
}

/// Validate the whole `--ocean` set and map it onto the two shapefile passes.
///
/// The zoom ranges are checked against what the engine can actually do rather
/// than accepted and rounded: the ocean plan's two passes split at z7/z8 and
/// nowhere else, so those are the only partitions expressible. Accepting
/// `z0-z5` and quietly serving a z7 split would put a false statement in the
/// recorded invocation, which is the whole thing this flag exists to prevent.
fn resolve_ocean_inputs(specs: &[String]) -> Result<Option<OceanInputs>, String> {
    let mut out = ParsedOceanInputs::default();
    let mut ranges: Vec<((u8, u8), PathBuf)> = Vec::new();
    for spec in specs {
        match parse_ocean_spec(spec)? {
            (None, path) => {
                if out.artifact.replace(path).is_some() {
                    return Err("--ocean names more than one .pmtiles artifact".to_string());
                }
            }
            (Some(range), path) => ranges.push((range, path)),
        }
    }
    ranges.sort_by_key(|(r, _)| *r);
    match ranges.as_slice() {
        [] => {}
        [((0, 14), full)] => out.high = Some(full.clone()),
        [((0, 7), low), ((8, 14), high)] => {
            out.low = Some(low.clone());
            out.high = Some(high.clone());
        }
        _ => {
            return Err(
                "--ocean shapefiles must partition z0-z14 as either z0-z14 alone or z0-z7 plus z8-z14 (the engine splits at z7/z8 and nowhere else)"
                    .to_string(),
            );
        }
    }
    if out.artifact.is_some() && out.high.is_none() {
        return Err(
            "--ocean names a .pmtiles artifact but no shapefile: an extract computes its boundary band from the shapefiles, and the artifact's key is validated by re-hashing them"
                .to_string(),
        );
    }
    Ok(out.high.map(|full| OceanInputs {
        full,
        simplified: out.low,
        artifact: out.artifact,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ocean(specs: &[&str]) -> Result<Option<OceanInputs>, String> {
        let owned: Vec<String> = specs.iter().map(|s| (*s).to_string()).collect();
        resolve_ocean_inputs(&owned)
    }

    #[test]
    fn ocean_absent_means_no_ocean() {
        assert_eq!(ocean(&[]).expect("valid ocean specs"), None);
    }

    #[test]
    fn ocean_single_full_range_serves_every_zoom() {
        let got = ocean(&["z0-z14:full.shp"])
            .expect("valid ocean specs")
            .expect("ocean enabled");
        assert_eq!(got.full, PathBuf::from("full.shp"));
        assert_eq!(got.simplified, None);
        assert_eq!(got.artifact, None);
    }

    #[test]
    fn ocean_split_maps_low_and_high_passes() {
        // Order of the flags must not matter; the ranges decide the passes.
        let got = ocean(&["z8-z14:full.shp", "z0-z7:simple.shp"])
            .expect("valid ocean specs")
            .expect("ocean enabled");
        assert_eq!(got.simplified, Some(PathBuf::from("simple.shp")));
        assert_eq!(got.full, PathBuf::from("full.shp"));
    }

    #[test]
    fn ocean_artifact_is_additive_to_the_shapefiles() {
        let got = ocean(&["z0-z14:full.shp", "ocean-tiles.pmtiles"])
            .expect("valid ocean specs")
            .expect("ocean enabled");
        assert_eq!(got.full, PathBuf::from("full.shp"));
        assert_eq!(got.artifact, Some(PathBuf::from("ocean-tiles.pmtiles")));
    }

    #[test]
    fn ocean_artifact_alone_is_rejected() {
        // An extract computes its boundary band from the shapefiles and the
        // artifact key is validated by re-hashing them, so an artifact with no
        // shapefile is an incomplete spec, not a world build.
        let err = ocean(&["ocean-tiles.pmtiles"]).expect_err("invalid ocean specs");
        assert!(err.contains("no shapefile"), "{err}");
    }

    #[test]
    fn ocean_rejects_a_split_the_engine_cannot_do() {
        // The ocean passes split at z7/z8 and nowhere else. Accepting this
        // and serving a z7 split would put a false statement in the recorded
        // invocation.
        let err = ocean(&["z0-z5:simple.shp", "z6-z14:full.shp"]).expect_err("invalid ocean specs");
        assert!(err.contains("partition z0-z14"), "{err}");
    }

    #[test]
    fn ocean_rejects_ranges_that_leave_a_gap() {
        let err = ocean(&["z0-z7:simple.shp"]).expect_err("invalid ocean specs");
        assert!(err.contains("partition z0-z14"), "{err}");
    }

    #[test]
    fn ocean_rejects_a_bare_shapefile() {
        let err = ocean(&["full.shp"]).expect_err("invalid ocean specs");
        assert!(err.contains("needs a zoom range"), "{err}");
    }

    #[test]
    fn ocean_rejects_two_artifacts() {
        let err =
            ocean(&["z0-z14:full.shp", "a.pmtiles", "b.pmtiles"]).expect_err("invalid ocean specs");
        assert!(err.contains("more than one"), "{err}");
    }

    #[test]
    fn ocean_rejects_malformed_ranges() {
        assert!(
            ocean(&["0-14:full.shp"])
                .expect_err("invalid ocean specs")
                .contains("not a zoom")
        );
        assert!(
            ocean(&["z9-z2:full.shp"])
                .expect_err("invalid ocean specs")
                .contains("range")
        );
        assert!(
            ocean(&["z0-z15:full.shp"])
                .expect_err("invalid ocean specs")
                .contains("range")
        );
    }
    #[test]
    fn simplify_factor_checks_finite_inclusive_range_without_rounding() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.099, 10.001] {
            assert!(SimplifyFactor::new(value).is_err());
        }
        for value in [0.1, 1.23456789012345, 10.0] {
            assert_eq!(
                SimplifyFactor::new(value)
                    .expect("valid factor")
                    .get()
                    .to_bits(),
                value.to_bits()
            );
        }
        assert_eq!(SimplifyFactor::default().get(), 1.0);
    }

    #[test]
    fn fanout_caps_default_override_zero_and_validation() {
        let caps = FanoutCaps::parse_specs(
            Some(42),
            &[
                " buildings = 12 ".into(),
                "buildings=0".into(),
                "buildings=7".into(),
            ],
        )
        .expect("valid caps");
        assert_eq!(caps.get(Layer::Buildings).expect("capped").get(), 7);
        for layer in Layer::ALL {
            if layer != Layer::Buildings {
                assert_eq!(
                    caps.get(layer).map(NonZeroU32::get),
                    layer.supports_fanout_cap().then_some(42)
                );
            }
        }
        let caps = FanoutCaps::parse_specs(Some(42), &["buildings=0".into()]).expect("valid zero");
        assert_eq!(caps.get(Layer::Buildings), None);
        for spec in [
            "unknown=1",
            "ocean=1",
            "buildings",
            "buildings=-1",
            "buildings=4294967296",
        ] {
            assert!(
                FanoutCaps::parse_specs(None, &[spec.into()]).is_err(),
                "{spec}"
            );
        }
    }

    #[test]
    fn artifact_build_requires_sources_and_refuses_artifacts() {
        assert!(OceanInputs::parse_build_specs(&[]).is_err());
        assert!(
            OceanInputs::parse_build_specs(&["z0-z14:full.shp".into(), "world.pmtiles".into()])
                .is_err()
        );
        assert_eq!(
            OceanInputs::parse_build_specs(&["z0-z14:full.shp".into()])
                .expect("source")
                .full,
            PathBuf::from("full.shp")
        );
    }
}
