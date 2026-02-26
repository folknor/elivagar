# elivagar TODO

## Release prep

- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server

## Planet scale

Denmark-only performance work is exhausted at 14s (18× from 242s). Geographic profiling
(Germany, Norway, Japan) revealed ocean polygon clipping as a major new bottleneck — see
performance section below.

### Step-by-step: scaling up to planet

#### Step 1: `pbfhogg node-stats` tool (in pbfhogg) ✅

Done. Streams through a PBF in constant memory and reports node count, coordinate ranges,
FOR block bit-width distribution (128-value blocks), and estimated compressed size.

Denmark results (52.5M nodes):
- lat avg: 19.5 bits, lon avg: 20.8 bits
- No blocks need 29-32 bits
- 63.8% compression ratio (0.39 GB → 0.25 GB)

Denmark is geographically compact though — need larger extracts to validate that
globally-distributed node IDs don't push bit-widths higher.

#### Step 2: Run `node-stats` on increasingly large extracts

Run on Germany (4.4 GB), North America (17 GB), Europe (28 GB) to see if the bit-width
distribution is stable across regions. If avg bit-width stays around 20, compression
is very comfortable. If it climbs to 28-30, need a fallback plan.

#### Step 3: Full pipeline on North America (~17 GB)

Download from Geofabrik. Use `scripts/run-safe.sh` (cgroup memory cap, pre-flight
node count estimate). North America should fit on dm6 (32 GB RAM) — expect ~1.5-2B
nodes → ~14-18 GB SortedNodeStore.

What we learn:
- Does scaling stay linear? (Germany was 10× Denmark at ~10-12× wall time)
- Sort phase and assemble phase at 4× Germany scale
- Drain thread pressure approaching planet regime
- New bottlenecks that don't appear at Germany scale

#### Step 4: Full pipeline on Europe (~28 GB)

Biggest Geofabrik regional extract. ~4-5B nodes → 34-42 GB SortedNodeStore.
**Will NOT fit on dm6 (32 GB) uncompressed.** Two options:
- If Step 1 shows compression is viable, implement it first, then run Europe
  as the compression validation test
- If compression isn't enough, implement the mmap fallback first

This is the dress rehearsal for planet.

#### Step 5: Planet (~75 GB) — needs ≥64 GB RAM hardware

1. Download planet PBF and ocean shapefiles
2. First run — expect SortedNodeStore needs compression (68 GB > 64 GB RAM).
   way_index offset file will be ~144 GB (12B max way_id × 12 bytes) and sparse.
   Sort chunks will be large — may need to tune SORT_CHUNK_SIZE.
3. Profile at planet scale — bottlenecks will shift. Drain thread may become
   critical again (828 blocks for Denmark → ~125K blocks for planet). Sort phase
   and assemble phase will be much larger.

### Tooling

- `scripts/run-safe.sh` — runs elivagar under a cgroup memory cap (MemoryMax + MemorySwapMax=0).
  Pre-flight scans PBF with `pbfhogg fileinfo --extended` and warns if estimated
  SortedNodeStore size is close to the cap. Desktop stays alive if OOM hits.
- `pbfhogg node-stats` — streaming bit-width analysis tool (to be built, see Step 1).

### Bitpacked coordinate compression for SortedNodeStore

8.5B nodes × 8 bytes = 68 GB uncompressed, target 64 GB RAM. Denmark doesn't need it.

Node IDs are chronological, NOT geographic — consecutive IDs can be anywhere on the globe,
so naive delta encoding between consecutive nodes is unreliable. Best approach: FOR
(Frame of Reference) encoding per 128-value block using `bitpacking` crate (v0.9, stable
Rust, SSE3 SIMD). Per block: compute min_lat/min_lon, store offsets as u32, bitpack at
the block's max bit-width. Estimated compression: ~24 bits avg (vs 32 raw) → 68 GB → ~51 GB.
Lookup overhead: ~150ns to decompress a 128-value block (+13% on `process_raw_way`).

**Validation step**: write a standalone tool that reads a planet PBF, builds the
SortedNodeStore, and measures the actual bit-width distribution across all 128-value
blocks. This gives an exact compression ratio without modifying the main pipeline.
If actual average is closer to 28-30 bits, savings drop to 59-63 GB — too tight.
A fallback path (dense packed mmap file with in-RAM bitmask index) should be designed.

### Not worth pursuing — investigated and rejected

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

### Completed

- [x] ~~**Visvalingam-Whyatt instead of Douglas-Peucker.**~~ Tried and reverted — VW's
  allocation overhead exceeds DP savings for small geometries (avg ~10 vertices). Non-cascading
  VW also produces more output at low zooms. See `notes/vw-simplification-experiment.md`.

### Not pursued

- **Rayon alternatives for slice-based parallelism** — Research notes in previous git
  history. Key options: paralight, orx-parallel, chili, forte. Not a current bottleneck.

## Performance: Linux kernel features — all reverted

All madvise/fadvise hints were tried and removed. Every hint caused regressions because the
node index was a sparse file. Now that SortedNodeStore is in-RAM, the madvise concern is moot
for the node index. See CLAUDE.md for full history.
