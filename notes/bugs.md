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

## BUG-012 - `0` means three different things across the budget knobs; `--dedup-cap 0` silently gives the default

The CLI now rejects `--dedup-cap 0` and resolves an omitted cap to the
writer's `DEFAULT_DEDUP_CAP`, so the user-facing ambiguity is gone. The
library sentinel remains: `TilegenConfig` budgets (`sort_chunk_size`,
`way_inflight_budget`, `assemble_batch_budget`, `dedup_cap`) still read 0 as
"use the default", fanout caps read 0 as "uncapped", and
`PmtilesWriter::set_dedup_cap(0)` stops new dedup entries. Proposed types:
`ByteBudget(NonZeroUsize)`, an entry cap,
`enum FanoutCap { Uncapped, Tiles(NonZeroU32) }`,
`enum DedupPolicy { Off, Cap(NonZeroUsize) }`, with `Option` meaning default
and resolution done once. The consumers in `pipeline/phase12.rs` (including
the locations-mode way-budget defaults), `pipeline/emit.rs` and
`pipeline/relations.rs` must move together with the field types, and the
change is a public `TilegenConfig` break brokkr must follow.

Reported by the storage hunt; the pipeline and run-contract hunts flag the
same sentinel.

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

## BUG-062 - A multipolygon gets one label point per component

`pipeline/relations.rs` calls emission once per assembled polygon
component, so a multipolygon relation matched by a centroid or
point-on-surface label layer gets one label point per outer, where
Planetiler's Shortbread profile emits one aggregate point for the whole
relation. Decide against `tests/fixtures/shortbread.yml` which the spec
wants; changing it moves label points and counts.

Reported by the wave-8 centroid fixer.

