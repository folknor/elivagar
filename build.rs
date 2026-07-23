//! Captures the build closure into env vars for archive provenance.
//!
//! Provenance that lies is worse than provenance that is absent: a stale
//! commit invites a reader to attribute a diff to code that was never linked.
//! So every value here is either established or emitted as `unknown`, and
//! freshness is handled explicitly rather than assumed.
//!
//! Freshness is the hard part. Cargo reruns a build script only when a
//! declared input changes, but it relinks the crate whenever a path
//! dependency's sources change - so without the `rerun-if-changed` lines
//! below, editing a path dependency would rebuild elivagar while this script
//! kept reporting the previous commit and dirty state. The declarations cover
//! both what git records (HEAD and the refs it points into) and what git does
//! not (uncommitted edits, via the source trees themselves).
//!
//! Registry dependencies are deliberately absent: pbfhogg and protohoggr are
//! pinned registry dependencies, so Cargo.lock identifies each by
//! content-addressed checksum and no git inspection is needed or possible.
//! Only path dependencies need this treatment, which is why pinning them
//! removed a whole class of staleness. There are currently no path
//! dependencies, so `PATH_DEPS` is empty; leaving the machinery in place keeps
//! the closure honest the day one is reintroduced.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Path dependencies whose git state has to be captured by hand.
const PATH_DEPS: [(&str, &str); 0] = [];

fn git(repo: &Path, args: &[&str]) -> Option<String> {
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
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
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
    let status = git(repo, &["status", "--porcelain"]).unwrap_or_default();
    // A clean tree yields empty output, which `git` maps to None; that is
    // indistinguishable here from a real failure, so probe separately.
    git(repo, &["rev-parse", "--is-inside-work-tree"])?;
    Some(status.lines().any(|line| {
        // Porcelain v1: two status columns, a space, then the path.
        let path = line.get(3..).unwrap_or("").trim();
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        !(path.ends_with(".md") || path == ".brokkr/results.db")
    }))
}

/// Declare the inputs that must invalidate this script.
///
/// The source tree covers uncommitted edits; HEAD covers checkouts; the ref
/// HEAD names covers commits made on the branch in place.
fn track(repo: &Path) {
    println!("cargo:rerun-if-changed={}/src", repo.display());
    println!("cargo:rerun-if-changed={}/Cargo.toml", repo.display());
    let git_dir = repo.join(".git");
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    if let Some(head) = git(repo, &["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed={}", git_dir.join(head).display());
    }
}

fn emit(name: &str, value: &str) {
    println!("cargo:rustc-env=ELIVAGAR_BUILD_{name}={value}");
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

    let repos = std::iter::once(("ELIVAGAR", manifest.clone())).chain(
        PATH_DEPS
            .iter()
            .map(|(name, rel)| (*name, manifest.join(rel))),
    );
    for (name, repo) in repos {
        track(&repo);
        emit(
            &format!("{name}_COMMIT"),
            commit(&repo).as_deref().unwrap_or("unknown"),
        );
        emit(
            &format!("{name}_DIRTY"),
            match dirty(&repo) {
                Some(true) => "true",
                Some(false) => "false",
                None => "unknown",
            },
        );
    }

    // Identifies every registry dependency at once, protohoggr included, by
    // the checksums the lockfile already pins.
    let lock = std::fs::read(manifest.join("Cargo.lock")).unwrap_or_default();
    emit(
        "CARGO_LOCK_XXH3_128",
        &format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&lock)),
    );

    // pbfhogg is our PBF reader and moved from path to registry; its semver
    // still belongs in provenance even though it no longer has a git tree.
    let lock_text = String::from_utf8_lossy(&lock);
    emit(
        "PBFHOGG_VERSION",
        locked_version(&lock_text, "pbfhogg")
            .as_deref()
            .unwrap_or("unknown"),
    );

    // Cargo exposes enabled features as CARGO_FEATURE_<NAME>. Sorted so the
    // value is stable across builds rather than dependent on env iteration
    // order, which would break same-commit byte identity.
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(str::to_string))
        .map(|f| f.to_lowercase().replace('_', "-"))
        .collect();
    features.sort();
    emit("CARGO_FEATURES", &features.join(","));
}
