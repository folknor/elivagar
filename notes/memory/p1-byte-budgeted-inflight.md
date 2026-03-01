# P1: Byte-Budgeted In-Flight Controls

## Detailed Implementation Plan

### 1. Current State Analysis

#### 1.1 Inventory of Count-Based Batch/Inflight Constants

There are exactly four count-based controls in the pipeline:

| Constant | Location | Value | Phase | What it controls |
|---|---|---|---|---|
| `MAX_INFLIGHT` | `pipeline.rs:489` | 4 | Way phase (phase12) | Number of PBF way-blocks in rayon pool simultaneously |
| `REL_BATCH_SIZE` | `pipeline.rs:854` | 1024 | Relation phase (phase12) | Number of `PreparedRelation` structs accumulated before parallel flush |
| `BATCH_SIZE` | `pipeline.rs:1496` | 4096 | Assemble phase (phase4) | Number of `PendingTile` structs accumulated before sending to encoder |
| `SORT_CHUNK_SIZE` | `pipeline.rs:117` | 1 GB | Sort writer (cross-phase) | Already byte-based; the only existing byte-budget |

Additionally, channel capacities act as implicit in-flight limits:
- `sync_channel::<PrimitiveBlock>(1)` at line 480 -- 1 block ahead on way dispatch
- `sync_channel::<Vec<ProcessedWay>>(4)` at line 481 -- 4 result batches ahead in drain
- `sync_channel::<Vec<PendingTile>>(1)` at line 1503 -- 1 PendingTile batch ahead
- `sync_channel::<Vec<EncodedTile>>(1)` at line 1504 -- 1 EncodedTile batch ahead

#### 1.2 Memory Variance Per Location

**Way phase (`MAX_INFLIGHT = 4`):**
- In-flight data per block: `Vec<RawWay>` (56 bytes/struct + `Vec<i64>` node_refs + `Vec<(String,String)>` tags)
- Per PBF block: typically ~8000 ways, comment estimates ~1.8 MB of RawWay data per block
- But this is just the *input*. Each `RawWay` produces a `ProcessedWay` (coords_e7 `Vec<(i32,i32)>` + records `Vec<SortRecord>`) simultaneously
- `ProcessedWay` records contain `Vec<u8>` wire format data (11 + geometry_cmds*4 + attrs bytes)
- Memory variance: a block of simple paths has ~8 node_refs and ~2 tags each. A block dominated by complex buildings or forests can have 50+ node_refs and 10+ tags. Ratio: ~10:1 between simple and complex blocks
- Worst case: 4 blocks * ~8000 ways * complex case: `Vec<i64>` (50 refs * 8 = 400 B) + tags (10 pairs * ~40 B = 400 B) + `ProcessedWay` (50 coords * 8 = 400 B + multiple SortRecords for multi-zoom) = ~10 MB/block, **40 MB total**. Plus the `Vec<ProcessedWay>` results channel holds up to 4 batches waiting for drain
- Realistic worst with results channel: `MAX_INFLIGHT(4)` in-processing + `sync_channel(4)` waiting + current block being extracted = up to **9 blocks' worth** of `ProcessedWay` results in memory

**Relation phase (`REL_BATCH_SIZE = 1024`):**
- Each `PreparedRelation` holds `member_ways: Vec<MemberWay>` where each `MemberWay` is 32 bytes struct + `coords: Vec<Point>` (16 bytes/point)
- Simple relations: 2-5 member ways, 10-50 points each = ~2-4 KB per relation
- Complex multipolygon relations (country borders, large forests): 200+ member ways, 100+ points each = **100-500 KB per relation**
- `flush_rel_batch` calls `batch.into_par_iter().map(...).collect::<Vec<Vec<SortRecord>>>()` -- this **materializes all outputs** before serial push
- Worst case: 1024 complex relations at 500 KB each (input) + output records = **500 MB input + potentially GB of output SortRecords**
- This is the most dangerous variance: a 250:1 ratio between a trivial and a monster relation batch

**Assemble phase (`BATCH_SIZE = 4096`):**
- `PendingTile.features: Vec<(u8, Vec<u8>)>` -- variable size per tile
- Sparse rural tiles: 1-5 features, ~50-200 bytes each = ~1 KB/tile
- Dense urban tiles (z14 in city centers): 100-500 features, 100-1000 bytes each = **50-500 KB/tile**
- Batch of 4096 tiles: sparse = ~4 MB; dense urban = **200 MB - 2 GB**
- The double-buffer means up to 2 batches in flight (one being read, one being encoded/written)
- Plus `Vec<EncodedTile>` for the encoded batch (compressed, typically smaller)
- Variance ratio: ~100:1 between sparse and dense batches
- Additional issue: `s.gz_buf = compressed.clone()` at line 1708 doubles the compressed buffer per rayon worker

### 2. Byte-Cost Estimation Strategy

Each location needs a cheap per-item byte estimate. Here is what to track at each point:

#### 2.1 Way Phase Byte Estimation
```
per_rawway_bytes = size_of::<RawWay>()   // 56
    + node_refs.len() * 8                 // Vec<i64> heap
    + tags.iter().sum(|t| t.0.len() + t.1.len() + 2*size_of::<String>())  // heap strings
per_block_bytes = sum(per_rawway_bytes for all ways in block)
```
This is computed *after* extracting raw_ways from the block (line 503-515), before spawning the rayon task. The cost is trivially amortized across the iteration that already exists.

For the output side (`ProcessedWay`), estimation is harder because we don't know until processing completes. But the input size is a good proxy: a way with N node_refs produces O(N * zoom_range) sort records. The factor is bounded by max_zoom - min_zoom + 1 = 15 at most. A conservative multiplier of ~10x input-to-output is safe for budgeting.

#### 2.2 Relation Phase Byte Estimation
```
per_relation_bytes = size_of::<PreparedRelation>()  // ~408 + SmallVec inline
    + member_ways.iter().sum(|mw| size_of::<MemberWay>() + mw.coords.len() * 16)
```
This is computed immediately after `prepare_relation` returns (line 591), before pushing into `rel_batch`. The member_ways coordinate vectors are already allocated at this point.

#### 2.3 Assemble Phase Byte Estimation
```
per_tile_bytes = size_of::<PendingTile>()  // 32
    + features.iter().sum(|f| f.1.len() + size_of::<(u8, Vec<u8>)>())
```
Tracked incrementally as features are pushed into `current.features` (line 1547). When moving `current` into `batch`, add the accumulated bytes to a batch-level byte counter.

### 3. Concrete Action Plan

#### Step 1: Add byte-estimation utility functions (new code, no behavior change)

Add a small set of inline estimation functions. These can live at the top of pipeline.rs:

```rust
fn estimate_raw_way_bytes(raw: &RawWay) -> usize {
    56 + raw.node_refs.len() * 8
       + raw.tags.iter().map(|(k, v)| k.len() + v.len() + 48).sum::<usize>()
}

fn estimate_prepared_relation_bytes(rel: &PreparedRelation) -> usize {
    std::mem::size_of::<PreparedRelation>()
        + rel.member_ways.iter()
            .map(|mw| 32 + mw.coords.len() * 16)
            .sum::<usize>()
}

fn estimate_pending_tile_bytes(tile: &PendingTile) -> usize {
    32 + tile.features.iter()
        .map(|(_, data)| data.len() + std::mem::size_of::<(u8, Vec<u8>)>())
        .sum::<usize>()
}
```

**Location:** `/home/folk/Programs/elivagar/src/pipeline.rs`, near the top after the existing constants.

#### Step 2: Byte-budgeted way phase in-flight control

**Target:** Replace `MAX_INFLIGHT: usize = 4` with a byte-based semaphore.

**Current mechanism (line 489-530):** Token-based semaphore using `sync_channel::<()>(MAX_INFLIGHT)`. Pre-fill with N tokens. Worker receives a token before spawning, returns it on completion.

**New mechanism:** Replace the `()` token with `usize` (byte count). Pre-fill with the total byte budget. Worker subtracts its block's estimated bytes when acquiring, adds them back when done.

```
// Instead of:
//   sync_channel::<()>(MAX_INFLIGHT)
//   prefill MAX_INFLIGHT tokens
//   recv() to acquire, send(()) to release

// New:
const WAY_INFLIGHT_BUDGET: usize = 128 * 1024 * 1024; // 128 MB
// Use AtomicUsize for current in-flight bytes
// Before spawn: add block bytes, block if over budget
// After completion: subtract block bytes, unblock waiters
```

**Detailed implementation at `/home/folk/Programs/elivagar/src/pipeline.rs` lines 489-530:**

Replace the token semaphore with a `(Mutex<usize>, Condvar)` pair:
1. Compute `block_bytes = raw_ways.iter().map(|r| estimate_raw_way_bytes(r)).sum::<usize>()` after collecting raw_ways (line 515)
2. Apply a multiplier for output (`block_bytes * 10` -- the output expansion factor)
3. Wait on condvar until `inflight_bytes + estimated_total < WAY_INFLIGHT_BUDGET`
4. Add to counter, spawn the rayon task
5. In the task completion (line 529, after `tx.send(results)`), subtract from counter and notify condvar

**Suggested budget: 128 MB.** Rationale: 4 blocks at ~1.8 MB input * 10x output multiplier = ~72 MB. Giving 128 MB provides headroom for variance while being well under the 64 GB target. This can be made configurable via `TilegenConfig`.

**Fallback:** Keep `MAX_INFLIGHT` as an absolute ceiling count (e.g., 8) in addition to the byte budget, as a safety net. This means: "never more than 8 blocks AND never more than 128 MB." The count limit prevents pathological cases with many tiny blocks.

#### Step 3: Byte-budgeted relation batching

**Target:** Replace `REL_BATCH_SIZE: usize = 1024` with a byte-based flush trigger.

**Location:** `/home/folk/Programs/elivagar/src/pipeline.rs` line 854 and lines 591-598.

**Current mechanism:** Accumulate `PreparedRelation` into `rel_batch` Vec, flush when `rel_batch.len() >= REL_BATCH_SIZE`.

**New mechanism:** Track cumulative bytes alongside the Vec:

```rust
const REL_BATCH_BUDGET: usize = 64 * 1024 * 1024; // 64 MB
const REL_BATCH_MAX_COUNT: usize = 4096; // absolute cap as safety net

let mut rel_batch_bytes: usize = 0;
// ...
if let Some(prepared) = prepare_relation(...) {
    rel_batch_bytes += estimate_prepared_relation_bytes(&prepared);
    rel_batch.push(prepared);
    if rel_batch_bytes >= REL_BATCH_BUDGET || rel_batch.len() >= REL_BATCH_MAX_COUNT {
        let batch = std::mem::replace(&mut rel_batch, Vec::with_capacity(256));
        rel_batch_bytes = 0;
        features_emitted += flush_rel_batch(batch, ...);
    }
}
```

**Suggested budget: 64 MB.** Rationale: The `flush_rel_batch` materializes `Vec<Vec<SortRecord>>` which amplifies input by ~5-20x (zoom expansion + per-tile clipping). A 64 MB input batch could produce 300-1200 MB of output records. These records flow immediately into the sort writer which flushes at 1 GB, so the peak is bounded by `batch_input + sort_buffer + output_records_before_drain`. With 64 MB input and the output flowing directly to sort, peak should be ~200-400 MB for the relation phase.

**Additional optimization:** Modify `flush_rel_batch` to drain results incrementally instead of `collect()`:

Currently (line 923-935):
```rust
let results: Vec<Vec<SortRecord>> = batch.into_par_iter().map(...).collect();
for rel_records in results { ... }
```

Change to use `par_bridge` with a channel, or simply accept the collect but with smaller batch sizes. The byte budget already limits input size, so the collect is bounded. For Phase 2 (if needed), replace with:
```rust
batch.into_par_iter()
    .map(|rel| process_prepared_relation(rel, ...))
    .for_each_with(sort_writer_sender, |tx, records| {
        for record in records { tx.send(record).expect("..."); }
    });
```

#### Step 4: Byte-budgeted assemble phase batching

**Target:** Replace `BATCH_SIZE: usize = 4096` with a byte-based batch trigger.

**Location:** `/home/folk/Programs/elivagar/src/pipeline.rs` lines 1496 and 1510-1543.

**Current mechanism:** Reader thread accumulates `PendingTile` into a batch Vec, sends when `batch.len() >= BATCH_SIZE`.

**New mechanism:** Track cumulative bytes of the batch:

```rust
const ASSEMBLE_BATCH_BUDGET: usize = 32 * 1024 * 1024; // 32 MB
const ASSEMBLE_BATCH_MAX_COUNT: usize = 8192; // safety net

let mut batch_bytes: usize = 0;
// ...
// When adding a completed tile to batch:
let tile_bytes = estimate_pending_tile_bytes(&current);
batch.push(current);
batch_bytes += tile_bytes;
if batch_bytes >= ASSEMBLE_BATCH_BUDGET || batch.len() >= ASSEMBLE_BATCH_MAX_COUNT {
    if read_tx.send(batch).is_err() { break; }
    batch = Vec::with_capacity(256);
    batch_bytes = 0;
}
```

But there is a subtlety: the byte estimate for `PendingTile` should be done **incrementally** as features are added to `current`, not computed once when the tile is complete. This avoids the cost of iterating all features at batch-send time:

```rust
let mut current_tile_bytes: usize = 32; // base PendingTile size
// On each feature push (line 1547):
current_tile_bytes += r.data.len() + std::mem::size_of::<(u8, Vec<u8>)>();
current.features.push((layer_idx, r.data));

// On tile completion (before push to batch):
batch_bytes += current_tile_bytes;
batch.push(current);
if batch_bytes >= ASSEMBLE_BATCH_BUDGET || batch.len() >= ASSEMBLE_BATCH_MAX_COUNT {
    // flush
}
current_tile_bytes = 32; // reset for next tile
```

**Suggested budget: 32 MB per batch.** Rationale: The sync_channel(1) double-buffer means up to 2 batches in flight: one being read/accumulated, one being encoded. Rayon encoding parallelizes the encode batch, producing `Vec<EncodedTile>` (compressed). At 32 MB input, compressed output is ~8-16 MB. Total assemble in-flight: ~80-100 MB. This is very comfortable under the 64 GB target.

#### Step 5: Fix compressed tile clone (bonus, low-hanging fruit)

**Location:** `/home/folk/Programs/elivagar/src/pipeline.rs` line 1708.

Currently:
```rust
let compressed = encoder.finish().expect("gzip finish failed");
s.gz_buf = compressed.clone();
Some(EncodedTile { tile_id: tile.tile_id, compressed })
```

The clone exists to preserve `gz_buf`'s capacity for the next tile. Fix:
```rust
let compressed = encoder.finish().expect("gzip finish failed");
s.gz_buf = Vec::with_capacity(compressed.capacity());
Some(EncodedTile { tile_id: tile.tile_id, compressed })
```

This preserves reuse (capacity hint) without cloning the data. Saves ~5 KB per tile on average across all rayon workers.

#### Step 6: Expose byte budgets in `TilegenConfig`

Add optional fields to `TilegenConfig` (with defaults matching the constants above):

```rust
pub struct TilegenConfig {
    // ... existing fields ...
    /// Byte budget for in-flight way processing (0 = use default 128 MB).
    pub way_inflight_budget: usize,
    /// Byte budget for relation batch accumulation (0 = use default 64 MB).
    pub rel_batch_budget: usize,
    /// Byte budget for assemble tile batches (0 = use default 32 MB).
    pub assemble_batch_budget: usize,
}
```

This allows `brokkr` to pass `--low-memory` flags that shrink these budgets for 64 GB hosts, or enlarge them for beefy servers.

### 4. Suggested Byte Budgets Summary

| Control Point | Default Budget | Low-Memory (64 GB) | Rationale |
|---|---|---|---|
| Way in-flight | 128 MB | 64 MB | 4 blocks * 10x expansion = ~72 MB typical. Headroom for variance. |
| Relation batch | 64 MB | 32 MB | Bounds the `collect()` output expansion. Complex relations are the biggest variance source. |
| Assemble batch | 32 MB | 16 MB | Double-buffered, so 2x in flight. Dense urban tiles at z14 are the stress case. |
| Sort chunk | 1 GB (existing) | 512 MB | Already byte-based. Reducing halves peak sort buffer at cost of more chunk files. |

**Total in-flight budget at defaults:** 128 + 64 + 32 + 1024 = ~1.25 GB. At low-memory: 64 + 32 + 16 + 512 = ~624 MB. Both are well within the 64 GB target, leaving ~51-63 GB for the node store (~51 GB) and OS/overhead.

### 5. Migration Strategy

Count-based and byte-based controls can coexist during transition. The approach:

1. **Phase 1 (non-breaking):** Add byte tracking alongside existing count limits. Both must be satisfied: flush when either count OR byte budget is hit. This is a pure addition -- if the byte budget is set very high, behavior is identical to before.

2. **Phase 2 (tune defaults):** Lower count limits to generous safety-net values (e.g., `REL_BATCH_MAX_COUNT = 4096`, `ASSEMBLE_BATCH_MAX_COUNT = 8192`) and let byte budgets be the primary control. Count limits only trigger for pathologically small items.

3. **Phase 3 (remove count primacy):** Once byte budgets are validated at planet scale, the count constants become pure safety nets and can be documented as such.

This means no flag day -- the transition is gradual and each step can be benchmarked independently.

### 6. Implementation Order and Dependencies

1. **Step 1** (estimation functions) -- no dependencies, enables everything else
2. **Step 5** (clone fix) -- independent, pure improvement, can be done first
3. **Step 4** (assemble batch) -- simplest control to implement (single-threaded reader, no sync primitives)
4. **Step 3** (relation batch) -- straightforward, single-threaded accumulation
5. **Step 2** (way phase) -- most complex, requires replacing the token semaphore with Mutex+Condvar
6. **Step 6** (config exposure) -- do after validating defaults on Denmark/Germany

### 7. Risk Assessment

| Risk | Severity | Mitigation |
|---|---|---|
| Byte estimation inaccuracy | Low | Estimates are conservative (overcount by ~10-20%). The allocator overhead (mimalloc metadata) is bounded. Over-estimating is safe (flushes sooner, uses less memory). |
| Throughput regression from smaller batches | Medium | Relation batching: smaller batches mean more rayon pool spin-up overhead. Mitigate by keeping batches large enough (64 MB = ~hundreds of relations). Benchmark before/after on Denmark. |
| Way phase Mutex+Condvar contention | Low | Only contended between the single worker thread (acquiring) and rayon completions (releasing). At 4-8 concurrent tasks, contention is negligible. |
| Estimation cost overhead | Negligible | Estimation is O(n) in items already being iterated. For ways: iterating tags is already done. For relations: member_ways.len() is a single field read per member. For tiles: incrementally tracked. |
| Existing tests break | None | Byte budgets are additive. Existing count limits remain as safety nets. All behavior is identical for small datasets. |
| Sort chunk size interaction | Low | The sort writer already has byte-based flushing. The way/relation byte budgets control *input to* the sort writer, not the sort writer itself. No interaction. |
| `compressed.clone()` removal breaks capacity reuse | Negligible | `Vec::with_capacity(compressed.capacity())` preserves the capacity hint. The GzEncoder will resize internally as needed regardless. |

### 8. Verification Plan

1. **Denmark (S1):** Run full pipeline with new byte budgets. Compare features, tiles, output bytes, and wall time against baseline. Expect identical output, wall time within 5%.
2. **Germany (S2):** Run with default budgets. Monitor `VmHWM` (peak RSS). This dataset has complex multipolygon relations that stress the relation phase.
3. **Add byte-tracking metrics:** Print `max_way_inflight_bytes`, `max_rel_batch_bytes`, `max_assemble_batch_bytes` at pipeline end (same pattern as existing `phase12_ms` etc). This enables data-driven tuning.
4. **Low-memory mode test:** Run Denmark with halved budgets. Expect wall time within 10%, identical output.

### Critical Files

- `/home/folk/Programs/elivagar/src/pipeline.rs` - All three batch/inflight constants live here, plus the way/relation/assemble processing loops that need byte tracking
- `/home/folk/Programs/elivagar/src/sort.rs` - SortWriter already has byte-based chunk flushing; its interface is the model for the new byte budgets
- `/home/folk/Programs/elivagar/src/wire_format.rs` - Understanding record payload sizes to validate byte estimation accuracy
- `/home/folk/Programs/elivagar/src/shortbread/mod.rs` - LayerMatch (408 bytes) and SmallVec sizing affect PreparedRelation byte estimates
- `/home/folk/Programs/elivagar/src/multipolygon.rs` - MemberWay (32 bytes + Vec<Point>) drives relation memory variance
