# Planet scale roadmap

Denmark-only performance work is exhausted at 14s (18× from 242s). Geographic profiling
(Germany, Norway, Japan) revealed ocean polygon clipping as a major new bottleneck — see
performance section below.

## Step-by-step: scaling up to planet

### Step 1: `pbfhogg node-stats` tool (in pbfhogg) ✅

Done. Streams through a PBF in constant memory and reports node count, coordinate ranges,
FOR block bit-width distribution (128-value blocks), and estimated compressed size.

Denmark results (52.5M nodes):
- lat avg: 19.5 bits, lon avg: 20.8 bits
- No blocks need 29-32 bits
- 63.8% compression ratio (0.39 GB → 0.25 GB)

Denmark is geographically compact though — need larger extracts to validate that
globally-distributed node IDs don't push bit-widths higher.

### Step 2: Run `node-stats` on increasingly large extracts ✅

Done. Results across all available extracts:

| Extract   | Nodes  | Avg lat bits | Avg lon bits | Ratio | Compressed |
|-----------|--------|--------------|--------------|-------|------------|
| Denmark   | 52.5M  | 19.5         | 20.8         | 63.8% | 0.25 GB    |
| Norway    | 208M   | 18.9         | 20.7         | 62.7% | 0.97 GB    |
| Japan     | 301M   | 19.2         | 20.5         | 62.7% | 1.41 GB    |
| Germany   | 429M   | 22.4         | 23.2         | 72.1% | 2.31 GB    |

Key findings:
- Norway and Japan match Denmark (~20 bits avg, ~63% ratio).
- Germany is worst case — denser edits push 62.5% of blocks to 25-28 bits, 72% ratio.
- Virtually zero blocks hit 29-32 bits across all extracts.
- Planet projection at worst-case 72% (Germany): 8.5B nodes × 8 bytes × 0.72 = **~49 GB**.
  Realistic mix of dense/sparse regions: **44-48 GB**. Fits comfortably under 64 GB.

Compression plan is validated. No fallback path needed.

### Step 3: SortedNodeStore compression ✅

Goal: reduce node store from 99% of raw to fit planet (68 GB) in 64 GB RAM.

**Root cause analysis**: BitPacker4x requires exactly 128 values per block. Average chunk
has 37 nodes → 91 padding zeros at ~20 bits each = 228 wasted bytes per chunk. Result:
74% of chunks couldn't compress at all (compressed ≥ raw). The `bitpacking` crate's SIMD
acceleration was irrelevant — decompression is behind a thread-local cache.

**Solution — two-phase rework:**

1. **Exact-size bitpacking** (Phase 1): Replaced `bitpacking` crate (BitPacker4x, 128-value
   SIMD blocks) with manual scalar bitpacking that packs exactly N values. Uses u64
   accumulator, no padding. Format: `[lat_min:4][lon_min:4][lat_bits:1][lon_bits:1][packed]`.
   Compressible chunks jumped from 26% to 80%.

2. **Flat byte blob** (Phase 2): Eliminated `ChunkMeta` struct entirely. Each group now stores
   all chunks inline in a single `Box<[u8]>`: `[node_mask:32][flags_and_len:u16][packed_data]`
   per chunk. No per-chunk allocation, no per-chunk struct overhead. `find_chunk_in_blob()`
   scans sequentially (~58 avg iterations, trivial cost).

**Other optimizations implemented during this step:**
- Arena allocation: per-group `Vec<u8>` reused across groups, avoids mimalloc fragmentation
- Selective compression: skip FOR when compressed ≥ raw (20% of chunks in final version)
- Drop way_index after phase12: explicit `drop(way_index)` releases mmap pages (~11 GB for N.A.)

**Iteration results on Germany (429M nodes, 11.6M chunks, dm6, commit `b866306`):**

| Variant                              | Node store total | Ratio | Uncompressed chunks |
|--------------------------------------|------------------|-------|---------------------|
| No compression (baseline)            | ~4.0 GB*         | ~122% | 100%                |
| FOR + per-chunk Box<[u8]>            | ~3.8 GB*         | ~116% | 74%                 |
| + arena + selective + ChunkMeta 64B  | 3515 MB          | 107%  | 74%                 |
| + ChunkMeta 40B                      | 3237 MB          | 99%   | 86%                 |
| **+ exact-size bitpacking + flat blob** | **2445 MB**   | **75%** | **20%**           |

*estimated from RSS delta, not directly measured

**Planet projection at 75% ratio:** 8.5B nodes × 8 bytes × 0.75 = **51 GB** (fits in 64 GB
with 13 GB headroom).

**Verified on Germany (dm6, commit `b866306`):** identical output (146,832,380 features,
225,644 unique tiles, 2,609,358,314 output bytes). All 182 tests pass. `bitpacking` crate removed.

### Step 4: Full pipeline on North America (~17 GB)

North America PBF: 17.4 GB, 2.38B nodes, 209M ways. Node store at 75% = ~14.3 GB.
Previous attempt OOM'd at 30 GB (node store 19 GB + way_index 11 GB). With compression
+ way_index drop, estimated peak RSS ~22-24 GB. Should fit on dm6 (32 GB).

### Step 5: Full pipeline on Europe (~28 GB)

Biggest Geofabrik regional extract. ~4-5B nodes → 25-32 GB node store at 75%.
Needs 64 GB RAM.

### Step 6: Planet (~75 GB) — needs ≥64 GB RAM hardware

1. Download planet PBF and ocean shapefiles
2. Node store: 8.5B nodes × 8 bytes × 0.75 = ~51 GB. Fits in 64 GB.
   way_index offset file will be ~144 GB (12B max way_id × 12 bytes) and sparse.
   Sort chunks will be large — may need to tune SORT_CHUNK_SIZE.
3. Profile at planet scale — bottlenecks will shift. Drain thread may become
   critical again (828 blocks for Denmark → ~125K blocks for planet). Sort phase
   and assemble phase will be much larger.

## Tooling

- `scripts/run-safe.sh` — runs elivagar under a cgroup memory cap (MemoryMax + MemorySwapMax=0).
  Pre-flight scans PBF with `pbfhogg fileinfo --extended` and warns if estimated
  SortedNodeStore size is close to the cap. Desktop stays alive if OOM hits.
- `pbfhogg node-stats` — streaming bit-width analysis tool.

## SortedNodeStore compression — design notes

8.5B nodes × 8 bytes = 68 GB uncompressed, target 64 GB RAM. Denmark doesn't need it.

Node IDs are chronological, NOT geographic — consecutive IDs can be anywhere on the globe,
so naive delta encoding between consecutive nodes is unreliable. Approach: FOR
(Frame of Reference) encoding per chunk — compute min_lat/min_lon, store offsets as u32,
bitpack at the chunk's max bit-width. Thread-local decompression cache amortizes lookup cost.

### Final design: exact-size FOR + flat blob

**Exact-size bitpacking**: Manual scalar packing using u64 accumulator. Packs exactly N values
at the required bit-width — no padding to block boundaries. Replaced `bitpacking` crate
(BitPacker4x, 128-value SIMD blocks) which wasted bits on padding zeros (avg 37 nodes/chunk
→ 91 zeros per block). Format: `[lat_min:4][lon_min:4][lat_bits:1][lon_bits:1][packed]`.

**Flat blob layout**: All chunks inline in a single `Box<[u8]>` per group. Per-chunk:
`[node_mask:32][flags_and_len:u16][packed_data]`. No struct per chunk, no per-chunk allocation.
`find_chunk_in_blob()` scans sequentially (avg ~58 iterations, trivial).

### What didn't work

- **BitPacker4x (128-value SIMD blocks)**: Required padding to 128 values. With avg 37 nodes/chunk,
  74% of chunks couldn't compress (compressed ≥ raw). SIMD speed irrelevant behind cache.
- **BitPacker1x (32-value blocks)**: Smaller blocks helped but [BlockMeta;8] per chunk adds
  60 bytes × 11.6M = +100 MB, exceeding compression improvement.
- **Per-chunk Box<[u8]> allocations**: 16B overhead per Box × 11.6M chunks = 186 MB wasted.
  Arena allocation (per-group Vec<u8>) eliminated this.
- **Vec take/recreate pattern**: Caused mimalloc fragmentation (+1.8 GB RSS). Fixed by
  reusing Vec with `.clear()`.

### Key insight

`pbfhogg node-stats` measures theoretical FOR ratio on raw coordinate data (72% for Germany).
But actual node store has per-chunk metadata overhead that erodes gains. With avg 37
nodes/chunk, metadata was 22% of coordinate data. The fix was eliminating metadata overhead
(flat blob) AND making compression work for all chunks (exact-size packing).

## Not worth pursuing — investigated and rejected

- **Thread-local arenas / hoisted simplification buffers** — Remaining per-feature allocs
  (1.3 KB avg × 6.6M = 8.1 GB) are: coords_e7 (ownership transfer to ProcessedWay),
  merc projection (read-only after creation), cascade/keep_buf/simp_buf in
  `for_each_zoom_simplified` (~1.5 GB, hoistable via `map_init` but requires threading
  buffers through 5-6 function signatures). Mimalloc handles these at Denmark scale but
  planet (150× features → ~1.2 TB alloc throughput) will stress the allocator harder.
  Estimated wall-time gain 0.1-0.3s Denmark, potentially larger at planet. Deferred until
  planet hardware is available for profiling — implement if allocator shows up in the profile.
- **Pre-size wire format buffers** — `add_feature_to_layer` already uses `with_capacity()`
  pre-sizing. 303 B avg, no realloc churn to cut.
- **Simplification algorithm** — already exhausted (DP + convergence + subpixel bbox, VW tried/reverted)
- **Sort phase** — 0.3s, negligible
- **match_element** — 296ns/call, very tight

## Completed

- [x] ~~**Visvalingam-Whyatt instead of Douglas-Peucker.**~~ Tried and reverted — VW's
  allocation overhead exceeds DP savings for small geometries (avg ~10 vertices). Non-cascading
  VW also produces more output at low zooms. See `notes/vw-simplification-experiment.md`.

## Not pursued

- **Rayon alternatives for slice-based parallelism** — Research notes in previous git
  history. Key options: paralight, orx-parallel, chili, forte. Not a current bottleneck.

## Performance: Linux kernel features — all reverted

All madvise/fadvise hints were tried and removed. Every hint caused regressions because the
node index was a sparse file. Now that SortedNodeStore is in-RAM, the madvise concern is moot
for the node index. See CLAUDE.md for full history.
