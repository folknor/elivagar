# Spec: injected way-members consumption plumbing (behavior-neutral Brick 4 subset)

Status: specified 2026-07-11, not implemented.

Written against `reference/technical-implementation-spec.md` (the contract
for this document). Spawned from `notes/injected-prepass-wait-work.md`,
Item B. The normative cross-repo contract this plumbing consumes is
`notes/injected-prepass-spec.md` (Brick 4, "Target artifacts", "The
contract", and the 2026-07-11 "Cross-repo ratifications"); where this
document and that one describe the same artifact, that one is normative
and this one narrows scope. Measurement record:
`reference/performance.md` + `.brokkr/results.db`.

## The problem

Brick 4 of the injected-prepass design (consume the field-5 way-member
bitmap, delete the relation-plan prepass stall) cannot land whole today:
its gates require enriched datasets that do not exist until pbfhogg's
producer landings (their 2-5) and the Brick 3 re-enrichment are done. But
most of Brick 4's code is inert on every input that exists today - no
current file declares `pbfhogg.WayMembers-v1` or
`pbfhogg.SharedNodePins-v1`, so the injected path is dormant by
construction. Item B lands exactly that inert subset now: header
detection, `MemberSource`, `WayBlock` plumbing, the `MembersForBlock`
plan-build split, the release-checked validations, and unit tests that
need no enriched file. The eventual Brick 4 activation then reduces to
data (Brick 3) plus gate runs; the H2a performance verdict and the
superset cost reading stay with Brick 4 proper.

This landing claims NO performance win. Its deliverable is a smaller,
already-validated activation diff, proven output-neutral on today's
inputs.

## Survey of the ground

### elivagar (`src/pipeline/phase12.rs`, HEAD `430f28b`)

- `430f28b` ("phase12: resolve way membership once at plan build") already
  landed one Brick 4 target artifact: `WayPlan` carries
  `pub(super) is_member: bool`, filled inside `build_way_plans` from
  `needed_ways.contains(&way_id)`; `process_planned_way_into` receives
  `plan.is_member`. The task-site lookup the parent spec deletes is
  already gone. This spec builds on that, it does not re-lay it.
- Detection: `phase_read_and_process` computes `locations_on_ways =
  config.locations_on_ways || reader.header().optional_features()...`
  where `reader.header()` returns `&HeaderBlock` (pbfhogg). Node-store
  mode selection follows via the pure, unit-tested
  `select_node_store_mode(...)` - the testability pattern this spec
  copies for flag validation.
- `prepass_relation_plan` is spawned UNCONDITIONALLY at phase12 start
  (`relation_plan_handle = Some(std::thread::spawn(...))`). It is joined
  at the first way block under the `WAIT.prepass_join` span (together
  with the optional shared-node prepass), and joined again after the read
  loop if no way block ever arrived (error surfacing). The join fills
  `relation_plan_needed_ways` / `relation_plan_superset_ways`, which flow
  through `Phase12Stats` (`src/pipeline/stats.rs`) to
  `emit_counter_usize` in `src/pipeline/mod.rs`.
- Block routing: the `route_block!` macro classifies via
  `block.block_type()` (pbfhogg `BlockType`, first-wire-tag-byte
  classification, no element decode) and sends way blocks through
  `block_tx: SyncSender<PrimitiveBlock>` (capacity 1) to the way worker.
  `BlockType::Empty | BlockType::Mixed` blocks are dropped.
- Way worker: spawned at the first way block; captures
  `relation_plan_clone: Arc<RelationPlan>`; each per-block rayon task
  calls `build_way_plans(&block, gsn_ref, &rp_ref.needed_ways)` and zips
  plans positionally against `block.elements()` way elements (documented
  invariant: plan index i = way element i; dropping a plan misaligns the
  zip).
- `build_way_plans(block: &PrimitiveBlock, global_shared: &FxHashSet<i64>,
  needed_ways: &FxHashSet<i64>) -> Vec<WayPlan>` - private fn. Its only
  callers are the task loop and (via the shared helpers, not the fn
  itself) the `#[cfg(test)]` wrapper `annotate_block_shared_node_refs`.
- Locations read path: `UnorderedBlockSource::spawn(path, decode_threads)`
  owns a `pbfhogg::BlobReader` on a reader thread (forwards `OsmData`
  blobs only, raw channel), N decode workers
  (`blob.to_primitiveblock()`), and a decoded channel of
  `Result<PrimitiveBlock, PipelineError>`; the consumer loop does
  `route_block!(block_result?)`, so a worker-sent `Err` already fails the
  run - the error channel a corrupt-enrichment hard error needs exists.
  The raw path uses pbfhogg's ordered `into_blocks_pipelined`, which
  yields `PrimitiveBlock` with NO `Blob` access - per-blob metadata is
  structurally unreachable there (and the injection is locations-only by
  contract anyway).
- Ways are counted task-side from `plans.len()` into the `way_counter`
  atomic; the ordered consumer never re-parses way blocks (5.7s of serial
  way counting was removed for this reason - do not reintroduce counting
  in the CONSUMER; the decode workers are parallel and are a different
  matter).
- Test fixtures: `src/pipeline_tests.rs` already writes synthetic PBFs
  with `pbfhogg::writer::PbfWriter`, and `tests/skip_to_assemble_cli.rs`
  builds way blocks with `pbfhogg::block_builder::BlockBuilder`. The
  in-file `mod shared_node_helper_tests` in phase12.rs is the precedent
  for unit tests beside the helpers.
- Gate archives exist on plantasjen:
  `data/tilegen/denmark-4ceacd1.pmtiles` and
  `data/tilegen/germany-4ceacd1.pmtiles` (locations-variant outputs built
  at commit `4ceacd1`), pinned by the wait-work note as the
  locations-neutrality references.

### pbfhogg (path dependency `../pbfhogg`; surveyed via the in-sync
### `research/pbfhogg/` mirror)

The producer-side API the parent spec's Brick 2 called for is already
public - this landing consumes it, it changes nothing in pbfhogg:

- `HeaderBlock::WAY_MEMBERS_V1` / `SHARED_NODE_PINS_V1` consts plus
  `has_way_members_v1()` / `has_shared_node_pins_v1()` /
  `has_locations_on_ways()` (`src/read/block.rs`).
- `Blob::way_members(&self) -> Option<&[u8]>` - field-5 bitmap bytes with
  the version byte and varint way-count preamble validated and stripped.
  Returns `None` when the field is absent, parsing is disabled, OR the
  payload is malformed (bad version, truncated varint, bitmap length not
  matching the ENCODED count). Consequence: elivagar cannot distinguish
  absent from malformed - and does not need to, because under
  `injected_members` both are the same corrupt-enrichment hard error.
- `Blob::way_member_count(&self) -> Option<u32>` - the encoded preamble
  count, `Some` exactly when `way_members()` is `Some`. This exists so
  elivagar can run the one check pbfhogg structurally cannot: encoded
  count vs the ACTUAL decoded way element count (the ratified Brick 4
  count compare; without it a producer bug that miscounts within one
  bitmap byte is invisible).
- `BlobReader::set_parse_waymembers(&mut self, enable: bool)` - public,
  default `false`; field 5 is skipped (zero cost) unless enabled.
- `Way::shared_node_pins()` exists but is Brick 5's ground - untouched
  here.

### Stale text in the parent spec, resolved

The parent spec's Target-artifacts detection bullet still says a flagged
file on the raw path "ignores the injection and logs it". That predates
its own review fold: the contract's "Flag/mode combinations" section and
the wait-work Item B both make a `WayMembers-v1` / `SharedNodePins-v1`
declaration WITHOUT `LocationsOnWays` a HARD ERROR at detection
(malformed enrichment - altw never produces it). The hard error is what
this spec implements. The check is against the header feature string,
not the effective mode: a user-forced `--locations-on-ways` on a file
whose header lacks the feature does not legitimize a flagged file.

### Failure history

`notes/rendering-postmortem.md` (the surviving geometry failure ledger)
contains nothing about membership planning or header metadata; no logged
failure is re-proposed. The one relevant burn is process-level: the
2026-07-09 cross-variant regress false alarm, which is why every gate
below pins `--dataset` and `--variant` explicitly.

### Baselines (plantasjen, `.brokkr/results.db`)

- denmark raw bench: run `262d55f3`, commit `430f28b`, 16.7s best-of-3.
  This is the raw-path verdict baseline; it is current HEAD, so no fresh
  raw baseline is needed. VERIFY BEFORE LANDING (review R1-1): the reuse of
  `262d55f3` is valid only if the branch tip is actually `430f28b`. That
  commit's ancestry is `0f67f51` -> `4ceacd1`; it is NOT a descendant of
  the i_overlay Landing-2 line (`aca9460` et al.). Confirm `git rev-parse
  HEAD` == `430f28b` at pre-landing; if the landing sits on the i_overlay
  line instead, the raw baseline shortcut breaks and a fresh raw baseline
  must be captured in step 0 alongside the locations one. (At spec time
  HEAD was confirmed `430f28b`.)
- denmark locations bench: newest is `fc55fca5` at `4ceacd1` (11.8s),
  which predates `430f28b` (a way-path change). A fresh pre-landing
  locations baseline is therefore captured as step 0 below.
- germany locations: `f2b93719` at `4ceacd1` (51.8s) - context only; no
  germany bench gates this landing (germany appears only in the
  compare-tiles neutrality gate, where wall time is irrelevant).

## Target artifacts

All in `src/pipeline/phase12.rs` unless noted. Types match the parent
spec's Target-artifacts section; deviations are named.

```rust
/// A way block plus its injected per-blob metadata, as routed to the way
/// worker. `members` is Some only on the injected locations path.
struct WayBlock {
    block: PrimitiveBlock,
    members: Option<Box<[u8]>>, // field-5 bitmap bytes, validated upstream
}

/// Where `is_member` comes from for this run. The CHOICE is made at header
/// detection (phase12 start, beside the LocationsOnWays sniff); the Plan
/// arm's value is filled at the first-way-block prepass join exactly as
/// today. On the Injected path no prepass is spawned and no join occurs.
enum MemberSource {
    /// Header declares pbfhogg.WayMembers-v1: read per-blob bitmaps.
    Injected,
    /// Runtime relation plan (today's path), joined from the prepass.
    Plan(std::sync::Arc<RelationPlan>),
}

/// Per-block projection of MemberSource, consumed by build_way_plans.
/// `pub(super)` (not private): the Bitmap/Set unit tests live in the sibling
/// `pipeline::tests` module (`pipeline_tests.rs`), which must be able to name
/// and construct these variants to drive `build_way_plans`. A private phase12
/// enum would not compile from there (review R2-4).
pub(super) enum MembersForBlock<'a> {
    /// Positional field-5 bitmap: bit i = way element i (LSB-first).
    Bitmap(&'a [u8]),
    /// The relation plan's member-way id set (today's path).
    Set(&'a FxHashSet<i64>),
}

/// Injected-enrichment feature flags read from the PBF header.
struct InjectedFeatures {
    members: bool, // pbfhogg.WayMembers-v1
    pins: bool,    // pbfhogg.SharedNodePins-v1 (detected + validated only;
                   // consumption is Brick 5)
}

/// Pure flag validation, unit-testable without a HeaderBlock (the
/// select_node_store_mode pattern). Hard error when (members || pins) &&
/// !has_locations: malformed enrichment per the contract's flag/mode
/// rules.
fn detect_injected_features(
    has_members: bool,
    has_pins: bool,
    has_locations: bool,
) -> Result<InjectedFeatures, PipelineError>

/// The three release-checked field-5 validations, pure over bytes and
/// counts so unit tests drive them with hand-built bitmaps. `members` is
/// `blob.way_members().zip(blob.way_member_count())`; `None` under
/// injected_members = corrupt enrichment (absence AND malformed payload
/// both surface as None from pbfhogg - same error class). Checks, in
/// order: presence; encoded count == actual decoded way element count
/// (the ratified count compare); bitmap.len() == actual.div_ceil(8)
/// (implied by pbfhogg's internal encoded-count/length check plus the
/// count compare, but the contract names it - keep it explicit, it is
/// one comparison). Ok returns the bitmap copied to a Box (the Blob does
/// not outlive the decode worker iteration).
///
/// Cast form (review R1-3): the encoded count is `u32`, `actual_way_count`
/// is `usize`. Compare through `u64` widening or `usize::try_from(count)?`
/// - no bare `as`, no `.unwrap()` - to satisfy the strict cast lints and
/// the `.unwrap()` ban (CLAUDE.md).
fn validate_and_take_members(
    members: Option<(&[u8], u32)>,
    actual_way_count: usize,
) -> Result<Box<[u8]>, PipelineError>

/// LSB-first positional bit read, bit i = way element i. Reads via
/// `bitmap.get(i / 8)` (or a bounds `expect` with context), not a bare
/// `bitmap[i / 8]` (review R1-5): bounds are guaranteed by the upstream
/// length validation, so a violated invariant "cannot happen" - but if it
/// ever does, a named panic beats a raw out-of-bounds.
fn member_bit(bitmap: &[u8], i: usize) -> bool

/// New signature. DEVIATION from the parent Brick 4 text, by design:
/// `global_shared` stays (its deletion is Brick 5's teardown, a named
/// exclusion) and there is no `pins: &PinSource` parameter (PinSource is
/// Brick 5, whole). Brick 5 turns this signature into the parent's final
/// form.
pub(super) fn build_way_plans(
    block: &PrimitiveBlock,
    global_shared: &FxHashSet<i64>,
    members: MembersForBlock<'_>,
) -> Vec<WayPlan>
```

`build_way_plans` becomes `pub(super)` so `pipeline_tests.rs` can drive
the Bitmap arm directly (same visibility as `WayPlan`). `MembersForBlock`
is `pub(super)` for the same reason (review R2-4): the test constructs the
argument, so the enum must be nameable from `pipeline::tests`.

### Data flow

- **Detection** (`phase_read_and_process`, right after the
  `locations_on_ways` computation): `let injected =
  detect_injected_features(reader.header().has_way_members_v1(),
  reader.header().has_shared_node_pins_v1(),
  reader.header().has_locations_on_ways())?;`. Log one line per detected
  flag: way-members detected (injected membership), and for pins
  "declared - not yet consumed, block-local pinning" (honest about the
  Brick 4/5 ordering: an enriched file arriving before Brick 5 lands
  runs injected membership with block-local pins, which is exactly the
  parent spec's brick order).
- **Conditional prepass**: `relation_plan_handle` is spawned only when
  `!injected.members`. The post-loop safety join already handles `None`;
  the only semantic change is that `None` can now mean "never spawned".
- **Resolution at first way block** (where the worker is spawned): on the
  Plan arm, join under `WAIT.prepass_join` exactly as today and set
  `relation_plan_needed_ways` / `relation_plan_superset_ways` from the
  plan; on the Injected arm, skip the relation-plan join entirely (the
  counters stay 0, matching the parent's "reads 0 on the injected path").
  The shared-node prepass join (`gsn`) is untouched. `member_source` is
  moved into the worker thread closure, replacing `relation_plan_clone`.
- **UnorderedBlockSource**: `spawn(path, decode_threads, injected_members:
  bool)`. The reader sets `set_parse_waymembers(injected_members)` at
  construction (the opt-in keyed to the feature flag, per the contract).
  Decode workers yield `Result<(PrimitiveBlock, Option<Box<[u8]>>),
  PipelineError>`: after `to_primitiveblock()`, when `injected_members &&
  block.block_type() == BlockType::Ways`, count the block's `Element::Way`
  elements (review R1-2: the SAME filter `build_way_plans` applies via its
  `filter_map(Element::Way)`, so this count equals the eventual
  `plans.len()`; NOT `block.elements().count()`, which would include any
  non-way elements and misalign the bitmap length check against the plans
  zip) and run `validate_and_take_members`, sending any error through the
  decoded channel (the consumer's `?` fails the run - the parent-pinned
  enforcement point: a corrupt enrichment dies in the decode worker,
  before the block reaches the way pipeline). All other blocks carry
  `None`. Field 5 on a non-way OSMData blob is out of contract but not
  one of the contract's three failure classes: it is ignored (never
  extracted), not an error. The way-count scan runs ONLY under
  `injected_members` (short-circuit first), in the parallel decode
  workers where blob decompression already dominates - today's inputs
  pay zero.
- **Routing**: `route_block!` takes `(block, members)`; the raw ordered
  path invokes it as `route_block!(block, None)`. The Ways arm constructs
  `WayBlock { block, members }` and `block_tx` becomes
  `SyncSender<WayBlock>`. Node and relation arms ignore `members`
  (always `None` for them on both paths).
- **Way worker task**: per block,
  `let members = match ms_ref { MemberSource::Injected =>
  MembersForBlock::Bitmap(way_block.members.as_deref().expect(
  "way block without members bitmap on the injected path")),
  MemberSource::Plan(plan) => MembersForBlock::Set(&plan.needed_ways) };`
  The `expect` is an invariant (the decode worker validated and the
  contract guarantees presence on way blobs), not a validation - the
  parent spec's "the way worker then trusts `WayBlock.members` is Some".
- **`build_way_plans`**: the Set arm is today's `needed_ways.contains`
  fill verbatim. The Bitmap arm fills `is_member:
  member_bit(bitmap, way_pos)` where `way_pos` is the running way-element
  index - the same positional index the plans vec is built on, i.e. the
  invariant the task zip already relies on. Bounds are guaranteed by the
  upstream length validation (bitmap sized to the actual way count).
  Block-local pin counting (`shared_node_counts` /
  `preserve_refs_for_way` with `Some(global_shared)`) runs unchanged on
  BOTH arms - pin skipping is Brick 5.
- **Counter**: new `way_members_marked` (u64). A separate
  `plans.iter().filter(|p| p.is_member).count()` per task is a second
  always-on O(plans) pass over the same vec on the live path (review R1-4 /
  R2-5). It is epsilon (the vec is hot in cache), but the cost survey below
  must not silently omit it. Prefer folding the tally into
  `build_way_plans` - return the marked count alongside the plans while
  `is_member` is already being assigned, so no extra traversal is added -
  or, if kept as a separate pass, name it in the cost bound rather than
  pretending the touched cost is only the option wrapper and channel
  struct. The chosen total lands in `Phase12Stats` and is emitted in
  `src/pipeline/mod.rs` beside the `relation_plan_*` counters. It is
  arm-independent (it counts `is_member` fills): on the injected path it
  reads consumed set bits (the parent's ledger-visibility requirement for
  the way_index feed); on the fallback path it reads the member ways
  actually present in the file - a new, always-emitted sidecar counter,
  which does not touch tiles or the results row. Pulled forward from the
  parent Brick 4 artifact list so the activation diff carries no code at
  all; `way_pins_marked` stays with Brick 5.

## The landing

One coherent commit - all of the above plus the tests, kept or reverted
on the gates. No env vars, no experiment switches; both arms are live
code (the Bitmap arm live-but-unreachable until an enriched file exists,
which is the point).

Unit tests (all runnable today, no enriched file):

- `detect_injected_features_requires_locations` (phase12.rs, beside the
  `shared_node_helper_tests` module): members-only, pins-only, and
  both-flags without locations are `Err`; both-with-locations yields
  `{members: true, pins: true}`; no flags yields all-false regardless of
  locations.
- `way_members_validation_rejects_missing_and_mismatched` (phase12.rs):
  hand-built inputs into `validate_and_take_members` - `None` errors
  (corrupt enrichment); `Some(([0x00], 9))` against actual 9 errors
  (bitmap 1 byte, needs 2); `Some(([0x00, 0x00], 9))` against actual 10
  errors (encoded/actual count compare - the within-one-byte producer bug
  the accessor exists to catch); `Some(([0x05], 3))` against actual 3
  returns the boxed bitmap.
- `member_bit_is_lsb_first` (phase12.rs): `0b0000_0101` sets positions 0
  and 2 only; a two-byte bitmap sets position 8 via byte 1 bit 0.
- `build_way_plans_bitmap_arm_marks_positionally`
  (`src/pipeline_tests.rs`, using the existing
  `BlockBuilder`/`PbfWriter` fixture pattern: write a 3-way block to a
  temp PBF under the test tmp dir, read it back to a `PrimitiveBlock`):
  `MembersForBlock::Bitmap(&[0b0000_0101])` marks ways 0 and 2, not 1;
  a 9-way block with `&[0x00, 0x01]` marks only way 8 (multi-byte
  positional check).
- `bitmap_and_set_arms_agree` (`src/pipeline_tests.rs`, same fixture):
  for the same block, a bitmap marking way 1 and a
  `MembersForBlock::Set` containing way 1's id produce identical
  `is_member` vectors, and `preserve_node_refs` is identical across arms
  (pins are arm-independent in this landing).

### Integration coverage of the injected path (review R2-3)

The five unit tests above pin the LEAF pieces in isolation
(`detect_injected_features`, `validate_and_take_members`, `member_bit`,
`build_way_plans` called directly). They do NOT exercise the PLUMBING
that is this landing's actual deliverable: `set_parse_waymembers(true)` on
the reader, field-5 extraction before the `Blob` is dropped, the
decode-worker `validate_and_take_members` call, transport of
`Option<Box<[u8]>>` through the decoded channel and `WayBlock`, the
conditional relation-prepass suppression when `injected.members`, routing
the bitmap into the way worker, and a malformed/missing field 5 becoming a
`PipelineError` that the consumer's `?` turns into a failed run. No
dataset that exists today reaches any of it (dormant by construction), so
nothing gates it - and the specification contract
(`reference/technical-implementation-spec.md`) requires that when no
oracle reaches a behavior, the verification instrument is itself a brick,
laid before the behavior it gates.

The honest instrument is a tiny synthetic ENRICHED PBF fixture with a
header declaring `WayMembers-v1` + `LocationsOnWays` and hand-built
field-5 bitmaps for four cases: valid, missing (field absent on a way
blob), malformed (bad version / truncated), and count-mismatched
(encoded count != decoded way count). Driving the pipeline over it
covers the whole injected path end-to-end.

DECIDED (adjudicated 2026-07-11): the integration coverage lands NOW,
inside this landing, with NO pbfhogg change. The obstacle the fork was
framed around is narrower than it looked: everything the fixture needs
except field 5 itself is ALREADY public in the path dependency -
`HeaderBuilder::optional_feature` (declare `HeaderBlock::WAY_MEMBERS_V1`
and `HeaderBlock::LOCATIONS_ON_WAYS`),
`BlockBuilder::add_way_with_locations`, `PbfWriter::new` - and elivagar's
test suite already uses all three. The one missing piece, BlobHeader
field 5, is added by a TEST-LOCAL byte splice: protobuf message fields
are order-independent, so appending an encoded field 5 (tag byte `0x2A`,
varint length, payload) to a written frame's BlobHeader bytes and
patching the frame's 4-byte big-endian header length yields a valid
enriched PBF. No wire-format writer is duplicated. The splice helper
carries exactly two facts: the frame walk (u32-BE header length, then
minimal BlobHeader field parsing - field 1 type, field 3 datasize - to
advance frame by frame) and the field-5 payload layout (version byte
`0x01`, varint way count, bitmap of `count.div_ceil(8)` bytes). Both are
pinned by the ratified WayMembers-v1 contract (the 2026-07-11 cross-repo
ratifications) and by pbfhogg's landed public reader
(`Blob::way_members`); if the wire format ever drifts, the valid-case
test fails loudly, which is the correct alarm - the plumbing consumes
exactly that format.

Why not (a), the public pbfhogg test-writer: it touches pbfhogg while
the altw producer landings are being implemented there (mid-flight,
build intermittently broken) and breaks this landing's stopping rule -
and it would not even remove the hand-framing: three of the four cases
are payloads no honest producer API can emit (missing, malformed,
count-mismatched), so a pbfhogg test entry point would have to accept
arbitrary field-5 bytes, i.e. it would BE this splice, just in the wrong
repo. Why not (b), deferral: the plumbing IS this landing's deliverable,
and the specification contract requires the instrument as a brick laid
before the behavior it gates; deferring ships the deliverable
unexercised for an open-ended window. Nor is the splice a stopgap: real
enriched data (Bricks 3/4) can never exercise the corrupt cases, so the
adversarial fixtures remain the durable home for those paths. The valid
case is ADDITIONALLY ratified on real altw output at Brick 4 activation
(superset semantics and all); that supersedes nothing here.

The tests live in `src/pipeline_tests.rs` - as the sibling
`pipeline::tests` module it calls `phase_read_and_process(&config)`
directly and reads `Phase12Stats`, same fixture pattern as the existing
`PbfWriter` tests. Test-local helpers (comment the wire layout in the
helpers themselves - code comments outlive notes/):

- `way_members_field5_payload(encoded_count: u32, bitmap: &[u8]) ->
  Vec<u8>` - version byte + varint count + bitmap.
- `splice_way_members(pbf: &mut Vec<u8>, osmdata_index: usize,
  payload: &[u8])` - append field 5 to the BlobHeader of the Nth
  OSMData frame, patch that frame's length prefix.

Shared fixture: header with both features declared; one way blob via
`add_way_with_locations` holding three tagless ways (positions 0..=2,
way 2's locations forming a small closed square - single-tile at the
test zoom); one relation blob holding a shortbread-matching multipolygon
(type=multipolygon + landuse=forest) whose only member is way
position 2's id. Four cases:

- `injected_members_end_to_end_marks_ways_and_skips_prepass`: splice a
  valid payload (count 3, bitmap `0b0000_0100` - position 2 only).
  `phase_read_and_process` returns Ok; assert
  `relation_plan_needed_ways == 0` and `relation_plan_superset_ways ==
  0` (the prepass was never spawned), `way_members_marked == 1` (no
  over-marking), and `rel_count == 1` with
  `missing_refs.missing_relation_way_refs == 0` - the relation found
  its member way in the way_index, proving the bit was consumed at the
  right POSITION (an off-by-one marks way 1 instead, way 2 misses the
  index, and the missing-ref counter fires).
- `injected_members_missing_field5_fails_run`: no splice; the header
  still declares WayMembers-v1. Err (corrupt enrichment - absence).
- `injected_members_malformed_field5_fails_run`: splice a payload with
  version byte `0x02`. pbfhogg's accessor yields `None`; Err (same
  corrupt-enrichment class, proving the malformed arm flows through the
  decode worker and the consumer's `?`).
- `injected_members_count_mismatch_fails_run`: splice encoded count 4
  with a one-byte bitmap over 3 actual ways. `4.div_ceil(8) == 1`, so
  pbfhogg's internal length check PASSES and the failure must come from
  elivagar's encoded-vs-actual compare in `validate_and_take_members` -
  the within-one-byte producer bug that check exists to catch.

All four run under `brokkr check` today (no enriched dataset, no pbfhogg
change) and land in the same single commit as the plumbing. The
"pbfhogg is untouched" stopping rule stands as written.

## Gates

Standing gates per the wait-work note: `brokkr check`; per-variant
neutrality via `brokkr regress` (raw-vs-raw AND locations-vs-locations,
same-variant, exit-coded); wall-time neutrality via bench. Benchmark
discipline: commit first, then measure, then record against the hash
(`reference/performance.md` reading rules; best-of-3, deltas under ~5%
read as noise).

Two corrections folded from review, both material to whether the gates
mean anything:

- `brokkr compare-tiles` is NOT a correctness gate and is demoted to
  informational (review R2-1). It samples 200 tiles per zoom by default
  (omitting `--sample` does NOT make it exhaustive - the default is 200,
  not "all"), it compares only per-layer feature counts, geometry-type
  breakdowns, and total geometry-command COUNTS - never coordinates,
  attributes, ids, tile presence, or bytes - and its `main` prints and
  returns 0 with no failure verdict. Two different geometries with equal
  command counts read as identical, and the process never fails. The
  neutrality proof must instead be a semantic `brokkr regress` between
  the pre-landing and post-landing archives OF THE SAME VARIANT, which
  decodes every tile, diffs coordinates/attrs/ids/ring-roles, and exits
  nonzero on any structural diff.
- `brokkr regress` with no `--file/--against` compares the current-HEAD
  build's archive against the dataset's single blessed archive, and the
  denmark blessed is a LOCATIONS-variant output (contract line 54). So
  running a raw bench and then a bare `brokkr regress --dataset denmark`
  compares a raw archive against a locations blessed - the exact
  cross-variant false alarm this spec cites from 2026-07-09 (review
  R2-2). The raw neutrality check must regress raw-vs-raw via explicit
  `--file`/`--against` paths, never against the locations blessed. Every
  bench also pins `--variant` explicitly (contract: gate commands pin
  dataset AND variant); `brokkr tilegen --bench 3 --dataset denmark`
  without `--variant raw` is underspecified.

The real-PBF bench and tilegen runs below are the AGENTS.md
"user explicitly asks" trigger (AGENTS.md forbids full-pipeline runs on
real PBF data otherwise); running these gates IS that explicit request -
do not run them reflexively outside the gate (review R1-process).

Step 0, BEFORE the landing, at clean pre-landing HEAD (`430f28b`,
verified per the Baselines note). This step captures BOTH the wall
baselines AND the pre-landing per-variant ARCHIVES that the post-landing
same-variant regress diffs against - the regress `--against` references.
It cannot reuse the durable-dir archives blindly: those are keyed by
commit hash alone (`data/tilegen/<dataset>-<commit>.pmtiles`), so a raw
build and a locations build at the SAME commit collide at the SAME path
and overwrite each other. Retain each variant under a distinct explicit
path before the next build overwrites it.

```
brokkr tilegen --bench 3 --dataset denmark --variant locations
# retain the locations archive -> data/tilegen/denmark-430f28b-locations.pmtiles
brokkr tilegen --bench 3 --dataset denmark --variant raw
# retain the raw archive -> data/tilegen/denmark-430f28b-raw.pmtiles
brokkr tilegen --dataset germany --variant locations
# retain -> data/tilegen/germany-430f28b-locations.pmtiles
```

Record both denmark UUIDs (the raw and locations wall baselines). A fresh
raw baseline is captured here rather than reusing `262d55f3` only if the
HEAD-drift check failed; if HEAD is confirmed `430f28b`, run `262d55f3`
(commit `430f28b`, plantasjen, 16.7s) remains the raw wall baseline and
the raw bench above is optional (but the retained raw ARCHIVE is still
required as the regress reference). The commit-`4ceacd1` archives
(`denmark-4ceacd1.pmtiles`, `germany-4ceacd1.pmtiles`) predate `430f28b`
(a way-path change), so they are NOT the primary neutrality reference -
the `430f28b` retained archives isolate THIS landing's effect; the
`4ceacd1` archives are at most a secondary informational compare-tiles.

Then, in order (never two elivagar processes at once):

```
brokkr check
# green -> brokkr fmt, commit the landing

# raw neutrality: build raw, retain it, regress raw-vs-raw
brokkr tilegen --bench 3 --dataset denmark --variant raw
# retain the post-landing raw archive -> <post raw output>
brokkr regress --dataset denmark --file <post raw output> --against data/tilegen/denmark-430f28b-raw.pmtiles

# locations neutrality: build locations, regress locations-vs-locations
brokkr tilegen --bench 3 --dataset denmark --variant locations
brokkr verify pmtiles --dataset denmark
brokkr regress --dataset denmark --file <post locations output> --against data/tilegen/denmark-430f28b-locations.pmtiles
brokkr tilegen --dataset germany --variant locations
brokkr regress --dataset germany --file <post germany locations output> --against data/tilegen/germany-430f28b-locations.pmtiles

# optional, informational only (NOT a pass/fail gate - see intro):
# brokkr compare-tiles data/tilegen/denmark-4ceacd1.pmtiles <post locations output>
```

(`<post ... output>` = the archive the immediately preceding
`brokkr tilegen` run wrote; retain it under a distinct path before any
later same-commit build overwrites it, exactly as in step 0. Both
`regress` calls are SAME-variant - raw against the pre-landing raw
archive, locations against the pre-landing locations archive - so
neither is the 2026-07-09 cross-variant trap. `brokkr regress` decodes
every tile, so the neutrality proof is exhaustive without any `--sample`
knob; that exhaustiveness is regress's, not compare-tiles'.)

Pass bars:

- `brokkr check`: green, including the new tests.
- All three same-variant `brokkr regress` runs (denmark raw-vs-raw,
  denmark locations-vs-locations, germany locations-vs-locations): exit
  0, zero diffs, tolerance 0 (the change is output-neutral by
  construction; anything structural means the plumbing leaked into the
  live path). Each is same-variant with explicit `--file`/`--against`, so
  none is a cross-variant read.
- `brokkr verify pmtiles --dataset denmark`: zero errors.
- denmark raw bench vs `262d55f3` (16.7s) and denmark locations bench vs
  the step-0 UUID: not slower beyond ~5% noise. There is no win to
  claim; the bound is pure neutrality. The touched costs are one
  `Option<Box<[u8]>>` (always `None` today) per routed block, a struct
  wrap on the way channel, a short-circuited boolean in the decode
  workers, and the `way_members_marked` tally (folded into
  `build_way_plans` so it adds no extra pass; see the Counter note) -
  all epsilon, and the bench proves it.
- compare-tiles, if run, is read as informational context only, never as
  a pass/fail bar (review R2-1: it samples, counts, and always exits 0).

Keep/revert: any bar missed reverts the landing (one commit, one
revert). A compare-tiles or regress diff is not patch-forward material -
dormant plumbing that changes tiles is wrong at the design level, and
the revert plus a survey correction to this spec comes first.

After the gates: write the before/after bench numbers (both denmark
variants' UUIDs - raw and locations; germany has no bench in this
landing, review R2-6 - plus commit hashes and host) into
`reference/performance.md` as a neutrality record - this landing touches
measured paths, so it owes the record even though it claims no effect.

## Stopping rule

- **Brick 5, whole, is out** (named exclusion, needs enriched data):
  `PinSource`, field-20 / `Way::shared_node_pins()` consumption, the
  positional `preserve_vertex_mask` fill, `way_pins_marked`, the
  `prepass_shared_nodes` teardown, and the removal of the
  `global_shared` parameter from `build_way_plans`.
- **Brick 4's activation residue is out**: the superset cost reading
  (`way_index_data_bytes` bound on germany locations), the H2a
  prepass-join performance verdict, and the earcut-oracle run on
  enriched output all require Brick 3 data and stay with Brick 4 proper.
  After this landing, that residue contains no elivagar code changes.
- **pbfhogg is untouched**: the producer API this consumes is already
  public in the path dependency.
- The relation tail, ocean, sort, and assemble phases are untouched; the
  raw ordered read path changes only by the `route_block!(block, None)`
  wrap.
- No blessing: the regress baseline is not rotated.

## Review fold (R1 Opus + R2 codex, 2026-07-11)

Both reviews were validated against the code before folding. Findings
folded above, by origin:

- R2-1 (High): compare-tiles is not a correctness gate (200/zoom sample,
  count-only comparison, always exits 0) - Gates rewritten to prove
  neutrality with same-variant `brokkr regress`; compare-tiles demoted to
  informational.
- R2-2 (High): bare `brokkr regress` is cross-variant (denmark blessed is
  locations) - Gates now regress raw-vs-raw and locations-vs-locations via
  explicit `--file`/`--against`, every bench pins `--variant`.
- R2-3 (High): the injected plumbing has no integration test; pbfhogg's
  field-5 writer is `pub(crate)` - added the "Integration coverage"
  subsection with the synthetic-enriched-PBF instrument. Adjudicated
  2026-07-11: the four-case integration test lands in THIS landing via a
  test-local field-5 byte splice over pbfhogg's public writer API -
  neither the pbfhogg-touch brick nor the deferral. See the subsection
  for the ruling and the test shapes.
- R2-4 (Medium): `MembersForBlock` must be `pub(super)` for the sibling
  test module - fixed in Target artifacts.
- R2-5 == R1-4 (Medium/smell): the `way_members_marked` filter().count()
  is a second live-path O(plans) pass omitted from the cost survey - fold
  the tally into `build_way_plans`; cost bound updated.
- R2-6 (minor): "both datasets' UUIDs" -> both denmark variants (germany
  has no bench).
- R1-1 (gap): verify branch tip is `430f28b` before reusing raw baseline
  `262d55f3` - added to Baselines (confirmed `430f28b` at spec time).
- R1-2 (gap): `actual_way_count` must use the `Element::Way` filter
  (== `plans.len()`), not `block.elements().count()` - added to the
  UnorderedBlockSource data flow.
- R1-3 (nit): name the `u32`-vs-`usize` count-compare cast form - added
  to `validate_and_take_members`.
- R1-5 (nit): `member_bit` should use bounds-safe access - added to its
  doc.
- R1-process: real-PBF bench is the AGENTS.md explicit-ask trigger - noted
  in Gates.

Rejected:

- R1's "superset semantics" observation. R1 itself labels it "correctly
  deferred, not a defect": when activated, the injected bitmap carries
  relation-membership superset semantics vs the Plan arm's shortbread-
  matched subset. The spec already quarantines this as Brick 4's superset
  cost reading and keeps this landing dormant, so it does not touch
  today's neutrality. Nothing to change.
- R1's suggestion to also fix the contradiction in the PARENT spec
  (`notes/injected-prepass-spec.md`, the flagged-raw-file "ignores vs hard
  error" contradiction). Real and worth doing, but out of scope for THIS
  file - the "Stale text in the parent spec, resolved" section already
  records the resolution this spec implements. Left as a parent-spec edit
  for whoever touches that document; not folded here.
- The overall-verdict prose in both reviews (R1 "no bugs"; R2 "not
  implementation-ready") is not a discrete finding and needs no fold; R2's
  three High items are the substance and are folded.
