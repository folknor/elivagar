# P2: Stream Relation Outputs Incrementally

## Detailed Analysis and Implementation Plan

### 1. Current Architecture (What Exists)

**Relation processing flow** (lines 556-624 of `/home/folk/Programs/elivagar/src/pipeline.rs`):

1. The PBF reader delivers relation blocks sequentially on the main thread
2. For each relation element, `prepare_relation()` runs serially on the main thread (line 587). It does:
   - Tag matching (while PBF borrows are alive, avoiding String cloning)
   - Way geometry lookup from `way_index.get()` per member (serial I/O over mmap)
   - Mercator projection of each way's coordinates
   - Returns a `PreparedRelation` with all member geometry materialized
3. Prepared relations accumulate into `rel_batch: Vec<PreparedRelation>` (line 591)
4. When `rel_batch.len() >= REL_BATCH_SIZE` (1024), the batch is flushed via `flush_rel_batch()` (line 594)

**`flush_rel_batch()`** (lines 914-937):
```rust
fn flush_rel_batch(batch: Vec<PreparedRelation>, ..., sort_writer: &mut SortWriter) -> u64 {
    let results: Vec<Vec<SortRecord>> = batch
        .into_par_iter()
        .map(|rel| process_prepared_relation(rel, ...))
        .collect();                     // <--- FULL MATERIALIZATION

    for rel_records in results {        // <--- SERIAL DRAIN
        for record in rel_records {
            sort_writer.push(record)... // <--- SERIAL PUSH
        }
    }
}
```

**Memory cost of full batch materialization:**

- `PreparedRelation` struct (line 847): `osm_id: u64` + `matches: SmallVec<[LayerMatch; 4]>` + `member_ways: Vec<MemberWay>` + `is_boundary: bool`
- `LayerMatch` is 408 bytes (line 209 of shortbread/mod.rs), SmallVec<[_; 4]> inline = ~1640 bytes
- `MemberWay` is 32 bytes + heap Vec<Point> where Point is 16 bytes
- A typical large multipolygon relation (e.g., a country boundary) can have hundreds of member ways, each with hundreds of coordinates = megabytes of geometry per relation
- With 1024 relations in a batch: the `batch: Vec<PreparedRelation>` holds all input geometry, then `results: Vec<Vec<SortRecord>>` holds all output records simultaneously
- **Peak memory = input batch + output records**, both fully materialized

**`process_prepared_relation()`** (lines 941-1020): Takes ownership of a `PreparedRelation`, runs `multipolygon::assemble()`, then emits sort records via `emit_multipolygon_feature()` and `emit_line_feature()`. Returns `Vec<SortRecord>`.

**`SortWriter::push()`** (lines 136-143 of sort.rs):
```rust
pub fn push(&mut self, record: SortRecord) -> io::Result<()> {
    self.buffer_bytes += record.data.len() + std::mem::size_of::<SortRecord>();
    self.buffer.push(record);
    if self.buffer_bytes >= self.chunk_size_bytes {
        self.flush_chunk()?;
    }
    Ok(())
}
```
Takes `&mut self` -- NOT thread-safe. The sort writer internally buffers up to 1 GB of records before flushing a sorted chunk file. This is the fundamental constraint.

### 2. Existing Parallel Patterns in the Codebase

The codebase has **two** established patterns for parallel processing with serial sort writer access:

**Pattern A: Way processing (channel-based, lines 480-547)**
- Worker thread receives PBF blocks, spawns rayon tasks
- Results flow through `sync_channel::<Vec<ProcessedWay>>(4)` to a dedicated drain thread
- Drain thread owns both `way_index` and `sort_writer`, pushes records serially
- Main thread is free to forward PBF blocks (pipeline parallelism)

**Pattern B: Ocean processing (parallel chunk flushing, ocean.rs lines 256-339)**
- `par_iter().fold()` with thread-local `OceanAcc` buffers
- Each rayon worker flushes directly to chunk files via `sort::write_sorted_chunk()`
- Uses `AtomicUsize` for chunk ID allocation
- After all processing, `sort_writer.adopt_chunk_files()` collects the chunk paths
- **Bypasses `SortWriter::push()` entirely** -- writes chunk files directly

### 3. Analysis of Options

#### Option A: Reduce batch size (e.g., 64 instead of 1024)

**How it works:** Change `REL_BATCH_SIZE` from 1024 to a smaller value (32-128).

**Pros:**
- Trivial one-line change
- Reduces peak memory proportionally (1024 -> 64 = 16x reduction in batch peak)
- No architectural change

**Cons:**
- Rayon parallelism degrades with small batches. With 12 cores and 64 items, rayon's overhead per work-steal becomes noticeable relative to per-item cost
- Does NOT solve the fundamental problem: output records are still fully collected before draining
- Still 2x materialization (input geometry + output records) even if the batch is smaller

**Memory reduction:** From ~O(1024 * avg_rel_size) to ~O(64 * avg_rel_size). For Denmark this is negligible. For planet-scale (large boundary relations), this could matter.

#### Option B: Per-relation sequential processing (no batching)

**How it works:** Process one relation at a time, push its records immediately. No rayon.

**Pros:**
- Minimum possible memory: only one relation's geometry + output at a time
- Simplest code

**Cons:**
- **Eliminates all parallelism** for relation processing
- At planet scale, relation processing is significant (hundreds of thousands of relations, some extremely complex)
- Would cause regression in phase12 time

**Verdict:** Unacceptable -- losing parallelism is a non-starter for planet-scale.

#### Option C: Channel-based streaming (adapt way processing pattern)

**How it works:**
- Main thread prepares relations serially (as now)
- Instead of batching, send each `PreparedRelation` through a bounded channel to a worker
- Worker thread uses rayon to process relations, sends `Vec<SortRecord>` back through another channel
- Drain thread pushes to sort writer

**Pros:**
- True streaming: records flow to sort writer as soon as each relation is processed
- Peak memory = channel buffer depth * avg relation size
- Maintains rayon parallelism

**Cons:**
- Significant complexity: two channels, two threads, ownership dance
- The way processing pattern is already quite complex
- Relations are processed *during* PBF reading (main thread runs `for_each_element`), so the main thread cannot also be a worker -- it must dispatch
- `prepare_relation()` needs PBF borrows alive -- cannot send relation data through a channel without cloning tags to owned Strings (which the current design carefully avoids)

**Critical problem:** The `prepare_relation()` function takes `&pbfhogg::Relation<'_>` and `&[(&str, &str)]` -- borrowed from the PBF block. It resolves matches while borrows are alive to avoid cloning tags. If we move to a channel model, we'd need to either:
1. Keep `prepare_relation()` on the main thread (as now) and send `PreparedRelation` through the channel -- this works because `PreparedRelation` is fully owned
2. Or clone tags to owned Strings -- which the code explicitly avoided

Option C1 (send `PreparedRelation` through channel) is viable.

#### Option D: Ocean-style parallel chunk flushing (RECOMMENDED)

**How it works:**
- Main thread prepares relations serially (unchanged)
- Instead of collecting into a flat `Vec<PreparedRelation>` batch, prepare relations until we have a batch, then use `par_iter().fold()` with thread-local accumulators
- Each rayon worker processes relations and accumulates sort records in a local buffer
- When the buffer exceeds `chunk_size_bytes`, flush directly to a chunk file (same as ocean pattern)
- After the parallel phase, `sort_writer.adopt_chunk_files()` collects the results

**Pros:**
- **Proven pattern** -- already works in ocean.rs at planet scale
- No channels, no extra threads, no ownership dance
- True streaming within rayon: each worker flushes independently
- Peak memory per worker = ~chunk_size_bytes (1 GB max, but usually much less since relation output is smaller than ocean)
- Maintains full rayon parallelism
- The `prepare_relation()` call stays on the main thread with PBF borrows -- no change needed there

**Cons:**
- Creates more chunk files (one per rayon-worker flush instead of one per sort_writer flush). At Denmark scale this is maybe 1-2 extra chunks. At planet scale, ocean already creates many chunks -- this is a known pattern.
- Slightly more complex than the current batch-collect, but much simpler than channel-based streaming
- Batch accumulation of `PreparedRelation` still exists (rayon needs a collection to iterate over), but output records are no longer double-materialized

**Key insight:** The real memory problem is **not** the `Vec<PreparedRelation>` batch (the input is needed for parallelism anyway), it is the `Vec<Vec<SortRecord>>` collect in `flush_rel_batch()` (line 923) which holds ALL output records for 1024 relations before draining them. The ocean pattern eliminates this by flushing records directly to disk from worker threads.

#### Option E: Hybrid -- Channel from main thread + ocean-style flushing in workers

This combines C and D: main thread sends `PreparedRelation` through a channel, workers process and flush directly to chunk files. Overkill for the problem.

### 4. Recommended Architecture: Option D (Ocean-Style Parallel Chunk Flushing)

This is the cleanest approach because it reuses an existing, proven pattern from the same codebase.

### 5. Detailed Implementation Plan

#### Step 1: Create a RelationAccumulator struct (analogous to OceanAcc)

Location: `/home/folk/Programs/elivagar/src/pipeline.rs`, near `flush_rel_batch` (~line 912)

```rust
struct RelAcc {
    records: Vec<SortRecord>,
    bytes: usize,
    chunk_paths: Vec<PathBuf>,
    count: u64,
    simp_scratch: geometry::SimplifyMultiScratch,
}

impl RelAcc {
    fn flush(&mut self, chunk_dir: &Path, chunk_id: &AtomicUsize) {
        if self.records.is_empty() { return; }
        let id = chunk_id.fetch_add(1, Ordering::Relaxed);
        let path = chunk_dir.join(format!("chunk_{id:04}.bin"));
        sort::write_sorted_chunk(&mut self.records, &path)
            .expect("relation chunk write failed");
        self.chunk_paths.push(path);
        self.count += self.records.len() as u64;
        self.records.clear();
        self.bytes = 0;
    }
}
```

This is directly modeled on `OceanAcc` from ocean.rs lines 280-303.

#### Step 2: Rewrite `flush_rel_batch` to use fold+flush pattern

Replace the current `flush_rel_batch` (lines 912-937) with:

```rust
fn flush_rel_batch(
    batch: Vec<PreparedRelation>,
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
    sort_writer: &mut SortWriter,
) -> u64 {
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let chunk_id = AtomicUsize::new(sort_writer.chunk_count());
    let chunk_dir = sort_writer.tmp_dir().to_path_buf();
    let chunk_size = sort_writer.chunk_size_bytes();

    let result = batch
        .into_par_iter()
        .fold(
            || RelAcc { records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0,
                        simp_scratch: geometry::SimplifyMultiScratch::new() },
            |mut acc, rel| {
                let before = acc.records.len();
                process_prepared_relation_into(rel, min_zoom, max_zoom, land_mask,
                    &mut acc.records, &mut acc.simp_scratch);
                for r in &acc.records[before..] {
                    acc.bytes += r.data.len() + std::mem::size_of::<SortRecord>();
                }
                if acc.bytes >= chunk_size {
                    acc.flush(&chunk_dir, &chunk_id);
                }
                acc
            },
        )
        .map(|mut acc| { acc.flush(&chunk_dir, &chunk_id); acc })
        .reduce(
            || RelAcc { records: Vec::new(), bytes: 0, chunk_paths: Vec::new(), count: 0,
                        simp_scratch: geometry::SimplifyMultiScratch::new() },
            |mut a, b| {
                a.chunk_paths.extend(b.chunk_paths);
                a.count += b.count;
                a
            },
        );

    sort_writer.adopt_chunk_files(result.chunk_paths);
    result.count
}
```

#### Step 3: Refactor `process_prepared_relation` to write into an external buffer

The current `process_prepared_relation` (lines 941-1020) returns `Vec<SortRecord>`. It needs to be modified (or a new variant created) that takes `&mut Vec<SortRecord>` and `&mut SimplifyMultiScratch` instead of allocating them internally.

New signature:
```rust
fn process_prepared_relation_into(
    rel: PreparedRelation,
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
    records: &mut Vec<SortRecord>,
    simp_scratch: &mut geometry::SimplifyMultiScratch,
)
```

This avoids per-relation allocation of the records Vec and simp_scratch (reusing the accumulator's buffers). The existing function already creates `simp_scratch` per call (line 961) -- hoisting it to the accumulator is an easy optimization.

#### Step 4: Consider reducing REL_BATCH_SIZE as a complementary measure

With the streaming approach, the batch size controls how much `PreparedRelation` input geometry is held simultaneously. The output side is now streaming. A batch size of 256-512 would reduce input-side peak while still providing good rayon parallelism (256 items across 12 cores is plenty).

However, this is optional -- the main memory win comes from eliminating the output double-materialization.

### 6. Expected Memory Reduction

**Current peak memory during relation flush (per batch):**
- Input: 1024 `PreparedRelation` structs with all member way geometry
- Output: 1024 `Vec<SortRecord>` from `par_iter().collect()`
- Both exist simultaneously between the `.collect()` on line 923 and the drain loop finishing on line 935

**After change:**
- Input: same 1024 `PreparedRelation` structs (consumed by `into_par_iter`)
- Output: per-worker accumulator buffer (~few MB each, flushed to disk at chunk_size_bytes)
- As rayon consumes `PreparedRelation` items via `into_par_iter`, their memory is freed immediately after each item is processed
- Output records are flushed to disk incrementally

**Quantification for Denmark:**
- Denmark has ~26K relations total, ~4K multipolygon/boundary relations that produce features
- Most are small (a few member ways). Average relation output is probably 10-100 sort records
- At Denmark scale, the memory savings are modest (maybe 10-50 MB peak reduction)
- The benefit is primarily structural correctness for planet-scale

**Quantification for planet-scale:**
- Planet has ~800K relations. Large boundary relations (Russia, Canada, etc.) can have 2000+ member ways
- A single batch of 1024 relations could include several of these monsters
- Each large relation's member_ways could hold 50K+ Point coordinates = ~800 KB just for geometry
- The output for one large relation across 15 zoom levels can be tens of thousands of sort records
- Conservative estimate: a bad batch could hold 500 MB+ of output records
- With streaming: peak drops to whatever single-worker chunk_size is (defaults to 1 GB, but accumulator flushes at that threshold, so actual peak is chunk_size / num_workers)

### 7. Performance Impact Analysis

**Throughput:**
- The rayon parallel processing is identical -- same `par_iter()` with same work distribution
- The fold pattern has slightly higher overhead than collect (fold merges thread-local state) but this is negligible compared to the geometry processing cost
- Disk I/O for chunk flushing happens on rayon worker threads, which could cause minor contention. However, ocean processing already does this successfully at planet scale
- Net effect: **neutral to slightly positive** (reusing simp_scratch across relations in the same worker is a minor optimization)

**Sort phase impact:**
- More chunk files means slightly more merge overhead in phase 3 (k-way merge with more k)
- For Denmark: maybe 1-2 extra chunks (negligible)
- For planet: ocean already creates many chunks, so relation chunks are incremental
- Net effect: **negligible**

**Latency:**
- Records reach disk sooner (streamed vs. batched). This does not affect wall time since sort+assemble run after all records are written
- Net effect: **none**

### 8. Risk Assessment

**Low risk:**
- The ocean pattern (`par_iter().fold()` + `write_sorted_chunk` + `adopt_chunk_files`) is battle-tested in the same codebase
- No thread safety changes -- `SortWriter` is still only accessed from the main thread
- The `adopt_chunk_files` mechanism handles chunk numbering correctly (AtomicUsize for unique IDs)
- `write_sorted_chunk` is a standalone function that writes chunk files in the standard format

**Medium risk:**
- Chunk file naming with `AtomicUsize` must be coordinated with `sort_writer.chunk_count()`. In ocean.rs, the AtomicUsize starts at `sort_writer.chunk_count()` and the sort_writer only learns about the new files via `adopt_chunk_files()`. The same pattern works here.
- If `flush_rel_batch` is called multiple times (it is -- once per 1024-relation batch), each call must pick up the correct starting chunk_id. The current ocean code calls `process_ocean_shapefile` once. For relations, each `flush_rel_batch` call needs the current chunk count. This is straightforward: `chunk_id` starts at `sort_writer.chunk_count()` which is updated by `adopt_chunk_files()`.

**Zero risk:**
- The `prepare_relation()` function is unchanged (still runs on main thread with PBF borrows)
- The `process_prepared_relation` logic is unchanged (just its output destination changes)
- The `#[hotpath::measure]` annotations remain compatible with both patterns
- Test fixtures and sort tests are unaffected

### 9. Implementation Sequence

1. **Add `RelAcc` struct** to pipeline.rs (near `flush_rel_batch`)
2. **Create `process_prepared_relation_into()`** -- refactor existing function to accept `&mut Vec<SortRecord>` and `&mut SimplifyMultiScratch` instead of allocating internally. Keep the old function as a thin wrapper if needed for hotpath measurement, or move the `#[hotpath::measure]` to the new function.
3. **Rewrite `flush_rel_batch()`** using `par_iter().fold()` + flush pattern
4. **Test:** Run `brokkr check` to verify compilation and tests pass
5. **Benchmark:** Run `brokkr bench self` on Denmark to verify no regression
6. **Optional:** Reduce `REL_BATCH_SIZE` to 256 or 512 as a complementary measure

### Critical Files

- `/home/folk/Programs/elivagar/src/pipeline.rs` - Contains `flush_rel_batch()` (lines 912-937), `process_prepared_relation()` (lines 941-1020), `PreparedRelation` struct (lines 847-852), and the relation processing loop (lines 556-624). This is the primary file to modify.
- `/home/folk/Programs/elivagar/src/sort.rs` - Contains `SortWriter::adopt_chunk_files()` (line 167), `write_sorted_chunk()` (line 201), and `SortWriter::chunk_count()`/`tmp_dir()`/`chunk_size_bytes()` accessors (lines 130-161). These are the APIs to call from the streaming pattern -- no modifications needed, just understanding.
- `/home/folk/Programs/elivagar/src/ocean.rs` - Contains the `OceanAcc` pattern (lines 280-339) that serves as the reference implementation to follow. No modifications needed.
- `/home/folk/Programs/elivagar/src/geometry.rs` - Contains `SimplifyMultiScratch` (line 419) which should be hoisted to the accumulator for reuse across relations. No modifications needed.
- `/home/folk/Programs/elivagar/src/multipolygon.rs` - Contains `MemberWay` and `assemble()` (line 60). No modifications needed, but understanding its memory profile (it borrows `&[MemberWay]` from `PreparedRelation`) confirms that the input geometry's lifetime is tied to the `PreparedRelation` consumed by `into_par_iter`.
