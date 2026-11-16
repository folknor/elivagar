//! The source identity recipe: what the `source_xxh3_128` provenance digest
//! and the dirty flag are computed from. Shared by `build.rs` and
//! `tests/source_identity.rs`, which both include this file by path so the
//! recipe is tested exactly as the build script runs it.
//!
//! In a git checkout the digest covers the HEAD tree, one `git diff HEAD`
//! patch of tracked content (staged and unstaged together: the index boundary
//! is not a compiler input), the contents of every untracked non-ignored file,
//! and `Cargo.lock`. Content is hashed, never merely named: `git status
//! --porcelain` names an untracked path without its bytes and cannot tell two
//! edits of one tracked file apart. A checkout without git metadata (a
//! packaged crate, a source tarball) is hashed as a plain file tree.
//!
//! Paths the shared inputs exclude (markdown, `.brokkr/results.db`) are left
//! out of every read, as git pathspecs and in tree walks alike. With them out,
//! the dirty rule is simply "the tracked patch or the untracked listing is
//! non-empty", so the rule and the digest cannot disagree about what counts.
//!
//! Untracked files above the shared `untracked_content_cap` are fed as length
//! plus modification time instead of content: capped, not skipped. A rewrite
//! of such a file that preserves both is the named blind spot; nothing the
//! compiler reads is anywhere near the cap.
//!
//! The recipe runs twice and the passes must agree; a tree edited while it is
//! being hashed has no single state to name, so it is unidentifiable rather
//! than a digest of a mixture. Every value is labelled and length-prefixed,
//! so no two input sets serialize to the same byte stream.
//!
//! The digest is xxh3-128, not a cryptographic hash. It names a developer's
//! own edit state for diagnostics; nothing compares it to decide anything
//! (provenance `build` is never equality-gated) and nobody crafts collisions
//! against it, so a 128-bit non-cryptographic hash makes an accidental
//! collision negligible, and xxhash-rust is already the one build dependency.
//!
//! Compiler-facing environment (rustc version, RUSTFLAGS, CC, profile) is
//! deliberately NOT an input: the provenance block must be identical for two
//! builds of the same commit (src/provenance.rs), and toolchain and profile
//! vary per host and per brokkr mode without changing the source.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::source_inputs::SourceInputs;

/// Schema label fed first, so a change to what the recipe covers can be named.
const RECIPE_SCHEMA: &str = "elivagar-source-1";

/// A git command run in `dir`, with every `GIT_*` variable removed and the
/// developer's global ignore file neutralised.
///
/// Every read describes the checkout at `dir`: a cargo build run from a git
/// hook inherits `GIT_DIR` and friends, which would otherwise route the reads
/// to another repository or index. The whole prefix is removed rather than a
/// named list, which is the shape that misses the next variable git grows.
/// `core.excludesFile=` keeps an authored untracked file from vanishing
/// behind a personal global ignore.
pub fn git_command(dir: &Path, inputs: &SourceInputs) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.excludesFile="]).current_dir(dir);
    for (key, _) in std::env::vars_os() {
        if key
            .as_encoded_bytes()
            .starts_with(inputs.git_environment_prefix.as_bytes())
        {
            cmd.env_remove(&key);
        }
    }
    cmd
}

/// Raw stdout of a successful git command; an empty answer stays `Some`.
fn git_bytes(dir: &Path, inputs: &SourceInputs, args: &[&str]) -> Option<Vec<u8>> {
    let out = git_command(dir, inputs).args(args).output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// The full HEAD commit of the checkout at `dir`.
pub fn commit(dir: &Path, inputs: &SourceInputs) -> Option<String> {
    let text = String::from_utf8(git_bytes(dir, inputs, &["rev-parse", "HEAD"])?).ok()?;
    Some(text.trim().to_string()).filter(|text| !text.is_empty())
}

/// How a checkout is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// A git checkout: tree, patch and untracked listing.
    Git,
    /// No git metadata: every file below the root, minus root exclusions.
    Tree,
}

/// Which layout `dir` has. A stat error is not absence.
pub fn layout(dir: &Path, inputs: &SourceInputs) -> Result<Layout, String> {
    match dir.join(&inputs.git_metadata_directory).try_exists() {
        Ok(true) => Ok(Layout::Git),
        Ok(false) => Ok(Layout::Tree),
        Err(error) => Err(format!(
            "cannot stat git metadata in {}: {error}",
            dir.display()
        )),
    }
}

/// One established reading of a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// 32 lowercase hex digits.
    pub digest: String,
    /// Known for a git checkout only; a plain tree has no baseline to differ
    /// from.
    pub dirty: Option<bool>,
}

/// The source identity of the checkout at `root`, run twice. `lockfile` is the
/// lockfile the build resolved against.
pub fn read(
    root: &Path,
    lockfile: &Path,
    layout: Layout,
    inputs: &SourceInputs,
) -> Result<Reading, String> {
    let first = pass(root, lockfile, layout, inputs)?;
    let second = pass(root, lockfile, layout, inputs)?;
    if first != second {
        return Err(format!(
            "source inputs changed between recipe passes at {}",
            root.display()
        ));
    }
    Ok(first)
}

struct Hasher(xxhash_rust::xxh3::Xxh3);

impl Hasher {
    fn feed(&mut self, label: &str, bytes: &[u8]) {
        self.0.update(label.as_bytes());
        self.0.update(b"\0");
        self.0
            .update(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        self.0.update(bytes);
    }
}

fn pass(
    root: &Path,
    lockfile: &Path,
    layout: Layout,
    inputs: &SourceInputs,
) -> Result<Reading, String> {
    let mut hasher = Hasher(xxhash_rust::xxh3::Xxh3::new());
    hasher.feed("schema", RECIPE_SCHEMA.as_bytes());
    // Absent and unreadable are different facts: absent is an identifiable
    // state, unreadable is not.
    let lock = match std::fs::read(lockfile) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => b"absent".to_vec(),
        Err(error) => return Err(format!("reading {}: {error}", lockfile.display())),
    };
    hasher.feed("lockfile", &lock);
    let dirty = match layout {
        Layout::Git => {
            hasher.feed("layout", b"git");
            Some(feed_git(&mut hasher, root, inputs)?)
        }
        Layout::Tree => {
            hasher.feed("layout", b"tree");
            feed_tree(&mut hasher, root, root, true, inputs)?;
            None
        }
    };
    Ok(Reading {
        digest: format!("{:032x}", hasher.0.digest128()),
        dirty,
    })
}

/// Returns whether the checkout differs from HEAD in any build input.
fn feed_git(hasher: &mut Hasher, root: &Path, inputs: &SourceInputs) -> Result<bool, String> {
    let failed = |what: &str| format!("git {what} failed in {}", root.display());
    let tree = git_bytes(root, inputs, &["rev-parse", "HEAD^{tree}"])
        .filter(|tree| !tree.trim_ascii().is_empty())
        .ok_or_else(|| failed("rev-parse HEAD^{tree}"))?;
    hasher.feed("tree", tree.trim_ascii());

    let pathspecs = inputs.git_pathspecs();
    let mut diff: Vec<&str> = DIFF_ARGS.to_vec();
    diff.push("--");
    diff.extend(pathspecs.iter().map(String::as_str));
    let patch = git_bytes(root, inputs, &diff).ok_or_else(|| failed("diff HEAD"))?;
    hasher.feed("worktree", &patch);

    let mut listing: Vec<&str> = UNTRACKED_ARGS.to_vec();
    listing.push("--");
    listing.extend(pathspecs.iter().map(String::as_str));
    let untracked = git_bytes(root, inputs, &listing).ok_or_else(|| failed("ls-files"))?;
    let mut names: Vec<&[u8]> = untracked
        .split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .collect();
    names.sort_unstable();
    for name in &names {
        let relative = String::from_utf8_lossy(name).into_owned();
        let path = root.join(relative.trim_end_matches('/'));
        // git lists an untracked nested checkout as the directory itself, with
        // a trailing slash. Its files are still inputs, so walk them.
        if relative.ends_with('/') {
            hasher.feed(&format!("untracked/{relative}"), b"tree");
            feed_tree(hasher, root, &path, false, inputs)?;
        } else {
            feed_file(hasher, &format!("untracked/{relative}"), &path, inputs)?;
        }
    }
    Ok(!patch.is_empty() || !names.is_empty())
}

/// The tracked-content read. `--no-renames` keeps a rename a deletion plus an
/// addition, so moving a source file to an excluded name still counts.
pub const DIFF_ARGS: &[&str] = &[
    "diff",
    "HEAD",
    "--binary",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--no-renames",
    "-U0",
];

/// The untracked non-ignored listing.
pub const UNTRACKED_ARGS: &[&str] = &["ls-files", "--others", "--exclude-standard", "-z"];

/// Feed one file: content up to the cap, a length and mtime witness above it,
/// a symlink's destination text without following it.
fn feed_file(
    hasher: &mut Hasher,
    label: &str,
    path: &Path,
    inputs: &SourceInputs,
) -> Result<(), String> {
    let unreadable = |error: std::io::Error| format!("reading {}: {error}", path.display());
    let meta = std::fs::symlink_metadata(path).map_err(unreadable)?;
    if meta.file_type().is_symlink() {
        let link = std::fs::read_link(path).map_err(unreadable)?;
        hasher.feed(label, link.as_os_str().as_encoded_bytes());
        return Ok(());
    }
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() <= inputs.untracked_content_cap {
        let bytes = std::fs::read(path).map_err(unreadable)?;
        hasher.feed(label, &bytes);
        return Ok(());
    }
    let since_epoch = meta
        .modified()
        .map_err(unreadable)?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| format!("{} has a pre-epoch mtime", path.display()))?;
    let witness = format!(
        "oversize:{}:{}.{:09}",
        meta.len(),
        since_epoch.as_secs(),
        since_epoch.subsec_nanos()
    );
    hasher.feed(label, witness.as_bytes());
    Ok(())
}

/// Hash every file below `dir`, labelled relative to `label_root`, in sorted
/// order. At `dir`'s own top level the git metadata is skipped, and when
/// `root_exclusions` the shared root-only names are skipped too. Excluded
/// paths and suffixes apply at every depth, as the pathspecs do for git.
fn feed_tree(
    hasher: &mut Hasher,
    label_root: &Path,
    dir: &Path,
    root_exclusions: bool,
    inputs: &SourceInputs,
) -> Result<(), String> {
    let mut pending: Vec<PathBuf> = vec![dir.to_path_buf()];
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    while let Some(current) = pending.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|error| format!("reading {}: {error}", current.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("reading {}: {error}", current.display()))?;
            let path = entry.path();
            let name = entry.file_name();
            if current == dir {
                if name == inputs.git_metadata_directory.as_str() {
                    continue;
                }
                if root_exclusions
                    && inputs
                        .root_tree_exclusions
                        .iter()
                        .any(|n| name == n.as_str())
                {
                    continue;
                }
            }
            let relative = path
                .strip_prefix(label_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if inputs.excludes_path(relative.as_bytes()) {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("reading {}: {error}", path.display()))?;
            if meta.is_dir() {
                pending.push(path);
            } else {
                files.push((relative, path));
            }
        }
    }
    files.sort();
    for (relative, path) in files {
        feed_file(hasher, &format!("file/{relative}"), &path, inputs)?;
    }
    Ok(())
}
