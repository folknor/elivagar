//! Small local stand-ins for the brokkr internals the ported gate called.

use std::time::Duration;

/// Wall time in whole milliseconds, never rounding a nonzero span to 0.
pub(crate) fn duration_ms(duration: Duration) -> u64 {
    let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    if ms == 0 && !duration.is_zero() {
        1
    } else {
        ms
    }
}

/// This process's peak resident set (`VmHWM`, kB). `None` when it cannot be
/// read or is not reported.
pub(crate) fn self_peak_rss_kb() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = contents.lines().find_map(|line| {
        let rest = line.strip_prefix("VmHWM:")?;
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })?;
    Some(kb).filter(|kb| *kb > 0)
}

/// Unit-test scratch directories under the workspace's gitignored target dir,
/// never `/tmp`. Each name must be unique per module: the directory is
/// cleared before it is handed back.
#[cfg(test)]
pub(crate) mod test_scratch {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static TAKEN: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());

    pub(crate) fn scratch(module: &str, name: &str) -> PathBuf {
        let fresh = TAKEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((module.to_owned(), name.to_owned()));
        assert!(
            fresh,
            "scratch name {name:?} is used twice in module {module:?}; the \
             allocator clears the path, so two tests sharing it race"
        );
        // The workspace target dir (this crate sits at crates/corpus), so no
        // stray crates/corpus/target/ appears.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/elivagar-corpus")
            .join(module)
            .join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .unwrap_or_else(|e| panic!("could not clear scratch {}: {e}", dir.display()));
        }
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("could not create scratch {}: {e}", dir.display()));
        dir
    }
}
