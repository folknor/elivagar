//! EPSG:4326 ocean shapefiles for Tilemaker, which reads lon/lat only.
//! Ported from brokkr's `ensure_ocean_4326` (`src/elivagar/download_ocean.rs`):
//! the full-resolution set is downloaded as published, the simplified set
//! reprojected from the 3857 one with `ogr2ogr`, as shortbread-tilemaker's
//! own `get-shapefiles.sh` does.
//!
//! The simplified 3857 source is the one tilegen reads
//! (`brokkr run elivagar-datasets -- download-ocean` fetches it). It is downloaded
//! here only when absent, into the same directory under the same
//! completeness marker, so the two never hold different releases side by
//! side. There is no content pin: osmdata regenerates these archives daily
//! under fixed URLs.

use std::path::Path;

use elivagar_devkit::DevError;

use crate::fetch;

/// Written into a shapefile directory once the extraction or reprojection
/// that produced it has finished. Shared with `elivagar-datasets` and
/// brokkr, so a tree either of them completed is adopted.
const COMPLETE_MARKER: &str = ".brokkr-complete";

pub const FULL_4326_DIR: &str = "water-polygons-split-4326";
const FULL_4326_SHP: &str = "water_polygons.shp";
const FULL_4326_URL: &str =
    "https://osmdata.openstreetmap.de/download/water-polygons-split-4326.zip";
const FULL_4326_ZIP: &str = "water-polygons-split-4326.zip";

pub const SIMPLIFIED_4326_DIR: &str = "simplified-water-polygons-split-4326";
const SIMPLIFIED_4326_SHP: &str = "simplified_water_polygons.shp";

const SIMPLIFIED_3857_DIR: &str = "simplified-water-polygons-split-3857";
const SIMPLIFIED_3857_SHP: &str = "simplified_water_polygons.shp";
const SIMPLIFIED_3857_URL: &str =
    "https://osmdata.openstreetmap.de/download/simplified-water-polygons-split-3857.zip";
const SIMPLIFIED_3857_ZIP: &str = "simplified-water-polygons-split-3857.zip";

/// Whether `shp` is complete: marked by a finished extraction, or a
/// pre-marker tree carrying every component, which is then marked.
fn shapefile_complete(shp: &Path) -> bool {
    let Some(dir) = shp.parent() else {
        return false;
    };
    let marker = dir.join(COMPLETE_MARKER);
    if marker.exists() {
        return shp.exists();
    }
    let legacy = ["shp", "shx", "dbf", "prj"]
        .iter()
        .all(|ext| shp.with_extension(ext).exists());
    legacy && std::fs::write(&marker, b"").is_ok()
}

/// Download and unzip one published archive unless already complete.
fn download_variant(
    data_dir: &Path,
    url: &str,
    zip_name: &str,
    dir_name: &str,
    shp_name: &str,
) -> Result<(), DevError> {
    let shp = data_dir.join(dir_name).join(shp_name);
    if shapefile_complete(&shp) {
        fetch::msg(&format!("already present: {}", shp.display()));
        return Ok(());
    }
    let zip = data_dir.join(zip_name);
    fetch::msg(&format!("downloading {url}"));
    fetch::download(url, &zip, None)?;
    let zip_str = zip.display().to_string();
    let data_str = data_dir.display().to_string();
    fetch::run("unzip", &["-o", &zip_str, "-d", &data_str], data_dir)?;
    if !shp.exists() {
        return Err(DevError::Config(format!(
            "unzip succeeded but {} is missing - archive layout changed?",
            shp.display()
        )));
    }
    std::fs::write(data_dir.join(dir_name).join(COMPLETE_MARKER), b"")?;
    drop(std::fs::remove_file(&zip));
    fetch::msg(&format!("done: {}", shp.display()));
    Ok(())
}

/// Ensure both 4326 ocean sets under `data_dir`.
pub fn ensure_4326(data_dir: &Path) -> Result<(), DevError> {
    fetch::require_programs(&["curl", "unzip"], "to fetch the ocean shapefiles")?;
    std::fs::create_dir_all(data_dir)?;
    download_variant(
        data_dir,
        FULL_4326_URL,
        FULL_4326_ZIP,
        FULL_4326_DIR,
        FULL_4326_SHP,
    )?;

    let simplified = data_dir.join(SIMPLIFIED_4326_DIR).join(SIMPLIFIED_4326_SHP);
    if shapefile_complete(&simplified) {
        fetch::msg(&format!("already present: {}", simplified.display()));
        return Ok(());
    }
    download_variant(
        data_dir,
        SIMPLIFIED_3857_URL,
        SIMPLIFIED_3857_ZIP,
        SIMPLIFIED_3857_DIR,
        SIMPLIFIED_3857_SHP,
    )?;
    fetch::require_programs(&["ogr2ogr"], "for the EPSG:4326 reprojection")?;
    std::fs::create_dir_all(data_dir.join(SIMPLIFIED_4326_DIR))?;
    let src = data_dir.join(SIMPLIFIED_3857_DIR).join(SIMPLIFIED_3857_SHP);
    let src_str = src.display().to_string();
    let dst_str = simplified.display().to_string();
    fetch::msg("reprojecting simplified ocean polygons to EPSG:4326");
    fetch::run(
        "ogr2ogr",
        &[
            // A partial reprojection may have left output behind.
            "-overwrite",
            "-f",
            "ESRI Shapefile",
            &dst_str,
            &src_str,
            "-t_srs",
            "EPSG:4326",
            "-lco",
            "ENCODING=utf8",
        ],
        data_dir,
    )?;
    std::fs::write(
        data_dir.join(SIMPLIFIED_4326_DIR).join(COMPLETE_MARKER),
        b"",
    )?;
    fetch::msg(&format!("done: {}", simplified.display()));
    Ok(())
}
