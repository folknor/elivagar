//! The pinned Tilemaker build and shortbread-tilemaker config the
//! `tilemaker` bench runs (benches/compare/tilemaker.lua). Ported from
//! brokkr's `ensure_tilemaker` (`src/tools.rs`), same pins and layout under
//! the host data dir:
//!
//! ```text
//! <data>/tilemaker/                    pinned source checkout
//! <data>/tilemaker-build/tilemaker     the binary   <data>/.tilemaker-version
//! <data>/shortbread-tilemaker/         pinned config checkout
//! <data>/shortbread-tilemaker-config.json   its config.json, sources absolute
//! ```
//!
//! The resolved config is new. Tilemaker opens a layer's `source`
//! shapefile relative to its working directory and silently skips one it
//! cannot open (`ShpProcessor::read` returns on a null handle). The config
//! names `data/...` relative to the shortbread-tilemaker checkout, where its
//! tracked `data/admin-points-4326` lives and where `get-shapefiles.sh`
//! puts the ocean; brokkr ran tilemaker from the project root, so the
//! boundary label points were never read and the ocean was found only
//! while the host data dir happened to be `<project>/data`. Here every
//! relative source is made absolute against the checkout, and each one is
//! required to exist before the install reports success.

use std::path::{Path, PathBuf};

use elivagar_devkit::DevError;

use crate::{fetch, ocean};

const TILEMAKER_COMMIT: &str = "facdf933f4f5ac2944a9d65c27970270583d68fa";
const SHORTBREAD_COMMIT: &str = "9ec009826b49b71bb1481a1c28c3681766c600e3";

/// The config the bench passes as `--config`.
pub const RESOLVED_CONFIG: &str = "shortbread-tilemaker-config.json";

pub fn install(data_dir: &Path) -> Result<(), DevError> {
    fetch::require_programs(&["git", "cmake", "g++", "make"], "to build tilemaker")?;
    std::fs::create_dir_all(data_dir)?;

    let source = ensure_pinned_git_source(
        data_dir,
        "tilemaker",
        "https://github.com/systemed/tilemaker.git",
        TILEMAKER_COMMIT,
        // brokkr's earlier layout built inside the checkout.
        &[":(exclude,top)build/"],
    )?;
    let binary = build(data_dir, &source)?;

    let shortbread = ensure_pinned_git_source(
        data_dir,
        "shortbread-tilemaker",
        "https://github.com/shortbread-tiles/shortbread-tilemaker.git",
        SHORTBREAD_COMMIT,
        &[],
    )?;
    std::fs::write(
        data_dir.join(".shortbread-tilemaker-version"),
        SHORTBREAD_COMMIT,
    )?;

    ocean::ensure_4326(data_dir)?;
    // The config's ocean paths, inside the checkout's gitignored data/.
    let sb_data = shortbread.join("data");
    std::fs::create_dir_all(&sb_data)?;
    for dir in [ocean::FULL_4326_DIR, ocean::SIMPLIFIED_4326_DIR] {
        fetch::ensure_symlink(&data_dir.join(dir), &sb_data.join(dir))?;
    }

    let text = std::fs::read_to_string(shortbread.join("config.json"))?;
    let (resolved, sources) = resolve_sources(&text, &shortbread)?;
    let missing: Vec<String> = sources
        .iter()
        .filter(|p| !p.is_file())
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        return Err(DevError::Config(format!(
            "shortbread-tilemaker config names shapefiles that do not exist:\n  {}",
            missing.join("\n  ")
        )));
    }
    let config_path = data_dir.join(RESOLVED_CONFIG);
    std::fs::write(&config_path, resolved)?;

    fetch::msg(&format!("tilemaker: {}", binary.display()));
    fetch::msg(&format!("config:    {}", config_path.display()));
    fetch::msg(&format!(
        "process:   {}",
        shortbread.join("process.lua").display()
    ));
    Ok(())
}

fn build(data_dir: &Path, source: &Path) -> Result<PathBuf, DevError> {
    let version_file = data_dir.join(".tilemaker-version");
    // Generated CMake files stay outside the pinned checkout, so the next
    // source verification sees only upstream files.
    let build_dir = data_dir.join("tilemaker-build");
    let binary = build_dir.join("tilemaker");
    if binary.exists()
        && std::fs::read_to_string(&version_file).is_ok_and(|v| v.trim() == TILEMAKER_COMMIT)
    {
        fetch::msg(&format!(
            "tilemaker {} already built",
            &TILEMAKER_COMMIT[..8]
        ));
        return Ok(binary);
    }
    std::fs::create_dir_all(&build_dir)?;
    let source_str = source.display().to_string();
    let build_str = build_dir.display().to_string();
    fetch::msg("configuring tilemaker (cmake)");
    fetch::run(
        "cmake",
        &[
            "-S",
            &source_str,
            "-B",
            &build_str,
            "-DCMAKE_BUILD_TYPE=Release",
        ],
        data_dir,
    )?;
    let jobs = format!(
        "-j{}",
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    );
    fetch::msg("building tilemaker");
    fetch::run("cmake", &["--build", &build_str, "--", &jobs], data_dir)?;
    if !binary.is_file() {
        return Err(DevError::Config(format!(
            "tilemaker build finished without {}",
            binary.display()
        )));
    }
    std::fs::write(&version_file, TILEMAKER_COMMIT)?;
    fetch::msg(&format!("built tilemaker {}", &TILEMAKER_COMMIT[..8]));
    Ok(binary)
}

/// Clone (blobless, shallow) or verify a checkout at exactly `commit`.
/// An existing checkout with local changes is refused, never reset: it may
/// hold someone's work.
fn ensure_pinned_git_source(
    data_dir: &Path,
    local_name: &str,
    repository: &str,
    commit: &str,
    generated_paths: &[&str],
) -> Result<PathBuf, DevError> {
    let path = data_dir.join(local_name);
    let path_str = path.display().to_string();
    let created = !path.exists();
    if created {
        fetch::msg(&format!("cloning pinned {local_name}"));
        let cloned = fetch::run(
            "git",
            &[
                "clone",
                "--depth",
                "1",
                "--no-checkout",
                "--filter=blob:none",
                repository,
                &path_str,
            ],
            data_dir,
        );
        if let Err(error) = cloned {
            drop(std::fs::remove_dir_all(&path));
            return Err(error);
        }
    }

    let result = pin_checkout(
        data_dir,
        &path,
        local_name,
        commit,
        created,
        generated_paths,
    );
    if created && result.is_err() {
        drop(std::fs::remove_dir_all(&path));
    }
    result.map(|()| path)
}

/// Move a clean checkout at `path` to `commit` and verify it is there and
/// still clean.
fn pin_checkout(
    data_dir: &Path,
    path: &Path,
    local_name: &str,
    commit: &str,
    created: bool,
    generated_paths: &[&str],
) -> Result<(), DevError> {
    let path_str = path.display().to_string();
    let status = source_status(data_dir, &path_str, generated_paths)?;
    if !created && !status.trim().is_empty() {
        return Err(DevError::Config(format!(
            "pinned {local_name} source at {} has local changes; move or remove it before retrying:\n{}",
            path.display(),
            status.trim_end()
        )));
    }
    let head = || -> Result<String, DevError> {
        Ok(
            fetch::captured("git", &["-C", &path_str, "rev-parse", "HEAD"], data_dir)?
                .trim()
                .to_owned(),
        )
    };
    let current = head().unwrap_or_default();
    if current != commit {
        fetch::captured(
            "git",
            &["-C", &path_str, "fetch", "--depth=1", "origin", commit],
            data_dir,
        )?;
    }
    if created || current != commit {
        fetch::captured(
            "git",
            &["-C", &path_str, "checkout", "--detach", commit],
            data_dir,
        )?;
    }
    let checked_out = head()?;
    if checked_out != commit {
        return Err(DevError::Config(format!(
            "git checked out {checked_out} for pinned {local_name}, expected {commit}"
        )));
    }
    let status = source_status(data_dir, &path_str, generated_paths)?;
    if !status.trim().is_empty() {
        return Err(DevError::Config(format!(
            "pinned {local_name} source at {} is not clean after checkout",
            path.display()
        )));
    }
    fetch::msg(&format!("{local_name} at {}", &commit[..8]));
    Ok(())
}

fn source_status(cwd: &Path, source: &str, generated_paths: &[&str]) -> Result<String, DevError> {
    let mut args = vec![
        "-C",
        source,
        "status",
        "--porcelain=v1",
        "--no-renames",
        "--untracked-files=all",
    ];
    if !generated_paths.is_empty() {
        args.extend(["--", "."]);
        args.extend_from_slice(generated_paths);
    }
    fetch::captured("git", &args, cwd)
}

/// `config.json` with every relative layer `source` made absolute against
/// `base`, byte for byte otherwise (tilemaker reads its layers in file
/// order, which a parse and re-serialize through a sorted map would lose).
/// Returns the text and every source path it names.
///
/// The rewrite replaces each relative source's JSON string literal in the
/// text, then proves itself: the result must parse to exactly the original
/// document with those sources replaced, so a literal that also occurred
/// somewhere else is refused rather than silently rewritten.
fn resolve_sources(text: &str, base: &Path) -> Result<(String, Vec<PathBuf>), DevError> {
    let parse = |t: &str| -> Result<serde_json::Value, DevError> {
        serde_json::from_str(t).map_err(|e| DevError::Config(format!("config.json: {e}")))
    };
    let original = parse(text)?;
    let mut expected = original.clone();
    let layers = expected
        .get_mut("layers")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| DevError::Config("config.json has no layers object".into()))?;

    let mut out = text.to_owned();
    let mut sources = Vec::new();
    let mut rewritten = std::collections::BTreeSet::new();
    for layer in layers.values_mut() {
        let Some(source) = layer.get_mut("source") else {
            continue;
        };
        let Some(name) = source.as_str().map(str::to_owned) else {
            return Err(DevError::Config("a layer source is not a string".into()));
        };
        let path = if Path::new(&name).is_absolute() {
            PathBuf::from(&name)
        } else {
            base.join(&name)
        };
        let absolute = path
            .to_str()
            .ok_or_else(|| DevError::Config(format!("{} is not UTF-8", path.display())))?
            .to_owned();
        if absolute != name && rewritten.insert(name.clone()) {
            let from = serde_json::Value::String(name.clone()).to_string();
            let to = serde_json::Value::String(absolute.clone()).to_string();
            out = out.replace(&from, &to);
        }
        *source = serde_json::Value::String(absolute);
        sources.push(path);
    }
    if parse(&out)? != expected {
        return Err(DevError::Config(
            "config.json source rewrite touched more than the layer sources".into(),
        ));
    }
    Ok((out, sources))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const CONFIG: &str = r#"{
    "layers": {
        "zeta": { "minzoom": 2, "source": "data/a/a.shp", "source_columns": true },
        "alpha": { "minzoom": 0 },
        "ocean": { "source": "data/b/b.shp" },
        "ocean-low": { "source": "/abs/c.shp", "write_to": "ocean" }
    },
    "settings": { "name": "Shortbread" }
}"#;

    #[test]
    fn relative_sources_become_absolute_and_layer_order_survives() {
        let (out, sources) = resolve_sources(CONFIG, Path::new("/sb")).unwrap();
        assert!(out.contains(r#""source": "/sb/data/a/a.shp""#), "{out}");
        assert!(out.contains(r#""source": "/sb/data/b/b.shp""#), "{out}");
        assert!(out.contains(r#""source": "/abs/c.shp""#), "{out}");
        assert!(out.find("zeta").unwrap() < out.find("alpha").unwrap());
        // Nothing but the two relative sources changed.
        assert_eq!(out.len(), CONFIG.len() + 2 * "/sb/".len());
        let mut got: Vec<_> = sources.iter().map(|p| p.display().to_string()).collect();
        got.sort();
        assert_eq!(got, ["/abs/c.shp", "/sb/data/a/a.shp", "/sb/data/b/b.shp"]);
    }

    #[test]
    fn a_source_literal_used_elsewhere_is_refused() {
        let text = r#"{ "layers": { "x": { "source": "data/a.shp" } }, "settings": { "name": "data/a.shp" } }"#;
        assert!(resolve_sources(text, Path::new("/sb")).is_err());
    }

    #[test]
    fn a_config_without_layers_is_refused() {
        assert!(resolve_sources("{}", Path::new("/sb")).is_err());
    }
}
