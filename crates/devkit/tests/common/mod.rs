//! Shared scratch helpers. Scratch lives under cargo's per-target test tmp
//! dir inside the repo's target directory, never the system temp dir.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// A fresh, empty, canonical directory unique to `name`.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("elivagar-devkit")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("create scratch");
    std::fs::canonicalize(&dir).expect("canonical scratch")
}

pub fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, text).expect("write file");
}

/// Run git in `repo` with a fixed identity, panicking on failure.
pub fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=devkit-test",
            "-c",
            "user.email=devkit-test@invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Write a one-tile PMTiles archive at `path` whose metadata is `metadata`
/// verbatim. The tile payload is opaque bytes: nothing here decodes it.
pub fn write_archive(path: &Path, metadata: &str) {
    use elivagar::archive::{LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter};
    let config = PmtilesConfig::new(
        0,
        0,
        LonLatBbox {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        },
        MapCenter {
            lon: 0.0,
            lat: 0.0,
            zoom: 0,
        },
    )
    .expect("archive config");
    let mut writer = PmtilesWriter::new(config);
    writer.add_tile(0, 0, 0, b"tile").expect("add tile");
    writer.set_metadata_verbatim(metadata.to_owned());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    writer.write_to(path).expect("write archive");
}

/// Archive metadata whose provenance records `commit` as the elivagar build.
pub fn provenance_metadata(commit: &str) -> String {
    format!(
        r#"{{"elivagar":{{"schema":1,"input":{{}},"config":{{}},"build":{{"elivagar":{{"commit":"{commit}","dirty":false}}}}}}}}"#
    )
}

/// Initialise a repo at `dir` with `count` empty commits; returns their full
/// ids, oldest first.
pub fn repo_with_commits(dir: &Path, count: usize) -> Vec<String> {
    git(dir, &["init", "--quiet"]);
    (0..count)
        .map(|i| {
            git(
                dir,
                &["commit", "--quiet", "--allow-empty", "-m", &format!("c{i}")],
            );
            git(dir, &["rev-parse", "HEAD"])
        })
        .collect()
}
