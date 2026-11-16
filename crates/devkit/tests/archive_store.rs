//! The durable archive store: naming, resolution, admission and retention.

mod common;

use std::path::Path;
use std::time::{Duration, SystemTime};

use common::{repo_with_commits, scratch, write};
use elivagar_devkit::DevError;
use elivagar_devkit::archive_store::{ArchiveKey, ArchiveStore, TILEGEN_RETENTION};
use elivagar_devkit::git;

fn message<T: std::fmt::Debug>(result: Result<T, DevError>) -> String {
    match result {
        Err(DevError::Config(message)) => message,
        Err(other) => panic!("expected a config error, got {other}"),
        Ok(value) => panic!("expected a config error, got {value:?}"),
    }
}

fn key(dataset: &str, variant: &str, commit: &str) -> ArchiveKey {
    ArchiveKey::new(dataset, variant, commit).expect("key")
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read store")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn set_mtime(path: &Path, secs: u64) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open");
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
        .expect("set mtime");
}

#[test]
fn key_refuses_unsafe_components_and_hyphenated_commits() {
    for (d, v, c) in [
        ("", "raw", "abc"),
        ("dk", "", "abc"),
        ("dk", "raw", ""),
        ("..", "raw", "abc"),
        (".", "raw", "abc"),
        ("a/b", "raw", "abc"),
        ("dk", "r\\w", "abc"),
        ("dk", "raw", "a\0b"),
    ] {
        let m = message(ArchiveKey::new(d, v, c));
        assert!(
            m.contains("non-empty filename components"),
            "{d:?} {v:?} {c:?}: {m}"
        );
    }
    let m = message(ArchiveKey::new("dk", "raw", "abc-def"));
    assert!(m.contains("cannot contain a hyphen"), "{m}");
    // Hyphens in dataset and variant are fine.
    assert_eq!(key("north-america", "raw-fast", "abc").commit(), "abc");
}

#[test]
fn path_for_is_dataset_variant_commit() {
    let store = ArchiveStore::new("/out");
    assert_eq!(
        store.path_for(&key("denmark", "locations", "0123456789")),
        Path::new("/out/denmark-locations-0123456789.pmtiles")
    );
}

#[test]
fn commit_short_is_nine_hex_digits() {
    let root = scratch("store-git-short");
    let ids = repo_with_commits(&root, 1);
    let head = git::head(&root).expect("head");
    assert_eq!(head.as_str(), ids[0]);
    assert_eq!(head.short(), ids[0][..git::SHORT_HASH_LEN]);
    assert_eq!(head.short().len(), 9);
    assert_eq!(git::resolve_commit(&root, "HEAD").expect("rev"), head);
    let m = message(git::resolve_commit(&root, "nope"));
    assert!(m.contains("does not name a commit"), "{m}");
}

#[test]
fn admit_moves_and_reports_replacement() {
    let root = scratch("store-admit");
    let scratch_file = root.join("scratch/out.pmtiles");
    let store = ArchiveStore::new(root.join("store"));
    let k = key("dk", "raw", "abc123def");

    write(&scratch_file, "one");
    let first = store.admit(&scratch_file, &k, &[]).expect("admit");
    assert_eq!(first.path, root.join("store/dk-raw-abc123def.pmtiles"));
    assert!(!first.replaced);
    assert!(!scratch_file.exists());

    write(&scratch_file, "two");
    let second = store.admit(&scratch_file, &k, &[]).expect("admit");
    assert!(second.replaced);
    assert_eq!(std::fs::read_to_string(&second.path).expect("read"), "two");
    assert_eq!(names(&root.join("store")), vec!["dk-raw-abc123def.pmtiles"]);
}

#[test]
fn admit_refusals() {
    let root = scratch("store-admit-refuse");
    let k = key("dk", "raw", "abc");
    let scratch_file = root.join("scratch/out.pmtiles");
    write(&scratch_file, "x");

    let same = ArchiveStore::new(root.join("scratch"));
    let m = message(same.admit(&scratch_file, &k, &[]));
    assert!(
        m.contains("coincides with elivagar's scratch/tmp dir"),
        "{m}"
    );

    let tmp = root.join("tmp");
    let protected = ArchiveStore::new(&tmp);
    let m = message(protected.admit(&scratch_file, &k, &[tmp.as_path()]));
    assert!(m.contains("coincides"), "{m}");
    assert!(
        scratch_file.exists(),
        "refusal must leave the output in place"
    );

    let store = ArchiveStore::new(root.join("store"));
    match store.admit(&root.join("scratch/none.pmtiles"), &k, &[]) {
        Err(DevError::Verify(m)) => assert!(m.contains("wrote no output"), "{m}"),
        other => panic!("expected a verify error, got {other:?}"),
    }
}

#[test]
fn retention_keeps_newest_per_dataset_variant_only() {
    let root = scratch("store-retention");
    let dir = root.join("store");
    let store = ArchiveStore::new(&dir);
    // Bystanders that must survive: a dash-extended variant, another
    // dataset, a non-archive, the pre-rename name form.
    for name in [
        "dk-raw-fast-old.pmtiles",
        "se-raw-old.pmtiles",
        "ocean-tiles.pmtiles",
        "dk-old.pmtiles",
    ] {
        write(&dir.join(name), "x");
        set_mtime(&dir.join(name), 1);
    }
    // Pre-existing group members with ascending mtimes.
    for i in 0..TILEGEN_RETENTION {
        let path = dir.join(format!("dk-raw-c{i}.pmtiles"));
        write(&path, "x");
        set_mtime(&path, 100 + i as u64);
    }
    let scratch_file = root.join("scratch/out.pmtiles");
    write(&scratch_file, "x");
    let admitted = store
        .admit(&scratch_file, &key("dk", "raw", "new"), &[])
        .expect("admit");
    assert!(admitted.path.exists());
    let mut expected: Vec<String> = vec![
        "dk-old.pmtiles".into(),
        "dk-raw-c1.pmtiles".into(),
        "dk-raw-c2.pmtiles".into(),
        "dk-raw-c3.pmtiles".into(),
        "dk-raw-c4.pmtiles".into(),
        "dk-raw-fast-old.pmtiles".into(),
        "dk-raw-new.pmtiles".into(),
        "ocean-tiles.pmtiles".into(),
        "se-raw-old.pmtiles".into(),
    ];
    expected.sort();
    assert_eq!(names(&dir), expected);
}

#[test]
fn resolve_explicit_file() {
    let root = scratch("store-resolve-file");
    let store = ArchiveStore::new(root.join("store"));
    let file = root.join("any.pmtiles");
    write(&file, "x");
    let f = file.display().to_string();
    assert_eq!(
        store
            .resolve("dk", "raw", None, Some(&f), &root)
            .expect("file"),
        file
    );
    let missing = root.join("missing.pmtiles").display().to_string();
    let m = message(store.resolve("dk", "raw", None, Some(&missing), &root));
    assert!(m.contains("file not found"), "{m}");
}

#[test]
fn resolve_by_head_and_by_revision() {
    let root = scratch("store-resolve-rev");
    let ids = repo_with_commits(&root, 2);
    let store = ArchiveStore::new(root.join("store"));
    let old = root.join(format!("store/dk-raw-{}.pmtiles", &ids[0][..9]));
    let head = root.join(format!("store/dk-raw-{}.pmtiles", &ids[1][..9]));

    let m = message(store.resolve("dk", "raw", None, None, &root));
    assert!(
        m.contains("no raw build for") && m.contains(&ids[1][..9]),
        "{m}"
    );

    write(&old, "x");
    write(&head, "x");
    assert_eq!(
        store.resolve("dk", "raw", None, None, &root).expect("head"),
        head
    );
    assert_eq!(
        store
            .resolve("dk", "raw", Some("HEAD~1"), None, &root)
            .expect("rev"),
        old
    );
    assert_eq!(
        store
            .resolve("dk", "raw", Some(&ids[0]), None, &root)
            .expect("full id"),
        old
    );
    // The wrong variant fails at resolution.
    let m = message(store.resolve("dk", "locations", None, None, &root));
    assert!(m.contains("no locations build"), "{m}");
}

#[test]
fn resolve_legacy_short_token_for_a_known_commit() {
    let root = scratch("store-resolve-legacy");
    let ids = repo_with_commits(&root, 1);
    let store = ArchiveStore::new(root.join("store"));
    // An archive named by a seven-digit `--short` hash before the fixed width.
    let legacy = root.join(format!("store/dk-raw-{}.pmtiles", &ids[0][..7]));
    write(&legacy, "x");
    assert_eq!(
        store
            .resolve("dk", "raw", Some("HEAD"), None, &root)
            .expect("legacy"),
        legacy
    );

    // Two candidate prefixes are ambiguous: fall back to the fixed name.
    write(
        &root.join(format!("store/dk-raw-{}.pmtiles", &ids[0][..8])),
        "x",
    );
    let m = message(store.resolve("dk", "raw", Some("HEAD"), None, &root));
    assert!(
        m.contains(&format!("no raw build for {}", &ids[0][..9])),
        "{m}"
    );
}

#[test]
fn resolve_tokens_for_revisions_git_no_longer_knows() {
    let root = scratch("store-resolve-gone");
    repo_with_commits(&root, 1);
    let store = ArchiveStore::new(root.join("store"));
    write(&root.join("store/dk-raw-deadbeef1.pmtiles"), "x");
    write(&root.join("store/dk-raw-mytag.pmtiles"), "x");

    // Exact token match.
    assert_eq!(
        store
            .resolve("dk", "raw", Some("mytag"), None, &root)
            .expect("exact"),
        root.join("store/dk-raw-mytag.pmtiles")
    );
    // A unique legacy short hash: a longer typed revision whose prefix is a
    // stored token.
    assert_eq!(
        store
            .resolve("dk", "raw", Some("deadbeef1234"), None, &root)
            .expect("legacy"),
        root.join("store/dk-raw-deadbeef1.pmtiles")
    );
    // An unresolvable typo gets the git error, not "no build".
    let m = message(store.resolve("dk", "raw", Some("typo"), None, &root));
    assert!(m.contains("cannot resolve git revision 'typo'"), "{m}");
    let m = message(store.resolve("dk", "raw", Some("a-b"), None, &root));
    assert!(m.contains("cannot resolve git revision"), "{m}");
}
