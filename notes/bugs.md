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

## BUG-001 - MLT archives record a tile compression they do not use

`assemble.rs` maps `TilePayloadFormat::Mlt` to `TileDataCompression::None` and
ignores `config.tile_compression`. `provenance::config_json` writes
`tile_compression_name(config.tile_compression)` unconditionally, and clap
defaults that to gzip; `run()` also emits `tile_compression=0` (gzip) to the
sidecar. So every MLT archive's comparability contract says `gzip`, while its
header and top-level `tile_compression` metadata say `none` - one metadata
document claiming two compressions. Nothing rejects
`--tile-format mlt --tile-compression brotli` either (the help text "MVT only"
is the only enforcement), and `base_compression_level` is recorded for MLT
where it means nothing.

Root cause per the hunters: format and compression are two independent enums
while the legal space is `Mvt(Gzip|Brotli) | Mlt`. A `TileContract` /
`TileEncoding` enum of that shape makes the lie unconstructible (the wider
ownership question is in `notes/consolidations.md`, tile contract entry).

Reported by the pipeline, run-contract and archive-surface hunts.

## BUG-002 - Ocean pass structure disagrees with itself; a single z0-z14 shapefile plus an artifact hands the low pass a z14 grid

The question "which ocean passes exist, which shapefile feeds each, and which
grid owns each zoom" is answered at seven or eight sites:

- `main.rs::resolve_ocean_inputs` accepts only `z0-z14` or `z0-z7`+`z8-z14`.
- `pipeline::run` ocean phase always runs a low pass `min..=min(max,7)`
  (simplified shapefile if named, else full) plus a high pass `8..=max` - the
  OCEAN_POLICY_VERSION v3 "split always happens" rule.
- `ocean_build` hard-codes `0,7` and `8,14` with a "mirror the run pipeline"
  comment.
- `pipeline::ocean_pass_max_zooms` returns two grids only when a simplified
  shapefile is named, otherwise one grid at `max_zoom` - the pre-v3 rule.
- `ocean::selected_pass_grid` picks the first grid iff
  `grids.len() == 2 && z <= 7`.
- `ocean::LOW_ZOOM_UNION_MAX_ZOOM = 7` decides the union per pass by
  `max_zoom <= 7`.
- `assemble::write_provenance` derives `low_zoom_source` with a comment
  "mirrors ocean_pass_max_zooms".
- `OceanSourceKey::from_inputs` / `OceanArtifactKey::from_inputs` re-derive
  from `(full, Option<simplified>, min, max)`.

Live disagreement: with `--ocean z0-z14:full.shp` plus an artifact, the run
loop executes two passes but `ocean_pass_max_zooms` yields one z14 grid. The
low pass gets `grids().first()` - the z14 grid - so `pass_owns_root_range` and
`ocean_band_tile` snap at 2^26 resolution while the pass's pyramid works at
2^19, and `OceanPassGrid::for_bounds` ceil/floored the inner rect at the wrong
scale. Producer and consumer both use the same wrong grid, so they agree with
each other; neither agrees with the pass that produced the chunks. No hunter
proved a byte difference. Production always names the simplified shapefile,
so the corpus never exercises this arm.

Related consequences:

- `--ocean z0-z14:full.shp` and `--ocean z0-z7:full.shp --ocean z8-z14:full.shp`
  run identical work but record different `low_zoom_source` (`full` vs
  `simplified`) and different `OceanSourceKey`s, so they are
  contract-incomparable while byte-identical (ocean hunt).
- Stale comments describe the pre-v3 structure: `OceanInputs` in `main.rs`
  ("a single z0-z14 entry ... serves every zoom, matching
  `ocean::selected_pass_grid`"), the `LOW_ZOOM_UNION_MAX_ZOOM` comment ("A
  single z0-z14 full-shapefile pass keeps the old per-piece behavior"), and
  the `ocean_pass_max_zooms` comment.
- `selected_pass_grid` infers "a split exists" from a collection length, and
  producers pick grids positionally with `first()` / `last()`.

Proposed owner (all four hunts converge): one `OceanPlan` built once from the
validated `--ocean` input, holding
`passes: [OceanPass { zooms: ZoomRange, source, grid, union }]` and an optional
artifact, exposing `grid_for_zoom(z)` and `contract()`. The run loop,
`ocean_build`, `OceanTiles::open`, `selected_pass_grid`, the provenance
`low_zoom_source` and both keys read it; the z7/z8 split becomes one constant
only the plan sees. `resolve_ocean_inputs` moves into the library as
`OceanPlan::parse`.

Reported by the pipeline, run-contract, ocean and coordinate-spaces hunts.

## BUG-007 - Archives can be written with no provenance block, reported only on stderr

`write_provenance` omits the whole `elivagar` block when the PBF has no file
name or no identity, printing a warning ("input PBF not identified - omitting
provenance") to stderr only. `ocean_build` hits this on every build: it passes
`RunProvenance::default()` and an empty `pbf_path`, so durable ocean artifacts
carry no `elivagar` block (and assemble also attempts
`ElementReader::from_path("")`). `RunProvenance::default().ocean_mode` is
`""`, outside the `none|shapefile|artifact` vocabulary. brokkr reports
`Absent` downstream, but the producer should fail the run or count it.

Reported by the pipeline and run-contract hunts.

## BUG-008 - `--locations-on-ways` can be forced on a PBF without locations

`locations_on_ways = config.locations_on_ways || header...`. If the header
lacks the feature, ways resolve coordinates from `way.node_locations()`,
presumably empty, so every way is dropped silently while provenance says
`coordinate_source: inline` and `input.features.locations_on_ways` is false.
Either refuse the flag when the header disagrees or delete it (header
detection covers the legitimate case). The run-contract hunt adds that the
contract/effective split here is correct by the documented rule but worth a
sentence in `reference/cli.md`.

Reported by the pipeline and run-contract hunts.

## BUG-010 - Data bounds come from node extent only; the buffer comment disagrees with the constant

In locations mode, if the producer stripped untagged nodes, extent tracking
sees only tagged nodes, so the ocean band and artifact inner rect are computed
from a bbox smaller than the ways' real extent (the pipeline hunt did not
check whether pbfhogg's add-locations-to-ways keeps untagged nodes). Separately,
`phase12.rs` comments "~1 degree buffer (... roughly 1/360 = 0.003)" but
`buf = 0.01`, about 3.6 degrees in x. Band ownership depends on this bbox. The
coordinate hunt suggests rewording the comment to not hard-code a figure
rather than "fixing" it.

Reported by the pipeline and coordinate-spaces hunts.

## BUG-012 - `0` means three different things across the budget knobs; `--dedup-cap 0` silently gives the default

- `sort_chunk_size`, `way_inflight_budget`, `assemble_batch_budget`,
  `dedup_cap` in `TilegenConfig`: 0 means "use the default".
- `fanout_caps`: 0 means "uncapped" (and `provenance::layer_map(..,
  zero_is_default)` is a third site deciding that).
- `PmtilesWriter::set_dedup_cap`: 0 means "dedup disabled" (writer tests rely
  on it).

So `elivagar run --dedup-cap 0` gives the 1M default, not "off", and the help
text does not say 0 is swallowed. Proposed types: `ByteBudget(NonZeroUsize)`,
`EntryCap`, `enum FanoutCap { Uncapped, Tiles(NonZeroU32) }`,
`enum DedupPolicy { Off, Cap(NonZeroUsize) }`, with `Option` meaning default
and resolution done once (see the budgets entry in
`notes/consolidations.md`).

Reported by the storage hunt; the pipeline and run-contract hunts flag the
same sentinel.

## BUG-013 - Env-var knobs fail open on typos

`ELIVAGAR_ASSEMBLE_WORKERS`, `ELIVAGAR_ASSEMBLE_PARK_BUDGET`,
`ELIVAGAR_ASSEMBLE_SPLIT_TARGET` and `ELIVAGAR_REL_BLOCKS_CAP` all go through
`.parse().ok()...unwrap_or(default)`. `ELIVAGAR_REL_BLOCKS_CAP=1G` (the
spelling every CLI budget accepts) silently means the default, as does
`ELIVAGAR_ASSEMBLE_WORKERS=eight`. brokkr's `capture_env` then records the
variable as set, so the results row claims an A/B arm the run never took -
the failure class the "either it is explicit, or it is not set" rule was
written against.

Reported by the storage hunt.

## BUG-014 - `--threads` is not a thread budget

The `TilegenConfig.threads` doc says it controls the rayon pool and pbfhogg
decode pool. In fact rayon gets all `threads`; pbfhogg decode gets `threads/3`
more (the comment says "rest to rayon"); node workers get
`(threads/4).clamp(2,6)`; way worker, drain and relation-prepass are plain
`std::thread`s; the way in-flight ceiling is `threads.max(8)`; assemble
workers are `threads.clamp(1, 8)`; and `SortPartition::plan_pieces` spawns one
OS thread per source with no bound (fan-in reached about 1000 on NA before
coalescing). Real concurrency is roughly 1.5x to 2x `-j`, stated nowhere.

Reported by the storage hunt.

---

# Ocean

## BUG-015 - Producer/consumer band ownership disagreements are silent in both directions

The band/artifact ownership answer (`ocean_band_tile`) is one function, but
the grid fed to it is chosen independently by the producer (positional
`first()/last()` in `run`) and the consumer (`selected_pass_grid` in
`OceanTiles::band_tile`). The producer also applies a coarser prefilter,
`pass_owns_root_range`, documented as "deliberately follows
pyramid::root_fragments" - a hand copy that already differs in method (ocean
snaps the raw float bbox, rounded and clamped; the pyramid takes the bbox of
the normalized quantized shape). `band_is_empty` is evaluated per pass in
`process_ocean_shapefile` and as `all(grids)` in assemble.

The runtime re-check in `splice_artifact_ocean` ("non-band OSM tile
unexpectedly contains computed ocean") fires only on tiles that also carry
OSM content. An ocean-only tile the producer thinks is band and the consumer
thinks is not is served computed with no error; the other direction drops
the tile's ocean entirely with no error. No test builds both sides from one
plan and checks every tile is owned by exactly one side.

Proposed: the ocean plan's `grid_for_zoom` plus one `owns(z, x, y)` used by
both the pyramid `tile_filter` and assemble; `pass_owns_root_range` calls a
pyramid-exported "root cells for bbox" function.

Reported by the ocean and coordinate-spaces hunts.

## BUG-019 - Retained artifact names collide on same-day same-policy rebuilds

`retain_outgoing_ocean_artifact` names the retained file by policy version and
mtime date only, so a same-version shapefile rotation is indistinguishable
in the name and two same-day rebuilds replace the retained generation. A
short source-hash prefix would make it self-describing.

Reported by the ocean hunt.

## BUG-020 - `artifact_run_copy_items` walks band runs tile by tile

It walks whole runs (`while cursor < run_end`) calling `tile_id_to_zxy` per
tile. Artifact runs are long (the artifact addresses far more tiles than it
stores); band-empty extracts skip this, but a near-world extract with a
non-empty band walks hundreds of millions of tile ids. Intersecting run ranges
with per-zoom band rectangles makes it proportional to band size.
(Performance, not correctness.)

Reported by the ocean hunt.

## BUG-021 - Spliced ocean tiles may differ in layer order from computed ones

The ocean hunt reports that spliced tiles carry the ocean layer last while
computed tiles carry it in Shortbread layer order - a second, unrecorded
difference between artifact-served and computed tiles. The layer-identity
hunt reads it differently: `Layer::Ocean = 25` is the last discriminant, so
locally assembled tiles also emit ocean last, and the two agree only because
both happen to put it last; reordering the enum would make them differ (and
break the pmtiles_writer test pinning `layers[25] == "ocean"`). Both readings
recorded. Proposed owner either way: an explicit tile order in the layer
table, with the splice inserting at the declared position or asserting ocean
is maximal.

Reported by the ocean and layer-identity hunts.

---

# Geometry

---

# Shortbread profile and layers

## BUG-033 - Assembly silently drops features with an out-of-range layer id

`encode_tile_batch_mvt` and `prepare_non_empty_layers` guard with
`if (layer_idx as usize) < s.layers.len()` and drop the feature otherwise. A
corrupt or unknown id should be a typed decode failure. The `Layer` doc
("No `from_index()` needed - layer indices are only used as array offsets")
is why the drop is silent; `Layer::try_from(u8)` carried through
`PendingTile` would fix it.

Reported by the layer-identity hunt.

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

Reported by the encoding hunt.

## BUG-039 - Merged features lose their MVT id; singletons keep theirs

`merge_same_attr_geometries` sets `id = None` on merged features but keeps
ids on singletons, so ids in one layer are present or absent depending on
whether a same-attr sibling happened to be in the tile. Probably intended per
the hunter, but it means feature ids are not a stable consumer contract
(MapLibre feature-state keys on them).

Reported by the encoding hunt.

---

# Archive surface and inspection tools

## BUG-050 - PMTiles header bounds and center are always the world

`assemble.rs` writes `(-180, -85.05, 180, 85.05)` and center `(0, 0, 2)` for
every extract, though `data_bounds` is known; clients that fit to bounds open
a denmark archive at the whole world. There is no production Merc-to-lon/lat
inverse (`merc_y_to_lat` is `cfg(test)`), and `85.05` is another spelling of
`MAX_LATITUDE` (85.051129). `f64_to_e7` truncates rather than rounds
(`85.05 * 1e7` can become `850499999`) - harmless for display but asymmetric
with inspect's `/ 1e7`.

Reported by the coordinate-spaces and archive-surface hunts.

---

# Documentation drift

## BUG-051 - Stale statements in comments and docs

Facts the hunters found stated wrongly. Per the documentation rule, reword so
the drifting specific is not hard-coded rather than updating it.

- `pyramid.rs` and `clip_shape_rect_fast` comments say the fallback is "the
  full i_overlay boolean"; it is the in-tree port.
- `AssemblyScratch`'s header still claims thread-local persistence, while its
  constructor documents deliberate per-tile allocation (see BUG-038).
- `wire_format` documents a `u16 geometry command count`; the code writes and
  reads `u32`. `add_feature_to_layer` references "the `as u8` encode on line
  69" and `LayerBuilder::reclaim_features` (it is `prepare_for_reuse`).
  `KEY_NAMES` marks `way_area` and `height` test-only, but `way_area` is
  emitted by `enrich_polygon_matches` and `height` by land.rs buildings.
- The `OceanInputs`, `LOW_ZOOM_UNION_MAX_ZOOM` and `ocean_pass_max_zooms`
  comments describe the pre-v3 pass structure (see BUG-002's account).
- An `ocean.rs` comment still describes unpartitioned chunk filenames; the
  sort module now refuses those names.

Reported across the polygon-engine, storage, encoding, run-contract, ocean and
layer-identity hunts.

---

# Found during the 2026-11-04 fix waves

## BUG-053 - A regular run hashes the ocean shapefiles before reading them

The artifact builder now keys on hashes of the bytes it actually read. The
regular `run` path still computes `OceanSourceKey` before phase12 and reads the
shapefiles later without comparing, so a shapefile replaced in between yields
computed chunks, or an artifact boundary band, that differ from the key the
checkpoint and provenance record.

Reported by the wave-1 ocean fixer.
