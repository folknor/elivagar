# Bugs

Defects the 2026-11-17 performance hunt turned up while reading the hot path.
Each is a hunter's source reading; none was reproduced. The fix pass should
expect some to be wrong. The raw reports are in the commit preceding the one
that added this file. Performance findings are in `notes/performance.md`.

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

# Ingest

## BUG-001 - Duplicate way ids make relation geometry scheduling-dependent

Reported by the phase12-actors hunter. The way index has no duplicate-id
policy: repeated way ids produce equal offset keys and lookup picks an
arbitrary equal entry (`way_index.rs`). With unordered append, conflicting
duplicates can make relation geometry depend on scheduling. Reject duplicates
or define a source-order winner.

## BUG-002 - Input identity is checked against pathnames, not open handles

Reported by the phase12-actors and ocean hunters. `same_file` compares the
replay handle with the pathname after the main reader opened it, not the two
open handles. The background hash, relation prepass, main reader and
assembly's provenance-header read each open the PBF separately, so a pathname
replacement mid-run can mix input generations, and the check does not
establish what its comment claims. Outside normal immutable-input operation.
pbfhogg's `PinnedInput` is the model for one retained identity.

## BUG-003 - Elivagar source comment describes an unbounded pbfhogg decode window that no longer exists

Reported by the phase12-actors, relations and sort hunters. pbfhogg's ordered
pipeline now has an admission gate whose permit survives reordering and
consumption (`research/pbfhogg/src/read/pipeline.rs`), but elivagar's comment
justifying its custom unordered source (`phase12/input.rs`) still describes
the old unbounded reorder window. The remaining reasons for the custom source
are bounded retention, head-of-line blocking and extra handoffs.

## BUG-004 - Malformed tag indices silently truncate an element's tags

Reported by the shortbread-matching hunter. Elivagar iterates `tags()`, whose
iterator stops at the first out-of-range string-table index
(`research/pbfhogg/src/read/elements.rs` `TagIter::next`), and block
validation does not reject such indices globally. A corrupt block yields an
archive built from a partial tag list with no error. Checked iterators exist.
Decide to reject, or document the tolerance.

## BUG-005 - pbfhogg's two element iterators visit plain and dense nodes in different orders

Reported by the shortbread-matching hunter. `for_each_element` visits plain
nodes before dense nodes within a group; `elements()` visits dense first
(`research/pbfhogg/src/read/block.rs`). Elivagar's node worker uses the former
and the sorted node store requires strictly increasing ids, so a group mixing
both representations behaves differently between the APIs. Blocks switching
to metadata-skipping iteration.

## BUG-006 - pbfhogg's geocode scanner misses duplicate string-table entries

Reported by the shortbread-matching hunter; in pbfhogg, not elivagar.
`resolve_tag_indices` (`research/pbfhogg/src/scan/way.rs`) records only the
first id per literal and matching compares ids, so a later dictionary entry
with identical text is missed.

## BUG-007 - Prepared-relation memory estimate omits match-owned allocations

Reported by the shortbread-matching hunter. `estimate_prepared_rel_bytes`
(`pipeline/relations.rs`) counts the inline struct and member geometry but not
attribute strings or spilled match and attribute vectors, so reported
in-flight bytes understate retained memory.

## BUG-008 - Memory budgets are checked only after the unit of work completes

Reported by the relations, emission, ocean, phase12-actors and
assemble-schedule hunters; one mechanism at five sites.

- Relation tail: the 8 MiB accumulator threshold is checked after an entire
  relation finishes (`pipeline/relations.rs` fold body), so one giant relation
  accumulates all its records first; shrinking capacity afterwards does not
  undo the peak.
- Way processing: flush checks run after a whole feature (`phase12/ways.rs`).
- Ocean: the 4 MiB threshold is checked after `split_for_parallel` or a whole
  work item returns; collected preparation results hold many residual
  accumulators at once (`ocean/emit.rs`).
- Way admission: tickets cover block and plan bytes and release on result
  send, not covering queued `way_puts`, emission scratch or coalescer buffers
  being written; member-coordinate clones can stay queued after release.
- Assemble: the result channel is unbounded; `parked_bytes` is charged only
  when the writer receives a batch and subtracted before its items are
  written (`assemble/schedule.rs`), so completed batches in the channel, the
  batch being drained, unused payload capacity and run-copy descriptors are
  not counted, and producers keep claiming partitions; admission is only at
  partition boundaries.

Proposed by the assemble hunter: credits attached to storage ownership,
reserved before admission and held through channels, reordering and
asynchronous writes until release, bounding descriptor counts as well as
bytes, with an exclusive allowance for oversized tiles, designed so later work
cannot take all credits the earliest unfinished range needs. The relations
hunter: write through a bounded flushing `FeatureRecordSink` during traversal.

---

# Emission and lines

## BUG-009 - Point and line tile enumeration ignores the clip buffer

Reported by the emission and line-kernels hunters. Point and line emission
enumerates tiles from the unbuffered bbox (`geometry/tiles.rs`) while line
clipping uses buffered rectangles and polygon coverage includes the buffer
(`pyramid.rs`). A point or line just inside one tile is never emitted into a
neighbour whose buffer contains it. The code establishes the omission; a
visible rendering defect was not established. Fixing it changes output.

## BUG-010 - Fan-out statistics count records, not tiles

Reported by the emission hunter. Fan-out accounting (`pipeline/stats.rs`)
counts records per layer and zoom without deduplicating tile ids, so several
clipped pieces in one tile count as several tiles touched, inflating fan-out
where fragmentation is worst.

## BUG-011 - Douglas-Peucker recursion has no depth bound

Reported by the line-kernels hunter. Both DP routines recurse
(`geometry/simplify.rs`); required vertices split at the first required
interior index, giving depth proportional to pin count on heavily pinned
lines, and adversarial splits give deep recursion and quadratic work. A stack
overflow is possible. An explicit work stack with pre-partitioned required
intervals removes it.

## BUG-012 - `MAX_LINE_VERTICES` does not bound a single input segment

Reported by the line-kernels hunter. The cap applies to concatenation; an
oversized first segment is accepted whole (`geometry/line_merge.rs`), so it is
no hard bound on chain allocation or tile geometry size.

## BUG-013 - `ring_is_simple` detects proper crossings only

Reported by the line-kernels hunter. `geometry/polygon.rs` misses touches and
collinear overlaps; a diagnostic limitation (verify), not a generation path,
and not proof of simplicity.

---

# Polygon engine

## BUG-014 - The fast rectangle clipper drops tiny contours the exact path keeps

Reported by the pyramid hunter. `clip_shape_rect_fast` drops outer and hole
contours below `Area::ONE` while the exact fallback clips with `Area::ZERO`
(`pyramid.rs`). Integer true area floors doubled area over two
(`geometry/polygon.rs`), so a nondegenerate triangle with doubled area 1 is
discarded on the fast path, and a tiny hole can vanish from base-grid
descendant geometry before the requested zoom's area policy runs.

## BUG-015 - Window thinning drops source-junction pins

Reported by the pyramid hunter. The first simplification pass pins
`params.pins`; the window-thinning pass (`thin_window_runs`) builds a new mask
of only window-run endpoints and simplifies the whole shape, so a protected
interior junction can be removed whenever any part of the ring triggers
thinning. Whether a shared boundary actually breaks depends on the neighbour.

## BUG-016 - A singleton root is emitted without clipping to its buffered rectangle

Reported by the pyramid hunter. `root_bisect` returns immediately for a
one-cell range (`pyramid.rs`). World-clamped root indices do not establish
containment for geometry crossing the world edge, so an antimeridian-shifted
polygon can reach emission outside its buffer. Separately, root selection uses
zero margin, so buffer-only neighbouring roots are omitted; changing that
changes tile presence.

## BUG-017 - Quantization rejects self-crossing rings with cancelling signed area

Reported by the pyramid hunter. `ring_is_valid` (`int_ocean.rs`) rejects zero
signed shoelace area, but a self-crossing ring can cancel to zero while
enclosing nonempty NonZero regions, so it is discarded before normalization
could repair it.

## BUG-018 - Overlay workspace reservations under-reserve

Reported by the overlay hunter. `Reserve::reserve_capacity`
(`overlay/port/extract.rs`) passes `desired_capacity - current_capacity` to
`Vec::reserve`, which reserves beyond length, not capacity (length 0, capacity
8, desired 12 reserves nothing). `KeyExpList` (`overlay/port/scan.rs`) has
the same issue. `init_fragment_buffer` (`overlay/port/grid.rs`) counts
nonvertical fragments over `[i0, i1)` while insertion also writes group `i1`,
so single-column nonvertical segments reserve nothing. Pushes stay safe;
growth moves into hot loops. Geometry is unaffected.

---

# Ocean

## BUG-019 - The low-zoom union assumes winding the reader does not establish

Reported by the ocean hunter. The union relies on NonZero composition
equalling union, but the shapefile reader groups rings by orientation and
record order without proving hole containment, and the comment states disjoint
interiors as the requirement when the real danger is contributions with
negative winding (consistently oriented, correctly nested polygons may
overlap safely). No evidence the shipped sources violate it.

---

# Sort and output

## BUG-020 - Chunk serializer narrows lengths and counts unchecked

Reported by the sort hunter. Payload lengths and record counts are cast with
`as u32` (`sort/format.rs`); command count is checked elsewhere but full
payload length and batch count are not, so oversized input writes a wrong
length or count.

## BUG-021 - Chunk readers do not validate record order

Reported by the sort hunter. Readers check tile-range membership but not
monotonic order (`sort/format.rs`), so in-range corruption can reorder layers,
ranks or payloads without tripping the archive writer's tile-order check. A
persisted-data validation gap, not evidence producers emit unsorted records.

## BUG-022 - Archive dedup treats two hashes as equality

Reported by the assemble-schedule and encode-write hunters. Matching length
and both SipHashes reuses an offset without comparing bytes
(`archive/writer.rs`). A collision is extremely unlikely but the contract is
exact content. A content index should compare retained bytes and miss safely
when it cannot.

## BUG-023 - Assemble worker panic can hang the scheduler

Reported by the assemble-schedule hunter. Error paths reach stop-and-notify,
but a worker panic has no guard; if the writer waits for that worker's batch
while other workers hold senders and sleep on the claim condition, the channel
never closes. The nested encoder join can propagate such a panic. pbfhogg's
ordered pools guard gate closure against unwinding.

## BUG-024 - Gzip level 10 passes validation and panics in zlib-rs

Reported by the encode-write hunter. Validation permits level 10
(`pipeline/mod.rs`) and the zoom mapping passes it through at z0-z12; zlib-rs
rejects levels outside 0-9 and its constructor asserts success, so the run
panics instead of returning a configuration error.

## BUG-025 - Brotli finish errors are dropped

Reported by the encode-write hunter. Brotli output finishes by dropping
`CompressorWriter`, and the dependency suppresses errors from its final
`FINISH`. With a `Vec` sink ordinary I/O errors cannot occur, but an encoder
finalization failure cannot reach the caller.
