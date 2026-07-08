#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};

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

// ---------------------------------------------------------------------------
// Stall accounting: cumulative *_wait_ns counters
// ---------------------------------------------------------------------------
//
// Blocking time is an accumulated quantity, not a phase boundary, so it lives in
// the counter channel rather than the marker stream. Each `wait_span` measures
// one blocking interval and, on drop, adds its nanoseconds to a category's
// atomic; `emit_wait_counters` flushes the accumulated totals once at end of run
// as `<category>_wait_ns`. brokkr's `--stalls` rolls these up (max per name,
// since they are cumulative and monotonic) as a fraction of wall.
//
// Emitting stalls as FIFO markers instead is a category error: every
// phase-oriented sidecar view (default summary, --durations, --phase) treats a
// marker as a segment boundary, so a high-frequency span - one per sort chunk
// write, hundreds per run - floods those views with thousands of near-zero rows
// that bury the handful of real phase boundaries. Counters have no such
// coupling; a stall category is just one more name in the counter stream.

macro_rules! counter_group {
    ($struct_name:ident { $($field:ident => $name:literal),* $(,)? }) => {
        pub struct $struct_name {
            $(pub $field: AtomicU64,)*
        }
        impl $struct_name {
            const fn new() -> Self {
                Self { $($field: AtomicU64::new(0),)* }
            }
            fn emit(&self) {
                $(
                    let ns = self.$field.load(Ordering::Relaxed);
                    if ns > 0 {
                        emit_counter_u64($name, ns);
                    }
                )*
            }
        }
    };
}

counter_group!(WaitCounters {
    sort_chunk_write => "sort_chunk_write_wait_ns",
    sort_flush => "sort_flush_wait_ns",
    sort_open => "sort_open_wait_ns",
    sort_finish => "sort_finish_wait_ns",
    assemble_partition_batch => "assemble_partition_batch_wait_ns",
    assemble_reader_backpressure => "assemble_reader_backpressure_wait_ns",
    assemble_writer_backpressure => "assemble_writer_backpressure_wait_ns",
    assemble_encode_input => "assemble_encode_input_wait_ns",
    assemble_write_input => "assemble_write_input_wait_ns",
    assemble_reader_join => "assemble_reader_join_wait_ns",
    assemble_writer_join => "assemble_writer_join_wait_ns",
    pmtiles_write => "pmtiles_write_wait_ns",
    way_block_send => "way_block_send_wait_ns",
    way_budget => "way_budget_wait_ns",
    way_result_send => "way_result_send_wait_ns",
    node_block_send => "node_block_send_wait_ns",
    node_worker_join => "node_worker_join_wait_ns",
    prepass_join => "prepass_join_wait_ns",
    read_raw_send => "read_raw_send_wait_ns",
    read_decoded_send => "read_decoded_send_wait_ns",
    read_decoded_recv => "read_decoded_recv_wait_ns",
});

// Busy time on the actors of phase12: the pbfhogg ordered consumer
// (node-block processing, way counting), plan building (now inside the rayon
// way tasks, so read it as summed thread-time, not a serial stage), the drain
// thread's result handling, and the parallel relation tail (thread-time). These
// are NOT stalls - the `_ns` suffix without `_wait` keeps them out of the
// `--stalls` rollup. Together with the wait counters above and pbfhogg's
// pipeline_decoded_recv/send waits they split phase12's serial actors into
// busy vs blocked: the falsification kit for the ordered-drain-removal
// hypothesis (a serial actor whose busy fraction is low is not the choke).
counter_group!(BusyCounters {
    phase12_node_blocks => "phase12_node_blocks_ns",
    phase12_plan_build => "phase12_plan_build_ns",
    phase12_drain => "phase12_drain_ns",
    phase12_relation_tail => "phase12_relation_tail_ns",
});

/// Process-global stall accumulators. There is one tilegen run per process, so
/// static zero-init is correct and nothing resets between runs.
pub static WAIT: WaitCounters = WaitCounters::new();

/// Process-global serial-actor busy-time accumulators; same lifecycle as
/// [`WAIT`]. Timed with the same [`wait_span`] RAII guard - the guard is just
/// an interval accumulator; whether the interval is a stall or busy work is
/// decided by which counter it feeds.
pub static BUSY: BusyCounters = BusyCounters::new();

/// Flush the accumulated stall and busy totals to the sidecar. Call once at
/// end of run; a no-op per counter when nothing accumulated.
pub fn emit_wait_counters() {
    WAIT.emit();
    BUSY.emit();
}

/// RAII guard timing one blocking interval. On drop it adds the elapsed
/// nanoseconds to `counter`. Create via [`wait_span`]; a category never exceeds
/// wall on its own, but the cross-thread sum of concurrent waits can, which is
/// why `--stalls` reports a fraction that may exceed 100%.
pub struct WaitSpan {
    counter: &'static AtomicU64,
    start: std::time::Instant,
}

impl Drop for WaitSpan {
    fn drop(&mut self) {
        let ns = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.counter.fetch_add(ns, Ordering::Relaxed);
    }
}

/// Time a blocking interval into `counter` (a field of [`WAIT`]). Hold the
/// returned guard across the wait; it records on drop.
#[must_use]
pub fn wait_span(counter: &'static AtomicU64) -> WaitSpan {
    WaitSpan {
        counter,
        start: std::time::Instant::now(),
    }
}

/// Snapshot mimalloc's committed memory at a phase boundary and emit it as
/// `mi_commit_<boundary>` / `mi_peak_commit_<boundary>` counters (bytes).
///
/// Why commit and not the glibc `mallinfo2` we used to read: the normal binary
/// uses mimalloc as the global allocator, so `mallinfo2` (which only sees
/// glibc-direct allocations) reported a few MB of arena against a multi-GB RSS -
/// dead signal. `mi_process_info` reports mimalloc's own accounting. RSS, peak
/// RSS, and page faults are already covered per phase by the sidecar's /proc
/// sampler, so the one number worth pulling from the allocator is committed
/// bytes: how much address space mimalloc has committed, which can sit well
/// above resident bytes and is the signal for allocator retention on a
/// memory-bound run.
///
/// No-op under `hotpath-alloc`, where mimalloc is not the global allocator and
/// its accounting would be meaningless.
#[cfg(not(feature = "hotpath-alloc"))]
pub fn emit_alloc_boundary(boundary: &str) {
    let mut current_commit: usize = 0;
    let mut peak_commit: usize = 0;
    // SAFETY: mi_process_info null-checks each out-pointer before writing, so
    // passing null for the fields we don't want is the documented way to select
    // a subset. It reads no input and is safe from any thread.
    unsafe {
        libmimalloc_sys::mi_process_info(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut current_commit,
            &mut peak_commit,
            std::ptr::null_mut(),
        );
    }
    emit_counter_usize(&format!("mi_commit_{boundary}"), current_commit);
    emit_counter_usize(&format!("mi_peak_commit_{boundary}"), peak_commit);
}

#[cfg(feature = "hotpath-alloc")]
pub fn emit_alloc_boundary(_boundary: &str) {}

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
