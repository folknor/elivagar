# Design hunt: storage, sort and resources

Scope: `src/sort.rs`, `src/node_index.rs`, `src/way_index.rs`,
`src/multipolygon.rs`, `src/debug.rs`, plus every budget and resource knob
(sort, way, assemble and dedup budgets, fanout cap, partition split zoom,
threads, `ELIVAGAR_*` env vars). This is a reconnaissance; nothing was edited.
Findings are ordered roughly by payoff within each section. Bugs and other
surprises found along the way are listed first because some are real.

---

## 0. Bugs and surprises found along the way

### 0.1 way_index decode is not the inverse of encode (latent wrong coordinates)

`way_index.rs` `encode_way` writes deltas with `wrapping_sub`, but
`decode_way` rebuilds them with `saturating_add`. They agree only while no
delta overflows i32. Longitude in E7 spans +-1.8e9 and i32 tops out at 2.147e9.
Two consecutive nodes on either side of the antimeridian (for example -179.9
and +179.9) have a true delta of about 3.6e9. That delta wraps on encode and
saturates on decode, so the decoded longitude comes back as i32::MAX
(214.7 degrees). OSM convention splits ways at 180, but such ways do exist in
the data, and phase12 already tracks a shifted-longitude extent because the
antimeridian matters. Every relation member way goes through this path
(`pipeline/relations.rs` `prepare_relation`). The fix is one word
(`wrapping_add`), but the structural point is section 1.3: the codec has no
round-trip property test over the full i32 range.

### 0.2 Single-partition sort chunks carry no compression tag

Multi-partition chunk files carry a per-compression magic
(`ELVGSRT1`/`ELVGSRL1`/`ELVGSRS1`), so a `--skip-to` resume under a different
`--compress-sort-chunks` setting "fails loudly at open". Single-partition
files (`chunk_NNNN_z7pNNNNN.bin`, written by `write_chunk_records_presorted` /
`write_payload_chunk_records_presorted`) and whole files carry no header at
all: just a u32 count and then records, possibly inside an lz4/snappy frame.
`compress_sort_chunks` is not recorded in the checkpoint either:
`provenance::producer_config` lists min/max zoom, fanout caps and simplify
factor, and its doc comment explicitly classes everything else as
"assemble-side". Chunk encoding is not assemble-side, though. It is a
property of the bytes on disk. So a resume with a mismatched setting is
caught only if at least one multi-partition chunk exists, and is otherwise a
garbage read. The magic-on-some-files design is a partial answer to a
question that belongs in the checkpoint (see 2.6).

### 0.3 The unsorted flat node index is effectively dead on modern OSM data

There are two independent guards on the flat path:
- `phase12::select_node_store_mode` refuses PBFs over 1 GB
  (`MAX_FLAT_PBF_SIZE`).
- `NodeIndex::put` panics past 16 GB of index (`MAX_FLAT_INDEX_SIZE`), which
  is node_id 2^31, about 2.1 billion.

Current OSM node ids are past 12 billion, so any unsorted extract smaller than
1 GB that contains a recently created node passes the first guard and then
panics in the second, mid-phase12. The first guard's message also talks
about a "~96 GB sparse file" while the second caps at 16 GB. These are two
answers to one question ("is the flat index acceptable for this input?"), they
disagree, and the second answers with a panic instead of an error. See 2.4.

### 0.4 Multipolygon inner-ring pairing tests the vertex most likely to be on the boundary

`multipolygon::assemble` runs `sort_rings_deterministic` (which rotates each
ring so it starts at its lexicographically smallest, that is leftmost,
vertex) BEFORE `pair_rings`. `pair_rings` then tests only `inner[0]` for
containment. An inner ring that touches its outer at a shared node (valid
OSM, and common) often has that touching node as its leftmost vertex. Point
in polygon on the boundary is undefined, so the inner can fail to match any
outer and get "promoted to outer (shell)", which fills the hole. Order the
normalization after pairing, or test a vertex that is not shared with any
outer, or (better, 1.4) pair by containment on the integer engine.

Related: the first containing outer in bbox-sorted order wins, so with
nesting (island in lake in island) an inner can attach to the large outer
rather than the innermost one. Unclassified-role rings are classified by
winding (`classify_unclassified`), but OSM way direction is arbitrary, so
that classification is effectively random. It should be by containment
depth.

### 0.5 `--fanout-cap` accepts layers it can never affect, and that changes the contract

`main.rs` fills `fanout_caps` for every `Layer` (the `--fanout-cap-default`
help says "all polygon layers"), and `provenance` records every non-zero
entry in both the comparability contract and `producer_config`. But the only
consumer is `pipeline/emit.rs` `polygon_z_bottom_after_caps`, reached from
OSM polygon emission. `ocean.rs` never reads `fanout_caps`, and line and
point layers do not either. So `--fanout-cap ocean=2048` or
`--fanout-cap streets=10` does nothing to the tiles while producing a
different contract (`corpus check` would refuse with exit 2) and a different
`producer_config` (resume refuses). The test
`producer_config_covers_chunk_affecting_settings` even uses
`fanout_caps[Layer::Ocean]` as its example of a chunk-affecting setting, but
for ocean it is not one.

### 0.6 `0` means three different things across the knobs

- For `sort_chunk_size`, `way_inflight_budget`, `assemble_batch_budget` and
  `dedup_cap`, 0 in `TilegenConfig` means "use the default".
- For `fanout_caps`, 0 means "uncapped".
- For `PmtilesWriter::set_dedup_cap`, 0 means "dedup disabled" (the writer
  tests rely on this: `set_dedup_cap(0)`).

So `elivagar run --dedup-cap 0` silently gives the 1M default, not "off".
The CLI help says "Default 1000000" and does not say that 0 is swallowed.
See 1.1.

### 0.7 Duplicated defaults, one already shadowed

- `DEFAULT_SORT_CHUNK_SIZE` is defined twice: `pipeline/mod.rs` (private) and
  `pipeline/phase12.rs` (`pub(super)`). The "0 means default" resolution is
  also written twice, in `pipeline::run` and in
  `phase12::phase_read_and_process`.
- The 32 MB assemble batch default is a literal written twice in
  `pipeline/assemble.rs` (the serial and partitioned paths).
- `threads` defaulting (`available_parallelism().unwrap_or(4)`) is written
  twice in `main.rs` (run and ocean-build).

### 0.8 Env-var knobs fail open on typos

`ELIVAGAR_ASSEMBLE_WORKERS`, `ELIVAGAR_ASSEMBLE_PARK_BUDGET`,
`ELIVAGAR_ASSEMBLE_SPLIT_TARGET` and `ELIVAGAR_REL_BLOCKS_CAP` all go through
`.parse().ok()...unwrap_or(default)`. `ELIVAGAR_REL_BLOCKS_CAP=1G` (the
spelling every CLI budget accepts) silently means the 1 GB default. So does
`ELIVAGAR_ASSEMBLE_WORKERS=eight`. brokkr's `capture_env` then records the
variable as set, so the results row claims an A/B arm the run never took. This
is the same failure class AGENTS.md's "either it is explicit, or it is not
set" rule was written against: the record names a value the run did not use.

### 0.9 `--threads` is not a thread budget

The `TilegenConfig.threads` doc says "Thread budget. Controls the rayon global
pool size and pbfhogg decode pool." In fact:
- The rayon pool is built with all `threads`.
- pbfhogg decode gets `threads/3` more (the comment says "rest to rayon", but
  rayon already has all of them).
- Node workers get `(threads/4).clamp(2,6)` (locations mode).
- The way-phase worker, drain and relation-prepass threads are plain
  `std::thread`s.
- The way in-flight ceiling is `threads.max(8)`, so it is not "threads" as
  the comments say.
- Assemble workers are `threads.clamp(1, 8)`.
- `SortPartition::plan_pieces` spawns one OS thread per source with no bound.
  Fan-in is per partition, and AGENTS.md records it reaching about 1000 on
  NA before coalescing.

The real number of concurrent threads is roughly 1.5x to 2x `-j`, and no
single place states it.

### 0.10 Smaller items

- `debug.rs` carries `#![allow(dead_code)]` over the whole module, and
  `malloc_trim`, `set_mmap_threshold` and `read_page_faults` have no callers.
  The allow is hiding real dead code.
- AGENTS.md lists `marker_span` among the `src/debug.rs` emitters. It does
  not exist.
- `debug::emit_marker`'s doc says timestamps are "monotonic microseconds since
  process start". They are relative to the first FIFO write (`OnceLock`
  init), which happens after the resume input hash on `--skip-to` runs.
- `node_index.rs`'s header comment cites "See CLAUDE.md" for madvise history,
  and `flush_group` cites mimalloc fragmentation. Both are stale: mimalloc is
  gone, and CLAUDE.md is not a durable citation target.
- Negative node ids: `NodeIndex::put` silently ignores them, while
  `SortedNodeStore::put` panics via the monotonicity assert (its first
  `last_node_id` is -1). The same input gets two policies depending on
  backend.
- `WayIndex::get` before `finish_writing` returns `None`, which the caller
  counts as a missing reference rather than a programming error (see 3.4).
- The `decode_way` / `read_varint` truncation paths silently return partial
  geometry on corrupt bytes, so there is no error channel.
- The compressed chunk writers pre-serialize the whole chunk into a second
  buffer before compressing (`write_chunk_records_presorted`,
  `write_payload_chunk_records_presorted`). With `--compress-sort-chunks`,
  peak memory for a 1 GB chunk is about 2 GB. The budget does not account for
  this.

---

## 1. Axes that should be types

### 1.1 Budgets and caps: `usize` with 0 as a sentinel -> a resolved `Resources` value

Today every budget travels as a bare `usize` (`sort_chunk_size`,
`way_inflight_budget`, `assemble_batch_budget`, `dedup_cap`) or `u32`
(fanout). 0 is a sentinel whose meaning varies (0.6). Validation (the 64M/1M
minimums) exists only in the clap `value_parser`s in `main.rs`, so a library
caller (brokkr links this crate; `lib.rs`'s doc example passes zeros) can
pass `sort_chunk_size: 1` and get one chunk file per record. Defaults are
resolved at the point of use, deep inside phase12 and assemble (0.7). The way
budget default also depends on the *effective* `locations_on_ways`, which is
only known after the header is read inside phase12.

Proposal:
- `struct ByteBudget(NonZeroUsize)` with the parse and the minimum checks in
  its constructor. It is a different type from `EntryCap(NonZeroUsize)` so a
  dedup entry count cannot be passed where bytes are expected. Today
  `dedup_cap` and `sort_chunk_size` are the same `usize`, adjacent in the
  struct literal, and swap-able.
- `enum FanoutCap { Uncapped, Tiles(NonZeroU32) }`, so "uncapped" is spelled
  rather than encoded as 0.
- `enum DedupPolicy { Off, Cap(NonZeroUsize) }`, so off is expressible
  through the CLI and does not collide with "default".
- `TilegenConfig` takes `Option<ByteBudget>` per knob (None = default).
  `pipeline::run` resolves once into a `ResolvedResources` struct, after
  header detection, so the locations-dependent way default lives there.
  Everything downstream receives the resolved struct and never sees an
  Option or a 0. See 2.1 for where this lives.

### 1.2 Sort keys, tile ids, partitions: `u64` / `usize` aliases -> newtypes

- `SortKey = u64` is a type alias, so any u64 (a tile id, a way id, a byte
  count) is a valid SortKey. `make_sort_key(tile_id: u64, layer: u8,
  priority: u8)` takes two adjacent u8s that can be swapped silently, and
  `layer` is a raw `u8` rather than `shortbread::Layer`.
- A tile id is a raw `u64` in sort.rs, emit, assemble and the pmtiles writer.
  `zoom_from_tile_id` (sort.rs) and `tile_id_to_zxy` (pmtiles_writer.rs)
  answer the same question two ways (2.3).
- A partition is a raw `usize`. `partition_start_tile_id(partition: usize)`
  `debug_assert!`s its range, and multi-chunk section tables carry it as a
  `u32` cast from usize.
- `PayloadRecord = (SortKey, usize, usize)` is a bare tuple where the two
  usizes (offset, len) are swappable. It is destructured positionally at
  about ten sites (`r.1..r.1 + r.2`, `&(key, offset, len)`).

Proposal: `struct SortKey(u64)` with `SortKey::new(TileId, Layer,
PaintPriority)` and accessors returning those types. `struct TileId(u64)`
owning `zoom()`, `zxy()`, `from_zxy()` and the base table. `struct
PartitionId(u16)` (`SORT_PARTITIONS` fits). `struct PayloadRef { key,
offset: u32/usize, len: u32 }` as a named struct. Wire lengths are u32
already, so this also shrinks the record, which `PayloadRecord`'s 24 bytes
pays for in the coalescer's byte accounting.

### 1.3 Coordinates: `(i32, i32)` / `Option<(i32, i32)>` -> `LatLonE7`

Node store `put(node_id, lat_e7, lon_e7)`, node store `get -> Option<(i32,
i32)>`, `WayIndex::put(way_id, &[(i32, i32)])`, `decode_way -> Vec<(i32,
i32)>`, `geometry::project_e7(lat, lon)`, and the extent trackers in phase12
(`min_lat_e7` and friends, as eight loose locals) all agree on (lat, lon)
order by convention only. A `LatLonE7 { lat: i32, lon: i32 }` (repr C, 8
bytes) removes the swap risk. It is also the natural owner of the delta
codec, and so the fix site for 0.1. Similarly `NodeId(i64)` / `WayId(i64)`:
`WayIndex::get(way_id: i64)` will accept a node id today.

### 1.4 Multipolygon: float Mercator with a 1e-9 hash-quantizer -> integer topology

`multipolygon.rs` receives member ways already projected to f64 Mercator
(`relations.rs` maps E7 through `project_e7`), and then rebuilds identity by
`quantize`-ing to a 1e-9 grid (about 4 cm). E7 resolution is about 1 cm, so
distinct nodes within about 4 cm of each other alias as the same endpoint
(false joins), and points straddling a rounding boundary fail to join. The
true identity of a ring endpoint is the node id, or at worst the exact E7
pair, and both are discarded before assembly.

`Polygon = (Vec<Point>, Vec<Vec<Point>>)` is another positional tuple, and
`RingGroups` a 3-tuple of same-typed Vecs (outer, inner and unclassified are
swappable).

Proposal: assemble in integer space keyed on the E7 pair (or on node ids,
if the way index stores them), producing integer rings the
`geometry/int_ocean` engine consumes directly. Ring role and pairing then
come from that engine's containment and winding (normalize with NonZero),
not from a float point-in-polygon of one vertex. Today OSM multipolygons
make a float detour (E7 -> f64 -> quantize-for-hash -> f64 ->
quantize-early-to-integer in the engine) that the rest of the polygon
pipeline already abandoned for ocean. This is the "big rewrite" candidate in
scope, and it fixes 0.4 by construction.

### 1.5 Typed outcomes in place of prose

- `multipolygon::assemble` returns `MultiPolygon` and drops unclosed chains
  silently (`join_ways` returns them and both callers bind `_`). Orphan
  inners promoted to shells and rings dropped for having fewer than 3
  vertices are also invisible. An `AssemblyReport { unclosed_chains,
  promoted_orphans, degenerate }` alongside the result would let
  relations.rs count broken relations next to `missing_ref_stats`, which is
  the existing data-quality counter family.
- Sort chunk errors are `io::Error::new(InvalidData, format!(...))` prose:
  compression mismatch, stale chunks, count mismatch, duplicate ids,
  partition out of range. `pipeline::run` cannot distinguish "your resume is
  inconsistent, re-run full" from "disk corrupt". A `ChunkStoreError` enum
  (`CompressionMismatch { written, expected }`, `StaleChunks { expected,
  found }`, `MissingChunk(u32)`, `Corrupt { path, what }`) maps cleanly onto
  a resume-refusal exit code versus a failure exit code. The CLI exits 1 for
  everything today except ocean spec errors (2).
- `NodeIndex::put` panics on the safety cap (0.3). It should be a
  `Result<_, NodeStoreError::FlatCapExceeded { node_id }>` surfaced as a
  PipelineError, ideally decided before the read starts (2.4).

### 1.6 Sidecar counter names: free strings -> a typed registry

`debug::emit_counter(name: &str, ...)` is called with string literals and
`format!`-built names (`fanout_capped_features_{name}_z{z}`,
`malloc_held_{boundary}`, `sort_layer_<name>_z<z>_...`) from pipeline/mod.rs,
pipeline/stats.rs, sort.rs, way_index.rs, ocean.rs, assemble.rs and phase12.
brokkr parses these names (`--stalls` keys on the `_wait_ns` suffix; the
AGENTS.md docs enumerate them). The naming protocol (suffix semantics, the
`_ns` vs `_wait_ns` split that keeps busy counters out of `--stalls`) is
enforced by convention only. `counter_group!` already shows the right
shape for wait, busy and ring-cap. Extend it so every counter is declared
once, with its kind (`Wait`, `Busy`, `Gauge`, `Total`) deciding the suffix,
and markers become an `enum Phase { Phase12, Ocean, Sort, Assemble }` with
`start`/`end`. AGENTS.md says markers are "reserved for phase boundaries",
and `emit_marker(&str)` lets anyone add a fifth that floods the views. A
`Phase` enum makes that unrepresentable.

---

## 2. Decisions made in more than one place

### 2.1 "What is this run's effective budget / thread count?"

| Question | Sites answering it |
|---|---|
| sort chunk budget default | `pipeline/mod.rs` `run` (own const), `phase12.rs` (own const, own resolution) |
| assemble batch budget default | `assemble.rs` serial path, `assemble.rs` partitioned path (two literals) |
| way budget default | `phase12.rs` inside the way-block arm, keyed on effective locations mode |
| dedup default | `pmtiles_writer.rs` `MAX_DEDUP_ENTRIES` + `assemble.rs` "if > 0 then set" |
| thread default | `main.rs` run, `main.rs` ocean-build |
| per-actor thread split | phase12 (decode /3, node workers /4 clamp, inflight max 8), assemble (clamp 8 + env), plan_pieces (unbounded) |
| ocean per-fold flush | `ocean.rs` `min(sort budget, OCEAN_CHUNK_SIZE_LIMIT)` |
| way-offset sort memory | `way_index.rs` `OFFSETS_SORT_BUDGET` (256 MB, no knob) |
| relation block buffer | `phase12.rs` `REL_BLOCKS_BUFFER_CAP` + env override |

They do not disagree yet, apart from the doc-versus-code mismatches in 0.9.
But the effective values of most of these never reach provenance or the
sidecar as one record. The reasoning in AGENTS.md (budgets are excluded from
the contract because they cannot change tile content) is right about the
contract. It still means "what memory shape did this run have?" can only be
reconstructed by re-deriving defaults from source at that commit.

Single owner: a `resources` module with `Resources::resolve(&TilegenConfig,
&InputFacts) -> ResolvedResources`, called once in `pipeline::run` after
header detection. It holds every budget, every thread split and every env
override (read and validated there, failing loudly on unparseable values;
see 0.8). Its fields are emitted once as sidecar counters (`res_*`), and it
is passed by reference to the phases. The env vars stop being read at use
sites in assemble.rs and phase12.rs, and the "is this an env knob or a flag"
rule in AGENTS.md gets one place to be true.

### 2.2 "Is per-layer stats firehose enabled?"

`ELIVAGAR_LAYER_STATS` is read independently by
`pipeline/mod.rs::layer_stats_enabled` (on every call) and
`ocean.rs::ocean_layer_stats_enabled` (OnceLock-cached). They agree because
both test `var_os(..).is_some()`. Change either to treat `=0` as off and
they diverge. This belongs to the resolved resources/diagnostics struct
from 2.1.

### 2.3 "What zoom is this tile id / where does z14 end?"

- `sort.rs` owns `TILE_ID_BASES` (16 entries), `zoom_from_tile_id` (scans
  down from a hard-coded 14), `TILE_ID_LIMIT_EXCLUSIVE`, and clamps
  out-of-range tile ids into the last partition (`partition_from_key`'s
  `.min(TILE_ID_LIMIT_EXCLUSIVE - 1)`, which is silent).
- `pmtiles_writer.rs` owns `xy_to_tile_id` / `tile_id_to_zxy`, computing
  bases inline and scanning up to z31.
- The "15 zooms" array width is hard-coded as `15` in sort.rs
  (`layer * 15 + zoom`, `32 * 15`), pipeline/emit.rs `RecordTally`,
  pipeline/stats.rs (about ten arrays), emit.rs `cap_events` index packing
  (`(layer * 15 + z) as u16`), ocean.rs (`[...; 15]`), and
  `assemble.rs` (`[0; 15]`).
- `pipeline::run` validates `max_zoom <= 14` with its own literal.

All of these are the same fact (the archive's max zoom is 14, and tile ids
use the PMTiles base formula), restated about twenty times. If the max zoom
ever changes, the sort silently misfiles z15 records into the last
partition. Owner: a `TileId` type (1.2) in one tiling module, with `const
MAX_ZOOM: u8 = 14` and `const ZOOMS: usize = MAX_ZOOM as usize + 1`, and
the pmtiles writer and sort both consuming it.

### 2.4 "Is the flat node index acceptable for this input?"

This is 0.3. Three sites answer it: `select_node_store_mode` by PBF file
size, `NodeIndex::create` vs `create_unbounded` by the unsafe flag, and
`NodeIndex::put` by node id at write time (panic). The first two can be
satisfied while the third fails. The owner should be `select_node_store_mode`,
deciding on facts available before the read. The max node id is not in the
header, but the flat file size is a function of max node id, which is what
the 16 GB cap actually bounds. The honest single rule is "unsorted input
requires `--allow-unsafe-flat-index` or `--force-sorted`". Given 0.3 that is
already the de facto behaviour. Separately, consider deleting the flat path
outright: every supported input is sorted or locations-on-ways, and the
module comment calls the flat path a 96 GB file.

### 2.5 "Per-layer and per-zoom record tally"

`SortWriter::push` (sort.rs) and `RecordTally::record` (pipeline/emit.rs)
make the identical classification (layer from key, `if layer < 32`, zoom
from tile id, `if zoom < 15`, index `layer * 15 + zoom`), and
`SortWriter::merge_tally` re-implements `RecordTally::merge`. Two copies,
kept in step by hand. `push_untracked` exists purely to stop the two copies
double-counting. Owner: `SortWriter` should hold a `RecordTally` (or the
tally should leave the writer entirely, see 3.1). The `32` is also
`Layer::count()` (26) rounded up, so a 33rd layer would silently fall out of
stats via `if layer < 32`.

### 2.6 "What were these chunks produced under?"

The resume guard is split across:
- the checkpoint's `producer_config` (zoom, fanout, simplify),
- the checkpoint's ocean mode and input hash,
- the chunk filename (`_z7` encodes `PARTITION_SPLIT_Z`; a different split
  zoom silently reclassifies files as `Legacy` and falls back to the global
  merge),
- the multi-chunk magic (compression, multi files only),
- the checkpoint's chunk count vs a directory scan.

Five carriers, each covering a different subset, and compression falls
between them (0.2). Owner: a single `ChunkStoreManifest`, written by the
sort writer at flush time and stored in the checkpoint. It records format
version, compression, split zoom, and the chunk list with partition tables
inlined, so assemble does not need to re-open every multi file to read its
section table. Filenames become opaque and the legacy-name parser
disappears (3.2).

### 2.7 "Which layers does a fanout cap apply to?"

This is 0.5. Answered by: `main.rs` (all layers accepted), `provenance.rs`
(all layers recorded), the emit.rs polygon path (applies), and ocean, lines
and points (ignore). Owner: the `Layer` table in shortbread should mark which
layers are fanout-cappable. The CLI parser then rejects others, and provenance
records only those.

### 2.8 "Default of 0 fanout = uncapped" vs `provenance::layer_map(.., zero_is_default)`

The meaning of 0 for fanout is also decided in `provenance.rs`
(`zero_is_default` skips zeros when recording). That is the third site of
the 0.6 sentinel. It goes away with `FanoutCap::Uncapped`.

### 2.9 Ring orientation and determinism of relation output

`multipolygon.rs` decides outer=positive / inner=negative signed area
(`ensure_outer_orientation` and friends) and its own deterministic ring
order (bbox key + lexicographic, at 1e-9 quantization). The integer engine
downstream normalizes winding and order again (by AGENTS.md's description of
`int_ocean`: rotation-invariant pin-aware DP, NonZero normalization). Both
decide winding for OSM relation polygons. If the integer engine is
authoritative, and it is for every other polygon, the float-side orientation
and ordering are redundant decisions that can only disagree with it. This is
the same move as 1.4.

---

## 3. Structure

### 3.1 sort.rs is four modules in one file

Its 2,070 non-test lines do these unrelated jobs:
1. **Key and tiling algebra**: `SortKey` packing, `zoom_from_tile_id`, the
   partition scheme (`PARTITION_SPLIT_Z`, `PARTITION_BASES`,
   `partition_from_key`, `partition_tile_range`). This is a tiling concept
   that assemble, ocean's artifact run-copying (`partition_tile_range` is
   public "because an external PMTiles run may cross a sort-partition
   boundary") and the pmtiles writer all need. It should live with `TileId`
   (2.3), not inside the sorter.
2. **Chunk file format**: three magics, single and multi layouts,
   compressed-section frames, the filename grammar including legacy names,
   and the directory scan. This is an on-disk codec.
3. **Write side**: `SortWriter`, `SpillCoalescer`, the shared chunk-id
   atomic, and two parallel families of writers. `SortRecord` and
   `PayloadRecord` each have single, multi, compressed-multi and
   presorted-single variants, which is six functions writing the same bytes.
   The doc on `write_sorted_payload_chunk` says "the bytes on disk are
   identical".
4. **Read side**: section parsing, `ChunkReader`, the merge heap,
   `SortPartition` and the hot-partition piece planner (`plan_pieces`,
   `scan_source_buckets`, `choose_bucket_cuts`), plus `SortReader`'s two
   modes.
5. Plus stats: the per-layer tally (2.5) and the merge counters.

Proposed layout: `sort/key.rs` (or the tiling module), `sort/format.rs` (one
`RecordSource` trait with `key(i)`, `payload(i)`, implemented by both record
stagings, and one writer generic over it and over `ChunkCompression`, which
removes four of the six writers), `sort/write.rs` (writer + coalescer),
`sort/merge.rs` (readers, heap), and `sort/pieces.rs` (assemble scheduling).
`plan_pieces` is an assemble scheduling policy (its constants `MAX_PIECES`
and `MAX_BUCKETS`, and its target from `ELIVAGAR_ASSEMBLE_SPLIT_TARGET`,
are assemble's), and it lives in sort.rs only because it needs private
chunk internals. Give the format module a small public "source
cursor/seek" API and move piece planning next to the claim window it feeds.

### 3.2 Delete the legacy chunk modes

`ChunkFileKind::Legacy`, the `_pNNNN` "first-cut P3" parser,
`legacy_chunk_path`, `SortReaderMode::Legacy`, `new_whole_paths`,
`SortReader::next`'s partitioned global iteration, and the
`first_cut_partition_suffix_falls_back_to_legacy_merge` test all exist to
read chunks from writers that no longer exist. Checkpoint v6 already refuses
every older checkpoint ("Neither is accepted; re-run a full tilegen"), so
there is no supported resume that can present a legacy chunk. The only
other route into Legacy is a filename with a different split zoom, which
should be an error. With legacy gone, `SortReader` reduces to "a manifest of
partitions" (2.6), and `take_partitions() -> Option<_>` loses its None arm.
Check whether any caller still uses `SortReader::next` globally. If only
assemble's partition path remains, the global iterator goes too.

### 3.3 Three hand-rolled external sorts

`sort.rs` (records), `way_index.rs` (`sort_offsets_file`,
`merge_offset_chunks`, `OffsetChunkReader`, `OffsetHeapEntry`: a complete
second external merge sort with its own fixed 256 MB budget, outside the
budget system), and the coalescer's in-memory sort. The way-offset sort
could also disappear entirely. Way ids arrive sorted in a Type_then_ID PBF
(the same property the sorted node store relies on), and in locations mode
too. If the way index writes offsets in arrival order and checks
monotonicity, the external sort is only a fallback, or can be replaced by
the same sorted-only rule as nodes. Otherwise, reuse a single generic
external sorter. Either way, one fewer k-way merge implementation to keep
correct.

### 3.4 Typestate split: WayIndex

`NodeIndex` -> `NodeIndexReader` and `SortedNodeStore` ->
`SortedNodeStoreReader` already use consuming `into_reader`. `WayIndex`
instead holds both phases in one struct with `Option` writers and `Option`
mmaps, `put` panics after finish, and `get` before finish silently returns
None, which becomes a "missing ref" statistic. Split it into `WayIndexWriter
-> finish() -> WayIndexReader` (Sync), matching the node side. The drain
thread's `(WayIndex, SortWriter, u64, FanoutStats)` return tuple in
phase12 then hands back a reader by type.

### 3.5 node_index.rs: the enum dispatch and naming

`NodeIndex` (flat) vs `SortedNodeStore` vs `NodeStore` (enum) vs
`NodeStoreReader` (enum) vs `NodeIndexReader` is five names for two
backends. With the flat path dead in practice (0.3), the right move is
probably one `SortedNodeStore` and a hard error for unsorted input. If the
flat path stays, it becomes a trait `NodeLookup` (`get(NodeId) ->
Option<LatLonE7>`), with `sorted_stats()` replaced by a backend-agnostic
`stats()` that the sidecar counters consume. The bitpacking, bitmask and
FOR codec (about 450 lines) is a separable codec module, `node_index/codec.rs`,
from the store's addressing logic.

### 3.6 multipolygon.rs is in the wrong layer

It sits at the crate root next to storage modules, operates on float
`geometry::Point`, and is consumed only by `pipeline/relations.rs` (and
emit for types). After 1.4 it belongs under `geometry/` as an integer ring
builder feeding the topology engine, and `WayRole::from_str` (an OSM tag
interpretation) belongs with the relation-member parsing in
`pipeline/relations.rs` or `shortbread`. `WayRole::from_str` also shadows
`FromStr` naming without implementing the trait.

### 3.7 debug.rs mixes four concerns

It holds the sidecar FIFO protocol (markers and counters), stall and busy
accounting, the ring-cap tallies (a geometry statistic parked here because
"emission runs on many workers and on three entry paths"), and glibc
allocator control (`mallinfo2`, plus dead `malloc_trim` and `mallopt`). The
name "debug" is wrong: this is the always-on telemetry channel that brokkr's
gate tooling depends on. Proposed `telemetry/` with `fifo.rs` (protocol
only), `counters.rs` (the declared registry from 1.6, including
`SORT_MERGE_*` from sort.rs, `OCEAN_LAYER_STATS` from ocean.rs, and the
ring-cap pair), and `alloc.rs`. Process-global statics are also why
"nothing resets between runs" is a documented assumption, and that
assumption breaks the moment brokkr (which links the crate) calls
`elivagar::run` twice in one process. A `Telemetry` handle owned by the run
and passed through (or at least reset at `run` entry) removes it.

### 3.8 State split from behaviour: chunk-id allocation

The chunk-number allocator is an `Arc<AtomicUsize>` created in phase12,
attached to `SortWriter` (`attach_chunk_counter`), passed separately to
`SpillCoalescer::new`, used by `write_partitioned_payload_chunks` callers,
and reconciled on `detach_chunk_counter`. The doc warns "The counter MUST be
initialized to the current `chunk_count()` by the caller". That is an
invariant enforced by prose. The chunk store (manifest owner, 2.6) should
own allocation. Writers and coalescers then get a cloneable `ChunkSink`
handle from it, and `adopt_chunk_files` / `attach` / `detach` go away. Today
the count is reconstructed from three sources (`chunk_count`, the atomic,
and the adopted path lists), and resume validates it against a directory
scan.

### 3.9 The budget is per-buffer, not per-run

`--sort-budget` is charged independently by the `SortWriter` buffer, the
`SpillCoalescer` buffer (which keeps filling while a swapped-out full buffer
is being sorted and written, and the doc concedes two producers can trip it
concurrently), the drain thread's writer, and the 2x pre-serialization on
compressed writes (0.10). Real peak sort-side memory is a small multiple of
the flag, with no single statement of the multiple. If budgets become
first-class (2.1), sort memory should be one budget shared through the chunk
store, with buffers drawing from it, so the flag means what the help text
says.

### 3.10 Tests

- The sort tests (about 1,100 lines in sort.rs) are mostly format round-trips
  and resume semantics, mixed in one module. They would follow the module
  split in 3.1 naturally. Resume/checkpoint tests belong with the chunk
  store manifest, not the record format.
- There is no property test for any of the three codecs (way delta varint,
  node FOR bitpacking, chunk record layout) over adversarial ranges. 0.1
  would have been caught by one over full-range i32 deltas.
- `multipolygon` tests exercise the float joiner. After 1.4 they become
  integer fixtures, and the touching-inner case (0.4) and the nested case
  should be added regardless.
- `pipeline_tests.rs` sits at `src/` root and builds `TilegenConfig` with
  struct literals about eight times, each repeating every budget field as
  `0`. That is why the 0-sentinel survives: a resolved-resources builder
  (`TilegenConfig::for_test()` or `Default`) removes the repetition and the
  sentinel together.
