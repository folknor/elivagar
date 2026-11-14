# Run plan: typed request, validated plan, separate artifact role

The bundle of the structural findings on the flat `TilegenConfig`, the
CLI-only domain validation in `main.rs`, and the ocean artifact builder that
faked a tilegen run. It is a chain. Link 1 landed the artifact role split;
links 2 and 3 remain, then the green agent.

## Settled rulings (sparred, do not reopen)

1. Shape A: `TilegenConfig` stays the public caller-facing request with
   plain fields, retyped where a type can carry an invariant honestly. It is
   a typed request plus a validated execution plan, not a config that makes
   every invalid combination unrepresentable: reversed zooms, MLT with
   brotli, sub-minimum budgets and an artifact the window or encoding cannot
   use stay representable and are refused by fallible library resolution
   before any phase work or scratch mutation. Rejected: a private-field
   builder replacing the DTO (it would need the same exhaustive destructure
   over its own storage to keep classification, and fights the u8 zoom
   ruling), and a validation-only pass that leaves the builder flags and NaN
   factor in the type.
2. The ocean artifact builder is an assembly role, not a run.
   `AssembleSpec::{tileset, ocean_artifact}` in `pipeline/assemble/mod.rs`
   are the only constructors; the artifact arm derives its whole tile
   contract (MVT, gzip at the key's level, z0-z14, uncompressed chunks,
   streamed) from the key and refuses a key naming another level or zoom
   range; the tileset arm requires the input identity and refuses ocean facts
   that disagree (admitted tiles exactly in artifact mode under the same key;
   low-zoom source present exactly with ocean). Landed in link 1.
3. The top-level `ocean_artifact` metadata member is not folded under
   `elivagar`, and artifacts keep carrying no run provenance. Folding it buys
   a taxonomy change at the price of an `OCEAN_POLICY_VERSION` bump, a world
   artifact rebuild and a corpus rotation; the reason already sits at the
   top of `src/provenance.rs`.
4. Classification stays at `provenance::ContractConfig::new`, and when a
   field becomes a nested type (ocean inputs, fanout caps, simplify factor)
   that site destructures the nested type exhaustively too, or the
   compile-time classification property is lost.
5. Execution and provenance share one resolved value: the producer config
   phase12 consumes, the checkpoint serializes (still JSON) and the archive
   records is the same value, and likewise the tile contract and the window.
   Resume keeps the split: producer settings may not change, tile encoding
   may.
6. Serialized bytes do not move: the simplify factor stores the caller's f64
   unchanged and serializes as the same JSON number; fanout caps serialize as
   supported capped layer names only; the ocean input roles are kept as
   named, including `z0-z7:<full> z8-z14:<full>` recording simplified hashes.
7. The NaN simplify factor and library-side infinities are refused once the
   range check moves into the library. That is an intended behaviour change,
   not an output change (the CLI already refused infinities, and NaN was
   never a meaningful factor).
8. The missing-`mlt`-feature refusal moves into library resolution; today the
   library reaches the first encoded batch before refusing.
9. Fanout caps get private storage with checked construction: zero means
   uncapped, the default applies only to layers that support caps, whitespace
   is trimmed, the last override wins, unsupported and unknown layers are
   errors.

## Link plan

- Link 1 (landed): `AssembleSpec` / `ArchiveRole`, `ocean_build` no longer
  builds a `TilegenConfig`, `ocean_artifact_key` and `ocean_only_metadata`
  removed from the public config, `Resources::resolve_request`, scheduling
  takes the tile contract and chunk codec instead of the config,
  `OceanTiles` carries its admitted key. Output-neutral; the synthetic ocean
  golden pins the artifact bytes.
- Link 2 (medium, about 500 lines): library validation types. Public
  `OceanInputs` (full required, simplified and artifact optional) with the
  `--ocean` spec parser and partition rule moved out of `main.rs`, its tests
  moving with it, and `ocean-build` rejecting an artifact spec through it;
  `SimplifyFactor` newtype; `FanoutCaps` with private storage and the
  `--fanout-cap` parser using `FromStr for Layer`; the MLT feature refusal in
  the library; `main.rs` reduced to clap structs and one mapping, its `*Arg`
  enums either removed by deriving `ValueEnum` behind clap in the binary's
  mapping or kept in one place. Update `reference/cli.md` library-callers
  paragraph, `docs/guide/install.md`, the lib.rs doctest, CHANGELOG, and the
  brokkr#119 comment.
- Link 3 (medium, about 400 lines): the crate-private resolved `RunPlan`
  built once at the top of `run` (window, producer, tile contract, resolved
  ocean, resources, paths and execution), with phase12, assembly, the
  checkpoint and provenance taking resolved parts; `ArchiveRole::Tileset`
  stops carrying `&TilegenConfig`; `TileContract::from_config`,
  `configured_tile_contract` and `zoom_window(&TilegenConfig)` calls collapse
  into the plan. `reference/corpus.md` classification sentence updated if the
  destructured source changes.
- Link 4: the green agent.
