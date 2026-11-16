//! The pinned Temurin JDK and Planetiler jar the `planetiler` bench runs
//! (benches/compare/planetiler.lua). Ported from brokkr's `src/tools.rs`
//! (`ensure_planetiler_runtime`), keeping its layout under the host data
//! dir and its marker formats, so a runtime brokkr installed at these pins
//! is adopted rather than fetched again:
//!
//! ```text
//! <data>/jdk/bin/java       <data>/.jdk-version         release + archive sha256
//! <data>/planetiler.jar     <data>/.planetiler-version  release + jar sha256
//! ```
//!
//! Nothing is compiled. brokkr's elivagar path once compiled a
//! `bench/planetiler-baseline` Java class this tree never had, so javac
//! failed every run and no planetiler row was ever recorded on bygg; the
//! bench runs the release jar's own `shortbread` command instead.

use std::path::{Path, PathBuf};

use elivagar_devkit::DevError;

use crate::fetch;

// Update pins together with their digests after reviewing an upstream release.
pub const JDK_RELEASE: &str = "jdk-25.0.4.1+1";
const JDK_ARCHIVE_VERSION: &str = "25.0.4.1_1";
pub const PLANETILER_RELEASE: &str = "v0.10.2";
const PLANETILER_SHA256: &str = "f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d";

/// Ensure the pinned JDK and jar, then check the JDK starts.
pub fn install(data_dir: &Path) -> Result<(), DevError> {
    fetch::require_programs(&["curl", "tar"], "to install the JDK and Planetiler")?;
    std::fs::create_dir_all(data_dir)?;
    let java = ensure_jdk(data_dir)?;
    let jar = ensure_jar(data_dir)?;
    let java_str = java.display().to_string();
    fetch::captured(&java_str, &["-version"], data_dir)?;
    fetch::msg(&format!("java: {}", java.display()));
    fetch::msg(&format!("jar:  {}", jar.display()));
    Ok(())
}

fn ensure_jdk(data_dir: &Path) -> Result<PathBuf, DevError> {
    let jdk_dir = data_dir.join("jdk");
    let version_file = data_dir.join(".jdk-version");
    let java = jdk_dir.join("bin/java");
    let (arch, os) = platform()?;
    let checksum = jdk_checksum(arch, os)?;

    if java.exists()
        && std::fs::read_to_string(&version_file).is_ok_and(|m| jdk_marker_matches(&m, checksum))
    {
        fetch::msg(&format!("JDK {JDK_RELEASE} already installed"));
        return Ok(java);
    }

    let release = JDK_RELEASE.replace('+', "%2B");
    let archive = format!("OpenJDK25U-jdk_{arch}_{os}_hotspot_{JDK_ARCHIVE_VERSION}.tar.gz");
    let url = format!(
        "https://github.com/adoptium/temurin25-binaries/releases/download/{release}/{archive}"
    );
    let tarball = data_dir.join("jdk-download.tar.gz");
    fetch::msg(&format!("downloading JDK {JDK_RELEASE}"));
    fetch::download(&url, &tarball, Some(checksum))?;

    // Extract to staging; the installed JDK stays until the new one has
    // unpacked and has a bin/java.
    let staged = data_dir.join("jdk-extract");
    fetch::remove_any(&staged)?;
    std::fs::create_dir_all(&staged)?;
    let tarball_str = tarball.display().to_string();
    let staged_str = staged.display().to_string();
    fetch::run(
        "tar",
        &[
            "xzf",
            &tarball_str,
            "-C",
            &staged_str,
            "--strip-components=1",
        ],
        data_dir,
    )?;
    if !staged.join("bin/java").is_file() {
        return Err(DevError::Config(format!(
            "Temurin {JDK_RELEASE} archive has no bin/java"
        )));
    }
    fetch::replace_directory(&staged, &jdk_dir, &data_dir.join("jdk-old"))?;
    std::fs::write(&version_file, jdk_marker(checksum))?;
    drop(std::fs::remove_file(&tarball));
    fetch::msg(&format!("installed JDK {JDK_RELEASE}"));
    Ok(java)
}

fn ensure_jar(data_dir: &Path) -> Result<PathBuf, DevError> {
    let jar = data_dir.join("planetiler.jar");
    let version_file = data_dir.join(".planetiler-version");
    let marker = planetiler_marker();
    if jar.exists() && std::fs::read_to_string(&version_file).is_ok_and(|m| m == marker) {
        fetch::msg(&format!(
            "Planetiler {PLANETILER_RELEASE} already installed"
        ));
        return Ok(jar);
    }
    let url = format!(
        "https://github.com/onthegomap/planetiler/releases/download/{PLANETILER_RELEASE}/planetiler.jar"
    );
    fetch::msg(&format!("downloading Planetiler {PLANETILER_RELEASE}"));
    fetch::download(&url, &jar, Some(PLANETILER_SHA256))?;
    std::fs::write(&version_file, marker)?;
    fetch::msg(&format!("installed Planetiler {PLANETILER_RELEASE}"));
    Ok(jar)
}

fn platform() -> Result<(&'static str, &'static str), DevError> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        other => {
            return Err(DevError::Config(format!(
                "unsupported architecture: {other}"
            )));
        }
    };
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "mac",
        other => return Err(DevError::Config(format!("unsupported OS: {other}"))),
    };
    Ok((arch, os))
}

fn jdk_checksum(arch: &str, os: &str) -> Result<&'static str, DevError> {
    match (arch, os) {
        ("x64", "linux") => Ok("dbb698396d478e7fa2b1e50f4103324b2a99b90569ee27c33f2261f9215cf41e"),
        ("aarch64", "linux") => {
            Ok("69df11a02cfa3ef7d7ca645e03edce6778ec090e100f6ae2b42097865730ac52")
        }
        ("x64", "mac") => Ok("e6229d9504f7922053ab31821b9e6bee8761daf7b026a3476d1a027563009880"),
        ("aarch64", "mac") => {
            Ok("61979887f7506a24a57439ff99adb8b3a7fc89977d9cfe3b8984f58a981b7b9d")
        }
        _ => Err(DevError::Config(format!(
            "no JDK {JDK_RELEASE} checksum for {arch}/{os}"
        ))),
    }
}

/// `.jdk-version`: the exact Temurin release and its archive digest. The
/// bench records the first line.
fn jdk_marker(checksum: &str) -> String {
    format!("{JDK_RELEASE}\n{checksum}\n")
}

fn jdk_marker_matches(marker: &str, checksum: &str) -> bool {
    let mut lines = marker.lines().map(str::trim);
    lines.next() == Some(JDK_RELEASE) && lines.next() == Some(checksum)
}

/// `.planetiler-version`: the exact release and its jar digest. The bench
/// records the first line.
fn planetiler_marker() -> String {
    format!("{PLANETILER_RELEASE}\n{PLANETILER_SHA256}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdk_marker_pins_the_exact_release() {
        assert!(jdk_marker_matches(&jdk_marker("digest-a"), "digest-a"));
        assert!(!jdk_marker_matches(&jdk_marker("digest-a"), "digest-b"));
        assert!(!jdk_marker_matches("jdk-25.0.2+10\ndigest-a\n", "digest-a"));
        // brokkr's pre-digest marker held the release alone.
        assert!(!jdk_marker_matches("jdk-25.0.4.1+1", "digest-a"));
        assert!(!jdk_marker_matches("", "digest-a"));
    }

    #[test]
    fn planetiler_marker_leads_with_the_release() {
        assert_eq!(planetiler_marker().lines().next(), Some(PLANETILER_RELEASE));
    }
}
