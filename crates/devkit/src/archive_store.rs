//! Durable storage for tilegen PMTiles archives: naming, resolution,
//! admission and retention, in the host's output directory (default
//! `data/tilegen`). Ported from brokkr's `elivagar/archive_store.rs`; the
//! names it constructs, `<dataset>-<variant>-<commit>.pmtiles`, are the ones
//! brokkr wrote, so archives brokkr admitted stay addressable.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::DevError;

/// Maximum number of archives tilegen keeps for one `(dataset, variant)`.
pub const TILEGEN_RETENTION: usize = 5;

/// The three values that identify a durable tilegen archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveKey {
    dataset: String,
    variant: String,
    commit: String,
}

impl ArchiveKey {
    /// Construct an archive key, refusing path components in the commit token.
    pub fn new(
        dataset: impl Into<String>,
        variant: impl Into<String>,
        commit: impl Into<String>,
    ) -> Result<Self, DevError> {
        let dataset = dataset.into();
        let variant = variant.into();
        let commit = commit.into();
        if !safe_component(&dataset) || !safe_component(&variant) || !safe_component(&commit) {
            return Err(DevError::Config(
                "PMTiles archive key fields must be non-empty filename components".into(),
            ));
        }
        if commit.contains('-') {
            return Err(DevError::Config(
                "PMTiles archive commit token cannot contain a hyphen".into(),
            ));
        }
        Ok(Self {
            dataset,
            variant,
            commit,
        })
    }

    /// The revision token the archive is named by.
    pub fn commit(&self) -> &str {
        &self.commit
    }
}

/// An archive [`ArchiveStore::admit`] placed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Admitted {
    pub path: PathBuf,
    /// Whether an archive already at the key was replaced.
    pub replaced: bool,
}

/// Owner of the durable PMTiles archive directory and its naming/retention
/// rules.
#[derive(Clone, Debug)]
pub struct ArchiveStore {
    dir: PathBuf,
}

impl ArchiveStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Construct the only canonical durable archive path.
    pub fn path_for(&self, key: &ArchiveKey) -> PathBuf {
        self.dir.join(format!(
            "{}-{}-{}.pmtiles",
            key.dataset, key.variant, key.commit
        ))
    }

    /// Resolve an explicit file or a configured store key. A revision that is
    /// no longer in git may still select a stored archive by its exact token or
    /// a unique legacy short hash; an unresolvable typo gets a git error rather
    /// than a misleading "no build" message.
    pub fn resolve(
        &self,
        dataset: &str,
        variant: &str,
        revision: Option<&str>,
        file: Option<&str>,
        build_root: &Path,
    ) -> Result<PathBuf, DevError> {
        if let Some(file) = file {
            let path = PathBuf::from(file);
            if !path.exists() {
                return Err(DevError::Config(format!(
                    "file not found: {}",
                    path.display()
                )));
            }
            return Ok(path);
        }

        let key = match revision {
            Some(revision) => self.resolve_revision(dataset, variant, revision, build_root)?,
            None => {
                let commit = crate::git::head(build_root)?.short();
                ArchiveKey::new(dataset, variant, commit)?
            }
        };
        let path = self.path_for(&key);
        if !path.exists() {
            return Err(DevError::Config(format!(
                "no {} build for {}; run `brokkr run --with elivagar --with-commit {} \
                 elivagar-devkit -- tilegen --dataset {} --variant {}` first (looked for {})",
                key.variant,
                key.commit,
                key.commit,
                key.dataset,
                key.variant,
                path.display()
            )));
        }
        Ok(path)
    }

    /// Move one successful tilegen output into the store and apply the
    /// store-owned per-group retention window. The protected directories are
    /// the caller's scratch and tool-temporary directories.
    ///
    /// The key holds the latest admitted archive of its revision: an archive
    /// already at the key is replaced, whatever tilegen block built either,
    /// and the result says so. The key carries no block identity on purpose -
    /// every reader resolves by revision - and each archive records the
    /// contract elivagar built it under in its own metadata.
    pub fn admit(
        &self,
        scratch_file: &Path,
        key: &ArchiveKey,
        protected_dirs: &[&Path],
    ) -> Result<Admitted, DevError> {
        let scratch_dir = scratch_file.parent().unwrap_or_else(|| Path::new(""));
        if self.dir.as_path() == scratch_dir || protected_dirs.contains(&self.dir.as_path()) {
            return Err(DevError::Config(format!(
                "output dir {} coincides with elivagar's scratch/tmp dir; set a distinct [<host>].output in brokkr.toml. Output left in {}",
                self.dir.display(),
                scratch_file.display(),
            )));
        }

        if !scratch_file.exists() {
            return Err(DevError::Verify(format!(
                "tilegen exited 0 but wrote no output at {}",
                scratch_file.display()
            )));
        }
        std::fs::create_dir_all(&self.dir).map_err(|error| {
            DevError::Io(io::Error::new(
                error.kind(),
                format!(
                    "failed to create output dir {}: {error}; output left in {}",
                    self.dir.display(),
                    scratch_file.display(),
                ),
            ))
        })?;

        let destination = self.path_for(key);
        // Asked before the rename, which replaces silently. Nothing else
        // writes the store while tilegen holds the brokkr lock.
        let replaced = destination.exists();
        std::fs::rename(scratch_file, &destination).map_err(|error| {
            DevError::Io(io::Error::new(
                error.kind(),
                format!(
                    "failed to move {} to {}: {error}; output left in place",
                    scratch_file.display(),
                    destination.display(),
                ),
            ))
        })?;

        // Retention is best-effort after admission, as a pruning failure must
        // not turn a successfully stored archive into a failed tilegen run.
        drop(self.prune_group(&key.dataset, &key.variant, TILEGEN_RETENTION));
        Ok(Admitted {
            path: destination,
            replaced,
        })
    }

    /// Remove archives of one `(dataset, variant)` beyond the newest `keep`
    /// by modification time, returning how many were removed.
    fn prune_group(&self, dataset: &str, variant: &str, keep: usize) -> io::Result<usize> {
        let mut removed = 0;
        for path in self.prune_candidates(dataset, variant, keep)? {
            if std::fs::remove_file(path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// The archives of one `(dataset, variant)` beyond the newest `keep` by
    /// modification time: what retention, and `clean --archives --keep N`,
    /// remove. Only names [`Self::path_for`] constructs for the group are
    /// candidates, so hand-named files, the ocean artifact and pre-rename
    /// archives are never among them. Filenames are never parsed back into
    /// keys, since dataset names carry hyphens.
    pub fn prune_candidates(
        &self,
        dataset: &str,
        variant: &str,
        keep: usize,
    ) -> io::Result<Vec<PathBuf>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut archives: Vec<(SystemTime, PathBuf)> = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !Self::matches_name(name, dataset, variant) {
                continue;
            }
            let metadata = entry.metadata()?;
            if !metadata.is_file() {
                continue;
            }
            archives.push((metadata.modified()?, path));
        }
        archives.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        Ok(archives
            .into_iter()
            .skip(keep)
            .map(|(_, path)| path)
            .collect())
    }

    fn matches_name(file_name: &str, dataset: &str, variant: &str) -> bool {
        file_name
            .strip_prefix(&archive_prefix(dataset, variant))
            .and_then(|rest| rest.strip_suffix(".pmtiles"))
            .is_some_and(|commit| !commit.is_empty() && !commit.contains('-'))
    }

    fn resolve_revision(
        &self,
        dataset: &str,
        variant: &str,
        revision: &str,
        build_root: &Path,
    ) -> Result<ArchiveKey, DevError> {
        match crate::git::resolve_commit(build_root, revision) {
            Ok(id) => {
                let fixed = ArchiveKey::new(dataset, variant, id.short())?;
                if self.path_for(&fixed).exists() {
                    return Ok(fixed);
                }
                let commit = self
                    .legacy_archive_token(dataset, variant, id.as_str())
                    .unwrap_or_else(|| id.short());
                ArchiveKey::new(dataset, variant, commit)
            }
            Err(error) => {
                if safe_component(revision) && !revision.contains('-') {
                    let exact = ArchiveKey::new(dataset, variant, revision)?;
                    if self.path_for(&exact).exists() {
                        return Ok(exact);
                    }
                    if revision.len() >= 4
                        && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                        && let Some(commit) = self.legacy_archive_token(dataset, variant, revision)
                    {
                        return ArchiveKey::new(dataset, variant, commit);
                    }
                }
                Err(DevError::Config(format!(
                    "cannot resolve git revision '{revision}': {error}"
                )))
            }
        }
    }

    fn legacy_archive_token(&self, dataset: &str, variant: &str, full: &str) -> Option<String> {
        let prefix = archive_prefix(dataset, variant);
        let mut found: Vec<String> = std::fs::read_dir(&self.dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| Self::matches_name(name, dataset, variant))
            .filter_map(|name| {
                let token = name
                    .strip_prefix(&prefix)?
                    .strip_suffix(".pmtiles")?
                    .to_owned();
                (token.len() >= 4
                    && token.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && full.starts_with(&token))
                .then_some(token)
            })
            .collect();
        if found.len() == 1 { found.pop() } else { None }
    }
}

/// The commit the elivagar that wrote `archive` was built from, read from the
/// archive's own provenance (`elivagar.build.elivagar.commit`).
///
/// This, not the live checkout's HEAD, is what an admitted archive is named
/// by: `brokkr run --with elivagar --with-commit X` runs a binary built from
/// X while HEAD names something else, and naming its output by HEAD would
/// file X's tiles under the wrong revision. Every way of not knowing - no
/// readable metadata, no contract, no build block, the `unknown` stamp a
/// build without git records, anything but a full object id - is an error,
/// never a fallback to HEAD.
pub fn built_commit(archive: &Path) -> Result<crate::git::Commit, DevError> {
    use elivagar::provenance::{ContractState, extract_contract};

    let refuse = |why: String| {
        DevError::Verify(format!(
            "cannot name {}: {why}; the commit that built it is unknown",
            archive.display()
        ))
    };
    let unreadable =
        |e: elivagar::DecodeError| refuse(format!("unreadable archive metadata ({e})"));
    let metadata = elivagar::archive::Archive::open(archive)
        .map_err(unreadable)?
        .metadata()
        .map_err(unreadable)?;
    let doc = match extract_contract(&metadata) {
        ContractState::Contract(doc) => doc,
        ContractState::Absent => return Err(refuse("no elivagar provenance".into())),
        ContractState::Unavailable(why) => {
            return Err(refuse(format!("provenance unavailable ({why})")));
        }
        ContractState::Invalid => return Err(refuse("invalid provenance".into())),
        ContractState::UnknownSchema(schema) => {
            return Err(refuse(format!("unknown provenance schema {schema}")));
        }
        ContractState::Incomplete(field) => {
            return Err(refuse(format!("provenance has no {field}")));
        }
    };
    let commit = doc
        .build
        .get("elivagar")
        .and_then(|repo| repo.get("commit")?.as_str())
        .ok_or_else(|| refuse("provenance records no build.elivagar.commit".into()))?;
    crate::git::Commit::parse(commit)
        .map_err(|e| refuse(format!("build.elivagar.commit is {commit:?} ({e})")))
}

fn archive_prefix(dataset: &str, variant: &str) -> String {
    format!("{dataset}-{variant}-")
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::{ArchiveKey, ArchiveStore};
    use std::path::Path;

    // Ported from brokkr's datasets/tests.rs.
    #[test]
    fn archive_matcher_accepts_only_constructed_names() {
        let store = ArchiveStore::new(Path::new("/out"));
        let key = ArchiveKey::new("north-america", "raw", "abc1234").expect("key");
        let name = store.path_for(&key);
        let file = name.file_name().and_then(|f| f.to_str()).expect("name");
        assert!(ArchiveStore::matches_name(file, "north-america", "raw"));
        // A split that constructs the same name is the same group by
        // definition - and prunes it identically. Not a bleed.
        assert!(ArchiveStore::matches_name(file, "north", "america-raw"));
        assert!(!ArchiveStore::matches_name(
            "ocean-tiles.pmtiles",
            "ocean",
            "raw"
        ));
        // The pre-rename `<dataset>-<commit>` form.
        assert!(!ArchiveStore::matches_name(
            "denmark-abc1234.pmtiles",
            "denmark",
            "raw"
        ));
        assert!(!ArchiveStore::matches_name(
            "denmark-raw-.pmtiles",
            "denmark",
            "raw"
        ));
        assert!(!ArchiveStore::matches_name(
            "denmark-raw-abc1234.pmtiles.tmp",
            "denmark",
            "raw"
        ));
    }

    #[test]
    fn archive_matcher_does_not_let_a_variant_claim_its_dash_extension() {
        assert!(ArchiveStore::matches_name(
            "denmark-raw-fast-abc1234.pmtiles",
            "denmark",
            "raw-fast"
        ));
        assert!(!ArchiveStore::matches_name(
            "denmark-raw-fast-abc1234.pmtiles",
            "denmark",
            "raw"
        ));
    }
}
