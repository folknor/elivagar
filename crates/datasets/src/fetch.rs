//! Transfers and extraction, behind [`Tools`] so the acquisition logic is
//! testable without the network or an `unzip` binary.
//!
//! Ported from brokkr's `tools.rs` download helpers: curl with a stall
//! detector rather than a total-duration cap, a stable `.partial` staged
//! sibling renamed into place only after a complete (and, when pinned,
//! verified) transfer.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// curl flags that bound a transfer without capping its length: give up on a
/// connection that has not opened in 30s, or on a transfer that has averaged
/// under 1 KiB/s for two minutes. A slow but moving download survives; a
/// dead one fails, so it cannot hold the machine lock forever.
const CURL_STALL_ARGS: [&str; 6] = [
    "--connect-timeout",
    "30",
    "--speed-limit",
    "1024",
    "--speed-time",
    "120",
];

/// Retry transient connection and server errors. `--retry-max-time` bounds
/// the retry window, not a transfer that is making progress.
const CURL_RETRY_ARGS: [&str; 6] = [
    "--retry",
    "3",
    "--retry-delay",
    "1",
    "--retry-max-time",
    "30",
];

/// The two external programs acquisition drives.
pub(crate) trait Tools {
    /// Transfer `url` to `staged`, which does not exist on entry. A failed
    /// transfer may leave a partial file; the caller removes it.
    fn fetch(&self, url: &str, staged: &Path) -> Result<(), String>;
    /// Extract `zip` into `dest`, overwriting; `junk_paths` flattens the
    /// archive's directories (`unzip -j`).
    fn unzip(&self, zip: &Path, dest: &Path, junk_paths: bool) -> Result<(), String>;
}

/// curl and unzip, found once on `PATH`.
pub(crate) struct System {
    curl: PathBuf,
    unzip: PathBuf,
}

impl System {
    /// Refuse before any work when either program is missing.
    pub(crate) fn find(path: Option<&std::ffi::OsStr>) -> Result<Self, String> {
        let find = |name: &str, why: &str| {
            path.and_then(|p| find_in_path(name, p))
                .ok_or_else(|| format!("'{name}' not found in PATH (required for {why})"))
        };
        Ok(Self {
            curl: find("curl", "downloads")?,
            unzip: find("unzip", "shapefile extraction")?,
        })
    }
}

impl Tools for System {
    fn fetch(&self, url: &str, staged: &Path) -> Result<(), String> {
        let status = Command::new(&self.curl)
            .args(CURL_STALL_ARGS)
            .args(CURL_RETRY_ARGS)
            .args(["-fL", "--progress-bar", "-o"])
            .arg(staged)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("spawning curl: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("download failed ({status}): {url}"))
        }
    }

    fn unzip(&self, zip: &Path, dest: &Path, junk_paths: bool) -> Result<(), String> {
        let mut command = Command::new(&self.unzip);
        command.arg("-o");
        if junk_paths {
            command.arg("-j");
        }
        let output = command
            .arg(zip)
            .arg("-d")
            .arg(dest)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("spawning unzip: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "unzip {} failed ({}): {}",
                zip.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

fn find_in_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The stable staged sibling of `dest`: `.<name>.partial`, the name brokkr
/// used, so a killed transfer is overwritten by the retry instead of leaked.
pub(crate) fn partial(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(".{name}.partial"))
}

pub(crate) fn remove_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Download `url` to `dest` through [`partial`], checking `xxh128` (32 hex
/// digits, brokkr's registered-digest format) before the rename when given.
/// `dest` is afterwards either untouched or a complete, verified transfer.
pub(crate) fn download(
    tools: &dyn Tools,
    url: &str,
    dest: &Path,
    xxh128: Option<&str>,
) -> Result<(), String> {
    let staged = partial(dest);
    remove_if_present(&staged)?;
    let result = transfer(tools, url, &staged, dest, xxh128);
    if result.is_err() {
        // Best effort: the error being returned is the one that matters.
        drop(std::fs::remove_file(&staged));
    }
    result
}

fn transfer(
    tools: &dyn Tools,
    url: &str,
    staged: &Path,
    dest: &Path,
    xxh128: Option<&str>,
) -> Result<(), String> {
    tools.fetch(url, staged)?;
    if let Some(expected) = xxh128 {
        let actual = elivagar_devkit::tilegen::file_digest(staged).map_err(|e| e.to_string())?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(format!(
                "XXH128 mismatch downloading {url}\n  expected: {expected}\n  actual:   {actual}"
            ));
        }
    }
    std::fs::File::open(staged)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("{}: {e}", staged.display()))?;
    std::fs::rename(staged, dest)
        .map_err(|e| format!("{} -> {}: {e}", staged.display(), dest.display()))
}

/// Progress line, stderr, brokkr's `[download]` voice.
pub(crate) fn note(message: &str) {
    eprintln!("[download] {message}");
}

#[cfg(test)]
pub(crate) mod fake {
    //! A network-free [`Tools`]: `fetch` writes fixed bytes, `unzip` lays out
    //! a configured file set, and both count their calls.

    use std::cell::{Cell, RefCell};
    use std::path::{Path, PathBuf};

    pub(crate) struct Fake {
        pub(crate) bytes: Vec<u8>,
        /// Files `unzip` creates, relative to its `-d` directory.
        pub(crate) entries: Vec<String>,
        pub(crate) fetches: Cell<usize>,
        pub(crate) unzips: Cell<usize>,
        pub(crate) urls: RefCell<Vec<String>>,
        pub(crate) unzipped: RefCell<Vec<(PathBuf, bool)>>,
        pub(crate) fail_fetch: bool,
    }

    impl Fake {
        pub(crate) fn new(bytes: &[u8], entries: &[&str]) -> Self {
            Self {
                bytes: bytes.to_vec(),
                entries: entries.iter().map(|e| (*e).to_owned()).collect(),
                fetches: Cell::new(0),
                unzips: Cell::new(0),
                urls: RefCell::new(Vec::new()),
                unzipped: RefCell::new(Vec::new()),
                fail_fetch: false,
            }
        }
    }

    impl super::Tools for Fake {
        fn fetch(&self, url: &str, staged: &Path) -> Result<(), String> {
            self.fetches.set(self.fetches.get() + 1);
            self.urls.borrow_mut().push(url.to_owned());
            assert!(!staged.exists(), "staged path not cleared");
            std::fs::write(staged, &self.bytes).map_err(|e| e.to_string())?;
            if self.fail_fetch {
                return Err("injected transfer failure".into());
            }
            Ok(())
        }

        fn unzip(&self, zip: &Path, dest: &Path, junk_paths: bool) -> Result<(), String> {
            self.unzips.set(self.unzips.get() + 1);
            assert!(zip.is_file(), "unzip of a missing archive");
            self.unzipped
                .borrow_mut()
                .push((dest.to_path_buf(), junk_paths));
            // An entry under a directory belongs to the archive of that
            // name (`<dir>.zip`), as osmdata's ocean zips are laid out; flat
            // entries are extracted from every archive.
            let stem = zip
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".zip"))
                .unwrap_or_default();
            for entry in &self.entries {
                if let Some((dir, _)) = entry.split_once('/')
                    && dir != stem
                {
                    continue;
                }
                let path = dest.join(entry);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&path, b"component").map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::test_scratch::scratch;

    #[test]
    fn partial_is_a_hidden_stable_sibling() {
        assert_eq!(
            partial(Path::new("/d/x.zip")),
            PathBuf::from("/d/.x.zip.partial")
        );
    }

    #[test]
    fn pinned_download_commits_only_a_matching_transfer() {
        let dir = scratch("fetch", "pinned");
        let probe = dir.join("probe");
        std::fs::write(&probe, b"payload").unwrap();
        let good = elivagar_devkit::tilegen::file_digest(&probe).unwrap();
        let dest = dir.join("a.zip");
        let fake = fake::Fake::new(b"payload", &[]);

        let bad = "0".repeat(32);
        let err = download(&fake, "u", &dest, Some(&bad)).unwrap_err();
        assert!(err.contains("XXH128 mismatch"), "{err}");
        assert!(!dest.exists());
        assert!(!partial(&dest).exists());

        download(&fake, "u", &dest, Some(&good)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        assert!(!partial(&dest).exists());
    }

    #[test]
    fn failed_transfer_leaves_neither_dest_nor_partial() {
        let dir = scratch("fetch", "failed");
        let dest = dir.join("a.zip");
        std::fs::write(partial(&dest), b"stale from a killed run").unwrap();
        let mut fake = fake::Fake::new(b"half", &[]);
        fake.fail_fetch = true;
        assert!(download(&fake, "u", &dest, None).is_err());
        assert!(!dest.exists());
        assert!(!partial(&dest).exists());
    }

    #[test]
    fn programs_are_found_only_as_absolute_executables() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("fetch", "path");
        for name in ["curl", "unzip"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let path = std::env::join_paths(["relative", dir.to_str().unwrap()]).unwrap();
        assert!(System::find(Some(&path)).is_err());
        for name in ["curl", "unzip"] {
            std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        assert!(System::find(Some(&path)).is_ok());
        assert!(System::find(None).is_err());
    }
}
