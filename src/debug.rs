use std::sync::atomic::{AtomicU64, Ordering};

/// Whether `ELIVAGAR_LAYER_STATS` is set, read once per process: the one
/// reader both the pipeline and the ocean flush path consult.
///
/// The per-layer per-zoom sort stats (records, bytes, fanout percentiles,
/// threshold counts) are a ~800-counter firehose - roughly 26 layers x 15 zooms
/// x several metrics - that swamps `brokkr sidecar --counters` and floods an
/// optimizer's context. They are diagnostic detail wanted only during layer or
/// fanout-cap analysis, so they are emitted only when the variable is set. The
/// per-layer totals (`sort_layer_<name>_records`/`_bytes`) are always emitted.
///
/// The gate is about volume, not cost: the pipeline's are emitted at end of
/// run and never touch timing; the ocean flush path asks per flush, which is
/// why the answer is cached. brokkr's `capture_env` records the variable with
/// the run, so a run carrying the firehose is identifiable from its results row.
pub(crate) fn layer_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("ELIVAGAR_LAYER_STATS").is_some())
}

/// Emit a named phase marker to the sidecar profiler, if active.
///
/// The timestamp is monotonic microseconds from the first successful open of
/// the current `BROKKR_MARKER_FIFO` path in this process. Changing the path
/// opens a new FIFO and starts a new timestamp origin. If no sidecar is
/// running, this is a no-op. If the FIFO buffer is full, the marker is
/// silently dropped.
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
            fn reset(&self) {
                $(self.$field.store(0, Ordering::Relaxed);)*
            }
            fn drain(&self) {
                $(
                    let ns = self.$field.swap(0, Ordering::Relaxed);
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
    assemble_claim_window => "assemble_claim_window_wait_ns",
    assemble_reader_backpressure => "assemble_reader_backpressure_wait_ns",
    assemble_writer_backpressure => "assemble_writer_backpressure_wait_ns",
    assemble_encode_input => "assemble_encode_input_wait_ns",
    assemble_encode_backpressure => "assemble_encode_backpressure_wait_ns",
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
    input_hash_join => "input_hash_join_wait_ns",
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
// Ring-cap partition accounting (src/geometry/pyramid.rs). Emission runs on
// many workers and on three entry paths - the ocean extract pass, ocean-build,
// and OSM ways/relations - so the tallies live in one process-global pair of
// atomics rather than per-path locals. Observability only: the gate is
// ring-cap-census.mjs at zero over the cap, plus the internal-invariant error
// the partition returns if a shape stays over the cap in an indivisible rect.
counter_group!(RingCapCounters {
    partitions => "ring_cap_partitions",
    pieces => "ring_cap_pieces",
});

// Multipolygon assembly salvage (src/multipolygon.rs). Assembly keeps what it
// can from broken relations rather than failing them; these make the losses
// visible. `unclosed_chains` are member chains that never closed and were
// dropped, `degenerate_rings` closed rings under 3 vertices dropped,
// `short_members` member ways under 2 coordinates ignored, and
// `orphan_inners` inner rings no outer contains, kept as outer shells.
counter_group!(MultipolygonCounters {
    unclosed_chains => "multipolygon_unclosed_chains",
    degenerate_rings => "multipolygon_degenerate_rings",
    short_members => "multipolygon_short_members",
    orphan_inners => "multipolygon_orphan_inners",
});

counter_group!(BusyCounters {
    phase12_node_blocks => "phase12_node_blocks_ns",
    phase12_plan_build => "phase12_plan_build_ns",
    phase12_drain => "phase12_drain_ns",
    phase12_relation_tail => "phase12_relation_tail_ns",
});

/// Process-global stall accumulators, drained by `emit_wait_counters`.
pub static WAIT: WaitCounters = WaitCounters::new();

/// Process-global serial-actor busy-time accumulators; same lifecycle as
/// [`WAIT`]. Timed with the same [`wait_span`] RAII guard - the guard is just
/// an interval accumulator; whether the interval is a stall or busy work is
/// decided by which counter it feeds.
pub static BUSY: BusyCounters = BusyCounters::new();

/// Process-global ring-cap partition tallies; drained with [`WAIT`].
/// `partitions` counts normalized shapes that exceeded
/// [`crate::mvt::MAX_FEATURE_RINGS`] and were partitioned, `pieces` the total
/// pieces those shapes produced.
pub static RING_CAP: RingCapCounters = RingCapCounters::new();

/// Process-global multipolygon salvage tallies; drained with [`WAIT`].
pub static MULTIPOLYGON: MultipolygonCounters = MultipolygonCounters::new();

/// Discard totals left by a previous failed run before starting the next one.
/// Pipeline runs must be sequential because these accumulators are shared.
pub(crate) fn reset_run_counters() {
    WAIT.reset();
    BUSY.reset();
    RING_CAP.reset();
    MULTIPOLYGON.reset();
}

/// Drain accumulated stall, busy, ring-cap and multipolygon totals to the
/// sidecar. Call once at end of run; a no-op per counter when nothing
/// accumulated.
pub fn emit_wait_counters() {
    WAIT.drain();
    BUSY.drain();
    RING_CAP.drain();
    MULTIPOLYGON.drain();
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

/// Snapshot glibc's heap accounting at a phase boundary and emit it as
/// `malloc_held_<boundary>` / `malloc_live_<boundary>` counters (bytes).
///
/// `held` is arena + hblkhd: everything glibc has taken from the OS for the
/// heap, whether or not it is in use. `live` is uordblks + hblkhd: what the
/// program actually holds allocated. `held - live` is allocator retention -
/// the free-list and arena memory glibc keeps rather than returning, the
/// glibc analogue of the `mi_commit`-vs-RSS gap that flagged the 2026-07-14
/// regression under mimalloc. RSS, peak RSS, and page faults are already
/// covered per phase by the sidecar's /proc sampler.
///
/// mallinfo2 became a live signal again when mimalloc was removed
/// (2026-07-15): under a non-glibc global allocator it only saw
/// glibc-direct allocations - a few MB against a multi-GB RSS - which is
/// why the mimalloc-era binary read `mi_process_info` here instead.
/// Old sidecar rows therefore carry `mi_commit_<boundary>` counters, not
/// these. hotpath-alloc's CountingAllocator wraps the system allocator, so
/// the reading stays meaningful in every build.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn emit_alloc_boundary(boundary: &str) {
    // SAFETY: mallinfo2 fills and returns a struct by value; it reads no
    // input and is safe to call from any thread.
    let info = unsafe { libc::mallinfo2() };
    emit_counter_usize(&format!("malloc_held_{boundary}"), info.arena + info.hblkhd);
    emit_counter_usize(
        &format!("malloc_live_{boundary}"),
        info.uordblks + info.hblkhd,
    );
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn emit_alloc_boundary(_boundary: &str) {}

/// Shared FIFO write logic for markers and counters.
fn write_fifo(f: impl FnOnce(&std::fs::File, u128)) {
    #[cfg(unix)]
    use std::sync::{Mutex, OnceLock};

    // Keep the path with the descriptor so sequential library runs can use
    // different sidecar FIFOs in one process.
    #[cfg(unix)]
    struct FifoState {
        path: std::path::PathBuf,
        file: std::fs::File,
        start: std::time::Instant,
    }

    #[cfg(unix)]
    static STATE: OnceLock<Mutex<Option<FifoState>>> = OnceLock::new();

    #[cfg(unix)]
    {
        let state = STATE.get_or_init(|| Mutex::new(None));
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let Some(path) = std::env::var_os("BROKKR_MARKER_FIFO") else {
            *state = None;
            return;
        };
        let path = std::path::PathBuf::from(path);

        if state.as_ref().is_none_or(|cached| cached.path != path) {
            use std::os::unix::fs::OpenOptionsExt;
            #[cfg(target_os = "linux")]
            const O_NONBLOCK: i32 = 0x800;
            #[cfg(target_os = "macos")]
            const O_NONBLOCK: i32 = 0x0004;
            let Ok(file) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(O_NONBLOCK)
                .open(&path)
            else {
                *state = None;
                return;
            };
            *state = Some(FifoState {
                path,
                file,
                start: std::time::Instant::now(),
            });
        }

        if let Some(cached) = state.as_ref() {
            let us = cached.start.elapsed().as_micros();
            f(&cached.file, us);
        }
    }

    #[cfg(not(unix))]
    {
        let _ = f;
    }
}
