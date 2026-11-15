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
//! ignored datasets and build outputs. Git resolves its own metadata paths,
//! including worktrees and submodules with a .git file.
//!
//! Rerunning this script rebuilds the whole crate, so it reruns only when its
//! git inputs change. brokkr computes `ELV_BUILD_OBSERVATION` before every
//! cargo call (`[cargo_env]` in brokkr.toml, `scripts/build_observation.py`):
//! HEAD plus git's object id of the raw status bytes. This script declares
//! `rerun-if-env-changed` on it, computes the same token from the very bytes
//! it stamps from, and where the variable is absent (plain cargo,
//! rust-analyzer, an old `brokkr --commit` worktree) or disagrees (the tree
//! moved mid-build, a failed read) it also watches an intentionally absent
//! input, so Cargo reruns it on every build until the two agree. The token
//! carries no interpretation: the dirty rule lives only in
//! `build/git_status.rs`. Both observers strip `GIT_*` from git's environment,
//! so they read this checkout's repository and index. The guarantee is
//! freshness equal to rerunning every build for any tree state that holds
//! from brokkr's trigger evaluation through the build; an edit landing inside
//! that window, or a git configuration visible to only one observer, is
//! outside it.
//!
//! Registry dependencies are deliberately absent: pbfhogg and protohoggr are
//! pinned registry dependencies, so Cargo.lock identifies each by
//! content-addressed checksum and no git inspection is needed or possible.
//! Only path dependencies need this treatment, which is why pinning them
//! removed a whole class of staleness. There are currently no path
//! dependencies. Reintroducing one means tracking its tree here and giving it
//! a commit/dirty field of its own in the provenance `Build` struct.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[path = "build/git_status.rs"]
mod git_status;

/// The `[cargo_env]` variable brokkr computes before every cargo call by
/// running `scripts/build_observation.py`. Not `ELIVAGAR_*`: brokkr refuses a
/// cargo_env name its `capture_env` patterns match.
const OBSERVATION_VAR: &str = "ELV_BUILD_OBSERVATION";

/// A git command with every `GIT_*` variable removed, run in `repo`.
///
/// Provenance describes this checkout, not wherever a hook's `GIT_DIR` or
/// `GIT_INDEX_FILE` routes git. The trigger strips the same variables, so the
/// two observers read the same repository and index.
fn git_command(repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    // current_dir rather than `-C`: same effect, and it fails cleanly when the
    // directory does not exist instead of asking git to interpret it.
    cmd.current_dir(repo).args(args);
    let git_vars: Vec<OsString> = std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| k.to_string_lossy().starts_with("GIT_"))
        .collect();
    for var in git_vars {
        cmd.env_remove(var);
    }
    cmd
}

/// Raw stdout of a successful git command; None when git could not run or
/// failed. Empty output is a success and stays `Some(vec![])`, so a caller can
/// tell a clean `status` apart from a failed one.
fn git_raw(repo: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = git_command(repo, args).output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// Full 40-character commit. Abbreviations collide, and a provenance record
/// that cannot be resolved to exactly one commit is not provenance.
fn commit(repo: &Path) -> Option<String> {
    let text = String::from_utf8(git_raw(repo, &["rev-parse", "HEAD"])?).ok()?;
    Some(text.trim().to_string()).filter(|text| !text.is_empty())
}

/// Git's object id for `bytes`, the digest both observers put in the trigger
/// token. Using git for it means the trigger and this script cannot disagree
/// on the algorithm, and a collision-resistant one keeps two different
/// statuses from sharing a token.
fn hash_object(repo: &Path, bytes: &[u8]) -> Option<String> {
    let mut child = git_command(repo, &["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(bytes).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.trim().to_string()).filter(|text| !text.is_empty())
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

/// The commit a packaged crate was published from, read from the
/// `.cargo_vcs_info.json` that `cargo package` writes at the crate root.
///
/// A crate unpacked from a registry has no `.git` of its own, so a git query
/// there walks up into whatever repository encloses the registry cache (a
/// dotfiles repo in `$HOME`, say) and would stamp that repository's commit.
/// The packaged file is the only honest source. `Some((commit, dirty))` when
/// the file exists; the commit is None when it names no sha1 (a package made
/// outside git), and `dirty` mirrors cargo's `"dirty": true`, which it writes
/// only for `--allow-dirty` packages.
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

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/git_status.rs");
    println!("cargo:rerun-if-changed=Cargo.lock");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));

    if let Some((commit, dirty)) = packaged_vcs(&manifest) {
        // A packaged source cannot change under us, so no freshness trigger.
        let commit = commit.unwrap_or_else(|| "unknown".to_string());
        let dirty = if dirty { "Some(true)" } else { "Some(false)" };
        write_closure(&manifest, &out_dir, &commit, dirty);
        return;
    }

    // One read of each git input; the stamp and the token below are both
    // derived from these same bytes, never from a second read.
    let commit = commit(&manifest);
    let status = git_raw(&manifest, git_status::STATUS_ARGS);

    // The token the trigger computes from the same two observations. A
    // failed read makes it unknown, which no trigger value equals.
    let token = match (&commit, &status) {
        (Some(commit), Some(status)) => {
            hash_object(&manifest, status).map(|oid| format!("{commit}:{oid}"))
        }
        _ => None,
    };
    println!("cargo:rerun-if-env-changed={OBSERVATION_VAR}");
    let observed = std::env::var(OBSERVATION_VAR).ok();
    if observed.is_none() || observed != token {
        // Cargo treats a missing input as changed on every build. Never create
        // this sentinel. Without the trigger (plain cargo, rust-analyzer, a
        // `brokkr --commit` worktree older than the script) this reruns the
        // script on every build, because status can change anywhere in the
        // repository and HEAD can move through refs in a separate git
        // directory. With it, a disagreement means the tree moved between the
        // trigger and this read, or the trigger failed: rerun next build too,
        // rather than let a skipped rerun keep what was read here.
        println!(
            "cargo:rerun-if-changed={}",
            out_dir.join("always-rerun-provenance").display()
        );
        if let Some(observed) = &observed {
            println!(
                "cargo:warning=build provenance observation changed during the build \
                 or could not be read ({OBSERVATION_VAR}={observed}, here {}); \
                 this crate rebuilds on every cargo call until they agree",
                token.as_deref().unwrap_or("unknown")
            );
        }
    }

    let commit = commit.unwrap_or_else(|| "unknown".to_string());
    let dirty = match status.as_deref().and_then(git_status::dirty) {
        Some(true) => "Some(true)",
        Some(false) => "Some(false)",
        None => "None",
    };
    write_closure(&manifest, &out_dir, &commit, dirty);
}

/// Writes `$OUT_DIR/build_closure.rs` from the established commit and dirty
/// literal plus the lockfile and feature observations.
fn write_closure(manifest: &Path, out_dir: &Path, commit: &str, dirty: &str) {
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
        commit = lit(commit),
        pbfhogg = lit(&pbfhogg),
        lock_hash = lit(&lock_hash),
        features = features.join(", "),
    );
    std::fs::write(out_dir.join("build_closure.rs"), source)
        .expect("cannot write the generated build closure");
}
