//! The three-pass diff engine.
//!
//! The unit of work is not a tile but a **blob pair**. PMTiles addresses tiles
//! through run-length directory entries, so one stored blob commonly serves
//! thousands of addressed tiles (empty ocean at low zoom, repeated land fill);
//! a denmark archive addresses ~1.3M tiles from ~166k unique blobs. Diffing per
//! tile would decode the same bytes over and over. Instead `merge_runs` cuts the
//! two directories into spans where both sides are constant, spans sharing a
//! `(current, baseline)` blob pair collapse into one work item, and the verdict
//! is multiplied back out by tile count at report time.
//!
//! Each pass only sees what the previous one could not settle:
//!
//! 1. **raw** - byte equality of the two stored blobs. Settles the overwhelming
//!    majority on a near-identical pair, at memcmp speed.
//! 2. **canonical** - the semantic hash per blob (computed once per blob, not
//!    once per pair) catches tiles that differ only in the intra-layer feature
//!    order the pipeline leaves unconstrained.
//! 3. **detail** - full structural decode and diff. The expensive tier, and the
//!    only one that produces classified events.
//!
//! Ported from elivagar's shed `regress.rs` (commit `0129ef3~1`). The one
//! deliberate omission is the counter FIFO emission: in elivagar those counters
//! went to brokkr's sidecar from a child process, and brokkr is the process that
//! drains that FIFO, so the same numbers are reported in-band instead (the
//! `regress ...` line of `print_text`, and `counters` in `--json`).

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use super::super::corpus::canonical::{DecodeScratch, semantic_hash};
use super::super::eliv::{ArchiveView, BlobRef, Strictness, next_zoom_boundary, tile_id_to_zxy};
use super::compare::{DetailOutcome, OutcomeClass, compare_detail_tiles};
use super::prepared::decode_prepared_tile;
use super::report::{
    DiffTotals, ExampleKey, ExampleSelector, LayerCounters, LayerInterner, RegressConfig,
    RegressReport, TileRange,
};

// ---------------------------------------------------------------------------
// Run-span planning
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct TileRun {
    start: u64,
    end: u64,
    blob: BlobRef,
}

/// A maximal tile id range over which both archives hold one constant blob (or
/// none). `current`/`baseline` are `None` where that side does not address the
/// range at all.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PairSpan {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) current: Option<BlobRef>,
    pub(crate) baseline: Option<BlobRef>,
}

impl PairSpan {
    fn tiles(self) -> u64 {
        self.end - self.start
    }
}

fn archive_runs(archive: &ArchiveView) -> io::Result<Vec<TileRun>> {
    archive
        .read_all_runs()?
        .into_iter()
        .map(|entry| {
            let end = entry
                .tile_id
                .checked_add(u64::from(entry.run_length))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tile run overflows"))?;
            Ok(TileRun {
                start: entry.tile_id,
                end,
                blob: entry.blob(),
            })
        })
        .collect()
}

/// Cut the two run lists into spans constant on both sides.
///
/// Spans are additionally clipped at zoom boundaries so every span sits at a
/// single zoom - the report is keyed by `(zoom, layer)`, and a span straddling
/// two zooms could not be attributed to either.
fn merge_runs(current: &[TileRun], baseline: &[TileRun]) -> Vec<PairSpan> {
    let mut spans = Vec::new();
    let (mut ci, mut bi) = (0usize, 0usize);
    let (mut cpos, mut bpos) = (0u64, 0u64);

    while ci < current.len() || bi < baseline.len() {
        let cur = current.get(ci);
        let bl = baseline.get(bi);
        let next_current = cur.map_or(u64::MAX, |run| run.start.max(cpos));
        let next_baseline = bl.map_or(u64::MAX, |run| run.start.max(bpos));
        let start = next_current.min(next_baseline);
        let cur_active = cur.filter(|run| cpos.max(run.start) == start);
        let bl_active = bl.filter(|run| bpos.max(run.start) == start);
        let mut end = cur_active.map_or(next_current, |run| run.end);
        end = end.min(bl_active.map_or(next_baseline, |run| run.end));
        end = end.min(next_zoom_boundary(start));

        spans.push(PairSpan {
            start,
            end,
            current: cur_active.map(|run| run.blob),
            baseline: bl_active.map(|run| run.blob),
        });

        if let Some(run) = cur_active {
            cpos = end;
            if cpos == run.end {
                ci += 1;
                cpos = 0;
            }
        }
        if let Some(run) = bl_active {
            bpos = end;
            if bpos == run.end {
                bi += 1;
                bpos = 0;
            }
        }
    }
    spans
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BlobPair {
    current: BlobRef,
    baseline: BlobRef,
}

#[derive(Clone, Debug)]
struct PairWork {
    pair: BlobPair,
    spans: Vec<PairSpan>,
}

impl PairWork {
    fn tiles(&self) -> u64 {
        self.spans.iter().map(|span| span.tiles()).sum()
    }
}

struct PairState {
    work: PairWork,
    raw_equal: bool,
    canonical_equal: bool,
    detail: Option<DetailOutcome>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Diff two archives. `strictness` is the decoder's unknown-field policy:
/// strict between two elivagar builds, tolerant against a foreign producer.
pub fn regress(
    current: &Path,
    baseline: &Path,
    cfg: &RegressConfig,
    strictness: Strictness,
) -> io::Result<RegressReport> {
    let current = ArchiveView::open(current)?;
    let baseline = ArchiveView::open(baseline)?;
    let current_runs = archive_runs(&current)?;
    let baseline_runs = archive_runs(&baseline)?;
    let spans = merge_runs(&current_runs, &baseline_runs);

    let mut report = RegressReport::default();
    report.counters.addressed_current = current.num_addressed();
    report.counters.addressed_baseline = baseline.num_addressed();
    report.counters.addressed_tiles = spans.iter().map(|span| span.tiles()).sum();
    report.counters.directory_runs =
        u64::try_from(current_runs.len() + baseline_runs.len()).unwrap_or(u64::MAX);
    report.counters.unique_blobs =
        unique_blob_count(&current_runs) + unique_blob_count(&baseline_runs);

    let (mut states, missing) = group_pair_spans(spans);
    report.counters.unique_blob_pairs = u64::try_from(states.len()).unwrap_or(u64::MAX);

    // Pass 1: raw bytes.
    let raw_start = Instant::now();
    states
        .par_iter_mut()
        .try_for_each(|state| -> io::Result<()> {
            let cur = current.raw_blob(state.work.pair.current)?;
            let bl = baseline.raw_blob(state.work.pair.baseline)?;
            state.raw_equal = raw_equal(&cur, &bl);
            Ok(())
        })?;
    report.counters.raw_pass_ms = crate::support::duration_ms(raw_start.elapsed());

    // Pass 2: the canonical semantic hash, memoized per blob rather than per
    // pair - the same blob typically appears in many surviving pairs.
    let canonical_start = Instant::now();
    let current_fingerprints =
        fingerprint_blobs(&current, unique_work_blobs(&states, |pair| pair.current))?;
    let baseline_fingerprints =
        fingerprint_blobs(&baseline, unique_work_blobs(&states, |pair| pair.baseline))?;
    states
        .par_iter_mut()
        .filter(|state| !state.raw_equal)
        .try_for_each(|state| -> io::Result<()> {
            let current = current_fingerprints
                .get(&state.work.pair.current)
                .copied()
                .ok_or_else(|| io::Error::other("current fingerprint is missing"))?;
            let baseline = baseline_fingerprints
                .get(&state.work.pair.baseline)
                .copied()
                .ok_or_else(|| io::Error::other("baseline fingerprint is missing"))?;
            state.canonical_equal = current == baseline;
            Ok(())
        })?;
    report.counters.canonical_pass_ms = crate::support::duration_ms(canonical_start.elapsed());

    // Pass 3: structural decode and classification.
    let detail_start = Instant::now();
    states
        .par_iter_mut()
        .filter(|state| !state.raw_equal && !state.canonical_equal)
        .try_for_each_init(DecodeScratch::default, |scratch, state| -> io::Result<()> {
            let cur_bytes = scratch.decompress(&current.raw_blob(state.work.pair.current)?)?;
            let cur = decode_prepared_tile(cur_bytes, strictness).map_err(invalid_tile)?;
            let bl_bytes = scratch.decompress(&baseline.raw_blob(state.work.pair.baseline)?)?;
            let bl = decode_prepared_tile(bl_bytes, strictness).map_err(invalid_tile)?;
            state.detail = Some(compare_detail_tiles::<DetailOutcome>(&cur, &bl, cfg));
            Ok(())
        })?;
    report.counters.detail_pass_ms = crate::support::duration_ms(detail_start.elapsed());

    let mut interner = LayerInterner::default();
    let mut examples = ExampleSelector::new(cfg.max_examples);
    for span in missing {
        apply_missing_span(&mut report, span);
    }
    for state in &states {
        let tiles = state.work.tiles();
        if state.raw_equal {
            report.counters.raw_equal_pairs += 1;
            report.counters.raw_equal_tiles += tiles;
            report.identical_tiles += tiles;
        } else if state.canonical_equal {
            report.counters.canonical_equal_pairs += 1;
            report.counters.canonical_equal_tiles += tiles;
            report.identical_tiles += tiles;
        } else {
            report.counters.detailed_pairs += 1;
            report.counters.detailed_tiles += tiles;
            let detail = state.detail.as_ref().ok_or_else(|| {
                io::Error::other("detail pass did not produce a differing work outcome")
            })?;
            for &span in &state.work.spans {
                apply_detail_span(&mut report, &mut interner, &mut examples, span, detail);
            }
        }
    }
    coalesce_differing_ranges(&mut report);
    report.examples = examples.finish();
    report.counters.peak_rss_kb = crate::support::self_peak_rss_kb().unwrap_or(0);
    Ok(report)
}

// ---------------------------------------------------------------------------
// Work grouping
// ---------------------------------------------------------------------------

fn group_pair_spans(spans: Vec<PairSpan>) -> (Vec<PairState>, Vec<PairSpan>) {
    let mut indexes: FxHashMap<BlobPair, usize> = FxHashMap::default();
    let mut states: Vec<PairState> = Vec::new();
    let mut missing = Vec::new();
    for span in spans {
        let (Some(current), Some(baseline)) = (span.current, span.baseline) else {
            missing.push(span);
            continue;
        };
        let pair = BlobPair { current, baseline };
        if let Some(&idx) = indexes.get(&pair) {
            states[idx].work.spans.push(span);
        } else {
            indexes.insert(pair, states.len());
            states.push(PairState {
                work: PairWork {
                    pair,
                    spans: vec![span],
                },
                raw_equal: false,
                canonical_equal: false,
                detail: None,
            });
        }
    }
    (states, missing)
}

fn unique_blob_count(runs: &[TileRun]) -> u64 {
    let seen: FxHashSet<BlobRef> = runs.iter().map(|run| run.blob).collect();
    u64::try_from(seen.len()).unwrap_or(u64::MAX)
}

fn unique_work_blobs(states: &[PairState], select: impl Fn(BlobPair) -> BlobRef) -> Vec<BlobRef> {
    let mut seen: FxHashSet<BlobRef> = FxHashSet::default();
    let mut blobs = Vec::new();
    for state in states.iter().filter(|state| !state.raw_equal) {
        let blob = select(state.work.pair);
        if seen.insert(blob) {
            blobs.push(blob);
        }
    }
    blobs
}

fn fingerprint_blobs(
    archive: &ArchiveView,
    blobs: Vec<BlobRef>,
) -> io::Result<FxHashMap<BlobRef, u128>> {
    let mut hashes = vec![0u128; blobs.len()];
    blobs
        .par_iter()
        .zip(hashes.par_iter_mut())
        .try_for_each_init(
            DecodeScratch::default,
            |scratch, (blob, hash)| -> io::Result<()> {
                // Keep semantic hashing identical to the corpus walk; rayon
                // changes scheduling only, not the hash implementation.
                *hash = semantic_hash(&archive.raw_blob(*blob)?, scratch)?;
                Ok(())
            },
        )?;
    Ok(blobs.into_iter().zip(hashes).collect())
}

// Slice equality is the whole tier: length check plus early-exiting memcmp
// over the two mmap slices. A digest prefilter would only pay off if digests
// were computed once per blob and reused across pairs; per pair it is strictly
// extra passes over the same bytes.
fn raw_equal(current: &[u8], baseline: &[u8]) -> bool {
    current == baseline
}

// ---------------------------------------------------------------------------
// Report aggregation
// ---------------------------------------------------------------------------

fn apply_missing_span(report: &mut RegressReport, span: PairSpan) {
    let count = span.tiles();
    match (span.current, span.baseline) {
        (Some(_), None) => report.totals.only_in_current += count,
        (None, Some(_)) => report.totals.only_in_baseline += count,
        (Some(_), Some(_)) | (None, None) => return,
    }
    report.diff_count += count;
    push_differing_range(report, span.start, span.end);
}

fn apply_detail_span(
    report: &mut RegressReport,
    interner: &mut LayerInterner,
    examples: &mut ExampleSelector,
    span: PairSpan,
    detail: &DetailOutcome,
) {
    let count = span.tiles();
    if !detail.counts.any() {
        report.identical_tiles += count;
        return;
    }
    report.diff_count += count;
    push_differing_range(report, span.start, span.end);
    let (z, _, _) = tile_id_to_zxy(span.start);
    // Every tile of the span is a legitimate example; the selector only ever
    // keeps `cap` per class, so offering more than `cap` from one span is
    // pointless. This bound keeps the per-tile example semantics of the old
    // per-tile engine bit-exact (the selector picks the lowest tile ids).
    let example_end = span.end.min(
        span.start
            .saturating_add(u64::try_from(examples.cap).unwrap_or(u64::MAX)),
    );
    for event in &detail.events {
        let layer = interner.intern(&event.layer);
        let counters = report
            .per_zoom_layer
            .entry((z, Arc::clone(&layer)))
            .or_default();
        add_event_counters(&mut report.totals, counters, event.class, count);
        if matches!(
            event.class,
            OutcomeClass::ToleranceMoved | OutcomeClass::StructuralMoved
        ) {
            report
                .displacement
                .entry((z, Arc::clone(&layer)))
                .or_default()
                .add(event.displacement, count);
        }
        for tile_id in span.start..example_end {
            examples.offer(
                event.class,
                ExampleKey {
                    tile_id,
                    layer: Arc::clone(&layer),
                    id: event.id,
                },
            );
        }
    }
}

fn add_event_counters(
    totals: &mut DiffTotals,
    layer: &mut LayerCounters,
    class: OutcomeClass,
    count: u64,
) {
    totals.outcomes.add(class, count);
    layer.outcomes.add(class, count);
}

fn push_differing_range(report: &mut RegressReport, start: u64, end: u64) {
    if let Some(last) = report.differing_ranges.last_mut()
        && last.end == start
    {
        last.end = end;
    } else {
        report.differing_ranges.push(TileRange { start, end });
    }
}

/// Spans are applied in blob-pair discovery order, not tile order, so the
/// opportunistic coalescing in `push_differing_range` misses most joins and
/// leaves the ranges unordered. Sort and re-coalesce once at the end; spans
/// never overlap (each addressed tile belongs to exactly one span).
fn coalesce_differing_ranges(report: &mut RegressReport) {
    let mut ranges = std::mem::take(&mut report.differing_ranges);
    ranges.sort_by_key(|range| range.start);
    let mut out: Vec<TileRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            Some(last) if last.end == range.start => last.end = range.end,
            _ => out.push(range),
        }
    }
    report.differing_ranges = out;
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

pub(crate) fn invalid_tile(error: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
