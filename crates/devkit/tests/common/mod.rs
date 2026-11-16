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
