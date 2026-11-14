use std::sync::atomic::{AtomicU64, Ordering};

use smallvec::SmallVec;

use crate::sort::{self, LayerZoom, PerLayerZoom};

pub(super) const TILE_OVERSIZE_WARN_BYTES: u64 = 500 * 1024;
pub(super) const TILE_OVERSIZE_SEVERE_BYTES: u64 = 1024 * 1024;
pub(super) const TILE_OVERSIZE_TOP_N: usize = 10;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MissingRefStats {
    pub(super) missing_way_node_refs: u64,
    pub(super) ways_with_missing_node_refs: u64,
    pub(super) missing_relation_way_refs: u64,
    pub(super) relations_with_missing_way_refs: u64,
    pub(super) relation_non_way_members: u64,
    pub(super) relation_nested_members: u64,
}

#[derive(Debug, Default)]
pub(super) struct MissingRefStatsAtomic {
    pub(super) missing_way_node_refs: AtomicU64,
    pub(super) ways_with_missing_node_refs: AtomicU64,
    pub(super) missing_relation_way_refs: AtomicU64,
    pub(super) relations_with_missing_way_refs: AtomicU64,
    pub(super) relation_non_way_members: AtomicU64,
    pub(super) relation_nested_members: AtomicU64,
}

impl MissingRefStatsAtomic {
    pub(super) fn record_way_missing_nodes(&self, missing_refs: usize) {
        self.missing_way_node_refs
            .fetch_add(missing_refs as u64, Ordering::Relaxed);
        self.ways_with_missing_node_refs
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_relation_missing_way_ref(&self) {
        self.missing_relation_way_refs
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_relation_with_missing_way_refs(&self) {
        self.relations_with_missing_way_refs
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_relation_non_way_member(&self) {
        self.relation_non_way_members
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_relation_nested_member(&self) {
        self.relation_nested_members.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> MissingRefStats {
        MissingRefStats {
            missing_way_node_refs: self.missing_way_node_refs.load(Ordering::Relaxed),
            ways_with_missing_node_refs: self.ways_with_missing_node_refs.load(Ordering::Relaxed),
            missing_relation_way_refs: self.missing_relation_way_refs.load(Ordering::Relaxed),
            relations_with_missing_way_refs: self
                .relations_with_missing_way_refs
                .load(Ordering::Relaxed),
            relation_non_way_members: self.relation_non_way_members.load(Ordering::Relaxed),
            relation_nested_members: self.relation_nested_members.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
pub(super) fn missing_ref_summary_lines(summary: MissingRefStats) -> [String; 6] {
    [
        format!("missing_way_node_refs={}", summary.missing_way_node_refs),
        format!(
            "ways_with_missing_node_refs={}",
            summary.ways_with_missing_node_refs
        ),
        format!(
            "missing_relation_way_refs={}",
            summary.missing_relation_way_refs
        ),
        format!(
            "relations_with_missing_way_refs={}",
            summary.relations_with_missing_way_refs
        ),
        format!(
            "relation_non_way_members={}",
            summary.relation_non_way_members
        ),
        format!(
            "relation_nested_members={}",
            summary.relation_nested_members
        ),
    ]
}

/// Phase12 statistics returned alongside the sort writer.
pub(super) struct Phase12Stats {
    /// The paths this run actually took, captured where they are chosen
    /// rather than re-derived downstream. Assemble writes them into archive
    /// provenance, and a re-derivation there could disagree with what
    /// actually ran.
    pub(super) effective: crate::provenance::Effective,
    pub(super) node_count: u64,
    pub(super) way_count: u64,
    pub(super) rel_count: u64,
    pub(super) node_store_stats: Option<(u64, usize)>,
    pub(super) max_way_inflight_bytes: usize,
    pub(super) max_rel_inflight_bytes: usize,
    pub(super) relation_blocks_buffered: usize,
    pub(super) relation_blocks_bytes: usize,
    pub(super) relation_blocks_spilled: bool,
    pub(super) relation_plan_needed_ways: usize,
    pub(super) relation_plan_superset_ways: usize,
    pub(super) way_members_marked: u64,
    pub(super) way_pins_marked: u64,
    pub(super) relation_blocks_drop_rss_kb: Option<u64>,
    pub(super) missing_refs: MissingRefStats,
    pub(super) sort_records: u64,
    pub(super) sort_record_bytes: u64,
    pub(super) layer_records: [u64; crate::shortbread::Layer::count()],
    pub(super) layer_bytes: [u64; crate::shortbread::Layer::count()],
    pub(super) layer_zoom_records: PerLayerZoom<u64>,
    pub(super) layer_zoom_bytes: PerLayerZoom<u64>,
    pub(super) fanout_stats: FanoutStats,
}

// ---------------------------------------------------------------------------
// Fanout distribution stats - tiles touched per feature per (layer, zoom)
// ---------------------------------------------------------------------------

/// Number of log2 histogram buckets for tiles-touched distribution.
/// Bucket 0: 1 tile, 1: 2, 2: 3-4, 3: 5-8, ..., 11: 1025+.
pub(super) const FANOUT_HIST_BUCKETS: usize = 12;

/// Per-(layer, zoom) tiles-touched distribution. Accumulated sequentially
/// (drain thread for ways, reduce for relations).
pub(super) struct FanoutStats {
    /// Max tiles touched by any single feature.
    pub(super) max_tiles: PerLayerZoom<u32>,
    /// Total features that emitted at least one record.
    pub(super) feature_count: PerLayerZoom<u64>,
    /// Sum of tiles touched (for mean).
    pub(super) sum_tiles: PerLayerZoom<u64>,
    /// Log2 histogram for each layer and zoom.
    pub(super) hist: PerLayerZoom<[u32; FANOUT_HIST_BUCKETS]>,
    /// Features capped (skipped at this zoom due to fanout cap).
    pub(super) capped_features: PerLayerZoom<u64>,
    /// Estimated tiles saved by capping (sum of bbox tile counts).
    pub(super) capped_tiles: PerLayerZoom<u64>,
    /// Top capped features by bbox tile count.
    pub(super) top_capped: Vec<CapEvent>,
}

/// One feature skipped by its layer's fanout cap at one zoom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CapEvent {
    pub(super) at: LayerZoom,
    /// Tiles the feature's bbox covers at that zoom.
    pub(super) bbox_tiles: u64,
    pub(super) osm_id: u64,
}

impl FanoutStats {
    pub(super) fn new() -> Self {
        Self {
            max_tiles: PerLayerZoom::default(),
            feature_count: PerLayerZoom::default(),
            sum_tiles: PerLayerZoom::default(),
            hist: PerLayerZoom::from_fn(|| [0; FANOUT_HIST_BUCKETS]),
            capped_features: PerLayerZoom::default(),
            capped_tiles: PerLayerZoom::default(),
            top_capped: Vec::new(),
        }
    }

    /// Record that a single feature touched `tiles` tiles at (layer, zoom).
    pub(super) fn record(&mut self, idx: LayerZoom, tiles: u32) {
        if tiles == 0 {
            return;
        }
        if tiles > self.max_tiles[idx] {
            self.max_tiles[idx] = tiles;
        }
        self.feature_count[idx] += 1;
        self.sum_tiles[idx] += u64::from(tiles);
        // Log2 bucket: 0→0, 1→1, 2..3→2, 4..7→3, ...
        let bucket = if tiles <= 2 {
            (tiles - 1) as usize
        } else {
            // floor(log2(tiles)) + 1, capped at FANOUT_HIST_BUCKETS - 1
            let b = (u32::BITS - (tiles - 1).leading_zeros()) as usize;
            b.min(FANOUT_HIST_BUCKETS - 1)
        };
        self.hist[idx][bucket] += 1;
    }

    /// Record that a feature was capped (skipped) at (layer, zoom) with bbox tile count.
    pub(super) fn record_cap(&mut self, event: CapEvent) {
        let CapEvent { at, bbox_tiles, .. } = event;
        self.capped_features[at] += 1;
        self.capped_tiles[at] += bbox_tiles;
        // Track top 10 capped features by bbox tile count.
        const TOP_N: usize = 10;
        if self.top_capped.len() < TOP_N
            || bbox_tiles > self.top_capped.last().map_or(0, |e| e.bbox_tiles)
        {
            self.top_capped.push(event);
            self.top_capped
                .sort_unstable_by_key(|e| std::cmp::Reverse(e.bbox_tiles));
            self.top_capped.truncate(TOP_N);
        }
    }

    /// Merge another FanoutStats into self.
    pub(super) fn merge(&mut self, other: &Self) {
        for (left, right) in self.max_tiles.iter_mut().zip(other.max_tiles.iter()) {
            if *right > *left {
                *left = *right;
            }
        }
        for (left, right) in self
            .feature_count
            .iter_mut()
            .zip(other.feature_count.iter())
        {
            *left += *right;
        }
        for (left, right) in self.sum_tiles.iter_mut().zip(other.sum_tiles.iter()) {
            *left += *right;
        }
        for (left, right) in self
            .capped_features
            .iter_mut()
            .zip(other.capped_features.iter())
        {
            *left += *right;
        }
        for (left, right) in self.capped_tiles.iter_mut().zip(other.capped_tiles.iter()) {
            *left += *right;
        }
        for (left, right) in self.hist.iter_mut().zip(other.hist.iter()) {
            for (left_bucket, right_bucket) in left.iter_mut().zip(right.iter()) {
                *left_bucket += *right_bucket;
            }
        }
        // Merge top capped: combine, sort, truncate.
        self.top_capped.extend_from_slice(&other.top_capped);
        self.top_capped
            .sort_unstable_by_key(|e| std::cmp::Reverse(e.bbox_tiles));
        self.top_capped.truncate(10);
    }

    /// Compute a percentile (0.0-1.0) from the histogram for a given (layer, zoom).
    /// Returns the upper bound of the bucket containing the percentile,
    /// capped at max_tiles to avoid reporting a percentile above the actual max.
    pub(super) fn percentile(&self, idx: LayerZoom, p: f64) -> u32 {
        let total = self.feature_count[idx];
        if total == 0 {
            return 0;
        }
        let max = self.max_tiles[idx];
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target = (total as f64 * p).ceil() as u64;
        let mut cumulative: u64 = 0;
        for (b, &n) in self.hist[idx].iter().enumerate().take(FANOUT_HIST_BUCKETS) {
            cumulative += u64::from(n);
            if cumulative >= target {
                let upper = match b {
                    0 => 1,
                    1 => 2,
                    _ if b >= FANOUT_HIST_BUCKETS - 1 => max,
                    _ => 1u32 << b,
                };
                return upper.min(max);
            }
        }
        max
    }

    /// Count features with tiles_touched >= threshold for a given (layer, zoom).
    pub(super) fn features_above(&self, idx: LayerZoom, threshold: u32) -> u64 {
        if self.feature_count[idx] == 0 {
            return 0;
        }
        // Find the first bucket that contains values >= threshold.
        let start_bucket = if threshold <= 1 {
            0
        } else if threshold <= 2 {
            1
        } else {
            // Bucket b covers [2^(b-1)+1, 2^b] for b>=2. We want the bucket
            // where the lower bound >= threshold.
            (u32::BITS - (threshold - 1).leading_zeros()) as usize
        };
        self.hist[idx]
            .iter()
            .take(FANOUT_HIST_BUCKETS)
            .skip(start_bucket)
            .map(|&n| u64::from(n))
            .sum()
    }
}

/// Extract tiles-touched per (layer, zoom) from a set of payload sort records
/// belonging to a single feature and record into FanoutStats.
pub(super) fn record_fanout_from_payload_records(
    records: &[sort::PayloadRecord],
    stats: &mut FanoutStats,
) {
    if records.is_empty() {
        return;
    }
    let mut counts: SmallVec<[(LayerZoom, u32); 8]> = SmallVec::new();
    for &(key, _, _) in records {
        let Some(layer_zoom) = LayerZoom::of_key(key) else {
            continue;
        };
        if let Some(entry) = counts.iter_mut().find(|e| e.0 == layer_zoom) {
            entry.1 += 1;
        } else {
            counts.push((layer_zoom, 1));
        }
    }
    for &(at, tiles) in &counts {
        stats.record(at, tiles);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct OversizeTile {
    pub(super) tile_id: u64,
    pub(super) bytes: u64,
}

#[derive(Debug, Default)]
pub(super) struct TileSizeDiagnostics {
    pub(super) total_tile_bytes: u64,
    pub(super) max_tile: OversizeTile,
    pub(super) oversize_warn_count: u64,
    pub(super) oversize_severe_count: u64,
    pub(super) top_oversized: [OversizeTile; TILE_OVERSIZE_TOP_N],
}

pub(super) fn insert_top_oversized(
    top: &mut [OversizeTile; TILE_OVERSIZE_TOP_N],
    tile: OversizeTile,
) {
    let mut pos = None;
    for (i, t) in top.iter().enumerate() {
        if tile.bytes > t.bytes {
            pos = Some(i);
            break;
        }
    }
    let Some(i) = pos else { return };
    for j in (i + 1..top.len()).rev() {
        top[j] = top[j - 1];
    }
    top[i] = tile;
}

pub(super) fn record_tile_size_diagnostics(
    size_diag: &mut TileSizeDiagnostics,
    tile_id: u64,
    tile_bytes: u64,
) {
    size_diag.total_tile_bytes += tile_bytes;
    if tile_bytes > size_diag.max_tile.bytes {
        size_diag.max_tile = OversizeTile {
            tile_id,
            bytes: tile_bytes,
        };
    }
    if tile_bytes > TILE_OVERSIZE_WARN_BYTES {
        size_diag.oversize_warn_count += 1;
    }
    if tile_bytes > TILE_OVERSIZE_SEVERE_BYTES {
        size_diag.oversize_severe_count += 1;
    }
    insert_top_oversized(
        &mut size_diag.top_oversized,
        OversizeTile {
            tile_id,
            bytes: tile_bytes,
        },
    );
}
