//! The slice of brokkr's host configuration elivagar's tools need: where the
//! project root is, the selected host's data, scratch and tilegen output
//! directories, and the host table itself (its tilegen blocks and datasets).
//!
//! Composition follows brokkr's rules for tables, as nidhogg's devkit does:
//! `brokkr.toml` and its recursive includes are merged with an included file
//! merged before the file that includes it, so the includer wins, and tables
//! merge key by key. The host is picked by kernel hostname unless named, as
//! `[host.<name>]` or the legacy `[<name>]`. An unconfigured host gets
//! brokkr's defaults. The user-wide brokkr config is not read: brokkr refuses
//! host tables there, so it cannot change anything resolved here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::error::DevError;

/// brokkr's defaults for a host that does not set them.
const DEFAULT_DATA: &str = "data";
const DEFAULT_SCRATCH: &str = "data/scratch";
const DEFAULT_OUTPUT: &str = "data/tilegen";
/// brokkr's per-project state directory, holding the corpus sort spill.
const PROJECT_STATE_DIR: &str = ".brokkr";
/// Where `mutate` puts a calibrand by default, under the data dir. `brokkr
/// clean` clears it.
const CORPUS_CALIBRAND_DIR: &str = "corpus-calibrands";
/// The `--tmp-dir` tilegen passes elivagar, under the data dir.
const TILEGEN_TMP_DIR: &str = "tilegen_tmp";
/// tilegen's scratch output, under the scratch dir, before admission.
const TILEGEN_SCRATCH_OUTPUT: &str = "bench-self-output.pmtiles";

/// Resolved paths for one host.
#[derive(Debug)]
pub struct Paths {
    /// The directory holding `brokkr.toml`: the repo root, where the
    /// committed corpus lives and git is asked.
    pub root: PathBuf,
    /// The host section name selected.
    pub hostname: String,
    pub data_dir: PathBuf,
    pub scratch_dir: PathBuf,
    pub output_dir: PathBuf,
    /// The selected host's composed table, empty for an unconfigured host.
    pub host: toml::Table,
}

impl Paths {
    /// Spill directory for the corpus digest's external sort, the same one
    /// brokkr used.
    pub fn corpus_sort_dir(&self) -> PathBuf {
        self.root.join(PROJECT_STATE_DIR).join("corpus-sort")
    }

    pub fn corpus_calibrands(&self) -> PathBuf {
        self.data_dir.join(CORPUS_CALIBRAND_DIR)
    }

    /// elivagar's `--tmp-dir`. elivagar empties it at every run start.
    pub fn tilegen_tmp(&self) -> PathBuf {
        self.data_dir.join(TILEGEN_TMP_DIR)
    }

    /// Where tilegen writes its archive before admission into the store.
    pub fn tilegen_scratch_output(&self) -> PathBuf {
        self.scratch_dir.join(TILEGEN_SCRATCH_OUTPUT)
    }
}

fn config(message: impl Into<String>) -> DevError {
    DevError::Config(message.into())
}

/// Find `brokkr.toml` (the working directory or its parent), compose it and
/// resolve `host` (default: the kernel hostname).
pub fn load(host: Option<&str>) -> Result<Paths, DevError> {
    load_from(&find_project_file()?, host)
}

/// Compose `project_file` and its includes and resolve `host` (default: the
/// kernel hostname). [`load`] is this with the file found from the working
/// directory.
pub fn load_from(project_file: &Path, host: Option<&str>) -> Result<Paths, DevError> {
    let parent = project_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let root =
        std::fs::canonicalize(parent).map_err(|e| config(format!("{}: {e}", parent.display())))?;
    let mut table = toml::Table::new();
    let mut stack = Vec::new();
    let mut seen = BTreeSet::new();
    visit(project_file, &mut stack, &mut seen, &mut table)?;

    let hostname = match host {
        Some(h) => h.to_owned(),
        None => std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map_err(|e| config(format!("reading the kernel hostname: {e}")))?
            .trim()
            .to_owned(),
    };
    let host_table = table
        .get("host")
        .and_then(|h| h.get(&hostname))
        .or_else(|| table.get(&hostname))
        .and_then(toml::Value::as_table);
    let path_key = |key: &str, default: &str| -> Result<PathBuf, DevError> {
        match host_table.and_then(|h| h.get(key)) {
            None => Ok(root.join(default)),
            Some(toml::Value::String(s)) => Ok(root.join(s)),
            Some(_) => Err(config(format!(
                "host '{hostname}' {key} must be a path string"
            ))),
        }
    };
    let data_dir = path_key("data", DEFAULT_DATA)?;
    let scratch_dir = path_key("scratch", DEFAULT_SCRATCH)?;
    let output_dir = path_key("output", DEFAULT_OUTPUT)?;
    let host = host_table.cloned().unwrap_or_default();
    Ok(Paths {
        root,
        hostname,
        data_dir,
        scratch_dir,
        output_dir,
        host,
    })
}

fn find_project_file() -> Result<PathBuf, DevError> {
    let cwd = std::env::current_dir().map_err(|e| config(format!("working directory: {e}")))?;
    let here = cwd.join("brokkr.toml");
    if here.is_file() {
        return Ok(here);
    }
    if let Some(parent) = cwd.parent() {
        let up = parent.join("brokkr.toml");
        if up.is_file() {
            return Ok(up);
        }
    }
    Err(config(format!(
        "no brokkr.toml in {} or its parent",
        cwd.display()
    )))
}

fn visit(
    path: &Path,
    stack: &mut Vec<PathBuf>,
    seen: &mut BTreeSet<PathBuf>,
    merged: &mut toml::Table,
) -> Result<(), DevError> {
    let at = |m: String| config(format!("{}: {m}", path.display()));
    let canonical = std::fs::canonicalize(path).map_err(|e| at(e.to_string()))?;
    if stack.contains(&canonical) {
        return Err(at("include cycle".into()));
    }
    if !seen.insert(canonical.clone()) {
        return Err(at("included more than once".into()));
    }
    let text = std::fs::read_to_string(&canonical).map_err(|e| at(e.to_string()))?;
    let mut table: toml::Table = toml::from_str(&text).map_err(|e| at(e.to_string()))?;
    let includes = match table.remove("include") {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                toml::Value::String(s) if !s.trim().is_empty() => Ok(s),
                _ => Err(at("'include' entries must be non-empty path strings".into())),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(at("'include' must be an array of paths".into())),
    };
    let parent = canonical
        .parent()
        .ok_or_else(|| at("config has no parent".into()))?
        .to_owned();
    stack.push(canonical);
    for name in includes {
        visit(&parent.join(name), stack, seen, merged)?;
    }
    stack.pop();
    merge(merged, table);
    Ok(())
}

fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}
