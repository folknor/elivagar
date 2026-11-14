//! The provenance dirty rule `build.rs` applies to raw `git status` output.

// STATUS_ARGS is the build script's, unused here.
#[allow(dead_code)]
#[path = "../build/git_status.rs"]
mod git_status;

use git_status::dirty;

#[test]
fn empty_status_is_clean() {
    assert_eq!(dirty(b""), Some(false));
}

#[test]
fn results_db_alone_is_clean_even_as_first_entry() {
    // The old text parser trimmed the whole output, which ate the leading
    // status space of the first line and misread this as dirty.
    assert_eq!(dirty(b" M .brokkr/results.db\0"), Some(false));
}

#[test]
fn markdown_is_clean_including_names_git_would_quote() {
    assert_eq!(dirty(b" M README.md\0?? notes/a b \"c\".md\0"), Some(false));
}

#[test]
fn source_change_is_dirty() {
    assert_eq!(dirty(b" M .brokkr/results.db\0 M src/lib.rs\0"), Some(true));
    assert_eq!(dirty(b"?? scripts/new.py\0"), Some(true));
    // A rename to markdown arrives as a deletion plus an addition under
    // --no-renames, so the removed source still counts.
    assert_eq!(dirty(b" D src/x.rs\0?? src/x.md\0"), Some(true));
}

#[test]
fn non_utf8_path_is_judged_by_bytes() {
    assert_eq!(dirty(b"?? notes/\xff.md\0"), Some(false));
    assert_eq!(dirty(b"?? src/\xff.rs\0"), Some(true));
}

#[test]
fn malformed_status_is_unknown_not_clean() {
    assert_eq!(dirty(b" M README.md"), None);
    assert_eq!(dirty(b"M\0"), None);
    assert_eq!(dirty(b"XYZsrc\0"), None);
}
