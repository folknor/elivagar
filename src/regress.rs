use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use flate2::read::GzDecoder;
use memmap2::{Mmap, MmapOptions};
use protohoggr::{Cursor, WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::json;
use xxhash_rust::xxh3::Xxh3;

use crate::pmtiles_reader::{HEADER_SIZE, RawDirEntry, decode_directory, read_u64_le};
use crate::pmtiles_writer::tile_id_to_zxy;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonRingRole {
    Point = 0,
    Path = 1,
    Outer = 2,
    Hole = 3,
}

#[derive(Clone, Debug)]
pub struct RegressConfig {
    pub tol: i32,
    pub max_moved: u64,
    pub max_examples: usize,
}

impl Default for RegressConfig {
    fn default() -> Self {
        Self {
            tol: 0,
            max_moved: 0,
            max_examples: 20,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffTotals {
    pub only_in_current: u64,
    pub only_in_blessed: u64,
    pub layers_added: u64,
    pub layers_removed: u64,
    pub extent_mismatch: u64,
    pub missing_features: u64,
    pub added_features: u64,
    pub attr_changed: u64,
    pub tolerance_moved: u64,
    pub structural_moved: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerCounters {
    pub layers_added: u64,
    pub layers_removed: u64,
    pub extent_mismatch: u64,
    pub missing_features: u64,
    pub added_features: u64,
    pub attr_changed: u64,
    pub tolerance_moved: u64,
    pub structural_moved: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplacementHistogram {
    pub values: BTreeMap<i32, u64>,
}

impl DisplacementHistogram {
    fn add(&mut self, value: i32, count: u64) {
        *self.values.entry(value).or_default() += count;
    }

    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn percentile(&self, pct: usize) -> i32 {
        let total: u64 = self.values.values().copied().sum();
        if total == 0 {
            return 0;
        }
        let target = ((total - 1) * u64::try_from(pct).unwrap_or(0)) / 100;
        let mut seen = 0u64;
        for (&value, &count) in &self.values {
            seen += count;
            if seen > target {
                return value;
            }
        }
        self.values.last_key_value().map_or(0, |(&value, _)| value)
    }

    fn max(&self) -> i32 {
        self.values.last_key_value().map_or(0, |(&value, _)| value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffExample {
    pub tile_id: u64,
    pub layer: Arc<str>,
    pub id: Option<u64>,
    pub class: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegressCounters {
    pub addressed_tiles: u64,
    pub addressed_current: u64,
    pub addressed_blessed: u64,
    pub directory_runs: u64,
    pub unique_blobs: u64,
    pub unique_blob_pairs: u64,
    pub raw_equal_pairs: u64,
    pub raw_equal_tiles: u64,
    pub canonical_equal_pairs: u64,
    pub canonical_equal_tiles: u64,
    pub detailed_pairs: u64,
    pub detailed_tiles: u64,
    pub raw_pass_ms: u64,
    pub canonical_pass_ms: u64,
    pub detail_pass_ms: u64,
    pub peak_rss_kb: u64,
}

#[derive(Clone, Debug, Default)]
pub struct RegressReport {
    pub identical_tiles: u64,
    /// Full differing-tile count. This replaces the old unbounded TileDiff Vec.
    pub diff_count: u64,
    /// Coalesced ranges retain enough tile-location context without a record per tile.
    pub differing_ranges: Vec<TileRange>,
    pub totals: DiffTotals,
    pub per_zoom_layer: BTreeMap<(u8, Arc<str>), LayerCounters>,
    pub displacement: BTreeMap<(u8, Arc<str>), DisplacementHistogram>,
    pub examples: Vec<DiffExample>,
    pub counters: RegressCounters,
}

impl RegressReport {
    pub fn passed(&self, cfg: &RegressConfig) -> bool {
        self.totals.only_in_current == 0
            && self.totals.only_in_blessed == 0
            && self.totals.layers_added == 0
            && self.totals.layers_removed == 0
            && self.totals.extent_mismatch == 0
            && self.totals.missing_features == 0
            && self.totals.added_features == 0
            && self.totals.attr_changed == 0
            && self.totals.structural_moved == 0
            && self.totals.tolerance_moved <= cfg.max_moved
    }

    pub fn print_text(&self) {
        println!(
            "identical_tiles={} diffs={} only_current={} only_blessed={} tolerance_moved={} structural_moved={} attr_changed={}",
            self.identical_tiles,
            self.diff_count,
            self.totals.only_in_current,
            self.totals.only_in_blessed,
            self.totals.tolerance_moved,
            self.totals.structural_moved,
            self.totals.attr_changed
        );
        println!(
            "regress addressed_tiles={} directory_runs={} unique_blobs={} unique_blob_pairs={} raw_equal_pairs={} raw_equal_tiles={} canonical_equal_pairs={} canonical_equal_tiles={} detailed_pairs={} detailed_tiles={} raw_ms={} canonical_ms={} detail_ms={} peak_rss_kb={}",
            self.counters.addressed_tiles,
            self.counters.directory_runs,
            self.counters.unique_blobs,
            self.counters.unique_blob_pairs,
            self.counters.raw_equal_pairs,
            self.counters.raw_equal_tiles,
            self.counters.canonical_equal_pairs,
            self.counters.canonical_equal_tiles,
            self.counters.detailed_pairs,
            self.counters.detailed_tiles,
            self.counters.raw_pass_ms,
            self.counters.canonical_pass_ms,
            self.counters.detail_pass_ms,
            self.counters.peak_rss_kb,
        );

        for ((z, layer), c) in &self.per_zoom_layer {
            if counters_are_zero(c) {
                continue;
            }
            println!(
                "z{z} {layer} added_layers={} removed_layers={} extent={} missing={} added={} attrs={} tol={} structural={}",
                c.layers_added,
                c.layers_removed,
                c.extent_mismatch,
                c.missing_features,
                c.added_features,
                c.attr_changed,
                c.tolerance_moved,
                c.structural_moved
            );
        }

        for ((z, layer), values) in &self.displacement {
            if values.is_empty() {
                continue;
            }
            println!(
                "z{z} {layer} displacement p50={} p95={} max={}",
                values.percentile(50),
                values.percentile(95),
                values.max()
            );
        }

        for ex in &self.examples {
            let (z, x, y) = tile_id_to_zxy(ex.tile_id);
            let id = ex
                .id
                .map_or_else(|| "anon".to_string(), |id| id.to_string());
            println!("z{z}/{x}/{y} {} {} {}", ex.layer.as_ref(), id, ex.class);
        }
    }

    pub fn to_json(&self, passed: bool) -> serde_json::Value {
        let per_zoom_layer: Vec<_> = self
            .per_zoom_layer
            .iter()
            .map(|((z, layer), c)| {
                json!({
                    "z": z,
                    "layer": layer.as_ref(),
                    "layers_added": c.layers_added,
                    "layers_removed": c.layers_removed,
                    "extent_mismatch": c.extent_mismatch,
                    "missing_features": c.missing_features,
                    "added_features": c.added_features,
                    "attr_changed": c.attr_changed,
                    "tolerance_moved": c.tolerance_moved,
                    "structural_moved": c.structural_moved,
                })
            })
            .collect();
        let displacement: Vec<_> = self
            .displacement
            .iter()
            .filter(|(_, values)| !values.is_empty())
            .map(|((z, layer), values)| {
                json!({
                    "z": z,
                    "layer": layer.as_ref(),
                    "p50": values.percentile(50),
                    "p95": values.percentile(95),
                    "max": values.max(),
                })
            })
            .collect();
        let examples: Vec<_> = self
            .examples
            .iter()
            .map(|ex| {
                let (z, x, y) = tile_id_to_zxy(ex.tile_id);
                json!({
                    "tile_id": ex.tile_id,
                    "z": z,
                    "x": x,
                    "y": y,
                    "layer": ex.layer.as_ref(),
                    "id": ex.id,
                    "class": ex.class,
                })
            })
            .collect();

        json!({
            "passed": passed,
            "identical_tiles": self.identical_tiles,
            "diffs": self.diff_count,
            "totals": {
                "only_in_current": self.totals.only_in_current,
                "only_in_blessed": self.totals.only_in_blessed,
                "layers_added": self.totals.layers_added,
                "layers_removed": self.totals.layers_removed,
                "extent_mismatch": self.totals.extent_mismatch,
                "missing_features": self.totals.missing_features,
                "added_features": self.totals.added_features,
                "attr_changed": self.totals.attr_changed,
                "tolerance_moved": self.totals.tolerance_moved,
                "structural_moved": self.totals.structural_moved,
            },
            "per_zoom_layer": per_zoom_layer,
            "displacement": displacement,
            "examples": examples,
            "counters": {
                "addressed_tiles": self.counters.addressed_tiles,
                "addressed_current": self.counters.addressed_current,
                "addressed_blessed": self.counters.addressed_blessed,
                "directory_runs": self.counters.directory_runs,
                "unique_blobs": self.counters.unique_blobs,
                "unique_blob_pairs": self.counters.unique_blob_pairs,
                "raw_equal_pairs": self.counters.raw_equal_pairs,
                "raw_equal_tiles": self.counters.raw_equal_tiles,
                "canonical_equal_pairs": self.counters.canonical_equal_pairs,
                "canonical_equal_tiles": self.counters.canonical_equal_tiles,
                "detailed_pairs": self.counters.detailed_pairs,
                "detailed_tiles": self.counters.detailed_tiles,
                "raw_pass_ms": self.counters.raw_pass_ms,
                "canonical_pass_ms": self.counters.canonical_pass_ms,
                "detail_pass_ms": self.counters.detail_pass_ms,
                "peak_rss_kb": self.counters.peak_rss_kb,
            },
        })
    }

    pub fn dump_svg_examples(
        &self,
        current: &Path,
        blessed: &Path,
        dir: &Path,
        max_examples: usize,
    ) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        for (idx, ex) in self
            .examples
            .iter()
            .filter(|ex| ex.class.contains("structural"))
            .take(max_examples)
            .enumerate()
        {
            let (z, x, y) = tile_id_to_zxy(ex.tile_id);
            let cur_path = dir.join(format!("{idx:03}-z{z}-x{x}-y{y}-current.svg"));
            let blessed_path = dir.join(format!("{idx:03}-z{z}-x{x}-y{y}-blessed.svg"));
            let mut cur_file = fs::File::create(cur_path)?;
            let mut blessed_file = fs::File::create(blessed_path)?;
            crate::svg::render_tile_svg(current, z, x, y, &mut cur_file)?;
            crate::svg::render_tile_svg(blessed, z, x, y, &mut blessed_file)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Immutable PMTiles view and run-span planning
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BlobRef {
    offset: u64,
    length: u32,
}

#[derive(Clone, Debug)]
struct TileRun {
    start: u64,
    end: u64,
    blob: BlobRef,
}

#[derive(Clone, Copy, Debug)]
struct PairSpan {
    start: u64,
    end: u64,
    current: Option<BlobRef>,
    blessed: Option<BlobRef>,
}

impl PairSpan {
    fn tiles(self) -> u64 {
        self.end - self.start
    }
}

struct ArchiveView {
    map: Mmap,
    root_dir_offset: u64,
    root_dir_length: u64,
    leaf_dirs_offset: u64,
    data_offset: u64,
    internal_compression: u8,
    num_addressed: u64,
}

impl ArchiveView {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the mapping is read-only and owned by this ArchiveView. The
        // mapped bytes are never exposed mutably, and every later offset is
        // range-checked before slicing.
        let map = unsafe { MmapOptions::new().map(&file)? };
        if map.len() < HEADER_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{} is shorter than a PMTiles header", path.display()),
            ));
        }
        let header = &map[..HEADER_SIZE];
        if &header[..7] != b"PMTiles" || header[7] != 3 {
            return Err(io::Error::other("not PMTiles v3"));
        }
        if header[99] != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not an MVT PMTiles archive", path.display()),
            ));
        }
        if header[98] != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} uses non-gzip tile compression", path.display()),
            ));
        }
        // Internal compression: 1 = none, 2 = gzip; directory() handles both.
        if header[97] != 1 && header[97] != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} uses unsupported internal compression", path.display()),
            ));
        }

        let root_dir_offset = read_u64_le(header, 8);
        let root_dir_length = read_u64_le(header, 16);
        let leaf_dirs_offset = read_u64_le(header, 40);
        let data_offset = read_u64_le(header, 56);
        let internal_compression = header[97];
        let num_addressed = read_u64_le(header, 72);
        Ok(Self {
            map,
            root_dir_offset,
            root_dir_length,
            leaf_dirs_offset,
            data_offset,
            internal_compression,
            num_addressed,
        })
    }

    fn slice(&self, offset: u64, length: u64, what: &str) -> io::Result<&[u8]> {
        let end = offset.checked_add(length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{what} range overflow"))
        })?;
        let start = usize::try_from(offset).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{what} offset too large"),
            )
        })?;
        let end = usize::try_from(end).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{what} end too large"))
        })?;
        self.map.get(start..end).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{what} lies outside archive"),
            )
        })
    }

    fn directory(&self, offset: u64, length: u64) -> io::Result<Vec<RawDirEntry>> {
        let compressed = self.slice(offset, length, "directory")?;
        let raw = if self.internal_compression == 2 {
            let mut scratch = Vec::new();
            gzip_decompress_into(compressed, &mut scratch)?;
            scratch
        } else {
            compressed.to_vec()
        };
        decode_directory(&raw)
    }

    fn runs(&self) -> io::Result<Vec<TileRun>> {
        let root = self.directory(self.root_dir_offset, self.root_dir_length)?;
        let mut runs = Vec::new();
        for entry in root {
            if entry.run_length == 0 {
                let offset = self
                    .leaf_dirs_offset
                    .checked_add(entry.offset)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "leaf directory offset overflow")
                    })?;
                let leaf = self.directory(offset, u64::from(entry.length))?;
                append_tile_runs(&mut runs, &leaf)?;
            } else {
                append_tile_runs(&mut runs, std::slice::from_ref(&entry))?;
            }
        }
        Ok(runs)
    }

    fn raw_blob(&self, blob: BlobRef) -> io::Result<&[u8]> {
        let offset = self.data_offset.checked_add(blob.offset).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "tile data offset overflow")
        })?;
        self.slice(offset, u64::from(blob.length), "tile payload")
    }
}

fn append_tile_runs(out: &mut Vec<TileRun>, entries: &[RawDirEntry]) -> io::Result<()> {
    for entry in entries {
        if entry.run_length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "leaf directory contains a directory pointer",
            ));
        }
        let end = entry
            .tile_id
            .checked_add(u64::from(entry.run_length))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tile run overflows"))?;
        if out
            .last()
            .is_some_and(|previous| previous.end > entry.tile_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "PMTiles directory runs overlap or are out of order",
            ));
        }
        out.push(TileRun {
            start: entry.tile_id,
            end,
            blob: BlobRef {
                offset: entry.offset,
                length: entry.length,
            },
        });
    }
    Ok(())
}

fn merge_runs(current: &[TileRun], blessed: &[TileRun]) -> Vec<PairSpan> {
    let mut spans = Vec::new();
    let (mut ci, mut bi) = (0usize, 0usize);
    let (mut cpos, mut bpos) = (0u64, 0u64);

    while ci < current.len() || bi < blessed.len() {
        let cur = current.get(ci);
        let bl = blessed.get(bi);
        let next_current = cur.map_or(u64::MAX, |run| run.start.max(cpos));
        let next_blessed = bl.map_or(u64::MAX, |run| run.start.max(bpos));
        let start = next_current.min(next_blessed);
        let cur_active = cur.filter(|run| cpos.max(run.start) == start);
        let bl_active = bl.filter(|run| bpos.max(run.start) == start);
        let mut end = cur_active.map_or(next_current, |run| run.end);
        end = end.min(bl_active.map_or(next_blessed, |run| run.end));
        end = end.min(next_zoom_boundary(start));

        spans.push(PairSpan {
            start,
            end,
            current: cur_active.map(|run| run.blob),
            blessed: bl_active.map(|run| run.blob),
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

fn next_zoom_boundary(tile_id: u64) -> u64 {
    let (z, _, _) = tile_id_to_zxy(tile_id);
    if z >= 30 {
        return u64::MAX;
    }
    let n = 1u64 << z;
    (n * n * 4 - 1) / 3
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BlobPair {
    current: BlobRef,
    blessed: BlobRef,
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
// Three-pass parallel engine
// ---------------------------------------------------------------------------

pub fn regress(current: &Path, blessed: &Path, cfg: &RegressConfig) -> io::Result<RegressReport> {
    let current = ArchiveView::open(current)?;
    let blessed = ArchiveView::open(blessed)?;
    let current_runs = current.runs()?;
    let blessed_runs = blessed.runs()?;
    let spans = merge_runs(&current_runs, &blessed_runs);

    let mut report = RegressReport::default();
    report.counters.addressed_current = current.num_addressed;
    report.counters.addressed_blessed = blessed.num_addressed;
    report.counters.addressed_tiles = spans.iter().map(|span| span.tiles()).sum();
    report.counters.directory_runs =
        u64::try_from(current_runs.len() + blessed_runs.len()).unwrap_or(u64::MAX);
    report.counters.unique_blobs =
        unique_blob_count(&current_runs) + unique_blob_count(&blessed_runs);

    let (mut states, missing) = group_pair_spans(spans);
    report.counters.unique_blob_pairs = u64::try_from(states.len()).unwrap_or(u64::MAX);

    let raw_start = Instant::now();
    states
        .par_iter_mut()
        .try_for_each(|state| -> io::Result<()> {
            let cur = current.raw_blob(state.work.pair.current)?;
            let bl = blessed.raw_blob(state.work.pair.blessed)?;
            state.raw_equal = raw_equal(cur, bl);
            Ok(())
        })?;
    report.counters.raw_pass_ms = elapsed_ms(raw_start);

    let canonical_start = Instant::now();
    let current_fingerprints =
        fingerprint_blobs(&current, unique_work_blobs(&states, |pair| pair.current))?;
    let blessed_fingerprints =
        fingerprint_blobs(&blessed, unique_work_blobs(&states, |pair| pair.blessed))?;
    states
        .par_iter_mut()
        .filter(|state| !state.raw_equal)
        .try_for_each(|state| -> io::Result<()> {
            let current = current_fingerprints
                .get(&state.work.pair.current)
                .copied()
                .ok_or_else(|| io::Error::other("current fingerprint is missing"))?;
            let blessed = blessed_fingerprints
                .get(&state.work.pair.blessed)
                .copied()
                .ok_or_else(|| io::Error::other("blessed fingerprint is missing"))?;
            state.canonical_equal = current == blessed;
            Ok(())
        })?;
    report.counters.canonical_pass_ms = elapsed_ms(canonical_start);

    let detail_start = Instant::now();
    states
        .par_iter_mut()
        .filter(|state| !state.raw_equal && !state.canonical_equal)
        .try_for_each_init(DecodeScratch::default, |scratch, state| -> io::Result<()> {
            let cur_bytes = scratch.decompress(current.raw_blob(state.work.pair.current)?)?;
            let cur = decode_detail_tile(cur_bytes).map_err(invalid_tile)?;
            let bl_bytes = scratch.decompress(blessed.raw_blob(state.work.pair.blessed)?)?;
            let bl = decode_detail_tile(bl_bytes).map_err(invalid_tile)?;
            state.detail = Some(compare_detail_tiles(&cur, &bl, cfg));
            Ok(())
        })?;
    report.counters.detail_pass_ms = elapsed_ms(detail_start);

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
            let detail = state
                .detail
                .as_ref()
                .expect("detail pass sets differing work outcome");
            for &span in &state.work.spans {
                apply_detail_span(&mut report, &mut interner, &mut examples, span, detail);
            }
        }
    }
    coalesce_differing_ranges(&mut report);
    report.examples = examples.finish();
    report.counters.peak_rss_kb = peak_rss_kb().unwrap_or(0);
    emit_regress_counters(&report.counters);
    Ok(report)
}

fn group_pair_spans(spans: Vec<PairSpan>) -> (Vec<PairState>, Vec<PairSpan>) {
    let mut indexes: FxHashMap<BlobPair, usize> = FxHashMap::default();
    let mut states: Vec<PairState> = Vec::new();
    let mut missing = Vec::new();
    for span in spans {
        let (Some(current), Some(blessed)) = (span.current, span.blessed) else {
            missing.push(span);
            continue;
        };
        let pair = BlobPair { current, blessed };
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
                *hash = semantic_hash(archive.raw_blob(*blob)?, scratch)?;
                Ok(())
            },
        )?;
    Ok(blobs.into_iter().zip(hashes).collect())
}

// Slice equality is the whole tier: length check plus early-exiting memcmp
// over the two mmap slices. A digest prefilter would only pay off if digests
// were computed once per blob and reused across pairs; per pair it is strictly
// extra passes over the same bytes.
fn raw_equal(current: &[u8], blessed: &[u8]) -> bool {
    current == blessed
}

#[derive(Default)]
struct DecodeScratch {
    gzip: Vec<u8>,
}

impl DecodeScratch {
    fn decompress<'a>(&'a mut self, data: &[u8]) -> io::Result<&'a [u8]> {
        gzip_decompress_into(data, &mut self.gzip)?;
        Ok(&self.gzip)
    }
}

fn gzip_decompress_into(data: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
    out.clear();
    if data.len() >= 4 {
        let tail = &data[data.len() - 4..];
        let size = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
        #[allow(clippy::cast_possible_truncation)]
        let size = size as usize;
        // len is 0 after clear(), so try_reserve(size) guarantees
        // capacity >= size (reserving size - capacity would under-reserve).
        if size > out.capacity() {
            out.try_reserve(size)
                .map_err(|_| io::Error::other("gzip output allocation failed"))?;
        }
    }
    let mut decoder = GzDecoder::new(data);
    decoder.read_to_end(out)?;
    Ok(())
}

fn semantic_hash(raw: &[u8], scratch: &mut DecodeScratch) -> io::Result<u128> {
    let data = scratch.decompress(raw)?;
    streaming_tile_hash(data).map_err(invalid_tile)
}

/// Hash the comparison canonical form without retaining its geometry.  The
/// detailed decoder below remains the authority for non-identical tiles; this
/// only decides whether it needs to run.  Unordered collections are reduced
/// by hashing their sorted child digests, while rings retain their encoded
/// point order and rotation.
fn streaming_tile_hash(data: &[u8]) -> Result<u128, String> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read tile tag: {error}"))?
    {
        if field == 3 && wire_type == WIRE_LEN {
            layers.push(streaming_layer_hash(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read tile layer: {error}"))?,
            )?);
        } else {
            cursor
                .skip_field(wire_type)
                .map_err(|error| format!("skip tile field {field}: {error}"))?;
        }
    }
    // Mirror decode_detail_tile's stable sort by name: duplicate layer names
    // (invalid MVT, but accepted by the detail decoder) keep their encounter
    // order bound, so the two hashes induce the same equivalence relation.
    layers.sort_by(|a, b| a.0.cmp(b.0));
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-tile-v1");
    write_len(&mut sink, layers.len());
    for (_, hash) in &layers {
        sink.bytes(&hash.to_le_bytes());
    }
    Ok(sink.finish())
}

fn streaming_layer_hash(data: &[u8]) -> Result<(&str, u128), String> {
    let mut name = "";
    let mut extent = 4096u32;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut features = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read layer tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                name = std::str::from_utf8(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read layer name: {error}"))?,
                )
                .map_err(|error| format!("layer name is not UTF-8: {error}"))?;
            }
            (2, WIRE_LEN) => features.push(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature message: {error}"))?,
            ),
            (3, WIRE_LEN) => keys.push(Arc::from(
                std::str::from_utf8(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read layer key: {error}"))?,
                )
                .map_err(|error| format!("layer key is not UTF-8: {error}"))?,
            )),
            (4, WIRE_LEN) => values.push(decode_detail_attr(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer value: {error}"))?,
            )?),
            (5, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read layer extent: {error}"))?;
                extent = u32::try_from(raw).map_err(|_| format!("extent out of range: {raw}"))?;
            }
            _ => cursor
                .skip_field(wire_type)
                .map_err(|error| format!("skip layer field {field}: {error}"))?,
        }
    }
    let mut feature_hashes = Vec::with_capacity(features.len());
    for feature in features {
        feature_hashes.push(streaming_feature_hash(feature, &keys, &values)?);
    }
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-layer-v1");
    write_string(&mut sink, name);
    sink.bytes(&extent.to_le_bytes());
    sink.bytes(&multiset_hash(b"features", &mut feature_hashes).to_le_bytes());
    Ok((name, sink.finish()))
}

fn streaming_feature_hash(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
) -> Result<u128, String> {
    let mut id = None;
    let mut attrs = Vec::new();
    let mut geom_type = 0u8;
    let mut geometry = None;
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read feature tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_VARINT) => {
                id = Some(
                    cursor
                        .read_varint()
                        .map_err(|error| format!("read feature id: {error}"))?,
                );
            }
            (2, WIRE_LEN) => {
                attrs = decode_detail_attrs(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read feature tags: {error}"))?,
                    keys,
                    values,
                )?;
            }
            (3, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read feature type: {error}"))?;
                geom_type =
                    u8::try_from(raw).map_err(|_| format!("geometry type out of range: {raw}"))?;
            }
            (4, WIRE_LEN) => {
                geometry = Some(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read feature geometry: {error}"))?,
                );
            }
            _ => cursor
                .skip_field(wire_type)
                .map_err(|error| format!("skip feature field {field}: {error}"))?,
        }
    }
    attrs.sort();
    let attrs_digest = detail_attrs_hash(&attrs);
    let geometry_digest = streaming_geometry_hash(geom_type, geometry.unwrap_or_default())?;
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-feature-v1");
    match id {
        Some(id) => {
            sink.bytes(&[1]);
            sink.bytes(&id.to_le_bytes());
        }
        None => sink.bytes(&[0]),
    }
    sink.bytes(&[geom_type]);
    sink.bytes(&attrs_digest.to_le_bytes());
    sink.bytes(&geometry_digest.to_le_bytes());
    Ok(sink.finish())
}

// Order-insensitive combine: hash the sorted child digests as a sequence.
// Sorting instead of a wrapping-add sum avoids the additive collision
// structure of sum-based multiset hashing - equal multisets, and only equal
// multisets, produce the same sorted sequence.
fn multiset_hash(domain: &[u8], hashes: &mut [u128]) -> u128 {
    hashes.sort_unstable();
    let mut sink = HashSink::new();
    sink.bytes(domain);
    write_len(&mut sink, hashes.len());
    for hash in hashes.iter() {
        sink.bytes(&hash.to_le_bytes());
    }
    sink.finish()
}

struct StreamingRing {
    points: HashSink,
    first: Option<(i32, i32)>,
    previous: Option<(i32, i32)>,
    vertices: usize,
    area: i128,
}

impl StreamingRing {
    fn new() -> Self {
        Self {
            points: HashSink::new(),
            first: None,
            previous: None,
            vertices: 0,
            area: 0,
        }
    }

    fn point(&mut self, point: (i32, i32)) {
        self.points.bytes(&point.0.to_le_bytes());
        self.points.bytes(&point.1.to_le_bytes());
        if let Some(previous) = self.previous {
            self.area += i128::from(previous.0) * i128::from(point.1)
                - i128::from(point.0) * i128::from(previous.1);
        } else {
            self.first = Some(point);
        }
        self.previous = Some(point);
        self.vertices += 1;
    }

    fn finish(self, role: CanonRingRole) -> u128 {
        let mut sink = HashSink::new();
        sink.bytes(b"elivagar-stream-ring-v1");
        sink.bytes(&[role as u8]);
        write_len(&mut sink, self.vertices);
        sink.bytes(&self.points.finish().to_le_bytes());
        sink.finish()
    }
}

struct StreamingComponent {
    rings: Vec<u128>,
}

impl StreamingComponent {
    fn new() -> Self {
        Self { rings: Vec::new() }
    }
    fn finish(self) -> u128 {
        let mut sink = HashSink::new();
        sink.bytes(b"elivagar-stream-component-v1");
        write_len(&mut sink, self.rings.len());
        for ring in self.rings {
            sink.bytes(&ring.to_le_bytes());
        }
        sink.finish()
    }
}

fn streaming_geometry_hash(geom_type: u8, data: &[u8]) -> Result<u128, String> {
    let mut components = match geom_type {
        1 => streaming_points(data)?,
        2 => streaming_lines(data)?,
        3 => streaming_polygons(data)?,
        _ => Vec::new(),
    };
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-geometry-v1");
    sink.bytes(&[geom_type]);
    sink.bytes(&multiset_hash(b"components", &mut components).to_le_bytes());
    Ok(sink.finish())
}

fn next_geometry_point(
    cursor: &mut Cursor<'_>,
    x: &mut i32,
    y: &mut i32,
    context: &str,
) -> Result<(i32, i32), String> {
    *x = x
        .checked_add(unzigzag(read_geometry_varint(
            cursor,
            &format!("{context} x"),
        )?))
        .ok_or_else(|| format!("{context} x overflows i32"))?;
    *y = y
        .checked_add(unzigzag(read_geometry_varint(
            cursor,
            &format!("{context} y"),
        )?))
        .ok_or_else(|| format!("{context} y overflows i32"))?;
    Ok((*x, *y))
}

fn streaming_points(data: &[u8]) -> Result<Vec<u128>, String> {
    let mut cursor = Cursor::new(data);
    let (mut x, mut y) = (0, 0);
    let mut ring = StreamingRing::new();
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "point command")?;
        if command & 7 != 1 {
            return Err(format!("point geometry contains command {}", command & 7));
        }
        for _ in 0..(command >> 3) {
            ring.point(next_geometry_point(&mut cursor, &mut x, &mut y, "point")?);
        }
    }
    Ok(if ring.vertices == 0 {
        Vec::new()
    } else {
        vec![
            StreamingComponent {
                rings: vec![ring.finish(CanonRingRole::Point)],
            }
            .finish(),
        ]
    })
}

fn streaming_lines(data: &[u8]) -> Result<Vec<u128>, String> {
    let mut cursor = Cursor::new(data);
    let (mut x, mut y) = (0, 0);
    let mut components = Vec::new();
    let mut path: Option<StreamingRing> = None;
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "line command")?;
        match (command & 7, command >> 3) {
            (1, count) => {
                // Mirrors decode_detail_lines: the previous path ends once,
                // the first MoveTo point stays the active path, and any
                // repeat-count extras become single-point components.
                if let Some(path) = path.take() {
                    components.push(
                        StreamingComponent {
                            rings: vec![path.finish(CanonRingRole::Path)],
                        }
                        .finish(),
                    );
                }
                for index in 0..count {
                    let mut ring = StreamingRing::new();
                    ring.point(next_geometry_point(
                        &mut cursor,
                        &mut x,
                        &mut y,
                        "line MoveTo",
                    )?);
                    if index == 0 {
                        path = Some(ring);
                    } else {
                        components.push(
                            StreamingComponent {
                                rings: vec![ring.finish(CanonRingRole::Path)],
                            }
                            .finish(),
                        );
                    }
                }
            }
            (2, count) => {
                let path = path
                    .as_mut()
                    .ok_or_else(|| "line LineTo without MoveTo".to_string())?;
                for _ in 0..count {
                    path.point(next_geometry_point(
                        &mut cursor,
                        &mut x,
                        &mut y,
                        "line LineTo",
                    )?);
                }
            }
            (7, _) => {}
            (id, _) => return Err(format!("unknown line command {id}")),
        }
    }
    if let Some(path) = path {
        components.push(
            StreamingComponent {
                rings: vec![path.finish(CanonRingRole::Path)],
            }
            .finish(),
        );
    }
    Ok(components)
}

fn streaming_polygons(data: &[u8]) -> Result<Vec<u128>, String> {
    let mut cursor = Cursor::new(data);
    let (mut x, mut y) = (0, 0);
    let mut components = Vec::new();
    let mut component = None;
    let mut ring = None;
    let add_ring = |ring: StreamingRing,
                    component: &mut Option<StreamingComponent>,
                    components: &mut Vec<u128>| {
        let role = if ring.area > 0 {
            CanonRingRole::Outer
        } else {
            CanonRingRole::Hole
        };
        if role == CanonRingRole::Outer || component.is_none() {
            if let Some(component) = component.take() {
                components.push(component.finish());
            }
            *component = Some(StreamingComponent::new());
        }
        component
            .as_mut()
            .expect("component initialized")
            .rings
            .push(ring.finish(role));
    };
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "polygon command")?;
        match (command & 7, command >> 3) {
            (1, count) => {
                for _ in 0..count {
                    if let Some(ring) = ring.take() {
                        add_ring(ring, &mut component, &mut components);
                    }
                    let mut next = StreamingRing::new();
                    next.point(next_geometry_point(
                        &mut cursor,
                        &mut x,
                        &mut y,
                        "polygon MoveTo",
                    )?);
                    ring = Some(next);
                }
            }
            (2, count) => {
                let ring = ring
                    .as_mut()
                    .ok_or_else(|| "polygon LineTo without MoveTo".to_string())?;
                for _ in 0..count {
                    ring.point(next_geometry_point(
                        &mut cursor,
                        &mut x,
                        &mut y,
                        "polygon LineTo",
                    )?);
                }
            }
            (7, _) => {
                if let Some(ring) = ring.as_mut()
                    && let Some(first) = ring.first
                {
                    ring.point(first);
                }
            }
            (id, _) => return Err(format!("unknown polygon command {id}")),
        }
    }
    if let Some(ring) = ring {
        add_ring(ring, &mut component, &mut components);
    }
    if let Some(component) = component {
        components.push(component.finish());
    }
    Ok(components)
}

fn invalid_tile(error: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn peak_rss_kb() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmHWM:")?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()
    })
}

fn emit_regress_counters(counters: &RegressCounters) {
    crate::debug::emit_counter_u64("regress_addressed_tiles", counters.addressed_tiles);
    crate::debug::emit_counter_u64("regress_addressed_current", counters.addressed_current);
    crate::debug::emit_counter_u64("regress_addressed_blessed", counters.addressed_blessed);
    crate::debug::emit_counter_u64("regress_directory_runs", counters.directory_runs);
    crate::debug::emit_counter_u64("regress_unique_blobs", counters.unique_blobs);
    crate::debug::emit_counter_u64("regress_unique_blob_pairs", counters.unique_blob_pairs);
    crate::debug::emit_counter_u64("regress_raw_equal_pairs", counters.raw_equal_pairs);
    crate::debug::emit_counter_u64("regress_raw_equal_tiles", counters.raw_equal_tiles);
    crate::debug::emit_counter_u64(
        "regress_canonical_equal_pairs",
        counters.canonical_equal_pairs,
    );
    crate::debug::emit_counter_u64(
        "regress_canonical_equal_tiles",
        counters.canonical_equal_tiles,
    );
    crate::debug::emit_counter_u64("regress_detailed_pairs", counters.detailed_pairs);
    crate::debug::emit_counter_u64("regress_detailed_tiles", counters.detailed_tiles);
    crate::debug::emit_counter_u64("regress_raw_pass_ms", counters.raw_pass_ms);
    crate::debug::emit_counter_u64("regress_canonical_pass_ms", counters.canonical_pass_ms);
    crate::debug::emit_counter_u64("regress_detail_pass_ms", counters.detail_pass_ms);
    crate::debug::emit_counter_u64("regress_peak_rss_kb", counters.peak_rss_kb);
}

// ---------------------------------------------------------------------------
// Comparison-native MVT decoder and streaming fingerprints
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum DetailAttr {
    String(Arc<str>),
    Float(u32),
    Double(u64),
    Int(i64),
    UInt(u64),
    SInt(i64),
    Bool(bool),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DetailRing {
    role: CanonRingRole,
    points: Vec<(i32, i32)>,
    bbox: Bbox,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DetailComponent {
    rings: Vec<DetailRing>,
    bbox: Bbox,
    digest: u128,
    structure: ComponentStructure,
}

#[derive(Clone, Debug)]
struct DetailFeature {
    id: Option<u64>,
    geom_type: u8,
    attrs: Vec<(Arc<str>, DetailAttr)>,
    components: Vec<DetailComponent>,
    attrs_digest: u128,
    geometry_digest: u128,
    bbox: Bbox,
    structure: FeatureStructure,
}

#[derive(Clone, Debug)]
struct DetailLayer {
    name: Arc<str>,
    extent: u32,
    features: Vec<DetailFeature>,
}

#[derive(Clone, Debug)]
struct DetailTile {
    layers: Vec<DetailLayer>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Bbox {
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
    empty: bool,
}

impl Bbox {
    fn from_points(points: &[(i32, i32)]) -> Self {
        let Some(&(x, y)) = points.first() else {
            return Self {
                empty: true,
                ..Self::default()
            };
        };
        let mut out = Self {
            min_x: x,
            min_y: y,
            max_x: x,
            max_y: y,
            empty: false,
        };
        for &(x, y) in &points[1..] {
            out.include_point(x, y);
        }
        out
    }

    fn include_point(&mut self, x: i32, y: i32) {
        if self.empty {
            *self = Self {
                min_x: x,
                min_y: y,
                max_x: x,
                max_y: y,
                empty: false,
            };
            return;
        }
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    fn include_bbox(&mut self, other: Self) {
        if other.empty {
            return;
        }
        self.include_point(other.min_x, other.min_y);
        self.include_point(other.max_x, other.max_y);
    }

    fn lower_bound_sq(self, other: Self) -> u64 {
        if self.empty || other.empty {
            return u64::MAX;
        }
        let dx = axis_gap(self.min_x, self.max_x, other.min_x, other.max_x);
        let dy = axis_gap(self.min_y, self.max_y, other.min_y, other.max_y);
        dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
    }

    fn center_distance_sq(self, other: Self) -> u64 {
        let x = i64::from(self.min_x) + i64::from(self.max_x)
            - i64::from(other.min_x)
            - i64::from(other.max_x);
        let y = i64::from(self.min_y) + i64::from(self.max_y)
            - i64::from(other.min_y)
            - i64::from(other.max_y);
        square_i64(x).saturating_add(square_i64(y))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ComponentStructure {
    rings: u32,
    vertices: u32,
    roles: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct FeatureStructure {
    components: u32,
    rings: u32,
    vertices: u32,
    roles: u64,
}

fn decode_detail_tile(data: &[u8]) -> Result<DetailTile, String> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read tile tag: {error}"))?
    {
        if field == 3 && wire_type == WIRE_LEN {
            let bytes = cursor
                .read_len_delimited()
                .map_err(|error| format!("read tile layer: {error}"))?;
            layers.push(decode_detail_layer(bytes)?);
        } else {
            cursor
                .skip_field(wire_type)
                .map_err(|error| format!("skip tile field {field}: {error}"))?;
        }
    }
    layers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(DetailTile { layers })
}

fn decode_detail_layer(data: &[u8]) -> Result<DetailLayer, String> {
    let mut name: Arc<str> = Arc::from("");
    let mut extent = 4096u32;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut feature_bytes = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read layer tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer name: {error}"))?;
                name = Arc::from(
                    std::str::from_utf8(bytes)
                        .map_err(|error| format!("layer name is not UTF-8: {error}"))?,
                );
            }
            (2, WIRE_LEN) => feature_bytes.push(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature message: {error}"))?,
            ),
            (3, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer key: {error}"))?;
                keys.push(Arc::from(
                    std::str::from_utf8(bytes)
                        .map_err(|error| format!("layer key is not UTF-8: {error}"))?,
                ));
            }
            (4, WIRE_LEN) => values.push(decode_detail_attr(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer value: {error}"))?,
            )?),
            (5, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read layer extent: {error}"))?;
                extent = u32::try_from(raw).map_err(|_| format!("extent out of range: {raw}"))?;
            }
            _ => cursor
                .skip_field(wire_type)
                .map_err(|error| format!("skip layer field {field}: {error}"))?,
        }
    }
    let mut features = Vec::with_capacity(feature_bytes.len());
    for bytes in feature_bytes {
        features.push(decode_detail_feature(bytes, &keys, &values)?);
    }
    features.sort_by(compare_detail_features);
    Ok(DetailLayer {
        name,
        extent,
        features,
    })
}

fn decode_detail_attr(data: &[u8]) -> Result<DetailAttr, String> {
    let mut out = None;
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read value tag: {error}"))?
    {
        let value = match (field, wire_type) {
            (1, WIRE_LEN) => Some(DetailAttr::String(Arc::from(
                std::str::from_utf8(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read string value: {error}"))?,
                )
                .map_err(|error| format!("string value is not UTF-8: {error}"))?,
            ))),
            (2, WIRE_32BIT) => Some(DetailAttr::Float(
                cursor
                    .read_fixed32()
                    .map_err(|error| format!("read float value: {error}"))?,
            )),
            (3, WIRE_64BIT) => Some(DetailAttr::Double(
                cursor
                    .read_fixed64()
                    .map_err(|error| format!("read double value: {error}"))?,
            )),
            (4, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read int value: {error}"))?;
                #[allow(clippy::cast_possible_wrap)]
                Some(DetailAttr::Int(raw as i64))
            }
            (5, WIRE_VARINT) => Some(DetailAttr::UInt(
                cursor
                    .read_varint()
                    .map_err(|error| format!("read uint value: {error}"))?,
            )),
            (6, WIRE_VARINT) => Some(DetailAttr::SInt(unzigzag64(
                cursor
                    .read_varint()
                    .map_err(|error| format!("read sint value: {error}"))?,
            ))),
            (7, WIRE_VARINT) => Some(DetailAttr::Bool(
                cursor
                    .read_varint()
                    .map_err(|error| format!("read bool value: {error}"))?
                    != 0,
            )),
            _ => {
                cursor
                    .skip_field(wire_type)
                    .map_err(|error| format!("skip value field {field}: {error}"))?;
                None
            }
        };
        if value.is_some() {
            out = value;
        }
    }
    out.ok_or_else(|| "empty MVT value".to_string())
}

fn decode_detail_feature(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
) -> Result<DetailFeature, String> {
    let mut id = None;
    let mut attrs = Vec::new();
    let mut geom_type = 0u8;
    let mut geometry = None;
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read feature tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_VARINT) => {
                id = Some(
                    cursor
                        .read_varint()
                        .map_err(|error| format!("read feature id: {error}"))?,
                );
            }
            (2, WIRE_LEN) => {
                let tags = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature tags: {error}"))?;
                attrs = decode_detail_attrs(tags, keys, values)?;
            }
            (3, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read feature type: {error}"))?;
                geom_type =
                    u8::try_from(raw).map_err(|_| format!("geometry type out of range: {raw}"))?;
            }
            (4, WIRE_LEN) => {
                geometry = Some(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read feature geometry: {error}"))?,
                );
            }
            _ => cursor
                .skip_field(wire_type)
                .map_err(|error| format!("skip feature field {field}: {error}"))?,
        }
    }
    attrs.sort();
    let mut components = match geometry {
        Some(geometry) => decode_detail_geometry(geom_type, geometry)?,
        None => Vec::new(),
    };
    components.sort_by(compare_detail_components);
    let attrs_digest = detail_attrs_hash(&attrs);
    let geometry_digest = detail_geometry_hash(geom_type, &components);
    let bbox = components.iter().fold(
        Bbox {
            empty: true,
            ..Bbox::default()
        },
        |mut bbox, component| {
            bbox.include_bbox(component.bbox);
            bbox
        },
    );
    let structure = feature_structure(&components);
    Ok(DetailFeature {
        id,
        geom_type,
        attrs,
        components,
        attrs_digest,
        geometry_digest,
        bbox,
        structure,
    })
}

fn decode_detail_attrs(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
) -> Result<Vec<(Arc<str>, DetailAttr)>, String> {
    let mut cursor = Cursor::new(data);
    let mut attrs = Vec::with_capacity(data.len() / 2);
    while !cursor.is_empty() {
        let key_idx = usize::try_from(
            cursor
                .read_varint()
                .map_err(|error| format!("read feature tag key: {error}"))?,
        )
        .map_err(|_| "feature tag key index overflow".to_string())?;
        let value_idx = usize::try_from(
            cursor
                .read_varint()
                .map_err(|error| format!("read feature tag value: {error}"))?,
        )
        .map_err(|_| "feature tag value index overflow".to_string())?;
        attrs.push((
            Arc::clone(
                keys.get(key_idx)
                    .ok_or_else(|| format!("key index out of range: {key_idx}"))?,
            ),
            values
                .get(value_idx)
                .ok_or_else(|| format!("value index out of range: {value_idx}"))?
                .clone(),
        ));
    }
    Ok(attrs)
}

fn decode_detail_geometry(geom_type: u8, data: &[u8]) -> Result<Vec<DetailComponent>, String> {
    match geom_type {
        1 => decode_detail_points(data),
        2 => decode_detail_lines(data),
        3 => decode_detail_polygons(data),
        _ => Ok(Vec::new()),
    }
}

fn decode_detail_points(data: &[u8]) -> Result<Vec<DetailComponent>, String> {
    let mut cursor = Cursor::new(data);
    let mut points = Vec::new();
    let (mut x, mut y) = (0i32, 0i32);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "point command")?;
        if command & 0x7 != 1 {
            return Err(format!("point geometry contains command {}", command & 0x7));
        }
        for _ in 0..(command >> 3) {
            x = x
                .checked_add(unzigzag(read_geometry_varint(&mut cursor, "point x")?))
                .ok_or_else(|| "point x overflows i32".to_string())?;
            y = y
                .checked_add(unzigzag(read_geometry_varint(&mut cursor, "point y")?))
                .ok_or_else(|| "point y overflows i32".to_string())?;
            points.push((x, y));
        }
    }
    if points.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![make_detail_component(vec![make_detail_ring(
            CanonRingRole::Point,
            points,
        )])])
    }
}

fn decode_detail_lines(data: &[u8]) -> Result<Vec<DetailComponent>, String> {
    let mut cursor = Cursor::new(data);
    let mut components = Vec::new();
    let mut path: Option<Vec<(i32, i32)>> = None;
    let (mut x, mut y) = (0i32, 0i32);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "line command")?;
        let id = command & 0x7;
        let count = command >> 3;
        match id {
            1 => {
                if let Some(path) = path.take() {
                    push_detail_line(&mut components, path);
                }
                for n in 0..count {
                    x = x
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "line MoveTo x",
                        )?))
                        .ok_or_else(|| "line x overflows i32".to_string())?;
                    y = y
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "line MoveTo y",
                        )?))
                        .ok_or_else(|| "line y overflows i32".to_string())?;
                    if n == 0 {
                        path = Some(vec![(x, y)]);
                    } else {
                        push_detail_line(&mut components, vec![(x, y)]);
                    }
                }
            }
            2 => {
                let path = path
                    .as_mut()
                    .ok_or_else(|| "line LineTo without MoveTo".to_string())?;
                for _ in 0..count {
                    x = x
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "line LineTo x",
                        )?))
                        .ok_or_else(|| "line x overflows i32".to_string())?;
                    y = y
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "line LineTo y",
                        )?))
                        .ok_or_else(|| "line y overflows i32".to_string())?;
                    path.push((x, y));
                }
            }
            7 => {}
            _ => return Err(format!("unknown line command {id}")),
        }
    }
    if let Some(path) = path {
        push_detail_line(&mut components, path);
    }
    Ok(components)
}

fn push_detail_line(components: &mut Vec<DetailComponent>, path: Vec<(i32, i32)>) {
    if !path.is_empty() {
        components.push(make_detail_component(vec![make_detail_ring(
            CanonRingRole::Path,
            path,
        )]));
    }
}

fn decode_detail_polygons(data: &[u8]) -> Result<Vec<DetailComponent>, String> {
    let mut cursor = Cursor::new(data);
    let mut rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let (mut x, mut y) = (0i32, 0i32);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "polygon command")?;
        let id = command & 0x7;
        let count = command >> 3;
        match id {
            1 => {
                for _ in 0..count {
                    x = x
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "polygon MoveTo x",
                        )?))
                        .ok_or_else(|| "polygon x overflows i32".to_string())?;
                    y = y
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "polygon MoveTo y",
                        )?))
                        .ok_or_else(|| "polygon y overflows i32".to_string())?;
                    rings.push(vec![(x, y)]);
                }
            }
            2 => {
                let ring = rings
                    .last_mut()
                    .ok_or_else(|| "polygon LineTo without MoveTo".to_string())?;
                for _ in 0..count {
                    x = x
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "polygon LineTo x",
                        )?))
                        .ok_or_else(|| "polygon x overflows i32".to_string())?;
                    y = y
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "polygon LineTo y",
                        )?))
                        .ok_or_else(|| "polygon y overflows i32".to_string())?;
                    ring.push((x, y));
                }
            }
            7 => {
                if let Some(ring) = rings.last_mut()
                    && let Some(&first) = ring.first()
                {
                    // ClosePath does not alter x/y. This is required by MVT 4.3.3.3.
                    ring.push(first);
                }
            }
            _ => return Err(format!("unknown polygon command {id}")),
        }
    }

    // Group rings into components first, then build each component once;
    // building on every hole insertion would re-digest the whole component
    // per hole (quadratic on hole-heavy ocean tiles).
    let mut grouped: Vec<Vec<DetailRing>> = Vec::new();
    for ring in rings {
        let role = if signed_area(&ring) > 0 {
            CanonRingRole::Outer
        } else {
            CanonRingRole::Hole
        };
        let ring = make_detail_ring(role, ring);
        if role == CanonRingRole::Outer || grouped.is_empty() {
            grouped.push(vec![ring]);
        } else if let Some(component) = grouped.last_mut() {
            component.push(ring);
        }
    }
    Ok(grouped.into_iter().map(make_detail_component).collect())
}

fn read_geometry_varint(cursor: &mut Cursor<'_>, context: &str) -> Result<u32, String> {
    let raw = cursor
        .read_varint()
        .map_err(|error| format!("read {context}: {error}"))?;
    u32::try_from(raw).map_err(|_| format!("{context} out of range: {raw}"))
}

fn make_detail_ring(role: CanonRingRole, points: Vec<(i32, i32)>) -> DetailRing {
    let bbox = Bbox::from_points(&points);
    DetailRing { role, points, bbox }
}

fn make_detail_component(rings: Vec<DetailRing>) -> DetailComponent {
    let bbox = rings.iter().fold(
        Bbox {
            empty: true,
            ..Bbox::default()
        },
        |mut bbox, ring| {
            bbox.include_bbox(ring.bbox);
            bbox
        },
    );
    let structure = component_structure(&rings);
    let mut sink = HashSink::new();
    write_detail_component(&mut sink, &rings);
    DetailComponent {
        rings,
        bbox,
        digest: sink.finish(),
        structure,
    }
}

fn component_structure(rings: &[DetailRing]) -> ComponentStructure {
    let mut vertices = 0u32;
    let mut roles = 0u64;
    for ring in rings {
        vertices = vertices.saturating_add(u32::try_from(ring.points.len()).unwrap_or(u32::MAX));
        roles = roles
            .wrapping_mul(5)
            .wrapping_add(u64::from(ring.role as u8) + 1);
    }
    ComponentStructure {
        rings: u32::try_from(rings.len()).unwrap_or(u32::MAX),
        vertices,
        roles,
    }
}

fn feature_structure(components: &[DetailComponent]) -> FeatureStructure {
    let mut rings = 0u32;
    let mut vertices = 0u32;
    let mut roles = 0u64;
    for component in components {
        rings = rings.saturating_add(component.structure.rings);
        vertices = vertices.saturating_add(component.structure.vertices);
        roles = roles
            .wrapping_mul(31)
            .wrapping_add(component.structure.roles);
    }
    FeatureStructure {
        components: u32::try_from(components.len()).unwrap_or(u32::MAX),
        rings,
        vertices,
        roles,
    }
}

fn compare_detail_features(a: &DetailFeature, b: &DetailFeature) -> Ordering {
    a.id.cmp(&b.id)
        .then_with(|| a.geom_type.cmp(&b.geom_type))
        .then_with(|| a.attrs.cmp(&b.attrs))
        .then_with(|| compare_detail_component_slices(&a.components, &b.components))
}

fn compare_detail_components(a: &DetailComponent, b: &DetailComponent) -> Ordering {
    compare_detail_component_slices(std::slice::from_ref(a), std::slice::from_ref(b))
}

fn compare_detail_component_slices(a: &[DetailComponent], b: &[DetailComponent]) -> Ordering {
    for (left, right) in a.iter().zip(b) {
        let rings = left.rings.len().cmp(&right.rings.len());
        if rings != Ordering::Equal {
            return rings;
        }
        for (left_ring, right_ring) in left.rings.iter().zip(&right.rings) {
            let ring = left_ring
                .role
                .cmp(&right_ring.role)
                .then_with(|| left_ring.points.cmp(&right_ring.points));
            if ring != Ordering::Equal {
                return ring;
            }
        }
    }
    a.len().cmp(&b.len())
}

fn detail_attrs_hash(attrs: &[(Arc<str>, DetailAttr)]) -> u128 {
    let mut sink = HashSink::new();
    write_len(&mut sink, attrs.len());
    for (key, value) in attrs {
        write_string(&mut sink, key);
        write_detail_attr(&mut sink, value);
    }
    sink.finish()
}

fn detail_geometry_hash(geom_type: u8, components: &[DetailComponent]) -> u128 {
    let mut sink = HashSink::new();
    sink.bytes(&[geom_type]);
    write_len(&mut sink, components.len());
    for component in components {
        write_detail_component(&mut sink, &component.rings);
    }
    sink.finish()
}

// The sorted-decode hash is the equivalence authority the streaming digest is
// differentially tested against; no production caller remains.
#[cfg(test)]
fn detail_tile_hash(tile: &DetailTile) -> u128 {
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-canon-v1");
    write_len(&mut sink, tile.layers.len());
    for layer in &tile.layers {
        write_string(&mut sink, &layer.name);
        sink.bytes(&layer.extent.to_le_bytes());
        write_len(&mut sink, layer.features.len());
        for feature in &layer.features {
            write_detail_feature(&mut sink, feature);
        }
    }
    sink.finish()
}

trait CanonSink {
    fn bytes(&mut self, bytes: &[u8]);
}

struct HashSink(Xxh3);

impl HashSink {
    fn new() -> Self {
        Self(Xxh3::new())
    }

    fn finish(self) -> u128 {
        self.0.digest128()
    }
}

impl CanonSink for HashSink {
    fn bytes(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

#[cfg(test)]
fn write_detail_feature(sink: &mut impl CanonSink, feature: &DetailFeature) {
    match feature.id {
        Some(id) => {
            sink.bytes(&[1]);
            sink.bytes(&id.to_le_bytes());
        }
        None => sink.bytes(&[0]),
    }
    sink.bytes(&[feature.geom_type]);
    write_len(sink, feature.attrs.len());
    for (key, value) in &feature.attrs {
        write_string(sink, key);
        write_detail_attr(sink, value);
    }
    write_len(sink, feature.components.len());
    for component in &feature.components {
        write_detail_component(sink, &component.rings);
    }
}

fn write_detail_component(sink: &mut impl CanonSink, rings: &[DetailRing]) {
    write_len(sink, rings.len());
    for ring in rings {
        sink.bytes(&[ring.role as u8]);
        write_len(sink, ring.points.len());
        for &(x, y) in &ring.points {
            sink.bytes(&x.to_le_bytes());
            sink.bytes(&y.to_le_bytes());
        }
    }
}

fn write_detail_attr(sink: &mut impl CanonSink, value: &DetailAttr) {
    match value {
        DetailAttr::String(value) => {
            sink.bytes(&[1]);
            write_string(sink, value);
        }
        DetailAttr::Float(value) => {
            sink.bytes(&[2]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::Double(value) => {
            sink.bytes(&[3]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::Int(value) => {
            sink.bytes(&[4]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::UInt(value) => {
            sink.bytes(&[5]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::SInt(value) => {
            sink.bytes(&[6]);
            sink.bytes(&value.to_le_bytes());
        }
        DetailAttr::Bool(value) => sink.bytes(&[7, u8::from(*value)]),
    }
}

fn write_string(sink: &mut impl CanonSink, value: &str) {
    write_len(sink, value.len());
    sink.bytes(value.as_bytes());
}

fn write_len(sink: &mut impl CanonSink, len: usize) {
    sink.bytes(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
}

#[inline]
fn unzigzag(value: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((value >> 1) as i32) ^ (-((value & 1) as i32))
    }
}

#[inline]
fn unzigzag64(value: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((value >> 1) as i64) ^ (-((value & 1) as i64))
    }
}

fn signed_area(ring: &[(i32, i32)]) -> i128 {
    ring.windows(2).fold(0i128, |area, pair| {
        area + i128::from(pair[0].0) * i128::from(pair[1].1)
            - i128::from(pair[1].0) * i128::from(pair[0].1)
    })
}

// ---------------------------------------------------------------------------
// Detail comparison, matching, and geometry distance
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct DetailOutcome {
    counts: ContentCounts,
    events: Vec<OutcomeEvent>,
}

#[derive(Clone, Debug)]
struct OutcomeEvent {
    layer: Arc<str>,
    id: Option<u64>,
    class: OutcomeClass,
    displacement: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum OutcomeClass {
    LayerAdded,
    LayerRemoved,
    ExtentMismatch,
    MissingFeatures,
    AddedFeatures,
    AttrChanged,
    ToleranceMoved,
    StructuralMoved,
}

impl OutcomeClass {
    fn name(self) -> &'static str {
        match self {
            Self::LayerAdded => "layer_added",
            Self::LayerRemoved => "layer_removed",
            Self::ExtentMismatch => "extent_mismatch",
            Self::MissingFeatures => "missing_features",
            Self::AddedFeatures => "added_features",
            Self::AttrChanged => "attr_changed",
            Self::ToleranceMoved => "tolerance_moved",
            Self::StructuralMoved => "structural_moved",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ContentCounts {
    layers_added: u64,
    layers_removed: u64,
    extent_mismatch: u64,
    missing_features: u64,
    added_features: u64,
    attr_changed: u64,
    tolerance_moved: u64,
    structural_moved: u64,
}

impl ContentCounts {
    fn any(&self) -> bool {
        self.layers_added
            + self.layers_removed
            + self.extent_mismatch
            + self.missing_features
            + self.added_features
            + self.attr_changed
            + self.tolerance_moved
            + self.structural_moved
            > 0
    }
}

fn compare_detail_tiles(
    current: &DetailTile,
    blessed: &DetailTile,
    cfg: &RegressConfig,
) -> DetailOutcome {
    let mut out = DetailOutcome::default();
    let (mut ci, mut bi) = (0usize, 0usize);
    while ci < current.layers.len() || bi < blessed.layers.len() {
        match (current.layers.get(ci), blessed.layers.get(bi)) {
            (Some(cur), Some(bl)) => match cur.name.cmp(&bl.name) {
                Ordering::Less => {
                    out.record(Arc::clone(&cur.name), None, OutcomeClass::LayerAdded, 0);
                    ci += 1;
                }
                Ordering::Greater => {
                    out.record(Arc::clone(&bl.name), None, OutcomeClass::LayerRemoved, 0);
                    bi += 1;
                }
                Ordering::Equal if cur.extent != bl.extent => {
                    out.record(Arc::clone(&cur.name), None, OutcomeClass::ExtentMismatch, 0);
                    ci += 1;
                    bi += 1;
                }
                Ordering::Equal => {
                    compare_detail_layer(cur, bl, cfg, &mut out);
                    ci += 1;
                    bi += 1;
                }
            },
            (Some(cur), None) => {
                out.record(Arc::clone(&cur.name), None, OutcomeClass::LayerAdded, 0);
                ci += 1;
            }
            (None, Some(bl)) => {
                out.record(Arc::clone(&bl.name), None, OutcomeClass::LayerRemoved, 0);
                bi += 1;
            }
            (None, None) => break,
        }
    }
    out
}

impl DetailOutcome {
    fn record(&mut self, layer: Arc<str>, id: Option<u64>, class: OutcomeClass, displacement: i32) {
        match class {
            OutcomeClass::LayerAdded => self.counts.layers_added += 1,
            OutcomeClass::LayerRemoved => self.counts.layers_removed += 1,
            OutcomeClass::ExtentMismatch => self.counts.extent_mismatch += 1,
            OutcomeClass::MissingFeatures => self.counts.missing_features += 1,
            OutcomeClass::AddedFeatures => self.counts.added_features += 1,
            OutcomeClass::AttrChanged => self.counts.attr_changed += 1,
            OutcomeClass::ToleranceMoved => self.counts.tolerance_moved += 1,
            OutcomeClass::StructuralMoved => self.counts.structural_moved += 1,
        }
        self.events.push(OutcomeEvent {
            layer,
            id,
            class,
            displacement,
        });
    }
}

fn compare_detail_layer(
    current: &DetailLayer,
    blessed: &DetailLayer,
    cfg: &RegressConfig,
    out: &mut DetailOutcome,
) {
    let ocean = current.name.as_ref() == "ocean";
    let mut cur_ids: IdGroups<'_> = FxHashMap::default();
    let mut bl_ids: IdGroups<'_> = FxHashMap::default();
    let mut cur_anon = AnonymousGroups::default();
    let mut bl_anon = AnonymousGroups::default();

    for feature in &current.features {
        match feature.id {
            Some(id) if !ocean => cur_ids.entry(id).or_default().push(feature),
            _ => cur_anon.push(feature),
        }
    }
    for feature in &blessed.features {
        match feature.id {
            Some(id) if !ocean => bl_ids.entry(id).or_default().push(feature),
            _ => bl_anon.push(feature),
        }
    }

    let mut ids: Vec<u64> = cur_ids.keys().chain(bl_ids.keys()).copied().collect();
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        compare_id_group(
            &current.name,
            &cur_ids.remove(&id).unwrap_or_default(),
            &bl_ids.remove(&id).unwrap_or_default(),
            cfg,
            out,
        );
    }

    let mut groups = cur_anon.merge_with(bl_anon);
    groups.sort_by_key(|group| group.hash);
    for group in groups {
        compare_anonymous_group(&current.name, &group.current, &group.blessed, cfg, out);
    }
}

type IdGroups<'a> = FxHashMap<u64, Vec<&'a DetailFeature>>;

/// Anonymous features grouped by attr digest (verified against the actual
/// attrs on collision). Each side builds its own instance, pushing into
/// `current`; `merge_with` then folds the other side's features into
/// `blessed`, so the field names are only meaningful after the merge.
#[derive(Default)]
struct AnonymousGroups<'a> {
    buckets: FxHashMap<u128, Vec<AnonymousGroup<'a>>>,
}

struct AnonymousGroup<'a> {
    attrs: &'a [(Arc<str>, DetailAttr)],
    current: Vec<&'a DetailFeature>,
    blessed: Vec<&'a DetailFeature>,
}

struct MergedAnonymousGroup<'a> {
    hash: u128,
    current: Vec<&'a DetailFeature>,
    blessed: Vec<&'a DetailFeature>,
}

impl<'a> AnonymousGroups<'a> {
    fn push(&mut self, feature: &'a DetailFeature) {
        let groups = self.buckets.entry(feature.attrs_digest).or_default();
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.attrs == feature.attrs.as_slice())
        {
            group.current.push(feature);
        } else {
            groups.push(AnonymousGroup {
                attrs: &feature.attrs,
                current: vec![feature],
                blessed: Vec::new(),
            });
        }
    }

    fn merge_with(mut self, other: Self) -> Vec<MergedAnonymousGroup<'a>> {
        for (hash, groups) in other.buckets {
            let ours = self.buckets.entry(hash).or_default();
            for group in groups {
                if let Some(existing) = ours
                    .iter_mut()
                    .find(|existing| existing.attrs == group.attrs)
                {
                    existing.blessed.extend(group.current);
                    existing.blessed.extend(group.blessed);
                } else {
                    ours.push(AnonymousGroup {
                        attrs: group.attrs,
                        current: group.blessed,
                        blessed: group.current,
                    });
                }
            }
        }
        self.buckets
            .into_iter()
            .flat_map(|(hash, groups)| {
                groups.into_iter().map(move |group| MergedAnonymousGroup {
                    hash,
                    current: group.current,
                    blessed: group.blessed,
                })
            })
            .collect()
    }
}

fn compare_id_group(
    layer: &Arc<str>,
    current: &[&DetailFeature],
    blessed: &[&DetailFeature],
    cfg: &RegressConfig,
    out: &mut DetailOutcome,
) {
    let pairs = current.len().min(blessed.len());
    for idx in 0..pairs {
        let cur = current[idx];
        let bl = blessed[idx];
        if cur.attrs != bl.attrs {
            out.record(Arc::clone(layer), cur.id, OutcomeClass::AttrChanged, 0);
        } else {
            classify_detail_geometry(Arc::clone(layer), cur, bl, cfg, out);
        }
    }
    for feature in current.iter().skip(pairs) {
        out.record(
            Arc::clone(layer),
            feature.id,
            OutcomeClass::AddedFeatures,
            0,
        );
    }
    for feature in blessed.iter().skip(pairs) {
        out.record(
            Arc::clone(layer),
            feature.id,
            OutcomeClass::MissingFeatures,
            0,
        );
    }
}

fn compare_anonymous_group(
    layer: &Arc<str>,
    current: &[&DetailFeature],
    blessed: &[&DetailFeature],
    cfg: &RegressConfig,
    out: &mut DetailOutcome,
) {
    let pairs = pair_detail_features(current, blessed);
    for (ci, bi) in pairs.paired {
        classify_detail_geometry(Arc::clone(layer), current[ci], blessed[bi], cfg, out);
    }
    for ci in pairs.unpaired_current {
        out.record(
            Arc::clone(layer),
            current[ci].id,
            OutcomeClass::AddedFeatures,
            0,
        );
    }
    for bi in pairs.unpaired_blessed {
        out.record(
            Arc::clone(layer),
            blessed[bi].id,
            OutcomeClass::MissingFeatures,
            0,
        );
    }
}

struct PairResult {
    paired: Vec<(usize, usize)>,
    unpaired_current: Vec<usize>,
    unpaired_blessed: Vec<usize>,
}

fn pair_detail_features(current: &[&DetailFeature], blessed: &[&DetailFeature]) -> PairResult {
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; blessed.len()];
    let mut paired = exact_feature_pairs(current, blessed, &mut cur_used, &mut bl_used);
    let residual = remaining_pairs(
        current,
        blessed,
        &mut cur_used,
        &mut bl_used,
        |feature| (feature.geom_type, feature.structure),
        |left, right| left.bbox.lower_bound_sq(right.bbox),
        |left, right| left.bbox.center_distance_sq(right.bbox),
        |left, right| feature_distance(left, right),
    );
    paired.extend(residual);
    finish_pairs(&cur_used, &bl_used, paired)
}

fn exact_feature_pairs(
    current: &[&DetailFeature],
    blessed: &[&DetailFeature],
    cur_used: &mut [bool],
    bl_used: &mut [bool],
) -> Vec<(usize, usize)> {
    let mut buckets: FxHashMap<u128, Vec<usize>> = FxHashMap::default();
    for (idx, feature) in blessed.iter().enumerate() {
        buckets
            .entry(feature.geometry_digest)
            .or_default()
            .push(idx);
    }
    let mut paired = Vec::new();
    for (ci, feature) in current.iter().enumerate() {
        let Some(candidates) = buckets.get(&feature.geometry_digest) else {
            continue;
        };
        if let Some(&bi) = candidates
            .iter()
            .find(|&&bi| !bl_used[bi] && detail_geometry_equal(feature, blessed[bi]))
        {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }
    paired
}

fn detail_geometry_equal(left: &DetailFeature, right: &DetailFeature) -> bool {
    left.geom_type == right.geom_type
        && compare_detail_component_slices(&left.components, &right.components) == Ordering::Equal
}

#[allow(clippy::too_many_arguments)]
fn remaining_pairs<T, K: Eq + std::hash::Hash + Copy>(
    current: &[T],
    blessed: &[T],
    cur_used: &mut [bool],
    bl_used: &mut [bool],
    key: impl Fn(&T) -> K,
    lower: impl Fn(&T, &T) -> u64,
    proxy: impl Fn(&T, &T) -> u64,
    distance: impl Fn(&T, &T) -> i32,
) -> Vec<(usize, usize)> {
    let remaining_current: Vec<usize> = cur_used
        .iter()
        .enumerate()
        .filter_map(|(idx, used)| (!*used).then_some(idx))
        .collect();
    let remaining_blessed: Vec<usize> = bl_used
        .iter()
        .enumerate()
        .filter_map(|(idx, used)| (!*used).then_some(idx))
        .collect();
    if remaining_current
        .len()
        .saturating_mul(remaining_blessed.len())
        <= 64
    {
        return exact_greedy_pairs(
            current,
            blessed,
            cur_used,
            bl_used,
            &remaining_current,
            &remaining_blessed,
            distance,
        );
    }

    let candidates = residual_candidates(
        current,
        blessed,
        &remaining_current,
        &remaining_blessed,
        &key,
        &lower,
        &proxy,
    );
    let mut paired =
        sparse_min_cost_pairs(current, blessed, cur_used, bl_used, &candidates, &distance);

    // Same-key completion: the K-nearest candidate graph need not contain a
    // matching that saturates the smaller side of every key group (clusters
    // larger than K can starve each other), and the pre-sparse contract was
    // that same-key pairings are exhausted before any cross-key fallback.
    // Sweep the leftovers with the old proxy-greedy, per key; leftover
    // counts are the starvation excess, so the quadratic edge enumeration
    // stays small.
    let mut completion_edges = Vec::new();
    for &ci in &remaining_current {
        if cur_used[ci] {
            continue;
        }
        for &bi in &remaining_blessed {
            if !bl_used[bi] && key(&current[ci]) == key(&blessed[bi]) {
                completion_edges.push((
                    lower(&current[ci], &blessed[bi]),
                    proxy(&current[ci], &blessed[bi]),
                    ci,
                    bi,
                ));
            }
        }
    }
    completion_edges.sort_unstable();
    for (_, _, ci, bi) in completion_edges {
        if !cur_used[ci] && !bl_used[bi] {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }

    // A type or topology change is still one structural movement, rather than
    // an arbitrary added/missing pair. This deliberate fallback keeps the old
    // cardinality semantics while avoiding impossible Hausdorff work.
    let left: Vec<usize> = cur_used
        .iter()
        .enumerate()
        .filter_map(|(idx, used)| (!*used).then_some(idx))
        .collect();
    let right: Vec<usize> = bl_used
        .iter()
        .enumerate()
        .filter_map(|(idx, used)| (!*used).then_some(idx))
        .collect();
    for (ci, bi) in left.into_iter().zip(right) {
        cur_used[ci] = true;
        bl_used[bi] = true;
        paired.push((ci, bi));
    }
    paired
}

const RESIDUAL_CANDIDATES: usize = 8;

#[allow(clippy::too_many_arguments)]
fn residual_candidates<T, K: Eq + std::hash::Hash + Copy>(
    current: &[T],
    blessed: &[T],
    remaining_current: &[usize],
    remaining_blessed: &[usize],
    key: &impl Fn(&T) -> K,
    lower: &impl Fn(&T, &T) -> u64,
    proxy: &impl Fn(&T, &T) -> u64,
) -> Vec<(usize, usize)> {
    let mut edges = FxHashSet::default();
    for &ci in remaining_current {
        let mut nearest: Vec<_> = remaining_blessed
            .iter()
            .copied()
            .filter(|&bi| key(&current[ci]) == key(&blessed[bi]))
            .map(|bi| {
                (
                    lower(&current[ci], &blessed[bi]),
                    proxy(&current[ci], &blessed[bi]),
                    bi,
                )
            })
            .collect();
        nearest.sort_unstable();
        edges.extend(
            nearest
                .into_iter()
                .take(RESIDUAL_CANDIDATES)
                .map(|(_, _, bi)| (ci, bi)),
        );
    }
    for &bi in remaining_blessed {
        let mut nearest: Vec<_> = remaining_current
            .iter()
            .copied()
            .filter(|&ci| key(&current[ci]) == key(&blessed[bi]))
            .map(|ci| {
                (
                    lower(&current[ci], &blessed[bi]),
                    proxy(&current[ci], &blessed[bi]),
                    ci,
                )
            })
            .collect();
        nearest.sort_unstable();
        edges.extend(
            nearest
                .into_iter()
                .take(RESIDUAL_CANDIDATES)
                .map(|(_, _, ci)| (ci, bi)),
        );
    }
    let mut edges: Vec<_> = edges.into_iter().collect();
    edges.sort_unstable();
    edges
}

fn sparse_min_cost_pairs<T>(
    current: &[T],
    blessed: &[T],
    cur_used: &mut [bool],
    bl_used: &mut [bool],
    candidates: &[(usize, usize)],
    distance: &impl Fn(&T, &T) -> i32,
) -> Vec<(usize, usize)> {
    // Successive shortest augmenting paths give a minimum-cost maximum-
    // cardinality matching while evaluating Hausdorff once per candidate
    // edge. Bellman-Ford handles the negative reverse (matched) edges.
    // Relaxation is STRICTLY improving: a predecessor-pointer cycle would
    // require a strict distance decrease around a zero-cost alternating
    // loop, which is impossible, and matchings built by shortest-path
    // augmentation stay extreme, so the residual graph never has a negative
    // cycle and Bellman-Ford converges within one pass per residual vertex.
    // Ties between equal-cost paths resolve to whichever the fixed edge
    // order relaxes first, which keeps the result deterministic.
    let edges: Vec<_> = candidates
        .iter()
        .map(|&(ci, bi)| (ci, bi, i64::from(distance(&current[ci], &blessed[bi]))))
        .collect();
    let edge_cost: FxHashMap<(usize, usize), i64> = edges
        .iter()
        .map(|&(ci, bi, cost)| ((ci, bi), cost))
        .collect();
    let mut residual_blessed: Vec<usize> = candidates.iter().map(|&(_, bi)| bi).collect();
    residual_blessed.sort_unstable();
    residual_blessed.dedup();
    // Candidates are sorted by (ci, bi), so ci values arrive grouped.
    let mut residual_current: Vec<usize> = candidates.iter().map(|&(ci, _)| ci).collect();
    residual_current.dedup();
    let vertex_bound = residual_current.len() + residual_blessed.len();

    let mut cur_match: Vec<Option<usize>> = vec![None; current.len()];
    let mut bl_match: Vec<Option<usize>> = vec![None; blessed.len()];
    let mut cur_cost = vec![0_i64; current.len()];

    loop {
        let mut cur_dist = vec![i64::MAX; current.len()];
        let mut bl_dist = vec![i64::MAX; blessed.len()];
        let mut prev_blessed: Vec<Option<usize>> = vec![None; blessed.len()];
        for &ci in &residual_current {
            if !cur_used[ci] && cur_match[ci].is_none() {
                cur_dist[ci] = 0;
            }
        }

        for _ in 0..=vertex_bound {
            let mut changed = false;
            for &(ci, bi, cost) in &edges {
                if cur_dist[ci] == i64::MAX || cur_match[ci] == Some(bi) {
                    continue;
                }
                let relaxed = cur_dist[ci].saturating_add(cost);
                if relaxed < bl_dist[bi] {
                    bl_dist[bi] = relaxed;
                    prev_blessed[bi] = Some(ci);
                    // The only edge back out of a matched blessed node is its
                    // matched current, so the reverse relaxation rides along
                    // here instead of needing its own scan.
                    if let Some(mi) = bl_match[bi] {
                        let back = relaxed.saturating_sub(cur_cost[mi]);
                        if back < cur_dist[mi] {
                            cur_dist[mi] = back;
                        }
                    }
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let target = (0..blessed.len())
            .filter(|&bi| !bl_used[bi] && bl_match[bi].is_none() && bl_dist[bi] != i64::MAX)
            .min_by_key(|&bi| (bl_dist[bi], bi));
        let Some(mut bi) = target else {
            break;
        };
        // Alternate forward predecessor and matched edge back to a free
        // current. Consistent pointers cannot revisit a vertex, so the walk
        // is bounded; exceeding the bound means the invariant broke.
        let mut hops = 0usize;
        loop {
            hops += 1;
            assert!(
                hops <= vertex_bound,
                "augmenting path exceeds its vertex bound"
            );
            let ci = prev_blessed[bi].expect("augmenting path reaches a relaxed blessed node");
            let previous_bi = cur_match[ci];
            cur_match[ci] = Some(bi);
            bl_match[bi] = Some(ci);
            cur_cost[ci] = *edge_cost
                .get(&(ci, bi))
                .expect("augmenting path uses a candidate edge");
            let Some(old_bi) = previous_bi else {
                break;
            };
            bl_match[old_bi] = None;
            bi = old_bi;
        }
    }

    let mut paired = Vec::new();
    for (ci, matched) in cur_match.into_iter().enumerate() {
        if let Some(bi) = matched {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }
    paired
}

fn exact_greedy_pairs<T>(
    current: &[T],
    blessed: &[T],
    cur_used: &mut [bool],
    bl_used: &mut [bool],
    remaining_current: &[usize],
    remaining_blessed: &[usize],
    distance: impl Fn(&T, &T) -> i32,
) -> Vec<(usize, usize)> {
    let mut paired = Vec::new();
    loop {
        let mut best: Option<(usize, usize, i32)> = None;
        for &ci in remaining_current {
            if cur_used[ci] {
                continue;
            }
            for &bi in remaining_blessed {
                if bl_used[bi] {
                    continue;
                }
                let candidate = distance(&current[ci], &blessed[bi]);
                if best.is_none_or(|(_, _, distance)| candidate < distance) {
                    best = Some((ci, bi, candidate));
                }
            }
        }
        let Some((ci, bi, _)) = best else {
            break;
        };
        cur_used[ci] = true;
        bl_used[bi] = true;
        paired.push((ci, bi));
    }
    paired
}

fn finish_pairs(cur_used: &[bool], bl_used: &[bool], paired: Vec<(usize, usize)>) -> PairResult {
    PairResult {
        paired,
        unpaired_current: cur_used
            .iter()
            .enumerate()
            .filter_map(|(idx, used)| (!*used).then_some(idx))
            .collect(),
        unpaired_blessed: bl_used
            .iter()
            .enumerate()
            .filter_map(|(idx, used)| (!*used).then_some(idx))
            .collect(),
    }
}

fn classify_detail_geometry(
    layer: Arc<str>,
    current: &DetailFeature,
    blessed: &DetailFeature,
    cfg: &RegressConfig,
    out: &mut DetailOutcome,
) {
    if current.geom_type != blessed.geom_type {
        out.record(layer, current.id, OutcomeClass::StructuralMoved, 0);
        return;
    }
    let Some(distance) = classify_detail_components(current, blessed) else {
        out.record(layer, current.id, OutcomeClass::StructuralMoved, 0);
        return;
    };
    if distance == 0 {
        return;
    }
    let class = if distance <= cfg.tol {
        OutcomeClass::ToleranceMoved
    } else {
        OutcomeClass::StructuralMoved
    };
    out.record(layer, current.id, class, distance);
}

fn classify_detail_components(current: &DetailFeature, blessed: &DetailFeature) -> Option<i32> {
    if current.components.len() != blessed.components.len() {
        return None;
    }
    let pairs = pair_detail_components(&current.components, &blessed.components);
    if !pairs.unpaired_current.is_empty() || !pairs.unpaired_blessed.is_empty() {
        return None;
    }
    let mut maximum = 0;
    for (ci, bi) in pairs.paired {
        let cur = &current.components[ci];
        let bl = &blessed.components[bi];
        if !component_structure_matches(cur, bl) {
            return None;
        }
        maximum = maximum.max(component_distance(cur, bl));
    }
    Some(maximum)
}

fn pair_detail_components(current: &[DetailComponent], blessed: &[DetailComponent]) -> PairResult {
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; blessed.len()];
    let mut buckets: FxHashMap<u128, Vec<usize>> = FxHashMap::default();
    for (idx, component) in blessed.iter().enumerate() {
        buckets.entry(component.digest).or_default().push(idx);
    }
    let mut paired = Vec::new();
    for (ci, component) in current.iter().enumerate() {
        if let Some(candidates) = buckets.get(&component.digest)
            && let Some(&bi) = candidates.iter().find(|&&bi| {
                !bl_used[bi]
                    && compare_detail_components(component, &blessed[bi]) == Ordering::Equal
            })
        {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }
    paired.extend(remaining_pairs(
        current,
        blessed,
        &mut cur_used,
        &mut bl_used,
        |component| component.structure,
        |left, right| left.bbox.lower_bound_sq(right.bbox),
        |left, right| left.bbox.center_distance_sq(right.bbox),
        component_distance,
    ));
    finish_pairs(&cur_used, &bl_used, paired)
}

fn component_structure_matches(current: &DetailComponent, blessed: &DetailComponent) -> bool {
    if current.rings.len() != blessed.rings.len()
        || current
            .rings
            .iter()
            .zip(&blessed.rings)
            .any(|(left, right)| left.role != right.role)
    {
        return false;
    }
    polygon_holes_contained(current) == polygon_holes_contained(blessed)
}

fn polygon_holes_contained(component: &DetailComponent) -> bool {
    let Some(outer) = component
        .rings
        .iter()
        .find(|ring| ring.role == CanonRingRole::Outer)
    else {
        return true;
    };
    component
        .rings
        .iter()
        .filter(|ring| ring.role == CanonRingRole::Hole)
        .all(|hole| {
            hole.points
                .first()
                .is_some_and(|&point| point_in_ring(point, &outer.points))
        })
}

fn point_in_ring(point: (i32, i32), ring: &[(i32, i32)]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let (px, py) = point;
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if (yi > py) != (yj > py) {
            let lhs = i64::from(px - xi) * i64::from(yj - yi);
            let rhs = i64::from(xj - xi) * i64::from(py - yi);
            let crosses = if yj > yi { lhs < rhs } else { lhs > rhs };
            if crosses {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

fn feature_distance(current: &DetailFeature, blessed: &DetailFeature) -> i32 {
    if current.geom_type != blessed.geom_type {
        return i32::MAX;
    }
    let mut best = i32::MAX;
    for cur in &current.components {
        for bl in &blessed.components {
            let lower = ceil_sqrt(cur.bbox.lower_bound_sq(bl.bbox));
            if lower >= best {
                continue;
            }
            best = best.min(component_distance(cur, bl));
        }
    }
    best
}

fn component_distance(current: &DetailComponent, blessed: &DetailComponent) -> i32 {
    current
        .rings
        .iter()
        .zip(&blessed.rings)
        .map(|(left, right)| discrete_hausdorff(&left.points, &right.points))
        .max()
        .unwrap_or(0)
}

fn discrete_hausdorff(a: &[(i32, i32)], b: &[(i32, i32)]) -> i32 {
    if a.is_empty() || b.is_empty() {
        return i32::MAX;
    }
    let ab = directed_distance(a, b);
    let ba = directed_distance(b, a);
    ceil_sqrt(ab.max(ba))
}

fn directed_distance(from: &[(i32, i32)], to: &[(i32, i32)]) -> u64 {
    let index = PointIndex::new(to);
    let mut maximum = 0u64;
    let mut rolling_start = 0usize;
    for &point in from {
        let (nearest, start) = index.nearest(point, rolling_start);
        rolling_start = start;
        maximum = maximum.max(nearest);
    }
    maximum
}

enum PointIndex<'a> {
    Small(&'a [(i32, i32)]),
    Tree(KdNode),
}

impl<'a> PointIndex<'a> {
    fn new(points: &'a [(i32, i32)]) -> Self {
        const KD_TREE_THRESHOLD: usize = 64;
        if points.len() < KD_TREE_THRESHOLD {
            Self::Small(points)
        } else {
            Self::Tree(KdNode::build(points.to_vec(), 0))
        }
    }

    fn nearest(&self, point: (i32, i32), rolling_start: usize) -> (u64, usize) {
        match self {
            Self::Small(points) => {
                let mut best = u64::MAX;
                let mut best_idx = 0usize;
                for step in 0..points.len() {
                    let idx = (rolling_start + step) % points.len();
                    let distance = squared_distance(point, points[idx]);
                    if distance < best {
                        best = distance;
                        best_idx = idx;
                        if best == 0 {
                            break;
                        }
                    }
                }
                (best, best_idx)
            }
            Self::Tree(tree) => (tree.nearest(point, u64::MAX), 0),
        }
    }
}

struct KdNode {
    point: (i32, i32),
    axis: usize,
    bbox: Bbox,
    left: Option<Box<Self>>,
    right: Option<Box<Self>>,
}

impl KdNode {
    fn build(mut points: Vec<(i32, i32)>, depth: usize) -> Self {
        let axis = depth % 2;
        points.sort_unstable_by_key(|point| if axis == 0 { point.0 } else { point.1 });
        let middle = points.len() / 2;
        let right = points.split_off(middle + 1);
        let point = points.pop().expect("KD tree build has a median point");
        let left = (!points.is_empty()).then(|| Box::new(Self::build(points, depth + 1)));
        let right = (!right.is_empty()).then(|| Box::new(Self::build(right, depth + 1)));
        let mut bbox = Bbox::from_points(&[point]);
        if let Some(left) = &left {
            bbox.include_bbox(left.bbox);
        }
        if let Some(right) = &right {
            bbox.include_bbox(right.bbox);
        }
        Self {
            point,
            axis,
            bbox,
            left,
            right,
        }
    }

    fn nearest(&self, point: (i32, i32), mut best: u64) -> u64 {
        best = best.min(squared_distance(point, self.point));
        let (near, far) = if (self.axis == 0 && point.0 <= self.point.0)
            || (self.axis == 1 && point.1 <= self.point.1)
        {
            (&self.left, &self.right)
        } else {
            (&self.right, &self.left)
        };
        if let Some(near) = near
            && near.bbox.lower_bound_sq(Bbox::from_points(&[point])) < best
        {
            best = near.nearest(point, best);
        }
        if let Some(far) = far
            && far.bbox.lower_bound_sq(Bbox::from_points(&[point])) < best
        {
            best = far.nearest(point, best);
        }
        best
    }
}

fn axis_gap(a_min: i32, a_max: i32, b_min: i32, b_max: i32) -> u64 {
    if a_max < b_min {
        u64::try_from(i64::from(b_min) - i64::from(a_max)).unwrap_or(u64::MAX)
    } else if b_max < a_min {
        u64::try_from(i64::from(a_min) - i64::from(b_max)).unwrap_or(u64::MAX)
    } else {
        0
    }
}

fn square_i64(value: i64) -> u64 {
    value.unsigned_abs().saturating_mul(value.unsigned_abs())
}

fn squared_distance(a: (i32, i32), b: (i32, i32)) -> u64 {
    square_i64(i64::from(a.0) - i64::from(b.0))
        .saturating_add(square_i64(i64::from(a.1) - i64::from(b.1)))
}

fn ceil_sqrt(value: u64) -> i32 {
    let root = integer_sqrt(value);
    let ceil = if root.saturating_mul(root) < value {
        root + 1
    } else {
        root
    };
    i32::try_from(ceil).unwrap_or(i32::MAX)
}

fn integer_sqrt(value: u64) -> u64 {
    let mut result = 0u64;
    let mut bit = 1u64 << 62;
    while bit > value {
        bit >>= 2;
    }
    let mut remainder = value;
    while bit != 0 {
        if remainder >= result + bit {
            remainder -= result + bit;
            result = (result >> 1) + bit;
        } else {
            result >>= 1;
        }
        bit >>= 2;
    }
    result
}

// ---------------------------------------------------------------------------
// Deterministic report aggregation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct LayerInterner {
    names: FxHashMap<Arc<str>, Arc<str>>,
}

impl LayerInterner {
    fn intern(&mut self, name: &Arc<str>) -> Arc<str> {
        if let Some(existing) = self.names.get(name) {
            return Arc::clone(existing);
        }
        self.names.insert(Arc::clone(name), Arc::clone(name));
        Arc::clone(name)
    }
}

/// Example selection key; its `Ord` is the selection priority (lowest tile id
/// first, then layer and feature id) within each outcome class.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExampleKey {
    tile_id: u64,
    layer: Arc<str>,
    id: Option<u64>,
}

/// Keeps at most `cap` example candidates per outcome class, always the
/// smallest keys seen so far. This bounds report memory on broad-difference
/// runs where the raw candidate stream is millions of events.
struct ExampleSelector {
    cap: usize,
    per_class: BTreeMap<OutcomeClass, std::collections::BinaryHeap<ExampleKey>>,
}

impl ExampleSelector {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            per_class: BTreeMap::new(),
        }
    }

    fn offer(&mut self, class: OutcomeClass, key: ExampleKey) {
        if self.cap == 0 {
            return;
        }
        let heap = self.per_class.entry(class).or_default();
        if heap.len() < self.cap {
            heap.push(key);
        } else if heap.peek().is_some_and(|worst| key < *worst) {
            heap.pop();
            heap.push(key);
        }
    }

    fn finish(self) -> Vec<DiffExample> {
        let mut examples: Vec<DiffExample> = self
            .per_class
            .into_iter()
            .flat_map(|(class, heap)| {
                heap.into_iter().map(move |key| DiffExample {
                    tile_id: key.tile_id,
                    layer: key.layer,
                    id: key.id,
                    class: class.name().to_string(),
                })
            })
            .collect();
        examples.sort_by(|left, right| {
            left.tile_id
                .cmp(&right.tile_id)
                .then_with(|| left.layer.cmp(&right.layer))
                .then_with(|| left.id.cmp(&right.id))
                .then_with(|| left.class.cmp(&right.class))
        });
        examples
    }
}

fn apply_missing_span(report: &mut RegressReport, span: PairSpan) {
    let count = span.tiles();
    match (span.current, span.blessed) {
        (Some(_), None) => report.totals.only_in_current += count,
        (None, Some(_)) => report.totals.only_in_blessed += count,
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
    match class {
        OutcomeClass::LayerAdded => {
            totals.layers_added += count;
            layer.layers_added += count;
        }
        OutcomeClass::LayerRemoved => {
            totals.layers_removed += count;
            layer.layers_removed += count;
        }
        OutcomeClass::ExtentMismatch => {
            totals.extent_mismatch += count;
            layer.extent_mismatch += count;
        }
        OutcomeClass::MissingFeatures => {
            totals.missing_features += count;
            layer.missing_features += count;
        }
        OutcomeClass::AddedFeatures => {
            totals.added_features += count;
            layer.added_features += count;
        }
        OutcomeClass::AttrChanged => {
            totals.attr_changed += count;
            layer.attr_changed += count;
        }
        OutcomeClass::ToleranceMoved => {
            totals.tolerance_moved += count;
            layer.tolerance_moved += count;
        }
        OutcomeClass::StructuralMoved => {
            totals.structural_moved += count;
            layer.structural_moved += count;
        }
    }
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

fn counters_are_zero(counters: &LayerCounters) -> bool {
    counters.layers_added
        + counters.layers_removed
        + counters.extent_mismatch
        + counters.missing_features
        + counters.added_features
        + counters.attr_changed
        + counters.tolerance_moved
        + counters.structural_moved
        == 0
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
