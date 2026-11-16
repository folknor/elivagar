//! `download-natural-earth`: pinned Natural Earth shapefiles for the low-zoom
//! layers, extracted flat into `<data>/natural_earth/`.
//!
//! Ported from brokkr's `download_natural_earth.rs`, same layer set, pins and
//! destinations. Idempotent: a layer is skipped only when every required
//! shapefile component exists.

use std::path::{Path, PathBuf};

use crate::fetch::{Tools, download, note, remove_if_present};

pub(crate) struct NaturalEarthLayer<'a> {
    /// Layer name, e.g. `ne_110m_ocean`.
    pub(crate) name: &'a str,
    /// URL category: `physical` or `cultural`.
    pub(crate) category: &'a str,
    /// Scale: `110m`, `50m`, or `10m`.
    pub(crate) scale: &'a str,
    /// Immutable S3 object version for this archive.
    pub(crate) s3_version_id: &'a str,
    /// XXH128 of the pinned archive bytes.
    pub(crate) xxh128: &'a str,
}

impl NaturalEarthLayer<'_> {
    pub(crate) fn url(&self) -> String {
        format!(
            "https://naturalearth.s3.amazonaws.com/{}_{}/{}.zip?versionId={}",
            self.scale, self.category, self.name, self.s3_version_id,
        )
    }
}

const fn layer<'a>(
    name: &'a str,
    category: &'a str,
    scale: &'a str,
    s3_version_id: &'a str,
    xxh128: &'a str,
) -> NaturalEarthLayer<'a> {
    NaturalEarthLayer {
        name,
        category,
        scale,
        s3_version_id,
        xxh128,
    }
}

// The Natural Earth CDN paths are mutable. These immutable S3 object
// versions identify the same published ZIP artifacts, and each XXH128 is
// computed over that exact object. Updating Natural Earth inputs requires
// changing both pins explicitly.
pub(crate) const LAYERS: &[NaturalEarthLayer<'static>] = &[
    // Ocean
    layer(
        "ne_110m_ocean",
        "physical",
        "110m",
        "MUObtTI2xWOdF_zuXwU_9GhzB.dVLbAN",
        "621fcb903ae7a02ac72d060fa73af1d1",
    ),
    layer(
        "ne_50m_ocean",
        "physical",
        "50m",
        "C73uPtV4GdJNU4k6oxytAblPD2fi6N92",
        "d4d8afefc83a09ea223d4a0a8f5157fe",
    ),
    layer(
        "ne_10m_ocean",
        "physical",
        "10m",
        "Fvr3vd_8GtWZ6sQoo.Je3C2rTj5Fotg1",
        "a69a1d94db8cb6cc26710ead34585c23",
    ),
    // Lakes
    layer(
        "ne_110m_lakes",
        "physical",
        "110m",
        "iU13Po9dZlJsf3dCtNOxZ0fre23Nf8ip",
        "885a3b8c5d42030862ab63828302d8af",
    ),
    layer(
        "ne_50m_lakes",
        "physical",
        "50m",
        "usgwJFifnRrM3VHtX13Dyz5X4fdBik.u",
        "145a541d09d4add0c4a82e0cc0dd1533",
    ),
    layer(
        "ne_10m_lakes",
        "physical",
        "10m",
        "I8f12xUKxLVqYQlDbPelAGtLPA7QLH8D",
        "329dadaae94e6333080669874ffa8f0b",
    ),
    // Glaciated areas
    layer(
        "ne_110m_glaciated_areas",
        "physical",
        "110m",
        "WyVugfTA0mpKpwIM600XZkK2f3xggU3W",
        "02f25c11e15523f721552fc77fb8f19c",
    ),
    layer(
        "ne_50m_glaciated_areas",
        "physical",
        "50m",
        "Y6gFJ0DNvm8.4LAdU2jwUkG8FoYjssrB",
        "741c69f7dc465bdd9cdb52c20516859c",
    ),
    layer(
        "ne_10m_glaciated_areas",
        "physical",
        "10m",
        "K5303JYT9Dync5kIWdeIoVw0LIikEgWz",
        "2b47229d575a1d042b254ad21f480fea",
    ),
    // Country boundaries
    layer(
        "ne_110m_admin_0_boundary_lines_land",
        "cultural",
        "110m",
        "6BXaI1J83h8twySMFcRVuyhMKloNnUFv",
        "49792e504a8a6d40c812767aca5f1d59",
    ),
    layer(
        "ne_50m_admin_0_boundary_lines_land",
        "cultural",
        "50m",
        "INgS9a.w9e940deDqwMgSNuQCvuCIhhu",
        "1c16ac540ad660d49f8b5330bd120ccf",
    ),
    layer(
        "ne_10m_admin_0_boundary_lines_land",
        "cultural",
        "10m",
        "bM5CgEPdE0hz.WZC4Zpu478UCD0ISGUH",
        "85cc9ef5aae081f28853bbad7c27923a",
    ),
];

const REQUIRED_COMPONENTS: &[&str] = &["shp", "shx", "dbf", "prj"];

pub(crate) fn run(data_dir: &Path, tools: &dyn Tools) -> Result<(), String> {
    run_layers(&data_dir.join("natural_earth"), LAYERS, tools)
}

pub(crate) fn run_layers(
    ne_dir: &Path,
    layers: &[NaturalEarthLayer<'_>],
    tools: &dyn Tools,
) -> Result<(), String> {
    std::fs::create_dir_all(ne_dir).map_err(|e| format!("{}: {e}", ne_dir.display()))?;
    let mut downloaded = 0usize;
    let mut skipped = 0usize;

    for layer in layers {
        if layer_is_complete(ne_dir, layer.name) {
            skipped += 1;
            continue;
        }
        let zip_path = ne_dir.join(format!("{}.zip", layer.name));

        note(&format!("downloading {}", layer.name));
        download(tools, &layer.url(), &zip_path, Some(layer.xxh128))?;

        note(&format!("extracting {}", layer.name));
        let staging = LayerStaging::create(ne_dir, layer.name)?;
        tools.unzip(&zip_path, staging.path(), true)?;

        for extension in REQUIRED_COMPONENTS {
            let staged = staging.path().join(format!("{}.{extension}", layer.name));
            if !staged.is_file() {
                return Err(format!(
                    "expected {} after extraction but not found",
                    staged.display()
                ));
            }
        }
        promote_staged(&staging, ne_dir, layer.name)?;
        if !layer_is_complete(ne_dir, layer.name) {
            return Err(format!(
                "Natural Earth layer {} is missing one or more required components after extraction",
                layer.name
            ));
        }
        remove_if_present(&zip_path)?;
        downloaded += 1;
    }

    if skipped == layers.len() {
        note(&format!(
            "all {} Natural Earth layers already present in {}",
            layers.len(),
            ne_dir.display()
        ));
    } else {
        note(&format!(
            "done: {downloaded} downloaded, {skipped} already present"
        ));
    }
    Ok(())
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{}: {e}", path.display())
}

fn promote_staged(staging: &LayerStaging, ne_dir: &Path, layer_name: &str) -> Result<(), String> {
    let required: Vec<PathBuf> = REQUIRED_COMPONENTS
        .iter()
        .map(|ext| staging.path().join(format!("{layer_name}.{ext}")))
        .collect();
    let mut extracted = Vec::new();
    for entry in std::fs::read_dir(staging.path()).map_err(io(staging.path()))? {
        let entry = entry.map_err(io(staging.path()))?;
        if entry.file_type().map_err(io(staging.path()))?.is_file() {
            extracted.push(entry.path());
        }
    }
    extracted.sort();

    // Clear any old partial set first. The required components are moved
    // last, so the final rename is the first point at which the layer can
    // pass the completeness check.
    for ext in REQUIRED_COMPONENTS {
        remove_if_present(&ne_dir.join(format!("{layer_name}.{ext}")))?;
    }
    // Extra archive files (such as .cpg) go before the required set, so they
    // cannot make a partial layer appear complete during promotion.
    let extras = extracted.iter().filter(|p| !required.contains(p));
    for path in extras.chain(required.iter()) {
        let name = path
            .file_name()
            .ok_or_else(|| format!("extracted path has no file name: {}", path.display()))?;
        std::fs::rename(path, ne_dir.join(name)).map_err(io(path))?;
    }
    Ok(())
}

fn layer_is_complete(ne_dir: &Path, name: &str) -> bool {
    REQUIRED_COMPONENTS
        .iter()
        .all(|ext| ne_dir.join(format!("{name}.{ext}")).is_file())
}

/// A per-layer extraction directory, removed on drop.
struct LayerStaging {
    path: PathBuf,
}

impl LayerStaging {
    fn create(ne_dir: &Path, layer_name: &str) -> Result<Self, String> {
        let path = ne_dir.join(format!(".{layer_name}.extracting"));
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&path).map_err(io(&path))?,
            Ok(_) => std::fs::remove_file(&path).map_err(io(&path))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(&path)(e)),
        }
        std::fs::create_dir(&path).map_err(io(&path))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LayerStaging {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.path));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fetch::fake::Fake;
    use crate::test_scratch::scratch;

    const BYTES: &[u8] = b"natural earth fixture";

    fn pin(dir: &Path) -> String {
        let probe = dir.join("probe");
        std::fs::write(&probe, BYTES).unwrap();
        elivagar_devkit::tilegen::file_digest(&probe).unwrap()
    }

    #[test]
    fn registry_is_well_formed_and_urls_are_version_pinned() {
        for l in LAYERS {
            assert_eq!(l.xxh128.len(), 32, "{}", l.name);
            assert!(l.xxh128.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(l.name.starts_with(&format!("ne_{}_", l.scale)));
            assert!(matches!(l.category, "physical" | "cultural"));
        }
        assert_eq!(
            LAYERS[0].url(),
            "https://naturalearth.s3.amazonaws.com/110m_physical/ne_110m_ocean.zip?versionId=MUObtTI2xWOdF_zuXwU_9GhzB.dVLbAN"
        );
    }

    #[test]
    fn extracts_flat_promotes_extras_and_skips_complete_layers() {
        let dir = scratch("natural_earth", "fresh");
        let ne = dir.join("natural_earth");
        let pin = pin(&dir);
        let layers = [layer("ne_t", "physical", "10m", "v", &pin)];
        let fake = Fake::new(
            BYTES,
            &["ne_t.shp", "ne_t.shx", "ne_t.dbf", "ne_t.prj", "ne_t.cpg"],
        );
        // An old partial set from an interrupted run.
        std::fs::create_dir_all(&ne).unwrap();
        std::fs::write(ne.join("ne_t.shp"), b"old").unwrap();
        run_layers(&ne, &layers, &fake).unwrap();
        assert!(fake.unzipped.borrow().iter().all(|(_, junk)| *junk));
        for ext in ["shp", "shx", "dbf", "prj", "cpg"] {
            assert_eq!(
                std::fs::read(ne.join(format!("ne_t.{ext}"))).unwrap(),
                b"component"
            );
        }
        assert!(!ne.join("ne_t.zip").exists());
        assert!(!ne.join(".ne_t.extracting").exists());
        run_layers(&ne, &layers, &fake).unwrap();
        assert_eq!(fake.fetches.get(), 1);
    }

    #[test]
    fn a_pin_mismatch_installs_nothing() {
        let dir = scratch("natural_earth", "mismatch");
        let ne = dir.join("natural_earth");
        let bad = "f".repeat(32);
        let layers = [layer("ne_t", "physical", "10m", "v", &bad)];
        let fake = Fake::new(BYTES, &["ne_t.shp", "ne_t.shx", "ne_t.dbf", "ne_t.prj"]);
        let err = run_layers(&ne, &layers, &fake).unwrap_err();
        assert!(err.contains("XXH128 mismatch"), "{err}");
        assert_eq!(fake.unzips.get(), 0);
        assert!(!layer_is_complete(&ne, "ne_t"));
        assert!(!ne.join("ne_t.zip").exists());
    }

    #[test]
    fn a_missing_component_refuses_and_leaves_no_staging() {
        let dir = scratch("natural_earth", "missing");
        let ne = dir.join("natural_earth");
        let pin = pin(&dir);
        let layers = [layer("ne_t", "physical", "10m", "v", &pin)];
        let fake = Fake::new(BYTES, &["ne_t.shp", "ne_t.shx", "ne_t.dbf"]);
        let err = run_layers(&ne, &layers, &fake).unwrap_err();
        assert!(err.contains("ne_t.prj"), "{err}");
        assert!(!ne.join("ne_t.shp").exists());
        assert!(!ne.join(".ne_t.extracting").exists());
    }
}
