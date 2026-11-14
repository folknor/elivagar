//! The dirty rule over raw `git status` output, shared by `build.rs` and its
//! unit test (`tests/build_git_status.rs`), which both include this file by
//! path so the rule is tested exactly as the build script runs it.

/// The status invocation both provenance observers make: `build.rs`, and the
/// brokkr `[cargo_env]` trigger `scripts/build_observation.py`, which hashes
/// the same bytes. Untracked directories are expanded to files, so a new
/// directory holding only markdown is not mistaken for a source change.
/// Rename detection is off, so a rename is a deletion plus an addition and the
/// removed path is judged on its own, while a copy is only the addition.
/// `--no-optional-locks` keeps status from rewriting the index it reads.
pub const STATUS_ARGS: &[&str] = &[
    "--no-optional-locks",
    "status",
    "--porcelain=v1",
    "-z",
    "--untracked-files=all",
    "--no-renames",
];

/// Dirty by brokkr's definition, not git's: a path that is markdown or
/// `.brokkr/results.db` does not count. A dirty `results.db` is this repo's
/// expected state and markdown never affects the binary, so counting either
/// would mark almost every build dirty and make the flag meaningless.
///
/// `status` is the raw stdout of [`STATUS_ARGS`]: NUL-terminated entries of
/// two status columns, a space, and the unquoted path. Output that does not
/// have that shape is unknown (`None`), never clean.
pub fn dirty(status: &[u8]) -> Option<bool> {
    if status.is_empty() {
        return Some(false);
    }
    let body = status.strip_suffix(b"\0")?;
    let mut dirty = false;
    for entry in body.split(|&b| b == 0) {
        if entry.len() < 4 || entry[2] != b' ' {
            return None;
        }
        let path = &entry[3..];
        if !(path.ends_with(b".md") || path == b".brokkr/results.db") {
            dirty = true;
        }
    }
    Some(dirty)
}
