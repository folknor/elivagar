# Spec: consume injected shared-node pins, delete the global prepass (H2b)

Status: specified 2026-07-11, not implemented. This is the standalone
implementation spec for what `notes/injected-prepass-spec.md` carries as
Brick 5 - the last unlanded code brick of the injected-prepass design
(Bricks 1-4 are DONE, Brick 6 is a user-gated measurement).

Written against `reference/technical-implementation-spec.md` (the contract
for this document). Spawned from `notes/injected-prepass-spec.md` (Brick 5
and the normative wire/semantic contract), itself spawned from
`notes/planet-30gb-roadmap.md` hypothesis H2, item (b) "Exact shared-node
pins". Measurement record: `reference/performance.md` +
`.brokkr/results.db`.

## What this landing is

One commit, elivagar only, two halves that only make sense together:

1. **Consume Way field-20 pin bitmaps** on the injected path: when the PBF
   header declares `pbfhogg.SharedNodePins-v1`, `preserve_vertex_mask` is
   filled positionally from `Way::shared_node_pins()` and the per-block
   shared-node counting in `build_way_plans` is skipped entirely.
2. **Delete the global shared-node prepass**: `prepass_shared_nodes`, its
   external-sort machinery, the `global_shared_node_pins` config/CLI flag,
   and every thread of the `gsn` set through the way worker. The disabled-
   by-default exact-global-pins feature is superseded by the injected pins,
   which are exact AND free at tilegen time.

This landing is geometry-changing ON THE INJECTED PATH ONLY: exact global
pins are a strict superset of block-local pins, so more vertices survive DP
at simplified zooms. The raw path and non-enriched locations files are
bit-identical (block-local pinning is unchanged; the deleted global prepass
was off by default and off in every recorded production run).

## Survey of the ground (HEAD `b6905de`)

### What already landed (Bricks 1-4)

- **pbfhogg side is complete** (their `29e4eabd`; vendored mirror
  `research/pbfhogg/` matches the `../pbfhogg` path dependency).
  `Way::shared_node_pins(&self) -> Option<&'a [u8]>` exists
  (`read/elements.rs`); field 20 is parsed unconditionally inside the Way
  wire parse (`read/wire.rs`, `(20, WIRE_LEN) => pins_data = ...`) - unlike
  BlobHeader field 5 there is NO reader opt-in toggle to thread, the
  accessor is simply `None` on unenriched ways.
  `HeaderBlock::has_shared_node_pins_v1()` exists (`read/block.rs`). altw
  `--inject-prepass` computes the pin bitmap over the exact ref array it
  writes, omits field 20 when all-zero, and emits both feature strings
  (`commands/altw/reframe.rs`). Brick 5 requires zero pbfhogg work.
- **The enriched gate datasets are registered and already declare the pins
  flag.** The `locations` variants of denmark, germany, norway in
  `brokkr.toml` were produced by `pbfhogg add-locations-to-ways
  --index-type external --inject-prepass --compression zlib:6` (Brick 3,
  elivagar `20c8bd7` + follow-up) and carry field 5 AND field 20. elivagar
  currently prints "SharedNodePins-v1 declared - not yet consumed; using
  block-local pins" (`phase12.rs` detection). So this landing is the
  reverse of Brick 4's shape: the data is live and waiting, the code
  activates on it the moment it lands - there is no dormant period.
- **Detection is landed.** `detect_injected_features(has_members, has_pins,
  has_locations)` returns `InjectedFeatures { members, pins }` and already
  hard-errors on a pins-or-members flag without `LocationsOnWays`.
  `injected.pins` is currently consumed only by the eprintln above.
- **Membership plumbing is landed and active** (Brick 4): `WayBlock`,
  `MemberSource::{Injected, Plan}`, `MembersForBlock::{Bitmap, Set}`,
  field-5 validation in the `UnorderedBlockSource` decode workers
  (`validate_and_take_members`: presence, encoded-vs-actual count, bitmap
  length - release-checked `PipelineError`s through the Result-typed
  decoded channel), `WayPlan.is_member` filled at plan build,
  `way_members_marked` counter. Activation readings are in
  `reference/performance.md` (2026-07-11 entries).

### The code this landing changes (`src/pipeline/phase12.rs` unless noted)

- `prepass_shared_nodes` (~185 lines): external merge-sort of every node
  ref via `BlobFilter::only_ways`, scratch dir
  `tmp_dir/shared_node_prepass`, helpers `encode_signed_i64_key` /
  `decode_signed_i64_key`, `NodeRefChunkReader`, `NodeRefHeapEntry`.
  Spawned at phase12 start only when `config.global_shared_node_pins`
  (default false; else-branch eprintln "Global shared-node prepass
  disabled - using block-local pins"). Joined into the `gsn`
  `Arc<FxHashSet<i64>>` under `WAIT.prepass_join` at the first way block,
  and again after the read loop if no way block arrived.
  `global_shared_nodes = gsn.len()` feeds a `Phase12Stats` field and the
  `global_shared_nodes` counter (`src/pipeline/stats.rs`,
  `src/pipeline/mod.rs` emission).
- `build_way_plans(block: &PrimitiveBlock, global_shared: &FxHashSet<i64>,
  members: &MembersForBlock<'_>) -> (Vec<WayPlan>, u64)`: emits one plan
  per way element (positional zip invariant), resolves `is_member` from
  bitmap or set, then runs `shared_node_counts` over every plan's
  `shared_scan_slice` and fills `WayPlan.preserve_node_refs` via
  `preserve_refs_for_way(&refs, &counts, Some(global_shared))`. Runs
  inside rayon way tasks under `BUSY.phase12_plan_build`; `gsn_ref` is
  threaded into the task closure.
- `preserve_refs_for_way(node_refs, counts, global_shared:
  Option<&FxHashSet<i64>>)`: the `Option` arm is the global-set union. Two
  tests in `shared_node_helper_tests` cover exactly that arm
  (`global_shared_pins_a_non_block_local_node`,
  `global_shared_never_pins_the_closing_dup_vertex`).
- `process_planned_way_into(way, plan, is_member, node_reader, min_zoom,
  max_zoom, seam_reconcile_layers, deferral_stats, missing_ref_stats,
  fanout_caps, polygon_simplify_factor, acc)`: the call site passes
  `plan.is_member` beside `plan` (redundant third argument). On the
  locations path `coords_e7` comes from `way.node_locations()` and
  `resolved_node_refs = plan.node_refs.clone()` (no missing-node
  compaction). `preserve_vertex_mask: Vec<bool>` is sized
  `coords_e7.len()` and filled by matching `plan.preserve_node_refs` ids
  against `resolved_node_refs` (linear scan up to 8 ids, hash set above).
  The mask is built AFTER the shortbread match early-returns, i.e. only
  for emitted ways, and flows into `emit_line_feature` /
  `emit_polygon_feature`. Downstream (`src/pipeline/emit.rs`)
  `fill_pin_keys_from_mask` keys pins by coordinate, so a closed ring's
  duplicated closing vertex shares its pin with position 0 automatically -
  the field-20 contract's mirrored closing bit needs no special handling.
- Config/CLI surface: `TilegenConfig.global_shared_node_pins`
  (`src/pipeline/mod.rs`) with its doc comment, the
  `--global-shared-node-pins` clap flag (`src/main.rs`, two sites), the
  doc-example line in `src/lib.rs`, and FIVE `TilegenConfig` literals in
  `src/pipeline_tests.rs` (the parent spec's teardown list says four -
  drift; the count at HEAD is five). No brokkr wrapper or `brokkr.toml`
  entry references the flag; it is passthrough-only, so deleting it breaks
  no tooling.
- `annotate_global_shared_node_refs` (cfg(test), `#[allow(dead_code)]`) -
  already dead.
- Test infrastructure (`src/pipeline_tests.rs`): the injected fixture
  writes real PBFs via pbfhogg's `PbfWriter` / `BlockBuilder` /
  `HeaderBuilder` with `PbfCompression::None`, and splices BlobHeader
  field 5 in with the byte-level `splice_way_members` helper.
  **`BlockBuilder` cannot write field 20** -
  `add_way_with_locations(id, tags, refs, locations, metadata)` has no pin
  parameter; the only field-20 encoder is altw's internal reframe path. A
  fixture with pinned ways therefore needs a new splice instrument (priced
  in the bricks below).

### Survey corrections to the parent spec (sibling reconciliation)

The parent's Brick 5 section was written 2026-07-09; four of its premises
are stale at HEAD and this spec's gates are restructured accordingly:

1. **The blessed denmark archive is a locations-variant run, not raw.**
   The parent's stopping rule ("the blessed denmark archive is raw-variant
   and stays valid throughout; nothing here rotates the regress baseline")
   is refuted by `reference/technical-implementation-spec.md`'s gate
   discipline (added `f7cc6b8`, 2026-07-10): blessed regress references
   are locations-variant runs, and a cross-variant regress read is the
   2026-07-09 false alarm. `brokkr.toml` pins
   `blessed/denmark-c9362c4.pmtiles`. Consequence: since H2b changes
   locations-path geometry, this landing BREAKS the standing denmark
   regress zero-diff against blessed, and a post-landing bless rotation
   (user say-so) is required follow-through. The raw-path bit-identity
   proof must run against an explicitly saved pre-landing raw archive,
   never against blessed.
2. **`brokkr verify pmtiles` is unrunnable on this project** (recorded in
   `reference/performance.md`, 2026-07-11: brokkr's verify resolves only
   brokkr.toml-pinned pmtiles entries and this project pins none). The
   parent's `brokkr verify pmtiles --dataset denmark` gate is replaced by
   `node scripts/validate/validate.mjs <file>` (vtvalidate structural
   pass, takes a file path directly) plus the earcut oracle plus the
   full-decode semantic regress.
3. **`brokkr regress` takes `--file`, `--against`, `--tol`, `--max-moved`,
   `--json`** (verified against the installed CLI). The injected-path
   landing therefore gets a real semantic-regress gate with explicit
   tolerance, which the parent could only approximate with compare-tiles.
4. **Output archives are named `target/<dataset>-<commit>.pmtiles` with no
   variant component** - a raw run and a locations run at the same commit
   clobber one file. The gate ordering below copies each baseline archive
   aside immediately after the build that produces it.

Failure history: `notes/rendering-postmortem.md` is the surviving geometry
failure ledger; nothing in it involves pin injection or vertex-retention
changes. The pin-adjacent history is exactly the two facts the parent spec
records: the global prepass was disabled on cost, and the block-local
approximation was accepted with cross-block junctions unpinned as a known
quality compromise. No logged failure is re-proposed here.

## Contract recap (consumption-relevant subset)

`notes/injected-prepass-spec.md`, "The contract", stays normative; this
landing consumes it. What the consumer must honor:

- Field 20: LSB-first bitmap over ref positions; length exactly
  `ceil(ref_count/8)` WHEN PRESENT; OMITTED means "no pins" and is never
  corruption (indistinguishable from a genuinely pinless way). Bit i =
  the node at ref position i is shared AND resolved (the ratified D9
  refinement); a closed ring's trailing duplicate mirrors bit 0.
- Release-checked validation (not `debug_assert!`) on the injected pin
  path: `bitmap.len() == refs.len().div_ceil(8)` when the field is
  present, and `coords_e7.len() == refs.len()` always. Locations mode has
  no missing-node compaction so the second holds by construction, but a
  silent violation mispins vertices - a corruption of DP retention the
  earcut oracle cannot see - so it must be loud.
- Ways with <= 2 refs may legitimately carry pin bits (altw counts 2-node
  connector endpoints as junction occurrences). Consumption needs no
  special case: DP never moves endpoints, so those pins are inert; the
  mask is built uniformly.
- Flags are authoritative; a pins flag without `LocationsOnWays` is
  already a landed detection hard error.

## Target artifacts

`src/pipeline/phase12.rs` unless noted.

```rust
/// Where DP pins come from for this run. Resolved once at header
/// detection, beside the LocationsOnWays sniff and MemberSource.
#[derive(Clone, Copy)]
enum PinSource {
    /// Header declares pbfhogg.SharedNodePins-v1: read Way field 20,
    /// skip block-local counting entirely.
    Injected,
    /// Block-local counting (today's path, and the raw-input fallback).
    BlockLocal,
}
```

- Detection: `let pin_source = if injected.pins { PinSource::Injected }
  else { PinSource::BlockLocal };`. The detection eprintln becomes
  "SharedNodePins-v1 detected - using injected pins". The "Global
  shared-node prepass disabled" eprintln disappears with its branch.
- `build_way_plans(block: &PrimitiveBlock, members: &MembersForBlock<'_>,
  pins: PinSource) -> (Vec<WayPlan>, u64)` - the `global_shared` parameter
  is GONE. On `PinSource::Injected` the whole `shared_node_counts` +
  `preserve_refs_for_way` pass is skipped (`preserve_node_refs` stays
  empty; the drop reads directly in the `phase12_plan_build_ns` busy
  counter). On `BlockLocal` the counting runs as today, calling the
  narrowed `preserve_refs_for_way(node_refs, counts)`.
- `preserve_refs_for_way(node_refs: &[i64], counts: &FxHashMap<i64, u8>)
  -> Vec<i64>` - the `global_shared: Option<...>` parameter and its union
  arm are deleted. `annotate_block_shared_node_refs` (test wrapper)
  updates to the new signature.
- `process_planned_way_into(way, plan, node_reader, ..., pins: PinSource,
  acc) -> u64`:
  - The redundant `is_member` parameter is dropped; the function reads
    `plan.is_member` (the call site currently passes both). Same-altitude
    cleanup in the same lines this landing already touches.
  - Mask building on `PinSource::Injected`: `way.shared_node_pins()` is
    `None` -> mask stays all-false (legal, common). `Some(bitmap)` ->
    assert the coords-length invariant (`coords_e7.len() ==
    plan.node_refs.len()`, below), then `mask[i] = bitmap bit i` for
    `i in 0..coords_e7.len()` (extracted as a small pure helper,
    `fill_mask_from_pin_bitmap(bitmap, mask)`, so the positional/LSB-first
    semantics get a direct unit test). Bits at positions >= len do not
    exist to read; trailing pad bits in the last byte are never inspected.
    NOTE: the bitmap-length-vs-ref-count check is NOT done here - mask
    building sits after several early returns (`process_planned_way_into`
    bails on tagless/non-member ways, on ways with no possible feature, on
    empty coords, and on no shortbread match, all before the mask is
    built), so a length check placed here would never inspect the field 20
    of a way that is not itself emitted as a feature. Corrupt enrichment
    on a non-emitted way would pass silently. That check is therefore
    hoisted to the decode workers (see error policy below), which see
    every way.
  - Mask building on `BlockLocal`: the existing id-matching branch over
    `plan.preserve_node_refs`, unchanged.
  - Return value: the popcount of the built `preserve_vertex_mask`, on
    BOTH paths (0 when no branch ran). See counters.
- **Error policy for the two pin validations.** The parent contract
  demands validation for EVERY way on the injected path, and the two
  checks split cleanly by what data they need and where every way is
  visible - so they land in two different places, not one:
  - **Bitmap-length vs ref-count (`bitmap.len() ==
    refs.len().div_ceil(8)`) -> decode workers, Result-typed
    `PipelineError`.** This is the check that must cover every way,
    emitted or not, and it needs ONLY the way's field 20 and its ref
    count - both already materialized in the `UnorderedBlockSource` decode
    workers where field-5 membership is validated
    (`validate_and_take_members`). It therefore mirrors field 5 exactly:
    per-way length check on the decoded Way, surfaced as a
    `PipelineError` through the Result-typed decoded channel and the `?`
    that already carries field-5 failures. This is a correction to an
    earlier draft that lumped both checks into a rayon-task `assert!`: the
    original "the pin checks run inside rayon way tasks that have no error
    channel" rationale is FALSE for the length check, which needs no
    coords and no rayon materialization. Putting it in the decode worker
    both closes the non-emitted-way coverage hole and gives it a real
    checked error, matching the field-5 precedent and the contract's
    "release-checked (not `debug_assert!`)" wording.
  - **Coords-length (`coords_e7.len() == refs.len()`) -> mask-build
    `assert!`.** This one genuinely needs the materialized coords, which
    only exist after resolution inside the rayon way task. In locations
    mode `coords_e7` is `way.node_locations()` collected with no
    compaction, so the invariant holds by construction and a violation is
    pure corrupt enrichment. It stays a release-checked `assert!` naming
    the way id and the mismatched lengths: loud, never compiled out, and a
    way-task panic aborts the run through the existing
    `worker_handle.join().expect("worker thread panicked")` chain - the
    established policy for unrecoverable mid-pipeline input failure (the
    relation re-read panics under the same rationale). Because it fires
    only on emitted ways, it is a backstop, not the primary contract
    check; the primary length guarantee is the decode-worker check above.
  - **Panic-message caveat (drove test #2's redesign, below).** A rayon
    way-task `assert!` does NOT surface its own message to a `join`ing
    caller: `worker_handle.join().expect("worker thread panicked")`
    (`phase12.rs`) replaces the inner payload with the outer expect
    string. So the coords-length assert is observable at the caller only
    as `"worker thread panicked"`, never as its corrupt-enrichment text.
    The decode-worker length check, being a `PipelineError` through `?`,
    surfaces its own message cleanly - another reason to prefer it as the
    tested corruption path.
- Worker plumbing: `way_pins_marked: Arc<AtomicU64>` beside
  `way_members_marked`; the task loop `fetch_add`s the return of
  `process_planned_way_into`.
- Counters/stats (`src/pipeline/stats.rs`, `src/pipeline/mod.rs`):
  `Phase12Stats.global_shared_nodes` and its
  `emit_counter_usize("global_shared_nodes", ...)` are deleted;
  `way_pins_marked: u64` is added and emitted as
  `emit_counter_u64("way_pins_marked", ...)`. Semantics: pinned vertex
  positions on emitted ways, both paths - so the counter reads
  block-local pin volume on non-enriched input and injected pin volume on
  enriched input. That makes the strict-superset claim OBSERVABLE (not a
  clean numeric superset) on the same region: denmark raw (block-local) vs
  denmark locations (injected) at the landed commit. Read the direction
  (injected >= block-local), not a ratio: the two counts differ by
  definition beyond the superset relationship. `shared_scan_slice` returns
  empty for `len <= 2`, so block-local pins NOTHING on 2-node connectors
  while altw/injected DOES count their endpoint junction occurrences; and
  the raw path compacts missing refs before masking while locations does
  not. Those are additive definitional biases on top of the true
  cross-block superset, so the gap is not itself the superset measure -
  "direct pin-volume comparison" overstated it.
- `WayBlock` / `MemberSource` / field-5 validation: untouched, EXCEPT the
  decode workers gain the per-way pin bitmap-length check described in the
  error policy above, sitting right beside `validate_and_take_members` and
  returning the same `PipelineError` shape through the decoded channel.
  Gated on `PinSource::Injected` (a raw/block-local run has no field 20 to
  check).

## Teardown (same commit)

Deleted outright:

- `prepass_shared_nodes` (with its `#[hotpath::measure]` attribute),
  `NodeRefChunkReader`, `NodeRefHeapEntry`, `encode_signed_i64_key`,
  `decode_signed_i64_key`, the `shared_node_prepass` scratch-dir handling.
- The prepass spawn block (`prepass_pbf_path` / `prepass_tmp_dir` /
  `prepass_sort_budget` / `prepass_handle`), the `gsn` join at the first
  way block, the after-loop `prepass_handle` join, the `gsn` Arc and
  `gsn_ref` threading into the way tasks, the `global_shared_nodes` local
  and stat assignment.
- `TilegenConfig.global_shared_node_pins` + doc comment
  (`src/pipeline/mod.rs`), the clap flag and its wiring (`src/main.rs`,
  both sites), the doc-example line (`src/lib.rs`), the field in all five
  `TilegenConfig` literals (`src/pipeline_tests.rs`).
- `annotate_global_shared_node_refs` (cfg(test), dead).
- `preserve_refs_for_way`'s `global_shared` parameter and the two
  global-branch tests in `shared_node_helper_tests`
  (`global_shared_pins_a_non_block_local_node`,
  `global_shared_never_pins_the_closing_dup_vertex`). The parent teardown
  said to keep "their tests" - narrowed here: the block-local tests stay,
  the tests OF the deleted branch go with the branch.

Kept: `WAIT.prepass_join` (still spans the relation-plan join on the
fallback member path), `BUSY.phase12_plan_build`, `shared_node_counts` /
`shared_scan_slice` / `preserve_refs_for_way` and the block-local tests
(`annotate_block_shared_node_refs` battery in `pipeline_tests.rs`),
`prepass_relation_plan` (fallback for non-enriched input).

## Tests (land in the same commit)

Instrument first - no existing oracle reaches field-20 consumption
end-to-end, and `BlockBuilder` cannot write the field, so the instrument
is a brick of this spec laid inside the same test file:

- `splice_way_pins` helper in `src/pipeline_tests.rs`: fixtures are
  written with `PbfCompression::None`, so the way blob body is a raw
  `Blob` message wrapping a raw `PrimitiveBlock`. The helper walks frame
  -> BlobHeader (for `datasize`) -> Blob raw-bytes field ->
  `PrimitiveBlock` field 2 (primitivegroup) -> `PrimitiveGroup` field 3
  (the target Way message, selected by ordinal), appends the field-20
  bytes (tag `0xA2 0x01`, varint length, bitmap) at the end of that Way
  message, and re-patches every enclosing varint length plus the frame's
  4-byte header-length prefix and `datasize`. Same style and helper set
  (`read_varint` / `append_varint`) as the landed `splice_way_members`.

Tests:

1. **`injected_pins_end_to_end_pins_vertex_through_dp`** (the behavior no
   other oracle reaches). Fixture header declares `LocationsOnWays` +
   `pbfhogg.WayMembers-v1` + `pbfhogg.SharedNodePins-v1` with a valid
   all-zero field 5 (matching production shape - altw always emits both).
   One shortbread-matching open way (e.g. `highway=motorway`, streets
   layer) with 3 refs whose middle vertex deviates from the endpoint
   chord by less than the DP tolerance at the asserted zoom, geometry
   confined to a single tile at that zoom (test-geometry rule). Pick an
   explicit zoom and explicit single-tile coordinates and record them in
   the test (the implementer fixes them; do not leave them to chance -
   the test-geometry rule makes them load-bearing). Run
   `phase_read_and_process` twice: field 20 spliced with bitmap `[0x02]`
   (middle position pinned) vs no field 20. Assert
   `stats.way_pins_marked == 1` vs `0`, and - by decoding the records the
   run actually emitted - that the emitted line record at a simplified
   zoom carries 3 vertices when pinned and 2 when not. Decode mechanism:
   `one_tile_sort_reader` is NOT reusable here - it builds its own
   synthetic POI point and returns a reader over THAT, it does not open
   the output of `phase_read_and_process`. Instead finish the `SortWriter`
   that `phase_read_and_process` filled (the run returns/owns it) and
   iterate its `SortReader`, then decode each `SortRecord.data` with the
   `wire_format` decoder to read the emitted geometry's vertex count. This
   pins retention behavior, not just counter flow.
2. **`injected_pins_wrong_length_bitmap_fails_run`**: same fixture with a
   2-byte bitmap on the 3-ref way. Because the primary length check is a
   `PipelineError` raised in the decode worker (error policy above), the
   test asserts the run returns `Err` - `assert!(injected_fixture(...).
   is_err())`, the exact shape of the landed member-corruption tests
   (`injected_members_count_mismatch_fails_run`), NOT `#[should_panic]`.
   Do not assert `#[should_panic(expected = <corrupt-enrichment text>)]`:
   the coords-length backstop is a rayon-task `assert!` whose payload is
   swallowed by `worker_handle.join().expect("worker thread panicked")`,
   so a `should_panic` there could only match `"worker thread panicked"`,
   which is not diagnostic. Routing corruption through the decode-worker
   `PipelineError` is exactly what makes this an `is_err()` test with a
   meaningful message.
3. **`fill_mask_from_pin_bitmap` unit tests**: LSB-first positional
   mapping, multi-byte bitmaps, all-zero bitmap.
4. **Mixed-flag arms stay live**: the landed Brick 4 members-only fixture
   (`injected_members_end_to_end_marks_ways_and_skips_prepass`) continues
   to pass unchanged - it exercises `MemberSource::Injected` with
   `PinSource::BlockLocal`, proving the two sources compose
   independently.

## Gates

All plantasjen. Benchmark discipline throughout: commit first, then
measure, then record against the hash. Never two elivagar processes at
once.

`<H0>`, `<H2b>`, and `<uuid>` in the command blocks below are
placeholders: substitute the real commit hashes and the real run UUID
before running. As literal shell text `<H0>` parses as an input
redirection, so the blocks are illustrations of the command shape, not
copy-pasteable lines. The recorded Brick 4 activation readings (germany locations 50.2s
run `da63d783`, denmark locations bench-3 11.7s run `67474f0d`, both at
`20c8bd7`) are context; the verdict baselines are the fresh pre-landing
runs below, because HEAD has moved (data/docs commits only) and the
sidecar byte counters for the growth bound must come from a run whose
UUID is at hand.

### Pre-landing baselines (at the pre-landing HEAD, call it H0)

```
brokkr tilegen --bench 3 --dataset denmark --variant raw
cp target/denmark-<H0>.pmtiles data/denmark-raw-h2b-baseline.pmtiles
brokkr tilegen --dataset denmark --variant locations
cp target/denmark-<H0>.pmtiles data/denmark-locations-h2b-baseline.pmtiles
brokkr tilegen --bench 3 --dataset germany --variant locations
brokkr sidecar <germany-uuid> --counters
```

Record from these: denmark raw best-of-3 wall (raw-path neutrality
anchor); germany locations best-of-3 wall, `output_bytes`, summed
`sort_layer_<name>_bytes`, `way_members_marked` (perf-verdict baseline).
The two `cp`s exist because a raw and a locations run at the same commit
write the same `target/denmark-<H0>.pmtiles` path; each copy is taken
immediately after the build that produced it. `data/` is gitignored; the
user retires the two baseline copies after the verdict.

### Landing gates (one commit, call it H2b; in this order)

```
brokkr check
```
Full suite: new pin tests, block-local pin tests, Shortbread battery.

Raw-path bit-identity (the teardown touches shared code, so this is a
proof, not a formality):
```
brokkr tilegen --bench 3 --dataset denmark --variant raw
brokkr regress --dataset denmark --file target/denmark-<H2b>.pmtiles --against data/denmark-raw-h2b-baseline.pmtiles
```
Zero diffs at tol 0. The bench-3 doubles as the raw-path perf gate: not
slower than the H0 raw anchor beyond the ~5% noise band
(`reference/performance.md` reading rules).

Injected-path validation, denmark locations:
```
brokkr tilegen --bench 3 --dataset denmark --variant locations
brokkr sidecar <uuid> --counters
cd scripts/validate
node validate.mjs ../../target/denmark-<H2b>.pmtiles
node earcut-oracle.mjs ../../target/denmark-<H2b>.pmtiles
cd ../..
```
This run is `--bench 3` (not plain) DELIBERATELY: `way_pins_marked` and
every other pipeline counter are FIFO/sidecar-only and are emitted ONLY
under a measured run (`src/debug.rs` no-ops when `BROKKR_MARKER_FIFO` is
unset - a bare `brokkr tilegen` attaches no sidecar). The injected-path
`way_pins_marked` reading the perf verdict later cites (denmark
locations) comes from this run's `--counters`, so it must be measured
here; a plain run would leave that number unobservable. `--bench` still
writes `target/denmark-<H2b>.pmtiles`, which the validate/oracle/regress/
svg gates below all consume. validate.mjs: structural pass, zero invalid
tiles. earcut oracle: 0 deviant polygons, 0 misattached holes, every
polygon layer (the standing geometry gate).

Semantic regress against the pre-landing locations archive, run as a
DIAGNOSTIC (not a pass/fail gate - see the metric caveat):
```
brokkr regress --dataset denmark --file target/denmark-<H2b>.pmtiles --against data/denmark-locations-h2b-baseline.pmtiles --tol 24 --max-moved 10000000 --json
```
**Metric caveat - regress's built-in `passed()` is NOT the verdict for
this landing, for two independent, code-confirmed reasons:**

1. *The move metric is vertex-to-nearest-vertex, not point-to-line.*
   `regress` classifies a matched-feature move by `component_distance` ->
   `discrete_hausdorff` (`src/regress.rs`), the symmetric discrete
   Hausdorff between the two VERTEX sets: each current vertex to its
   NEAREST blessed vertex. This landing's whole effect is to ADD interior
   junction vertices. A newly pinned midpoint D that lies exactly on the
   old simplified segment [A,E] has zero point-to-line error, yet its
   nearest blessed vertex is A or E, so its Hausdorff contribution is up
   to HALF the segment length - hundreds to thousands of extent units on a
   sparsely simplified rural road at z <= 11, far above any small `--tol`.
   The `--tol 24` derivation (one DP tolerance + headroom) reasons about
   point-to-LINE deviation; it does not match the point-to-nearest-VERTEX
   metric the tool computes, and is void as a threshold. Under the metric,
   legitimate pin retention lands in `structural_moved` (`distance > tol`
   at `src/regress.rs`, since there is no third bucket), and `passed()`
   hard-requires `structural_moved == 0` (not configurable; `--max-moved`
   bounds only `tolerance_moved`). So the built-in pass rule produces a
   false "bug -> revert" on exactly the change it is meant to validate.

2. *Pin retention is not vertex-monotonic.* Pin-aware DP
   (`dp_recurse_with_required`, `src/geometry/simplify.rs`; likewise
   `dp_keep_into` in `int_ocean.rs`) pre-marks pinned vertices and splits
   the recursion at them, then runs ordinary DP on the shorter
   sub-segments. A vertex that plain DP KEPT (far from the long chord) can
   fall within tolerance of a shorter sub-chord and be DROPPED. So the pin
   SET is a strict superset but the KEPT-VERTEX set is not: individual
   features can lose a vertex, tile coverage can shift, and in edge cases a
   feature can appear or disappear. `passed()` also hard-fails on
   `added_features != 0` and `missing_features != 0` - both legitimately
   nonzero here. The "pins only ADD retained vertices, nothing may vanish"
   framing is false per-feature and cannot be a gate.

**What regress still validates soundly (the part that IS a hard gate),
read from the `--json` per-feature records, not the aggregate bool:**
- TRUE structural changes on matched features - component-count,
  ring-role, and hole-containment mismatches - must be ZERO. These are the
  `classify_components -> None` path, recorded as `structural_moved` with
  `distance == 0`; distinguish them from added-vertex artifacts (also
  bucketed `structural_moved`, but with `distance > 0`). A ring gaining or
  losing a hole, an outer flipping to hole, a polygon splitting into two
  components - none of those is a DP-retention effect and each is a bug.
- `attr_changed`, `extent_mismatch`, `layers_added`, `layers_removed` must
  be zero (unrelated to pins; a nonzero here is a real regression).
- `structural_moved` records with `distance > 0`, `tolerance_moved`,
  `added_features`, and `missing_features` are EXPECTED, unbounded, and
  human-triaged, NOT auto-fail: they are the added-vertex / non-monotonic
  footprint. Spot-check a sample - each added feature should sit at
  z <= 13 in an OSM layer; each dropped/moved feature should trace to a
  short sub-chord swallowing an unpinned vertex. Unexplainable ones =
  bug = revert.
- Displacement percentiles and the added/removed/moved counts go into
  `reference/performance.md` as the landing's geometric footprint.

Vertex-direction reading (coarse diagnostic, not a hard gate):
```
brokkr compare-tiles data/denmark-locations-h2b-baseline.pmtiles target/denmark-<H2b>.pmtiles
```
Expect AGGREGATE per-layer vertex counts to rise, concentrated in
streets/water/boundaries at z <= 11. Read only that direction. Do NOT
gate on "any vertex decrease is a bug": per the non-monotonicity above, a
pin split can drop a previously-kept unpinned vertex, so individual
layers/tiles can show a decrease legitimately. Two further limits keep
this a diagnostic and not a verdict: `compare-tiles` samples only 200
common tiles per zoom by default (`examples/compare_tiles.rs`; raise with
`--sample`), and it counts geometry-command varints, not decoded
vertices - so it cannot establish any global vertex-count claim. A whole
LAYER vanishing is still worth investigating, but chase it through the
regress structural records, not this count.

Human gate (the one that needs eyes) - junction integrity at simplified
zooms, before/after pairs from the same two archives:
```
brokkr svg --file data/denmark-locations-h2b-baseline.pmtiles -z 10 -x 547 -y 323 -W 2 -H 2 -l streets -o notes/qa-cph-streets-before.svg
brokkr svg --file target/denmark-<H2b>.pmtiles -z 10 -x 547 -y 323 -W 2 -H 2 -l streets -o notes/qa-cph-streets-after.svg
brokkr svg --file data/denmark-locations-h2b-baseline.pmtiles -z 10 -x 544 -y 316 -l streets,water_polygons -o notes/qa-tisso-before.svg
brokkr svg --file target/denmark-<H2b>.pmtiles -z 10 -x 544 -y 316 -l streets,water_polygons -o notes/qa-tisso-after.svg
```
(z10/544/316 = Tissoe, the known inland water tile - these exact
coordinates are the ones the project's own visual-QA record uses for
Tissoe, so they are kept. z10/547/323 = Copenhagen road network - this
one is UNVERIFIED: a Web-Mercator check in review puts central Copenhagen
nearer z10/547/321, two rows off the named tile, and the 2x2 grid rooted
at y=323 walks further from it, not toward it.) Before running the human
gate, confirm each tile actually contains the named feature - render a
quick `brokkr svg` preview and adjust x/y if the tile is empty ocean;
inspecting an empty tile proves nothing. Correct looks like: connected
road junctions with no new gaps or spikes versus the before renders;
water outlines unchanged in character, no missing rings.

Performance verdict, germany locations (two opposing terms: plan-build
counting removed = win; more retained vertices through simplify/encode =
cost):
```
brokkr tilegen --bench 3 --dataset germany --variant locations
brokkr sidecar <uuid> --counters
brokkr sidecar <uuid> --stalls --human
```
Keep bounds, read against the H0 germany baseline:
- best-of-3 wall regression <= 2%;
- `output_bytes` + summed `sort_layer_<name>_bytes` growth <= 3%.
Outside either bound -> revert the commit and record the finding in the
roadmap (the pins would then need a zoom cap, which is a new spec).
Readings for the record (not pass/fail): `phase12_plan_build_ns` drop;
`way_pins_marked` (injected, germany); `way_pins_marked` from the denmark
raw H2b bench (block-local) vs the denmark locations H2b bench (injected,
the `--bench 3` run above) - a same-region direction check (injected >=
block-local), NOT a clean numeric superset, since the 2-node-connector
and missing-ref biases noted in the counters section shift the totals
independently of the cross-block superset; `prepass_join` still absent
from the stalls top (unchanged from Brick 4, membership is injected).

### Evidence limitation, stated honestly

The human gate is three hand-picked tiles, and the automated gates do not
directly test the property pins exist for: the earcut oracle validates
tessellation fidelity (not junction-gap correctness), and the
regress/compare-tiles gates only bound the STRUCTURAL footprint
(component/ring/hole integrity, no attr or extent drift) and read the
aggregate vertex direction - they cannot confirm the RIGHT cross-block
junction vertices were retained, and (per the metric caveat) they cannot
even bound per-feature vertex moves, since the regress metric is
vertex-to-nearest-vertex Hausdorff. The planet-scale "exact pins fix
cross-block gaps" claim rests
on: the spot-checks, the algorithmic strict-superset argument, and the
new same-region `way_pins_marked` block-local-vs-injected reading. If the
eyeball gate is inconclusive, the escalation is a targeted diff of
pin-mask coverage at known PBF primitive-block seams (where block-local
pinning provably drops junctions and injected pinning must retain them) -
an escalation, not a standing requirement.

## Post-landing follow-through

- Record the hash-anchored numbers in `reference/performance.md`: both
  bench verdicts against their bounds, displacement percentiles,
  `way_pins_marked` readings, `phase12_plan_build_ns` delta, and the
  keep/revert verdict.
- **Bless rotation (user say-so, named here because this landing forces
  it):** the blessed denmark reference (`blessed/denmark-c9362c4.pmtiles`,
  locations-variant) legitimately diffs against H2b locations output, so
  the standing denmark regress gate is broken until the user rotates it:
  `brokkr bless --dataset denmark --commit <H2b>` (the last denmark
  archive built at H2b must be the locations run - the gate order above
  leaves it so). Until rotation, a regress against blessed showing
  pin-retention diffs is expected, not a regression - do not re-run the
  2026-07-09 false alarm.
- The banked norway baseline (`norway-20c8bd7.pmtiles`, run `525c553a`)
  now predates H2b geometry: a future norway locations regress against it
  must expect pin-retention diffs. Written here so the record carries the
  warning.
- Update the status header of `notes/injected-prepass-spec.md`: Brick 5
  DONE (or reverted, with the finding).

## Ordering and keep/revert

One coherent, fully intrusive commit: consumption + teardown + tests. No
env-vars, no experiment switches, no dormant halves - the enriched inputs
are registered, so the code activates on landing. `brokkr check` and the
validate/oracle gates are green at the single boundary. Revert is a git
revert of the one commit: both fallback paths are restored, and the
enriched files remain valid (field 20 returns to being skipped, exactly
today's behavior).

## Stopping rule

- The relation tail, ocean, sort, and assemble phases are untouched.
  Specifically: multipolygon member-ring emission does not consume pins
  today and does not gain them here; if junction artifacts on relation
  geometry ever justify pin consumption in the relation tail, that is a
  new spec.
- `prepass_relation_plan` and the block-local pin path remain first-class
  fallbacks: raw Geofabrik input stays supported. Requiring enriched
  input (deleting the fallbacks) is the H10 record-framing product
  decision, not this spec.
- pbfhogg is untouched (Brick 2 landed everything this consumer needs).
- H2c (shortbread relevance masks) and H2d (partition calibration stats)
  remain separate roadmap items.
- The bless rotation is named follow-through executed on user say-so; it
  is not part of the landing commit.

## Review reconciliation (2026-07-11)

Two independent reviews (an Opus pass, `notes/injected-pins-spec-R1.md`;
a codex gpt-5.6-sol xhigh pass, `notes/injected-pins-spec-R2.md`) were
validated against HEAD code and folded above. The substantive changes
they drove:

- **Regress metric mismatch (R1 headline, R2 #3).** `regress` measures
  vertex-to-nearest-vertex discrete Hausdorff, not point-to-line DP
  error; `--tol 24` was derived against the wrong metric and its
  built-in `passed()` would false-reject legitimate pin retention. The
  semantic-regress gate is reframed as a diagnostic whose only hard part
  is TRUE structural integrity (component/ring/hole, attr, extent), read
  from the JSON records.
- **Non-monotonic retention (R1 #2, R2 #2).** Pin-aware DP splits at
  pinned indices and re-runs ordinary DP on the sub-segments, so the
  kept-vertex set is NOT a superset even though the pin set is. Removed
  the "nothing may vanish" / "any vertex decrease is a bug" gate rules;
  `added_features`/`missing_features` are expected, not auto-fail.
- **Field-20 validation coverage (R2 #1).** `process_planned_way_into`
  early-returns on tagless/non-matching/empty-coord ways before the mask
  is built, so a mask-time length check misses non-emitted ways. Split
  the two validations: bitmap-length-vs-ref-count moves to the decode
  workers as a `PipelineError` (covers every way, mirrors field 5);
  coords-length stays a mask-build `assert!` backstop.
- **Panic test observability (R1 #3, R2 #4).** `join().expect("worker
  thread panicked")` swallows a rayon-task assert payload, so
  `#[should_panic(expected = <inner text>)]` can never match. Test #2 is
  now an `is_err()` assertion on the decode-worker `PipelineError`.
- **Counter observability (R2 #5a).** The denmark locations validation
  run is now `--bench 3` so its `way_pins_marked` counter (FIFO/sidecar
  only) is actually emitted and readable.
- **compare-tiles limits (R2 #5b).** Noted the 200-tiles-per-zoom default
  sampling and that it counts geometry varints, not decoded vertices;
  downgraded to a coarse diagnostic.
- **way_pins_marked comparison (R1 #4).** Softened "direct pin-volume
  comparison" to a direction check, noting the 2-node-connector and
  missing-ref definitional biases.
- **Test decode mechanism (R2 #7).** Corrected the claim that
  `one_tile_sort_reader` can read pipeline output - it builds its own
  synthetic point; the test must finish and decode the run's own
  `SortWriter`. Added a placeholder-substitution note for `<H0>` /
  `<H2b>` / `<uuid>`.
- **Accessor lifetime (R1 nit).** `Option<&'a [u8]>`.

Rejected / not folded:

- **Human-inspection tile coordinates (R2 #6), partially rejected.** R2
  called both QA tiles geographically wrong and offered replacements
  (Copenhagen ~547/320, Tissoe ~544/321). Tissoe's spec coordinate
  z10/544/316 is KEPT: it is exactly the tile the project's own durable
  visual-QA record (`memory MEMORY.md`, 2026-03-07: "z10/544/316 (Tissoe,
  inland Denmark) has ocean Ring 0 ... 100% tile coverage") uses for
  Tissoe, and a review's fresh projection estimate does not override the
  project's own recorded QA tile. R2's Copenhagen concern is folded as an
  unverified-coordinate warning plus a "preview before you trust the
  tile" step, not as a hard coordinate replacement, since the reviewers'
  own numbers disagree (R1 did not flag this; R2's estimate is
  approximate) and the fix is a cheap pre-render check regardless.
