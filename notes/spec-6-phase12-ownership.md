# Spec 6: Phase 1+2 ownership rewrite (way/relation producers)

Written against `reference/technical-implementation-spec.md`. Source items:
`notes/performance-backlog.md` P2 - item 16 (way-phase ownership rewrite, the
core), item 22 (selective way resolution + relation planning), and item 15
half 2 (compact shared-node counters). This spec makes the P2-mandated
decision between "16 then fold 22 in" and "straight to 22" (Section 3) and
carries all three items to completion as one ordered campaign, no deferral.

Measurement record: `reference/performance.md` + `.brokkr/results.db`. This
spec is squarely on the largest measured phase, so every landing carries a
`--bench 3` verdict. Pre-change baselines (plantasjen, clean bench mode, HEAD
`9994e5f`) the keep/revert verdicts are read against:

| dataset | commit | wall | phase12 | phase12 % wall | ocean | assemble | peak RSS |
|---|---|---|---|---|---|---|---|
| denmark | `a0fca65` | 26.4s | 15.8s | 60% | 5.9s | 4.1s | 2.9 GB |
| norway  | `b26b335` | 105.0s | 70.5s | 67% | 7.8s | 26.5s | 5.9 GB |
| germany | `9994e5f` | 230.7s | 188.3s | 82% | 5.2s | 36.2s | 14.8 GB |

Reference profiles (2026-07-06 campaign, commit `95d6d52`; the way phase is
structurally unchanged since, so these price the target):

| dataset | run | signal |
|---|---|---|
| germany | `cc14c34a` (hotpath) | `process_raw_way` 754 thread-s; `prepass_shared_nodes` 79s pure serial |
| norway  | `4e62f519` (hotpath) | `process_raw_way` 168 thread-s |
| denmark | `13c024bb` (hotpath) | `process_raw_way` 73 thread-s (denmark badly underprices this phase) |
| denmark | `081975b3` (alloc)   | `write_sorted_chunk` 9.9 GB, `process_raw_way` 8.1 GB of tracked allocation |

**Primary keep gate: germany.** Germany is inland/way-volume-bound (69.6M
ways, phase12 82% of wall) and prices `process_raw_way` at 754 thread-s -
10x denmark. Denmark and norway are neutrality/regression gates only; a
denmark-only verdict on this spec is not a verdict (`reference/performance.md`
reading rules). Norway additionally guards the relation path (item 22's
relation-planning prepass) and its coastal-multipolygon read-back.

---

## 1. The idea in one paragraph

The way phase resolves its ownership backwards. A single dispatch thread
parses every way out of every block and copies all tags to owned
`(String, String)` and node refs/coords into fresh `Vec`s (`RawWay`) BEFORE
rayon starts; each rayon task then re-borrows those `String`s as `&str`, and
in locations-on-ways mode clones the coords again; every emitted feature is a
separate `Box<[u8]>` funnelled through a channel to a drain thread that
re-serialises it into a contiguous buffer inside `SortWriter`. `RawWay` exists
only because PBF element borrows do not outlive the read callback - but the
`PrimitiveBlock` DOES outlive it. The rewrite sends the whole block into the
rayon task and does all extraction and processing there against borrowed
`&str` tags (zero tag allocation, extraction parallel across blocks), emits
features into a per-worker payload arena, and flushes sorted chunk files
directly (the `OceanAcc`/`RelAcc` idiom ocean and relations already use). The
drain thread shrinks to way-index puts only. Layered on that structure, an
upfront relation-planning prepass lets the way phase resolve coordinates and
store way-index geometry only for the ~5% of ways a relation actually reads
back plus the ways that match Shortbread directly - everything else is
classified from borrowed tags and dropped before any node-store lookup.
Finally the global shared-node prepass, whose `FxHashSet<i64>` of every unique
node id is 30-60 GB at planet scale, is replaced by exact rank-indexed 2-bit
counters (node-store path) or an exact external count (locations-on-ways) so
the box does not OOM before geometry begins.

## 2. Survey of the ground

All line numbers drift; names are stable. The way phase lives in
`src/pipeline/phase12.rs`, the emitters in `src/pipeline/emit.rs`, relations
in `src/pipeline/relations.rs`, the sort machinery in `src/sort.rs`, the way
geometry store in `src/way_index.rs`.

### 2.1 The current way-phase data flow (`phase_read_and_process`)

Nodes are handled inline on the read thread (`handle_node!`): put into the
node store, extent tracked, tagged nodes pushed as `SortRecord`s straight into
`sort_writer`. When the first `BlockType::Ways` block arrives, three things
spin up:

- the shared-node prepass thread (spawned even earlier, before the node
  phase - `prepass_shared_nodes`, `BlobFilter::only_ways()`) is joined here to
  produce `gsn: Arc<FxHashSet<i64>>` of nodes appearing in 2+ ways;
- a **worker thread** running `rayon::in_place_scope`. Its loop
  `while let Ok(block) = brx.recv()` extracts a `Vec<RawWay>` from the block
  serially (owned `String` tags, `node_refs`, and in locations-on-ways mode
  `coords_e7`), runs `annotate_block_shared_node_refs` +
  `annotate_global_shared_node_refs`, applies a condvar byte-budget throttle
  (`WAY_OUTPUT_MULTIPLIER = 10`, `MAX_INFLIGHT = 8`, `way_budget`), then
  `s.spawn`s a task that `into_par_iter()`s the block's `RawWay`s through
  `process_raw_way` and `tx.send(Vec<ProcessedWay>)`;
- a **drain thread** owning `way_index` + `sort_writer`. Its loop
  `while let Ok(results) = rrx.recv()` runs `drain_processed_ways`:
  `way_index.put(way_id, &coords_e7)` per way, `record_fanout_from_records`,
  cap-event harvest, then `sort_writer.push(record)` per `SortRecord`.

Relation blocks are buffered (`relation_blocks: Vec<PrimitiveBlock>`) and
processed only after all blocks are read, because locations-on-ways PBFs can
place way blocks after relation blocks. `prepare_relation` reads
`way_index.get(way_id)` for each member, batches into `flush_rel_batch` (the
`RelAcc` rayon fold), which flushes its own chunk files and pushes the
remainder through `sort_writer`.

`ProcessedWay { way_id, coords_e7: Vec<(i32,i32)>, records: Vec<SortRecord>,
cap_events }` carries BOTH the resolved coords (for `way_index.put`) and the
emitted records through the same channel. `process_raw_way`:

1. resolves coords - clones `raw.coords_e7`/`raw.node_refs` in
   locations-on-ways mode, else `nr.get(id)` per ref (the node-store mmap
   reads, `find_chunk_in_blob` + `decompress_chunk` = 187 thread-s germany);
2. rebuilds `tags_ref: Vec<(&str,&str)>` from the owned `String`s and
   `match_element`s - **so the owned `String`s existed only to be re-borrowed
   here**;
3. builds `preserve_vertex_mask` from `preserve_node_refs`;
4. via `WAY_WORKER_SCRATCH` thread-local, projects, unwraps antimeridian,
   enriches polygon matches, and calls `emit_point_or_centroid` /
   `emit_line_feature` / `emit_polygon_feature` per match, all appending to a
   local `records: Vec<SortRecord>`.

### 2.2 The emit boundary (already decoupled from the engine)

`emit_polygon_feature` / `emit_multipolygon_feature` build a local `sink`
closure `|z, tx, ty, geom: &[u32]|` and hand it to `emit_shape_pyramid` (the
spec-4 descent engine in `src/geometry/int_ocean.rs`). **The int_ocean engine
is already SortRecord-agnostic** - it yields `(z, tx, ty, geom)` and the emit
closure calls `push_sort_record(tile_id, osm_id, layer, geom_type, geom_buf,
attrs_buf, records)`, which does `encode_feature_data_with_attrs` (returns a
`Box<[u8]>`) + `make_sort_key` and pushes a `SortRecord { key, data }`. Ocean
does not use these emitters; it drives int_ocean with its own `OceanAcc` sink.
So the arena conversion is contained to `push_sort_record`, the four emit
functions' record parameter, `process_node`, and their two callers
(phase12.rs, relations.rs). int_ocean and spec-4's descent are untouched.

### 2.3 The arena idiom already in the tree

`OceanAcc` (ocean.rs): `records: Vec<sort::PayloadRecord>` where
`PayloadRecord = (SortKey, offset, len)`, `payload: Vec<u8>`, `bytes`,
`chunk_paths`, `count`, `compression`. `flush` calls
`sort::write_sorted_payload_chunk(&mut records, &payload, path, compression)`
(sorts `(key,off,len)` by key, serialises to the identical on-disk chunk
format) and the driver `adopt_chunk_files`. `RelAcc` (relations.rs) is the
same pattern but still holds `Vec<SortRecord>` (`Box<[u8]>` each) and
`write_sorted_chunk`. This spec converges the way AND relation producers on
the `OceanAcc` payload-arena form.

### 2.4 SortWriter accounting the arena must reproduce

`SortWriter::push` is the only place per-layer/zoom stats are tallied
(`layer_records[32]`, `layer_bytes[32]`, `layer_zoom_records[32*15]`,
`layer_zoom_bytes[32*15]`, plus `total_records`/`total_record_bytes`), read
out at the end into `Phase12Stats`. **Chunks written directly by
`write_sorted_*_chunk` bypass this** - today `flush_rel_batch`'s flushed
chunks already escape the per-layer tally (only its remainder, pushed through
`sort_writer.push`, is counted). Moving the way path to direct flush would
zero out the way layers in `Phase12Stats` unless the arena reproduces the
tally. The arena therefore accumulates the same counters and merges them into
`Phase12Stats` (Section 4.4). This also fixes the pre-existing relation
undercount as a side effect.

### 2.5 WayIndex

`WayIndex::put(way_id, coords)` delta-varint-appends coords to `way_data.bin`
and writes a 16-byte `(way_id, offset)` to `way_offsets.bin` - append-only,
single-writer, **order-independent**: `finish_writing` external-sorts the
offsets by `way_id`. `get(way_id)` binary-searches the sorted mmap. Today
every way is `put`; only relation members are ever `get`. Item 22 gates the
`put`.

### 2.6 The shared-node prepass

`prepass_shared_nodes` streams way blobs (`BlobFilter::only_ways()`),
building `seen: FxHashSet<i64>` (every unique node id in any way) and
`shared: FxHashSet<i64>` (ids seen 2+ times). Returns `shared`. Overlaps the
node phase (spawned before it, joined at the first way block). `seen` is the
planet monster: at ~2B unique node ids an `FxHashSet<i64>` is 30-60 GB. It is
built purely to detect duplicates and dropped immediately after. Output use:
`shared` drives `annotate_global_shared_node_refs`, which pins junction
vertices so DP simplification does not tear ways apart at shared endpoints.

### 2.7 Failure history relevant to this spec

The condvar throttle in the worker loop had a deadlock (a single block
exceeding `way_budget` with zero in-flight tasks slept forever with no
`notify_one`); fixed by the "always allow at least one task" wait predicate
(`count > 0 && ...`). Any rewrite of the throttle must preserve that
invariant. This is a data-flow/perf spec, not a geometry spec: it must be
output-identical through Landings 1 and 2 as measured by `elivagar regress` at
`--tol 0` (zero structural diffs, `tolerance_moved 0`). Note this is
REGRESS-identical, not literal-byte-identical: `regress` canonicalizes
intra-layer feature order (Section overview / regress design), and the rewrite
DOES reorder records (arena flush order, flush-vs-tail split), so the raw chunk
and tile bytes are NOT `cmp`-identical - only the regress-canonical output is.
Where this spec says "byte-identical" it means this `--tol 0` regress identity;
do not reach for a literal `cmp` and panic (Opus R2). So the geometry failure
ledger
(`notes/rendering-fix-log.md`, R01-R24/S01-S09) is not re-litigated here -
the earcut oracle stays a standing gate purely to prove no geometry moved.

## 3. The P2 spec decision: 16 first, 22 folded on, 15h2 last

P2 forbids landing 16 and 22 as "two separate intrusive rewrites of the same
phase." This spec resolves that as **"16 then fold 22 in,"** structured so 22
is not a second teardown:

- **Landing 1 (item 16)** rewrites the way-phase data flow: block-into-rayon,
  borrowed tags, per-worker payload arena, direct chunk flush, drain reduced
  to way-index puts. It is a pure mechanical ownership change with **no policy
  change** - every way is still classified, resolved, stored, and emitted
  exactly as today. Output is regress-identical (Section 2.7). Credit assignment
  matters for honest per-landing bench expectations (Opus R2): the germany
  `process_raw_way` 754 thread-s is dominated by node-store mmap reads (~187s),
  geometry, and the per-record `Box`. Tag STRING cloning and RawWay extraction
  are NOT inside `process_raw_way` - they run on the serial worker thread during
  RawWay build, so they do not appear in that function's thread-s at all. Of the
  754s, **Landing 1 removes only the per-record `Box` (arena) and the serial
  extraction/tag-clone on the worker thread**; the ~187s node-store cost is
  removed by LANDING 2's selective resolution, not here. Landing 1's isolated
  win is therefore smaller than a naive read of the 754s implies - which sharpens
  the parallelism-cap risk in Section 4.6: with a smaller constant-factor win,
  losing pool saturation would flip the landing to a regression.
- **Landing 2 (item 22)** adds an upfront relation-planning prepass and gates
  coordinate resolution and `way_index.put` on match-or-member. It **reuses
  Landing 1's per-worker task and arena verbatim** - it changes what the task
  DECIDES to do (classify first; skip resolution/storage for
  non-matching-non-member ways), not the data flow's shape. It is also
  byte-identical (skipped ways emitted nothing and were never read back).

Landing them separately keeps each verdict gate-isolable: Landing 1's
`--tol 0` regress proves the ownership rewrite moved no bytes; Landing 2's
`--tol 0` regress proves selective resolution changed no output. Going
"straight to 22" would entangle the data-flow rewrite and the resolution
policy in one un-bisectable landing - if the combined output diffed you could
not tell which half did it, violating keep/revert discipline. 22 is genuinely
a policy layer on 16's structure, not a rewrite of it, so this ordering
honours the "not two teardowns" constraint.

- **Landing 3 (item 15 half 2)** replaces the shared-node prepass's `seen`
  set. It is independent of the 16/22 axis: item 22's relation planning is a
  DIFFERENT prepass (relation members, not way-shared nodes), so the
  shared-node prepass survives item 22 and still needs its memory fixed. The
  backlog's "dissolves into it" note conflates the two prepasses; the
  shared-node counter is required on its own.

## 4. Landing 1 (item 16): arena-flush way phase

Output-byte-identical ownership rewrite. Concrete target types and flow.

### 4.1 The record sink: payload arena replaces `Box<[u8]>`

Replace the `records: &mut Vec<SortRecord>` parameter threaded through the
emitters with a sink backed by a payload arena. The append primitive **already
exists** in `src/wire_format.rs` - `append_feature_data_with_attrs(buf, osm_id,
geom_type, geom_cmds, attrs_bytes) -> std::ops::Range<usize>` - and
`encode_feature_data_with_attrs` (the `Box`-returning form) already delegates to
it (`append` on a fresh `Vec` + `into_boxed_slice`). So there is NO new encoder
to write and nothing to add to `sort.rs`; the arena writes via the existing
`wire_format::append_feature_data_with_attrs`, taking `range.start..range.end`
as the `(offset, len)` for the `PayloadRecord`. (Earlier drafts of this spec
proposed a new `encode_feature_data_with_attrs_into` in `sort.rs`; that was a
misdirection - the primitive and its byte-for-byte tie to the `Box` form are
already in `wire_format.rs`. Codex R1.)

New sink type (in `src/pipeline/emit.rs`), the single arena all OSM producers
write through:

```rust
pub(super) struct RecordSink {
    pub(super) records: Vec<sort::PayloadRecord>, // (SortKey, offset, len)
    pub(super) payload: Vec<u8>,
    // full tally reproducing SortWriter::push (Section 2.4) - per-layer/zoom
    // arrays AND the scalar totals Phase12Stats reads (Section 4.4)
    pub(super) layer_records: [u64; 32],
    pub(super) layer_bytes: [u64; 32],
    pub(super) layer_zoom_records: Box<[u64; 32 * 15]>,
    pub(super) layer_zoom_bytes: Box<[u64; 32 * 15]>,
    pub(super) total_records: u64,
    pub(super) total_record_bytes: u64,
}
```

`push_sort_record` loses its `records` parameter and gains `sink: &mut
RecordSink`; it calls `encode_feature_data_with_attrs_into`, computes the key,
pushes the `(key, off, len)`, and updates the tally exactly as
`SortWriter::push` does (layer from key, zoom from tile id). The four emitters
(`emit_point_or_centroid`, `emit_line_feature`, `emit_polygon_feature`,
`emit_multipolygon_feature`) and `process_node` swap their record parameter
for `&mut RecordSink`; their internal `sink` closures now capture the
`RecordSink`. No other emitter logic changes.

Fanout stats: `record_fanout_from_records(&[SortRecord], ..)` reads only
`key`; add `record_fanout_from_payload_records(&[sort::PayloadRecord], ..)`
reading `key` directly (the `.data.len()` it uses is available as the tuple's
`len`). Cap events are unchanged (they live on the emit scratch, harvested as
today).

### 4.2 The per-worker way accumulator

```rust
pub(super) struct WayAcc {
    sink: RecordSink,
    bytes: usize,               // sink.payload.len() + records.len()*size_of::<PayloadRecord>()
    chunk_paths: Vec<PathBuf>,
    count: u64,
    compression: sort::ChunkCompression,
    fanout: FanoutStats,
    way_puts: Vec<(i64, Vec<(i32, i32)>)>, // buffered for the drain thread
    // geometry scratch, now owned per task instead of thread_local
    merc: Vec<Point>,
    point_emit: PointEmitScratch,
    line_emit: LineEmitScratch,
    polygon_emit: PolygonEmitScratch,
}
```

`flush(chunk_dir, chunk_id)` calls
`sort::write_sorted_payload_chunk(&mut sink.records, &sink.payload, path,
compression)`, extends `chunk_paths`, adds `count`, clears `sink.records`/
`sink.payload`/`bytes`. `WAY_WORKER_SCRATCH` (the thread-local) is deleted -
scratch moves onto `WayAcc`, owned by the task, which is correct now that one
task owns one block rather than `into_par_iter` fanning a block across the
pool.

### 4.3 The rewritten worker loop

The worker thread still owns `rayon::in_place_scope`, but the block is moved
INTO the spawned task and parsed there:

```
while let Ok(block) = brx.recv() {
    // cheap pre-scan on the worker thread: collect node_refs only (i64, no
    // tag strings) to run block-local + global shared-node annotation, which
    // needs all ways' refs before any way is processed. Produces a
    // Vec<SmallVec<i64>> of preserve-ref sets aligned to way order, OR a
    // block-local FxHashSet passed by Arc into the task. (node_refs are also
    // needed inside the task for node-store resolution / pinning, so this
    // pre-scan is the one unavoidable i64 copy - it is 8 bytes/ref, not the
    // 48+ bytes/tag the old RawWay copied.)
    let plan = build_block_shared_plan(&block, &gsn); // Section 4.5
    let block_cost = estimate_block_cost(&block, &plan);
    // condvar throttle, unchanged predicate incl. "always allow one task"
    reserve_inflight(block_cost);
    let tx = rtx.clone();
    s.spawn(move |_| {
        let mut acc = WayAcc::new(compression);
        for (way, preserve) in block.ways().zip(plan.iter()) {
            process_way_into(&way, preserve, nr_ref, mz, xz, &srl, ds_ref,
                             mr_ref, &fcs, psf, &mut acc);
            if acc.bytes >= chunk_size { acc.flush(&chunk_dir, &chunk_id); }
        }
        // do NOT flush the remainder: send it back so tiny tails merge, like RelAcc
        let _ = tx.send(WayTaskResult {
            chunk_paths: acc.chunk_paths, count: acc.count, fanout: acc.fanout,
            leftover: acc.sink,     // records+payload+tally below chunk size
            way_puts: acc.way_puts,
        });
        release_inflight(block_cost);
    });
}
```

`process_way_into` is the old `process_raw_way` body operating on a live
`pbfhogg::Way<'_>` borrowed from the block: `match_element` on borrowed
`&str` tags (zero `String`), resolve coords (locations-on-ways: `way
.node_locations()` directly into `acc.merc` region / `way_index` put buffer;
node-store: `nr.get(ref)`), build the preserve mask from `preserve`, project
into `acc.merc`, emit into `acc.sink`. It appends `(way_id, coords)` to
`acc.way_puts` (the coords it just resolved) for the drain.

`block.ways()` is `block.elements().filter_map(Element::Way)`; sorted PBFs
have single-type way blocks so this is the whole block. The `move` closure
takes ownership of `block`, so its lifetime is the task's - the borrow issue
that forced `RawWay` disappears because nothing outlives the task.

### 4.4 The drain thread, reduced

`WayTaskResult` carries pre-written chunk paths + the leftover `RecordSink` +
buffered way puts + fanout. The drain thread loop:

```
while let Ok(res) = rrx.recv() {
    for (id, coords) in res.way_puts { way_index.put(id, &coords); }
    sort_writer.adopt_chunk_files(res.chunk_paths);
    fanout.merge(&res.fanout);
    merge_tally(&mut sort_stats, &res.leftover);        // Section 2.4
    for (key, off, len) in &res.leftover.records {      // tail merge
        sort_writer.push(SortRecord { key: *key,
            data: res.leftover.payload[*off..*off+*len].into() })?;
    }
    ds_drain.check_budgets(&srl_drain);
}
```

Stats accounting is the subtle part, so it is specified as one decided design
rather than left to discover (Codex R1, Opus R2). The tally must cover BOTH the
flush-written chunk records AND the leftover tail records, and it must reproduce
EVERY counter `Phase12Stats` reads from `SortWriter` today - not only the
per-layer/zoom arrays but the scalar totals `total_records` /
`total_record_bytes` (and `features_emitted`, derived from record count).
`RecordSink`'s tally fields (Section 4.1) therefore include those scalars, not
just the `layer_*` arrays. The decided flow:

- `WayAcc` keeps a running `tally` that `flush` accumulates into and does NOT
  clear (so flushed-chunk records stay counted after their payload is dropped);
  `push_sort_record` updates that tally on every record.
- `WayTaskResult` carries the full `tally` separately from the leftover
  `RecordSink` payload+records.
- The drain merges the whole `tally` into `Phase12Stats` via `merge_tally`, then
  pushes the leftover tail through a new `SortWriter::push_untracked` that
  writes the record but SKIPS `SortWriter`'s own per-layer/total accounting - so
  the tail is counted exactly once (by `merge_tally`) and never double-counted.
  `push_untracked` (not the tracked `push`) is used precisely because the tail
  is already in `tally`; it still lets small tails merge with node/ocean records
  instead of spawning one-record chunk files (the `RelAcc` reasoning).

This keeps `Phase12Stats` bit-exact against today across layers, zooms, and
totals. The same `push_untracked` + arena-owned tally is applied to `RelAcc`
(Section 4.7), which incidentally fixes the pre-existing relation undercount
noted in Section 2.4.

Way ordering into `way_index` no longer matters (external-sorted); way puts
from concurrent tasks interleave freely.

### 4.5 Block-local + global shared-node plan

`build_block_shared_plan(&block, &gsn)` reproduces
`annotate_block_shared_node_refs` + `annotate_global_shared_node_refs` on the
live block: one pass counting node-ref occurrences within the block
(`FxHashMap<i64,u8>`, closed-ring dedup of the closing vertex as today), then
per way the preserve set = refs with block-count >= 2 OR present in `gsn`. It
returns preserve data aligned to way order (a `Vec<SmallVec<[i64; 4]>>` or a
per-way bitmask against that way's ref list). Per i64 ref it is cheaper than the
old `RawWay` extraction (no tag strings, no coord clone), but note it is an
ADDED serial pass over the block's elements, not a like-for-like swap (Opus R2):
the worker thread now decodes elements for this pre-scan, and the task decodes
them again for tags/coords, so a block's elements are decoded twice (plus once
more if `way_count` counting re-parses - fold that count into this pre-scan to
avoid a third pass). The pre-scan sits on the critical serial dispatcher path;
it is expected to stay cheap (i64 refs only), but "strictly cheaper than the old
extraction" understates that it is an extra decode - watch it in the germany
serial-phase timing. The block-local vs global split and the closed-ring
closing-vertex handling are preserved bit-for-bit so pinning - and therefore
output - is identical.

### 4.6 In-flight budget must count block-held-alive bytes

The old throttle counted `estimate_raw_ways_bytes * 10`. Now the block itself
is held alive inside the task, so `estimate_block_cost` counts the block's
decoded byte size (`block` retains its backing buffer) PLUS an output estimate
for the arena. Keep the `way_budget` byte semantics and the "always allow at
least one task" deadlock predicate verbatim (Section 2.7). Record the HWM
(`max_way_inflight_bytes`) from the block+arena estimate. `RawWay`,
`estimate_raw_ways_bytes`, `ProcessedWay`, `drain_processed_ways`,
`WayWorkerScratch`, `WAY_WORKER_SCRATCH` are deleted.

**`MAX_INFLIGHT` must NOT be kept verbatim - it caps concurrency and would sink
the keep gate.** Today one block spawns `raw_ways.into_par_iter().map(process
_raw_way)`, so a single in-flight block fans its ways across the ENTIRE rayon
pool; `MAX_INFLIGHT = 8` bounds only how many blocks are decoded ahead, not the
worker count. Section 4.3's one-`s.spawn`-per-block serial `for` loop occupies
exactly one rayon thread per block (rayon cannot steal a slice of a serial
loop), so with `MAX_INFLIGHT = 8` at most 8 threads run - 16 of 24 idle on the
5900X reference host, far worse on a 64+-thread planet server. That directly
regresses the germany wall the landing is gated on. Resolution: `MAX_INFLIGHT`
becomes `max(config.threads, 8)` (or a small multiple of `config.threads` to
keep the read thread ahead), so the block-per-thread model saturates the pool;
the `way_budget` byte ceiling stays the real backpressure, and the deadlock
predicate is unchanged. If per-block granularity proves too coarse (blocks vary
in way count, so tail blocks would leave threads idle), the alternative is to
keep bounded intra-block parallelism - `block.ways().par_bridge()` (or collect
refs and `into_par_iter`) into per-thread `WayAcc` arenas merged at task end -
which restores full-pool fan-out per block; pick per the germany `--bench 3`
(Opus R2 blocker). Either way the "keep MAX_INFLIGHT verbatim" instruction above
is void.

### 4.7 Landing 1 stopping line

Landing 1 does NOT touch: the node phase (`handle_node!` inline path stays),
relation processing, the shared-node prepass internals (still
`FxHashSet<i64>`), ocean, int_ocean, the sort/merge/assemble phases. It
converts the relation emitters' shared record parameter to `&mut RecordSink`
(unavoidable - they share the four emit functions), so `RelAcc` is updated to
carry a `RecordSink` and flush via `write_sorted_payload_chunk`; this is a
mechanical follow-through, gated by the same `--tol 0`.

### 4.8 Landing 1 gates

Regress-identical (Section 2.7) is the correctness bar; the win is the germany
`--bench 3` phase12 drop. Commit first, then run.

The `data/probes/<dataset>-<hash>.pmtiles` archives the regress/oracle gates
consume are not conjured - they are produced by a plain (non-bench) tilegen of
each dataset into that path, e.g.
`brokkr tilegen --dataset germany -o data/probes/germany-<commit>.pmtiles`
(and denmark/norway), run once per commit under test. The `-9994e5f`/`-a0fca65`
/`-b26b335` blessed baselines are the same command at those hashes (via
`--commit`), generated once and kept. Substitute the real short hash for
`<commit>` in every command below; they are otherwise copy-pasteable (Codex R1).
Commands:

- `brokkr check`
- `brokkr tilegen --dataset germany -o data/probes/germany-<commit>.pmtiles`
  (and denmark, norway - produces the current-commit probes the regress/oracle
  gates read)
- `brokkr tilegen --bench 3 --dataset germany` (primary keep gate)
- `brokkr tilegen --bench 3 --dataset denmark` (neutrality)
- `brokkr tilegen --bench 3 --dataset norway` (neutrality + relation path)
- `brokkr verify pmtiles --dataset germany`
- `brokkr verify pmtiles --dataset denmark`
- `brokkr verify pmtiles --dataset norway`
- `elivagar regress data/probes/germany-<commit>.pmtiles --against data/probes/germany-9994e5f.pmtiles --tol 0`
- `elivagar regress data/probes/denmark-<commit>.pmtiles --against data/probes/denmark-a0fca65.pmtiles --tol 0`
- `elivagar regress data/probes/norway-<commit>.pmtiles --against data/probes/norway-b26b335.pmtiles --tol 0`
- from `scripts/validate/`: `node earcut-oracle.mjs ../../data/probes/germany-<commit>.pmtiles` (0 deviant, 0 misattached)
- `brokkr tilegen --dataset germany --skip-to sort` immediately after the full
  germany run, then `brokkr verify pmtiles --dataset germany` (chunk-adoption
  path still valid after the direct-flush change)

Keep bound: germany phase12 must drop (target: the serial-extraction / tag-clone
and per-record-`Box` share of the 754 thread-s becomes wall time recovered - NOT
the ~187s node-store share, which Landing 2 owns; see the credit note in Section
3. A conservative proceed threshold is germany wall `< 220s`, phase12 `< 175s`,
read from `--bench 3` best-of, > 5% is signal - but see Section 4.6: if the
concurrency ceiling is left at 8 this gate will likely FAIL, so `MAX_INFLIGHT`
must scale with `config.threads` before this bench is read). denmark and norway
wall must not regress > 5%. Peak RSS (bench mode only) must not exceed the
baseline by more than the block-held-alive delta; state the measured delta and
accept it if < 2 GB on germany (blocks now live through their task). If output
is not byte-identical (`--tol 0` shows ANY structural diff or `tolerance_moved
> 0`), the landing is wrong - fix or revert, do not re-bless. Record numbers
in `reference/performance.md` against the commit hash.

## 5. Landing 2 (item 22): selective resolution + relation planning

Reuses Landing 1's `WayAcc`/task/drain verbatim; changes only the task's
decisions and adds one prepass. Still output-byte-identical.

### 5.1 The relation-planning prepass

A second prepass thread, spawned alongside the shared-node prepass (before the
node phase), streaming relation blobs:

```rust
fn prepass_relation_plan(pbf_path, decode_threads)
    -> Result<RelationPlan, PipelineError>;

struct RelationPlan {
    needed_ways: FxHashSet<i64>,   // union of member way ids of matching relations
    // relation skeletons: matches + is_boundary + ordered member (way_id, role)
    // + non-way/nested member tallies (Section 5.4), so the relation phase does
    // not re-read/re-match relation blocks yet reproduces every counter.
    skeletons: Vec<RelSkeleton>,
}
```

Uses `reader.with_blob_filter(BlobFilter::only_relations())` (confirmed
present in pbfhogg 0.4.x / the local `../pbfhogg`). For each relation it does
the same fast reject + `match_element` on borrowed tags that
`prepare_relation` does today (type multipolygon/boundary, non-empty
Shortbread match); for matching relations it records the skeleton and inserts
every `MemberId::Way(id)` into `needed_ways`. Overlaps the node phase; joined
at the first way block (like the shared-node prepass). Because relation
matching now happens here, the buffered-`relation_blocks` re-read at end of
phase is replaced by draining `skeletons` - the relation phase resolves member
geometry from `way_index.get` and processes, but no longer re-parses or
re-matches relation PBF blocks. The skeleton must carry enough to reproduce
every counter and drop rule of today's `prepare_relation` (Section 5.4) - matches,
`is_boundary`, ordered members, non-way/nested tallies, and `rel_count`.

**IO contention of a third concurrent reader** (Opus R2): Landing 2 adds
`prepass_relation_plan` (`only_relations`) alongside the existing shared-node
prepass (`only_ways`) and the main node read - three concurrent full-file scans.
On the planet target (64 GB RAM, mmaps do NOT fit page cache per MEMORY.md)
three simultaneous scans can thrash the page cache. Assessment: the relation
prepass reads only relation blobs (a small fraction of a planet PBF - relations
are ~0.1% of elements) and overlaps only the node phase, joining at the first
way block, so its scan is short and finishes early; the shared-node prepass
likewise reads only way blobs. The two filtered prepasses plus the node read are
three sequential-within-each-file scans of DISJOINT blob subsets, so the working
set is the union, not 3x. This is judged acceptable, but is a real planet-scale
risk to watch on bring-up - if the germany bench shows prepass-vs-node-phase
contention (node phase wall regresses with the relation prepass live), the
relation prepass can be serialized after the shared-node prepass instead of
alongside it. Named, not gated here (no planet run in scope, Section 7).

### 5.2 The gated way task

`process_way_into` reorders to classify before resolving. The reorder has one
non-obvious hazard that must be handled for output identity (Codex R1 High):
today `match_element` is called with a `geom_type` of `ClosedWay`/`OpenWay`
**derived from the RESOLVED coords** (`coords_e7.len() >= 4 && first == last`,
phase12.rs), not from the raw node refs. A closed OSM ring whose shared closing
node is missing from the store resolves to an open coord list, so
resolved-closure can differ from ref-closure. Classifying before resolving
therefore cannot use ref-closure as a drop-in - it could flip
`ClosedWay`/`OpenWay`, flip the match result, and change which ways are emitted
(output diff, not just a stat). The step order that preserves output:

1. `let is_member = plan.needed_ways.contains(&way_id)`;
2. build `tags_ref` on borrowed `&str`. Run a **tag-only pre-filter**: if the
   way's tags cannot match under EITHER geom_type (the union of the
   `ClosedWay`/`OpenWay` matchers is empty), it is a guaranteed non-feature -
   this reject needs no coords and no closure. If also `!is_member`, **return
   immediately** - no coord resolution, no node-store lookup, no `way_index`
   put, no emit. This is the item-22 win: on the node-store path it skips
   `find_chunk_in_blob`/`decompress_chunk` for every way that is neither a
   possible feature nor a relation member (the 187 thread-s germany standard-
   path sink); on every path it skips storing geometry for the ~95% of ways no
   relation reads back;
3. otherwise resolve coords (a possible-feature or a member);
4. compute `geom_type` from the resolved coords EXACTLY as today, then
   `let matches = match_element(&tag_helper, geom_type)` - so closure and the
   final match are byte-identical to Landing 1 for every way that survives the
   pre-filter;
5. if `matches` non-empty: emit into `acc.sink` (unchanged);
6. if `is_member`: `acc.way_puts.push((way_id, coords))` - **only members are
   stored**. Non-member matching ways are emitted but not stored (they are
   never `get`).

The pre-filter in step 2 must be a true SUPERSET of the real matcher (it may
only reject ways that both geom types would reject), or a matching way could be
dropped. Simplest sound form: match under both `ClosedWay` and `OpenWay` on the
borrowed tags and reject only if both are empty; tag matching is cheap relative
to a node-store lookup, so this is still a large net win. If the Shortbread
matcher makes an all-geom-types tag pre-screen awkward, fall back to resolving
coords for every non-member way (keeping only the `is_member`-gated `way_index
.put` as Landing 2's win) and revisit the resolution skip as a follow-on - state
which form was taken and why in the commit.

Shared-node pinning still needs a way's refs even when the way itself is
dropped, because a dropped way can still contribute a shared junction to a
KEPT way - so `build_block_shared_plan` (Section 4.5) still scans ALL ways'
refs; only per-way geometry resolution/emission/storage is gated. This
preserves pinning and thus output.

### 5.3 Why output is identical

Ways skipped at step 2 reject under BOTH `ClosedWay` and `OpenWay`, so today -
whatever their resolved closure turned out to be - they produced zero records;
they were `put` into `way_index` only to satisfy a `get` that never came (no
relation references them - that is exactly `!is_member`). Ways that survive the
pre-filter are resolved and matched with the exact resolved-closure `geom_type`
of Landing 1 (Section 5.2 step 4), so their records are byte-identical. Matching
non-member ways: emitted identically, and their absence from `way_index` is
invisible because nothing `get`s them. Relation geometry is unchanged: every
member way is in `needed_ways`, so every member is still resolved and stored.
Pinning is unchanged (Section 5.2 / 4.5 scans all refs). Therefore `--tol 0`
holds. NOTE this is output identity only - diagnostic counters change, see
Section 5.4.

### 5.4 Risks resolved inline

- **Relation-prepass memory at planet scale**: `needed_ways` holds member way
  ids of matching relations only (planet ~ a few M relations x tens of members
  = O(100M) ids worst case, ~1-2 GB as `FxHashSet<i64>`; acceptable, and far
  below the way-geometry it deletes). If this proves tight, `needed_ways`
  becomes a sorted `Vec<i64>` + binary search, or a rank bitset over the way
  space; state which if the germany prepass RSS shows pressure.
- **Missing member refs / unusual PBF ordering**: a member way id in
  `needed_ways` that never appears in a way block is a missing ref, recorded
  exactly as `prepare_relation` records it today
  (`missing_ref_stats.record_relation_missing_way_ref`); the relation phase
  reads the same `missing` counters. Way blocks after relation blocks
  (locations-on-ways) are already handled by deferring the relation phase to
  end-of-read; the prepass reads relation blobs independently of block order.
- **Way-missing-NODE counters legitimately drop (accepted, not a bug)** (Codex
  R1, Opus R2): `record_way_missing_nodes` fires today INSIDE coord resolution
  (phase12.rs), before and independent of tag matching. Landing 2 returns from
  pre-filtered non-member ways BEFORE resolving, so those ways' missing nodes
  are never counted - `missing_way_node_refs` / `ways_with_missing_node_refs`
  fall to the resolved subset (features + members). This is correct behaviour
  (those ways contribute no geometry), does NOT affect `--tol 0` (the counters
  are diagnostic, not output), and is expected. The gate must therefore NOT
  assert way-missing-node equality against Landing 1 - see the rewritten
  Section 5.5. Only the RELATION missing-way-ref counters must stay exact.
- **The relation skeleton must carry ALL of `prepare_relation`'s observable
  accounting** (Codex R1 High): walking members today records not just missing
  way refs but `record_relation_non_way_member` (node/relation members),
  `record_relation_nested_member` (relation members), and
  `record_relation_with_missing_way_refs` (the per-relation flag), derives
  `is_boundary` from tags, and returns `None` (dropping the relation) when
  `member_ways.is_empty()`. It also feeds the total `rel_count`. If the prepass
  reduces relations to `needed_ways` + a way-only skeleton and the relation
  blocks are no longer re-parsed, every one of those counters and the
  empty-members drop must be reproduced by the prepass/skeleton, or the relation
  phase must re-walk full membership. `RelSkeleton` therefore carries: the
  matches, `is_boundary`, the ordered `(way_id, role)` members, AND the non-way
  member tallies (non-way count, nested count) captured at prepass time; the
  relation phase re-derives per-relation missing-way-ref flags from
  `way_index.get` misses as today, and preserves `rel_count`.
- **`prepare_relation` no longer re-matches**: its match + skeleton now come
  from the prepass; the relation phase's `flush_rel_batch` consumes skeletons.
  Keep `prepare_relation` as the skeleton->geometry resolver (member
  `way_index.get` + `MemberWay` build), matched already.

### 5.5 Landing 2 gates

Same command set as Section 4.8, against the Landing-1 blessed archives
(re-bless denmark/germany/norway at Landing 1's kept commit first, so Landing
2's `--tol 0` reads against a Landing-1 archive). Primary keep gate germany;
norway is the critical relation-path gate (777K relations - the prepass and
selective read-back must not drop or duplicate a single member). Additional:

- `elivagar regress data/probes/germany-<commit>.pmtiles --against data/probes/germany-<landing1-commit>.pmtiles --tol 0`
  (and denmark, norway likewise) - zero structural diffs, `tolerance_moved 0`.
- RELATION missing-way-ref counters (`relation_missing_way_ref`,
  `relation_with_missing_way_refs`) plus the non-way / nested member counters
  must match the Landing-1 run exactly (no member silently dropped or
  miscounted). The WAY missing-NODE counters (`missing_way_node_refs`,
  `ways_with_missing_node_refs`) are EXPECTED to fall to the resolved subset
  (Section 5.4) - do NOT gate them against Landing 1; instead record the new
  values and confirm the drop equals the pre-filtered non-member way count.
  (An earlier draft demanded all missing-ref counters match exactly - that is
  self-contradictory with the resolution skip; Opus R2.)

Keep bound: germany phase12 drops further (node-store lookups skipped for
non-feature-non-member ways; way_data.bin shrinks from all-ways to member-
ways). way_index size is a recordable proxy - state the germany
`way_offsets.bin` entry count before/after (should fall to ~ the member-way
count). denmark/norway must not regress > 5%; output `--tol 0`.

## 6. Landing 3 (item 15 half 2): compact shared-node counters

Planet-scale memory requirement for the surviving shared-node prepass. No
planet run exists to gate it, so per the technical-spec instrument-first rule
this landing is priced by Landing 0 below and its win is a by-construction
memory bound, gated for CORRECTNESS (output identity) and for the phase12
peak-RSS not regressing on the three datasets.

### 6.0 Landing 0 (instrument, lands first): price the estimate

`prepass_shared_nodes` already prints `{seen_count} unique nodes`. Before any
counter rewrite, read it on germany and extrapolate:

- capture the prepass log line's unique count `U_de` and germany way count.
  Reading `{seen_count}` does NOT require a dedicated `brokkr tilegen --dataset
  germany` run (that is a full real-PBF pipeline, gated behind explicit user
  request per AGENTS.md) - lift the number from any existing germany bench
  sidecar/log, or from the next germany bench this campaign already runs (Opus
  R2). Only run a dedicated germany pipeline if no such log exists.
- Planet `seen` estimate: `U_planet ~= 2.0e9` unique node ids (memory note).
  An `FxHashSet<i64>` at ~50% load is ~48 bytes/slot -> `~96 GB`. State the
  arithmetic against `U_de` (germany's `U_de x (planet_ways / germany_ways)`
  as a cross-check on the 2e9 figure).
- **Proceed/close threshold**: if the extrapolated `seen` set is `< 20 GB`
  (comfortably inside the 64 GB box beside the ~600 MB node store and the
  pipeline's clean 10-15 GB working set), item 15 half 2 CLOSES as mispriced
  and Landing 3 is never laid. If `>= 20 GB` (it will be ~96 GB), proceed.

Landing 0 writes no code beyond reading an existing log line; its
"gate" is the recorded number and the proceed/close verdict in this file.

### 6.1 The exact replacement

`prepass_shared_nodes` returns `shared` = ids seen 2+ times. Replace the
`seen`/`shared` `FxHashSet` pair with an EXACT count so `shared` is identical
(output depends on `shared` via pinning, so any inexactness - e.g. a Bloom pair
whose false positives over-pin - changes output and fails `--tol 0`).

**The shipped mechanism is ONE exact external count, used on BOTH paths.**
Codex R1 and Opus R2 both flagged that a rank-indexed counter is not buildable
without either buffering all way refs (the 30-60 GB this landing exists to
delete) or re-scanning ways serially after node-store finalization (killing the
prepass's overlap with the node phase). The rank approach also assumes the
node-store path, so it would leave the production locations-on-ways path shipped
with zero gate coverage. Committing to the external count for both paths removes
the "measure and pick" hole and means every dataset gate in 6.2 exercises the
one mechanism that ships:

- **Both paths (node-store AND locations-on-ways)**: stream way `node_refs` to a
  size-budgeted external sorter (reuse the `SortWriter` chunk/merge machinery,
  keyed on the raw i64 node id), then in the merged, sorted stream mark an id
  `shared` when it appears in 2+ positions. Exact -> `shared` bit-identical ->
  output identical -> `--tol 0`. This is rank-free and independent of node-store
  timing, so it keeps the existing overlap: the way-blob scan that feeds the
  sorter runs concurrently with the node phase exactly as
  `prepass_shared_nodes` does today; only the in-RAM `seen`/`shared` sets are
  replaced by the on-disk sort. Memory is bounded by the sort budget, not by
  unique-node count, so the planet `seen` monster (~96 GB) is gone by
  construction. **Bloom-pair is explicitly rejected**: false positives over-pin
  and change output.

- **Rank-indexed 2-bit counter is deferred, not shipped** (optional follow-on):
  if the external count measurably regresses the germany serial prepass (it was
  79s and is on the critical serial section), a 2-bit-per-rank counter over a
  `SortedNodeStore::rank(id)`/`id_at_rank(r)` API (~500 MB planet, unit-tested
  against `get`) is the CPU-cheaper alternative FOR THE NODE-STORE PATH ONLY -
  but it must run after store finalization, which trades away the node-phase
  overlap, so it is only worth it if that overlap is not on the critical path.
  Named here and excluded from this landing; revisit only against a measured
  prepass regression, and never for the locations path (no rank exists there).

If the external count's planet-scale IO cost proves unacceptable, that is a
follow-on measured on a real planet run (out of scope, Section 7) - the exact
external count is what this spec lands.

### 6.2 Landing 3 gates

- `brokkr check`
- `brokkr tilegen --bench 3 --dataset germany` (exact external count on the
  node-store path; phase12 peak RSS via bench-mode sidecar must not regress;
  the external-count prepass serial time must not regress > 5% - it was 79s
  germany and is on the critical serial section)
- `brokkr tilegen --bench 3 --dataset denmark`
- `brokkr tilegen --bench 3 --dataset norway`
- **locations-on-ways coverage** (Opus R2): the production path has no node
  store, so at least one dataset must run `--variant locations` -
  `brokkr tilegen --dataset denmark --variant locations` then
  `elivagar regress` at `--tol 0` and `brokkr verify pmtiles` - to exercise the
  external count in the mode it ships in. Without this, the production path
  lands with zero gate coverage.
- `elivagar regress data/probes/germany-<commit>.pmtiles --against data/probes/germany-<landing2-commit>.pmtiles --tol 0` (and denmark, norway) - the exact count must reproduce `shared` bit-for-bit, so zero diffs
- `brokkr verify pmtiles --dataset germany`
- The prepass log line's `{} shared` count must equal the Landing-2 run's
  shared count exactly on all three datasets (proves the counter is exact).

Keep bound: output `--tol 0`; germany phase12 peak RSS (bench mode) not above
Landing-2 baseline; prepass serial time within 5%. The planet-memory win
(~96 GB -> ~500 MB node-store / bounded external count) is stated
arithmetically and accepted by construction - it cannot be bench-verified
without a planet run, which is out of scope (Section 7). If the exact count
regresses germany prepass time > 5% with no compensating phase12 win, keep the
memory rewrite only if the estimate threshold (Landing 0) confirms the box
would otherwise OOM at planet - the memory correctness is the deliverable, a
small serial-time cost is the accepted price, stated as such.

## 7. Stopping rule and scope

In scope: the way phase data flow (Landing 1), selective resolution + relation
planning (Landing 2), the shared-node prepass memory (Landing 3), and the
mechanical converge of the relation emitters onto `RecordSink`. The node
inline path, ocean, int_ocean / spec-4's descent, the sort/merge machinery,
and the assemble phase are NOT rewritten - Landing 1 touches `SortWriter` only
to add `push_untracked` and the arena helpers, not its merge/reader model
(that is P3 / item 14, a named separate TODO). The `--skip-to sort`/`ocean`
checkpoint contract is preserved (chunk files remain the standard on-disk
format; direct-flush chunks are already the ocean/relation norm). No planet
run is performed or gated here (production capacity is asserted by
construction; a planet bring-up is its own effort). No env-var scaffolding,
routing switches, or benchmark knobs survive any landing.

Out of scope, explicitly named (not deferred work of these items): item 14
(partitioned sort / parallel assemble reader, P3), item 17 (chunk
sorting/writing off the drain, subsumed by P2+P3 landing together), item 19
(ocean compositor, P4), the descent-adjacent P1 drive-bys (items 11/13).

## 8. Ordering and green-at-every-boundary

1. **Landing 0** - instrument (read + record the shared-node estimate;
   proceed/close verdict for Landing 3). No code. `brokkr check` unaffected.
2. **Landing 1** - arena-flush way phase. `brokkr check` + `verify` green;
   `--tol 0` on all three datasets; germany `--bench 3` verdict; re-bless the
   three archives at the kept commit.
3. **Landing 2** - selective resolution + relation planning against Landing 1
   archives. `brokkr check` + `verify` green; `--tol 0`; germany + norway
   `--bench 3` verdict; re-bless.
4. **Landing 3** - compact shared-node counters against Landing 2 archives (if
   Landing 0 said proceed). `brokkr check` + `verify` green; `--tol 0`;
   germany `--bench 3` RSS/time verdict.

Each boundary keeps `brokkr check` and `brokkr verify` green and output
`--tol 0` identical (Landings 1-3 are all regress-neutral, Section 2.7).
Benchmark discipline per `reference/performance.md`: commit, then `--bench 3`
best-of, then write hash-anchored numbers into `reference/performance.md`. A
single-run delta or a denmark-only verdict is not a verdict; the germany bench
is the keep signal for every landing.

## 9. Review resolutions (codex R1 + opus R2)

Two outside reviews (`notes/spec-6-review-codex.md`, `notes/spec-6-review-opus.md`)
were validated against the code and folded above. This ledger records where each
landed and what was rejected. Findings that overlapped between the two reviews
are merged.

Folded (each verified against `src/`):

- **Landing 1 parallelism cap (Opus BUG, blocker)** - `MAX_INFLIGHT = 8`
  (phase12.rs) + one serial `s.spawn` per block caps concurrency at 8 vs today's
  whole-pool `into_par_iter` fan-out. Fixed in Section 4.6 (scale
  `MAX_INFLIGHT` with `config.threads`, or par_iter the block into per-thread
  arenas) and cross-referenced from the 4.8 keep bound and the Section 3 credit
  note. Verified: `MAX_INFLIGHT = 8`, `raw_ways.into_par_iter()` present.
- **Landing 2 stats + closure identity (Codex High + Opus BUG)** -
  `record_way_missing_nodes` fires during resolution before matching, and
  `geom_type` is derived from RESOLVED coords, both in `process_raw_way`
  (phase12.rs). Fixed: Section 5.2 reorders to a both-geom tag pre-filter that
  resolves before computing closure/matches for survivors (output identity
  preserved); Section 5.4 accepts the way-missing-NODE counter drop; Section 5.5
  gate rewritten to stop asserting way-missing-node equality. Verified against
  phase12.rs resolution + closure logic.
- **Relation skeleton drops accounting (Codex High)** - `prepare_relation`
  records non-way / nested / missing-way-ref counters, derives `is_boundary`,
  drops empty-member relations, feeds `rel_count` (relations.rs). Fixed: Section
  5.1 struct comment + Section 5.4 require the skeleton to carry all of it.
  Verified against relations.rs.
- **Landing 3 rank-count buildability + zero-coverage (Codex High + Opus GAP)** -
  rank needs the finalized store, breaking the node-phase overlap; and all gates
  ran the node-store variant, leaving the production locations path untested.
  Fixed: Section 6.1 commits to ONE exact external count for both paths (rank
  demoted to a named optional follow-on), and 6.2 adds a `--variant locations`
  gate. Resolves Codex Open Question 2.
- **Sort-stats double-count (Codex Medium + Opus SMELL)** - Section 4.4's
  mid-stream self-correction replaced by a decided design (`push_untracked` +
  arena-owned tally covering layer arrays AND scalar totals); `RecordSink`
  (4.1) gains `total_records`/`total_record_bytes`.
- **Encoder in wrong module (Codex Medium)** - the append primitive already
  exists as `wire_format::append_feature_data_with_attrs` returning a `Range`;
  Section 4.1 now points at it instead of inventing `sort.rs::..._into`.
  Verified in wire_format.rs.
- **Gate commands not runnable (Codex Medium)** - Section 4.8 now gives the
  probe-generation `brokkr tilegen -o data/probes/...` command and states the
  `<commit>` substitution.
- **"byte-identical" mislabel (Opus SMELL)** - defined in Section 2.7 as
  regress-`--tol 0` identity (not literal `cmp`), since the rewrite reorders
  records; usage clarified through Sections 3/8.
- **Landing 1 win miscredited (Opus SMELL)** - Section 3 credit note: the ~187s
  node-store share of the 754 thread-s is Landing 2's, tag-string cloning is on
  the serial thread (not in `process_raw_way`); Landing 1 removes only the `Box`
  + serial extraction. Verified: `nr.get` resolution is inside `process_raw_way`.
- **Three concurrent PBF readers (Opus GAP)** - IO-contention paragraph added to
  Section 5.1 (judged acceptable: disjoint blob subsets, short relation scan;
  named as a planet bring-up watch item with a serialize-the-prepass fallback).
- **Added serial decode pass (Opus NIT)** - Section 4.5 now flags the pre-scan
  as an extra block decode on the serial dispatcher, not a like-for-like swap,
  and folds `way_count` into it.
- **Landing 0 needs a full run (Opus NIT)** - Section 6.0 lifts `{seen_count}`
  from an existing bench log rather than mandating a dedicated germany pipeline.

Rejected / not folded:

- Nothing was rejected as wrong. Both reviews were accurate against the code.
- Codex Open Question 1 (should skipped-way missing-node telemetry change?) is
  answered, not open: yes, it legitimately falls to the resolved subset (Section
  5.4) - folded as a decision, not left as a question.
- The rank-indexed 2-bit counter is not "rejected" but deferred: kept in Section
  6.1 as a named optional CPU optimization for the node-store path only, gated on
  a measured external-count prepass regression. It is out of this spec's landing
  scope, not discarded.
- Opus's "Things that check out" list (arena containment, `write_sorted_payload
  _chunk` byte-identity, `PrimitiveBlock: Send`, pinning-preservation, the
  16-then-22 ordering) are confirmations, not findings; independently
  re-verified during this pass, no spec change needed.
