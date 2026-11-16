//! The binary end to end: tilegen admission naming, `archive` resolution and
//! `clean`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::{provenance_metadata, repo_with_commits, scratch, write, write_archive};
use elivagar_devkit::DevError;
use elivagar_devkit::archive_store::built_commit;

const DATASET: &str = "[box.datasets.dk.pbf.raw]\nfile = \"dk.osm.pbf\"\n";

/// A project under scratch `name`: brokkr.toml with host `box`, an empty
/// tilegen block, dataset `dk`/`raw` and its PBF.
fn project(name: &str, extra: &str) -> PathBuf {
    let root = scratch(name);
    write(
        &root.join("brokkr.toml"),
        &format!("[box]\n[box.tilegen.default]\n{DATASET}{extra}"),
    );
    write(&root.join("data/dk.osm.pbf"), "x");
    root
}

/// The stand-in elivagar: `/bin/sh` reading the script `run` from the
/// project root, since tilegen's argv starts with `run` and the child runs
/// there. It copies `$FAKE_ARCHIVE` to the `-o` path. Running a written file
/// through sh, rather than executing it, sidesteps ETXTBSY races with other
/// test threads forking.
fn fake_elivagar(root: &Path) {
    write(
        &root.join("run"),
        "out=\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = -o ]; then out=$2; fi\n  shift\ndone\n\
         cp \"$FAKE_ARCHIVE\" \"$out\"\n",
    );
}

fn devkit(root: &Path, args: &[&str], fake_archive: Option<&Path>) -> Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_elivagar-devkit"));
    cmd.current_dir(root)
        .args(["--host", "box"])
        .args(args)
        .env_remove("BROKKR_WITH_EXE");
    if let Some(archive) = fake_archive {
        cmd.env("FAKE_ARCHIVE", archive);
    }
    cmd.output().expect("run elivagar-devkit")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn tilegen(root: &Path, archive: &Path) -> Output {
    fake_elivagar(root);
    devkit(
        root,
        &["--elivagar", "/bin/sh", "tilegen", "--dataset", "dk"],
        Some(archive),
    )
}

#[test]
fn tilegen_names_the_archive_by_the_binary_commit_not_head() {
    let root = project("cli-tilegen-commit", "");
    let ids = repo_with_commits(&root, 2);
    // The binary was built from the older commit, as under --with-commit.
    let fixture = root.join("fixture/built.pmtiles");
    write_archive(&fixture, &provenance_metadata(&ids[0]));

    let output = tilegen(&root, &fixture);
    assert!(output.status.success(), "{}", stdout(&output));
    assert_eq!(
        names(&root.join("data/tilegen")),
        vec![format!("dk-raw-{}.pmtiles", &ids[0][..9])]
    );
    let out = stdout(&output);
    assert!(
        out.contains(&format!("built from {}", &ids[0][..9])),
        "{out}"
    );
    assert!(out.contains(&format!("HEAD {}", &ids[1][..9])), "{out}");
}

#[test]
fn tilegen_at_head_is_named_by_head_without_a_warning() {
    let root = project("cli-tilegen-head", "");
    let ids = repo_with_commits(&root, 1);
    let fixture = root.join("fixture/built.pmtiles");
    write_archive(&fixture, &provenance_metadata(&ids[0]));

    let output = tilegen(&root, &fixture);
    assert!(output.status.success(), "{}", stdout(&output));
    assert_eq!(
        names(&root.join("data/tilegen")),
        vec![format!("dk-raw-{}.pmtiles", &ids[0][..9])]
    );
    assert!(!stdout(&output).contains("not the checkout's HEAD"));
}

#[test]
fn tilegen_refuses_an_archive_whose_commit_is_unknown() {
    let cases = [
        ("cli-tilegen-unknown", provenance_metadata("unknown")),
        ("cli-tilegen-short", provenance_metadata("abc1234")),
        ("cli-tilegen-absent", "{}".to_owned()),
        (
            "cli-tilegen-nobuild",
            r#"{"elivagar":{"schema":1,"input":{},"config":{}}}"#.to_owned(),
        ),
    ];
    for (name, metadata) in cases {
        let root = project(name, "");
        repo_with_commits(&root, 1);
        let fixture = root.join("fixture/built.pmtiles");
        write_archive(&fixture, &metadata);

        let output = tilegen(&root, &fixture);
        let out = stdout(&output);
        assert!(!output.status.success(), "{name}: {out}");
        assert!(
            out.contains("the commit that built it is unknown"),
            "{name}: {out}"
        );
        assert!(names(&root.join("data/tilegen")).is_empty(), "{name}");
        assert!(
            root.join("data/scratch/bench-self-output.pmtiles").exists(),
            "{name}: the scratch output must be left in place"
        );
    }
}

#[test]
fn built_commit_reads_provenance() {
    let root = scratch("cli-built-commit");
    let full = "0123456789abcdef0123456789abcdef01234567";
    let path = root.join("a.pmtiles");
    write_archive(&path, &provenance_metadata(full));
    assert_eq!(built_commit(&path).expect("commit").as_str(), full);

    let not_archive = root.join("b.pmtiles");
    write(&not_archive, "not pmtiles");
    match built_commit(&not_archive) {
        Err(DevError::Verify(m)) => assert!(m.contains("unreadable archive metadata"), "{m}"),
        other => panic!("expected a verify error, got {other:?}"),
    }
}

#[test]
fn tilegen_dry_run_names_the_slot_not_head() {
    let root = project("cli-tilegen-dry", "");
    let output = devkit(&root, &["--dry-run", "tilegen", "--dataset", "dk"], None);
    let out = stdout(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("dk-raw-COMMIT.pmtiles"), "{out}");
}

#[test]
fn archive_prints_the_resolved_path() {
    let root = project("cli-archive", "");
    let ids = repo_with_commits(&root, 2);
    let head = root.join(format!("data/tilegen/dk-raw-{}.pmtiles", &ids[1][..9]));
    let old = root.join(format!("data/tilegen/dk-raw-{}.pmtiles", &ids[0][..9]));
    write(&head, "x");
    write(&old, "x");

    let output = devkit(&root, &["archive", "--dataset", "dk"], None);
    assert!(output.status.success(), "{}", stdout(&output));
    assert_eq!(stdout(&output), format!("{}\n", head.display()));

    let output = devkit(
        &root,
        &["archive", "--dataset", "dk", "--commit", "HEAD~1"],
        None,
    );
    assert_eq!(stdout(&output), format!("{}\n", old.display()));

    let file = root.join("data/ocean-tiles.pmtiles");
    write(&file, "x");
    let f = file.display().to_string();
    let output = devkit(&root, &["archive", "--file", &f], None);
    assert_eq!(stdout(&output), format!("{f}\n"));

    let output = devkit(&root, &["archive", "--commit", "HEAD", "--file", &f], None);
    assert!(!output.status.success(), "--commit and --file conflict");

    let output = devkit(
        &root,
        &["archive", "--dataset", "dk", "--variant", "locations"],
        None,
    );
    let out = stdout(&output);
    assert!(!output.status.success(), "{out}");
    assert!(out.contains("no locations build"), "{out}");
    assert!(
        out.contains("brokkr run --with elivagar --with-commit")
            && out.contains("elivagar-devkit -- tilegen --dataset dk --variant locations"),
        "{out}"
    );
    assert!(!out.contains("brokkr tilegen"), "{out}");
}

/// The clean fixture: every target class, plus bystanders that must stay.
fn clean_fixture(name: &str, extra: &str) -> PathBuf {
    let root = project(name, extra);
    write(&root.join("data/tilegen_tmp/chunk"), "x");
    write(&root.join("data/corpus-calibrands/a.pmtiles"), "x");
    write(&root.join("data/scratch/keep"), "x");
    write(&root.join("data/ocean-tiles.pmtiles"), "x");
    write(&root.join("corpus/dk/digest"), "x");
    let sort = root.join(".brokkr/corpus-sort");
    // pid 0 is never a user process, so its spill is stale.
    write(&sort.join("brokkr-corpus-sort-0-1/run"), "x");
    // This test process is alive, so its spill is not.
    write(
        &sort.join(format!("brokkr-corpus-sort-{}-2/run", std::process::id())),
        "x",
    );
    write(&sort.join("mine/run"), "x");
    root
}

#[test]
fn clean_removes_only_its_targets() {
    let root = clean_fixture("cli-clean", "");
    let live = format!("brokkr-corpus-sort-{}-2", std::process::id());

    let output = devkit(&root, &["clean"], None);
    let out = stdout(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("cleaned tilegen_tmp"), "{out}");
    assert!(out.contains("cleaned corpus-calibrands"), "{out}");
    assert!(
        out.contains("removed 1 stale corpus-sort spill dir"),
        "{out}"
    );
    assert!(!root.join("data/tilegen_tmp").exists());
    assert!(!root.join("data/corpus-calibrands").exists());
    assert_eq!(
        names(&root.join(".brokkr/corpus-sort")),
        vec![live, "mine".to_owned()]
    );
    assert!(
        root.join("data/scratch/keep").exists(),
        "scratch is brokkr's"
    );
    assert!(root.join("data/ocean-tiles.pmtiles").exists());
    assert!(root.join("corpus/dk/digest").exists());

    let output = devkit(&root, &["clean"], None);
    assert!(stdout(&output).contains("nothing to clean"));
}

#[test]
fn clean_dry_run_removes_nothing() {
    let root = clean_fixture("cli-clean-dry", "");
    write(&root.join("data/tilegen/dk-raw-a.pmtiles"), "x");
    write(&root.join("data/tilegen/dk-raw-b.pmtiles"), "x");
    let output = devkit(
        &root,
        &["--dry-run", "clean", "--archives", "--keep", "0"],
        None,
    );
    let out = stdout(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("[dry-run] would clean tilegen_tmp"), "{out}");
    assert!(out.contains("would remove 2 archives"), "{out}");
    assert!(root.join("data/tilegen_tmp/chunk").exists());
    assert!(root.join("data/corpus-calibrands/a.pmtiles").exists());
    assert!(
        root.join(".brokkr/corpus-sort/brokkr-corpus-sort-0-1")
            .exists()
    );
    assert_eq!(names(&root.join("data/tilegen")).len(), 2);
}

fn set_mtime(path: &Path, secs: u64) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open");
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .expect("set mtime");
}

#[test]
fn clean_archives_keeps_newest_per_configured_group() {
    let extra = "[box.datasets.dk.pbf.locations]\nfile = \"dk.osm.pbf\"\n";
    let root = clean_fixture("cli-clean-archives", extra);
    let store = root.join("data/tilegen");
    for (name, secs) in [
        ("dk-raw-a.pmtiles", 1),
        ("dk-raw-b.pmtiles", 2),
        ("dk-raw-c.pmtiles", 3),
        ("dk-locations-a.pmtiles", 1),
        ("dk-locations-b.pmtiles", 2),
        // Not a configured group, a hand-named file, a pre-rename name.
        ("se-raw-a.pmtiles", 1),
        ("mine.pmtiles", 1),
        ("dk-a.pmtiles", 1),
    ] {
        write(&store.join(name), "x");
        set_mtime(&store.join(name), secs);
    }

    let output = devkit(&root, &["clean", "--keep", "1"], None);
    assert!(!output.status.success(), "--keep needs --archives");

    let output = devkit(&root, &["clean", "--archives", "--keep", "1"], None);
    let out = stdout(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("removed 3 archives"), "{out}");
    assert_eq!(
        names(&store),
        vec![
            "dk-a.pmtiles",
            "dk-locations-b.pmtiles",
            "dk-raw-c.pmtiles",
            "mine.pmtiles",
            "se-raw-a.pmtiles",
        ]
    );

    // Without --archives the store is untouched.
    let output = devkit(&root, &["clean"], None);
    assert!(output.status.success());
    assert_eq!(names(&store).len(), 5);
}

#[test]
fn clean_leaves_tilegen_tmp_that_is_the_scratch_dir() {
    // scratch = data/tilegen_tmp: tilegen_tmp is then scratch, which is
    // brokkr's to clean.
    let root = scratch("cli-clean-tmp-is-scratch");
    write(
        &root.join("brokkr.toml"),
        &format!("[box]\nscratch = \"data/tilegen_tmp\"\n[box.tilegen.default]\n{DATASET}"),
    );
    write(&root.join("data/tilegen_tmp/keep"), "x");
    let output = devkit(&root, &["clean"], None);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(root.join("data/tilegen_tmp/keep").exists());
}

#[test]
fn clean_refuses_a_target_that_holds_a_protected_dir() {
    // An output store inside the calibrand dir would go with it.
    let root = scratch("cli-clean-protected");
    write(
        &root.join("brokkr.toml"),
        &format!(
            "[box]\noutput = \"data/corpus-calibrands/store\"\n[box.tilegen.default]\n{DATASET}"
        ),
    );
    write(
        &root.join("data/corpus-calibrands/store/dk-raw-a.pmtiles"),
        "x",
    );
    let output = devkit(&root, &["clean"], None);
    let out = stdout(&output);
    assert!(!output.status.success(), "{out}");
    assert!(out.contains("refusing to remove"), "{out}");
    assert!(
        root.join("data/corpus-calibrands/store/dk-raw-a.pmtiles")
            .exists()
    );
}
