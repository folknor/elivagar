# Hunt: layer identity and per-layer policy

Scope: what a layer is (26 Shortbread layers incl. ocean), its name, id, zoom
range, geometry kind, paint order, attribute schema, and every per-layer policy
keyed off it. Design reconnaissance; no code was changed.

## Summary

`shortbread::Layer` is a real enum with `name()`, `min_zoom()`, `ALL` and
`from_name()`, which is a good start. Nearly everything else about a layer
is decided somewhere else: by an `==` against a variant in emit code, by a
string `match` on the layer name in verify, by array position in the sort key
and stats arrays, and by implicit vocabularies (KEY_NAMES, KIND_VALUES,
is_known_*_kind, corpus/style.toml) kept in step by hand. The enum
discriminant alone decides five separate things (wire id in the sort key,
order inside a tile, MVT layer order, vector_layers order, stats index). The
fix is a single static layer descriptor table, with typed attribute keys and
kinds, that every policy site reads. Details follow.

## 1. Axes that should be types

### 1.1 Layer id travels as a bare `u8` everywhere past phase12
- `sort::make_sort_key(tile_id, layer: u8, priority: u8)` takes two adjacent
  `u8`s of different meaning. Swapping them compiles, and the result sorts
  wrongly without any error.
- `sort::layer_from_key -> u8`, `PendingTile.features: Vec<(u8, Box<[u8]>)>`,
  `ocean_sink(.., layer_idx: u8, ..)`, and `Layer::Ocean as u8` in ocean.rs.
- Assembly guards with `if (layer_idx as usize) < s.layers.len()` and **silently
  drops** a feature that has an out-of-range id (encode_tile_batch_mvt and
  prepare_non_empty_layers in assemble.rs). A corrupt or unknown id should be a
  typed decode failure, not a missing feature.
- The `Layer` doc comment says "No `from_index()` needed - layer indices are
  only used as array offsets". That is exactly why the drop is silent. Add
  `Layer::try_from(u8)` and carry `Layer` in `PendingTile` and the decoded key,
  so the u8 exists only inside the key's bit packing.
- Paint priority is a bare `u8` (`LayerMatch.paint_rank`, the key's low byte).
  It should be a `PaintRank` newtype that only paint_order.rs can mint.

### 1.2 Per-layer arrays sized by magic numbers, not by `Layer`
- `RecordTally` (emit.rs), the sort stats in sort.rs, and `FanoutStats`
  (stats.rs) use `[u64; 32]` and `[u64; 32 * 15]`. The 32 is "u8 layers fit
  in 32" and the 15 is "z0..=14". Indexing is `layer as usize * 15 + z`, done by
  hand (emit.rs `cap_events` packs this into a `u16`). pipeline/mod.rs then
  re-guards with `i < Layer::ALL.len()`.
- `TilegenConfig.fanout_caps: [u32; Layer::count()]` does use the count, but
  it is still positional: main.rs writes `caps[layer as usize]` and
  relations.rs/phase12.rs read `fanout_caps.get(m.layer as usize).unwrap_or(0)`.
  The `unwrap_or(0)` hides an impossible case.
- Use an `EnumMap<Layer, T>`-style `LayerMap<T>` plus a `Zoom` newtype (0..=14)
  with `PerLayerZoom<T>`. That removes the 32 and the 15, the manual index
  arithmetic, and the u16 packing.

### 1.3 Attribute keys and kinds are strings validated at runtime
- `Attr = (&'static str, AttrValue, u8)`: the key is a string and the third
  slot is a min zoom typed as bare `u8`. The comment defends the tuple on
  grounds of construction-site count. That is not the real cost. The real cost
  is that `wire_format::key_to_id` does a linear string search per attribute
  per zoom on the hot path, and **panics** on an unknown key. The attribute
  schema exists only as KEY_NAMES order.
- An `AttrKey` enum (`#[repr(u8)]`, the wire id *is* the discriminant) makes an
  unknown key impossible to construct and the lookup free.
- `kind` values: the same idea applies to a `Kind` enum per layer family.
  Today `KIND_VALUES` interns kind strings for the wire, and a kind missing
  from it silently falls back to an inline string. Nothing ties KIND_VALUES to
  what the matchers emit (see 2.3).

### 1.4 Geometry kind is not a property of the layer
- `GeomExpect` lives on each `LayerMatch`, not on `Layer`. So "is `land` a
  polygon layer" has no answer. Land emits polygons and also `cliff` lines
  (land.rs). `water_polygons_labels`, `boundary_labels` and
  `streets_polygons_labels` are point layers fed from polygons. Nothing states
  that a given layer only ever carries certain geometry kinds.
- relations.rs: `GeomExpect::Line` "falls through to a no-op". A multipolygon
  match that expects a line is silently discarded. A per-layer allowed-geometry
  set would make that impossible or a loud error.
- The oracles (earcut "every polygon layer", ring-cap `all`) discover polygon
  layers from data because the crate cannot tell them.

### 1.5 Zoom range: three different "min zoom" concepts, all `u8`
- `Layer::min_zoom()` is a hand-written lower bound over every matcher's
  per-match min zoom. Its only consumer is pmtiles_writer `build_metadata`
  (vector_layers `minzoom`).
- `LayerMatch.min_zoom` / `max_zoom` is the per-feature range. `max_zoom` is the
  literal `14` at every construction site (about 40 of them) and never varies.
  It is a redundant field.
- The per-attribute min zoom (third tuple slot) is a third concept.
- Nothing checks that `Layer::min_zoom()` <= every matcher-emitted min zoom.
  They agree today (I spot-checked land 7, streets 5, place_labels 4,
  boundary_labels 2 via the area override, water_polygons 4, water_lines 9,
  public_transport 11). Lowering a matcher zoom would let features exist
  below their declared vector_layers minzoom, and no test catches it.
- `build_metadata` writes `minzoom = layer.min_zoom()` without clamping to
  `config.min_zoom`, and `maxzoom = config.max_zoom`. With `--min-zoom 10`, the
  metadata advertises boundaries from z0.

### 1.6 Failures as prose
- verify's layer problems (undeclared, unseen, non-Shortbread, paint-order
  violations) exist only as `println!` / `format!` strings
  (`"[{layer_name}] feat {i}: paint rank {rank} follows {previous}"`). brokkr,
  which links the crate, cannot branch on "paint order violated in layer X at
  z". These should be a `VerifyFinding` enum carrying `Layer`, zoom and kind.
- main.rs `--fanout-cap` parsing exits with prose on an unknown layer name.
  That is fine at the CLI edge, but the parse belongs in a
  `FromStr for Layer` / `LayerCapSpec` in the library, so the library and
  brokkr spell layer names once.

## 2. Decisions made in more than one place

### 2.1 Merge policy per layer: MVT and MLT already disagree
Question: which layers get same-attribute merging and connected-line
merging, at which zooms?
- `encode_tile_batch_mvt` (assemble.rs): skips `merge_same_attr_geometries`
  for `Layer::Ocean` by raw index (`li == Layer::Ocean as usize`), and runs
  `merge_connected_lines` on every layer when `z < 14`.
- `prepare_non_empty_layers` (MLT path): merges **every** layer, ocean
  included, and **never** runs `merge_connected_lines`.
- **They disagree today.** MLT output differs from MVT in feature grouping for
  ocean, and in line topology for boundaries and streets at z0-13. The
  boundary-line oracle's guarantees are therefore MVT-only.
- The ocean exclusion has no rationale anywhere (it dates to the
  "Cleanup March 8" commit 7fdeb0a). The reason is probably the ring-cap
  partition / artifact piece identity, but that is a guess.
- Owner: a `LayerPolicy { merge_same_attr: bool, merge_lines: ZoomRange }` in
  the layer descriptor table, read by a single format-agnostic
  `prepare_layers(tile) -> Vec<&LayerBuilder>` that both encoders call. The
  MVT closure also duplicates the MLT helper's body, so this is a structural
  fix as much as a policy one.

### 2.2 "Connectivity-sensitive line layer" (Boundaries, Streets)
- emit.rs `emit_line_feature` decides it twice in the same function:
  `skip_size_filter = z >= 14 || layer == Boundaries || layer == Streets` and
  `skip_bbox_check = layer == Boundaries || layer == Streets`. The two sites
  agree today.
- The polygon analogue is a separate list: `polygon_min_area` exempts
  `StreetPolygons` and `Bridges`. But `polygon_z_start`, which drops a polygon
  until its bbox stops being subpixel, does **not** exempt them. So "never drop
  small bridges" holds only partly: a tiny bridge polygon is kept by min_area
  but can still have its first emitted zoom delayed by the bbox rule. Whether
  that is intended is not recorded.
- Owner: descriptor fields `keep_subpixel: bool` (lines) and
  `min_area: MinAreaPolicy` (polygons).

### 2.3 Kind vocabularies: five copies
Question: what `kind` values can each layer carry?
- The matchers (land.rs `land_match`, streets.rs `highway_zoom`, and the
  others) are the actual producers.
- `paint_order::is_known_land_kind` / `is_known_street_kind`.
- `paint_order::land_paint_rank` / `street_class_rank` (fallbacks `_ => 0` and
  `UNKNOWN_STREET_CLASS`).
- `wire_format::KIND_VALUES` (interning table; includes `motorway_link`
  etc. which, if streets.rs strips `_link` into the `link` attr, are dead
  entries).
- `corpus/style.toml`, whose comment says "every kind is_known_land_kind()
  accepts has a match below". That is a hand-held agreement across repos.
- paint_order.rs tests assert agreement for the fixtures they list. That is
  the "pairwise-agreement test" pattern: it catches only the drift it happens
  to exercise. A new kind added to `land_match` gets paint rank 0 silently and
  emits fine. verify then rejects it as "unknown land kind", but only on an
  archive that contains one.
- Owner: one `LandKind` / `StreetKind` enum per family, carrying
  `(name, min_zoom, paint_rank)` in a single table. The matcher returns the
  enum, KIND_VALUES derives from it, `is_known_*` becomes `from_str`, and
  brokkr's style test can enumerate it.

### 2.4 Which layers have a paint order, and how rank is recovered
- Producers: land.rs (land_paint_rank, including `"cliff"`) and streets.rs
  (street_paint_rank for streets and street_polygons). Every other matcher
  passes `paint_rank: 0`.
- verify.rs `validate_paint_order` re-decides the set by name:
  `matches!(layer, "land" | "streets" | "street_polygons")`.
- verify also re-decides *how* rank is derived at low zoom: `"streets" if
  z < 11 => street_class_rank`. That threshold exists only because the
  `link`/`tunnel`/`bridge` attrs carry min zoom 11 in streets.rs. These are
  two literal 11s kept in step. If someone moves `tunnel` to z10, verify still
  uses the class-only rank at z10, and the check weakens or false-fires
  depending on the direction.
- Owner: descriptor `paint_order: Option<PaintOrderFn>`. The rank recovery for
  verify should come from the same function as emission, applied to the
  attributes actually present at z, rather than a separately chosen zoom.

### 2.5 Boundary match built twice
- shortbread/boundaries.rs `match_boundaries_line` builds the Boundaries
  `LayerMatch`: attrs `admin_level`, `maritime`, `disputed`, with maritime =
  `maritime=yes || natural=coastline`.
- pipeline/phase12.rs `resolve_boundary_match` rebuilds the same match from
  relation metadata, with the **maritime expression copied verbatim** and its
  own `max_zoom: 14, paint_rank: 0, GeomExpect::Line`. It overwrites
  `existing.min_zoom` and `existing.attrs` if the matcher already produced one.
- The two agree today. A schema change to the boundaries layer (adding an
  attr) must be made in both, and phase12 would silently win.
- Owner: boundaries.rs should expose `boundary_line_match(tags, level,
  disputed_override)` and phase12 should only resolve the level.

### 2.6 Area-dependent min zoom of boundary_labels
- boundaries.rs sets `min_zoom: 5` with a comment that the overrides happen
  "later in tilegen".
- emit.rs `enrich_polygon_matches` applies the z2/z3/z4 thresholds, and
  re-extracts `admin_level` from the attrs vector by string key.
- `Layer::BoundaryLabels.min_zoom() = 2` encodes the outcome of that rule a
  third time.
- Owner: shortbread, as a function of (tags, area). The schema module should
  own every zoom decision. Pipeline code should pass geometry facts in, not
  edit `LayerMatch` fields after the fact.

### 2.7 Ocean polygon policy versus OSM polygon policy
- OSM: emit.rs `polygon_dp_tol` (OSM_DP_TOL_PX * factor, 0 at z14),
  `polygon_min_area` (MIN_POLY_AREA = 256, 0 at z14 and for exemptions),
  `Simplifier::DouglasPeucker`, `maxz = OSM_POLYGON_MAX_Z = 14`.
- Ocean: ocean.rs `ocean_dp_tol` (OCEAN_DP_TOL_PX), `ocean_min_area` (the
  **literal 256**, not MIN_POLY_AREA), `Simplifier::Visvalingam`, and
  `polygon_simplify_factor` deliberately not applied.
- These are one question: what is the simplification policy of layer L at
  zoom z? It is answered in two modules with two constant sets, and only the
  ocean side is versioned (OCEAN_POLICY_VERSION). A descriptor-level
  `PolygonPolicy` would make the ocean one the hashed, versioned instance of a
  general type.

### 2.8 "Ocean is the last layer in a tile"
- The enum puts `Ocean = 25` last, so locally assembled tiles emit ocean last
  (assembly iterates layers by index).
- assemble.rs `splice_ocean_layer` appends the artifact's ocean field at the
  end of the raw tile.
- Band tiles (computed ocean) and interior tiles (spliced ocean) agree on
  layer order only because both happen to put it last. Reordering the enum
  would make artifact-served tiles differ in layer order from computed ones.
  It would also break the pmtiles_writer test that pins `layers[25] == "ocean"`.
- Owner: the descriptor table's explicit `tile_order`, with the splice
  inserting at the declared position, or an assertion that ocean is maximal.

### 2.9 Ocean identity by string
- Ocean is recognised by `"ocean"` string literals in assemble.rs
  (`raw_contains_ocean`, `ocean_layer_field`), ocean.rs (artifact
  vector_layers check), and verify.rs (twice, for geometry stats and ring
  checks). Elsewhere it is `Layer::Ocean`.
- They should all be `Layer::Ocean.name()`. They agree today, but the
  crate has no single spelling.

### 2.10 Fanout caps: accepted for layers where they do nothing
- `--fanout-cap layer=N` accepts every layer name (main.rs) and records it in
  the producer contract (provenance.rs `layer_map`). As a result it can force
  a resume refusal or a corpus contract mismatch.
- Caps are honoured only on the OSM polygon path (phase12 `GeomExpect::Polygon`
  arm, relations.rs). A cap on `streets`, `pois`, or **`ocean`** is a no-op.
  pipeline_tests even sets `fanout_caps[Layer::Ocean]` to prove that it changes
  the producer contract.
- Two sites decide "does a fanout cap apply to this layer": the CLI/provenance
  side says every layer, the emitter says polygon matches only. **They
  disagree today.** Owner: the descriptor (`fanout_cap_applies`), with the CLI
  rejecting a cap for a layer that cannot honour it.

### 2.11 Zoom clamp and geometry dispatch repeated
- `z_lo = m.min_zoom.max(min_zoom); z_hi = m.max_zoom.min(max_zoom)` followed by
  a `match m.geom_expect` dispatch appears in phase12 node emission, phase12
  way emission and relations.rs, each with its own handling of the
  antimeridian and of unsupported kinds.
- One `emit_match(match, geometry_source, sink)` would own both decisions.

## 3. Structure

- **The real seam is a schema/profile, and it has no home.** `shortbread/`
  holds matching (tags to LayerMatch), but layer facts and policies leak into
  pipeline/emit.rs (min-area exemptions, subpixel exemptions, area-based zoom
  overrides), pipeline/phase12.rs (boundary match construction),
  pipeline/assemble.rs (merge policy), wire_format.rs (attribute and kind
  vocabularies), verify.rs (paint-order layer set), pmtiles_writer.rs
  (vector_layers), and ocean.rs (ocean policy). The recommendation is a
  `profile` module owning a `static LAYERS: [LayerSpec; 26]`, where `LayerSpec`
  holds: name, tile order, zoom range, allowed geometry kinds, attribute
  schema (typed keys with value types and per-key min zoom), kind enum, paint
  order fn, polygon/line policy, merge policy, and whether fanout caps apply.
  Matchers, emitters, assembler, writer, verify and provenance would all
  read it. That is a rewrite of the seams, not the algorithms, and it is
  worth doing.
- **pois.rs at crate root** is a shortbread matcher (`Layer::Pois`) that lives
  outside `shortbread/` for no structural reason (the mod.rs header has to
  explain it). Move it in.
- **paint_order.rs is consumed by verify** as an oracle for itself. Verify
  re-deriving rank from the same function is a deliberate re-check of one
  answer, which is fine. Verify choosing its own layer list and zoom threshold
  is not (2.4).
- **wire_format.rs owns the schema vocabulary.** KEY_NAMES and KIND_VALUES are
  Shortbread schema facts sitting in a serialization module. The wire format
  should consume `AttrKey as u8` / `Kind as u8` from the profile and know
  nothing about names.
- **The vector_layers metadata is incomplete.** TileJSON `vector_layers`
  entries should carry `fields` (attribute name to type). The writer cannot
  produce them because no attribute schema exists in code. A typed profile
  would give it for free, and verify could then check that observed attributes
  match the declared schema. Today verify checks only layer names.
- **The encode paths fork** (MVT inline closure vs MLT `prepare_non_empty_layers`).
  This duplication of structure is what produced the divergence in 2.1.
  Collapse it into one layer-prep stage followed by a format encoder.
- **Stats live in three parallel structures** (RecordTally, sort stats,
  FanoutStats), each with its own `32 * 15` arrays and merge loops. A single
  `PerLayerZoom<T>` type and one stats module would replace them.
- **Tests mirror files, not the design.** shortbread_tests.rs is spliced into
  shortbread/mod.rs through `#[path]`, and pipeline_tests.rs carries layer
  assertions (`Layer::Ocean` fanout). No test iterates `Layer::ALL` against a
  descriptor invariant (minzoom lower bound, kind coverage, geometry kinds).
  With a descriptor table, these become one property test per invariant.

## Lateral findings

- **SVG colors are unstable per tile** (svg.rs): a layer's color is
  `LAYER_COLORS[first-appearance index]`, so the same layer gets different
  colors in different renders or grids. There are 22 colors for 26 layers, so
  colors also repeat. Paint order is first-appearance order. Because ocean is
  last in each tile, ocean paints over land at opacity 0.8. Key color and order
  by `Layer` instead.
- **svg `-l` filter** never validates layer names. A typo renders nothing,
  with no error.
- **Stale wire_format header**: it documents `u16 geometry command count`, but
  the code writes a `u32`. KEY_NAMES marks `way_area` and `height` as
  "test-only", but `way_area` is emitted in production by
  enrich_polygon_matches and `height` by land.rs (buildings).
- **max_zoom below 14**: `OSM_POLYGON_MAX_Z` and literal `z >= 14` / `z < 14`
  checks (emit.rs lines, assemble.rs `merge_connected_lines`) assume z14 is
  the lossless top. With `--max-zoom 12`, the top zoom is simplified and
  min-area filtered, and lines at z12 get the size filter. That is probably
  harmless, but no decision covers it.
- **`LayerMatch.max_zoom`** is always 14 and could be removed (1.5).
- The `const _: () = assert!(size_of::<LayerMatch>() == 408)` pins a 408-byte
  match struct whose inline `SmallVec<[Attr; 8]>` of string-keyed tuples is
  most of the size. Typed `AttrKey` would shrink it considerably.
