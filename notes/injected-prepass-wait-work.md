# TODO: injected-prepass wait-work (elivagar side)

Work that is landable now, while pbfhogg builds the injected-prepass
producer (their landings 2-5) and before the Brick 3 re-enriched datasets
exist. Derived from `notes/injected-prepass-spec.md` (the normative
contract; Brick 1 landed and both cross-repo gates were ratified
2026-07-11, see its "Cross-repo ratifications" section and the Brick 1
measured verdict). Both items must be behavior-neutral on every input
that exists today: no current file carries `pbfhogg.WayMembers-v1` or
`pbfhogg.SharedNodePins-v1`, so the injected path is dormant by
construction.

Standing gates for each item: `brokkr check`; denmark raw bench +
`brokkr regress` zero-diff; locations-path neutrality shown with
`brokkr compare-tiles` against the commit-`4ceacd1` outputs
(`data/tilegen/denmark-4ceacd1.pmtiles`, `data/tilegen/germany-4ceacd1.pmtiles`).

## Item B: behavior-neutral Brick 4 consumption plumbing

The subset of Brick 4 (`notes/injected-prepass-spec.md`, Target
artifacts + data-flow bullets) that is inert until an enriched file
exists, landed now so the eventual Brick 4 activation is a small diff:

- Header detection beside the `LocationsOnWays` sniff: `injected_members`
  / `injected_pins` from the two feature strings
  (`HeaderBlock::has_way_members_v1()` / `has_shared_node_pins_v1()`,
  already public in the pbfhogg path dependency), honored only with
  `locations_on_ways`; a flagged file WITHOUT `LocationsOnWays` is a
  hard error (malformed enrichment, per the contract's flag/mode rules).
- `MemberSource` resolved at header detection (Injected vs
  Plan(RelationPlan)); `prepass_relation_plan` spawned only when
  `!injected_members`.
- `WayBlock { block, members: Option<Box<[u8]>> }` plumbing through the
  block routing to the way worker; decode workers call
  `blob.way_members()` (with `set_parse_waymembers` keyed to the header
  flag) and hard-error on a missing field 5 on a way blob under
  `injected_members`; the raw ordered path wraps `members: None`.
- `build_way_plans` takes `MembersForBlock::{Bitmap(&[u8]),
  Set(&FxHashSet<i64>)}` and fills `WayPlan.is_member` from either
  (positional bit i = way element i on the Bitmap arm).
- Release-checked validations per the contract: field-5 bitmap length vs
  the blob's way count, encoded `way_member_count()` vs the actual
  decoded Way element count (the ratified count compare), and the
  flags-without-LocationsOnWays error above.
- Unit tests drive the Bitmap arm and the validations with hand-built
  bitmap bytes - no enriched file needed. PinSource / field-20
  consumption and all teardown are OUT of scope (that is Brick 5, whole,
  after enriched data exists).

On every input that exists today the header carries neither feature
string, so `MemberSource::Plan` is always selected and output is
byte-identical; the compare-tiles gate proves it.
