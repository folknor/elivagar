//! The build.rs source identity recipe, and its agreement with the pre-Cargo
//! trigger `scripts/source_identity.py`.
//!
//! The recipe files are included by path, so they are tested exactly as the
//! build script runs them. The trigger is run for real: it is the subject of
//! the agreement checks, not a stand-in. If the two read the shared inputs
//! differently, or one changes on an edit the other ignores, either the stamp
//! goes stale (trigger misses an edit the recipe covers) or the crate
//! recompiles needlessly (the reverse).

#![allow(dead_code)]

#[path = "../build/source_inputs.rs"]
mod source_inputs;

#[path = "../build/source_identity.rs"]
mod source_identity;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use source_identity::{Layout, Reading};
use source_inputs::SourceInputs;

fn trigger_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/source_identity.py")
}

/// A scratch directory under cargo's per-target temp dir, inside the build
/// tree rather than the system temp dir.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("source-identity")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

/// Fixture git in a hermetic config, so developer hooks, templates and
/// identity cannot change what the scratch repository holds.
fn sealed_git(dir: &Path, args: &[&str]) {
    let inputs = SourceInputs::embedded();
    let status = source_identity::git_command(dir, &inputs)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("fixture git runs");
    assert!(status.success(), "git {args:?}");
}

fn recipe(root: &Path) -> Reading {
    let inputs = SourceInputs::embedded();
    source_identity::read(root, &root.join("Cargo.lock"), Layout::Git, &inputs)
        .expect("the fixture is identifiable")
}

fn trigger(root: &Path) -> String {
    const PYTHON: &str = "\
import pathlib, runpy, sys
module = runpy.run_path(sys.argv[1])
value = module['identity'](pathlib.Path(sys.argv[2]), module['source_inputs']())
sys.stdout.write('None' if value is None else value)
";
    let output = Command::new("python3")
        .arg("-c")
        .arg(PYTHON)
        .arg(trigger_script())
        .arg(root)
        .output()
        .expect("run the trigger under test");
    assert!(
        output.status.success(),
        "the trigger failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = String::from_utf8(output.stdout).expect("trigger output is text");
    assert_ne!(
        value,
        "None",
        "the trigger could not describe {}",
        root.display()
    );
    value
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
    std::fs::write(path, content).expect("write fixture file");
}

#[test]
fn trigger_reads_the_shared_inputs_exactly_as_compiled() {
    const PYTHON: &str = "\
import runpy, sys
i = runpy.run_path(sys.argv[1])['source_inputs']()
lines = [str(i.untracked_content_cap), i.git_environment_prefix, i.git_metadata_directory,
         ','.join(i.excluded_suffixes), ','.join(i.excluded_paths),
         ','.join(i.root_tree_exclusions), '\\x1f'.join(i.git_pathspecs())]
sys.stdout.write('\\n'.join(lines) + '\\n')
";
    let output = Command::new("python3")
        .arg("-c")
        .arg(PYTHON)
        .arg(trigger_script())
        .output()
        .expect("run the trigger's input reader");
    assert!(
        output.status.success(),
        "the trigger could not read the shared inputs: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inputs = SourceInputs::embedded();
    let expected = [
        inputs.untracked_content_cap.to_string(),
        inputs.git_environment_prefix.clone(),
        inputs.git_metadata_directory.clone(),
        inputs.excluded_suffixes.join(","),
        inputs.excluded_paths.join(","),
        inputs.root_tree_exclusions.join(","),
        inputs.git_pathspecs().join("\x1f"),
    ]
    .join("\n")
        + "\n";
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

#[test]
fn trigger_and_recipe_change_on_the_same_edits() {
    let root = scratch("agreement");
    sealed_git(&root, &["init", "-q"]);
    write(&root, "src/lib.rs", "pub fn a() {}\n");
    write(&root, "README.md", "readme\n");
    write(&root, ".brokkr/results.db", "rows-1");
    write(&root, "Cargo.lock", "lock-1\n");
    write(&root, ".gitignore", "ignored/\n");
    sealed_git(&root, &["add", "-A"]);
    sealed_git(&root, &["commit", "-q", "-m", "init"]);

    let clean = recipe(&root);
    assert_eq!(clean.dirty, Some(false));
    assert_eq!(clean, recipe(&root), "the recipe is deterministic");

    // (label, edit, recipe digest should change, dirty after)
    type Edit = fn(&Path);
    let steps: [(&str, Edit, bool, bool); 10] = [
        (
            "markdown edit",
            |r| write(r, "README.md", "changed\n"),
            false,
            false,
        ),
        (
            "new markdown",
            |r| write(r, "notes/x.md", "n\n"),
            false,
            false,
        ),
        (
            "results.db",
            |r| write(r, ".brokkr/results.db", "rows-2"),
            false,
            false,
        ),
        (
            "ignored file",
            |r| write(r, "ignored/big.bin", "x"),
            false,
            false,
        ),
        (
            "untracked file",
            |r| write(r, "scratch.txt", "aaaa"),
            true,
            true,
        ),
        // Same path set, same length: the edit git status cannot see.
        (
            "untracked rewrite",
            |r| write(r, "scratch.txt", "bbbb"),
            true,
            true,
        ),
        (
            "tracked edit",
            |r| write(r, "src/lib.rs", "pub fn b() {}\n"),
            true,
            true,
        ),
        // A second edit of an already-modified file: the same status again.
        (
            "tracked re-edit",
            |r| write(r, "src/lib.rs", "pub fn c() {}\n"),
            true,
            true,
        ),
        (
            "staging",
            |r| sealed_git(r, &["add", "src/lib.rs"]),
            false,
            true,
        ),
        (
            "lockfile",
            |r| write(r, "Cargo.lock", "lock-2\n"),
            true,
            true,
        ),
    ];
    let mut before_recipe = clean;
    let mut before_trigger = trigger(&root);
    for (label, edit, changes, dirty) in steps {
        edit(&root);
        let after_recipe = recipe(&root);
        let after_trigger = trigger(&root);
        assert_eq!(
            after_recipe.digest != before_recipe.digest,
            changes,
            "recipe on {label}"
        );
        assert_eq!(
            after_trigger != before_trigger,
            changes,
            "trigger on {label}"
        );
        assert_eq!(after_recipe.dirty, Some(dirty), "dirty after {label}");
        before_recipe = after_recipe;
        before_trigger = after_trigger;
    }
}

#[test]
fn a_rename_onto_an_excluded_name_still_counts() {
    let root = scratch("rename");
    sealed_git(&root, &["init", "-q"]);
    write(&root, "src/x.rs", "fn x() {}\n");
    sealed_git(&root, &["add", "-A"]);
    sealed_git(&root, &["commit", "-q", "-m", "init"]);
    let clean = recipe(&root);
    sealed_git(&root, &["mv", "src/x.rs", "src/x.md"]);
    let moved = recipe(&root);
    assert_ne!(moved.digest, clean.digest);
    assert_eq!(moved.dirty, Some(true));
}

#[test]
fn a_plain_tree_is_hashed_by_content_without_its_exclusions() {
    let root = scratch("tree");
    let inputs = SourceInputs::embedded();
    let read = |root: &Path| {
        source_identity::read(root, &root.join("Cargo.lock"), Layout::Tree, &inputs)
            .expect("a plain tree is identifiable")
    };
    write(&root, "src/lib.rs", "a");
    let first = read(&root);
    assert_eq!(first.dirty, None);
    write(&root, "target/debug/out", "build output");
    write(&root, "data/huge.pbf", "dataset");
    write(&root, "docs/guide.md", "prose");
    assert_eq!(
        read(&root),
        first,
        "root exclusions and markdown are not inputs"
    );
    // Root-only: a nested directory named like an exclusion is source.
    write(&root, "src/data/table.rs", "t");
    let nested = read(&root);
    assert_ne!(nested, first);
    write(&root, "src/lib.rs", "b");
    assert_ne!(read(&root), nested);
}

#[test]
fn oversize_files_are_witnessed_not_skipped() {
    let root = scratch("oversize");
    let mut inputs = SourceInputs::embedded();
    inputs.untracked_content_cap = 4;
    let read = |root: &Path| {
        source_identity::read(root, &root.join("Cargo.lock"), Layout::Tree, &inputs)
            .expect("identifiable")
            .digest
    };
    write(&root, "big.bin", "0123456789");
    let first = read(&root);
    write(&root, "big.bin", "0123456789abc");
    assert_ne!(read(&root), first, "a length change moves the witness");
}

#[test]
fn the_shared_file_parses_and_malformed_files_are_refused() {
    let inputs = source_inputs::parse(source_inputs::SOURCE_INPUTS_TEXT).expect("parses");
    assert_eq!(inputs.untracked_content_cap, 4_194_304);
    assert_eq!(inputs.git_environment_prefix, "GIT_");
    assert_eq!(inputs.git_metadata_directory, ".git");
    assert!(inputs.excludes_path(b"README.md"));
    assert!(inputs.excludes_path(b"docs/guide/a.md"));
    assert!(inputs.excludes_path(b".brokkr/results.db"));
    assert!(!inputs.excludes_path(b"src/lib.rs"));

    let layout = |extra: &str| {
        format!(
            "untracked_content_cap = 1\n\
             git_environment_prefix = \"GIT_\"\n\
             git_metadata_directory = '.git'\n\
             excluded_paths = []\n\
             root_tree_exclusions = [\n  # comment\n  \"target\",\n]\n\
             {extra}\n"
        )
    };
    assert!(source_inputs::parse(&layout("excluded_suffixes = [\".md\"] # why")).is_ok());
    for (case, extra) in [
        ("missing", ""),
        ("duplicate entry", "excluded_suffixes = [\".md\", \".md\"]"),
        ("not a string", "excluded_suffixes = [3]"),
        ("unknown key", "excluded_suffixes = []\nsurprise = \"x\""),
        (
            "duplicate key",
            "excluded_suffixes = []\nexcluded_suffixes = []",
        ),
        ("table", "excluded_suffixes = []\n[nested]\nkey = \"x\""),
        ("escape", "excluded_suffixes = [\"\\n\"]"),
    ] {
        assert!(source_inputs::parse(&layout(extra)).is_err(), "{case}");
    }
    let negative = layout("excluded_suffixes = []")
        .replace("untracked_content_cap = 1", "untracked_content_cap = -1");
    assert!(source_inputs::parse(&negative).is_err(), "negative cap");
}
