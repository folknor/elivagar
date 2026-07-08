#![allow(dead_code)]

/// Emit a named phase marker to the sidecar profiler, if active.
///
/// The marker is timestamped with monotonic microseconds since process start
/// and written to the FIFO at `BROKKR_MARKER_FIFO`. If no sidecar is running,
/// this is a no-op. If the FIFO buffer is full, the marker is silently dropped.
pub fn emit_marker(name: &str) {
    use std::io::Write;
    write_fifo(|f, us| {
        drop((&*f).write_all(format!("{us} {name}\n").as_bytes()));
    });
}

/// Emit a named counter value to the sidecar profiler, if active.
///
/// Counters carry application-level data through the same FIFO as phase
/// markers. The `@` prefix distinguishes counters from markers in the protocol.
///
/// Format: `<timestamp_us> @<name>=<value>\n`
pub fn emit_counter(name: &str, value: i64) {
    use std::io::Write;
    write_fifo(|f, us| {
        drop((&*f).write_all(format!("{us} @{name}={value}\n").as_bytes()));
    });
}

pub fn emit_counter_u64(name: &str, value: u64) {
    emit_counter(name, i64::try_from(value).unwrap_or(i64::MAX));
}

pub fn emit_counter_usize(name: &str, value: usize) {
    emit_counter(name, i64::try_from(value).unwrap_or(i64::MAX));
}

pub struct MarkerSpan {
    end: &'static str,
}

impl Drop for MarkerSpan {
    fn drop(&mut self) {
        emit_marker(self.end);
    }
}

pub fn marker_span(start: &'static str, end: &'static str) -> MarkerSpan {
    emit_marker(start);
    MarkerSpan { end }
}

/// Snapshot glibc allocator state via `mallinfo2()` and emit the key fields
/// as counters with `<prefix>_<field>` names.
///
/// The normal binary uses mimalloc for Rust allocations, so these counters
/// cover glibc allocations rather than mimalloc-managed Rust heap blocks.
/// They become a direct Rust heap view only in builds that use glibc as the
/// process allocator.
///
/// Fields emitted:
/// - `<prefix>_arena`: total brk-managed heap size in bytes
/// - `<prefix>_hblks`: count of mmap-managed chunks
/// - `<prefix>_hblkhd`: total bytes in mmap-managed chunks
/// - `<prefix>_uordblks`: bytes allocated in normal blocks
/// - `<prefix>_fordblks`: bytes free in normal blocks
/// - `<prefix>_keepcost`: top-most releasable block in arena
///
/// On non-glibc platforms this is a no-op.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn emit_mallinfo2(prefix: &str) {
    // SAFETY: mallinfo2 is a glibc function safe to call from any thread.
    let info = unsafe { libc::mallinfo2() };
    #[allow(clippy::cast_possible_wrap)]
    {
        emit_counter(&format!("{prefix}_arena"), info.arena as i64);
        emit_counter(&format!("{prefix}_hblks"), info.hblks as i64);
        emit_counter(&format!("{prefix}_hblkhd"), info.hblkhd as i64);
        emit_counter(&format!("{prefix}_uordblks"), info.uordblks as i64);
        emit_counter(&format!("{prefix}_fordblks"), info.fordblks as i64);
        emit_counter(&format!("{prefix}_keepcost"), info.keepcost as i64);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn emit_mallinfo2(_prefix: &str) {}

/// Ask glibc to return free chunks above the trim threshold to the OS.
/// Returns 1 if memory was released, 0 otherwise.
/// In the normal mimalloc binary this does not trim Rust heap blocks.
///
/// On non-glibc or non-Linux this is a no-op returning 0.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn malloc_trim() -> i32 {
    // SAFETY: malloc_trim is a glibc function safe to call from any thread.
    unsafe { libc::malloc_trim(0) }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn malloc_trim() -> i32 {
    0
}

/// Lower glibc's `M_MMAP_THRESHOLD` so allocations at least `bytes` route
/// through mmap-backed chunks that are released to the OS when freed.
/// In the normal mimalloc binary this does not route Rust heap blocks.
///
/// Call once early in the run; the setting is process-global. On non-glibc
/// or non-Linux this is a no-op returning 0.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn set_mmap_threshold(bytes: i32) -> i32 {
    // SAFETY: mallopt is a glibc function safe to call before allocations grow
    // past the desired threshold.
    unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, bytes) }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn set_mmap_threshold(_bytes: i32) -> i32 {
    0
}

/// Read cumulative minor and major page faults from `/proc/self/stat`.
/// Returns `(0, 0)` on failure or non-Linux.
#[cfg(target_os = "linux")]
pub fn read_page_faults() -> (u64, u64) {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return (0, 0);
    };
    // Fields are space-separated. Field 10 is minflt, field 12 is majflt.
    let mut fields = stat.split_whitespace();
    let minflt = fields
        .nth(9)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    // Skip field 11, cminflt, to get field 12.
    let majflt = fields
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    (minflt, majflt)
}

#[cfg(not(target_os = "linux"))]
pub fn read_page_faults() -> (u64, u64) {
    (0, 0)
}

/// Shared FIFO write logic for markers and counters.
fn write_fifo(f: impl FnOnce(&std::fs::File, u128)) {
    use std::sync::OnceLock;

    static STATE: OnceLock<Option<(std::fs::File, std::time::Instant)>> = OnceLock::new();

    let state = STATE.get_or_init(|| {
        let path = std::env::var("BROKKR_MARKER_FIFO").ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            #[cfg(target_os = "linux")]
            const O_NONBLOCK: i32 = 0x800;
            #[cfg(target_os = "macos")]
            const O_NONBLOCK: i32 = 0x0004;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(O_NONBLOCK)
                .open(&path)
                .ok()?;
            Some((file, std::time::Instant::now()))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    });

    if let Some((file, start)) = state.as_ref() {
        let us = start.elapsed().as_micros();
        f(file, us);
    }
}
