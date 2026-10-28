# Hunt: the run contract, end to end

Scope: how a run's meaning flows from CLI flags into `TilegenConfig`, into the
resume checkpoint, into the provenance block, and on to what brokkr's corpus
gate compares. Files read: `src/main.rs`, `src/pipeline/mod.rs`,
`src/pipeline/assemble.rs` (config, provenance and compression sections),
`src/provenance.rs`, `src/ocean.rs` (keys, artifact open, pass grids),
`src/inspect.rs` (provenance printer), `src/sort.rs` (chunk magic),
`reference/corpus.md`, `corpus/denmark/contract.json`.

## Summary

No single place owns the question "what is this run". The answer gets rebuilt
at least five times, each with its own representation:

1. `main.rs` turns flags into a flat `TilegenConfig` of primitives
   (`Option<PathBuf>` x3 for ocean, `u32` level, `[u32; N]` caps, `f64`
   factor, `bool`s).
2. `pipeline::resolved_ocean_mode` derives `OceanMode` from that config and
   decides artifact eligibility.
3. `provenance::producer_config` picks the resume-relevant subset as a JSON
   `Value`.
4. `assemble::write_provenance` re-derives the ocean contract
   (`low_zoom_source`, the `mode` string) from the config again, and
   `provenance::config_json` picks the comparability subset as another JSON
   `Value`.
5. brokkr reads that JSON back and applies its own rules about which members
   gate. For example, `input.name` is warned about but not gated, a rule that
   lives only in brokkr and `reference/corpus.md`.

Every subset is chosen by hand in its own function, and they are held in step
only by doc comments. There are several concrete holes and lies (section A).
The fix is structural: one typed `RunSpec` resolved once from the CLI, with
the contract, producer and assemble views as typed projections of it
(section D).

## A. Concrete defects and disagreements found

A1. **The contract does not record computed-ocean shapefile identity.**
`OceanContract` carries `mode`, `low_zoom_source`, `runtime_simplification`
and `artifact_key`. In `mode = "shapefile"` the artifact key is `None`, so
two computed-ocean archives built from different water-polygon releases (or
under different `OCEAN_POLICY_VERSION`s) have equal contracts. The identity
already exists as `OceanSourceKey` in `OceanMode::Computed { key }`, and the
checkpoint records it. `RunProvenance` drops it: `ocean_artifact_key` is
populated only for `Band`. The gate is safe today only because the denmark
baseline happens to be artifact-active. Any computed-ocean corpus (raw
variant, another dataset, an artifact-absent A/B arm) has a contract that
cannot tell shapefile releases apart. Owner: the ocean member of the
contract should be `OceanMode` itself, serialized with its key in every arm.

A2. **MLT archives record `tile.compression: "gzip"`.** `config_json` writes
`tile_compression_name(config.tile_compression)` unconditionally. For
`--tile-format mlt`, `assemble.rs` sets `TileDataCompression::None` and
ignores `tile_compression` entirely, while the CLI default is `Gzip`. So every
MLT archive's contract states a compression it does not use. Nothing rejects
`--tile-format mlt --tile-compression brotli` either; the CLI help says "MVT
only" and that is the only enforcement. `base_compression_level` is recorded
for MLT too, where it means nothing. The root cause is that format and
compression are two independent enums, while the legal space is
`Mvt(Gzip|Brotli) | Mlt`. A `TileEncoding` enum shaped like that makes the lie
unconstructible.

A3. **The `compression_policy: "zoom-v1"` label is free text detached from
the policy.** `compression_level_for_zoom` (`assemble.rs`: z0-8 clamp to 9-10,
z13-14 cap at 3) is the policy. The label in `provenance::config_json` is a
string literal nothing ties to it. Change the function without editing the
string and the contract reads as unchanged. The same applies to the ocean
artifact: `OceanArtifactKey` carries `compression_level` but no
compression-policy version. An artifact built under an older per-zoom policy
still matches the key, and assemble splices its blobs next to tiles
compressed under the new policy. Owner: a `const COMPRESSION_POLICY` next to
the function, referenced by both the provenance block and the artifact key.

A4. **The "does the low-zoom pass exist and which grid owns it" decision
disagrees with itself.** Sites answering "how is z0-z14 split into ocean
passes":
- `main.rs::resolve_ocean_inputs` accepts only `z0-z14` or `z0-z7`+`z8-z14`.
- The `pipeline::run` ocean phase always runs a separate z0-z7 pass. It uses
  `max_zoom.min(7)`, sourced from the simplified shapefile or else from the
  full one (the v3 "split always happens" rule).
- `pipeline::ocean_pass_max_zooms` emits a low-pass grid only when
  `ocean_simplified_shapefile.is_some()`. That is the pre-v3 rule.
- `ocean::selected_pass_grid` picks the first grid only when
  `grids.len() == 2 && z <= 7`.
- `ocean_build` hard-codes `0, 7` and `8, 14`.
- `ocean::LOW_ZOOM_UNION_MAX_ZOOM` is a sixth statement of the 7.
- `assemble::write_provenance` computes `low_zoom_source`. Its comment says
  "mirrors ocean_pass_max_zooms".

Live disagreement: with `--ocean z0-z14:full.shp` plus an artifact, the ocean
phase runs a z0-z7 pass, but `ocean_pass_max_zooms` yields a single z14 grid.
`artifact.grids().first()` therefore hands the low pass a grid quantized at
z14, while that pass computes at a max zoom of 7. Band ownership
(`ocean_band_tile`, `pass_owns_root_range`) is then evaluated on a grid whose
`inner_rect` was ceil/floored at a different resolution from the pass. The
assemble side uses the same z14 grid via `selected_pass_grid`, so the two
sides at least agree with each other, but neither agrees with the pass that
produced the chunks. I could not prove a visible defect. It is an unadjudicated
path, and production always names the simplified shapefile, so the corpus never
exercises it. Owner: one `OceanPasses` value (a list of
`{zoom_range, source}`) built once by the CLI resolver and consumed by the
ocean phase, the grids, `selected_pass_grid`, `ocean_build` and provenance.
The z7/z8 split becomes one const that only that type sees.

A5. **Resume compatibility omits sort-chunk compression.** The checkpoint
records the input hash, the producer config, the ocean mode and the effective
paths, but not `compress_sort_chunks`. A `--skip-to sort` or `assemble` run
with a different value is caught late, by the per-file magic check in
`sort.rs`. That check produces an error message, not a resume refusal. A
`--skip-to ocean` run with a different value writes ocean chunks in a second
codec next to PBF chunks in the first. The magic check then fails at
sort-open, after the whole ocean phase has run. The chunk files already
self-describe through their magic, so the cleaner fix is to drop the flag from
the read path entirely: the reader detects the codec, and the flag becomes a
writer-only setting.

A6. **A missing `sort_chunks.count` is a warning, not a refusal.**
`load_sort_chunk_count` prints "cannot verify chunk integrity" and continues.
That file is the only evidence that the ocean phase finished before a
`--skip-to sort|assemble`. The checkpoint is written before the ocean phase
starts, so an ocean phase that crashed midway leaves a valid-looking
checkpoint. A resume then assembles a partial ocean and stamps a full
contract on it. The count belongs inside the versioned checkpoint, as a
phase-completion record, not in a sidecar text file whose absence is
tolerated.

A7. **Two readers of the provenance schema classify "no contract"
differently.** `provenance::extract_contract` (brokkr's entry point) returns
`Incomplete("build")` when `build` is missing. `inspect::print_provenance`
hand-walks the same JSON and treats `build` as optional. The two also differ in
what counts as "invalid". `ContractState::Unavailable(String)` is never
constructed by `extract_contract`, so whoever produces it lives elsewhere.
`inspect.rs` should consume `ContractState`, not reparse.

A8. **Ocean mode is checked twice on `--skip-to ocean`.** Once in the
up-front resume block and once more in the `--skip-to ocean` arm, with
different error prose. The checkpoint is also loaded three or four times per
run (`load_checkpoint` returns a 4-tuple and every caller discards three
fields). This is harmless today, but it is the shape that lets one site get
updated and not the other.

A9. **The `OceanSourceKey` vs `OceanArtifactKey` rationale is asserted in one
place and contradicted in another.** The `OceanSourceKey` doc says
compression level must not gate a resume, because chunks hold geometry and
not encoded tiles. The `OceanMode::Band { key: OceanArtifactKey }` arm compares
a key that includes `compression_level`. So for artifact-active runs, a
`--skip-to` with a different level is refused by the chunk-identity check.
The refusal is moot in practice only because `resolved_ocean_mode` already
refuses an artifact/level mismatch. But the checkpoint's ocean identity is the
wrong key: the band chunks are a function of the source key plus the band
geometry, not of the artifact's encoding.

A10. **Doc drift.** `reference/corpus.md` lists "seam" among the contract
fields. There is no seam member in `config_json`. Also,
`provenance::Effective`'s doc says the member is omitted only for `--skip-to`
without a checkpoint, while inspect's tests say "effective is absent on an
ocean-artifact build". `ocean_build` passes `RunProvenance::default()` and
`pbf_path: PathBuf::new()`, so `write_provenance` bails on "no PBF file name"
and the artifact carries no `elivagar` block at all. That is reported to
stderr only.

## B. Axes that should be types

- **Ocean input.** Three `Option<PathBuf>` fields plus
  `ocean_artifact_key: Option<OceanArtifactKey>` plus
  `ocean_only_metadata: bool` on `TilegenConfig`. Illegal states are
  representable and partly guarded at runtime: a simplified shapefile without
  a full one, an artifact without a shapefile (rejected only in `main.rs`, so
  a library caller can construct it), and `ocean_artifact_key` set on a
  normal run. `OceanInputs` in `main.rs` is the right idea, but it is private
  to the binary and flattened again on the way in. Proposed:
  `enum OceanSpec { None, Shapefiles(OceanPasses), WithArtifact(OceanPasses, PathBuf) }`
  in the library. The CLI parser produces it, and `resolve_ocean_inputs`
  becomes `OceanSpec::from_str`.
- **Builder mode vs run mode.** `ocean_artifact_key` and `ocean_only_metadata`
  exist only so `ocean_build` can reuse `phase_assemble` by faking a
  `TilegenConfig` with an empty `pbf_path`. That should be an
  `ArchiveRole { Run(RunProvenance), OceanArtifact(OceanArtifactKey) }`
  argument to assemble, not two config fields every caller must zero.
- **Tile encoding.** See A2: `enum TileEncoding { Mvt(MvtCompression, Level), Mlt }`.
  The artifact eligibility check (`format == Mvt && compression == Gzip`)
  becomes a pattern match on that type.
- **Zoom range.** `min_zoom: u8, max_zoom: u8` is validated in `pipeline::run`
  (`<= 14`, ordered), and then `0..=14` is assumed again in
  `OceanArtifactKey` (`min_zoom`/`max_zoom` fields that are always 0/14),
  `OceanTiles::open`, `resolved_ocean_mode`, `ocean_build` and the CLI (which
  hard-codes 0/14 with no flag). A `ZoomRange` newtype with a `FULL` const
  removes the scattered literals. The artifact key's two zoom fields carry no
  information, and the key could drop them at the next policy bump.
- **Content hashes.** `u128` in the ocean keys, lowercase-hex `String` in
  `provenance::Input`, `String` in `CheckpointProvenance`. There are two
  `hash_file` implementations (provenance: 8 MiB buffer, returns `(String,
  u64)`; ocean: 1 MiB, returns `u128`) and two hex validators (artifact
  `from_json` checks length and lowercase; source `from_json` accepts any
  radix-16 string). One `Xxh128` newtype with one `Display` and one `FromStr`
  would replace all of them.
- **Contract strings.** `OceanContract::mode`, `low_zoom_source`, and the
  `Effective` fields are `&'static str`, and the checkpoint loader re-interns
  them against hand-maintained allow-lists (`["inline", "node_store"]`,
  ...). Enums with serde renames would make the checkpoint loader and
  provenance writer share one vocabulary. `RunProvenance::resumed_from:
  Option<&'static str>` is the same: it should be `Option<SkipTo>`.
- **Contract and checkpoint as `serde_json::Value`.** `producer_config` is
  compared as JSON "so the float needs no float-equality dance". The cost is
  that the producer subset and the contract subset are two hand-written
  `json!` literals that must list the same fields. Typed structs with
  `#[derive(Serialize, Deserialize, PartialEq)]` (the factor as a validated
  newtype holding the `f64` bits) get field-named diffs for free, and the
  contract becomes `Contract { input, config: ContractConfig }` with
  `ProducerConfig` as a field of `ContractConfig`. Then the producer subset
  is a subset by construction, not by keeping two literals in step.
- **Ocean key JSON.** `OceanArtifactKey::json()` is a hand-formatted string.
  It is parsed back into a `Value` in three places (`save_checkpoint`,
  `RunProvenance` construction, `write_provenance`), each silently `.ok()`-ing
  a parse failure into `None`. In `write_provenance`, a failure would silently
  drop the artifact key from the contract.
- **Errors.** `PipelineError(String)` carries every refusal: contract-level
  ("artifact cannot serve this run"), resume-level ("checkpoint built from a
  different PBF") and I/O. brokkr and scripts can only grep prose. A
  `RunRefusal` enum (`ArtifactIneligible { field }`,
  `ArtifactKeyMismatch { field }`, `ResumeInputMismatch`,
  `ResumeProducerMismatch { fields }`, `ResumeOceanMismatch`,
  `ResumeIncomplete`) with a distinct exit code would let brokkr's
  `--dry-run` and queued scripts branch on cause. `OceanTiles::open` returns
  `io::Error::other` for key mismatch without naming which hash differs, even
  though the struct makes that trivial.
- **CLI-only validation.** The `polygon_simplify_factor` range (0.1-10.0),
  the fanout-cap parsing, and the MLT feature-gate check all live in
  `main.rs::run` and `process::exit` from inside a struct literal. The library
  `TilegenConfig` accepts any `f64`, including NaN, which would then be
  serialized into the contract. These belong in the constructors of the
  library types.

## C. Decisions made in more than one place

| Question | Sites | Disagree today? | Owner |
|---|---|---|---|
| Which ocean passes exist (the z7/z8 split) | `main::resolve_ocean_inputs`, `pipeline::run` ocean phase, `ocean_pass_max_zooms`, `ocean::selected_pass_grid`, `ocean_build`, `LOW_ZOOM_UNION_MAX_ZOOM`, `write_provenance::low_zoom_source` | Yes (A4) | `OceanPasses` type in `ocean.rs` |
| May the artifact serve this run | `resolved_ocean_mode` (format, compression, zooms, level), `OceanTiles::open` (header type 1, compression 2, z0-14, world bounds, layer list, key), `ocean_build` (what it writes), `main::resolve_ocean_inputs` (artifact needs a shapefile) | Agree, held by hand; `open` uses magic ints `1`/`2` | One `ArtifactContract` const describing the encoding+zoom space, used by build, eligibility and open |
| Which settings form the comparability contract | `provenance::config_json`, `provenance::Input::to_json`, brokkr's name-is-not-gated rule, `reference/corpus.md` prose | Doc lists "seam" (A10); contract omits computed-ocean source (A1) | Typed `Contract` in `provenance.rs`, including a `gated()` projection brokkr calls instead of applying its own exceptions |
| Which settings a resume may not change | `producer_config`, the checkpoint `ocean` member (`OceanMode`), `input_xxh3_128`, sort-chunk magic | Missing `compress_sort_chunks` (A5) and phase completion (A6) | `Checkpoint` struct owning all of it; `ProducerConfig` embedded in `Contract` |
| Is the ocean contract `artifact`, `shapefile` or `none` | `OceanMode` and the string map in `pipeline::run`, `write_provenance` comment "ocean runs only when the full shapefile is present" | Agree | `OceanMode` serializes itself |
| Per-zoom compression level | `compression_level_for_zoom`, the `"zoom-v1"` literal, the artifact key's `compression_level` | Unlinked (A3) | Policy const plus version |
| Is a provenance block a contract | `extract_contract`, `inspect::print_provenance` | Yes, on `build` (A7) | `extract_contract`, with inspect rendering `ContractState` |
| Default budgets (sort 1G, assemble 32M, way 128M/4G, dedup 1M) | `0`-sentinels in `TilegenConfig` resolved in `pipeline::run`, `assemble.rs`, `phase12`, `pmtiles_writer`; CLI help strings restate them | Agree | Not contract (correctly), but the `0 = default` sentinel should be `Option` or resolved values |

Not a duplicate: the compression-level check appears in both
`resolved_ocean_mode` (`declared_key`) and `OceanTiles::open` (full key
equality). The first exists to give a better error message, so this is one
answer confirmed twice.

## D. Structure

- **`TilegenConfig` is three things in one struct**: the contract (zooms,
  encoding, caps, factor, ocean spec), operational knobs (budgets, threads,
  `in_memory`, `tmp_dir`, `force_sorted`, `allow_unsafe_flat_index`,
  `compress_sort_chunks`), and the per-invocation action (`skip_to`,
  `output_path`), plus two builder-mode fields. Proposed split:
  `RunSpec { input: PathBuf, contract: ContractConfig }`,
  `Resources { ... }`, and `Action { output, skip_to }`. Then
  `provenance::config_json` is `serde_json::to_value(&spec.contract)`, and a
  new contract-relevant setting is impossible to add without it landing in
  provenance. Today a new field defaults to "not in contract" silently. That
  is the dangerous default.
- **`provenance.rs` depends on `TilegenConfig`**, and picks fields out of it.
  The dependency should point the other way: the config is built from typed
  contract parts that `provenance.rs` owns, and the pipeline reads them.
- **The checkpoint lives in `pipeline/mod.rs`** as ad-hoc `json!` plus a
  4-tuple loader. It deserves its own module (`pipeline/checkpoint.rs`) with
  a `Checkpoint` struct, and a single `Checkpoint::admits(&RunSpec) ->
  Result<(), ResumeRefusal>` that holds every resume decision now spread
  across the top of `run()` and the `--skip-to ocean` arm.
- **Ocean resolution is split across three modules.** Spec parsing is in
  `main.rs` (unreachable from the library and from brokkr's `--dry-run`),
  eligibility is in `pipeline/mod.rs`, and the key types and artifact
  open-checks are in `ocean.rs`. They should be one `ocean::spec` module:
  parse, then resolve against files, then a `ResolvedOcean` that carries keys
  and passes.
- **`ocean_build` should be a sibling entry point, not a faked run.** It
  builds a `TilegenConfig` with an empty PBF path and zeroed fields to reach
  `phase_assemble`, and duplicates the pass structure by hand (A4). Factoring
  "chunks to archive" out of assemble with an `ArchiveRole` argument removes
  the fake and the builder-only config fields.
- **brokkr owns gating policy over a schema elivagar defines.**
  `ContractDoc { input: Value, config: Value, build: Value }` exports raw
  JSON, and brokkr decides that `input.name` is diagnostic. That rule is a
  property of the schema and belongs next to `SCHEMA_VERSION`. Export a typed
  `Contract` with a `gated_eq` or `diff` function, so the "name never gates"
  rule documented in `provenance::Input` is enforced by the type that states
  it.
- **Tests mirror the accident.** `main.rs` tests the ocean partition rules
  because that is where the parser lives. `pipeline_tests.rs` constructs
  `TilegenConfig` literals by hand (seven-plus copies, each listing every
  field). There is no test that `producer_config`'s fields are a subset of
  `config_json`'s, or that `ocean_pass_max_zooms` agrees with the ocean
  phase's pass loop. That is exactly the drift in A4. With the types above,
  most of these become compile-time facts. A default-able `RunSpec` builder
  removes the literal copies.

## E. Lateral notes

- `write_provenance` silently omits the whole block when the PBF has no file
  name or no identity, with only a stderr warning. A fully built archive with
  no contract. brokkr does report `Absent`, so it is caught downstream, but
  the producer should fail the run instead.
- `--locations-on-ways` and `--force-sorted` change the code path, and the
  contract records only the header features. That is correct by the
  documented rule (they land in `effective`), but a forced
  `--locations-on-ways` on a header-less PBF yields an archive whose
  `input.features.locations_on_ways` is false while `effective` says
  `inline`. Worth a sentence in `reference/cli.md`.
- `compression_level` is in the contract, but the corpus digest is over
  decoded content (per `reference/corpus.md`). A level change therefore
  refuses a comparison whose content is guaranteed equal. This is defensible
  as strictness, but it is a choice nobody has written down.
