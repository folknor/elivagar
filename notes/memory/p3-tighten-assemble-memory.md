# P3: Tighten Assemble Phase Memory Behavior

## Status: IMPLEMENTED

| Change | Commit | Notes |
|--------|--------|-------|
| 1. Eliminate `compressed.clone()` | `c88cfc0` (P1) | Done as part of P1 byte-budgeted in-flight controls |
| 2. Reuse MVT encode buffer | `2442343` | `encode_tile_into()` writes to caller-provided `&mut Vec<u8>` via `AssemblyScratch.mvt_buf` |
| 3. Adaptive byte-budget batching | `c88cfc0` (P1) | Done as part of P1 assemble batch budget (32 MB cap) |
| 4. `should_emit` flag optimization | `2442343` | Replaced closure scan with incremental `has_non_ocean` bool |

Denmark (dm6, `2442343`): peak RSS 2019 → 1907 MB (-5.5%). Output byte-identical.
Germany (dm6, `2442343`): stable, no regression.

## Detailed Analysis and Implementation Plan

### 1. Current Architecture Summary

The assemble phase (`phase_assemble`, `/home/folk/Programs/elivagar/src/pipeline.rs:1481`) uses a 3-stage pipeline:

**Stage 1 -- Reader thread (lines 1508-1549):**
- Pulls `SortRecord` from k-way merge (`sort_reader.next()`)
- Groups records by `tile_id` into `PendingTile { tile_id: u64, features: Vec<(u8, Vec<u8>)> }`
- Accumulates tiles into a batch `Vec<PendingTile>` of up to `BATCH_SIZE = 4096`
- Sends full batches through `sync_channel(1)` to the encoder

**Stage 2 -- Main thread + rayon (lines 1580-1586):**
- Receives batches from reader
- Calls `encode_tile_batch(&batch, compression_level)` which uses `par_iter()` with `thread_local!` scratch state
- Per tile: decode wire format -> populate LayerBuilders -> merge geometries -> MVT encode -> gzip compress
- Sends `Vec<EncodedTile>` through `sync_channel(1)` to writer

**Stage 3 -- Writer thread (lines 1554-1578):**
- Receives encoded batches
- Calls `pmtiles.add_tile(z, x, y, &tile.compressed)` for each tile

### 2. Data Structures and Their Sizes

**PendingTile** (32 bytes struct, line 1466):
```
tile_id: u64           // 8 bytes
features: Vec<(u8, Vec<u8>)>  // 24 bytes (Vec header)
```
The actual feature payload is in the inner `Vec<u8>` elements (owned, moved from sort reader).

**EncodedTile** (32 bytes struct, line 1473):
```
tile_id: u64           // 8 bytes
compressed: Vec<u8>    // 24 bytes (Vec header)
```
The compressed tile data is typically 5-12 KB.

**Feature** (72 bytes struct, mvt.rs line 84):
```
id: Option<u64>       // 16 bytes
geom_type: GeomType   // 1 byte + padding
geometry: Vec<u32>    // 24 bytes
tags: Vec<(u16, u16)> // 24 bytes
```

### 3. Batch Lifecycle and Memory Analysis

**A. Batch allocation (reader thread, line 1510):**
`Vec::with_capacity(BATCH_SIZE)` = 4096 * 32 bytes = 128 KB for the outer Vec. Plus each `PendingTile.features` is a fresh `Vec::new()` (line 1545) that grows as features are pushed (line 1547).

**B. Batch fill (reader thread):**
Each feature's `data: Vec<u8>` is moved from `SortRecord.data` (zero-copy from sort reader). The feature data Vecs are owned by the PendingTile. For Denmark (16M features, 54K tiles), average is ~296 features/tile. With an average sort record payload of ~60-100 bytes, a typical batch of 4096 tiles holds ~4096 * 296 * 80 bytes = ~97 MB of feature data.

**C. Batch encode (rayon, lines 1644-1714):**
For each tile, the rayon worker:
1. Decodes each feature's wire-format `Vec<u8>` into `Feature { geometry: Vec<u32>, tags: Vec<(u16, u16)> }` via `add_feature_to_layer` (pooled Vecs)
2. Runs `merge_same_attr_geometries` (reduces feature count by ~97%)
3. Calls `mvt::encode_tile_with` -- allocates a fresh `Vec<u8>` with capacity 65536 (line 283 of mvt.rs), fills it with protobuf
4. Gzip compresses: takes `s.gz_buf` via `mem::take`, wraps in `GzEncoder`, finishes, then **clones** the result (line 1708)
5. Returns `EncodedTile { tile_id, compressed }` -- the clone goes into the output, the original stays in scratch

**D. Batch write (writer thread):**
Each `EncodedTile.compressed` is passed by reference to `pmtiles.add_tile()`, then dropped when the batch Vec is dropped.

### 4. In-Flight Batch Analysis

With `sync_channel(1)`, the maximum in-flight state is:

| Location | Contents | Typical Size | Worst Case |
|---|---|---|---|
| Reader: building next batch | Up to 4096 PendingTiles | ~100 MB | ~600 MB |
| read_tx channel buffer | 1 complete PendingTile batch | ~100 MB | ~600 MB |
| Encoder: rayon processing | 1 PendingTile batch + intermediate state | ~150 MB | ~700 MB |
| encode_tx channel buffer | 1 complete EncodedTile batch | ~20 MB | ~50 MB |
| Writer: processing | 1 EncodedTile batch | ~20 MB | ~50 MB |

**Total in-flight: ~390 MB typical, up to ~2 GB worst case.**

The worst case occurs when 4096 consecutive tiles in Hilbert order are all dense urban z14 tiles (e.g., Berlin, Tokyo). Since Hilbert order clusters spatially, this scenario IS realistic for planet-scale runs.

### 5. Identified Inefficiencies

#### 5.1 `compressed.clone()` (pipeline.rs line 1708) -- HIGH

```rust
let compressed = encoder.finish().expect("gzip finish failed");
s.gz_buf = compressed.clone();
```

Every compressed tile is cloned to preserve the scratch buffer's capacity. For Denmark (54K tiles at ~5 KB avg), this is ~270 MB of unnecessary copies. For planet (~100M tiles at ~8 KB avg), this would be ~800 GB of allocator churn.

**Fix:** Instead of cloning, give the compressed Vec to the EncodedTile and create a new gz_buf for scratch:

```rust
let compressed = encoder.finish().expect("gzip finish failed");
s.gz_buf = Vec::with_capacity(compressed.len());
Some(EncodedTile { tile_id: tile.tile_id, compressed })
```

This eliminates the clone at the cost of a small allocation for the new gz_buf (which will reuse mimalloc's thread-local free list, so it is nearly free).

#### 5.2 Fresh `Vec<u8>` per tile in `encode_tile_with` (mvt.rs line 283) -- MEDIUM

```rust
pub fn encode_tile_with(layers: &[&LayerBuilder], scratch: &mut EncodeScratch) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 << 16);
    ...
    buf
}
```

A 64 KB allocation per tile, never reused. For 100M tiles at planet scale, that is 6.4 TB of allocator throughput for just the initial allocations (many tiles will exceed 64 KB and trigger further reallocs).

**Fix:** Add an `mvt_buf: Vec<u8>` field to `AssemblyScratch`. Change `encode_tile_with` to accept `&mut Vec<u8>` as an output parameter. The signature would become:

```rust
pub fn encode_tile_into(buf: &mut Vec<u8>, layers: &[&LayerBuilder], scratch: &mut EncodeScratch) {
    buf.clear();
    for layer in layers { ... }
}
```

The caller would take the buf from scratch, encode into it, then compress from it, then put it back. After a few tiles, the buffer is warm at ~65 KB and no further allocations occur.

#### 5.3 Fresh `PendingTile.features` Vec per tile in reader (pipeline.rs line 1545) -- LOW-MEDIUM

```rust
current = PendingTile { tile_id, features: Vec::new() };
```

Every new tile creates a fresh `Vec<(u8, Vec<u8>)>` that grows as features are pushed. For tiles with 296 features on average, this means ~4-5 Vec doublings per tile. At planet scale with 100M+ tiles from the merge, this is significant allocator churn.

**Revised assessment:** This is LOW priority. The features Vecs hold sort record payloads that are moved (not copied), so the allocation is just the Vec header doublings. With mimalloc, this is efficient.

#### 5.4 Fresh batch Vec never reused (pipeline.rs line 1542) -- LOW

```rust
batch = Vec::with_capacity(BATCH_SIZE);
```

After sending a batch, a fresh 128 KB Vec is allocated. The old Vec is moved into the channel. At planet scale with ~24K batches, this is ~24K * 128 KB = 3 GB of Vec allocations. Minor compared to feature data.

#### 5.5 Fixed batch count of 4096 regardless of tile complexity -- MEDIUM (architectural)

The core concern: a batch of 4096 dense urban z14 tiles uses far more memory than 4096 sparse ocean z8 tiles. Since tiles arrive in zoom order (z0 first, z14 last), the early batches contain very few tiles (z0-z8 has only ~87K tiles total), while z14 alone has ~86% of all tiles.

**Current behavior:**
- z0-z8 batches: few tiles, each potentially large (many features). Small batch count but high per-tile cost.
- z13-z14 batches: many tiles, typically smaller. Large batch count with moderate per-tile cost.
- Pathological case: a batch of 4096 z14 tiles over a dense urban area, each with 500-5000 features.

**Byte-budget approach:** Instead of `BATCH_SIZE = 4096`, track cumulative feature data bytes as features are pushed. When the byte count exceeds a threshold (e.g., `BATCH_BYTE_BUDGET = 128 * 1024 * 1024`), send the batch regardless of tile count.

### 6. Concrete Action Plan

#### Change 1: Eliminate `compressed.clone()` -- HIGH priority, LOW risk

**File:** `/home/folk/Programs/elivagar/src/pipeline.rs`, line 1708

**Current code:**
```rust
let compressed = encoder.finish().expect("gzip finish failed");
s.gz_buf = compressed.clone();
Some(EncodedTile { tile_id: tile.tile_id, compressed })
```

**New code:**
```rust
let compressed = encoder.finish().expect("gzip finish failed");
s.gz_buf = Vec::with_capacity(compressed.len());
Some(EncodedTile { tile_id: tile.tile_id, compressed })
```

**Rationale:** The scratch `gz_buf` exists to avoid allocating a fresh Vec each time. By giving the compressed Vec directly to the EncodedTile (already happening), we just need a new Vec for the next tile. `Vec::with_capacity(compressed.len())` gives a correctly-sized buffer (the next tile's compressed size will be similar due to spatial locality). mimalloc's thread-local free list will likely satisfy this from the just-freed compressed clone memory, making it nearly zero-cost.

**Expected savings:** Eliminates one full copy of every compressed tile. At planet scale: ~800 GB of memcpy eliminated. Wall-clock impact: potentially measurable (0.5-2% of assemble time).

**Risk:** Minimal. The only behavioral change is that `s.gz_buf` starts with capacity equal to the previous tile's compressed size instead of containing the previous tile's data. Since `gz_buf.clear()` is called before each use (line 1703), the content does not matter.

#### Change 2: Reuse MVT encode buffer -- MEDIUM priority, LOW risk

**Files:**
- `/home/folk/Programs/elivagar/src/pipeline.rs` (AssemblyScratch struct + encode_tile_batch)
- `/home/folk/Programs/elivagar/src/mvt.rs` (encode_tile_with signature)

**Step 2a:** Add `mvt_buf: Vec<u8>` to `AssemblyScratch` (line 1614-1622):
```rust
struct AssemblyScratch {
    encode_scratch: mvt::EncodeScratch,
    merge_scratch: mvt::MergeScratch,
    geom_pool: Vec<Vec<u32>>,
    tags_pool: Vec<Vec<(u16, u16)>>,
    compression_levels: [Option<flate2::Compression>; 11],
    gz_buf: Vec<u8>,
    mvt_buf: Vec<u8>,   // <-- NEW
    layers: [Option<LayerBuilder>; LAYER_COUNT],
}
```

**Step 2b:** Add `encode_tile_into` to mvt.rs (alternative to `encode_tile_with`):
```rust
#[hotpath::measure]
pub fn encode_tile_into(buf: &mut Vec<u8>, layers: &[&LayerBuilder], scratch: &mut EncodeScratch) {
    buf.clear();
    for layer in layers {
        if !layer.is_empty() {
            layer.encode(buf, scratch);
        }
    }
}
```

**Step 2c:** Update `encode_tile_batch` (line 1684) to use the new function:
```rust
let mvt_buf = &mut s.mvt_buf;
mvt::encode_tile_into(mvt_buf, &non_empty, &mut s.encode_scratch);
if mvt_buf.is_empty() {
    return None;
}
// ... compress from mvt_buf ...
```

**Expected savings:** Eliminates one 64 KB allocation per tile. At planet scale: ~6.4 TB of initial allocations eliminated, plus all reallocation churn. After the first few tiles, the buffer stabilizes at peak tile size (warm buffer). Wall-clock: potentially 1-3% of assemble time.

**Risk:** Very low. The MVT data is consumed immediately by compression; it does not need to outlive the encoding call. The only change is that the buffer is reused instead of allocated fresh.

#### Change 3: Adaptive byte-budget batching -- MEDIUM priority, MEDIUM risk

**File:** `/home/folk/Programs/elivagar/src/pipeline.rs`, reader thread (lines 1508-1549)

**Approach:** Replace the fixed `BATCH_SIZE = 4096` check with a dual limit: a tile count cap AND a byte budget:

```rust
const MAX_BATCH_TILES: usize = 8192;  // upper bound for rayon granularity
const BATCH_BYTE_BUDGET: usize = 128 * 1024 * 1024;  // 128 MB of feature data

// Track cumulative bytes in the reader:
let mut batch_bytes: usize = 0;

// On tile boundary, when adding to batch:
batch_bytes += current.features.iter().map(|(_, d)| d.len()).sum::<usize>();
batch.push(current);
if batch.len() >= MAX_BATCH_TILES || batch_bytes >= BATCH_BYTE_BUDGET {
    if read_tx.send(batch).is_err() { break; }
    batch = Vec::with_capacity(MAX_BATCH_TILES.min(4096));
    batch_bytes = 0;
}
```

**Expected behavior:**
- For sparse tiles (most of z14): batches will reach `MAX_BATCH_TILES` before byte budget, giving good rayon utilization.
- For dense tiles (urban z14, low-zoom overview tiles): byte budget triggers first, limiting in-flight memory.
- For z0-z8 tiles: few tiles but potentially large, so byte budget provides protection.

**Expected savings:** Caps peak in-flight memory from ~2 GB worst case to ~3 * 128 MB = ~384 MB for PendingTile data. More predictable memory behavior.

**Risk:** MEDIUM.
- Smaller batches mean more frequent rayon dispatch, potentially more scheduling overhead. Mitigation: `MAX_BATCH_TILES = 8192` ensures batches are never tiny for sparse tiles.
- The byte budget of 128 MB is a heuristic; it may need tuning based on planet-scale profiling.

**Alternative (simpler):** Instead of byte-budget batching, just reduce `BATCH_SIZE` to 1024. This would reduce worst-case batch size by 4x while still providing enough tiles for rayon. For Denmark (54K tiles), this gives ~54 batches instead of ~13. For planet (~100M tiles), ~97K batches instead of ~24K. The extra batch overhead is negligible. This is lower risk but less adaptive.

#### Change 4: Reduce `should_emit` scan cost with early flag -- LOW priority, VERY LOW risk

**File:** `/home/folk/Programs/elivagar/src/pipeline.rs`, reader thread

**Current (line 1517-1519):**
```rust
let should_emit = |tile: &PendingTile| -> bool {
    tile.features.iter().any(|(layer, _)| *layer != ocean_idx)
};
```

**Fix:** Track a `has_non_ocean: bool` flag during feature accumulation:
```rust
let mut has_non_ocean: bool = false;
// ...
if layer_idx != ocean_idx { has_non_ocean = true; }
current.features.push((layer_idx, r.data));
// ...
// At tile boundary:
if current.tile_id != u64::MAX && has_non_ocean {
    batch.push(current);
    // ...
}
has_non_ocean = false;
```

**Expected savings:** Eliminates the `should_emit` scan of all features. For ocean-only tiles at planet scale with ~millions of tiles, each with ~10-50 features, this saves a scan of ~100M feature tuples. Wall-clock: negligible (< 0.01% of assemble time).

**Risk:** Very low. Simple boolean flag replacement.

### 7. Prioritized Implementation Order

| Priority | Change | Expected Savings (Planet) | Effort | Risk |
|---|---|---|---|---|
| 1 | Eliminate `compressed.clone()` | ~800 GB allocator churn | 10 minutes | Very low |
| 2 | Reuse MVT encode buffer | ~6.4 TB allocations | 30 minutes | Low |
| 3 | Adaptive byte-budget batching (or simpler: reduce batch size to 1024) | Caps peak in-flight from ~2 GB to ~400 MB | 1-2 hours | Medium |
| 4 | `should_emit` flag optimization | Negligible | 10 minutes | Very low |

### 8. What NOT to Change

- **`sync_channel(1)` depth:** Changing to `sync_channel(2)` would add one more in-flight batch. The current design already has good pipeline overlap. The bottleneck is rayon encode+compress CPU, not channel depth. Adding depth would increase peak memory by one batch (~100-600 MB) for marginal throughput gain. Not worth it.

- **PendingTile.features Vec reuse in reader:** The feature Vec grows incrementally and holds moved sort record data. The allocation overhead is just the Vec doublings, which mimalloc handles efficiently. Pooling complexity is not justified.

- **EncodedTile compressed Vec pooling via return channel:** Would require a second channel from writer back to encoder. The `compressed` Vec is small (~5-12 KB avg), and mimalloc recycles these efficiently. Not worth the architectural complexity.

- **Pass-through encoding for Point-only tiles:** The decode-encode roundtrip is still needed to produce valid MVT protobuf. Both are cheap for Points. Not worth the special case.

### 9. Memory Budget Summary (Planet-Scale Projection)

**Before changes (current code):**
- PendingTile batches (3 in-flight): ~300 MB typical, ~2 GB worst case
- Per-worker rayon scratch (24 workers): ~24 MB
- Per-tile transient: 64 KB MVT buf + 5-12 KB compressed clone = ~70 KB * 24 workers = ~1.7 MB
- PMTiles dedup: ~50 MB
- Sort reader buffers: ~20 MB
- **Total assemble RSS: ~400 MB typical, ~2.1 GB worst case**

**After changes 1+2+3:**
- PendingTile batches (3 in-flight, byte-budgeted): ~300 MB typical, ~400 MB worst case
- Per-worker rayon scratch (24 workers + reused MVT buf): ~26 MB
- Per-tile transient: ~0 (both buffers reused)
- PMTiles dedup: ~50 MB
- Sort reader buffers: ~20 MB
- **Total assemble RSS: ~400 MB typical, ~500 MB worst case**

The main win is eliminating the worst-case spike from ~2 GB to ~500 MB, plus eliminating ~7 TB of allocator throughput at planet scale.

### Critical Files

- `/home/folk/Programs/elivagar/src/pipeline.rs` - Core changes: AssemblyScratch struct, encode_tile_batch (clone elimination + MVT buffer reuse), reader thread (byte-budget batching)
- `/home/folk/Programs/elivagar/src/mvt.rs` - Add `encode_tile_into` function that writes to a provided buffer instead of allocating
- `/home/folk/Programs/elivagar/src/wire_format.rs` - Context for understanding feature decode path (no changes needed)
- `/home/folk/Programs/elivagar/src/pmtiles_writer.rs` - Context for understanding writer memory footprint (no changes needed)
