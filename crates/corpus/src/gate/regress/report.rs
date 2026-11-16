//! The regress report: what the diff accumulated, and how it prints.
//!
//! The report is bounded by construction. A full-archive diff can produce
//! millions of raw events, so nothing here grows with the diff size except the
//! coalesced tile ranges: per-tile records were replaced by a count plus ranges,
//! per-zoom/layer counters roll up the classes, displacements are kept as
//! histograms rather than samples, and examples are capped per outcome class by
//! `ExampleSelector`.
//!
//! Ported verbatim from elivagar's shed `regress.rs` (commit `0129ef3~1`).

use std::collections::{BTreeMap, BinaryHeap};
use std::sync::Arc;

use serde_json::json;

use super::super::constants::DEFAULT_MAX_EXAMPLES;
use super::super::eliv::tile_id_to_zxy;
use super::compare::{OutcomeClass, OutcomeCounts};
use crate::output;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RegressConfig {
    /// Displacements at or below this many tile units are `tolerance_moved`
    /// rather than `structural_moved`.
    pub tol: i32,
    /// How many `tolerance_moved` features a run may carry and still pass.
    /// Defaults to 0, so `--tol` alone accepts nothing - a tolerance is only
    /// ever granted together with a budget.
    pub max_moved: u64,
    pub max_examples: usize,
}

impl Default for RegressConfig {
    fn default() -> Self {
        Self {
            tol: 0,
            max_moved: 0,
            max_examples: DEFAULT_MAX_EXAMPLES,
        }
    }
}

// ---------------------------------------------------------------------------
// Report structure
// ---------------------------------------------------------------------------

/// A half-open tile id range `[start, end)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffTotals {
    pub only_in_current: u64,
    pub only_in_baseline: u64,
    pub(crate) outcomes: OutcomeCounts,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerCounters {
    pub(crate) outcomes: OutcomeCounts,
}

impl DiffTotals {
    pub(crate) fn count(&self, class: OutcomeClass) -> u64 {
        self.outcomes.get(class)
    }
}

impl LayerCounters {
    pub(crate) fn count(&self, class: OutcomeClass) -> u64 {
        self.outcomes.get(class)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplacementHistogram {
    pub values: BTreeMap<i32, u64>,
}

impl DisplacementHistogram {
    pub(crate) fn add(&mut self, value: i32, count: u64) {
        *self.values.entry(value).or_default() += count;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Nearest-rank percentile over the exact distribution (the histogram keeps
    /// every distinct displacement, so this needs no interpolation).
    pub(crate) fn percentile(&self, pct: usize) -> i32 {
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

    pub(crate) fn max(&self) -> i32 {
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

/// Engine instrumentation: how much work each pass avoided. The three
/// `*_equal_*` pairs are the point of the tiered design - the raw and canonical
/// tiers should absorb nearly everything on a near-identical archive pair, and
/// `detailed_pairs` staying small is what keeps a full-archive diff affordable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegressCounters {
    pub addressed_tiles: u64,
    pub addressed_current: u64,
    pub addressed_baseline: u64,
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
    /// Full differing-tile count, kept as a number because the per-tile records
    /// it replaced were unbounded.
    pub diff_count: u64,
    /// Coalesced ranges retain enough tile-location context without a record per
    /// tile.
    pub differing_ranges: Vec<TileRange>,
    pub totals: DiffTotals,
    pub per_zoom_layer: BTreeMap<(u8, Arc<str>), LayerCounters>,
    pub displacement: BTreeMap<(u8, Arc<str>), DisplacementHistogram>,
    pub examples: Vec<DiffExample>,
    pub counters: RegressCounters,
}

impl RegressReport {
    /// The verdict. Every class must be zero except `tolerance_moved`, which is
    /// allowed up to the configured budget.
    #[must_use]
    pub fn passed(&self, cfg: &RegressConfig) -> bool {
        self.totals.only_in_current == 0
            && self.totals.only_in_baseline == 0
            && OutcomeClass::ALL.iter().all(|class| {
                let count = self.totals.count(*class);
                if *class == OutcomeClass::ToleranceMoved {
                    count <= cfg.max_moved
                } else {
                    count == 0
                }
            })
    }

    pub fn print_text(&self) {
        output::plain(&format!(
            "identical_tiles={} diffs={} only_current={} only_baseline={} tolerance_moved={} structural_moved={} attr_changed={}",
            self.identical_tiles,
            self.diff_count,
            self.totals.only_in_current,
            self.totals.only_in_baseline,
            self.totals.count(OutcomeClass::ToleranceMoved),
            self.totals.count(OutcomeClass::StructuralMoved),
            self.totals.count(OutcomeClass::AttrChanged)
        ));
        output::plain(&format!(
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
        ));

        for ((z, layer), c) in &self.per_zoom_layer {
            if counters_are_zero(c) {
                continue;
            }
            let counts = OutcomeClass::ALL
                .iter()
                .map(|class| format!(" {}={}", class.text_label(), c.count(*class)))
                .collect::<String>();
            output::plain(&format!("z{z} {layer}{counts}"));
        }

        for ((z, layer), values) in &self.displacement {
            if values.is_empty() {
                continue;
            }
            output::plain(&format!(
                "z{z} {layer} displacement p50={} p95={} max={}",
                values.percentile(50),
                values.percentile(95),
                values.max()
            ));
        }

        for ex in &self.examples {
            let (z, x, y) = tile_id_to_zxy(ex.tile_id);
            let id = ex
                .id
                .map_or_else(|| "anon".to_string(), |id| id.to_string());
            output::plain(&format!(
                "z{z}/{x}/{y} {} {} {}",
                ex.layer.as_ref(),
                id,
                ex.class
            ));
        }
    }

    #[must_use]
    pub fn to_json(&self, passed: bool) -> serde_json::Value {
        let per_zoom_layer: Vec<_> = self
            .per_zoom_layer
            .iter()
            .map(|((z, layer), c)| {
                let mut fields = serde_json::Map::new();
                fields.insert("z".into(), json!(z));
                fields.insert("layer".into(), json!(layer.as_ref()));
                insert_outcome_counts(&mut fields, &c.outcomes);
                serde_json::Value::Object(fields)
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

        let mut totals = serde_json::Map::new();
        totals.insert("only_in_current".into(), json!(self.totals.only_in_current));
        totals.insert(
            "only_in_baseline".into(),
            json!(self.totals.only_in_baseline),
        );
        insert_outcome_counts(&mut totals, &self.totals.outcomes);

        json!({
            "passed": passed,
            "identical_tiles": self.identical_tiles,
            "diffs": self.diff_count,
            "totals": serde_json::Value::Object(totals),
            "per_zoom_layer": per_zoom_layer,
            "displacement": displacement,
            "examples": examples,
            "counters": {
                "addressed_tiles": self.counters.addressed_tiles,
                "addressed_current": self.counters.addressed_current,
                "addressed_baseline": self.counters.addressed_baseline,
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
}

fn insert_outcome_counts(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    counts: &OutcomeCounts,
) {
    for class in OutcomeClass::ALL {
        fields.insert(class.field_name().to_owned(), json!(counts.get(class)));
    }
}

pub(crate) fn counters_are_zero(counters: &LayerCounters) -> bool {
    counters.outcomes.is_zero()
}

// ---------------------------------------------------------------------------
// Bounded example selection
// ---------------------------------------------------------------------------

/// Layer names repeat across every tile of an archive; interning them keeps the
/// report's maps holding one `Arc` per name instead of one per event.
#[derive(Default)]
pub(crate) struct LayerInterner {
    names: rustc_hash::FxHashMap<Arc<str>, Arc<str>>,
}

impl LayerInterner {
    pub(crate) fn intern(&mut self, name: &Arc<str>) -> Arc<str> {
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
pub(crate) struct ExampleKey {
    pub(crate) tile_id: u64,
    pub(crate) layer: Arc<str>,
    pub(crate) id: Option<u64>,
}

/// Keeps at most `cap` example candidates per outcome class, always the
/// smallest keys seen so far. This bounds report memory on broad-difference
/// runs where the raw candidate stream is millions of events.
pub(crate) struct ExampleSelector {
    pub(crate) cap: usize,
    per_class: BTreeMap<OutcomeClass, BinaryHeap<ExampleKey>>,
}

impl ExampleSelector {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            per_class: BTreeMap::new(),
        }
    }

    /// A max-heap of the smallest `cap` keys: the root is the worst kept
    /// candidate, so one comparison decides whether a new key displaces it.
    pub(crate) fn offer(&mut self, class: OutcomeClass, key: ExampleKey) {
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

    pub(crate) fn finish(self) -> Vec<DiffExample> {
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
