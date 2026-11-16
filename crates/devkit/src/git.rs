//! The two git questions archive naming and resolution ask: what `HEAD` is,
//! and what commit a user-typed revision names. Same abbreviation rule as
//! brokkr, which named the archives before this crate did: the first nine hex
//! digits of the full object id, never `git rev-parse --short`, whose width
//! grows with the repo.

use std::path::Path;
use std::process::Command;

use crate::error::DevError;

/// Width of every abbreviated commit hash an archive is named by.
pub const SHORT_HASH_LEN: usize = 9;

/// A full Git commit object id (40 or 64 lowercase hex digits).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit(String);

impl Commit {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn short(&self) -> String {
        self.0[..SHORT_HASH_LEN].to_owned()
    }

    /// Accept a full object id as recorded elsewhere (an archive's build
    /// provenance), refusing anything abbreviated or non-hex.
    pub fn parse(value: &str) -> Result<Self, String> {
        if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "commit id must be 40 or 64 hexadecimal characters; got {value:?}"
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

fn rev_parse(repo: &Path, args: &[&str]) -> Result<Option<String>, DevError> {
    let output = Command::new("git")
        .arg("rev-parse")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|error| DevError::Config(format!("failed to run git: {error}")))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

/// Resolve `rev` to the commit it names, peeling tags.
pub fn resolve_commit(repo: &Path, rev: &str) -> Result<Commit, DevError> {
    let spec = format!("{rev}^{{commit}}");
    let Some(text) = rev_parse(repo, &["--verify", "--quiet", &spec])? else {
        return Err(DevError::Config(format!(
            "{rev:?} does not name a commit in {}",
            repo.display()
        )));
    };
    Commit::parse(&text).map_err(|error| {
        DevError::Config(format!(
            "git returned an invalid commit id for {rev:?}: {error}"
        ))
    })
}

/// The commit `HEAD` names. A dirty tree does not change the answer: an
/// archive built from uncommitted edits is named by its HEAD, as brokkr named
/// it.
pub fn head(repo: &Path) -> Result<Commit, DevError> {
    let Some(text) = rev_parse(repo, &["HEAD"])? else {
        return Err(DevError::Config(format!(
            "git rev-parse HEAD failed in {}",
            repo.display()
        )));
    };
    Commit::parse(&text).map_err(|error| {
        DevError::Config(format!("git returned an invalid HEAD commit id: {error}"))
    })
}
