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

## BUG-003 - Sort-chunk compression is a property of the chunks, but resume takes it from the current invocation

`compress_sort_chunks` is in neither `provenance::producer_config` nor the
checkpoint; `producer_config`'s doc classes everything it omits as
"assemble-side", but chunk encoding is a property of bytes on disk. A
`--skip-to sort|assemble` with a different `--compress-sort-chunks` passes the
current config to `SortReader::from_dir` / `SortWriter::resume`.

- Multi-partition chunk files carry a per-compression magic
  (`ELVGSRT1`/`ELVGSRL1`/`ELVGSRS1`), so those fail at open - with a late,
  opaque I/O error rather than a checkpoint refusal.
- Single-partition files (`chunk_NNNN_z7pNNNNN.bin`, from
  `write_chunk_records_presorted` / `write_payload_chunk_records_presorted`)
  and whole files carry no header at all - a u32 count then records, possibly
  inside an lz4/snappy frame. A resume whose chunks are all single-partition
  is a garbage read (storage hunt).
- A `--skip-to ocean` with a different value writes ocean chunks in a second
  codec next to PBF chunks in the first; the magic check then fails at
  sort-open after the whole ocean phase has run (run-contract hunt).

Proposed: record chunk format in a typed checkpoint / chunk-store manifest,
or make every chunk self-describing so the reader detects the codec and the
flag becomes writer-only. See also the resume-compatibility entry in
`notes/consolidations.md`.

Reported by the pipeline, run-contract and storage hunts.

## BUG-004 - A missing `sort_chunks.count` is a warning, so a crashed ocean phase resumes as complete

`load_sort_chunk_count` prints "cannot verify chunk integrity" and continues.
That file is the only evidence that the ocean phase finished before a
`--skip-to sort|assemble`, and the checkpoint is written before the ocean
phase starts, so an ocean phase that crashed midway leaves a valid-looking
checkpoint. A resume then assembles a partial ocean and stamps a full contract
on it. The count belongs inside the versioned checkpoint as a
phase-completion record. The warning is also stderr-only, which brokkr does
not read under `--bench`.

Reported by the run-contract hunt (the pipeline hunt flags the same stderr-only
warning).

## BUG-005 - Computed-ocean archives' contracts do not identify the shapefiles

`OceanContract` carries `mode`, `low_zoom_source`, `runtime_simplification`
and `artifact_key`. In `mode = "shapefile"` the artifact key is `None`, and
`RunProvenance` populates `ocean_artifact_key` only for `Band`. So two
computed-ocean archives built from different water-polygon releases, or under
different `OCEAN_POLICY_VERSION`s, present identical contracts. The identity
already exists as `OceanSourceKey` in `OceanMode::Computed { key }` and the
checkpoint records it; provenance drops it. The denmark gate is safe only
because its baseline is artifact-active; any computed-ocean corpus (raw
variant, another dataset, an artifact-absent A/B arm) cannot tell shapefile
releases apart.

Proposed: the ocean member of the contract is `OceanMode` (or the ocean plan's
`contract()`) serialized with its key in every non-none arm.

Reported by the run-contract and ocean hunts.

## BUG-006 - The artifact key is silently dropped from provenance on a JSON round-trip failure

`OceanArtifactKey::json()` is a hand-formatted string. Callers that want a
`Value` round-trip it through text: in `run()` and `write_provenance` with
`.ok()`, in `save_checkpoint` with `map_err`. A parse failure in
`write_provenance` would record `artifact_key: null` on an artifact-active
archive - indistinguishable from computed - without a word. Should be
infallible by construction (a typed key with one serde codec).

Reported by the run-contract, ocean and archive-surface hunts.

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

## BUG-009 - `parse_byte_size` overflows on large values

`v * 1024 * 1024 * 1024` on a user-supplied `usize` panics in debug and wraps
in release; `--sort-budget 99999999999G` wraps to a small number that then
passes or fails the `>= 64M` check arbitrarily. Use `checked_mul`.

Reported by the pipeline hunt.

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

## BUG-011 - `--fanout-cap` accepts layers it can never affect, and that changes the contract

`main.rs` fills `fanout_caps` for every `Layer`, and `provenance` records every
non-zero entry in both the comparability contract and `producer_config`. The
only consumer is `pipeline/emit.rs::polygon_z_bottom_after_caps` on the OSM
polygon path. Ocean, line and point layers never read it. So
`--fanout-cap ocean=2048` or `--fanout-cap streets=10` changes no tile but
yields a different contract (`corpus check` refuses with exit 2) and a
different `producer_config` (resume refuses). The test
`producer_config_covers_chunk_affecting_settings` uses
`fanout_caps[Layer::Ocean]` as its example of a chunk-affecting setting.

Proposed: the layer table marks which layers are fanout-cappable; the CLI
rejects others and provenance records only those.

Reported by the storage and layer-identity hunts.

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

## BUG-016 - Process-global statistics are never reset

`OCEAN_STATS` is a process-global set of atomics accumulated across passes and
flushed by `emit_ocean_counters`; `debug.rs` statics (WAIT, BUSY, RING_CAP)
are the same shape. "Nothing resets between runs" is a documented assumption
that breaks when a library caller (brokkr links this crate; tests run in one
process) calls `elivagar::run` twice. Return per-pass stats, or own telemetry
in a run-scoped handle.

Reported by the ocean and storage hunts.

## BUG-017 - Malformed shapefile records are skipped silently

`process_ocean_shapefile` skips shx records with negative offsets silently,
and records with bad part indices or short reads with an `eprintln!`, then
continues. Under `--bench` stderr is not read, so a corrupted shapefile
produces a silently thinner ocean whose key still validates (the hash is of
the same corrupt bytes). For a durable world artifact this should be an error
or at least a counter.

Also in the reader: ring role is `w == 0 || signed_area >= 0.0`, inner rings
before any outer are dropped, and a hole attaches to the most recent outer
rather than its container. Correct for the OSMCD split data per the hunter,
but unstated; `union_pieces`' disjoint-interior invariant depends on it.

Reported by the ocean hunt.

## BUG-018 - `ocean_build` hashes the shapefiles, then reads them again

A file replaced between the hash and the geometry read yields an artifact
whose key names bytes it was not built from. Hash while reading, or after.

Reported by the ocean hunt.

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

## BUG-022 - `rescale_shape_pinned_into` can leave a hole-oriented ring in the outer slot

It orients by ORIGINAL index (`i == 0`) and zeroes `used` only if `out[0]` is
short. If ring 0 dies under rescale and ring 1 survives, ring 1 lands in slot
0 oriented as a hole and the shape is kept. Every downstream consumer
re-orients by slot index (`encode_tile_shape` uses `i == 0`), so the emitted
geometry is a free-standing hole promoted to an outer. The test-only
`rescale_shape` has the same shape. Worth a targeted test: a base shape whose
outer collapses under rescale while a hole survives.

Related, latent: `simplify_shape_dp` orients by original index (`read == 0`),
`simplify_shape_vw` handles ring 0 separately, and `clean_shape_in_place`
orients by WRITE index (`write == 0`) - so if the outer is dropped for area,
the first surviving hole is re-oriented as an outer. Two slot-0 policies in
one file; `clean_shape_in_place` only runs on overlay output today.

Reported by the polygon-engine hunt.

## BUG-023 - The fanout cap counts tiles differently from the descent that emits them

`emit.rs::tile_count_for_base_rect` plus its own `base_tile_index` is a
verbatim copy of `pyramid::tile_range_for_base_rect` + `base_tile_index`,
used to decide `z_bottom`. Neither accounts for the 128-unit tile buffer: a
shape whose bbox ends within the buffer past a tile edge is emitted into the
neighbour by the buffered descent but not counted by the cap, so the cap
undercounts. Owner proposed: one `PyramidCell::cover(rect, span)` in
pyramid.rs used by emit.rs (see the tile-cover entry in
`notes/consolidations.md`).

Reported by the polygon-engine hunt; the coordinate-spaces hunt flags the same
copy.

## BUG-024 - Multipolygon inner-ring pairing tests the vertex most likely to be on the boundary

`multipolygon::assemble` runs `sort_rings_deterministic` (rotating each ring
to start at its lexicographically smallest, i.e. leftmost, vertex) BEFORE
`pair_rings`, which then tests only `inner[0]` for containment. An inner ring
touching its outer at a shared node (valid OSM, common) often has that node as
its leftmost vertex; point-in-polygon on the boundary is undefined, so the
inner can fail to match and get "promoted to outer (shell)", filling the hole.

Also: the first containing outer in bbox-sorted order wins, so with nesting
(island in lake in island) an inner can attach to the large outer rather than
the innermost; and unclassified-role rings are classified by winding
(`classify_unclassified`), which is effectively random because OSM way
direction is arbitrary. Should be containment depth. The storage hunt's
structural answer is integer ring assembly on the topology engine (see
`notes/structural.md`).

Reported by the storage hunt.

## BUG-025 - way_index delta decode is not the inverse of encode

`encode_way` writes deltas with `wrapping_sub`; `decode_way` rebuilds them
with `saturating_add`. Two consecutive nodes either side of the antimeridian
(-179.9 and +179.9 in E7) have a true delta of about 3.6e9, which wraps on
encode and saturates on decode, so the decoded longitude comes back as
i32::MAX (214.7 degrees). Every relation member way goes through this path
(`relations.rs::prepare_relation`). The fix is `wrapping_add`; the hunter's
structural point is that none of the three codecs (way delta varint, node FOR
bitpacking, chunk record layout) has a full-range round-trip property test.

Reported by the storage hunt.

## BUG-026 - The unsorted flat node index panics on modern node ids

Two independent guards answer "is the flat index acceptable for this input":
`phase12::select_node_store_mode` refuses PBFs over 1 GB
(`MAX_FLAT_PBF_SIZE`), and `NodeIndex::put` panics past 16 GB of index
(`MAX_FLAT_INDEX_SIZE`, node id 2^31). Current OSM node ids are past 12
billion, so any unsorted extract under 1 GB containing a recent node passes
the first and panics in the second, mid-phase12. The first guard's message
talks about a "~96 GB sparse file" while the second caps at 16 GB. A third
site, `NodeIndex::create` vs `create_unbounded`, keys on the unsafe flag. The
hunter's single honest rule: unsorted input requires
`--allow-unsafe-flat-index` or `--force-sorted` - or delete the flat path.

Also in node_index: `NodeIndex::put` silently ignores negative node ids while
`SortedNodeStore::put` panics on them via the monotonicity assert (its first
`last_node_id` is -1) - one input, two policies.

Reported by the storage hunt.

## BUG-027 - `WayIndex::get` before `finish_writing` returns `None`, counted as a missing reference

`WayIndex` holds both phases in one struct with `Option` writers and mmaps;
`put` panics after finish and `get` before finish silently returns `None`,
which becomes a `missing_relation_way_refs` statistic rather than a
programming error. `decode_way` / `read_varint` truncation paths likewise
return partial geometry on corrupt bytes with no error channel. See the
typestate entry in `notes/structural.md`.

Reported by the storage hunt.

## BUG-028 - Compressed chunk writers double peak memory

`write_chunk_records_presorted` and `write_payload_chunk_records_presorted`
pre-serialize the whole chunk into a second buffer before compressing; with
`--compress-sort-chunks`, peak memory for a 1 GB chunk is about 2 GB and the
budget does not account for it.

Reported by the storage hunt.

---

# Shortbread profile and layers

## BUG-029 - Matchers that return early on key presence drop valid matches

`transport.rs::public_transport_match` returns `None` as soon as a key is
present with a non-listed value: `amenity=shelter + highway=bus_stop`,
`aeroway=gate + railway=halt` and `amenity=<anything> + railway=station` all
yield nothing. Shortbread's `include_when` (in `tests/fixtures/shortbread.yml`)
is an OR over specific values, so these should match; shelter-plus-stop on
one node is a real tagging pattern. The same early-return shape exists in
`streets.rs::street_match` (`highway=<unknown> + railway=rail` gives no
street), `street_polygon_match` (`area:aeroway=<other> + highway=pedestrian`)
and `land.rs::sites_kind` (harmless today since amenity is last).
`land_match` and `water_polygon_match` deliberately fall through instead
(tests B7). So "does an unlisted value on key A block key B?" is answered per
matcher, and the spec YAML has no multi-key cases for these layers.

Proposed owner: one table-walk helper with the upstream semantics (first
matching (key, value) row wins).

Reported by the Shortbread hunt.

## BUG-030 - Street label zoom and kind check keys in different orders

`street_label_zoom` checks highway, railway, aeroway; `street_label_kind_raw`
checks highway, aeroway, railway. With `aeroway=runway + railway=rail` on one
way, the zoom comes from railway (10) and the kind from aeroway ("runway").
Owner proposed: one classifier returning `(StreetKind, link, source key)` read
by both (see the kind-vocabulary entry in `notes/consolidations.md`).

Reported by the Shortbread hunt.

## BUG-031 - `UNKNOWN_STREET_CLASS = 12` collides with `unclassified`'s rank

An unknown kind sorts as unclassified. The verifier refuses unknown kinds so
the archive cannot carry one, but the sentinel is a real rank value, not an
out-of-band marker. `Option<u8>` or an exhaustive enum removes it.

Reported by the Shortbread hunt.

## BUG-032 - Relation geometry dispatch silently drops non-polygon matches

The relation path (`process_prepared_relation_into`) ends its
`match m.geom_expect` in `_ => {}`, so `GeomExpect::Point` and `Line` on a
relation match are discarded silently, and a new `GeomExpect` variant would be
dropped rather than fail to compile. The layer-identity hunt notes the same
("falls through to a no-op"). See the dispatch entry in
`notes/consolidations.md` for the proposed single `emit_match`.

Reported by the pipeline and layer-identity hunts.

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

## BUG-034 - MVT and MLT post-process tiles differently

`assemble.rs::encode_tile_batch_mvt` runs `merge_same_attr_geometries` on
every layer except Ocean (spelled `li == Layer::Ocean as usize`) and
`merge_connected_lines` on every layer at `z < 14`.
`prepare_non_empty_layers` (MLT) merges every layer including Ocean and never
runs `merge_connected_lines`. So MLT archives have merged ocean features and
no line merging at all - the boundary-line duplicate/palindrome defects the
merger and its clip-boundary rule exist to prevent are handled differently per
format, and the boundary-line oracle's guarantees are MVT-only. Nothing tests
the two paths against each other. The ocean exclusion has no recorded
rationale (it dates to commit 7fdeb0a, "Cleanup March 8"); the `z < 14` gate
is a bare literal.

Proposed owner: one format-independent tile post-processing stage (merge
eligibility, line-merge zoom range per layer, from the layer table) that both
encoders consume. The MVT closure also duplicates the MLT helper's body.

Reported by the encoding and layer-identity hunts.

## BUG-035 - `encode_attrs_bytes` writes more attributes than its count byte says

The count is `min(filtered, 255)` but the loop writes every filtered
attribute, so over 255 attrs the record carries trailing bytes the decoder
ignores. Latent (no Shortbread feature has 255 attrs); the cap tests pass only
because they check decoded count. The same function also spells the attribute
zoom filter twice (`zoom >= az` to count, `zoom < attr_zoom -> continue` to
write).

Reported by the encoding hunt.

## BUG-036 - String truncation in the sort record can split a UTF-8 sequence

After which the decoder's `from_utf8(..).unwrap_or("")` turns the whole value
into an empty string. Latent (OSM caps values at 255 chars), silent.

Reported by the encoding hunt.

## BUG-037 - MVT interning saturates at u16::MAX and mis-tags features

`intern_key`, `intern_value`, `intern_string_value` compute
`len().min(u16::MAX)`; on the 65,536th distinct value in one layer of one
tile, every further distinct value gets index 65535, a real and different
value - silently wrong attributes. Use `u32` (MVT tags are uint32) or return
an error; `(u16, u16)` tag pairs should be `(KeyIdx, ValIdx)` newtypes since
the encoder remaps each through a different table by tuple position.

Reported by the encoding hunt.

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

## BUG-040 - Out-of-order tiles silently corrupt the directory in release builds

`push_dir_entry` only `debug_assert!`s Hilbert order, and its comment claims
out-of-order input "would degrade run-length compression but not produce an
invalid archive - PMTiles readers binary-search the directory regardless".
`encode_tile_id_column` uses `saturating_sub`, so an out-of-order id encodes
delta 0 and decodes as the previous entry's id - the directory addresses the
wrong tile or a duplicate id. `add_run` has no order check at all. Should be a
hard error (one compare per tile).

Reported by the archive-surface hunt.

## BUG-041 - `PmtilesReader` decides tile and directory compression wrongly and allocates unbounded

- `read_tile` always `gzip_decompress`es regardless of the header, so `svg`
  and `diag` on a brotli-MVT archive fail with a gzip error rather than
  "unsupported tile compression".
- It treats any `internal_compression != 2` as none, so brotli- or
  zstd-compressed directories decode as raw varints (garbage or a confusing
  varint error), while `ArchiveView::open` rejects anything but 1 or 2.
- `read_metadata` uses `from_utf8_lossy` (invalid UTF-8 becomes U+FFFD and the
  contract parses as something it is not) while `ArchiveView::metadata` is
  strict.
- `read_directory` / `read_metadata` / `read_tile` allocate
  `vec![0; length]` from untrusted header values with no bound; inspect caps
  metadata at 10 MB but not directories, svg/diag cap nothing.

The hunter proposes deleting `PmtilesReader` in favour of the mmap view (see
`notes/structural.md`), which moots most of this. The inspect module header's
claim that it reads "optional gzip-compressed metadata" is related drift.

Reported by the archive-surface hunt.

## BUG-042 - `elivagar verify` does not enforce the MVT command grammar it cites

`expect_moveto` is written but never read for LineTo, so a line starting with
LineTo passes; and a linestring's first MoveTo may have count > 1 (the check
is gated on `!expect_moveto`), contrary to spec 4.3.4.3, which the error
message itself cites. `verify` is also silently MVT-gzip-only: it cannot
verify any MLT or brotli archive the writer produces.

Reported by the archive-surface hunt.

## BUG-043 - `tile_detail` is more permissive than verify in Strict mode

The gate's decoder accepts streams verify rejects: zero repeat count,
ClosePath on lines silently ignored, LineTo count with fewer than 2 points,
polygon ring without ClosePath. It accepts `geom_type` other than 1/2/3 by
returning no components, treats a feature with no geometry field as valid,
and resolves repeated `id`/`type` fields last-wins silently. For a strict gate
these are "foreign structure". `tile_detail` - the decoder the standing gate
depends on - also has no tests in this crate.

Reported by the archive-surface hunt.

## BUG-044 - The lenient MVT polygon decoder can panic `verify` on a corrupt tile

`geometry::decode_mvt_polygon` uses unchecked `i32 +=` on deltas (debug panic,
release wrap on hostile input), silently skips unknown commands and returns
partial rings on truncation. Its doc says it is for "trusted in-pipeline
data", yet `verify` - whose job is to distrust - feeds archive bytes into it
via the ocean ring check. `tile_detail` uses `checked_add`. See the decoder
entry in `notes/consolidations.md`.

Reported by the polygon-engine and archive-surface hunts.

## BUG-045 - `verify` without `--unique-payloads` re-decodes identical bytes per addressed tile

In non-unique mode it caches only the geometry outcome per run;
`decode_mvt_layers`, `validate_ocean_rings` and the stats collector are
recomputed for every addressed tile in a run. On ocean-heavy archives (runs of
hundreds of thousands of full-tile fills) that dominates. Cache the whole
per-(blob, class) outcome. (Performance.) The payload is also
protobuf-parsed four times per tile.

Reported by the archive-surface hunt.

## BUG-046 - `inspect` misreports the section layout and parses JSON by substring

The section layout prints the header at the root directory's offset (127,
not 0), and lists sections in a fixed order (root, metadata, leaf, data) that
no longer matches the writer (data precedes metadata and leaves).
Separately, `extract_json_string(j, "\"tile_payload_format\":\"")`,
`extract_number` and `print_metadata_json`'s `"id":"` scanner reimplement JSON
parsing by `find` in a module that also `serde_json::from_str`s the same
document; they break on whitespace after a colon (any non-elivagar or
pretty-printed metadata).

Reported by the archive-surface hunt.

## BUG-047 - `producer_config_diff` dedups by prefix

It dedups by `starts_with(key)`, so a key that prefixes another (`min_zoom`
vs a hypothetical `min_zoom_ocean`) would suppress the second diff line.

Reported by the archive-surface hunt.

## BUG-048 - `svg` and `diag` accept invalid tile coordinates

`main.rs` declares `z: u8, x: u32, y: u32` with no range parser.
`xy_to_tile_id` overflows `1u64 << z` for `z >= 64` and truncates `n as u32`
at z = 32; out-of-range x/y produce a valid-looking but wrong tile id (the
high bits are reflected away in `hilbert_xy2d`), so the command renders an
empty or wrong tile instead of refusing.

Reported by the coordinate-spaces hunt.

## BUG-049 - `svg` colours and paint order are per-render, and `-l` is unvalidated

A layer's colour is `LAYER_COLORS[first-appearance index]`, so the same layer
gets different colours in different tiles or grids - working against the
"compare the same ROI across archives" svg-roi idiom. There are 22 colours for
26 layers. Paint order is first-appearance order, and because ocean is last
in each tile it paints over land at opacity 0.8. The `-l` filter never
validates layer names; a typo renders nothing with no error. Key colour and
order by `Layer`.

Reported by the archive-surface and layer-identity hunts.

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

- AGENTS.md cites `specs/` ("see specs/"); no such directory exists.
- AGENTS.md's module list says `geometry.rs` uses Sutherland-Hodgman for OSM
  layers; the float S-H polygon clip has no production caller.
- AGENTS.md lists `marker_span` among the `src/debug.rs` emitters; it does not
  exist.
- `pyramid.rs` and `clip_shape_rect_fast` comments say the fallback is "the
  full i_overlay boolean"; it is the in-tree port.
- `geometry_tests.rs` carries a "simplify-then-reconcile hypothesis" benchmark
  citing `notes/simplify-then-reconcile-design.md` - durable code citing
  notes/ for a retired design.
- `node_index.rs` header cites "See CLAUDE.md" for madvise history and
  `flush_group` cites mimalloc fragmentation; mimalloc is gone and CLAUDE.md
  is not a durable citation target.
- `debug::emit_marker`'s doc says timestamps are "monotonic microseconds since
  process start"; they are relative to the first FIFO write (OnceLock init),
  which happens after the resume input hash on `--skip-to` runs.
- `wire_format` documents a `u16 geometry command count`; the code writes and
  reads `u32`. `add_feature_to_layer` references "the `as u8` encode on line
  69" and `LayerBuilder::reclaim_features` (it is `prepare_for_reuse`).
  `KEY_NAMES` marks `way_area` and `height` test-only, but `way_area` is
  emitted by `enrich_polygon_matches` and `height` by land.rs buildings.
- `reference/corpus.md` lists "seam" among the contract fields; `config_json`
  has no seam member.
- `provenance::Effective`'s doc says the member is omitted only for
  `--skip-to` without a checkpoint; inspect's tests say "effective is absent
  on an ocean-artifact build".
- The `OceanInputs`, `LOW_ZOOM_UNION_MAX_ZOOM` and `ocean_pass_max_zooms`
  comments describe the pre-v3 pass structure (see BUG-002's account).
- `parse_ele_meters` converts via `rounded.to_string().parse::<i64>()`,
  silently dropping values beyond i64.

Reported across the polygon-engine, storage, encoding, run-contract, ocean and
layer-identity hunts.
