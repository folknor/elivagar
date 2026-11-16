//! `download-ocean`: the EPSG:3857 water polygons from osmdata.openstreetmap.de,
//! full resolution and simplified, extracted under the data dir.
//!
//! Ported from brokkr's `download_ocean.rs`. The destinations are unchanged,
//! so the `[<host>.tilegen.default]` ocean specs
//! (`z8-z14:water-polygons-split-3857/water_polygons.shp`,
//! `z0-z7:simplified-water-polygons-split-3857/simplified_water_polygons.shp`)
//! keep resolving. Idempotent: a complete tree is never re-fetched.

use std::path::Path;

use crate::fetch::{Tools, download, note, remove_if_present};

pub(crate) struct OceanVariant {
    pub(crate) url: &'static str,
    pub(crate) zip_name: &'static str,
    pub(crate) dir_name: &'static str,
    pub(crate) shp_name: &'static str,
    label: &'static str,
    size_hint: &'static str,
}

pub(crate) const FULL_RES: OceanVariant = OceanVariant {
    url: "https://osmdata.openstreetmap.de/download/water-polygons-split-3857.zip",
    zip_name: "water-polygons-split-3857.zip",
    dir_name: "water-polygons-split-3857",
    shp_name: "water_polygons.shp",
    label: "full-resolution ocean polygons",
    size_hint: "~765 MB",
};

pub(crate) const SIMPLIFIED: OceanVariant = OceanVariant {
    url: "https://osmdata.openstreetmap.de/download/simplified-water-polygons-split-3857.zip",
    zip_name: "simplified-water-polygons-split-3857.zip",
    dir_name: "simplified-water-polygons-split-3857",
    shp_name: "simplified_water_polygons.shp",
    label: "simplified ocean polygons",
    size_hint: "~13 MB",
};

/// Marker written into an extracted shapefile directory once the extraction
/// that produced it has finished. The `.shp` alone is not evidence of a
/// complete set: an interrupted `unzip` can leave it present - even
/// truncated - beside a missing `.dbf`/`.shx`.
///
/// The name is brokkr's, kept so trees brokkr extracted stay recognized.
/// A tree extracted before the marker existed is adopted (marker written)
/// when its whole component set is present, rather than re-fetched: these
/// files feed tilegen's ocean inputs, whose digests the corpus contract and
/// the ocean artifact key pin, and osmdata regenerates them daily - a silent
/// re-download would change every archive's ocean. The adoption cannot see
/// a truncated `.shp`; only extractions this code performed carry that
/// guarantee.
///
/// There is no content hash to verify against: osmdata regenerates these
/// archives daily under fixed URLs, so any pinned digest would be stale
/// within a day. Completeness is what can be checked here.
pub(crate) const COMPLETE_MARKER: &str = ".brokkr-complete";

pub(crate) fn run(data_dir: &Path, tools: &dyn Tools) -> Result<(), String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    download_variant(data_dir, &FULL_RES, tools)?;
    download_variant(data_dir, &SIMPLIFIED, tools)
}

/// Whether the shapefile at `shp` is complete: marked by a finished
/// extraction, or (pre-marker trees) carrying every component file, in which
/// case the marker is written now.
fn shapefile_complete(shp: &Path) -> bool {
    let Some(dir) = shp.parent() else {
        return false;
    };
    let marker = dir.join(COMPLETE_MARKER);
    if marker.exists() {
        return shp.exists();
    }
    let legacy_complete = ["shp", "shx", "dbf", "prj"]
        .iter()
        .all(|ext| shp.with_extension(ext).exists());
    legacy_complete && std::fs::write(&marker, b"").is_ok()
}

pub(crate) fn download_variant(
    data_dir: &Path,
    variant: &OceanVariant,
    tools: &dyn Tools,
) -> Result<(), String> {
    let shp_path = data_dir.join(variant.dir_name).join(variant.shp_name);
    let marker = data_dir.join(variant.dir_name).join(COMPLETE_MARKER);

    if shapefile_complete(&shp_path) {
        note(&format!(
            "{} already exists: {}",
            variant.label,
            shp_path.display()
        ));
        return Ok(());
    }

    let zip_path = data_dir.join(variant.zip_name);
    note(&format!(
        "downloading {} ({})",
        variant.label, variant.size_hint
    ));
    // No pin, deliberately: see `COMPLETE_MARKER`. `download` guarantees a
    // complete transfer only.
    download(tools, variant.url, &zip_path, None)?;

    note("extracting...");
    tools.unzip(&zip_path, data_dir, false)?;
    if !shp_path.exists() {
        return Err(format!(
            "unzip succeeded but {} is missing - archive layout changed?",
            shp_path.display()
        ));
    }
    std::fs::write(&marker, b"").map_err(|e| format!("{}: {e}", marker.display()))?;
    remove_if_present(&zip_path)?;
    note(&format!("done: {}", shp_path.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fetch::fake::Fake;
    use crate::test_scratch::scratch;

    fn components(variant: &OceanVariant) -> Vec<String> {
        ["shp", "shx", "dbf", "prj", "cpg"]
            .iter()
            .map(|ext| {
                let stem = variant.shp_name.trim_end_matches(".shp");
                format!("{}/{stem}.{ext}", variant.dir_name)
            })
            .collect()
    }

    #[test]
    fn destinations_match_the_tilegen_ocean_specs() {
        // The relative paths brokkr.d/hosts/*.toml name in tilegen.default.
        for (variant, spec) in [
            (&FULL_RES, "water-polygons-split-3857/water_polygons.shp"),
            (
                &SIMPLIFIED,
                "simplified-water-polygons-split-3857/simplified_water_polygons.shp",
            ),
        ] {
            assert_eq!(format!("{}/{}", variant.dir_name, variant.shp_name), spec);
            assert!(variant.url.ends_with(variant.zip_name));
        }
    }

    #[test]
    fn extracts_marks_removes_the_zip_and_is_idempotent() {
        let dir = scratch("ocean", "fresh");
        let mut entries = components(&FULL_RES);
        entries.extend(components(&SIMPLIFIED));
        let entries: Vec<&str> = entries.iter().map(String::as_str).collect();
        let fake = Fake::new(b"zip", &entries);
        run(&dir, &fake).unwrap();
        assert_eq!(fake.fetches.get(), 2);
        assert_eq!(
            *fake.urls.borrow(),
            [FULL_RES.url.to_owned(), SIMPLIFIED.url.to_owned()]
        );
        // Extracted into the data dir itself, paths kept (no -j).
        assert!(fake.unzipped.borrow().iter().all(|(d, j)| d == &dir && !j));
        for variant in [&FULL_RES, &SIMPLIFIED] {
            assert!(dir.join(variant.dir_name).join(COMPLETE_MARKER).is_file());
            assert!(!dir.join(variant.zip_name).exists());
        }
        run(&dir, &fake).unwrap();
        assert_eq!(fake.fetches.get(), 2);
    }

    #[test]
    fn a_complete_pre_marker_tree_is_adopted_not_refetched() {
        let dir = scratch("ocean", "legacy");
        for entry in components(&FULL_RES) {
            let path = dir.join(entry);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }
        let fake = Fake::new(b"zip", &[]);
        download_variant(&dir, &FULL_RES, &fake).unwrap();
        assert_eq!(fake.fetches.get(), 0);
        assert!(dir.join(FULL_RES.dir_name).join(COMPLETE_MARKER).is_file());
    }

    #[test]
    fn an_unmarked_partial_tree_is_refetched() {
        let dir = scratch("ocean", "partial");
        let shp = dir.join(FULL_RES.dir_name).join(FULL_RES.shp_name);
        std::fs::create_dir_all(shp.parent().unwrap()).unwrap();
        std::fs::write(&shp, b"truncated").unwrap();
        let entries = components(&FULL_RES);
        let entries: Vec<&str> = entries.iter().map(String::as_str).collect();
        let fake = Fake::new(b"zip", &entries);
        download_variant(&dir, &FULL_RES, &fake).unwrap();
        assert_eq!(fake.fetches.get(), 1);
        assert_eq!(std::fs::read(&shp).unwrap(), b"component");
    }

    #[test]
    fn a_changed_archive_layout_refuses_without_marking() {
        let dir = scratch("ocean", "layout");
        let fake = Fake::new(b"zip", &["elsewhere/water_polygons.shp"]);
        let err = download_variant(&dir, &FULL_RES, &fake).unwrap_err();
        assert!(err.contains("archive layout changed"), "{err}");
        assert!(!dir.join(FULL_RES.dir_name).join(COMPLETE_MARKER).exists());
    }
}
