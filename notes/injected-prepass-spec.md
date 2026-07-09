# Spec: injected relation plan and exact shared-node pins (H2a + H2b)

Status: specified 2026-07-09, review-refined 2026-07-09 (R1 Opus + R2
codex folded, see Review resolutions), not implemented.

Written against `reference/technical-implementation-spec.md` (the contract
for this document). Spawned from `notes/planet-30gb-roadmap.md`, hypothesis
H2, items (a) "Relation plan" and (b) "Exact shared-node pins". Measurement
record: `reference/performance.md` + `.brokkr/results.db`.

## The problem, priced

The production input is written by pbfhogg (`cat -> merge ->
add-locations-to-ways`). elivagar nevertheless re-derives two global facts
from the file on every run:

1. **The relation plan.** `prepass_relation_plan`
   (`src/pipeline/phase12.rs`) reads every relation blob, shortbread-matches
   each `type=multipolygon` / `type=boundary` relation, and collects member
   way ids into `RelationPlan { needed_ways: FxHashSet<i64> }`. The way pass
   consults it per way (`rp_ref.needed_ways.contains(&plan.way_id)`) to
   decide `is_member`, which gates `way_index` puts. The prepass runs on its
   own thread from phase12 start, but on the locations path way blocks
   arrive almost immediately, so the join at the first way block is nearly
   all stall: 6.3s on germany locations, 13.2s raw (roadmap H2 evidence;
   `prepass_join` wait counter). Minutes at planet scale, plus a planet-
   scale `FxHashSet<i64>` of ~30-40M member ways (order 1 GB of RAM) held
   for the whole way phase.

2. **Shared-node pins.** DP simplification must not move junction vertices
   independently in the ways that share them (visible gaps otherwise). The
   exact global answer was `prepass_shared_nodes` (a second full way-blob
   read + external merge-sort of every node ref); it is disabled by default
   (`global_shared_node_pins: false`, "substantial runtime and RSS cost")
   and production runs use the block-local approximation
   (`shared_node_counts` / `preserve_refs_for_way` inside
   `build_way_plans`): junctions are only pinned when both ways sit in the
   same PBF primitive block. Cross-block junctions are unpinned - a standing
   quality compromise.

Both facts are computable at enrichment time. altw already streams every
way and every relation of the planet once; the marginal cost of computing
membership and shared-ness there is paid once per enrichment instead of
once per tilegen run. This spec injects both through pbfhogg's existing
metadata channel and deletes the runtime derivation on the production path.

Honesty clause (roadmap H10): any record claim on enriched input must
publish both framings. This spec moves work, it does not destroy it. The
framing must also account for the two costs the relocation ADDS, not just
the runtime work it removes: (1) on-disk growth of the enriched file (field
5 on every way blob, field 20 on every way that carries a pin), and (2) the
per-read decode/skip tax paid by EVERY reader of the enriched file, not
only elivagar.

Dual-consumer decision (MEMORY: the enriched altw output is read by two
parallel consumers - elivagar tilegen AND the nidhogg PBF-ingest query
API). There is ONE shared enriched file carrying both fields, not an
elivagar-only pin variant. This keeps the "once per enrichment" property
intact (altw runs once). Field 20 sits at a field number outside
osmformat.proto/osmium, so nidhogg's ingest (and any standard reader) skips
it as an unknown field: the query API pays only a wire-skip cost, never a
decode/allocate cost, and its output is unaffected. Field 5 rides the
BlobHeader and is likewise skipped by readers that do not opt into parsing
it (see the read-path enablement note below). The on-disk-growth gate in
Brick 3 measures the shared-file size delta so this added cost is on the
record, per the honesty clause.

## Survey of the ground

### elivagar today (`src/pipeline/phase12.rs`, HEAD `4f2cd90`)

- `phase_read_and_process` detects locations mode from the header:
  `optional_features().iter().any(|f| f == "LocationsOnWays")` or the
  `--locations-on-ways` flag. The locations path reads blocks through the
  elivagar-owned `UnorderedBlockSource` (BlobReader -> bounded raw channel
  -> decode workers -> bounded decoded channel); the raw path uses
  pbfhogg's ordered `into_blocks_pipelined`. Both feed the shared
  `route_block!` macro; way blocks go through `block_tx:
  SyncSender<PrimitiveBlock>` to the way worker.
- `prepass_relation_plan` is spawned unconditionally at phase12 start;
  `prepass_shared_nodes` only when `config.global_shared_node_pins` (off by
  default; the else-branch logs "Global shared-node prepass disabled -
  using block-local pins"). Both are joined at the first way block under
  the `WAIT.prepass_join` span, and again after the read loop if no way
  block ever arrived.
- `build_way_plans(block, global_shared)` produces one `WayPlan { way_id,
  node_refs, preserve_node_refs }` per way element, positionally zipped
  against `block.elements()` in the task loop (dropping any plan would
  misalign the zip - documented invariant). It computes block-local counts
  via `shared_node_counts` over each way's `shared_scan_slice` (empty for
  ways with <= 2 refs; closed rings drop the trailing duplicate) and fills
  `preserve_node_refs` via `preserve_refs_for_way` (block counts >= 2, OR
  membership in the global set when supplied).
- `process_planned_way_into(way, plan, is_member, ...)`: `is_member` lets
  tagless member ways through the early return and pushes `(way_id,
  coords)` into `acc.way_puts` (the way_index feed). Emission itself is
  still gated by a shortbread match, so a false-positive `is_member` costs
  way_index bytes and coord resolution, never wrong output. The preserve
  set becomes a positional `preserve_vertex_mask: Vec<bool>` by matching
  node ids against `resolved_node_refs`; in locations mode
  `resolved_node_refs` is `plan.node_refs.clone()` (no store, nothing
  missing), so mask positions equal ref positions.
- The relation tail (buffered blocks or `BlobFilter::only_relations`
  re-read) never touches `needed_ways`; it re-matches relations and reads
  `way_index`. The only consumer of the plan is the `is_member` check.
- Ledger counters already emitted: `relation_plan_needed_ways`,
  `global_shared_nodes`, `way_index_ways` / `way_index_data_bytes` /
  `way_index_index_bytes` (germany locations: way_index 126 MB total,
  roadmap H3 first reading).
- Semantics quirk worth recording: the disabled `prepass_shared_nodes`
  counts EVERY ref including a ring's closing duplicate, so every closed
  ring's start vertex self-counts to 2 and gets pinned - a divergence from
  the block-local scan-slice semantics. The injected design below adopts
  the scan-slice semantics (closing duplicate skipped) and fixes this.
- Failure history: `notes/rendering-fix-log.md` (the R/S ledger) no longer
  exists in the tree; `notes/rendering-postmortem.md` is the surviving
  geometry failure history. Nothing in it involves pin injection or
  membership planning; the pin-adjacent history is exactly the two facts
  above (global prepass disabled on cost; block-local approximation
  accepted). No logged failure is being re-proposed.

### pbfhogg today (`../pbfhogg`, path dependency)

- BlobHeader wire fields (`src/read/blob_wire.rs`): 1 = type, 2 =
  indexdata (parsed only at 26/42 bytes, stored as an inline fixed array -
  the v1/v2 `BlobIndex`: kind, id range, count, node bbox), 3 = datasize,
  4 = tagdata (variable-length `Box<[u8]>`, the per-blob tag-key index).
  Unknown fields are skipped. `MAX_BLOB_HEADER_SIZE` = 64 KiB (PBF spec) -
  a hard budget for anything header-carried.
- `BlobFilter` (`src/blob_meta/mod.rs`) skips blob decompression by
  indexdata kind / bbox / tag keys. `Blob::index()` and `Blob::tag_index()`
  are `pub(crate)` - there is no public per-blob metadata accessor today.
- altw (`src/commands/altw/mod.rs`): two passes (nodes -> index; re-read ->
  rewrite), `require_indexdata` on input, sparse and external (bounded
  memory, double radix permutation over refs) index modes, way blobs are
  re-encoded with locations (`encode_way_with_locations` and raw-bytes
  variants), `HeaderBuilder` carries `optional_features`.
- Way message fields in use: 1 id, 2 keys, 3 vals, 4 info, 8 refs, 9 lat /
  10 lon (the osmium locations-on-ways convention). Field numbers >= 16
  encode with 2-byte tags and are unused by osmium and upstream
  osmformat.proto.

### Consequence of the survey

- Per-way pin data cannot live in the BlobHeader: a dense way blob carries
  hundreds of thousands of refs; a ref bitmap alone can exceed the 64 KiB
  header cap. Pins must ride in the Way messages, like the locations do.
- Per-blob membership CAN live in the BlobHeader, but the 8000 ways/block
  figure is a DEFAULT, not an invariant: pbfhogg's `BlockBuilder` takes a
  caller-supplied element cap and explicitly supports planet-style
  densities (~228k entities/blob, `with_element_cap`). At the 8000 osmium
  default the bitmap is ~1 KB; at 228k ways it is ~28 KB - still under the
  64 KiB BlobHeader budget, but the sizing must be stated against altw's
  ACTUAL chosen density, not the default. See the field-5 size policy in
  the contract.
- The fixed-size inline `indexdata` parse is a deliberate hot-path
  optimization; extending its format would perturb every read path. A new
  BlobHeader field (5), parsed like tagdata, is the non-invasive channel.
- elivagar's positional plan/element zip and the locations-mode identity
  between ref positions and coord positions make positional bitmaps the
  natural consumption shape - no id sets, no lookups.

## The contract (normative for both repos)

### Header feature flags

altw output declares, in `HeaderBlock.optional_features`, alongside
`LocationsOnWays`:

- `pbfhogg.WayMembers-v1` - every OSMData way blob carries BlobHeader
  field 5 (way-member bitmap), and it is trustworthy.
- `pbfhogg.SharedNodePins-v1` - every way element carries exact shared-node
  pin data (field 20, possibly omitted when empty), and it is exact.

elivagar treats the flags as authoritative and validates them with
CHECKED errors that fire in RELEASE, not `debug_assert!` (a corrupt
enrichment is exactly the case you want loud in production). Three failure
classes, all hard errors on the injected path:
- field 5 absent on a way blob under `WayMembers-v1` = corrupt enrichment
  (field 5 has presence=validity semantics: an all-zero bitmap is still
  emitted, so absence is unambiguously corruption). Field 20 absence is NOT
  in this class - omission legitimately means "no pins" and the reader
  cannot distinguish a dropped pin bitmap from a genuinely pinless way, so
  field-20 absence is never treated as corruption;
- data present but WRONG LENGTH (field-5 bitmap not matching the blob's way
  count, or field-20 bitmap not matching the way's ref count) = corrupt
  enrichment;
- on the injected pin path, a way whose written location count does not
  equal its ref count (see the coords/ref alignment guard in Target
  artifacts) = corrupt enrichment.
Missing data is not the only corruption; a present-but-wrong-length bitmap
silently misaligns membership or pins and MUST be caught, so the length and
alignment checks below are promoted from `debug_assert!` to release-checked
errors that fail loudly, matching the house style of loud invariant
violations.

Flag/mode combinations. The injection is honored only on the
locations-on-ways path (altw output always declares `LocationsOnWays`). A
file that declares `WayMembers-v1` or `SharedNodePins-v1` WITHOUT
`LocationsOnWays` is a malformed enrichment (altw never produces it): this
too is a hard error at detection, not a silent ignore-and-log - "authori-
tative" and "loudly reject the impossible combination" are the same stance.
Files without ANY of the flags (raw Geofabrik, pre-change locations files)
take today's runtime paths unchanged; that is the supported non-enriched
case, distinct from a partially-flagged file.

### BlobHeader field 5: way-member bitmap

protobuf field 5, wire type 2 (len-delimited bytes), on OSMData way blobs
only. Layout:

```
byte 0        version, 0x01
varint        way_count (number of Way elements in the blob)
ceil(n/8) B   bitmap, LSB-first: bit i = way at position i among the
              blob's Way elements in file order is a member way
```

Bit i set means: way i is referenced as a Way-type member by at least one
relation with tag `type=multipolygon` or `type=boundary` (superset
semantics - no shortbread matching in pbfhogg; the coupling stays zero).
Blobs where no way is a member still carry the field (presence = validity;
all-zero bitmap).

Size policy (the 64 KiB BlobHeader cap is a hard limit, not a soft one).
The bitmap is `ceil(way_count/8)` bytes plus the version/count preamble, so
its size is set by altw's per-blob element density, which is caller-
configurable in pbfhogg (`with_element_cap`), NOT fixed at 8000. Bounds:
8000 ways -> ~1 KB; 228k ways (planet-style density) -> ~28 KB, still under
budget; the header overflows only above ~512k ways/blob. altw is REQUIRED
to keep its way-blob element cap at or below the density where field 5 plus
all other header fields fit the 64 KiB budget (the 8000 osmium default and
the 228k planet default both satisfy this with wide margin). altw asserts
the encoded BlobHeader length against the cap when writing field 5; a blob
that would overflow is a hard failure at write time (split the blob or
reject), never a truncated bitmap. elivagar's reader validates field-5
length against the blob's way count regardless, so a truncated or
oversized bitmap is caught on read as a corrupt-enrichment hard error.

### Way message field 20: shared-node pin bitmap

protobuf field 20, wire type 2 (len-delimited bytes), inside the Way
message, alongside refs (8) / lat (9) / lon (10). Layout: bitmap,
LSB-first, bit i = the node at ref position i is a shared node. Length is
exactly `ceil(ref_count/8)` when present. The field is OMITTED when no bit
is set (the common case; omission = no pins). Field 20 is outside the
range used by osmformat.proto and osmium; standard readers skip it.

Shared-ness, exact definition (computed by altw over ALL ways of the
input): for each way take its refs, minus the trailing ref when
`len >= 4 && first == last` (ring-closure duplicate). Count occurrences of
each node id across all these slices, all ways, including repeats within
one way. A node id with total count >= 2 is shared. The bitmap sets the
bit at EVERY position holding a shared id, including a closed ring's
trailing duplicate (it mirrors bit 0 by construction).

Two deliberate divergences from elivagar's block-local semantics, both
quality-positive and inside this landing's geometry-change budget:
(1) ways with <= 2 refs CONTRIBUTE occurrences (a 2-node connector's
endpoints are real junctions; block-local excluded them wholesale) though
elivagar still ignores pins on <= 2-ref ways at consumption (nothing to
simplify); (2) the ring-closure self-count of the old global prepass is
gone (closing duplicate skipped when counting).

Alignment contract: altw computes the bitmap over the exact ref/location
arrays it writes for that way, so bitmap length always matches the written
ref count. On the injected pin path elivagar maps `bit i ->
preserve_vertex_mask[i]`, where the mask is sized by the RESOLVED
coordinate count (`coords_e7.len()`, from `way.node_locations()`), while
the bitmap length is keyed to the ref count. That mapping is only correct
when `node_locations().len() == refs().len() == coords_e7.len()`. Locations
mode has no missing-node compaction so this holds by construction, but the
consumption must not merely ASSUME it: elivagar checks BOTH
`bitmap.len() == refs.len().div_ceil(8)` AND
`coords_e7.len() == refs.len()` as RELEASE-checked hard errors (not
`debug_assert!`), because a location/ref count mismatch silently pins the
wrong vertices - a corruption of DP retention, not of ring validity, which
the earcut oracle would not necessarily catch.

### pbfhogg public API (consumed by elivagar)

- `Blob::way_members(&self) -> Option<&[u8]>` - the raw bitmap bytes of
  header field 5 (version + count stripped and validated), `None` when the
  field is absent. Public (today's `Blob::index()` is `pub(crate)`; this is
  the first public per-blob metadata accessor).
- Read-path enablement (field 5 is currently SKIPPED). `WireBlobHeader::
  parse` today decodes only fields 1-4 and gates fields 2/4 behind
  `parse_indexdata` / `parse_tagdata` toggles; field 5 falls through to
  `skip_field`. Parsing field 5 therefore needs a new opt-in toggle
  (`parse_waymembers`, modeled on `parse_tagdata`) threaded through
  `BlobReader::new` the same way, so hot read paths that do not want it pay
  nothing. elivagar's `UnorderedBlockSource` sets this toggle ON exactly
  when the header declares `pbfhogg.WayMembers-v1`; every other elivagar
  read path (and every other consumer) leaves it off and skips the field.
  This is the concrete answer to "how elivagar enables field 5": it is
  opt-in, keyed to the feature flag, set at reader construction.
- `Way::shared_node_pins(&self) -> Option<&[u8]>` - the field-20 bitmap,
  `None` when omitted.
- `HeaderBuilder` grows nothing structurally (it already owns
  `optional_features`); altw appends the two feature strings.

### altw computation (pbfhogg side, pinned at algorithm level)

- Membership: a relation-blob pre-scan (`BlobFilter::only_relations`,
  indexdata-assisted skip - relations are the small file tail) before any
  way blob is written; member way ids of `type=multipolygon|boundary`
  relations go into an `IdSet` (module exists); pass 2 reads the set when
  building each way blob's field 5.
- Shared counting: structurally the same join altw already performs to
  attach coordinates to refs. External mode: the double radix permutation
  already brings every ref occurrence together per node id; a run-length
  >= 2 over the id-sorted refs yields the shared bit, carried back through
  the same permutation as one extra bit beside the (lat, lon) payload -
  bounded memory is preserved. Sparse mode (small extracts): an
  occurrence-count table is fine. Either way pass 2 sets field 20 from the
  per-ref shared bits it already has in way order.
- Every way blob altw writes carries field 5; every way it writes carries
  field 20 semantics (a passthrough fast path that would skip re-encoding
  way payloads is incompatible with the pins flag and must be disabled or
  made pin-aware when pins are requested - pbfhogg-side detail, contract
  stands).

The pbfhogg implementation is specified to pbfhogg's own standards in a
paired document in that repository; THIS contract section is the interface
both implementations are written against. That is a named exclusion, not a
deferral: the format, semantics, feature strings, and API surface are all
pinned here.

## Target artifacts (elivagar)

`src/pipeline/phase12.rs` unless noted.

```rust
/// A way block plus its injected per-blob metadata, as routed to the way
/// worker. `members` is Some only on the injected locations path.
struct WayBlock {
    block: PrimitiveBlock,
    members: Option<Box<[u8]>>, // field-5 bitmap bytes, validated
}

/// Where `is_member` comes from for this run. Resolved once at HEADER
/// DETECTION (phase12 start, beside the `LocationsOnWays` sniff) - not
/// deferred to the first way block, which would needlessly mirror the
/// prepass-join timing this spec deletes. `injected_members` is known from
/// the header before any block is read.
enum MemberSource {
    /// Header declares pbfhogg.WayMembers-v1: read per-blob bitmaps.
    Injected,
    /// Runtime relation plan (today's path), joined from the prepass.
    Plan(std::sync::Arc<RelationPlan>),
}

/// Where DP pins come from for this run.
enum PinSource {
    /// Header declares pbfhogg.SharedNodePins-v1: read Way field 20,
    /// skip block-local counting entirely.
    Injected,
    /// Block-local counting (today's path).
    BlockLocal,
}

pub(super) struct WayPlan {
    pub(super) way_id: i64,
    pub(super) node_refs: Vec<i64>,
    /// Filled on the BlockLocal path; empty on the Injected path
    /// (pins are read positionally from the Way message instead).
    pub(super) preserve_node_refs: Vec<i64>,
    /// Resolved at plan build for BOTH paths (a plain `bool`, so it cannot
    /// be deferred): the bitmap bit on the Injected path, the `needed_ways`
    /// lookup performed INSIDE `build_way_plans` on the fallback path. The
    /// old task-site lookup at the `process_planned_way_into` call is gone
    /// - one place computes membership.
    pub(super) is_member: bool,
}
```

Data flow changes:

- Detection, next to the existing `LocationsOnWays` sniff:
  `injected_members` / `injected_pins` from the two feature strings,
  honored only when `locations_on_ways` (altw output always is; a flagged
  file on the raw path ignores the injection and logs it).
- `prepass_relation_plan` is spawned only when `!injected_members`.
  `prepass_shared_nodes` is DELETED (see teardown), so nothing spawns for
  pins in any mode.
- `UnorderedBlockSource` decode workers call `blob.way_members()` before
  decoding and yield `(PrimitiveBlock, Option<Box<[u8]>>)`; `route_block!`
  and `block_tx` carry `WayBlock`. The raw ordered path wraps blocks with
  `members: None`. Enforcement point for "field 5 missing on a way blob
  under `injected_members`": the decode worker, right where it calls
  `blob.way_members()`. `injected_members && blob is OSMData-ways &&
  way_members().is_none()` is a hard error raised there (fail the run), so
  a corrupt enrichment is caught before the block reaches the way worker.
  The way worker then trusts `WayBlock.members` is `Some` on the injected
  path.
- `build_way_plans` loses its `global_shared: &FxHashSet<i64>` parameter
  (deleted with the prepass); the new signature is
  `build_way_plans(block: &PrimitiveBlock, members: MembersForBlock<'_>,
  pins: &PinSource) -> Vec<WayPlan>` with
  `enum MembersForBlock<'a> { Bitmap(&'a [u8]), Set(&'a FxHashSet<i64>) }`
  (the per-block projection of `MemberSource`: the worker passes
  `Bitmap(way_block.members)` on the injected path, `Set(&plan.needed_ways)`
  on the fallback). On `PinSource::Injected` it skips `shared_node_counts` +
  `preserve_refs_for_way` (whole per-block counting pass gone - this was
  `phase12_plan_build_ns` thread-time); `preserve_node_refs` stays empty.
  `is_member` fills from the bitmap positionally (plan index i = way
  element i, the invariant the zip already relies on) or from the set -
  either way it now fills at plan build, and the task-site lookup at the
  `process_planned_way_into` call goes away (one place computes it).
- `process_planned_way_into` builds `preserve_vertex_mask` from
  `way.shared_node_pins()` when `PinSource::Injected`: bit i ->
  `preserve_vertex_mask[i]`. Validation is RELEASE-checked (not
  `debug_assert!`): both `bitmap.len() == refs.len().div_ceil(8)` and
  `coords_e7.len() == refs.len()` are hard errors on the injected path (see
  the field-20 alignment contract). Locations mode has no missing-node
  compaction so the coord/ref counts align by construction, but the check
  makes a malformed way loud instead of silently mispinning. The
  id-matching branch remains for the BlockLocal path.
- Counters: `relation_plan_needed_ways` keeps its meaning on the fallback
  path and reads 0 on the injected path; new `way_members_marked` (ways
  with the member bit set, summed) preserves ledger visibility of the
  way_index feed; new `way_pins_marked` (set bits consumed). Both ride the
  existing end-of-run counter flush in `src/debug.rs` conventions.

## Teardown (with the H2b landing)

Deleted outright:

- `prepass_shared_nodes` and its machinery: `NodeRefChunkReader`,
  `NodeRefHeapEntry`, `encode_signed_i64_key` / `decode_signed_i64_key`,
  the prepass scratch dir handling (~180 lines).
- `TilegenConfig.global_shared_node_pins` (`src/pipeline/mod.rs`), the CLI
  flag (`src/main.rs`), the four `pipeline_tests.rs` config-literal
  fields, the `global_shared_nodes` stat + counter emission
  (`src/pipeline/stats.rs`, `mod.rs`), the doc line in `src/lib.rs`.
- `annotate_global_shared_node_refs` (cfg(test), already dead).
- The `gsn` Arc, its join, and the `global_shared` parameter threading.

Kept: `WAIT.prepass_join` (still spans the relation-plan join on the
fallback path), `shared_node_counts` / `preserve_refs_for_way` /
`shared_scan_slice` and their tests (the BlockLocal fallback is live
code), `prepass_relation_plan` (fallback for non-enriched input).

## Bricks

Each brick is one commit; `brokkr check` and `elivagar verify` are green at
every boundary. Commit first, then measure, then record numbers against the
hash (`reference/performance.md` discipline; keep/revert verdicts from
`--bench 3` best-of, deltas under ~5% treated as noise).

### Brick 1 - price the superset (elivagar, instrument-first)

The one estimate in this design is superset inflation: altw marks members
of ALL mp/boundary relations, elivagar's exact plan only shortbread-matched
ones. Instrument before any format work: in `prepass_relation_plan`, also
collect member way ids of the mp/boundary relations that FAIL the
shortbread match into a second set; emit `relation_plan_superset_ways`
beside the existing `relation_plan_needed_ways`. Transient memory,
end-of-run counters, zero timing impact on the measurement path.

`relation_plan_superset_ways` is, explicitly, the size of the UNION of
`needed_ways` (members of mp/boundary relations that ALSO pass the
shortbread match) with the members of mp/boundary relations that FAIL the
shortbread match. That union equals "members of ALL mp/boundary relations"
- exactly the set altw marks in field 5 (superset semantics, no shortbread
in pbfhogg). So the counter is a direct read of what the injected bitmap
will contain, and `relation_plan_superset_ways / relation_plan_needed_ways`
is the superset inflation factor.

Gates:
```
brokkr check
brokkr tilegen --bench --dataset germany --variant locations
brokkr sidecar <uuid> --counters      # read both relation_plan_* counters
brokkr tilegen --bench --dataset denmark --variant locations
brokkr sidecar <uuid> --counters
```

Proceed threshold: `relation_plan_superset_ways <= 1.5 x
relation_plan_needed_ways` on germany locations -> superset semantics
proceed as specified. This count ratio is only an EARLY SCREEN, not the
governing gate: the real cost is BYTES (a superset way with a large
coordinate array costs far more than its count implies), which Brick 4
gates directly via `way_index_data_bytes`. Passing the cheap count proxy
here does not by itself imply passing Brick 4's bytes bound; Brick 4
governs. Brick 1 exists to catch a gross count blowup before any format
work, cheaply.

Above 1.5x -> the membership contract gains a relation tag filter: altw
takes the filter as a CLI argument evaluated with pbfhogg's existing
`tag_expr` machinery against relation tags, and this spec's field-5
semantics become "member of a filter-passing mp/boundary relation"; the
filter expression published here would then be derived from the shortbread
relation matchers and recorded in this spec before Brick 2 lands. Name the
trade honestly: this contingency is NOT free and it negates a headline
design goal. The superset design's stated virtue is that "the coupling
stays zero" (no shortbread knowledge in pbfhogg); a `tag_expr` filter
derived from the shortbread relation matchers is a durable, cross-repo
shortbread->altw coupling that must be kept in sync forever. It is a whole
feature, not a clean fallback. That is precisely why it is gated behind a
measured 1.5x threshold and why Below threshold the filter is never built
(mispriced contingency, closed): the zero-coupling superset is strongly
preferred, and the coupling is only accepted if the measured inflation
forces it.

### Brick 2 - format, writer, reader, altw (pbfhogg, paired change)

Implements the contract section verbatim in the pbfhogg repository: field-5
parse/encode (a `parse_waymeta`-style opt-in beside the existing tagdata
flag), field-20 Way accessor, `Blob::way_members()`, altw relation
pre-scan + shared-bit join, header feature strings, and a roundtrip test:
altw a fixture, read back bitmaps and pins, compare against an in-test
oracle that recomputes both from the fixture's raw elements. Gated by
pbfhogg's own suite in that repo. elivagar's tree is untouched; the
elivagar boundary gate is trivially green.

### Brick 3 - re-enrich the gate datasets (data, no code)

Run the new altw over the `indexed` variants to produce new locations
files (new filenames beside the old ones - the pre-change locations files
are pre-existing state and are left in place; the user retires them):
denmark, germany, norway on plantasjen. Register each as the `locations`
variant in `brokkr.toml` with hashes from `brokkr env`. north-america is
re-enriched in Brick 6 (it is only needed for the slope check).

Gates (per dataset, old binary, new file): the CURRENT elivagar must run
the new files unchanged - the injection is additive and the consumption
has not landed yet. Exact commands:
```
brokkr tilegen --dataset denmark --variant locations
brokkr verify pmtiles --dataset denmark
```
plus `brokkr tilegen --bench 3 --dataset denmark` and `brokkr regress`
(raw variant standing gate, zero-diff - proves the toml/dataset touch
changed nothing on the measured path).

On-disk-growth reading (the honesty-clause gate for the added cost that
BOTH consumers pay). Record, per dataset, the byte size of the new
enriched locations file versus the pre-change locations file it replaces;
report the delta as an absolute size and a percentage. This is the
shared-file growth from field 5 (every way blob) plus field 20 (every
pinned way) - the cost nidhogg ingest and every other reader now carries.
It is a reading for the record (H10), not a pass/fail, but a delta far
above the bitmap-size expectation (field 5 ~1 KB/blob + field 20
~ceil(refs/8) on pinned ways) flags an encoding problem. Carry the germany
and north-america figures (Brick 6) into `reference/performance.md`.

### Brick 4 - consume the membership bitmap (elivagar, H2a landing)

`WayBlock` plumbing, `MemberSource`, detection, conditional prepass spawn,
`WayPlan.is_member`, counters - the full H2a target-artifact list above,
as one coherent change. Both paths (injected, fallback) live; no
env-vars, no experiment switches; keep or revert on the gates.

Gates, in order:
```
brokkr check
brokkr tilegen --bench 3 --dataset denmark          # standing gate (raw)
brokkr regress                                      # zero-diff (raw path untouched)
brokkr tilegen --dataset denmark --variant locations
brokkr verify pmtiles --dataset denmark             # zero errors
cd scripts/validate
node earcut-oracle.mjs ../../target/<denmark locations output>.pmtiles
                                                    # 0 deviant, 0 misattached
                                                    # (path as printed by brokkr tilegen)
```
Output-neutrality on the injected path (H2a must not change tiles - a
superset `is_member` only widens way_index): compare the locations output
against the same-dataset locations output built at the Brick 3 commit:
```
brokkr compare-tiles target/<brick3 output>.pmtiles target/<brick4 output>.pmtiles
```
zero vertex-command differences, and `features` / `unique_tiles` /
`output_bytes` sidecar counters equal between the two runs' `--bench`
UUIDs.

Superset cost reading (the Brick 1 threshold, now measured end-to-end):
`way_index_data_bytes` on germany locations <= 2 x the Brick 3 baseline
(baseline order 126 MB). Above the bound -> revert and take the Brick 1
contingency filter.

Performance verdict (the H2a claim - prepass join stall deleted):
```
brokkr tilegen --bench 3 --dataset germany --variant locations
brokkr sidecar <uuid> --stalls --human    # prepass_join gone from the top
```
Baseline: fresh `--bench 3` at the Brick 3 commit on plantasjen (the
roadmap's `92803833` 77.8s / `11dc159` row is context, not the verdict
baseline). Expected win: up to ~6s of germany-locations wall (the measured
6.3s prepass minus overlap). Keep bound: any best-of-3 result not SLOWER
than baseline beyond noise keeps the brick even if the win is small - the
planet claim is the serial-minutes deletion and the ~1 GB planet
`needed_ways` stock, both structural. Record post-landing numbers in
`reference/performance.md` against the commit hash.

### Brick 5 - consume the pins, delete the global prepass (elivagar, H2b landing)

`PinSource`, positional mask consumption in `process_planned_way_into`,
`build_way_plans` counting skip on the injected path, plus the full
teardown list above, as one coherent change.

This landing is geometry-changing ON THE INJECTED PATH ONLY (exact global
pins are a strict superset of block-local pins: more vertices survive DP
at simplified zooms). The raw path and non-enriched locations files are
bit-identical.

Gates, in order:
```
brokkr check                                        # incl. block-local pin tests
brokkr tilegen --bench 3 --dataset denmark          # standing gate (raw)
brokkr regress                                      # zero-diff (raw path proof)
brokkr tilegen --dataset denmark --variant locations
brokkr verify pmtiles --dataset denmark             # zero errors
cd scripts/validate
node earcut-oracle.mjs ../../target/<denmark locations output>.pmtiles
                                                    # 0 deviant, 0 misattached
brokkr compare-tiles target/<brick4 output>.pmtiles target/<brick5 output>.pmtiles
```
compare-tiles is a reading, not a pass/fail: expect small per-layer vertex
INCREASES concentrated in streets/water/boundaries at z <= 11; any vertex
DECREASE or layer disappearance is a bug.

Human gate (the one that needs eyes) - junction integrity at simplified
zooms, denmark locations output (use the `brokkr svg` wrapper, matching the
rest of these gates, not the raw binary):
```
brokkr svg --file target/<output>.pmtiles -z 10 -x 547 -y 323 -W 2 -H 2 -l streets -o notes/qa-cph-streets.svg
brokkr svg --file target/<output>.pmtiles -z 10 -x 544 -y 316 -l streets,water_polygons -o notes/qa-tisso.svg
```
(z10/547/323 = Copenhagen road network; z10/544/316 = Tissoe, the known
inland water tile.) Correct looks like: connected road junctions with no
new gaps or spikes versus the same tiles rendered from the Brick 4 output;
water outlines unchanged in character, no missing rings.

Evidence limitation, stated honestly. This human gate is three hand-picked
tiles, and the automated gates do NOT cover the property pins exist for:
the earcut oracle validates tessellation fidelity (not junction-gap
correctness), and compare-tiles only confirms that vertices INCREASED (not
that the RIGHT vertices - the cross-block junction vertices - were
retained). So the planet-scale "exact global pins fix cross-block gaps"
claim rests on spot-checks plus the algorithmic argument that exact global
pins are a strict superset of block-local pins. That is thin for a planet
claim; the honest record says so. A stronger check, if the spot-checks
raise doubt, is a targeted diff of pin-mask coverage at KNOWN cross-block
boundaries (tiles straddling a PBF primitive-block seam, where block-local
pinning provably drops junctions and injected pinning must retain them) -
left as the escalation if the eyeball gate is inconclusive, not a standing
requirement.

Performance verdict:
```
brokkr tilegen --bench 3 --dataset germany --variant locations
```
Two opposing terms: plan-build counting removed (win) vs more retained
vertices through simplify/encode (cost). Keep bounds, read against the
Brick 4 commit baseline: best-of-3 wall regression <= 2%, and
`output_bytes` + summed `sort_layer_<name>_bytes` growth <= 3% on germany
locations. Outside either bound -> revert and record the finding in the
roadmap (the pins would then need a zoom cap, which would be a new spec).
Record post-landing numbers in `reference/performance.md`.

### Brick 6 - the planet-slope reading (measurement, no code)

Re-enrich north-america locations with the new altw, register in
`brokkr.toml`, then:
```
brokkr tilegen --bench --dataset north-america --variant locations
brokkr sidecar <uuid> --human
brokkr sidecar <uuid> --stalls --human
brokkr sidecar <uuid> --counters
```
Readings that feed the roadmap's planet ledger (H3): phase12 s/GB with the
prepass deleted (baseline 8.2 s/GB at NA `b66fcc6e` / `69c0f18`),
`way_index_data_bytes` under superset membership at NA scale,
`way_members_marked` vs the old `relation_plan_needed_ways`, peak RSS with
the `needed_ways` stock gone. Write the row into
`notes/planet-30gb-roadmap.md` (H2 section) and
`reference/performance.md`.

## Ordering summary

1 (instrument, elivagar) -> 2 (pbfhogg) -> 3 (datasets) -> 4 (H2a,
elivagar) -> 5 (H2b + teardown, elivagar) -> 6 (NA reading). Bricks 4 and
5 are separable landings with independent keep/revert verdicts; 5 depends
on 4 only through the shared `WayBlock` plumbing. Every boundary leaves
both repos' suites green and the standing denmark gate zero-diff.

## Stopping rule

- H2c (shortbread relevance masks) and H2d (partition calibration stats)
  are separate roadmap items; nothing here designs for them beyond leaving
  BlobHeader field numbering room (they would be fields 6+).
- The runtime `prepass_relation_plan` and the block-local pin path are NOT
  deleted: raw Geofabrik input remains first-class. Requiring enriched
  input (and deleting the fallbacks) is a product decision that belongs to
  the H10 record-framing work, not this spec.
- The relation tail, ocean, sort, and assemble phases are untouched.
- pbfhogg internals beyond the contract section (pass structure, radix
  join details, passthrough handling) are the paired pbfhogg spec's
  ground.
- No blessing: the blessed denmark archive is raw-variant and stays valid
  throughout; nothing here rotates the regress baseline.

## Review resolutions

Two step-2 reviews (R1 Opus, elivagar source-checked; R2 codex gpt-5.5
xhigh, elivagar + pbfhogg source-checked) were folded into the sections
above. Findings validated and folded, by section:
- WayPlan.is_member doc contradiction -> Target artifacts (fills at plan
  build for both paths; fallback lookup moves into `build_way_plans`).
- Dual-consumer / on-disk cost never priced -> Honesty clause (single
  shared enriched file; nidhogg ingest skips field 20 as unknown) + Brick 3
  on-disk-growth reading.
- Coords/ref alignment invariant under-guarded -> field-20 alignment
  contract + `process_planned_way_into` bullet (release-checked
  `coords_e7.len() == refs.len()`).
- `debug_assert` vs "fail loudly" -> Header feature flags (all length /
  alignment / missing-data checks promoted to release-checked hard errors).
- Field 5 skipped by the reader / enablement unpinned -> pbfhogg public API
  (opt-in `parse_waymembers` toggle keyed to the feature flag).
- Enforcement point for field-5-missing unpinned -> data-flow bullet (fires
  in the decode worker at the `way_members()` call).
- 8000 ways/blob not an invariant -> survey consequence + field-5 size
  policy (sized against altw's actual density; write-time header-length
  assert; 228k density ~28 KB under budget).
- Authoritative flags vs raw-path ignore -> Header feature flags (a
  WayMembers/SharedNodePins flag without LocationsOnWays is a hard error,
  not a silent ignore).
- Superset union left implicit / 1.5x vs 2x thresholds not derived / Brick
  1 contingency reintroduces coupling -> Brick 1 (union spelled out; count
  ratio flagged as an early screen with Brick 4 bytes governing; the
  `tag_expr` fallback named as a durable cross-repo coupling, not free).
- Quality claim verified only by spot-check -> Brick 5 evidence-limitation
  paragraph.
- MemberSource resolved at first way block -> MemberSource doc (resolved at
  header detection).
- `elivagar svg` in gates -> Brick 5 human gate uses the `brokkr svg`
  wrapper, matching the other gates.

Findings NOT folded:
- R2 "Brick 2 delegates too much of the writer design (OwnedBlock type,
  framing plumbing)". Rejected: the spec deliberately and explicitly
  designates the pbfhogg implementation a NAMED EXCLUSION (see "The pbfhogg
  implementation is specified ... in a paired document"), while pinning the
  format, semantics, feature strings, and public API surface here. Writer
  internals (`OwnedBlock` shape, `framing.rs` plumbing) are legitimately
  pbfhogg's ground; that is a scoping decision, not a hole. The load-bearing
  interface concerns the finding raised - the concrete field-5 reader
  enablement - ARE folded on the elivagar side (pbfhogg public API bullet).
- R2 "earcut gate not copy-pasteable (pnpm)". Rejected in part: `scripts/
  validate` has no npm-script wrapper (package.json lists only deps, and
  node_modules is committed/installed), so `cd scripts/validate; node
  earcut-oracle.mjs <file>` is exactly how the oracle runs. Only the `svg`
  half of that finding was a real inconsistency and was folded.
