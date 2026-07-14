# The elivagar CLI

The complete command-line surface. This document is the reference for what
elivagar accepts; `AGENTS.md` summarises the commands a session reaches for
most, and defers here for the full set.

Note that most work in this repo does not invoke `elivagar` directly - `brokkr`
wraps the measured paths (`brokkr tilegen`, `brokkr regress`, `brokkr bless`,
`brokkr pmtiles-inspect`) and records provenance that a raw invocation does
not. Reach for the raw binary when there is no wrapper for what you need.

## The rule that shapes this surface

**Either it is explicit, or it is not set.** Nothing is inferred from the
filesystem. A run's behaviour is a function of its arguments and its input, and
of nothing else.

This is not a style preference; it was bought with a specific failure. The
ocean inputs used to be auto-detected from `data/`: the full shapefile, the
simplified shapefile, and `data/ocean-tiles.pmtiles`. Two consequences
followed, and both bit.

The run's meaning lived in the filesystem rather than the invocation. Two runs
of the same binary on the same PBF produced different ocean geometry depending
on whether a file happened to exist, with nothing in the recorded command line
saying which. `.brokkr/results.db` stores the literal subprocess invocation in
`cli_args`, so no bench row could be classified after the fact as
artifact-active or computed - the comparison simply could not be made from the
record.

And absence was silent. On 2026-07-14 a denmark archive was built, verified and
blessed as the regress baseline while `data/ocean-tiles.pmtiles` was missing,
so it took the computed path throughout while every gate passed. The baseline
was wrong about the thing it was baselining, and nothing said so.

Hence: no auto-detection, `--ocean` names every ocean input, omitting it means
no ocean, and a named input that cannot be honoured is an error rather than a
fallback.

## `elivagar run <INPUT> -o <OUTPUT> [OPTIONS]`

Generate a PMTiles archive from an OSM PBF.

### Input and output

| flag | meaning |
|---|---|
| `<INPUT>` | input OSM PBF (positional, required) |
| `-o`, `--output <PATH>` | output PMTiles path (required) |
| `--tmp-dir <PATH>` | sort chunks and intermediates. Default `data/tilegen_tmp` |

### Ocean

`--ocean <SPEC>` is repeatable and is the only ocean input. **Omit it entirely
for no ocean.** Each value is one of:

| spec | meaning |
|---|---|
| `z0-z14:<file.shp>` | one shapefile serves every zoom |
| `z0-z7:<file.shp>` | low-zoom shapefile (the pre-generalized one) |
| `z8-z14:<file.shp>` | full-resolution shapefile |
| `<file.pmtiles>` | precomputed world-ocean artifact |

Shapefile entries must partition z0-z14 exactly, and the only accepted
partitions are a single `z0-z14` or the `z0-z7` + `z8-z14` pair. That is not
arbitrary: `ocean::selected_pass_grid` implements one split, at z7/z8, and
nowhere else. A `z0-z5` request is rejected rather than accepted and quietly
served at z7, because the alternative is a false statement in the recorded
invocation - which is the failure this flag exists to prevent.

The two shapefiles are the OSM standard datasets from
osmdata.openstreetmap.de - same coastlines, same projection (EPSG:3857),
differing in vertex density. At z0-z7 the whole world is at most 256x128
pixels, so full-resolution coastline vertices are overwhelmingly sub-pixel:
the split exists to avoid paying for detail that cannot be seen. It is an
optimization, not a correctness requirement, and `z0-z14:<full>` alone is a
legal and slower way to say the same thing.

**The `.pmtiles` artifact is a cache over the shapefiles, not a substitute for
them**, and is rejected on its own. Two things force this. An extract still
computes its boundary band near the bbox edge from the shapefiles and takes
only the interior from the artifact (the band is empty only at world bounds).
And the artifact's key is validated by re-hashing the shapefiles it claims to
have been built from, so both sides must be present for the check to mean
anything. Build one with `ocean-build`.

A named artifact that cannot serve the run is an **error**. It applies only to
the exact MVT + gzip + z0-14 contract at a matching compression level, and a
mismatch on any of those four fails the run rather than quietly computing the
ocean instead. This used to print `Ocean artifact inactive` to stderr and carry
on - and brokkr reads nothing from tilegen's stderr under `--bench`, so a
measured run could switch ocean paths on four separate conditions leaving no
trace in the results row, the sidecar, or the archive. If you want the computed
path, say so by dropping the artifact from `--ocean`.

Example, the production shape:

```
--ocean z0-z7:data/simplified-water-polygons-split-3857/simplified_water_polygons.shp
--ocean z8-z14:data/water-polygons-split-3857/water_polygons.shp
--ocean data/ocean-tiles.pmtiles
```

### Pipeline

| flag | meaning |
|---|---|
| `--skip-to <ocean\|sort\|assemble>` | resume from a checkpoint |
| `--in-memory` | keep the tile blob in RAM. Faster for small extracts |
| `--compression-level <0-10>` | gzip level. Default 6 |
| `--tile-format <mvt\|mlt>` | payload format. Default `mvt` |
| `--tile-compression <gzip\|brotli>` | MVT only. Default `gzip` |
| `--compress-sort-chunks <lz4\|snappy>` | compress sort chunks: less disk I/O, more CPU. Wanted at planet scale, where scratch exceeds RAM |
| `-j`, `--threads <N>` | rayon pool and pbfhogg decode pool. Default: logical CPUs |

A resume validates the checkpoint's input identity, producer config, and ocean
source against the current run and refuses a mismatch, so `--skip-to` cannot
silently blend two contracts into one archive.

The ocean half of that is checkpoint v6 (2026-07-14). v5 recorded computed
ocean as the bare string `computed`, naming no source, so chunks built from one
shapefile could be reused by a resume naming another while the archive's
metadata described the second - the artifact arm had carried its key since it
was introduced and the computed arm simply had not. Both arms now carry their
producer's identity: shapefile content hashes plus the zoom span, and
deliberately *not* the compression level, which is assemble-side and may
legitimately differ across a resume. v5 and earlier checkpoints are refused;
re-run a full tilegen.

### Node and way handling

| flag | meaning |
|---|---|
| `--locations-on-ways` | PBF has node coordinates embedded in ways. Auto-detected from the PBF header; this forces it when the header does not declare it |
| `--force-sorted` | use the compact node store without the PBF header flag. Errors if nodes are not monotonic |
| `--allow-unsafe-flat-index` | bypass flat-index guardrails. Expert debugging only; may cause severe IO/RSS degradation |

`--locations-on-ways` reads a PBF header, not the filesystem, and is the one
piece of detection that survives: it is a property of the input file, which is
itself named explicitly and hashed into provenance.

### Memory budgets

| flag | default | meaning |
|---|---|---|
| `--sort-budget <SIZE>` | 1G | per-chunk sort buffer. Min 64M. Lower cuts phase12 RSS, costs merge chunks |
| `--way-budget <SIZE>` | 128M, or 256M under `--locations-on-ways` | in-flight way processing. Min 1M |
| `--assemble-budget <SIZE>` | 32M | tile assembly batch. Min 1M |

Sizes accept `256M`, `1G`, or raw bytes.

### Geometry

| flag | meaning |
|---|---|
| `--seam-reconcile-layers <SPEC>` | polygon layers getting shared-edge seam reconciliation at low zoom. `layer` or `layer:maxzoom` (default maxzoom 8), comma-separated. Default `boundaries` |
| `--fanout-cap-default <N>` | default fanout cap for all polygon layers. 0 or omitted = uncapped |
| `--fanout-cap <layer=N,...>` | per-layer fanout caps; takes precedence over the default. Features whose bbox tile count exceeds the cap are skipped at that zoom |
| `--polygon-simplify-factor <F>` | simplification multiplier for polygon layers. Default 1.0 (same as lines). Higher simplifies harder, cutting sort volume; fills tolerate this better than stroked lines do |

## `elivagar ocean-build --ocean <SPEC> -o <OUTPUT>`

Build the durable world-ocean PMTiles artifact. One shot per shapefile
release, not per run.

| flag | meaning |
|---|---|
| `--ocean <SPEC>` | repeatable, same spelling as `run --ocean`, shapefiles only. Required |
| `-o`, `--output <PATH>` | output artifact path (required) |
| `--tmp-dir <PATH>` | default `data/ocean-build_tmp` |
| `--compression-level <0-10>` | default 6 |
| `-j`, `--threads <N>` | default: logical CPUs |

The `--ocean` spelling is shared with `run` deliberately: the shapefiles and
the zoom split are part of the artifact's identity, so the key it records and
the key a `run` recomputes are derived from the same statement. An artifact is
valid only for a run naming the same shapefiles at the same compression level
over z0-14.

Rotating the artifact is an output-changing event. The blessed regress baseline
must be rebuilt and re-blessed with it, and the gate machine must carry the
same artifact the baseline was built with.

## `elivagar inspect <FILE>`

Header, tile statistics, section layout, provenance, and metadata (layer list
with zoom ranges) for a PMTiles archive. Wrapped by `brokkr pmtiles-inspect` -
the brokkr name avoids colliding with `brokkr inspect`, which is pbfhogg's PBF
inspector.

### Provenance

The `elivagar` metadata member (`reference/metadata.md`, schema at
`src/provenance.rs`), contract first:

```
  Provenance:  schema 1
    Input:      north-america-seq4710-locations.osm.pbf
                xxh3 8122bcc83873ef95349e6a3522827fd9  17.8 GB
                features: sorted, locations-on-ways
    Config:     shortbread, z0-z14, polygon simplify x1
                tile: mvt gzip, base level 6, policy zoom-v1
                seam: boundaries=8
                fanout: none
    Ocean:      artifact, low zoom simplified, simplifier on
                key: shp c10be1c7843c simplified b3417e31c287 level 6 policy 1
    Build:      elivagar b833fc8730cd, pbfhogg 0f1eb01a1c1e
    Effective:  coords inline, way members relation_scan, pins block_local
```

`Input` and `Config` together are the comparability contract, and the **whole**
of both is printed - not a selection. That matters: two archives differing only
in `polygon_simplify_factor` or one fanout cap would otherwise display
identically, and a reader told these lines are the contract would conclude a
geometry diff between them says something about the code. It is cheap to print
in full because the seam and fanout maps omit defaults, so they are a line each
at most.

`Build`, `Effective` and `Resumed` (only on a `--skip-to` run) are diagnostic.
They explain a diff once the contract matches and must never be
equality-gated - given identical input and config they are a function of the
code, and a regression gate exists to compare revisions.

**Reporting is not enforcement.** `brokkr regress` does not read the block, so
these lines let a human refuse a comparison; they do not refuse it. See the
consumer-contract gap in `reference/metadata.md`.

`Ocean` answers "was this artifact-active or computed", which before this
existed could only be recovered by dumping the raw metadata - and on
2026-07-14 was not recovered at all, which is how a computed-path archive was
blessed as the baseline for an artifact-active pipeline. Its `key` line
carries the shapefile identities, which appear nowhere else in the block: an
artifact-active archive's contract is incomplete without them.

`features` reports the PBF header bits, not the filename. A name ending
`-locations-prepass` is a label and can lie; the bits decide which coordinate,
membership and pin paths the run took, and `Effective` says which it then
used. Unset features are omitted, so `features: none` means a plain PBF - but
a *missing* flag reports `unknown` rather than being read as false, since
`none` is a positive claim and must not be derived from absent data.

Commits are abbreviated to 12 characters for reading; the block stores them in
full and that remains the identity. `(dirty)` means the commit names the
nearest ancestor of the code that ran, not the code that ran.

**Every way of having no contract names itself**, because "no block",
"unreadable metadata" and "not JSON" are different facts and reporting any of
them as silence would read as nothing to report:

```
  Provenance:  absent - archive predates the elivagar metadata block
  Provenance:  unavailable - archive stores no metadata
  Provenance:  unavailable - metadata could not be read or decompressed
  Provenance:  invalid - metadata is not JSON
  Provenance:  schema 99 - this build understands 1; not interpreted
  Contract:    INCOMPLETE - block is missing input and config
```

A schema this build does not know is refused rather than summarised: a bump
means an existing field changed meaning, so interpreting it with these
meanings would report confident nonsense. Adding members does not bump, so
unknown members at a known schema are ignored silently.

For the raw block use `scripts/dump-pmtiles-metadata.py`.

## `elivagar verify <FILE> [--geometry-stats] [--unique-payloads]`

End-to-end archive validation: container integrity, metadata schema, tile
decompression, MVT payload structure, geometry commands, layer coverage, and
ocean ring self-intersection. Exit 0 on pass, 1 on failure; stops after 100
tile-level errors.

| flag | meaning |
|---|---|
| `--geometry-stats` | per-zoom ocean statistics: ring counts, max/p99 ring vertices, consecutive duplicates, full-tile fills |
| `--unique-payloads` | validate each distinct compressed payload once, keeping addressed-tile accounting. For very large run-heavy archives such as the ocean artifact |

`verify` is a standing gate, but it is a structural one: it cannot see a
tessellation fault that decodes cleanly. That is what the earcut oracle
(`scripts/validate/earcut-oracle.mjs`) is for.

## `elivagar regress <CURRENT> --against <BLESSED>`

Semantic diff of two PMTiles archives (MVT + gzip only).

| flag | default | meaning |
|---|---|---|
| `<CURRENT>` | | archive to compare (positional) |
| `--against <PATH>` | | blessed archive to compare against (required) |
| `--tol <N>` | 0 | geometry tolerance in layer extent units |
| `--max-moved <N>` | 0 | tolerance-moved features allowed before failure |
| `--max-examples <N>` | 20 | per-class example cap |
| `--svg-dump <DIR>` | | side-by-side SVG dumps of structural examples |
| `--json` | | machine-readable output |

**Invoke this as `brokkr regress`, not directly.** brokkr resolves the current
output and the blessed archive from `brokkr.toml` itself, which is what keeps a
comparison from being made across incomparable archives. Two rules the wrapper
enforces that the raw binary cannot: a blessed archive is always
locations-generated, and comparing a locations baseline against a raw build
reports a six-figure structural diff for two correct builds - that exact false
alarm was investigated at length on 2026-07-09 and again on 2026-07-14.

Exit 0 only if nothing structural.

## `elivagar svg <FILE> -z <Z> -x <X> -y <Y> [OPTIONS]`

Render tiles as SVG. Single tile or an NxM grid.

| flag | default | meaning |
|---|---|---|
| `-z`, `-x`, `-y` | | tile coordinates (top-left of the grid) |
| `-W`, `--width <N>` | 1 | grid width in tiles |
| `-H`, `--height <N>` | 1 | grid height in tiles |
| `-l`, `--layers <LIST>` | all | comma-separated layer filter, e.g. `ocean,boundaries` |
| `-o`, `--output <PATH>` | stdout | output path |

Polygons fill with `nonzero`, matching MapLibre; points are circles, lines are
stroked paths, background is land-coloured. Grid lines are drawn when width or
height exceeds 1.

## `elivagar diag <FILE> -z <Z> -x <X> -y <Y>`

Ring winding for one tile: per-ring vertex count, signed area, and direction
(CW outer, CCW hole) for every polygon feature across all layers. Prints
first/last 3 vertices for large rings, all vertices for rings of 6 or fewer.

## `elivagar ocean-coverage <FILE> --baseline <REF> [OPTIONS]`

| flag | default |
|---|---|
| `--baseline <PATH>` | required |
| `--zmin <Z>` | 1 |
| `--zmax <Z>` | 6 |
| `--threshold-2x <N>` | 512 |
| `--layer <NAME>` | `ocean` |

Measures per-tile one-sided ocean coverage loss against a verbatim same-source
baseline.

**Currently unfeedable.** Its baseline was built with `--no-ocean-simplify`,
which no longer exists, and there is no other way to produce a verbatim
same-source comparand. The subcommand remains only because an archive built
before the flag's removal still works as `--baseline`.

It was never a gate in any case. At low zoom any correct simplifier removes
large sub-pixel coastline detail versus a verbatim baseline, so this
over-reports and cannot separate legitimate generalization from a coverage
defect - a confirmed false negative on the 2026-07-12 ocean VW landing, and the
reason `reference/performance.md` and AGENTS.md both record it as triage only.
The authoritative ocean gates are the earcut oracle and the human visual check.

## Environment variables

None of these duplicates a flag or outranks one. A knob is a flag or an env
var, never both, because two ways to say one thing means the quieter one wins
silently.

They are recordable: brokkr's `capture_env` in brokkr.toml matches `ELIVAGAR*`
and `MALLOC*` and stores what it finds with the run, so an env-configured run
can be reconstructed from the results row. That is what makes them legitimate
rather than ambient - before it, the knob below that moves NA assemble by 26s
was tunable from any shell and recorded nowhere.

| var | effect |
|---|---|
| `ELIVAGAR_ASSEMBLE_WORKERS` | assemble worker cap, default 8. Measured on NA locations: 4 workers left the writer idle 68% of assemble; 8 cut assemble 186.4 to 160.7s; 12 was WORSE at 176.1s and +3 GB RSS - encode CPU saturates near 8 |
| `ELIVAGAR_ASSEMBLE_PARK_BUDGET` | bytes of encoded-but-unwritten tiles workers may run ahead of the writer, default 2 GiB. The RAM ceiling on assemble, and the term the planet ledger's 6-9 GB assemble estimate is derived from |
| `ELIVAGAR_REL_BLOCKS_CAP` | relation-block buffer cap in bytes. Past it the tail re-reads relation blobs instead of holding them; set it to 1 to force the spill path on a small extract |
| `ELIVAGAR_LAYER_STATS=1` | per-layer per-zoom sort-stats firehose (~800 counters). Emitted at end of run, so it never affects timing. The per-layer totals are always emitted |
| `BROKKR_MARKER_FIFO` | set by brokkr. Phase markers and counters go here; unset means every counter in `src/debug.rs` is a silent no-op |

`BROKKR_MARKER_FIFO` is why a bare `elivagar run` emits no metrics at all: the
sidecar numbers exist only for runs brokkr started with a measurement flag.

The harness cannot express "allow brokkr with any env assignments", so
`VAR=x brokkr ...` is blocked in-session; `scripts/envrun.sh` exists only to
carry that, and gates nothing.

## What is not on this surface, and why

`--no-ocean`, `--no-ocean-simplify`, `--ocean-simplified` and `--ocean-tiles`
were removed on 2026-07-14 along with the auto-detection.

`--no-ocean` existed only to defeat the auto-detection, so it died with it:
omitting `--ocean` now means what `--no-ocean` used to.

`--ocean-simplified` and `--ocean-tiles` folded into `--ocean`'s zoom-range
spelling. Both named an ocean input; there is now one flag for that, and its
value states the coverage it claims rather than leaving it to the flag's name.

`--no-ocean-simplify` skipped the ocean VW simplifier to emit verbatim polygons
for an `ocean-coverage` baseline. It was a production-binary flag whose only
purpose was producing deliberately non-production tiles for a diagnostic that
had already been demoted from gate to triage. `provenance.rs` keeps the
`runtime_simplification` field, invariably true on archives built after the
removal and possibly false on older ones - dropping the field would make those
archives incomparable to these, which is the opposite of what the block is for.
