# Spec: durable precomputed ocean tile stream (H5)

Status: v5 IMPLEMENTED AND ADJUDICATED 2026-07-12 (`92ed329` and
follow-ups). The brick 5 denmark regress read 27 structural ocean diffs
vs the computed blessed baseline, including one on a tile whose buffered
footprint never touches the clip edge (z8/121/81) - so interior tiles
are NOT independent of the ocean clip rect: the pyramid's root-cell
selection and row-band bisection depend on each piece's clipped extent,
and the artifact's world-descent seams legitimately differ from the
extract-clip descent's. The stopping-rule retraction was briefly landed,
then REVERSED by the human gate: the artifact-active archive was judged
equivalent in the MapLibre viewer (displacements mostly 10-97 units, the
same class as the accepted 2-unit seam-thinning drift), so extracts DO
consume the artifact and the blessed baseline rotated to the
artifact-active build. Standing consequence: extract output depends on
artifact presence - regress gates assume the gate machine carries the
same data/ocean-tiles.pmtiles the blessed archive was built with. The
artifact's own gates (unique-payloads verify, unique earcut, 942.7 MB /
9.2M blobs / 13.97M runs vs the 8 GB / 25M / 40M bounds) all passed.
Three same-day codex spec critiques (R1: 19 findings, R2: 14, R3: 6)
plus a competitor-comparison review (R4, Opus agent over
research/planetiler, tilemaker, tippecanoe, stedsplakat) are folded -
see "Review resolutions". One spec erratum found at the gates: the
brick 4 eyeball tile z10/544/316 named as inland is actually the Great
Belt strait (sea) - the artifact's content there matches the computed
path exactly. Written against `reference/technical-implementation-spec.md`
(the contract). Spawned from `notes/planet-30gb-roadmap.md` hypothesis H5,
which carries the claim and evidence. Measurement record:
`reference/performance.md` + `.brokkr/results.db`.

## The claim, repriced against current code

Ocean input is constant across runs - it changes only when the
water-polygons shapefile release changes. Every run recomputes it, and the
recompute price has three parts:

1. **The ocean phase.** NA locations (`88dc2385`, 4e7847c): 19.7s wall,
   7.8 GB RSS - the peak-RSS phase of the run. Planet projection (H3
   go/no-go): 50-60s and 12-17 GB. The roadmap's honest framing: that
   fits standalone but consumes the 30 GB comfort margin; H5 is wanted,
   not strictly required, on the RAM axis alone.
2. **Ocean's trip through sort.** 106.5M ocean features at NA (vs 516M
   OSM records; norway: ocean is 59% of all sort records). At 60-100
   B/record that is ~14-24 GB of planet scratch, ~28-47 GB of write+read
   traffic through a 30 GB page cache. Not separately counted today
   (survey: the stats gap).
3. **Assemble work per ocean tile.** `PmtilesWriter::add_tile` dedups on
   the ALREADY-GZIPPED payload; dedup saves storage only. Each of NA's
   101,793,871 reused tiles (85% of 119.8M addressed) paid partition
   merge + record decode + MVT encode + gzip before dedup discarded the
   bytes. At planet, every ocean-full tile (~230-250M, priced by brick 4)
   pays that chain for a constant ~100-byte fill.

The fix: compute the world's ocean tiles ONCE into a durable artifact
keyed by its inputs; assemble merges the artifact as a second ordered
tile stream, run-aware, so full-fill tiles stay collapsed as references
end to end. The runtime ocean phase prunes to the boundary band AT THE
TRAVERSAL level (D3) - at world bounds it does no shapefile work at all -
and the full-fill encode chains become run copies.

## Survey of the ground

### Producer path

- `src/pipeline/mod.rs` (~line 358): after phase12, calls
  `ocean::process_ocean_shapefile` once or twice. Pass base grids are
  NOT fixed z7/z14: the pipeline uses `min(config.max_zoom, 7)` and
  `config.max_zoom` (~line 420), so a library run with `max_zoom < 14`
  quantizes on different grids. Each pass derives its integer clip rect
  (`ocean.rs::data_bounds_rect`) by OUTWARD floor/ceil quantization of
  `data_bounds` onto its own base grid (scale
  `1 << (pass_max_zoom + 12)`).
- `src/ocean.rs`: shapefile records are first rejected against the
  FLOATING-POINT `data_bounds` (~line 447) - before quantization - then
  quantized with round-to-nearest (`int_ocean.rs` ~line 461), then
  shapes fully inside the pass's `data_rect` skip clipping
  (`push_quantized_pieces` identity arm) and crossing shapes are cut by
  `intersect_rect_into`. Consequence (R2 finding 2): a shape in the
  sub-grid sliver between the float bound and the outward-rounded
  `data_rect` edge is DROPPED by the computed path but present in a
  world artifact; the band predicate must treat that sliver as band
  (D3's inner safety rect).
- `src/geometry/pyramid.rs` + `geometry/int_ocean.rs`: every emitted
  tile is built from its cell EXPANDED by the 128-unit buffer
  (TILE_BUFFER at extent 4096, scaled to base-grid units as
  `128 << (pass_max_zoom - z)`, matching `pyramid.rs` ~line 255; full
  fills span -128..4224, `int_ocean.rs` ~line 988).
  `emit_full_subtree` emits every descendant of a uniform cell as an
  individual record. Emission reaches records through the sink closure
  passed by `ocean.rs` (`ocean_sink`, ~line 714), which captures the
  piece id; `emit_full_tile` itself carries NO id (R2 finding 6) - the
  full-fill/coastal distinction does not currently reach the sink.
- **Ocean records are NOT anonymous**: the sink writes `piece_idx` as
  `osm_id` -> `Feature.id = Some(..)` (`wire_format.rs` ~line 490).
  Piece indices are assigned after bounds filtering, so ids differ
  between world and extract runs, and full-fill payloads differ per
  piece - not canonical. Non-semantic id dependencies to be aware of:
  the id leads the record payload and participates in sort tie ordering
  (`sort.rs` ~line 240), it changes MVT bytes and therefore PMTiles
  dedup groupings, and regress's tier-2 fingerprint binds it (tiles
  differing only in ocean ids demote to the detail tier, where ocean
  matching is geometric and id-blind - `regress.rs` ocean handling).
- Stats gap: per-layer totals harvest phase12 pushes only
  (`pipeline/stats.rs`; snapshot taken before ocean runs); ocean flushes
  through `OceanAcc` -> `sort::SpillCoalescer` uncounted. `OceanAcc.bytes`
  includes `PayloadRecord` struct overhead, NOT comparable to
  `sort_layer_*_bytes` payload-only semantics (`sort.rs` ~line 581).

### Consumer path

- `src/sort.rs`: partition tile ranges are computed independently
  (~line 283); `take_partitions` (~line 1670) returns only partitions
  holding sort sources; `SortPartition` (~line 1528) represents sort
  sources only. Artifact runs can cross partition boundaries (PMTiles
  run extension cares only about consecutive ids + same blob), so
  consumption must clip runs at partition range edges (D5).
- Legacy non-partitioned assemble path (`assemble.rs` ~line 150):
  explicitly OUT - computed fallback only, never extended.
- `src/pipeline/assemble.rs`: per-partition merge -> `LayerBuilder`s by
  `Layer::ALL` index; Ocean is index 25, LAST; `encode_tile_into` emits
  in supplied order, so the ocean layer message (top-level field 3:
  tag + length varint + payload) is the final bytes of any tile that
  has one. `PartitionBatch` items are owned `EncodedTile`s (~line 51)
  consumed sequentially by the single writer (~line 697). Assemble
  supports gzip AND brotli, with per-zoom compression levels derived
  from `TilegenConfig.compression_level` (~line 1216); MLT exists as a
  tile format.
- `src/pmtiles_writer.rs`: `add_tile` returns `bool` (unique) and feeds
  per-zoom unique/byte counters; `push_dir_entry` extends a run when
  ids are consecutive AND offset+length match; dedup LOOKUP continues
  past the 1M insert cap but INSERTION stops - so post-cap, repeated
  `add_tile` of a novel payload stores it repeatedly and produces no
  run. Any run API must define its cap behavior explicitly (D5).
- `src/pmtiles_reader.rs`: `read_all_runs` -> sorted invariant-checked
  `Vec<RawDirEntry>`; `PmtilesReader` is seek-based with per-call
  allocation - not the artifact reader. `src/regress.rs::ArchiveView`
  (memmap2, zero-copy `raw_blob(&self)`) is the right shape; it
  currently validates MVT+gzip archives (~line 410), which matches the
  artifact format exactly. Promoted in D4.
- Checkpoints (`pipeline/mod.rs` ~line 186): bounds + chunk count only.
  THREE resume modes exist: `--skip-to ocean|sort|assemble`
  (`SkipTo::Assemble` at ~line 65 reuses ocean-bearing chunks at
  ~line 489). All three are unsound across an ocean-mode change.
- Scale limits: `elivagar verify` replays per addressed tile; the
  earcut oracle expands runs per tile. Neither can gate a ~240M-tile
  artifact; brick 3 builds run-aware modes first.

### Failure-history check

`notes/rendering-fix-log.md` (R01-R20/S01-S09): no prior
precomputed-ocean attempt; nearest is the reverted ocean-only tile skip
(`1f4adda`) - this spec makes ocean tiles cheaper, never absent. The
2026-07-09 cross-variant lesson: every gate pins `--dataset` AND
`--variant`.

## Target design

### D1. Canonical full-fill identity (runtime change, lands first)

Full-fill emissions get constant feature id 0 in both the runtime path
and the artifact build; coastal emissions keep piece ids. Mechanism
(pinned, because `emit_full_tile` carries no id today): the pyramid sink
signature gains an emission kind -

```rust
pub enum PyramidEmitKind { Fragment, FullFill }
// PyramidSink becomes: FnMut(z: u8, tx: u32, ty: u32, geom: &[u32], kind: PyramidEmitKind)
```

`emit_full_subtree` / `emit_full_tile` call sites pass `FullFill`; all
others `Fragment`. The ocean sink maps `FullFill` to id 0 and `Fragment`
to the piece id; OSM polygon emission (the other pyramid user) ignores
the kind and keeps its osm ids - asserted by unit test, not by
inspection. Effects: full-fill payloads become canonical per
zoom-compression-regime, today's dedup improves, artifact and runtime
full fills become byte-equal. Output bytes change; regress is
geometry-blind to ocean ids, so the gate is regress tol 0 plus the named
unit tests below.

Unit tests (brick 2): full fill decodes with `Feature.id = Some(0)`;
coastal cell emission preserves the piece id; an OSM polygon emitted
through the pyramid preserves its osm id.

Competitor calibration (R4): this is not cleanup - planetiler bakes an
incrementing per-piece id into every fill feature
(`ShapefileReader.java` ~line 159, `FeatureRenderer.emitFilledTiles`),
so its fill dedup works only within one shapefile piece's interior;
identical fills across piece boundaries hash differently and are NOT
deduped. D1 closes a dedup gap the best competitor still has.
Planetiler's planet census (`VectorTile.java` ~line 639: 267M total
tiles, 38M unique, fill/edge heuristic catching >99.9% of repeats)
independently brackets this spec's artifact estimates.

### D2. The artifact is a PMTiles archive

`data/ocean-tiles.pmtiles`: world-bounds, MVT + gzip, z0-14, every
ADDRESSED tile exactly one layer named `ocean` (land-only tiles are
absent). Produced by `elivagar ocean-build` running the existing
ocean -> sort -> assemble machinery with `data_bounds` = the full
Mercator square and the OSM phases skipped.

Artifact metadata: `build_metadata` gains an optional extension object;
the artifact declares ONLY the ocean layer in `vector_layers` and
carries the invalidation key:

```rust
/// Serialized under the "ocean_artifact" key in the PMTiles metadata
/// JSON. All hashes are XXH3-128 rendered as fixed-width 32-char
/// lowercase hex strings; absent simplified files serialize as null.
pub struct OceanArtifactKey {
    pub full_shp_xxh128: u128,
    pub full_shx_xxh128: u128,
    pub simplified_shp_xxh128: Option<u128>,
    pub simplified_shx_xxh128: Option<u128>,
    pub min_zoom: u8,             // 0
    pub max_zoom: u8,             // 14
    pub compression_level: u32,   // TilegenConfig.compression_level at build
    pub policy_version: u32,      // OCEAN_POLICY_VERSION
}
```

`OCEAN_POLICY_VERSION` (const in `ocean.rs`, starts at 1) pins by
convention everything else that shapes ocean tile bytes:
OCEAN_DP_TOL_PX, the min-area schedule, the simplified/full pass split
rule, TILE_BUFFER, extent, the D1 id policy, the per-zoom gzip level
regime shape, and tile format. Any change bumps it by hand (the same
class of change that already forces a bless rotation). Both `.shp` AND
`.shx` are hashed for both shapefiles.

Key production (R3 finding 3 - there is no trusted hash manifest):
`OceanArtifactKey::from_inputs(full_shp, full_shx, simplified_shp,
simplified_shx, min_zoom, max_zoom, compression_level) ->
io::Result<OceanArtifactKey>` streams and hashes the four files at
activation time. That is a sequential read of ~1.3 GB (full .shp) plus
three small files, ~1-2s - accounted in the world-path timing and
accepted. The no-shapefile claim is therefore precisely: after key
validation, no shapefile GEOMETRY is parsed or mmap'd; the files are
read once, for hashing, even at world bounds. ocean-build embeds the
key it computed from the same inputs.

### D3. Exact band predicate + traversal pruning

Band membership is a pure function of (z, tx, ty) and the pass geometry:

```rust
/// One ocean pass's integer clip geometry, precomputed per run.
pub struct OceanPassGrid {
    pub max_zoom: u8,         // pass base grid zoom
    pub inner_rect: IntRect,  // INWARD quantization: ceil(min), floor(max)
    pub world_rect: IntRect,  // data_bounds_rect(&WORLD, max_zoom)
}

/// True = computed path owns this tile; false = artifact owns it.
pub fn ocean_band_tile(z: u8, tx: u32, ty: u32, pass: &OceanPassGrid) -> bool {
    let shift = u32::from(pass.max_zoom - z);
    let cell = 4096_i64 << shift;
    let buffer = 128_i64 << shift; // TILE_BUFFER scaled to base units
    let buffered = IntRect {
        min_x: clamp_base(i64::from(tx) * cell - buffer),
        min_y: clamp_base(i64::from(ty) * cell - buffer),
        max_x: clamp_base((i64::from(tx) + 1) * cell + buffer),
        max_y: clamp_base((i64::from(ty) + 1) * cell + buffer),
    };
    !rect_contains(pass.inner_rect, rect_intersect(buffered, pass.world_rect))
}
```

Two deliberate asymmetries vs the producer's clip rect: (a) the
predicate tests against an INWARD-quantized rect (ceil minima, floor
maxima) while the producer clips against the outward rect - this makes
the float-vs-quantized sliver (survey) band territory, so the sliver's
possibly-dropped shapes can never be served stale from the artifact;
(b) the buffered footprint is intersected with the world rect first, so
world-bounds runs have an EMPTY band at every zoom including z0 and the
world edge.

Precedence: band tiles use the computed result EVEN IF EMPTY; non-band
tiles use the artifact exclusively; no tile is fed from both.

**Traversal pruning (R2 finding 1 - the compute claim must be earned,
not just the record claim):** the same containment test prunes work at
every level that enumerates tiles, using the monotonicity (verified in
R3) that a child cell's buffered footprint is contained in its parent's:

- World precheck: when every pass grid reports an empty band
  (data_bounds covers the world), the ocean phase does no shapefile
  geometry work at all - the passes return before parsing.
- Input filter (extracts): a piece is skipped iff its SNAPPED z-top
  root-cell range - the exact range `root_fragments` would select
  (`pyramid.rs` ~line 150 snaps the piece bbox outward to whole z-top
  cells) - buffered at z-top, is contained in `inner_rect` intersected
  with `world_rect`. Testing the raw bbox instead of the snapped root
  range is UNSOUND (R3 finding 1): a piece can sit farther than the
  buffer from the clip edge while its snapped root cell still crosses
  it, and dropping it would drop a band tile whose emptiness suppresses
  the artifact.
- Descent: a cell whose buffered footprint is contained the same way
  emits only non-band tiles - pruned before `emit_cell`.
- Full subtrees (R3 finding 2): `emit_full_subtree` becomes band-aware
  recursion instead of the flat all-descendants loop - emit the full
  tile for the current cell, prune any child whose buffered footprint
  is contained (its whole subtree is artifact territory), recurse into
  the rest. Cost is O(band tiles in the subtree), not O(subtree). The
  same ownership check runs inside `split_for_parallel` before both its
  full-cell and `emit_cell` arms (large pieces bypass the ordinary
  descent entry, `pyramid.rs` ~line 84).
- Sink: leaf records for non-band tiles are dropped (covers mixed
  cells' leaves).

The runtime ocean phase therefore genuinely shrinks to band compute,
not merely band records; at world bounds it does no geometry work.

### D4. Artifact reader (promoted ArchiveView)

`regress::ArchiveView` moves to `pmtiles_reader::ArchiveView` (regress
keeps using it; one mmap'd view type). The artifact handle:

```rust
pub struct OceanTiles {
    view: ArchiveView,             // memmap2, zero-copy raw_blob()
    runs: Vec<RawDirEntry>,        // sorted, invariant-checked
    key: OceanArtifactKey,
    grids: Vec<OceanPassGrid>,     // one per pass, built from run bounds
}

impl OceanTiles {
    /// Opens, validates, and binds the artifact to THIS run's geometry.
    /// Validations, all hard errors: PMTiles v3, MVT + gzip, min/max zoom
    /// 0..14, exactly one declared vector layer named "ocean",
    /// "ocean_artifact" metadata present and equal to `expected`.
    pub fn open(
        path: &Path,
        expected: &OceanArtifactKey,
        data_bounds: &MercBbox,     // this run's bounds -> inner/world rects
        pass_max_zooms: &[u8],      // this run's pass base grids
    ) -> io::Result<Self>;
    /// Runs overlapping [start, end) - band filtering is the caller's.
    pub fn runs_in(&self, start: u64, end: u64) -> &[RawDirEntry];
    pub fn raw_blob(&self, offset: u64, length: u32) -> io::Result<&[u8]>;
}
```

Shared as `Arc<OceanTiles>` across assemble workers (`&self` reads on
an `Mmap`, no seeks, no per-call allocation). The runs vector is a real
RAM stock: 24 B/run, counted by the brick 4 build (`H3 ledger entry`;
estimate 5-20M runs = 120-480 MB; brick 4's keep bound caps it).

### D5. Run-aware consumption in assemble

**Partition union.** Scheduling iterates the ordered union of (a)
today's sort-source partitions and (b) artifact-only ranges: maximal
tile-id intervals covered by artifact runs, clipped at every sort
partition boundary (partition ranges from `sort.rs` ~line 283), that
fall in no scheduled sort partition. Pinned carrier types:

```rust
enum UnionPartition {
    Sort(SortPartition),                       // may also overlap artifact runs
    ArtifactOnly { start: u64, end: u64 },     // copy-only loop, no merge heap
}

enum PartitionItem {
    Encoded(EncodedTile),                          // today's item
    RunCopy { tile_id: u64, run_length: u32, offset: u64, length: u32 },
}
```

The writer thread consumes `PartitionItem`s in tile-id order;
`RunCopy` resolves its bytes via `Arc<OceanTiles>::raw_blob` at write
time (no payload copies parked in batches).

**Run splitting.** Within a partition, artifact runs are split at:
(1) partition range boundaries (always); (2) zoom-boundary tile ids
(always - runs can numerically cross zoom levels, and per-zoom counter
attribution needs zoom-pure runs; 14 fixed boundaries, cheap); (3) tile
ids holding OSM/band sort records (splice those tiles); (4) band tile
ids (suppress those tiles). Two regimes, chosen once per run from the
band state:

- Band empty (world bounds): no per-tile checks exist at all - runs are
  clipped only at partition edges and OSM-record ids. This is the
  planet path; the ~230-250M full-fill tiles flow as run copies.
- Band non-empty (extracts): runs additionally walk their tile ids for
  band membership. Extract archives are millions of tiles, band checks
  are two rect tests; the per-tile walk is acceptable at extract scale
  and does not exist at planet scale. (No sub-run band enumeration
  cleverness is needed at either scale.)

**Splice.** Artifact tile + OSM records for the same id: gunzip the
artifact blob, validate exactly one layer named `ocean` (else hard
error), append the COMPLETE top-level field-3 encoding (tag byte +
length varint + payload, exactly as present in the artifact tile) after
the encoded OSM layer bytes, gzip once at the tile's zoom-regime level.
The OSM encode path asserts it produced no computed ocean layer for a
non-band tile.

**Writer API.** `add_run` is NOT add_tile-equivalence (R2 finding 3 -
the dedup cap breaks that): it has its own pinned semantics -

```rust
/// Add `run_length` consecutive tiles sharing one payload. The payload
/// is stored at most once PER CALL regardless of the dedup cap (run-local
/// sharing); the dedup map is probed once and, below the cap, inserted
/// once. Returns true if this call stored a new blob. num_addressed
/// grows by run_length, num_unique by at most 1, per-zoom tile counters
/// by run_length (attributed by tile zoom), directory runs extend
/// arithmetically.
pub fn add_run(&mut self, tile_id: u64, run_length: u32, data: &[u8]) -> io::Result<bool>;
```

Equivalence to N `add_tile` calls is GUARANTEED when the dedup cap is
not exceeded during the sequence (and can still hold above the cap when
the payload already sits in the map - lookup survives the cap); the
brick 5 unit test pins BOTH regimes (identical archives below cap;
above cap with a novel payload, add_run's archive has the run + single
blob while add_tile's has neither, with header counts asserted for
each).

**Dedup-cap interaction (R4).** Artifact run copies carry their sharing
structurally (one blob per call), so ocean tiles - the reason the 1M
insert cap exists (`pmtiles_writer.rs` cap comment) - stop depending on
the dedup map at all under this design. The cap's residual planet cost
(missed dedup on post-cap NOVEL payloads, i.e. OSM tiles) is the
roadmap ledger's existing open pricing item; if that pricing ever
demands action, planetiler's answer is a shape predicate on WHAT to
hash (`VectorTile.likelyToBeDuplicated`: fills/edges only, 38M -> 735K
tracked) rather than a count cap - recorded here as the candidate
design, out of this spec's scope.

**Counter accounting (R3 finding 4 - the standing counters must stay
true, not just the archive).** The per-zoom and size counters live in
assemble/stats, not the writer, and update per `EncodedTile` today. A
consumed `RunCopy { tile_id, run_length, length, .. }` (zoom-pure by
split rule 2) updates: `tiles_written` and `tiles_per_zoom[z]` +=
run_length; `bytes_per_zoom[z]` += run_length * length;
`unique_per_zoom[z]` += 1 iff add_run stored a new blob (its bool);
`TileSizeDiagnostics` observes (length, z, x, y of the run head) once
per run with weight run_length for totals/average and a single max/
oversize probe (all run tiles share the payload, so per-tile oversize
replay adds nothing); `DedupStats.tiles_reused` += run_length - 1 for
the intra-run shares plus the usual accounting for the cross-run dedup
probe, and `bytes_saved` correspondingly. The brick 5 counter unit test
asserts a run-copied archive and its add_tile-expanded twin report
identical counters below the cap.

### D6. Activation, fallback, configuration, resume

`TilegenConfig.ocean_tiles: Option<PathBuf>` + CLI `--ocean-tiles
<path>`, auto-detected as `data/ocean-tiles.pmtiles` next to the
shapefile auto-detection; `--no-ocean` disables artifact and shapefiles
both. Activation requires ALL of: `tile_format == Mvt`,
`tile_compression == Gzip`, `config.compression_level ==
key.compression_level`, `config.min_zoom == 0 && config.max_zoom == 14`
(the artifact's exact build shape). Any other configuration - brotli,
MLT, non-default levels, library zoom ranges - takes the computed path
with a single log line. That IS the complete support story for those
configurations (v2's "regenerate per format" is DROPPED as
contradictory; a future record run that wants a brotli artifact writes
a future one-line spec extending the key - stated here so nothing is
silently deferred: today's behavior for those configs is the computed
path, fully supported, gated by the same standing gates as today).

Checkpoints: `CHECKPOINT_VERSION` bumps; checkpoint gains

```rust
pub enum OceanMode {
    None,                              // --no-ocean
    Computed,                          // shapefiles, no artifact
    Band { key: OceanArtifactKey },    // artifact active
}
```

ALL resume modes - `--skip-to ocean`, `--skip-to sort`, AND `--skip-to
assemble` - hard-reject (named error) when the resuming run's resolved
ocean mode differs from the checkpoint's.

## Bricks

Baselines (plantasjen): denmark locations bench-3 12.3s at `2c770c7`
(current denmark ocean-phase split unmeasured at this commit; brick 5
reads its pre-landing sidecar split before landing). NA locations
single-run 348.5s at `4e7847c` (`88dc2385`), flags `--variant locations
--compress-sort-chunks lz4` (the lz4+8-worker configuration; 8 workers
are default since the same campaign). Blessed regress reference:
`blessed/denmark-506b9bc.pmtiles`. Noise rule per
`reference/performance.md`: extract keep bounds carry 3%; NA readings
use the H9 ladder discipline (`--bench 1` at NA scale is the roadmap's
own rule for non-record runs; the brick 6 verdict is read against a
same-flags baseline, not across configurations).

Where a gate consumes an output whose filename embeds the build commit,
the spec pins the PATTERN: `brokkr tilegen` writes
`data/tilegen/<dataset>-<short-head>.pmtiles` and prints the path; the
bench prints the run UUID on completion. The gate command is
copy-pasteable modulo those two printed values, which the harness echoes
immediately before the gate is run.

### Brick 1 - ocean pricing counters (no behavior change)

Always-on `sort_layer_ocean_records` / `sort_layer_ocean_bytes` totals
plus per-zoom rows under `ELIVAGAR_LAYER_STATS`, harvested at the
OceanAcc flush. Byte semantics pinned: PAYLOAD bytes only (matching
`sort_layer_*_bytes`), not `OceanAcc.bytes`. Share denominators:
records = `sort_records + ocean records`; bytes = `sort_record_bytes +
ocean payload bytes` (the phase12 snapshot excludes ocean).

Gates:
- `brokkr check`
- `brokkr tilegen --dataset denmark --variant locations` then
  `brokkr regress --dataset denmark` - tol 0, zero diffs.
- Reading: `ELIVAGAR_LAYER_STATS=1 brokkr tilegen --bench 1 --dataset
  norway --variant locations`, then `brokkr sidecar <uuid printed by
  the bench> --counters`.
- Verdict (single, deterministic): PROCEED iff ocean records >= 20% of
  total records OR ocean payload bytes >= 10% of total payload bytes.
  Otherwise the roadmap's 59%-of-records claim is refuted, this spec
  CLOSES, and H5 returns to the roadmap with the reading attached.
  (Expected: ~50-60% records, ~15-30% bytes.)

### Brick 2 - canonical full-fill identity (D1)

`PyramidEmitKind` + id-0 full fills, both paths, one landing.

Gates:
- `brokkr check` (includes the three D1 unit tests, named there)
- `brokkr tilegen --dataset denmark --variant locations` then
  `brokkr regress --dataset denmark` - tol 0, zero diffs; a diff means
  the id leaked into geometry/attrs and the brick reverts.
- `elivagar verify data/tilegen/denmark-<short-head>.pmtiles` (path as
  printed by the tilegen line above) - zero errors.
- Dedup reading recorded (informational): denmark sidecar
  `dedup_tiles_reused` / `pmtiles_dedup_entries` vs the `2c770c7` run.

### Brick 3 - scalable artifact verification (instrument before object)

- `elivagar verify <FILE> --unique-payloads`: validate each unique blob
  once (dedup by (offset, length)), per-(z, seam) parameterization
  applied per distinct group referencing the blob; reports addressed
  tiles, unique blobs, runs, per-zoom counts. Default mode unchanged.
- `scripts/validate/earcut-oracle.mjs --unique`: tessellate each unique
  blob once.

Gates:
- `brokkr check`
- From `scripts/validate/`:
  `node earcut-oracle.mjs ../../data/blessed/denmark-506b9bc.pmtiles`
  then
  `node earcut-oracle.mjs ../../data/blessed/denmark-506b9bc.pmtiles --unique`
  - identical verdicts (0 over threshold, 0 misattached).
- `elivagar verify data/blessed/denmark-506b9bc.pmtiles
  --unique-payloads` - zero errors, addressed count 1,296,996 equal to
  default mode's.
- REVERT rule: any disagreement between default and `--unique` modes on
  the same archive reverts the brick (the unique mode is wrong by
  definition; the default mode is the authority).

### Brick 4 - `elivagar ocean-build` (D2)

Subcommand + pipeline entry: world data_bounds, ocean passes + sort +
assemble only, writer metadata extension, canonical ids from brick 2.
Emits per-zoom addressed/unique/reused/runs/bytes counters - this build
IS the deterministic pricing gate the roadmap's H5 first step asked for.

Gates:
- `brokkr check`
- Build (paths are the exact auto-detection targets from
  `main.rs::detect_ocean`):
  `elivagar ocean-build
  --ocean data/water-polygons-split-3857/water_polygons.shp
  --ocean-simplified data/simplified-water-polygons-split-3857/simplified_water_polygons.shp
  -o data/ocean-tiles.pmtiles`
- `elivagar verify data/ocean-tiles.pmtiles --unique-payloads` - zero
  errors.
- From `scripts/validate/`:
  `node earcut-oracle.mjs ../../data/ocean-tiles.pmtiles --unique` -
  clean.
- Eyeball (human gate): `elivagar svg data/ocean-tiles.pmtiles -z 0 -x 0
  -y 0` (world: continents as holes in ocean), `elivagar svg
  data/ocean-tiles.pmtiles -z 4 -x 8 -y 4` (North Atlantic coasts),
  `elivagar svg data/ocean-tiles.pmtiles -z 10 -x 547 -y 320` (Oresund
  coastline detail), and `elivagar diag data/ocean-tiles.pmtiles -z 10
  -x 544 -y 316` (inland Denmark: reports tile not found).
- Winding check (R4/L1): `elivagar diag data/ocean-tiles.pmtiles -z 8
  -x 128 -y 128` (mid-Pacific full fill) - the fill's outer ring must
  report CW (MVT positive-area exterior, matching planetiler's
  encodeFill convention); a flipped fill renders as a hole in viewers.
- KEEP/REVERT (mispricing closes the spec here, before any pipeline
  change): artifact file <= 8 GB AND unique blobs <= 25M AND directory
  runs <= 40M. Over any bound: revert, record readings in the roadmap,
  close as mispriced.
- Build wall + peak RSS + per-zoom counters recorded in
  `reference/performance.md` and the roadmap H5 block.

### Brick 5 - consumption (D3+D4+D5+D6, one landing)

Band predicate + traversal pruning, OceanTiles + ArchiveView promotion,
partition union + run clipping, add_run, splice, activation matrix,
checkpoint versioning. One landing, one keep/revert verdict.

Named unit tests (no oracle reaches these):
- `ocean_band_tile`: world bounds -> empty band at z0/z7/z8/z14 corners
  and world edges; extract bounds -> a tile strictly inside but within
  128 base-buffer units of the clip edge IS band; fractional
  data_bounds where the float bound and the inward-quantized rect
  disagree (the sliver) -> sliver tiles are band; polar edge clamping;
  extract bounds TOUCHING the world x-edges (x=0 / x=1 Mercator, the
  antimeridian columns) -> those columns are band unless bounds cover
  the world (R4/T2: planetiler and tippecanoe both carry explicit
  cross-world-copy defenses at these columns; our unwrapped [0,1]
  model + world-rect intersection makes band the safe answer there);
  both pass grids independently.
- Band inner seam (R4/T1 - planetiler's `bad_polygon_fill` guard exists
  for exactly this geometry): a synthetic ocean polygon whose edge runs
  exactly along a cell boundary at the band's inner edge - the
  band/artifact ownership split must agree with what each path emits
  (band tiles computed, adjacent interior tiles artifact-full), no
  dropped and no doubled tile.
- Traversal pruning: a piece whose snapped z-top root range is interior
  emits nothing; a piece whose raw bbox is interior but whose snapped
  root cell crosses the clip edge is NOT skipped (the R3 counterexample);
  a coastal piece emits band tiles only; a full-subtree cell straddling
  the band prunes its interior children; world bounds parses no
  shapefile geometry (files are read only for key hashing).
- Partition union: sort-only, artifact-only, and mixed partitions;
  artifact runs crossing partition boundaries are clipped; runs
  crossing zoom boundaries.
- Run splitting: OSM record at run start / middle / end; adjacent OSM
  ids; repeated records on one tile id; band tile inside a mixed run is
  suppressed (empty band result suppresses artifact geometry).
- `add_run`: below-cap equivalence to N add_tile calls (byte-identical
  archive); above-cap pinned divergence (run + single blob vs repeated
  blobs, header num_addressed/num_unique asserted both ways); per-zoom
  counter attribution.
- Splice: decoded tile has ocean last, layer set = union; artifact tile
  with a wrong layer name or two layers -> hard error.
- `OceanTiles::open`: key mismatch, malformed/absent metadata, wrong
  format/compression/zoom -> named errors.
- Activation matrix: artifact absent -> computed; key mismatch -> hard
  error; `--no-ocean` -> neither; brotli / MLT / non-default level /
  library zoom range -> computed with the log line.
- Checkpoint: `--skip-to ocean`, `sort`, AND `assemble` each
  hard-reject on OceanMode mismatch.

Gates:
- `brokkr check`
- `brokkr tilegen --dataset denmark --variant locations` then
  `brokkr regress --dataset denmark` - tol 0, zero diffs. Load-bearing:
  denmark's bbox exercises band, sliver, splice, and artifact interior
  simultaneously. It proves geometry equality; byte/id equality is NOT
  claimed (D1 changed ids deliberately).
- `elivagar verify data/tilegen/denmark-<short-head>.pmtiles` - zero
  errors.
- From `scripts/validate/`: `node earcut-oracle.mjs
  ../../data/tilegen/denmark-<short-head>.pmtiles` - default mode, all
  polygon layers clean.
- Perf keep bound: `brokkr tilegen --bench 3 --dataset denmark
  --variant locations` <= 12.7s (12.3s baseline + 3%). Expected
  reading: a win equal to the interior share of the CURRENT ocean
  phase, read from the pre-landing sidecar split taken at the start of
  this brick - no number is promised here because the current split is
  unmeasured (v2's 10.5-11.5s figure was derived from a stale 13.7s-era
  split and is withdrawn). Above the bound: revert.
- Norway (coastal stress + the deferred activation regress in one).
  PREREQUISITE, user-gated: `brokkr bless --dataset norway --file
  data/tilegen/norway-20c8bd7.pmtiles` (the banked pre-enrichment
  baseline, xxhash `525c553a`; if archive rotation moved it, locate the
  file by that hash before blessing). Then `brokkr tilegen --bench 1
  --dataset norway --variant locations` and `brokkr regress --dataset
  norway` - zero diffs everywhere (ocean matches geometrically across
  the id change).
- H3 ledger entries from the norway sidecar: artifact runs-vector
  bytes, artifact mmap size, per-worker splice scratch.
- Measurement record at THIS boundary (contract: commit-anchored
  numbers per measured landing, not deferred to the user-gated brick
  6): the denmark bench-3 result and the norway readings above go into
  `reference/performance.md` as part of the brick 5 landing; brick 6
  adds only the NA rows.

### Brick 6 - NA reading and docs

User-gated NA run, flags MATCHED to the `88dc2385` baseline:
`brokkr tilegen --bench 1 --dataset north-america --variant locations
--compress-sort-chunks lz4`, then `brokkr sidecar <uuid printed by the
bench> --human` and `--stalls`. Readings vs `88dc2385`: ocean phase
(19.7s -> band-empty, expected <2s), peak-RSS phase composition,
assemble wall (reused-tile encode chains -> run copies), scratch bytes.

KEEP/REVERT: NA wall must come in BELOW 348.5s (same flags, same
`--bench 1` ladder discipline). At or above it, the planet claim failed
its slope check: brick 5 (the single behavior landing) is REVERTED,
bricks 1-4 are kept (neutral instruments + standalone artifact), and
the readings go to the roadmap for a redesign round. Docs ride along
with whichever verdict lands: AGENTS.md pipeline phases, roadmap H5
status block, `reference/performance.md` rows.

## Ordering and green boundaries

1 -> 2 -> 3 -> 4 -> 5 -> 6. Brick 1 is counters-only; brick 2 is
output-changing but regress-neutral with named unit tests; brick 3 is
instruments; brick 4 adds an object nothing consumes; brick 5 is the
single behavior landing; brick 6 is the scale verdict with an explicit
revert path back to brick 4 state. `brokkr check` + the denmark
verify/regress gates are green at every boundary.

## Stopping rule / out of scope

- No changes to OSM layer emission, phase12, or sort formats beyond the
  brick 1 stats hook, the D1 sink-kind parameter, and the D3 sink
  filter.
- The legacy non-partitioned assemble path never consumes the artifact.
- The virtual-planet serve path (`notes/virtual-planet-serving.md`)
  consumes the same artifact by design; its integration is a separate
  named TODO.
- Multi-member gzip concatenation for mixed tiles: REJECTED (decoder
  support unproven; mixed-tile count small).
- A last-tile encode memo for the computed band path (planetiler's
  `FeatureGroup.hasSameContents` trick): OUT OF SCOPE - it optimizes
  re-encoding runs of identical ocean tiles, which under this spec is
  precisely the work the artifact deletes; the band that remains is an
  extract-bbox perimeter, too small to memo. Recorded in case the band
  ever profiles hot.
- Brotli / MLT / non-default-level / library-zoom-range runs: computed
  path, fully supported, unchanged - the artifact simply does not
  activate (D6). No dual-format artifacts.

## Review resolutions

R4 (v4 -> v5, competitor comparison over research/): CONFIRMED -
planetiler's fill subsystem exists for this spec's exact thesis
(FeatureRenderer/TileArchiveWriter memoization comments), its PMTiles
run extension matches D5/add_run semantics byte-for-byte, its
skip-filled-tiles default-off confirms cheaper-never-absent,
tilemaker's config mirrors the simplified/full z-split, its constant
ocean attrs mirror D1, and the planetiler planet census (267M/38M)
brackets brick 4's bounds. NOVEL: no competitor caches ocean across
runs - the artifact has no precedent to borrow from, so this spec's
gates are the only validation. FOLDED: D1 gains the planetiler
dedup-gap justification; D5 gains the dedup-cap note (run copies
bypass it; planetiler's hash-shape predicate recorded as the candidate
if the roadmap's cap-pricing item ever demands action); brick 5 gains
the band-inner-seam test (planetiler's bad_polygon_fill guard
territory) and the antimeridian-column band test (extract bboxes
touching the world x-edges are band); brick 4 gains the fill-winding
eyeball check (CW exterior, matching planetiler's encodeFill).
DECLINED: last-tile encode memo for the band path (the artifact
deletes the work it optimizes; recorded in the stopping rule).

R1 (v1 -> v2), 19 findings: band predicate buffered/quantized/per-pass;
ocean ids surveyed + D1 added; precedence by band membership; partition
union; checkpoints; brotli matrix; RLE/dedup conditionality; reader
shape; metadata/hashing; run-aware copy in core; splice precision;
counter semantics; deterministic pricing; scalable gates; command
pinning; keep/revert bounds; concrete types.

R3 (v3 -> v4), 6 findings: (1) input filter reformulated on the
snapped z-top root range `root_fragments` actually selects - the raw
bbox test was unsound against root snapping; (2) band-aware recursive
`emit_full_subtree` + the same ownership check inside
`split_for_parallel` - the two paths that bypassed descent pruning;
(3) `OceanArtifactKey::from_inputs` pinned, the no-shapefile claim
narrowed to no-geometry-parsing (files are read once for hashing,
~1-2s, accounted); (4) RunCopy counter accounting pinned
(tiles/bytes/unique per zoom, TileSizeDiagnostics weighting,
DedupStats) plus zoom-boundary run splitting so runs are zoom-pure;
(5) brick 5 records its own performance.md rows at its boundary;
(6) below-cap equivalence wording corrected to "guaranteed when".
R3 also VERIFIED sound: cell monotonicity, the inner safety rect (no
half-unit residue), world-edge band emptiness, extract per-tile walk,
and the brick 2 -> 4 ordering via policy_version.

R2 (v2 -> v3), 14 findings: (1) traversal pruning added to D3 - the
compute claim is now structural, and the stale 5.6s-derived expectation
is withdrawn from brick 5; (2) inner safety rect closes the
float-vs-quantized sliver; (3) add_run respecified as run-local sharing
with pinned cap behavior + both-regime tests; (4) mixed-run band
subtraction resolved by the two-regime rule (world: no per-tile checks
exist; extract: per-tile walk, priced as acceptable); (5) partition
boundary clipping mandatory + UnionPartition/PartitionItem carrier
types + add_run returns bool for counters; (6) D1 mechanism pinned via
PyramidEmitKind + named unit tests + id-dependency inventory; (7)
compression level and zoom range join the key, activation matrix
restricted to the exact build shape; (8) `--skip-to assemble` included
in resume rejection; (9) brotli/MLT regeneration contradiction removed
- computed fallback is the complete policy; (10) commands pinned
(shapefile paths from detect_ocean, printed-value convention stated,
verify gates use `elivagar verify` on named paths); (11) brick 6 flags
matched to baseline + explicit revert semantics; (12) unit-test list
expanded to the failure boundaries; (13) brick 1 verdict includes the
record share; (14) OceanTiles::open full signature + hash wire encoding
+ open validations. Nits: brick-4 bound attribution fixed, "every
addressed tile", brick 3 revert rule added.
