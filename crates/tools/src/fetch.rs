//! Downloads, subprocesses and the small filesystem helpers the installers
//! share. Ported from brokkr's `src/tools.rs`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use elivagar_devkit::DevError;
use sha2::{Digest, Sha256};

/// curl flags that bound a transfer without capping its length: give up on a
/// connection that has not opened in 30s, or on a transfer averaging under
/// 1 KiB/s for two minutes. A slow but moving download survives.
const CURL_STALL_ARGS: [&str; 6] = [
    "--connect-timeout",
    "30",
    "--speed-limit",
    "1024",
    "--speed-time",
    "120",
];

/// Retry transient connection and server errors; `--retry-max-time` bounds
/// the retry window, not a transfer that is making progress.
const CURL_RETRY_ARGS: [&str; 6] = [
    "--retry",
    "3",
    "--retry-delay",
    "1",
    "--retry-max-time",
    "30",
];

/// brokkr's tagged line format: the tag padded to a 10-column field, stdout.
pub fn msg(text: &str) {
    for line in text.lines() {
        println!("[tools]   {line}");
    }
}

/// The executable `name` on PATH, or `name` itself when it is a path.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let p = PathBuf::from(name);
        return executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| executable(candidate))
}

/// Refuse unless every program in `names` is on PATH, naming what needs it.
pub fn require_programs(names: &[&str], purpose: &str) -> Result<(), DevError> {
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| find_executable(n).is_none())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(DevError::Config(format!(
            "not found in PATH (required {purpose}): {}",
            missing.join(", ")
        )))
    }
}

/// Run `program` with inherited stdio in `cwd`; a nonzero exit is an error.
pub fn run(program: &str, args: &[&str], cwd: &Path) -> Result<(), DevError> {
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| DevError::Config(format!("failed to run {program}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(DevError::Config(format!(
            "{program} {} failed: {status}",
            args.join(" ")
        )))
    }
}

/// Run `program`, returning its stdout; a nonzero exit is an error carrying
/// its stderr.
pub fn captured(program: &str, args: &[&str], cwd: &Path) -> Result<String, DevError> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| DevError::Config(format!("failed to run {program}: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(DevError::Config(format!(
            "{program} {} failed: {}\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim_end()
        )))
    }
}

/// The staged sibling a download lands in before it is renamed into place.
/// A stable name, so a killed transfer is overwritten by the retry rather
/// than leaked.
fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    dest.with_file_name(name)
}

/// Download `url` to `dest` through a staged sibling, checked against
/// `sha256` (lowercase hex) before the rename when one is given. `dest` is
/// afterwards either untouched or a complete (and, when pinned, verified)
/// transfer.
pub fn download(url: &str, dest: &Path, sha256: Option<&str>) -> Result<(), DevError> {
    let staged = partial_path(dest);
    let staged_str = staged.display().to_string();
    let mut args: Vec<&str> = CURL_STALL_ARGS.into_iter().chain(CURL_RETRY_ARGS).collect();
    args.extend(["-fL", "--progress-bar", "-o", &staged_str, url]);
    let status = Command::new("curl")
        .args(&args)
        .stdout(Stdio::null())
        .status()
        .map_err(|e| DevError::Config(format!("failed to run curl: {e}")))?;
    if !status.success() {
        drop(std::fs::remove_file(&staged));
        return Err(DevError::Config(format!(
            "download failed ({status}): {url}"
        )));
    }
    if let Some(expected) = sha256 {
        let actual = sha256_file(&staged)?;
        if actual != expected {
            drop(std::fs::remove_file(&staged));
            return Err(DevError::Config(format!(
                "sha256 mismatch downloading {url}\n  expected: {expected}\n  actual:   {actual}"
            )));
        }
    }
    std::fs::rename(&staged, dest)?;
    Ok(())
}

/// SHA-256 of a file as lowercase hex, computed in-process.
pub fn sha256_file(path: &Path) -> Result<String, DevError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Remove `path` whether it is a file, a link or a directory; absent is fine.
pub fn remove_any(path: &Path) -> Result<(), DevError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(std::fs::remove_dir_all(path)?),
        Ok(_) => Ok(std::fs::remove_file(path)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Swap an extracted directory into place without discarding the prior one
/// if the final rename fails. A leftover backup is recovered on the next run.
pub fn replace_directory(staged: &Path, destination: &Path, backup: &Path) -> Result<(), DevError> {
    if backup.exists() {
        if destination.exists() {
            std::fs::remove_dir_all(backup)?;
        } else {
            std::fs::rename(backup, destination)?;
        }
    }
    let had_destination = destination.exists();
    if had_destination {
        std::fs::rename(destination, backup)?;
    }
    if let Err(error) = std::fs::rename(staged, destination) {
        if had_destination {
            drop(std::fs::rename(backup, destination));
        }
        return Err(error.into());
    }
    if had_destination {
        std::fs::remove_dir_all(backup)?;
    }
    Ok(())
}

/// Make `link` a symlink to `target`, repairing one that points elsewhere or
/// dangles. A real directory at `link` is the user's and is used as-is.
pub fn ensure_symlink(target: &Path, link: &Path) -> Result<(), DevError> {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(link)? == target && link.exists() {
                return Ok(());
            }
            std::fs::remove_file(link)?;
        }
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xab]), "000fab");
    }

    #[test]
    fn sha256_matches_the_fips_vector() {
        let digest = hex(&Sha256::digest(b"abc"));
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn partial_sits_beside_the_destination() {
        assert_eq!(
            partial_path(Path::new("/d/planetiler.jar")),
            PathBuf::from("/d/planetiler.jar.partial")
        );
    }
}
