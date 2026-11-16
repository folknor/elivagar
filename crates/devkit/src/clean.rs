//! `elivagar-devkit clean`: the elivagar half of what `brokkr clean` used to
//! remove, ported from brokkr's `clean.rs` elivagar branches and the
//! corpus-sort leftover sweep of its `maintenance.rs`.
//!
//! Every target is a path this kit or elivagar's corpus tool constructs by
//! name; nothing is inferred from a directory's contents:
//!
//! - `data/tilegen_tmp`, elivagar's `--tmp-dir` under tilegen, unless it is
//!   the scratch dir itself;
//! - `data/corpus-calibrands`, the default `-o` of the corpus `mutate`;
//! - `.brokkr/corpus-sort/brokkr-corpus-sort-<pid>-<id>`, the spill dirs a
//!   killed corpus sort left, when `<pid>` is no longer running;
//! - with `--archives --keep N`, stored archives beyond the newest N per
//!   configured `(dataset, variant)`, by the names the store constructs.
//!
//! Host scratch is not touched: `brokkr clean` owns it, and bench iterations
//! live there. Neither is anything that is, or contains, the project root,
//! the data, scratch or output dir, or the committed corpus.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use crate::archive_store::ArchiveStore;
use crate::config::Paths;
use crate::error::DevError;

/// The prefix the corpus sorter names its spill dirs with, brokkr's name, so
/// leftovers from either tool are recognised.
const SPILL_PREFIX: &str = "brokkr-corpus-sort-";

#[derive(Clone, Copy, Debug, Default)]
pub struct CleanOpts {
    /// Prune the archive store to this many per `(dataset, variant)`.
    pub archives: Option<usize>,
    /// Name what would go and remove nothing.
    pub dry_run: bool,
}

/// What a clean removed (or would remove), one line per target class.
#[derive(Debug, Default)]
pub struct CleanReport {
    pub lines: Vec<String>,
    /// Every path removed or, in a dry run, that would be.
    pub removed: Vec<PathBuf>,
}

/// Whether `name` is a corpus-sort spill dir: `brokkr-corpus-sort-PID-ID`
/// with both parts decimal. The same rule as brokkr's `is_spill_dir_name`.
pub fn is_spill_dir_name(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    name.strip_prefix(SPILL_PREFIX)
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(pid, id)| digits(pid) && digits(id))
}

/// The pid a spill dir name carries, when it is one.
fn spill_pid(name: &str) -> Option<u32> {
    if !is_spill_dir_name(name) {
        return None;
    }
    name.strip_prefix(SPILL_PREFIX)?
        .split_once('-')?
        .0
        .parse()
        .ok()
}

/// A spill dir is stale once the process that made it is gone. brokkr
/// proved that with its lock; this kit runs without it, so it asks /proc. A
/// recycled pid keeps a leftover one more round, never the reverse.
fn pid_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

/// Run the clean for `paths`.
pub fn clean(paths: &Paths, opts: CleanOpts) -> Result<CleanReport, DevError> {
    let mut c = Cleaner {
        dry_run: opts.dry_run,
        protected: protected(paths),
        report: CleanReport::default(),
    };
    let verb = if opts.dry_run {
        "would clean"
    } else {
        "cleaned"
    };

    let tmp = paths.tilegen_tmp();
    if tmp != paths.scratch_dir && c.dir(&tmp)? {
        c.say(format!("{verb} tilegen_tmp"));
    }
    if c.dir(&paths.corpus_calibrands())? {
        c.say(format!("{verb} corpus-calibrands"));
    }

    let mut spills = 0usize;
    for dir in stale_spill_dirs(&paths.corpus_sort_dir())? {
        spills += usize::from(c.dir(&dir)?);
    }
    if spills > 0 {
        let what = c.count_verb();
        c.say(format!(
            "{what} {}",
            count(spills, "stale corpus-sort spill dir")
        ));
    }

    if let Some(keep) = opts.archives {
        let removed = prune_archives(paths, keep, &mut c)?;
        if removed > 0 {
            let what = c.count_verb();
            c.say(format!("{what} {}", count(removed, "archive")));
        }
    }
    Ok(c.report)
}

/// The spill dirs under `dir` whose maker is no longer running.
pub fn stale_spill_dirs(dir: &Path) -> Result<Vec<PathBuf>, DevError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error("cannot read directory", dir, &error)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error("cannot read directory entry in", dir, &e))?;
        let Some(pid) = entry.file_name().to_str().and_then(spill_pid) else {
            continue;
        };
        if !pid_alive(pid) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// Prune each configured `(dataset, variant)` group to `keep`. Groups come
/// from `[<host>.datasets.<D>.pbf.<V>]`, as brokkr took them, so an archive
/// of a dataset no longer configured stays.
fn prune_archives(paths: &Paths, keep: usize, c: &mut Cleaner) -> Result<usize, DevError> {
    if paths.output_dir == paths.scratch_dir {
        return Err(DevError::Config(format!(
            "output dir {} coincides with the scratch dir; refusing to prune it",
            paths.output_dir.display()
        )));
    }
    let store = ArchiveStore::new(&paths.output_dir);
    let mut doomed = BTreeSet::new();
    for (dataset, variant) in configured_groups(paths) {
        let found = store
            .prune_candidates(&dataset, &variant, keep)
            .map_err(|e| io_error("cannot prune archives in", &paths.output_dir, &e))?;
        doomed.extend(found);
    }
    let mut removed = 0usize;
    for path in &doomed {
        removed += usize::from(c.file(path)?);
    }
    Ok(removed)
}

fn configured_groups(paths: &Paths) -> Vec<(String, String)> {
    let Some(datasets) = paths.host.get("datasets").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    let mut groups = Vec::new();
    for (dataset, entry) in datasets {
        if let Some(pbf) = entry.get("pbf").and_then(toml::Value::as_table) {
            for variant in pbf.keys() {
                groups.push((dataset.clone(), variant.clone()));
            }
        }
    }
    groups
}

/// Paths no clean target may be, or contain.
fn protected(paths: &Paths) -> Vec<PathBuf> {
    vec![
        paths.root.clone(),
        paths.root.join("corpus"),
        paths.data_dir.clone(),
        paths.scratch_dir.clone(),
        paths.output_dir.clone(),
    ]
}

struct Cleaner {
    dry_run: bool,
    protected: Vec<PathBuf>,
    report: CleanReport,
}

impl Cleaner {
    fn say(&mut self, line: String) {
        self.report.lines.push(line);
    }

    fn count_verb(&self) -> &'static str {
        if self.dry_run {
            "would remove"
        } else {
            "removed"
        }
    }

    fn refuse_protected(&self, path: &Path) -> Result<(), DevError> {
        let target = lexical(path);
        if let Some(kept) = self
            .protected
            .iter()
            .find(|p| lexical(p).starts_with(&target))
        {
            return Err(DevError::Config(format!(
                "refusing to remove {}: it is or contains {}",
                path.display(),
                kept.display()
            )));
        }
        Ok(())
    }

    /// Remove a directory, or a symlink in its place (the link, not its
    /// target). Absent is not an error. A regular file at a directory's name
    /// is not ours and is refused.
    fn dir(&mut self, path: &Path) -> Result<bool, DevError> {
        let kind = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata.file_type(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("cannot inspect", path, &error)),
        };
        if !kind.is_dir() && !kind.is_symlink() {
            return Err(DevError::Config(format!(
                "refusing to remove {}: expected a directory",
                path.display()
            )));
        }
        self.refuse_protected(path)?;
        if !self.dry_run {
            let result = if kind.is_symlink() {
                std::fs::remove_file(path)
            } else {
                std::fs::remove_dir_all(path)
            };
            match result {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(io_error("cannot remove directory", path, &error)),
            }
        }
        self.report.removed.push(path.to_path_buf());
        Ok(true)
    }

    fn file(&mut self, path: &Path) -> Result<bool, DevError> {
        self.refuse_protected(path)?;
        if !self.dry_run {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(io_error("cannot remove file", path, &error)),
            }
        }
        self.report.removed.push(path.to_path_buf());
        Ok(true)
    }
}

/// `path` with `.` and `..` components folded, without touching the
/// filesystem: configured paths are joined, never canonicalized, here.
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn count(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

fn io_error(action: &str, path: &Path, source: &io::Error) -> DevError {
    DevError::Io(io::Error::new(
        source.kind(),
        format!("{action} {}: {source}", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::{is_spill_dir_name, spill_pid};

    #[test]
    fn spill_names_match_brokkr_rule() {
        assert!(is_spill_dir_name("brokkr-corpus-sort-4242-7"));
        assert!(!is_spill_dir_name("brokkr-corpus-sort-x-7"));
        assert!(!is_spill_dir_name("brokkr-corpus-sort-4242-"));
        assert!(!is_spill_dir_name("brokkr-corpus-sort-4242"));
        assert!(!is_spill_dir_name("brokkr-corpus-sort-1-2-3"));
        assert!(!is_spill_dir_name("mine"));
        assert_eq!(spill_pid("brokkr-corpus-sort-4242-7"), Some(4242));
    }
}
