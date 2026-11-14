//! Memory budgets, worker sizing and environment knobs: the one place their
//! defaults, minimums and derivations live.
//!
//! [`crate::TilegenConfig`] carries what the caller chose: an `Option` budget
//! is "use the default", never a zero sentinel. [`Resources::resolve`] turns
//! that into the values the run uses, once, at the top of the pipeline. Two
//! derivations depend on facts discovered later, so they are methods here
//! rather than fields: the way-admission shape needs the locations mode phase12
//! reads from the PBF header, and the assembly worker count needs the
//! partition count. The rule still lives here; only its input arrives late.
//!
//! None of these values is part of the provenance contract: they shape memory
//! and concurrency, never what a consumer decodes.

use std::num::NonZeroUsize;

use crate::debug::emit_counter_usize;
use crate::pipeline::{PipelineError, TilegenConfig};

/// A positive byte budget.
///
/// Positivity is the type's guarantee; the per-budget minimums are checked by
/// [`Resources::resolve`], because they differ by field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteBudget(NonZeroUsize);

impl ByteBudget {
    /// `None` for zero.
    #[must_use]
    pub const fn new(bytes: usize) -> Option<Self> {
        match NonZeroUsize::new(bytes) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

const MIB: usize = 1024 * 1024;

/// Smallest accepted sort chunk budget.
pub const MIN_SORT_BUDGET: usize = 64 * MIB;
/// Smallest accepted way in-flight and assemble batch budget.
pub const MIN_BUDGET: usize = MIB;

/// Default per-chunk sort buffer.
pub const DEFAULT_SORT_BUDGET: usize = 1 << 30;
/// Default assemble batch budget.
pub const DEFAULT_ASSEMBLE_BUDGET: usize = 32 * MIB;
/// Default way in-flight budget on the node-store path, in raw bytes.
pub const DEFAULT_WAY_BUDGET: usize = 128 * MIB;
/// Default way in-flight budget for locations-on-ways input, in RAW bytes:
/// decompressed block size plus, once a task has built its plans, the plans'
/// measured bytes. The primary admission control is the count ceiling
/// (`threads.max(8)` blocks in flight); this budget is a safety net against
/// pathologically fat blocks only, and on any input seen so far it never
/// binds.
///
/// History: admission used to charge decompressed_size x 10 against the
/// budget, which made admission concurrency inversely proportional to
/// upstream blob packing - an input property this pipeline does not control.
/// Extract blobs (8,000 elements, every Geofabrik file) cost ~35MB of budget
/// so ~20 admitted; planet blobs (~66,500 elements, ~4.8MB compressed) cost
/// several hundred MB so 2-3 admitted, and planet phase12 ran at 7.2 of 32
/// cores with way_budget blocking 63.5% of wall. Raising the default
/// (768M -> 8G, 2026-07-31) took planet 1062.5s -> 571.7s but was a
/// workaround: a 500k-element blob would have re-broken it. The 2026-08-07
/// rework removed the multiplier so the count ceiling rules on any packing;
/// the budget survives only to bound real memory when blocks are individually
/// huge. Real in-flight memory is (count x raw block+plan bytes):
/// planet-shaped 30MB blocks x 32 threads is ~1GB against this 4G net, while
/// a hypothetical 500k-element blob (~270MB raw) is bounded at ~15 in flight.
pub const DEFAULT_WAY_BUDGET_LOCATIONS: usize = 4 * 1024 * MIB;

/// Relation blocks buffer at most this many decompressed bytes in RAM; past
/// the cap the tail re-reads relation blobs from the PBF instead (planet-scale
/// inputs hold several GB of relation blocks - an input-scaled stock the
/// 30 GB RAM ledger cannot absorb).
const REL_BLOCKS_BUFFER_CAP: usize = 1024 * MIB;
/// Assembly worker ceiling. Measured on NA locations (2026-07-09): 4 workers
/// left the writer idle 68% of assemble; 8 workers cut assemble 186.4 ->
/// 173.0s under the old count window and 160.7s with the byte-budgeted claim
/// window; 12 was WORSE (176.1s, +3 GB RSS) - encode CPU saturates around 8.
const ASSEMBLE_WORKER_CAP: usize = 8;
const ASSEMBLE_SPLIT_TARGET: u64 = 64 * MIB as u64;
const ASSEMBLE_PARK_BUDGET: usize = 2 * 1024 * MIB;

/// The CLI's thread default: logical CPUs, or 4 when that cannot be read.
#[must_use]
pub fn default_threads() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::new(4).expect("4 is nonzero"))
}

#[derive(Clone, Copy)]
enum EnvZero {
    Allow,
    Reject,
}

/// Unset knobs use their default; set knobs must be plain decimal integers.
fn env_integer<T>(name: &str, default: T, zero: EnvZero) -> Result<T, PipelineError>
where
    T: std::str::FromStr + PartialEq + From<u8>,
    T::Err: std::fmt::Display,
{
    let Some(raw) = std::env::var_os(name) else {
        return Ok(default);
    };
    let value = raw
        .into_string()
        .map_err(|_| PipelineError(format!("invalid {name}: value is not valid UTF-8")))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PipelineError(format!(
            "invalid {name}={value:?}: expected a decimal integer without signs, whitespace or size suffixes"
        )));
    }
    let parsed = value
        .parse::<T>()
        .map_err(|error| PipelineError(format!("invalid {name}={value:?}: {error}")))?;
    if matches!(zero, EnvZero::Reject) && parsed == T::from(0) {
        return Err(PipelineError(format!(
            "invalid {name}={value:?}: value must be greater than zero"
        )));
    }
    Ok(parsed)
}

fn budget(
    name: &str,
    value: Option<ByteBudget>,
    min: usize,
    default: usize,
) -> Result<usize, PipelineError> {
    match value {
        None => Ok(default),
        Some(b) if b.get() >= min => Ok(b.get()),
        Some(b) => Err(PipelineError(format!(
            "{name} of {} bytes is below the minimum of {min} bytes",
            b.get()
        ))),
    }
}

/// Phase12's way-admission shape, which depends on the locations mode.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WayAdmission {
    /// Raw-byte safety net over in-flight way blocks.
    pub(crate) budget: usize,
    /// Count ceiling on in-flight way blocks: the primary admission control.
    pub(crate) max_inflight: usize,
    /// Node block workers: one on the raw path, whose sorted store needs
    /// sequential puts; a small pool in locations mode, which has no store.
    pub(crate) node_workers: usize,
}

/// What a caller asked for: three optional byte budgets, an optional dedup
/// cap and the worker sizing input. `None` selects the default.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResourceRequest {
    pub(crate) threads: NonZeroUsize,
    pub(crate) way_inflight: Option<ByteBudget>,
    pub(crate) assemble_batch: Option<ByteBudget>,
    pub(crate) sort_chunk: Option<ByteBudget>,
    pub(crate) dedup_cap: Option<NonZeroUsize>,
}

/// The effective budgets, worker sizing and environment knobs of one run.
#[derive(Clone, Debug)]
pub(crate) struct Resources {
    pub(crate) threads: usize,
    pub(crate) sort_chunk: usize,
    way_budget: Option<usize>,
    pub(crate) assemble_batch: usize,
    pub(crate) dedup_cap: usize,
    /// pbfhogg decode workers.
    pub(crate) decode_threads: usize,
    /// `ELIVAGAR_REL_BLOCKS_CAP`: debug hook; zero forces the re-read path.
    pub(crate) rel_blocks_buffer_cap: usize,
    /// `ELIVAGAR_ASSEMBLE_WORKERS`.
    assemble_worker_cap: usize,
    /// `ELIVAGAR_ASSEMBLE_SPLIT_TARGET`.
    pub(crate) assemble_split_target: u64,
    /// `ELIVAGAR_ASSEMBLE_PARK_BUDGET`.
    pub(crate) assemble_park_budget: usize,
}

impl Resources {
    /// Validate and resolve every budget and environment knob.
    ///
    /// Every knob is read here, so a malformed one fails the run up front even
    /// when the phase that uses it would be skipped.
    pub(crate) fn resolve(config: &TilegenConfig) -> Result<Self, PipelineError> {
        Self::resolve_request(&ResourceRequest {
            threads: config.threads,
            way_inflight: config.way_inflight_budget,
            assemble_batch: config.assemble_batch_budget,
            sort_chunk: config.sort_chunk_size,
            dedup_cap: config.dedup_cap,
        })
    }

    /// Resolve an explicit request. The ocean artifact builder has no run
    /// config and asks for the defaults through this.
    pub(crate) fn resolve_request(request: &ResourceRequest) -> Result<Self, PipelineError> {
        let config = request;
        let threads = config.threads.get();
        let way_budget = match config.way_inflight {
            None => None,
            some => Some(budget("way in-flight budget", some, MIN_BUDGET, 0)?),
        };
        Ok(Self {
            threads,
            sort_chunk: budget(
                "sort chunk budget",
                config.sort_chunk,
                MIN_SORT_BUDGET,
                DEFAULT_SORT_BUDGET,
            )?,
            way_budget,
            assemble_batch: budget(
                "assemble batch budget",
                config.assemble_batch,
                MIN_BUDGET,
                DEFAULT_ASSEMBLE_BUDGET,
            )?,
            dedup_cap: config
                .dedup_cap
                .map_or(crate::archive::DEFAULT_DEDUP_CAP, NonZeroUsize::get),
            decode_threads: (threads / 3).max(1),
            rel_blocks_buffer_cap: env_integer(
                "ELIVAGAR_REL_BLOCKS_CAP",
                REL_BLOCKS_BUFFER_CAP,
                EnvZero::Allow,
            )?,
            assemble_worker_cap: env_integer(
                "ELIVAGAR_ASSEMBLE_WORKERS",
                ASSEMBLE_WORKER_CAP,
                EnvZero::Reject,
            )?,
            assemble_split_target: env_integer(
                "ELIVAGAR_ASSEMBLE_SPLIT_TARGET",
                ASSEMBLE_SPLIT_TARGET,
                EnvZero::Reject,
            )?,
            assemble_park_budget: env_integer(
                "ELIVAGAR_ASSEMBLE_PARK_BUDGET",
                ASSEMBLE_PARK_BUDGET,
                EnvZero::Reject,
            )?,
        })
    }

    /// Way admission for the input's locations mode.
    pub(crate) fn way_admission(&self, locations_on_ways: bool) -> WayAdmission {
        let default = if locations_on_ways {
            DEFAULT_WAY_BUDGET_LOCATIONS
        } else {
            DEFAULT_WAY_BUDGET
        };
        WayAdmission {
            budget: self.way_budget.unwrap_or(default),
            max_inflight: self.threads.max(8),
            node_workers: if locations_on_ways {
                (self.threads / 4).clamp(2, 6)
            } else {
                1
            },
        }
    }

    /// Assembly partition workers for `partition_count` partitions.
    pub(crate) fn assemble_workers(&self, partition_count: usize) -> usize {
        self.threads
            .clamp(1, self.assemble_worker_cap)
            .min(partition_count)
    }

    /// Emit the resolved limits as `res_*` sidecar counters. Counts that
    /// depend on later facts are emitted where they are derived.
    pub(crate) fn emit(&self) {
        emit_counter_usize("res_threads", self.threads);
        emit_counter_usize("res_sort_chunk_bytes", self.sort_chunk);
        emit_counter_usize("res_assemble_batch_bytes", self.assemble_batch);
        emit_counter_usize("res_dedup_cap", self.dedup_cap);
        emit_counter_usize("res_decode_threads", self.decode_threads);
        emit_counter_usize("res_rel_blocks_buffer_cap", self.rel_blocks_buffer_cap);
        emit_counter_usize("res_assemble_worker_cap", self.assemble_worker_cap);
        emit_counter_usize(
            "res_assemble_split_target",
            usize::try_from(self.assemble_split_target).unwrap_or(usize::MAX),
        );
        emit_counter_usize("res_assemble_park_budget", self.assemble_park_budget);
    }
}

impl WayAdmission {
    pub(crate) fn emit(&self) {
        emit_counter_usize("res_way_budget_bytes", self.budget);
        emit_counter_usize("res_way_max_inflight", self.max_inflight);
        emit_counter_usize("res_node_workers", self.node_workers);
    }
}
