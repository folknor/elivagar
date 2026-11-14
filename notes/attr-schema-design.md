# Attribute schema and finished matches - chain design

Covers the consolidation entry "What attribute keys exist, and what type is
each?" and the structural entry "`LayerMatch` is a wide, mutable,
post-processed record". Settled in a codex spar on 2026-11-14. This is a
chain: link 1 lands the global key schema and the finished-match barrier,
link 2 lands per-layer field declarations, link 3 is the green agent.

## Settled rulings

1. Keys are one closed schema owned by `shortbread`, declared by one macro in
   typed groups (kind, str, int, bool, float). It generates `StrKey`,
   `IntKey`, `BoolKey`, `FloatKey` sharing one id space and an umbrella
   `AttrKey` with `name()`, `value_type()`, `from_id()`, `from_name()`.
   Discriminant = wire id, identical to the old `KEY_NAMES` order, so sort
   record bytes do not change and `CHECKPOINT_VERSION` does not move. Const
   asserts: ids dense, fit u8, names distinct.
2. `Attr` is a struct with private fields `{ key, value, from_zoom }`, built
   only through typed constructors (`attr_str(StrKey, ..)`,
   `attr_int(IntKey, ..)`, `attr_bool`, `attr_float`, `attr_kind(impl
   WireKind)`, `.from_zoom(z)`). Wrong-typed values and string-built kinds are
   type errors. Attribute list order is construction order, never group order.
3. `AttrValue::Kind(KindId)` carries the global `KIND_VALUES` id, computed at
   const time from the variant's wire name (the family discriminant is a
   family-local index, not the dictionary id). `KindId` is not constructible
   outside the macro. Its output type is String everywhere (MVT, MLT,
   TileJSON). The encoder's raw-kind fallback goes; the decoder keeps
   accepting raw kind strings (type 0) and interned (type 4).
4. The sort-record decoder rejects a value type byte that disagrees with the
   key's declared type, right after the key id check, as part of record
   layout. It guarantees "kind is a string", not "kind is in the profile
   vocabulary"; those stay distinct invariants.
5. MLT: `Mixed` is deleted. Column type comes from the schema; an unknown key,
   invalid tag indices or a value of the wrong type is an `MltEncodeError`,
   never a silent `None`. Tests that used arbitrary keys are rewritten to
   schema keys; the schema is not grown to keep them passing.
6. Matching returns `Hit`s: `{ matched, area_rule: Option<AreaRule> }` with no
   accessor exposing the provisional `LayerMatch` for emission. Finishing
   consumes the hit list with one lazily computed area per element (ways: the
   unwrapped projected ring; relations: outers minus inners, unchanged
   arithmetic), before zoom filtering, appending `way_area` where it is today.
   Emission takes only `LayerMatch`. `PreparedRelation` stores hits.
7. `match_element` takes the relation `BoundaryMeta` (None for nodes and
   relations); the Boundaries arm calls `boundary_line(tags, meta)` and
   `resolve_boundary_match` goes. Both early-return guards in ways stay
   (membership, presence in `boundary_way_meta`).
8. The ways prefilter caches its per-geometry results (uncomputed or computed)
   and selects by the resolved-coordinate closure test, running the matcher
   only for an uncomputed type. It never selects the probe that admitted the
   way merely because it admitted it.
9. `LayerMatch.max_zoom` is dropped; emission uses `tiling::MAX_ZOOM`.
10. Refused: a general `Zoom` newtype (standing decision in `src/tiling.rs`),
    a `PaintRank` newtype (no invariant beyond u8), a classify step separate
    from attribute building (splits every matcher for a perf-only payoff; the
    duplicate prefilter call is removed instead), and avoiding the
    `attr_dyn` allocation (owned strings exist because relation matches
    outlive the PBF tag borrow). In their place, `cfg(test)` assertions in
    `match_element`: ordered layers carry exactly `Layer::paint_rank_for` of
    their own attributes, unordered layers rank 0, every `from_zoom <= 14`.
11. `LayerMatch`, `Hit`, `Attr`, `AttrValue`, `match_element`, `Tags`,
    `GeomExpect`, `OsmGeomType` become crate-private: public by accident, no
    consumer. `Layer` and the kind enums stay public. CHANGELOG entry and a
    brokkr#119 comment record it.

## Link plan

- Link 1 (this landing, large): rulings 1-11. Claims global key typing and
  the finished-match barrier, not per-layer schema enforcement.
- Link 2 (medium): per-layer allowed-key declarations in `define_layers!`
  (allowed, not required; `way_area` declared for boundary_labels even though
  provisional hits lack it), a `cfg(test)` assertion that emitted keys are
  declared, `archive::VectorLayer` gains `fields` (public surface: install.md,
  CHANGELOG, brokkr#119), TileJSON type vocabulary kept distinct from the
  schema's (Int and Float both `Number`). `verify` checks observed MVT keys
  and value types (distinguishing int from float) against the canonical
  declarations, and separately that the archive metadata's complete field map
  equals the canonical map. Metadata is outside the corpus digest, so the
  corpus gate cannot prove this link; verify on a fresh denmark build must.
- Link 3: green agent.
