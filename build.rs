//! Captures the build closure for archive provenance.
//!
//! The result is one generated Rust file, `$OUT_DIR/build_closure.rs`, holding
//! the initializer of a typed constant whose struct `src/provenance.rs`
//! declares. The types live with the serializer and the values are written
//! here once, already typed - `dirty` as `Option<bool>`, features as a slice -
//! so nothing on the consumer side reparses a string protocol. The file is
//! rewritten in full on every run of this script; a stale copy left in
//! `OUT_DIR` is never reused.
//!
//! Provenance that lies is worse than provenance that is absent: a stale
//! commit invites a reader to attribute a diff to code that was never linked.
//! So every value here is either established or emitted as `unknown`, and
//! freshness is handled explicitly rather than assumed.
//!
//! Freshness is the hard part. The dirty flag observes the whole git status,
//! including untracked files outside src/. No fixed list of source inputs
//! covers that state, and watching the repository recursively also walks
//! ignored datasets and build outputs. An intentionally absent input makes
//! Cargo rerun this small script on every build instead. Git resolves its own
//! metadata paths, including worktrees and submodules with a .git file.
//!
//! Registry dependencies are deliberately absent: pbfhogg and protohoggr are
//! pinned registry dependencies, so Cargo.lock identifies each by
//! content-addressed checksum and no git inspection is needed or possible.
//! Only path dependencies need this treatment, which is why pinning them
//! removed a whole class of staleness. There are currently no path
//! dependencies. Reintroducing one means tracking its tree here and giving it
//! a commit/dirty field of its own in the provenance `Build` struct.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Trimmed stdout of a successful git command; None when git could not run
/// or failed. Empty output is a success and stays `Some("")`, so a caller can
/// tell a clean `status` apart from a failed one.
fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    // current_dir rather than `-C`: same effect, and it fails cleanly when the
    // directory does not exist instead of asking git to interpret it.
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.trim().to_string())
}

/// Like [`git_output`], but empty output also reads as absent.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    git_output(repo, args).filter(|text| !text.is_empty())
}

/// Full 40-character commit. Abbreviations collide, and a provenance record
/// that cannot be resolved to exactly one commit is not provenance.
fn commit(repo: &Path) -> Option<String> {
    git(repo, &["rev-parse", "HEAD"])
}

/// Dirty by brokkr's definition, not git's.
///
/// A dirty `.brokkr/results.db` is the expected state of this repo and
/// markdown never affects the binary, so counting either would mark almost
/// every build dirty and make the flag meaningless. This mirrors the rule
/// brokkr applies when deciding whether a run's results may be stored.
fn dirty(repo: &Path) -> Option<bool> {
    // A failed status is unknown, not clean: an empty string from a failed
    // command would otherwise record `dirty: false` for a tree nobody read.
    let status = git_output(repo, &["status", "--porcelain"])?;
    Some(status.lines().any(|line| {
        // Porcelain v1: two status columns, a space, then the path.
        let path = line.get(3..).unwrap_or("").trim();
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        !(path.ends_with(".md") || path == ".brokkr/results.db")
    }))
}

/// A Rust string literal for `value`. Debug formatting escapes quotes,
/// backslashes and control characters; how it escapes them never reaches the
/// archive, which sees only the decoded string.
fn lit(value: &str) -> String {
    format!("{value:?}")
}

/// The locked semver of a registry dependency, read straight from Cargo.lock.
///
/// A registry dependency has no git tree to inspect, so it carries no
/// commit/dirty pair - but its version is still worth recording in provenance,
/// and `cargo_lock_xxh3_128` pins only an opaque checksum a human cannot read.
/// The lockfile lists `name` immediately above `version` inside each
/// `[[package]]` block, so a line scan resolves it without taking a build
/// dependency on a TOML parser.
fn locked_version(lock: &str, package: &str) -> Option<String> {
    let name_line = format!("name = \"{package}\"");
    let mut in_target = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_target = false;
        } else if line == name_line {
            in_target = true;
        } else if in_target && let Some(rest) = line.strip_prefix("version = \"") {
            return rest.strip_suffix('"').map(str::to_string);
        }
    }
    None
}

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.lock");

    // Cargo treats a missing input as changed on every build. Never create
    // this sentinel: status can change anywhere in the repository, and HEAD
    // can move through loose or packed refs in a separate git directory.
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    println!(
        "cargo:rerun-if-changed={}",
        out_dir.join("always-rerun-provenance").display()
    );
    let commit = commit(&manifest).unwrap_or_else(|| "unknown".to_string());
    let dirty = match dirty(&manifest) {
        Some(true) => "Some(true)",
        Some(false) => "Some(false)",
        None => "None",
    };

    // Identifies every registry dependency at once, protohoggr included, by
    // the checksums the lockfile already pins.
    let lock = std::fs::read(manifest.join("Cargo.lock")).unwrap_or_default();
    let lock_hash = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&lock));

    // pbfhogg is our PBF reader and moved from path to registry; its semver
    // still belongs in provenance even though it no longer has a git tree.
    let lock_text = String::from_utf8_lossy(&lock);
    let pbfhogg = locked_version(&lock_text, "pbfhogg").unwrap_or_else(|| "unknown".to_string());

    // Cargo exposes enabled features as CARGO_FEATURE_<NAME>. Sorted so the
    // value is stable across builds rather than dependent on env iteration
    // order, which would break same-commit byte identity.
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(str::to_string))
        .map(|f| f.to_lowercase().replace('_', "-"))
        .collect();
    features.sort();
    let features: Vec<String> = features.iter().map(|f| lit(f)).collect();

    let source = format!(
        "Build {{\n    elivagar: Repo {{ commit: {commit}, dirty: {dirty} }},\n    \
         pbfhogg_reader: Reader {{ version: {pbfhogg} }},\n    \
         cargo_lock_xxh3_128: {lock_hash},\n    cargo_features: &[{features}],\n}}\n",
        commit = lit(&commit),
        pbfhogg = lit(&pbfhogg),
        lock_hash = lit(&lock_hash),
        features = features.join(", "),
    );
    std::fs::write(out_dir.join("build_closure.rs"), source)
        .expect("cannot write the generated build closure");
}
