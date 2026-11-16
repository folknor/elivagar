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
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &target)
    })();
    if let Err(e) = result {
        drop(std::fs::remove_file(&tmp));
        return Err(e);
    }
    // Best effort, as in brokkr: the target has already changed.
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
    Ok(())
}
