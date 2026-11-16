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
//! So every value here is either established or emitted as `unknown`/null,
//! and freshness is handled explicitly rather than assumed.
//!
//! # Source identity
//!
//! Beside the commit and dirty flag, elivagar's record carries
//! `source_xxh3_128`, a content digest of the source that was compiled
//! (`build/source_identity.rs` states the recipe), so two dirty builds of one
//! commit are told apart. In a git checkout it covers the HEAD tree, the
//! tracked patch, untracked non-ignored file contents and the lockfile; a
//! packaged crate (`.cargo_vcs_info.json` present) or a checkout without git
//! metadata is hashed as a file tree. The paths named in
//! `build/source_inputs.toml` (markdown, `.brokkr/results.db`) are not build
//! inputs and enter neither the digest nor the dirty flag. A tree that cannot
//! be read, or moves between the recipe's two passes, records null.
//!
//! # Why the rerun trigger is content
//!
//! Cargo cannot express "this output depends on the whole working tree".
//! Rerunning unconditionally is exact but recompiles the crate on every cargo
//! call, because a rerun build script recompiles its package. The previous
//! trigger, HEAD plus `git status --porcelain`, was cheap but wrong: status
//! names paths, not contents, so a second edit to an already-modified file, or
//! a rewrite of an untracked one, left the token unchanged and the stamp
//! stale. The trigger is therefore content computed outside cargo: brokkr runs
//! `scripts/source_identity.py` before every cargo call (`[cargo_env]` in
//! brokkr.toml) and exports a hash of the same inputs as
//! `ELV_SOURCE_IDENTITY`. This script watches only that variable and reruns,
//! still computing the stamp from content, exactly when it changes. The two
//! hashes are never compared - the trigger only has to change whenever the
//! recipe's inputs do - so the trigger uses Python's stdlib SHA-256 and this
//! script xxh3-128. Both read `build/source_inputs.toml`, and
//! `tests/source_identity.rs` pins that they read it identically and that they
//! change on the same edits.
//!
//! Without the variable (plain cargo, rust-analyzer, a `brokkr --commit`
//! worktree older than the script) or when the recipe could not establish an
//! identity, the script also watches an intentionally absent file, so cargo
//! reruns it on every build: exact, just slower. The trigger's own fallback
//! for a tree it cannot describe is a fresh `unidentifiable-<time_ns>` per
//! call, with the same effect; the `source-identity-stable` script check
//! turns that red on a normal tree, where it would silently make every cargo
//! call recompile the crate.
//!
//! The trigger is evaluated before cargo starts and the stamp when this
//! script runs, so an edit landing between the stamp and rustc reading the
//! file ships stale for that one build; the next trigger evaluation sees the
//! edit and reruns.
//!
//! # Packaged builds
//!
//! A crate unpacked from a registry has no `.git` of its own, so a git query
//! there walks up into whatever repository encloses the registry cache. The
//! commit and dirty flag come from `.cargo_vcs_info.json` instead, and the
//! digest hashes the package directory, which cannot change under us, so no
//! trigger is needed.
//!
//! Registry dependencies are deliberately absent: pbfhogg and protohoggr are
//! pinned registry dependencies, so Cargo.lock identifies each by
//! content-addressed checksum and no git inspection is needed or possible.
//! There are no path dependencies outside this workspace. Reintroducing one
//! means covering its tree here and giving it a field of its own in the
//! provenance `Build` struct.

use std::path::{Path, PathBuf};

#[path = "build/source_inputs.rs"]
mod source_inputs;

#[path = "build/source_identity.rs"]
mod source_identity;

use source_identity::Layout;
use source_inputs::SourceInputs;

/// The `[cargo_env]` variable brokkr computes before every cargo call by
/// running `scripts/source_identity.py`. Not `ELIVAGAR_*`: brokkr refuses a
/// cargo_env name its `capture_env` patterns match.
const TRIGGER_VAR: &str = "ELV_SOURCE_IDENTITY";

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

/// The commit a packaged crate was published from, read from the
/// `.cargo_vcs_info.json` that `cargo package` writes at the crate root.
///
/// `Some((commit, dirty))` when the file exists; the commit is None when it
/// names no sha1 (a package made outside git), and `dirty` mirrors cargo's
/// `"dirty": true`, which it writes only for `--allow-dirty` packages.
fn packaged_vcs(manifest: &Path) -> Option<(Option<String>, bool)> {
    let text = std::fs::read_to_string(manifest.join(".cargo_vcs_info.json")).ok()?;
    let sha = text.split("\"sha1\"").nth(1).and_then(|rest| {
        let rest = rest.trim_start().strip_prefix(':')?.trim_start();
        let rest = rest.strip_prefix('"')?;
        let sha = &rest[..rest.find('"')?];
        (sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())).then(|| sha.to_string())
    });
    let dirty = text.split("\"dirty\"").nth(1).is_some_and(|rest| {
        rest.trim_start()
            .strip_prefix(':')
            .is_some_and(|v| v.trim_start().starts_with("true"))
    });
    Some((sha, dirty))
}

/// The lockfile the build resolved against. A packaged crate carries its own
/// copy at the package root; in the checkout it is the workspace's.
fn lockfile(manifest: &Path, root: &Path) -> PathBuf {
    let packaged = manifest.join("Cargo.lock");
    if packaged.is_file() {
        packaged
    } else {
        root.join("Cargo.lock")
    }
}

/// What the stamp says about elivagar itself.
struct Stamp {
    commit: Option<String>,
    dirty: Option<bool>,
    source: Option<String>,
}

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // The git root: this package is crates/lib in the workspace.
    let root = manifest.join("../..");
    let inputs = SourceInputs::embedded();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/source_identity.rs");
    println!("cargo:rerun-if-changed=build/source_inputs.rs");
    println!("cargo:rerun-if-changed=build/source_inputs.toml");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));

    if let Some((commit, dirty)) = packaged_vcs(&manifest) {
        // A packaged source cannot change under us, so no freshness trigger.
        let lock = lockfile(&manifest, &manifest);
        let source = source_identity::read(&manifest, &lock, Layout::Tree, &inputs);
        if let Err(cause) = &source {
            println!("cargo:warning=build provenance: source identity unavailable: {cause}");
        }
        let stamp = Stamp {
            commit,
            dirty: Some(dirty),
            source: source.ok().map(|reading| reading.digest),
        };
        write_closure(&lock, &out_dir, &stamp);
        return;
    }

    let lock = lockfile(&manifest, &root);
    println!("cargo:rerun-if-changed={}", lock.display());
    let layout = source_identity::layout(&root, &inputs);
    let reading = layout
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|layout| source_identity::read(&root, &lock, *layout, &inputs));
    // Asked only of a git checkout: without its own metadata, git would walk
    // up into an enclosing repository and stamp that one's commit.
    let commit = match layout {
        Ok(Layout::Git) => source_identity::commit(&root, &inputs),
        _ => None,
    };

    println!("cargo:rerun-if-env-changed={TRIGGER_VAR}");
    if std::env::var_os(TRIGGER_VAR).is_none() || reading.is_err() {
        // Cargo treats a missing input as changed on every build. Never create
        // this sentinel. Without the trigger nothing else tells cargo the
        // tree moved; with it, a failed reading must not be kept by a skipped
        // rerun.
        println!(
            "cargo:rerun-if-changed={}",
            out_dir.join("always-rerun-provenance").display()
        );
    }
    if let Err(cause) = &reading {
        println!(
            "cargo:warning=build provenance: source identity unavailable ({cause}); \
             this crate rebuilds on every cargo call until it is readable"
        );
    }

    let (dirty, source) = match reading {
        Ok(reading) => (reading.dirty, Some(reading.digest)),
        Err(_) => (None, None),
    };
    write_closure(
        &lock,
        &out_dir,
        &Stamp {
            commit,
            dirty,
            source,
        },
    );
}

/// Writes `$OUT_DIR/build_closure.rs` from the stamp plus the lockfile and
/// feature observations.
fn write_closure(lockfile: &Path, out_dir: &Path, stamp: &Stamp) {
    // Identifies every registry dependency at once, protohoggr included, by
    // the checksums the lockfile already pins.
    let lock = std::fs::read(lockfile).unwrap_or_default();
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

    let dirty = match stamp.dirty {
        Some(true) => "Some(true)",
        Some(false) => "Some(false)",
        None => "None",
    };
    let source = stamp.source.as_deref().map_or_else(
        || "None".to_string(),
        |digest| format!("Some({})", lit(digest)),
    );
    let text = format!(
        "Build {{\n    elivagar: Repo {{ commit: {commit}, dirty: {dirty}, \
         source_xxh3_128: {source} }},\n    \
         pbfhogg_reader: Reader {{ version: {pbfhogg} }},\n    \
         cargo_lock_xxh3_128: {lock_hash},\n    cargo_features: &[{features}],\n}}\n",
        commit = lit(stamp.commit.as_deref().unwrap_or("unknown")),
        pbfhogg = lit(&pbfhogg),
        lock_hash = lit(&lock_hash),
        features = features.join(", "),
    );
    std::fs::write(out_dir.join("build_closure.rs"), text)
        .expect("cannot write the generated build closure");
}
