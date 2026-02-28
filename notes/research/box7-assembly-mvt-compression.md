# Box 7 Investigation: Tile Assembly, MVT Encoding, and Compression

**Date:** 2026-02-28
**Status:** Deep code-level investigation (read-only)
**Scope:** `src/pipeline.rs` (phase_assemble, encode_tile_batch), `src/mvt.rs`, `src/wire_format.rs`
**Commit:** `568813f` (HEAD of main)

## 1. Assembly Pipeline Architecture

### 1.1 Three-Stage Pipeline

The assembly phase (`phase_assemble`, `src/pipeline.rs:1320`) uses a 3-thread pipeline connected by two `sync_channel(1)` channels:

```
Reader thread  --[sync_channel(1)]--> Main thread (rayon)  --[sync_channel(1)]--> Writer thread
  (k-way merge)                        (MVT encode + gzip)                          (PMTiles add)
```

**Thread 1 — Reader** (`src/pipeline.rs:1347`):
- Calls `sort_reader.next()` in a loop to pull records from the k-way merge.
- Groups records by `tile_id` (extracted from sort key, `sort::tile_id_from_key`, `src/sort.rs:31`).
- Accumulates features into `PendingTile` structs.
- When a tile boundary is crossed (`tile_id != current.tile_id`, line 1376), pushes the completed tile into the current batch.
- When the batch reaches `BATCH_SIZE` (4096), sends it through `read_tx`.
- Applies the `should_emit` filter (line 1356): skips tiles containing ONLY ocean features.
- Final partial batch is sent when the merge is exhausted (lines 1362-1368).

**Thread 2 — Encoder / Main thread** (`src/pipeline.rs:1419`):
- Receives batches from `read_rx`.
- Calls `encode_tile_batch(&batch, compression_level)` which uses rayon's `par_iter().map_init()`.
- Forwards encoded results through `encode_tx`.
- After the read channel closes, drops `encode_tx` to signal the writer.

**Thread 3 — Writer** (`src/pipeline.rs:1391`):
- Receives encoded batches from `encode_rx`.
- Calls `pmtiles.add_tile(z, x, y, &tile.compressed)` for each tile.
- Tracks per-zoom tile counts, unique counts, and byte totals.
- Returns the `PmtilesWriter` for finalization.

### 1.2 Channel Depth and Backpressure

Both channels use `sync_channel(1)` (`src/pipeline.rs:1342-1343`). This means:

- The reader can have at most **1 batch queued** beyond what the encoder is processing.
- The encoder can have at most **1 batch queued** beyond what the writer is consuming.

**Total in-flight batches:** At peak, 3 batches can exist simultaneously:
1. One batch being read/grouped by the reader.
2. One batch being encoded by rayon workers.
3. One batch being written by the writer.

Plus up to 2 more sitting in the channel buffers (one in each `sync_channel(1)` buffer).

**Verdict on the theoretical review's "underlap" claim (Box 1, Finding 1):** The claim is **partially valid**. With `sync_channel(1)`, if the writer stalls (e.g., disk I/O spike), the encoder blocks on `encode_tx.send()`, and then the reader blocks on `read_tx.send()`. This creates a full pipeline stall. However, for Denmark at 2.5s assemble time with 54K tiles, the throughput is ~21K tiles/s, and each batch of 4096 tiles processes in ~195ms. The overlap is likely sufficient for Denmark but could become a bottleneck at planet scale with dense urban tiles.

### 1.3 Batch Construction

**Batch size constant:** `BATCH_SIZE = 4096` (`src/pipeline.rs:1335`).

**Batch contents:** `Vec<PendingTile>` where each `PendingTile` is:
```rust
struct PendingTile {
    tile_id: u64,                      // 8 bytes
    features: Vec<(u8, Vec<u8>)>,      // 24 bytes (Vec header)
}
// Static assert: 32 bytes per PendingTile (line 1309)
```

Each feature in `PendingTile.features` is a `(layer_idx, feature_data)` tuple where `feature_data` is the raw sort record payload (`Vec<u8>` owned, transferred from the k-way merge via `SortRecord.data`).

**How the 4096 count was chosen:** There's no comment explaining the rationale. It appears to be a heuristic balancing:
- Rayon parallelism granularity (4096 tiles gives good load balancing across workers).
- Memory footprint per batch (bounded by feature count per tile).
- Latency between reader and writer (smaller batches = more frequent writer wake-ups).

**Last (partial) batch:** When the merge is exhausted, the remaining tiles in the current batch are sent as-is (lines 1362-1368). The batch may have anywhere from 1 to 4095 tiles. Rayon handles partial batches fine -- `par_iter()` distributes whatever is available.

### 1.4 Feature Grouping by Tile

The sort reader yields records in globally sorted order by sort key. The sort key encodes `(tile_id << 16) | (layer << 8) | priority` (`src/sort.rs:26`). This means:

1. All records for the same `tile_id` are consecutive in the merge output.
2. Within a tile, records are ordered by `layer` then `priority`.
3. The reader simply checks `tile_id != current.tile_id` (line 1376) to detect tile boundaries.

**Important:** Records arrive with their `data` field as an owned `Vec<u8>` from the k-way merge (`src/sort.rs:370`). This Vec is moved directly into `PendingTile.features` (line 1386) -- no copy.

### 1.5 What Happens with Varying Feature Counts

Dense tiles (e.g., central Berlin at z14) can have thousands of features per tile. Sparse tiles (e.g., rural areas) may have only 1-5 features.

**Consequence for batch construction:** A batch of 4096 tiles could contain anywhere from ~4K features (all sparse) to millions of features (if the batch includes dense urban tiles). The batch Vec (`Vec<PendingTile>`) itself is small (4096 * 32 bytes = 128 KB), but the aggregate feature data can vary enormously.

**Consequence for rayon encoding:** The `par_iter().map_init()` distributes tiles across rayon workers. A tile with 5000 features takes much longer than one with 5 features. Rayon's work-stealing helps, but if one worker gets a dense tile, it can extend the batch's wall time. The theoretical review's concern about "pathological dense tiles" (Finding 3) is valid but mitigated by rayon's stealing.

## 2. Sort Record to MVT Feature Conversion

### 2.1 Wire Format Decode (`add_feature_to_layer`)

`add_feature_to_layer` (`src/wire_format.rs:109`) decodes a sort record's `data` field and adds it as a feature to a `LayerBuilder`. The function is `#[hotpath::measure]` annotated.

**Decode sequence:**
1. **osm_id** (bytes 0..8): `u64` little-endian. Used as `Feature.id`.
2. **geom_type** (byte 8): `u8` mapped to `GeomType::{Point, LineString, Polygon}`.
3. **geometry commands** (bytes 9..9+4+cmd_count*4):
   - First 4 bytes: `cmd_count` as `u32`.
   - Then `cmd_count * 4` bytes of MVT command data.
   - Uses `unsafe` pointer copy (`src/wire_format.rs:151-157`) for zero-cost memcpy on little-endian. The geometry Vec is **pooled**: popped from `geom_pool` or allocated fresh.
4. **attributes** (remaining bytes):
   - `attr_count` as `u8`.
   - For each attribute: key_len + key_bytes + value_type + value_bytes.
   - Keys and values are interned into the `LayerBuilder` via `intern_key` / `intern_string_value` / `intern_value`.
   - Tag pairs stored as `Vec<(u16, u16)>`, also **pooled** from `tags_pool`.

**Performance data (Germany, dm6):** `add_feature_to_layer` is called 146M times at 269 ns avg, allocating 38.6 GB total (283 B avg per call) (from geographic-profiles.md).

### 2.2 Key/Value Interning

Each `LayerBuilder` maintains three hash maps (`src/mvt.rs:123-131`):
- `key_map: FxHashMap<String, u16>` — maps key string to index.
- `value_map: FxHashMap<Value, u16>` — maps non-string values to index.
- `string_value_map: FxHashMap<String, u16>` — maps string values to index (separate path for borrowed `&str` lookups).

**Interning flow for `intern_key`** (`src/mvt.rs:155`):
1. Check `key_map.get(key)` (borrows `&str` into `HashMap<String>`).
2. On hit: return cached index. **Zero allocation.**
3. On miss: allocate `key.to_string()`, clone it (one for map key, one for `keys` Vec), insert both.

**Interning flow for `intern_string_value`** (`src/mvt.rs:188`):
1. Check `string_value_map.get(s)` (borrows `&str`).
2. On hit: return cached index. **Zero allocation.**
3. On miss: allocate two owned Strings (one for `values` Vec as `Value::String`, one for map key).

**Cache hit rate:** ~90% (from hotpath-profile.md optimization #10). This is because within a single tile, the same key names (e.g., "kind", "highway", "name") and many of the same string values appear repeatedly.

**Important:** LayerBuilders are created **per tile, per layer**. The interning caches are tile-local. This means the hash maps are small (typically < 50 entries per layer per tile) and FxHashMap's speed advantage matters.

### 2.3 Memory Profile of a Fully Materialized Tile

Before encoding, a tile's data is spread across up to 26 `LayerBuilder` instances (one per Shortbread layer). Each `LayerBuilder` contains:

- `name: String` — allocated once from `Layer::ALL[idx].name()` (e.g., "streets").
- `features: Vec<Feature>` — one per decoded sort record for this layer.
- `keys: Vec<String>` + `key_map` — interned key strings.
- `values: Vec<Value>` + `value_map` + `string_value_map` — interned values.

Each `Feature` is 72 bytes (`src/mvt.rs:84`):
```rust
pub struct Feature {
    id: Option<u64>,       // 16 bytes (niche optimization doesn't apply to u64)
    geom_type: GeomType,   //  1 byte + padding
    geometry: Vec<u32>,    // 24 bytes
    tags: Vec<(u16, u16)>, // 24 bytes
}
```

For a dense urban tile at z14 with ~500 features:
- 500 Features * 72 bytes = 36 KB of Feature structs.
- Geometry data: ~500 * 40 bytes avg = 20 KB.
- Tag pairs: ~500 * 12 bytes avg = 6 KB.
- Keys/values interning: ~1-2 KB.
- **Total per tile: ~65 KB for a typical dense tile.**

For a pathologically dense tile (e.g., 5000 features):
- 5000 * 72 = 360 KB of Feature structs.
- Geometry: ~200 KB.
- Tags: ~60 KB.
- **Total: ~620 KB.**

This is manageable. With 4096 tiles in a batch and an average of ~50 features per tile, the total batch materialization is ~4096 * 10 KB = 40 MB. Dense batches could spike to 200-400 MB.

## 3. MVT Encoding

### 3.1 `encode_tile_with` Step by Step

`encode_tile_with` (`src/mvt.rs:276`) takes a slice of `LayerBuilder` references and an `EncodeScratch`:

```rust
pub fn encode_tile_with(layers: &[&LayerBuilder], scratch: &mut EncodeScratch) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4096);       // Line 277: initial allocation
    for layer in layers {
        if !layer.is_empty() {
            layer.encode(&mut buf, scratch);       // Line 280
        }
    }
    buf
}
```

**Allocation:** One `Vec<u8>` per tile with initial capacity 4096. The scratch buffers are reused across tiles.

**Performance data (Germany, dm6):** `encode_tile_with` allocates 11.7 GB total at 54.1 KB avg per call (from geographic-profiles.md). This means the average encoded tile before compression is ~54 KB. The `Vec::with_capacity(4096)` underestimates by ~13x for the average tile, meaning typical tiles will need 2-4 reallocations.

### 3.2 Layer Encoding (`LayerBuilder::encode`)

`LayerBuilder::encode` (`src/mvt.rs:216`) writes protobuf using `protohoggr` primitives:

1. **Clear scratch buffers** (line 217): `layer_buf.clear()`.
2. **Header fields:**
   - field 15: version = 2 (varint).
   - field 1: name (length-delimited bytes).
   - field 5: extent = 4096 (varint).
3. **Features** (field 2, lines 227-246): For each feature:
   - Clear `feat_buf`.
   - Encode optional `id` (field 1, varint).
   - Encode `tags` as packed uint32 array (field 2).
   - Encode `geom_type` (field 3, varint).
   - Encode `geometry` as packed uint32 array (field 4).
   - Write feature as length-delimited message into `layer_buf`.
4. **Keys** (field 3, lines 249-251): Each key as length-delimited string.
5. **Values** (field 4, lines 254-258): Each value as a sub-message via `encode_value`.
6. **Write layer** into tile buf as field 3 (Tile.layers), length-delimited.

**Protobuf implementation:** Hand-rolled via `protohoggr` crate (sibling project). No generated code. The MVT schema is simple enough that this is appropriate.

### 3.3 EncodeScratch Buffers

`EncodeScratch` (`src/mvt.rs:87`) contains 5 scratch Vecs:
```rust
pub struct EncodeScratch {
    layer_buf: Vec<u8>,          // Accumulated layer protobuf bytes
    feat_buf: Vec<u8>,           // Per-feature protobuf bytes
    val_buf: Vec<u8>,            // Per-value protobuf bytes
    packed: Vec<u8>,             // Temporary for packed encoding
    tag_vals: Vec<u32>,          // Flattened tag key-value pairs
}
```

These are created once per rayon worker via `map_init` (line 1462) and reused across all tiles processed by that worker. The `.clear()` calls reset length without releasing capacity, so after a few tiles, the buffers are warm and no further allocations occur.

### 3.4 Encoding vs Compression CPU Split

From the hotpath data:
- Germany (dm6): `encode_tile_with` allocated 11.7 GB for 226K tiles. Assemble phase was 32.1s.
- Denmark (plantasjen): Assemble phase is 2.5s for 54K tiles.

The hotpath timing data doesn't separately instrument gzip compression. However, we can infer from the Germany profiling that `encode_tile_with` is a significant allocator (11.7 GB) but the actual CPU time of encoding is likely dominated by the compression step. libdeflate at level 6 is the heaviest per-byte operation in the pipeline. A rough estimate: for 2.6 GB output (Germany), compression at ~500 MB/s throughput would take ~5s single-threaded, but parallelized across rayon workers it overlaps with encoding.

## 4. Compression

### 4.1 Compressor Initialization and Reuse

In `encode_tile_batch` (`src/pipeline.rs:1458`), `map_init` creates per-rayon-worker state:

```rust
.map_init(
    || {
        let lvl = libdeflater::CompressionLvl::new(compression_level as i32)
            .expect("invalid compression level");
        (mvt::EncodeScratch::new(), mvt::MergeScratch::new(),
         Vec::<Vec<u32>>::new(), Vec::<Vec<(u16, u16)>>::new(),
         libdeflater::Compressor::new(lvl), Vec::<u8>::new())  // <-- compressor + gz_buf
    },
    |(encode_scratch, merge_scratch, geom_pool, tags_pool, compressor, gz_buf), tile| {
        // ...
    },
)
```

**How `map_init` works with rayon:** The closure passed as the first argument to `map_init` is called **once per rayon worker thread** that participates in processing this batch. The state tuple is thread-local and persists across tiles within a single `encode_tile_batch` call.

**Critical detail:** The state is initialized fresh for **each** `encode_tile_batch` call. Between batches, the state is dropped and recreated. This means:
- The `Compressor` is allocated per batch per worker thread, not once for the entire assembly.
- The `EncodeScratch`, `MergeScratch`, `geom_pool`, `tags_pool`, and `gz_buf` are also recreated per batch.

**Verified fact:** With a 12-core machine and rayon using ~24 threads, there are up to 24 compressor instances. Each `libdeflater::Compressor` at level 6 holds internal state (~400 KB per instance based on typical libdeflate internals). Total: ~10 MB for compressors alone. This is fine.

**However, the per-batch recreation is wasteful.** The Vec pools (`geom_pool`, `tags_pool`) lose their accumulated buffers between batches. For Denmark's 54K tiles / 4096 = ~13 batches, this means 13 pool rebuilds. For planet scale's ~100M tiles / 4096 = ~24K batches, this is 24K rebuilds. Each rebuild loses the warm Vec buffers that were previously accumulated.

**CORRECTION/UPDATE:** Looking more carefully at `map_init` semantics in rayon: the init closure is called **once per thread that starts working on items**, and the state persists for the lifetime of that thread's participation in the iterator. Since each `encode_tile_batch` call creates a new `par_iter().map_init()`, the state IS recreated per batch. This is a confirmed inefficiency.

### 4.2 Compression Mechanics

```rust
// Line 1515-1519
let bound = compressor.gzip_compress_bound(mvt_data.len());
gz_buf.resize(bound, 0);
let compressed_len = compressor.gzip_compress(&mvt_data, gz_buf)
    .expect("gzip compress failed");
let compressed = gz_buf[..compressed_len].to_vec();  // <-- the unavoidable .to_vec()
```

**`gz_buf` sizing strategy:**
- `gzip_compress_bound()` returns the worst-case output size for the given input length. For typical MVT data, this is input_len + ~18 bytes (gzip header/trailer) + expansion margin.
- `gz_buf.resize(bound, 0)` grows the buffer if needed. Since `gz_buf` persists per worker within a batch, it only grows (never shrinks). After a few tiles, it stabilizes at the worst-case size seen so far.
- For an average tile of ~54 KB uncompressed, the bound is ~55-56 KB. After gzip level 6, typical compressed size is ~5-8 KB (based on Denmark: 286 MB / 54K tiles = ~5.3 KB avg compressed).

**Compression ratio by zoom level:** Not directly measured in the codebase, but the per-zoom breakdown is logged (lines 1437-1444). Low-zoom tiles (z0-z7) tend to have more features per tile and higher compression ratios. High-zoom tiles (z13-z14) have fewer features but more geometry detail, so compression ratios are lower.

### 4.3 The Unavoidable `.to_vec()`

```rust
let compressed = gz_buf[..compressed_len].to_vec();
```

This copies the compressed data from the thread-local `gz_buf` into a new owned `Vec<u8>` that becomes `EncodedTile.compressed`. This copy is necessary because:

1. `gz_buf` is reused for the next tile on this worker.
2. The `EncodedTile` must own its data to be sent through the channel to the writer thread.

**Size of the copy:** For Denmark, the average compressed tile is ~5.3 KB. For Germany, ~11.5 KB (2.6 GB / 226K tiles). This is a small allocation per tile.

**Could it be eliminated?** Theoretically, if the writer and encoder shared a ring buffer or arena, the copy could be avoided. But the architecture requires the data to survive the channel send, so ownership transfer is mandatory. The alternative would be to write directly to the PMTiles blob file from rayon workers, which would require synchronization.

A more practical optimization: instead of a `Vec::new()` allocation each time, maintain a pool of pre-sized Vec<u8> buffers. After the writer consumes a tile's compressed data, it could return the Vec for reuse. This would require a return channel (not currently present).

### 4.4 Compression Level Impact

The default is level 6 (`src/main.rs:25`). The user can set `--compression-level 0-10`.

**Level tradeoff:** Level 3-4 is documented as "noticeably faster with ~5% larger output" (`src/pipeline.rs:96`). At planet scale with ~100M tiles, the difference between level 6 and level 3 could be significant:
- Level 6: better compression, but higher CPU per tile.
- Level 3: ~50-70% of the compression CPU, ~5% larger output.

At planet scale, if assemble takes ~60 minutes (extrapolating from Germany's 35.5s for 4.4 GB), even a 30% reduction in compression CPU could save ~18 minutes. The 5% output increase on a ~30 GB archive would be ~1.5 GB -- likely acceptable.

## 5. Geometry Merge (`merge_same_attr_geometries`)

### 5.1 Algorithm

`merge_same_attr_geometries` (`src/mvt.rs:505`) performs in-place geometry merging:

**Step 1 — Build index** (lines 512-517): Collect indices of all non-Point features into `scratch.indices`. Points are excluded from merging.

**Step 2 — Sort** (lines 524-529): Sort the indices by `(geom_type, tags)` using Rust's stable sort (Timsort). The comparison is:
```rust
fa.geom_type.cmp(&fb.geom_type)
    .then_with(|| fa.tags.cmp(&fb.tags))
```
Tags are `Vec<(u16, u16)>` where each pair is `(interned_key_idx, interned_value_idx)`. Lexicographic comparison on these pairs determines merge groups.

**Step 3 — Scan and merge** (lines 533-568): Linear scan over sorted indices:
- Find consecutive runs with the same `(geom_type, tags)`.
- For each run of 2+: concatenate all geometries into `scratch.geom` via `append_geometry()`.
- Swap the merged geometry into the first feature of the run.
- Tombstone secondary features by `mem::take`-ing their geometry and tags Vecs (returned to pools).
- Set `id = None` on the merged feature (can't assign a single OSM ID to a multi-geometry).

**Step 4 — Compact** (lines 571-576): If any merges occurred, `retain(|f| !f.geometry.is_empty())` removes tombstoned features.

### 5.2 Complexity Analysis

- **Sort:** O(n log n) where n = number of non-Point features in the layer.
- **Scan:** O(n) with O(m) geometry concatenation per merge group (m = total geometry commands in the group).
- **Compact:** O(n) for the `retain` pass.
- **Overall:** O(n log n) dominated by the sort.

The comparison function for sort is O(k) where k = number of tag pairs. Typical features have 2-5 tags, so this is effectively O(1).

### 5.3 Merge Effectiveness

From project memory: "merge_same_attr_geometries reduced features 97% (1.2M -> 35K), output 630 -> 457 MB."

This is extremely effective because:
- Many features share identical attributes (e.g., all "residential" roads in a tile have the same kind/surface/bridge tags).
- The Shortbread schema groups features by layer, so within a layer, tag homogeneity is high.
- Polygons (buildings, landuse) and lines (roads) benefit most.

### 5.4 `append_geometry` Mechanics

`append_geometry` (`src/mvt.rs:432`) re-encodes delta coordinates from one geometry stream into another:

1. Tracks source cursor (`src_cx`, `src_cy`) and destination cursor (`cx`, `cy`).
2. For MoveTo/LineTo commands: decodes source deltas, computes absolute positions, re-encodes as deltas relative to the destination cursor.
3. For ClosePath: just pushes the command and resets cursor to last MoveTo position.

This is O(total commands) -- no quadratic behavior. Each feature's geometry is processed exactly once.

### 5.5 Edge Cases Where Merging Could Hurt

1. **Very large merged geometries:** If 1000 road segments merge into one multi-linestring, the merged geometry Vec could be large. However, since MVT tiles are bounded in extent (4096x4096), the total vertex count per tile is bounded by the number of features emitted to that tile.

2. **ID loss:** Merged features lose their OSM IDs (line 559). Clients using feature IDs for hover/click identification would be affected. This is a data fidelity tradeoff, not a performance concern.

3. **Tag comparison cost:** If features have many tags with large interned indices, the comparison is slightly more expensive. In practice, Shortbread features have 2-5 tags with small indices.

## 6. Memory During Assembly

### 6.1 Steady-State Memory Model

At any given time during assembly, the following is in memory:

**Reader thread:**
- k-way merge heap: `k` HeapEntry instances (k = chunk count). Each HeapEntry is 40 bytes (`src/sort.rs:279`) plus its `data` Vec. For Denmark with ~3 chunks, this is ~120 bytes + 3 data Vecs. For planet with ~80 chunks (80 GB / 1 GB per chunk), this is ~3.2 KB + 80 data Vecs.
- `BufReader` per chunk: 256 KB each (`src/sort.rs:236`). For 80 chunks: 20 MB.
- Current PendingTile being built: one tile's features in memory.
- Current batch being built: up to 4096 PendingTiles.

**In-channel (read_tx -> read_rx):**
- Up to 1 batch of 4096 PendingTiles queued.

**Encoder (rayon workers):**
- The batch currently being encoded: 4096 PendingTiles + their decoded LayerBuilders.
- Per-worker state: EncodeScratch, MergeScratch, geom_pool, tags_pool, Compressor, gz_buf.
- Per-tile during encoding: 26 LayerBuilder slots, each with interning maps and Feature Vecs.

**In-channel (encode_tx -> encode_rx):**
- Up to 1 batch of encoded tiles (Vec<EncodedTile>).

**Writer thread:**
- PmtilesWriter: dedup HashMap (up to 1M entries * ~40 bytes = ~40 MB), plus streaming file handles.
- Current batch of EncodedTiles being written.

### 6.2 Peak Memory Estimation

**Per batch (4096 tiles):**

For Denmark (avg ~295 features/batch, i.e., 16M features / 54K tiles * 4096 tiles ≈ 1.2M features per batch -- wait, let me recalculate):

Denmark: 16M features, 54K tiles => ~296 features per tile on average. But this is misleading -- most tiles have very few features and a few have many.

Per batch of 4096 tiles:
- PendingTile overhead: 4096 * 32 bytes = 128 KB.
- Feature data (sort record payloads): highly variable. If avg sort record is ~100 bytes and avg features/tile is 296, that's 4096 * 296 * 100 = ~122 MB per batch. But this includes the feature data Vecs.

**However**, feature data is consumed tile-by-tile in rayon workers. Each worker processes one tile at a time. The PendingTile's `features` Vec is borrowed (not consumed) during encoding, so the entire batch stays in memory until the batch's rayon encoding completes.

**Worst case for a single batch:** If a batch happens to contain 4096 dense urban tiles at z14, each with 1000+ features, the aggregate feature data could be 4096 * 1000 * 150 bytes = ~600 MB. This is plausible for a planet run over a dense region.

**Total peak memory during assembly:**
- Sort reader buffers: ~20-80 MB (depending on chunk count).
- Up to 3 batches in flight: 3 * ~120 MB (typical) = ~360 MB typical, up to 3 * ~600 MB = 1.8 GB worst case.
- Per-worker rayon state: 24 workers * ~1 MB = ~24 MB (modest).
- PMTiles dedup map: ~40 MB.
- **Total typical: ~500 MB. Total worst-case: ~2 GB.**

For planet scale with dense urban regions, the worst-case batch spike of ~600 MB is the main concern. Combined with SortedNodeStore still in memory at ~44-52 GB, this could push total RSS close to the 64 GB limit.

**Important:** SortedNodeStore is not explicitly freed before assembly starts. Looking at the pipeline:

```rust
// src/pipeline.rs:129 - run()
// ... phase_read_and_process returns sort_writer ...
// ... ocean phase ...
// ... sort phase ...
// ... phase_assemble ...
```

The `phase_read_and_process` function creates the NodeStore, but it's consumed within that function scope. The NodeStore is dropped when `phase_read_and_process` returns. So by the time assembly starts, the NodeStore memory is freed. **This is a good design.**

### 6.3 Channel Depth and Memory Interaction

With `sync_channel(1)`, the maximum queued memory is:
- read channel: 1 batch of PendingTiles (~120 MB avg).
- encode channel: 1 batch of EncodedTiles (~4096 * 5 KB compressed = ~20 MB).

The PendingTile batches are much larger than the EncodedTile batches because PendingTiles contain raw feature data while EncodedTiles contain compressed output.

## 7. Verification and Expansion of Theoretical Review Claims

### 7.1 Claim: "Compression is likely top CPU sink at high zoom density" (High)

**Verdict: CONFIRMED with nuance.**

Evidence:
- Germany assemble phase: 35.5s for 226K tiles, 2.6 GB output. At rayon parallelism of 12 threads, this is ~426 CPU-seconds.
- `encode_tile_with` allocated 11.7 GB at 54 KB avg per tile. The encoding itself (protobuf serialization) is relatively cheap -- mostly memory copies and varint encoding.
- Gzip compression at level 6 on 11.7 GB of MVT data is the dominant CPU cost. libdeflate at level 6 typically processes ~200-300 MB/s per thread. With 12 threads, that's ~3.6 GB/s, giving ~3.3s for 11.7 GB. But this is input bytes; the actual throughput depends on data entropy.
- The remaining ~32s (426 - 3.3 * 12 = 386 CPU-s for non-compression work) doesn't add up cleanly, suggesting compression is interleaved with encoding and the overhead of LayerBuilder creation/teardown, merge, and feature decode is also significant.

**Key insight:** At planet scale, the assemble phase is projected to be ~35.5s * (100M/226K) = ~15,700s / 12 threads = ~1,300s wall time (~22 minutes) if scaling is linear. Compression level becomes a significant lever.

### 7.2 Claim: "Batch size fixed at 4096 may be suboptimal" (Medium)

**Verdict: CONFIRMED as a real concern, but low priority.**

Evidence:
- For Denmark: 54K tiles / 4096 = ~13 batches. Each batch takes ~190ms (2.5s / 13). This is fine.
- For Germany: 226K tiles / 4096 = ~55 batches. Each batch takes ~645ms (35.5s / 55).
- For planet: ~100M tiles / 4096 = ~24K batches.

The fixed batch size means:
1. **At low zooms (z0-z8):** Very few tiles, so batches are mostly partial. Not a problem.
2. **At high zooms (z13-z14):** Many tiles with varying density. The fixed count means some batches are dominated by a few dense tiles while many sparse tiles finish instantly. Rayon's work stealing handles this.
3. **Per-batch state recreation:** As noted in section 4.1, `map_init` state is recreated per `encode_tile_batch` call. With 24K batches at planet scale, this is 24K compressor allocations per worker. Not catastrophic, but wasteful.

**A byte-budget batch approach** (accumulate tiles until the aggregate feature data exceeds, say, 50 MB) would help at planet scale by making batches more uniform in encoding cost. But this adds complexity to the reader thread.

### 7.3 Claim: "Full feature materialization per pending tile can create spikes" (Medium)

**Verdict: CONFIRMED but bounded.**

Evidence:
- Each `PendingTile` holds all features as raw `(layer_idx, Vec<u8>)` tuples. The entire batch is held in memory during rayon encoding.
- For a tile with 5000 features at ~150 bytes each, the raw data is ~750 KB per tile. In a batch of 4096 such tiles, that's ~3 GB.
- However, extremely dense tiles are rare even in urban areas. Typical z14 tiles have 100-500 features.

**The real concern** is not a single tile's data but the accumulation across a batch. Since tiles in Hilbert order are spatially clustered, a batch of tiles over central Tokyo or Berlin will all be dense simultaneously.

## 8. What the Review Might Have Missed

### 8.1 Per-Batch State Recreation (Confirmed Inefficiency)

The `map_init` state in `encode_tile_batch` is recreated per batch call. This includes:
- `Compressor::new()` — allocates ~400 KB of internal state per worker.
- `Vec::new()` for geom_pool, tags_pool — lose accumulated warm buffers.
- `EncodeScratch::new()`, `MergeScratch::new()` — start with empty Vecs.

**Impact:** At planet scale with 24K batches and 24 workers, this is potentially 24K * 24 * ~500 KB = ~288 GB of allocator churn for compressor state alone. With mimalloc, this is likely fast, but the loss of warm Vec pools is more concerning.

**Fix:** Move the per-worker state outside `encode_tile_batch` into a struct that persists across batches. This requires refactoring the rayon usage to use `install` with a thread-local pattern instead of `map_init`.

### 8.2 `Vec::with_capacity(4096)` in `encode_tile_with` Underestimates

`encode_tile_with` (`src/mvt.rs:277`) allocates `Vec::with_capacity(4096)` for the MVT output buffer. Average tile size is ~54 KB (Germany). This means almost every tile triggers 2-4 reallocations (4096 -> 8192 -> 16384 -> 32768 -> 65536).

**Fix:** Use a larger initial capacity (e.g., 65536 or dynamically sized from feature count) or, better, reuse an output buffer from the per-worker state.

### 8.3 LayerBuilder Creation Per Tile

`new_layer_slots()` (`src/pipeline.rs:1530`) creates `[Option<LayerBuilder>; 26]` per tile. `get_or_create_layer` (`src/pipeline.rs:1535`) lazily initializes LayerBuilders by calling `LayerBuilder::new(Layer::ALL[idx].name())` which does `name.to_string()`.

While lazy initialization avoids creating unused layers, each used layer allocates:
- A `String` for the name.
- Empty Vecs for features, keys, values.
- Empty FxHashMaps for key_map, value_map, string_value_map.

For 54K tiles (Denmark) each using ~5 layers on average, that's 270K LayerBuilder instances. At ~200 bytes overhead each, that's ~54 MB of allocator churn.

**Fix:** Pool LayerBuilders per worker, clearing instead of dropping. The `features` Vec could be `.clear()`ed instead of dropped, and the interning maps could be `.clear()`ed while retaining capacity.

### 8.4 Redundant Copies in Decode -> Encode Path

The data flow is:
1. Sort record `data: Vec<u8>` — owned by `PendingTile`.
2. `add_feature_to_layer` decodes it into `Feature { geometry: Vec<u32>, tags: Vec<(u16, u16)> }`.
3. `merge_same_attr_geometries` may combine geometries.
4. `encode` serializes features back into protobuf bytes (`layer_buf -> buf`).
5. `gzip_compress` compresses `buf` into `gz_buf`.
6. `.to_vec()` copies compressed data into owned `Vec<u8>`.

**Redundant allocations:**
- Step 2: geometry decoded from bytes to `Vec<u32>` (pooled, acceptable).
- Step 4: geometry re-encoded from `Vec<u32>` to protobuf bytes. Could be zero-copy if geometry was kept in wire format, but merge step requires decoded commands.
- Step 6: unavoidable copy (analyzed in section 4.3).

The fundamental tension is that merge needs decoded geometry, but encoding needs serialized bytes. Without merge, a "pass-through" encoding could skip the decode/re-encode cycle. For tiles with no mergeable features (e.g., all Points), this is wasted work.

### 8.5 No Tile-Level Dedup Before Encoding

The PMTiles writer deduplicates compressed tiles by content hash. But encoding and compression happen before dedup. If a tile at z7 is identical to a tile at z8 (same features, same coordinates after simplification), both are fully encoded and compressed before the writer discovers the duplicate.

**Opportunity:** A pre-encoding hash of the raw feature data could detect potential duplicates early. However, feature data changes by zoom (different simplification, different attributes due to zoom-dependent filtering), so cross-zoom dedup is rare. Within the same zoom, dedup is mainly for ocean tiles, which are already filtered by `should_emit`.

### 8.6 No Per-Zoom Batch Sizing

Low-zoom tiles (z0-z8) are few but can be very large (aggregating many features). High-zoom tiles (z13-z14) are numerous but individually small. A fixed batch size of 4096 treats all zooms equally.

**Potential improvement:** At low zooms, smaller batches (e.g., 64-256 tiles) would reduce peak memory for dense overview tiles. At high zooms, larger batches (e.g., 8192-16384) would improve rayon utilization by providing more parallel work items.

**Counterargument:** Tiles arrive from the merge in Hilbert order, which interleaves zooms somewhat (Hilbert IDs encode z in the key). Actually, looking at the sort key construction (`src/sort.rs:26`): `(tile_id << 16)` where tile_id is a Hilbert ID. The Hilbert IDs for z0 tiles are small (0-3), z1 tiles are 4-19, etc. So tiles DO arrive in zoom order (all z0 first, then z1, ..., then z14). This means:
- First few batches: low-zoom tiles (few tiles, dense features).
- Later batches: high-zoom tiles (many tiles, sparser features).

Per-zoom batch sizing is therefore feasible and could be beneficial.

### 8.7 Hidden Quadratic Behavior in Merge

The sort in `merge_same_attr_geometries` is O(n log n). The comparison is by `(geom_type, tags)` where `tags` comparison is lexicographic over `Vec<(u16, u16)>`. If all features have identical tags up to the last pair, the comparison degrades to O(k) per comparison where k is the tag count. Total: O(nk log n).

For typical tiles with 2-5 tags, this is effectively O(n log n). No hidden quadratic behavior.

The `append_geometry` loop in the merge phase is O(total geometry commands), which is linear in the total geometry data. No quadratic behavior here either.

The `retain` compaction pass is O(n). No quadratic behavior.

**Verdict: No hidden quadratic behavior in merge or encode.**

### 8.8 `should_emit` Filter Scans All Features

```rust
let should_emit = |tile: &PendingTile| -> bool {
    tile.features.iter().any(|(layer, _)| *layer != ocean_idx)
};
```

This scans all features of a tile to check if any non-ocean feature exists. For tiles with many ocean features (e.g., dense ocean polygons at high zooms), this is O(features_per_tile). Called once per tile, so total cost is O(total_features). Not a bottleneck, but could be optimized with a flag set during feature accumulation.

### 8.9 `new_layer_slots()` Stack Allocation of 26 Options

```rust
fn new_layer_slots() -> [Option<LayerBuilder>; LAYER_COUNT] {
    [const { None }; LAYER_COUNT]
}
```

`LAYER_COUNT` is 26. `Option<LayerBuilder>` contains a `LayerBuilder` with Strings and Vecs, so its size is substantial. However, all slots start as `None` (zero-initialized), so the initial stack allocation is just 26 * `size_of::<Option<LayerBuilder>>()` bytes. This is created per tile on each rayon worker's stack. With 26 slots, even at ~200 bytes per Option (due to String + 3 Vecs + 3 HashMaps), this is ~5 KB per tile -- well within stack limits.

## 9. Planet-Scale Projections

### 9.1 Assemble Phase Wall Time

From empirical data:
- Denmark (54K tiles, 286 MB): 2.5s on plantasjen (24 threads).
- Germany (226K tiles, 2.6 GB): 35.5s on dm6 (12 threads).
- Japan (182K tiles, 1.2 GB): 17.0s on dm6 (12 threads).

Planet projection (~100M tiles, ~30 GB output):
- Scaling from Germany: 100M/226K * 35.5s = ~15,700s on dm6, or ~7,850s on plantasjen (~2.2 hours).
- But scaling is likely sublinear (more dedup at planet scale, ocean tiles skipped).
- Conservative estimate: **1-3 hours on plantasjen** for the assemble phase alone.

### 9.2 Memory During Assembly at Planet Scale

- Sort reader buffers: 80 chunks * 256 KB = 20 MB.
- Batch feature data: worst-case ~600 MB per batch (dense urban), typical ~120 MB.
- Per-worker rayon state: 24 * ~1 MB = 24 MB.
- PMTiles dedup map: ~50 MB (capped at 1M entries).
- PMTiles streaming temp files: disk-backed, minimal RAM.
- **Total: ~200-700 MB during assembly.** Well within budget since NodeStore is freed by this point.

### 9.3 Compression CPU Budget

At planet scale with 100M tiles averaging 54 KB uncompressed:
- Total uncompressed: ~5.4 TB of MVT data.
- At libdeflate level 6 throughput of ~250 MB/s per thread:
  - Single-threaded: 5.4 TB / 250 MB/s = ~21,600s = 6 hours.
  - 24 threads: ~900s = 15 minutes.
- Compression is clearly the dominant CPU cost at planet scale.

Reducing compression level from 6 to 3 could save ~30-40% of compression time (~5-6 minutes).

## 10. Summary of Findings

### Confirmed Issues

| # | Severity | Finding | Lines | Impact at Planet |
|---|----------|---------|-------|------------------|
| 1 | **High** | Per-batch `map_init` state recreation wastes compressor allocs and pool warmth | `pipeline.rs:1458` | 24K batches * 24 workers * ~500 KB = ~288 GB allocator churn |
| 2 | **High** | Compression is top CPU sink; level 6 is static and not tunable per zoom | `pipeline.rs:1460`, `main.rs:25` | ~15-30 min compression CPU at planet scale |
| 3 | **Medium** | `encode_tile_with` underestimates initial buffer capacity (4 KB vs 54 KB avg) | `mvt.rs:277` | 2-4 reallocations per tile * 100M tiles |
| 4 | **Medium** | LayerBuilder created/dropped per tile instead of pooled per worker | `pipeline.rs:1467,1530` | 100M tiles * ~5 layers = 500M LayerBuilder alloc/dealloc cycles |
| 5 | **Medium** | Fixed batch size of 4096 regardless of zoom or density | `pipeline.rs:1335` | Suboptimal parallelism at low zooms, memory spikes at dense high zooms |
| 6 | **Low** | `should_emit` scans all features per tile (could use flag) | `pipeline.rs:1356` | O(total_features) but with tiny constant |
| 7 | **Low** | `sync_channel(1)` limits pipeline overlap to 1 batch ahead | `pipeline.rs:1342-1343` | Stalls if writer has I/O bursts |

### Opportunities Identified

| # | Type | Description | Estimated Impact |
|---|------|-------------|------------------|
| A | **Persistent per-worker state** | Move compressor, pools, scratch buffers to a struct that persists across batches | Eliminate ~288 GB allocator churn at planet; maintain warm Vec pools |
| B | **Adaptive compression level** | Lower level at high zooms (z13-14) where tiles are small; higher at low zooms where compression ratio matters more | 5-10% faster assembly at planet |
| C | **Larger encode buffer** | `Vec::with_capacity(65536)` or reuse from per-worker state | Eliminate 2-4 reallocations per tile |
| D | **LayerBuilder pooling** | Clear and reuse LayerBuilders per worker thread | Eliminate ~500M alloc/dealloc cycles at planet |
| E | **Byte-budget batching** | Accumulate tiles until aggregate data exceeds N MB instead of fixed count | More uniform batch encoding times |
| F | **Channel depth 2** | Change `sync_channel(1)` to `sync_channel(2)` | Better pipeline overlap, especially if writer has I/O variance |

### Non-Issues Verified

1. **No hidden quadratic behavior** in merge or encode.
2. **NodeStore is freed before assembly starts** -- no RAM contention.
3. **Feature data ownership transfer** is zero-copy from sort reader to PendingTile.
4. **Rayon work-stealing** handles tile density variance within batches.
5. **Merge effectiveness** is extremely high (97% feature reduction).
6. **Pooling of geometry/tags Vecs** is already implemented and working.
