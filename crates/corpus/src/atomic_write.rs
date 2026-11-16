//! Atomic replacement of a committed corpus file: write a sibling temp file,
//! fsync it, rename it over the target, fsync the directory. A reader sees the
//! old bytes or the new ones, never a prefix. A simplification of brokkr's
//! `atomic_write::replace`: same temp naming and symlink/permission handling,
//! without its reservation and durable tiers, which the corpus never used.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn temp_suffix() -> String {
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("tmp-{}-{n}", std::process::id())
}

/// Atomically replace `path` with `contents`. An existing symlink target is
/// resolved first, so the link survives and its target is replaced; the
/// target's permission bits carry over.
pub(crate) fn replace(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    stage(path, contents)?.commit()
}

/// A fully written, fsynced sibling temp file not yet renamed over its target.
/// [`Staged::commit`] performs the rename; dropping it uncommitted removes the
/// temp file. Used to stage several files before any of them changes (the
/// bless journal in `gate::corpus::journal`).
pub(crate) struct Staged {
    pub(crate) tmp: PathBuf,
    pub(crate) target: PathBuf,
    committed: bool,
}

impl Staged {
    /// Rename the temp over the target and sync the directory.
    pub(crate) fn commit(mut self) -> std::io::Result<()> {
        std::fs::rename(&self.tmp, &self.target)?;
        self.committed = true;
        sync_parent(&self.target);
        Ok(())
    }

    /// Leave the temp in place for someone else (the journal replay) to
    /// rename; dropping no longer removes it.
    pub(crate) fn keep(mut self) -> (PathBuf, PathBuf) {
        self.committed = true;
        (
            std::mem::take(&mut self.tmp),
            std::mem::take(&mut self.target),
        )
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            drop(std::fs::remove_file(&self.tmp));
        }
    }
}

/// Best-effort directory fsync after a rename, as in brokkr: the target has
/// already changed, so a failure is a warning.
pub(crate) fn sync_parent(target: &Path) {
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if let Ok(d) = File::open(&dir)
        && let Err(e) = d.sync_all()
        && !matches!(e.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP))
    {
        crate::output::warn(&format!(
            "{} was replaced, but syncing {} failed: {e}; whether the new contents \
             survive a crash is unconfirmed",
            target.display(),
            dir.display()
        ));
    }
}

/// Write `contents` to a fsynced sibling temp of `path` (symlink resolved,
/// permissions carried over) without touching `path` itself.
pub(crate) fn stage(path: &Path, contents: &[u8]) -> std::io::Result<Staged> {
    let target = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e),
    };
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}", temp_suffix()));
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(contents)?;
        if let Ok(meta) = std::fs::metadata(&target) {
            file.set_permissions(std::fs::Permissions::from_mode(
                meta.permissions().mode() & 0o7777,
            ))?;
        }
        file.sync_all()
    })();
    if let Err(e) = result {
        drop(std::fs::remove_file(&tmp));
        return Err(e);
    }
    Ok(Staged {
        tmp,
        target,
        committed: false,
    })
}
