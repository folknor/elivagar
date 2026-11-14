# Bugs

Defects and surprises the 2026-10-28 design hunt turned up on the way. The
hunt asked a structural question; these are what the hunters flagged
laterally. None has been verified - the fix pass verifies, and is expected to
find phantoms.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

---

# Run contract, provenance and resume

## BUG-065 - Sort records read back from chunk files are not checked against their partition

`sort/format.rs` `read_record` and `next_header_discard_payload` return the
raw key, and the merge reader never re-derives its partition. Keys are
checked where they are minted (`make_sort_key` refuses tiles beyond z14) and
in `partition_from_key`, but a corrupted or foreign chunk record reaches
assembly unchecked. A key for a z15 tile then indexes slot 15 of the
`[u64; ZOOM_COUNT]` per-zoom counters in `assemble::write_encoded_item` and
panics; a valid z14 key from another partition is written out of order and
caught only by the writer's ordering check. The piece pre-scan checks the
bucket range but runs only for split partitions. Proposed: validate every
decoded key (normal reads and header-only scans) against its section's or
piece's tile range and return `InvalidData`.

Found by the coordinates-core landing's review.

---

# Ocean

---

# Geometry

---

# Shortbread profile and layers

---

# Encoding and the sort record

## BUG-038 - Per-tile `AssemblyScratch::new()` defeats the documented pooling

`MergeScratch`, `LayerBuilder::prepare_for_reuse` and `AssemblyScratch` docs
all describe a `thread_local!` reused across tiles, and the
`merge_same_attr_geometries` comment cites a 3.8 GB churn saving from keeping
scratch at high-water capacity. In fact both `encode_tile_batch_mvt` and
`encode_tile_batch_mlt` call `AssemblyScratch::new()` per tile inside the
rayon closure; there is no `thread_local!` in assemble. Pools reuse only
within one tile and `prepare_for_reuse` is a no-op on fresh builders. Either
the thread-local was removed deliberately (delete the pool plumbing threaded
through three public signatures) or lost in a refactor (the alloc profile that
justified it has silently regressed). `brokkr tilegen --alloc` settles which.
The `AssemblyScratch` header meanwhile contradicts its own constructor, which
documents the per-tile allocation as deliberate - whichever way this is
settled, the two must agree.

Reported by the encoding hunt.

---

# Found during the 2026-11-04 fix waves

## BUG-064 - Relation area ignores holes

`pipeline/relations.rs` computes a relation's area by summing its outer
rings without subtracting inner rings. That area drives the
`boundary_labels` area-based minimum zoom (the z2/z3/z4 thresholds now in
`shortbread/boundaries.rs`), so a boundary with large enclaves or lakes can
be promoted to a lower zoom than its true area warrants. Subtract the holes;
it can move `boundary_labels` min zooms.

Reported by the wave-10 relation fixer.

