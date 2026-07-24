# PMTiles archive metadata: the `elivagar` provenance block

Every archive elivagar writes carries a record of the contract it was built
under, as a top-level `elivagar` member of the PMTiles metadata JSON.

Read it with `scripts/dump-pmtiles-metadata.py <archive> --key elivagar`.
`elivagar inspect` renders only the fields it knows about (layer list, zoom
range) and will not show this block.

## Why it exists

A prose rule already said not to compare across dataset variants.
`reference/technical-implementation-spec.md` states it plainly, and names the
2026-07-09 false alarm that "burned a session tail proving the code innocent".

On 2026-07-14 the same thing happened again. A denmark archive blessed from a
`--variant locations` build was compared against a default-variant (raw) build.
`brokkr regress` reported 363,620 structural moves across every layer, which
was read as a code regression and investigated as one, at length, in the wrong
subsystem. The two builds were both correct.

The rule was five days old and written down. It failed twice because it asks a
human to remember an invariant at exactly the moment they are focused on
something else, and because nothing in the artifacts could contradict them.

That is what this block changes. The archive states its own contract, so a
consumer can refuse an incomparable diff instead of reporting it as a finding.
A rule that only exists as prose is a rule that gets broken on a busy
afternoon; the same rule enforced against recorded facts is not.

## The split, which is the whole design

Groups get opposite treatment, and getting this backwards inverts the gate.

**`input` and `config` are the comparability contract.** They describe the work
requested. Two archives whose contract differs describe different work, so a
geometry diff between them says nothing about the code. A consumer compares
these first and refuses the diff on mismatch.

**`effective`, `build` and `execution` are diagnostic and must never be
equality-gated.** Given identical `input` and `config`, `effective` is a pure
function of the code, and `build` *is* the code. Regression testing exists to
compare revisions, so gating on either would refuse precisely the comparisons
the gate is for. Change how the pipeline selects a path and a contract-gated
consumer would refuse the diff exactly when it matters most.

These groups are not ranked by importance. `effective` is the most
*explanatory* thing in the block - it is what tells you why a diff exists once
the contract matches - and it is still not part of the gate.

## Reproducibility constraint

Nothing in this block may vary between two builds of the same commit on the
same input.

No wall-clock, no hostname, no absolute paths, no thread counts, no timings, no
measured durations, no environment-order-dependent values. Same-commit builds
are byte-identical (AGENTS.md) and the corpus digest and contract are
recomputed from archive content; a per-run value here would break both.

Anything that fails this test does not belong in the archive. It belongs in
`.brokkr/sidecar.db`, which exists for exactly that class of data.

## Labels versus identities

The governing distinction. A label describes; an identity pins.

- `input.name` is a **label**. Never gate on it. Two files named for the same
  region and built at the same commit can be entirely different contracts,
  which is how 2026-07-14 happened.
- `input.xxh3_128` is the **identity**. Gate on this.
- `input.features` are **observed**, read from the PBF header rather than
  inferred from a name, and they determine which paths the pipeline takes.
- `build.*.commit` is only an identity when `dirty` is `false`. A dirty tree
  means the commit names the nearest ancestor of the code that ran, not the
  code that ran.

The same lesson applied to dependencies. protohoggr was a path dependency
labelled `0.4.0` while a *second*, content-checksummed `0.4.0` came from the
registry via pbfhogg - two crates, one label, no assertion that they matched.
Pinning it to the registry version made the lockfile checksum its identity.

## Schema

`schema` is the version. Bump it when the meaning of an existing field changes.
Adding a member does not require a bump: readers ignore unknown members, and
the contract comparison is defined over named fields, not the whole object.

```json
{
  "elivagar": {
    "schema": 1,

    "input": {
      "name": "denmark-20260220-seq4704-locations-prepass.osm.pbf",
      "xxh3_128": "58c47f32d3a55b04a56813565efc78ac",
      "bytes": 531544047,
      "replication_timestamp": 1771622445,
      "features": {
        "sort_type_then_id": true,
        "locations_on_ways": true,
        "way_members_v1": true,
        "shared_node_pins_v1": true
      }
    },

    "config": {
      "profile": "shortbread",
      "min_zoom": 0,
      "max_zoom": 14,
      "tile": {
        "format": "mvt",
        "compression": "gzip",
        "base_compression_level": 6,
        "compression_policy": "zoom-v1"
      },
      "seam_reconcile_layers": { "boundaries": 8 },
      "fanout_caps": {},
      "polygon_simplify_factor": 1.0,
      "ocean": {
        "mode": "artifact",
        "runtime_simplification": true,
        "low_zoom_source": "simplified",
        "artifact_key": { }
      }
    },

    "effective": {
      "coordinate_source": "inline",
      "way_members": "injected_v1",
      "shared_node_pins": "injected_v1"
    },

    "build": {
      "elivagar":       { "commit": "ec5bd11...", "dirty": true },
      "pbfhogg_reader": { "version": "0.5.0" },
      "cargo_lock_xxh3_128": "f850fa82608190a904500b37810e3def",
      "cargo_features": ["default", "mimalloc-alloc"]
    },

    "execution": { "resumed_from": null }
  }
}
```

### Field notes

`input.xxh3_128` matches the `xxhash` brokkr records per dataset variant in
`brokkr.toml`, byte for byte, with no coordination between the two - the same
XXH3-128 over the same bytes. They can be compared directly.

`config.tile.base_compression_level` is a **base**, not a uniform setting: the
encoder clamps low zooms up and caps z13/z14 down. Recording the level alone
would misdescribe the output, so `compression_policy` names and versions the
mapping from base to per-zoom level.

`config.ocean.mode` is the **resolved** mode, taken from `OceanMode` rather
than inferred from which config paths are set. Those differ: the ocean phase
runs only when the full shapefile is present, so a simplified-only config
produces no ocean and correctly reports `none`. It distinguishes `artifact`
from `shapefile` and is part of the contract: artifact-served tiles are not
geometry-identical to extract-computed ones (descent seams depend on each
piece's clip extent - see the extract-consumption note in
`src/pipeline/mod.rs`). Comparing an artifact-active archive against an
artifact-absent one is a contract mismatch, not a regression.

`config.ocean.low_zoom_source` is independent of `runtime_simplification`.
Source selection (a separate simplified shapefile serving z0-7) and runtime
simplification (`--no-ocean-simplify`) are different things; a single boolean
conflates them.

`effective.shared_node_pins` is the largest of the three effective paths in
blast radius. Pin scope changes which vertices survive simplification, so
`injected_v1` and `block_local` produce materially different geometry from the
same OSM data.

`build.pbfhogg_reader` is the pbfhogg **linked as our reader**, not the
pbfhogg that produced the input PBF. Those can differ. The producer is not
recorded (see gaps); the input hash is authoritative regardless. It is a
pinned registry dependency now, so it records its locked semver `version`
rather than a `commit`/`dirty` pair: a registry dependency has no git tree to
be dirty against, and `cargo_lock_xxh3_128` already pins its exact checksum -
the version is the reader-legible half of that pin.

`build` has no protohoggr entry by design. It is a pinned registry dependency,
identified exactly by the content-addressed checksum in `cargo_lock_xxh3_128`,
and cannot drift. pbfhogg is likewise pinned there; it keeps a named entry only
to surface its human-readable version. Only path dependencies would need a
commit/dirty pair, and there are none.

`execution.resumed_from` names the phase a `--skip-to` run began at, or is
`null` for a full run.

## Checkpoint provenance

`--skip-to` reuses sort chunks from a previous run, and nothing in a later
invocation reveals what produced them. The v5 checkpoint (JSON) records the
input hash, the producer config, and the effective paths, and a resume
validates the first two.

**Input hash.** A mismatch is a hard error: resuming would mix two inputs into
one archive and stamp it with provenance describing only the second.

**Producer config.** The subset of settings that decide what phase12 writes
into chunks - zoom range, fanout caps, `polygon_simplify_factor`,
`seam_reconcile_layers`. Chunks cannot be reinterpreted under different values,
so a mismatch is a hard error naming the field. It deliberately excludes the
assemble-side settings (tile format, tile compression, compression level,
memory budgets), which are applied after the chunks are read and may
legitimately change on a resume; including them would reject safe resumes.

**Effective paths.** A matching resume inherits them rather than re-deriving,
because re-derivation would describe what *this* invocation would have done,
not what produced the chunks on disk.

v4 and earlier are rejected; re-run a full tilegen. v4 also used a
whitespace-positional encoding where each added field shifted an index and a
miscount was a silent misparse - which is why v5 is JSON.

## Where the input hash is taken

Identifying the input means reading all of it, so placement is a performance
decision, not a detail. A full run hashes **after phase12**, when the PBF is
warm in page cache from the read that just finished. A resume hashes up front
because it must validate before proceeding, and can afford to because phase12
never runs and so never reads the file again.

Hashing before phase12 on a full run would put a cold serial pass in front of
the reader and, at planet scale, cost a second whole-file read plus the page
cache it evicts.

## Freshness, and why absent beats stale

Provenance that lies is worse than provenance that is absent, because it
invites a reader to attribute a diff to code that was never linked. Every value
is therefore either established or emitted as `unknown`, and nothing is
guessed. Where the input cannot be hashed, the entire block is omitted rather
than written partially.

`build.rs` is where this is hardest. Cargo reruns a build script only when a
declared input changes, but relinks the crate whenever a path dependency's
sources change - so without care, editing pbfhogg would rebuild elivagar while
the script kept reporting the previous commit and dirty state. The
`rerun-if-changed` declarations cover both what git records (HEAD, and the ref
it points into) and what git does not (uncommitted edits, via the source trees
themselves).

**This mechanism is not verified.** Confirming it means editing pbfhogg to
observe whether the script reruns. Treat a `build` block as strong evidence,
not proof, until it is.

`dirty` uses brokkr's definition, not git's: `*.md` and `.brokkr/results.db`
are ignored. A dirty `results.db` is this repo's expected state and markdown
never affects the binary, so counting either would mark nearly every build
dirty and make the flag meaningless.

## Consumer contract

A consumer comparing two archives:

1. Compare `input` and `config`. On mismatch, **refuse** the geometry
   comparison and report which field differs. Do not emit a diff.
2. Report `build` differences. Never gate on them.
3. Surface `effective` when explaining a diff that survived step 1.

An archive with no `elivagar` member predates this schema or failed to identify
its input. It cannot be gated, and a consumer should say so rather than
assuming comparability.

## Known gaps

Recorded here so they are not rediscovered as bugs:

- **Enforcement landed for the standing gate only.** `brokkr pmtiles-corpus
  check` (native brokkr code since the 2026-07-24 corpus redesign, decoding
  this block through the linked crate) compares the committed `contract.json`
  against this block before reading content and refuses with the differing
  field named, including the calibrated stale-artifact mismatch at
  `config.ocean.artifact_key.policy_version`. `brokkr regress` still reads
  nothing here: an ad-hoc two-archive diff can be run across incomparable
  archives, so establish comparability with `elivagar inspect` first.
- `input.replication_sequence` is not recorded (the pbfhogg header API for it
  is unconfirmed). `replication_timestamp` is.
- `config.ocean` records shapefile hashes only via `artifact_key`, so a
  `shapefile`-mode archive does not identify its shapefiles.
- `data_bounds` is not recorded. It is derived from the input, and it drives
  ocean selection, so it explains ocean-layer diffs between variants. Note it
  is **derived**, so it is diagnostic, not contract.
- `pbf_generator` (the pbfhogg revision that produced an injected PBF) is not
  recorded. It could be stamped into the header by pbfhogg at write time.
- No `tileset_digest`. Adding provenance makes archives byte-different across
  commits even when every tile is identical; a digest over ordered tile IDs and
  uncompressed payloads, excluding metadata, would restore a cheap
  tiles-identical check. Regress already establishes tile equality per blob, so
  this is a convenience.
