# The output corpus: methodology

The complete methodology for elivagar's output baseline - what the corpus
is, why it replaced the pmtiles bless machinery, how the standing gate is
run, how a rotation is adjudicated, and what the gates are calibrated
against. `reference/cli.md` documents the command surface; AGENTS.md
carries the terse operational version; `reference/performance.md` holds
the calibration records. This document is the methodology those three
defer to.

## What it is

A git-committed, human-viewable, exhaustively-gated output baseline in
three tiers, all under `corpus/<dataset>/`:

1. **The digest gate** (tier 1, THE standing gate): a committed canonical
   content digest over EVERY addressed tile of a dataset build
   (`contract.json`, `digest`, and in leaves mode `leaves`). Categorical:
   pass is byte-equality of recomputed against committed hashes; fail
   names the zooms and tiles. Exhaustive detection, no sampling.
2. **The SVG corpus** (tier 2, the human layer): committed canonical SVG
   renders of a curated tile manifest (`manifest.toml`, `tiles/*.svg`,
   styled by `corpus/style.toml`). Text-diffable in git, viewable in
   Inkscape or a browser. This is where a human judges a rotation; it is
   deliberately NOT an independent detector - the digest subsumes its
   detection duty, and the SVG compare inside `corpus check` exists only
   as a staleness guard on the committed files.
3. **Overlay attribution** (tier 3, on demand): `elivagar regress
   <current> --against <comparand> --overlay` renders per-tile diff SVGs -
   added and removed features in the two comparison colors, id-matched
   changed pairs drawn on both sides, per-key attribute old/new lines.
   Takes two explicit archive paths, no registry; the natural comparand
   source is the per-commit archives in `data/tilegen/`.

## Why it replaced the bless machinery

Ruling of 2026-07-15. The blessed archive was a machine-local binary
baseline outside version control, and that single property produced four
incidents in one week: the 07-09 rotation grind (regress ground 15+
minutes against a stale baseline because the de-facto one had been
rotated away), the 07-14 mis-bless (an artifact-absent archive blessed
while every gate passed), the un-gateable window (nothing could regress
against blessed from the provenance landing until the 07-15 re-bless),
and the stale-ocean-artifact incident (three days of pre-VW coastline
spikes served world-wide while landings gated against each other - a
chain with no anchor). A baseline in git is versioned with the code,
survives archive rotation by construction, travels to any machine, and
turns every rotation into a reviewable diff.

What was genuinely lost and accepted: regress-vs-blessed gave instant
attribution against a curated baseline archive. The digest detects but
attributes only to tiles; attribution needs a comparand archive - the
per-commit archives in `data/tilegen/` in practice, or a rebuild of the
old commit. Accepted because detection is the gate and attribution is a
debugging workflow.

## The standing gate

For an output-neutral landing:

```
brokkr tilegen --dataset denmark --variant locations
elivagar corpus check data/tilegen/denmark-<commit>.pmtiles --corpus corpus/denmark
```

Exit 0 is the pass. Exit 1 is a content mismatch and names the changed
zooms and tiles (`changed z x y ...` / `added` / `removed`). Exit 2 is a
refusal - missing baseline, invalid archive, or a differing input/config
contract - and is never a verdict on content. Routine gating is
denmark-only, matching the standing gate policy; heavier datasets gate at
their own baselines if and when those are blessed.

A check costs ~2.2 s on denmark (1.3M tiles): cheap enough to run per
landing.

For a landing that states an explicit geometry tolerance instead of
zero-diff, the gate is the two-archive diff with an explicit move budget:
`elivagar regress <new> --against <prev> --tol N --max-moved M`, verdict
read against the displacement percentiles stated in that landing's spec.
`--max-moved` is mandatory - `passed()` requires `tolerance_moved <=
max_moved`, which defaults to 0, so `--tol` alone can never accept a
tolerated move.

## Rotation: accepting an intended output change

A rotation is one commit, and the commit IS the bless:

1. Run `corpus check` on the new build; read the named tiles.
2. Adjudicate: use the changed-leaf names, the corpus SVG diffs, and
   tier-3 overlays against the previous commit's archive to decide the
   change is intended.
3. `elivagar corpus bless <archive> --corpus corpus/denmark --rotate` -
   rewrites contract.json, digest, leaves, and re-renders the manifest
   SVGs in the same command. Without `--rotate`, bless refuses to
   replace an existing baseline. Bless refuses dirty builds (provenance
   `Build` dirty flags), non-locations archives, and non-MVT-gzip
   archives at the door.
4. Commit the resulting `corpus/` diff with the landing. The reviewable
   git diff - contract changes by path, leaf hash changes by tile, SVG
   changes by pixel-relevant geometry - is the human gate the old bless
   never had.

Rotating the ocean artifact, the style file, or any config in the
contract is a rotation like any other: `check` refuses on the changed
contract or style hash until the baseline is re-blessed, so drift is
loud, never silent.

## The contract

`contract.json` is written at bless time from the archive's provenance
block: the input identity (PBF xxh128, variant features), the full config
contract (profile, zooms, tile format/compression, seam/fanout/simplify,
ocean artifact key including OCEAN_POLICY_VERSION), the build block, and
- when the corpus has a manifest - the style file path and hash. `check`
compares contract before content and refuses a mismatch with the field
named. Input NAMES and the build block are diagnostic only, never gated:
renaming a PBF warns, a differing build commit is recorded not refused.
This mirrors the provenance rule that `Input` plus `Config` is the
comparability contract while `Build`/`Effective`/`Resumed` are
diagnostic.

The strictness is a feature: an output-neutral config-schema change
refuses the comparison (contract.json changes while digest does not),
which forces the rotation commit to say so instead of letting two
incomparable baselines drift silently - the exact failure mode of the
2026-07-14 mis-bless.

## The digest

One definition, two committed representations:

- **Leaves mode** (default; denmark): one line per canonicalized run - a
  maximal same-zoom span of tiles sharing one semantic content hash -
  plus per-zoom rollups and a root. Names every changed tile directly.
  Denmark: ~10.8 MB committed, a pure function of tile content, immune
  to writer dedup and run-structure churn (runs are zoom-split then
  merged by equal semantic hash).
- **Buckets mode** (planet-scale): z0-z7 tiles hash individually; z8-z14
  roll up under their z7 ancestor, with a bucket root (`broot`) as the
  integrity guard. Detection localizes to buckets; tile-level naming
  comes from tier 3 against a comparand archive. The rotation
  reviewability of bucket-mode baselines is weaker by construction and
  recorded as an open limitation.

Both modes carry internal self-consistency (recomputing rollups and root
from the committed rows) so a hand-edited or merge-damaged baseline
refuses before any archive is read.

The canonical hash (`streaming_tile_hash`, shared with regress) covers
tile addressing, layer names, versions and extents, feature ids,
attributes bit-exactly, and geometry; it absorbs gzip bytes,
layer/feature/attribute ordering, key/value-table permutation, and
multi-geometry component ordering. Unknown fields error at all four MVT
message levels, and repeated packed tag/geometry fields concatenate -
each direction pinned by mutation tests that assert the streaming hash
and the detail decoder remain one equivalence relation. Archive
metadata, header bounds, and directory layout are outside the digest;
the provenance contract and `elivagar verify` own those surfaces.

## The SVG corpus

The render core decodes MVT in wire order, groups polygon rings through
a verbatim integer port of MapLibre's classifyRings (self-calibrating
winding, zero-area skip, maxRings=500 clamp with stable area-descending
truncation), and emits canonical SVG: layers in committed style order,
features in the regress canonical total order, integer coordinates,
nonzero fill, XML-escaped, deterministic to the byte. Unchanged tiles
re-render byte-identical, so rotation commits only rewrite genuinely
changed files.

`corpus/style.toml` is the committed 26-layer style (layer +
attribute-value match, OSM-Carto-inspired colors); its hash is part of
the corpus contract. Layer order is paint order, bottom first, with
land below water_polygons so lakes are never occluded, and every kind
`is_known_land_kind()` accepts has an explicit fill so no land feature
can render background-colored and read as missing (both fixed after the
2026-07-19 eyeball pass caught land painting over lakes and
near-invisible streets). Unstyled layers render magenta so schema drift
is visible, never silent. Style edits are digest-neutral rotations:
re-render via `corpus render-manifest`, commit.

The classifyRings port is kept honest by a differential oracle:
`elivagar corpus rings <archive> -o <dump>` must byte-match the output
of `scripts/validate/ring-grouping-oracle.mjs`, an independent Node
implementation over the pmtiles and vector-tile libraries. Run it over a
full denmark archive whenever the ring-grouping code is touched; its
2026-07-15 reading was byte-equal over all 1.3M tiles. The f64/i128
equivalence it rests on holds for in-extent tile coordinates (sums below
2^53), which real archives satisfy.

The manifest is curated and append-only: representative classes (coast,
dense city, rural, boundary junction, island cluster, full-fill) plus
the hard-tiles ledger - every fixed visual bug drops its tile in.
Entries may pin layer subsets to keep dense tiles at review size. The
2026-07-15 seed is nine tiles including both stale-artifact spike tiles
and the R23 ClosePath class.

## Calibration and oracle discipline

Both gates are categorical byte-equality on deterministic output - no
thresholding of continuous quantities (the ocean-coverage
false-negative trap). Per the AGENTS.md oracle discipline they were
calibrated in both directions before rotation, and the instrument is
committed so recalibration is a command away:

`elivagar corpus mutate <in> -o <out> --op
drop-tile|nudge-geometry|layer-version|regzip [--tile z/x/y]` produces
same-contract calibrands: the first three make a single-tile known-bad
(splitting shared directory runs so only the named tile changes), and
regzip is the byte-different semantically-equal known-good control. The
2026-07-15 readings (both recorded in `reference/performance.md`): all
three defects FIRE with the target tile named, regzip CLEARS through
both tiers, the self-check passes, and the pre-fix stale-artifact
archive refuses on `config.ocean.artifact_key.policy_version` - the
contract guard doing for the corpus what OCEAN_POLICY_VERSION does for
runs. Recalibrate (rerun mutate + check, plus the ring-grouping oracle)
whenever the digest hashing, the canonical decode, or the render core
changes.

## Limitations, stated plainly

- **No baseline diff catches a defect already present in the baseline.**
  That is the stale-artifact incident's lesson and it applies to the
  corpus exactly as it did to blessed archives. The corpus's
  contribution to that class is making the baseline humanly reviewable
  at bless time and diffable forever after. Baseline-free detectors -
  the earcut oracle, the boundary-line oracle, the recorded seam-gap
  candidate - remain the only guard against inherited wrongness and
  stay authoritative for correctness-in-itself.
- The digest attributes to tiles (leaves) or buckets, not to causes;
  attribution needs a comparand archive via tier 3.
- Bucket-mode rotations are not exhaustively human-reviewable; the
  committed rows are opaque hashes. Open, accepted for planet scale.
- `elivagar regress` reads no contract by design - it diffs whatever two
  archives it is handed. Comparing across variants or configs produces
  six-figure diffs on two correct builds (2026-07-09, 2026-07-14);
  establish comparability from the provenance blocks first. The corpus
  gate refuses such comparisons mechanically; regress trusts you.

## Viewer geometry interpretation (the render core's ground truth)

Established against the earcut oracle's verbatim classifyRings and a
clean-room OpenLayers review; not restated anywhere else.

- Both viewers fill NONZERO. evenodd agrees only on well-formed
  alternating outers/holes and papers over exactly the ring-role bugs
  the corpus must catch.
- OpenLayers Canvas submits all of a feature's rings as subpaths of one
  path, nonzero rule; ring order irrelevant, only relative winding.
- MapLibre classifyRings self-calibrates: the first nonzero-area ring
  defines outer winding; matching-wound rings start polygons,
  opposite-wound become holes; zero-area rings are skipped; maxRings=500
  keeps the largest by area (sorting the clamped polygon's rings
  area-descending in the process). ~20 lines, derivable from the MVT
  spec.
- The one valid-input divergence: MapLibre's 500-ring clamp (OL keeps
  all rings). No single SVG matches both; the renderer emits the
  MapLibre clamp and flags the tile, never silently picks.
- Raster-only concerns (AA, joins, caps, label collision) live where no
  geometry bug does and are deliberately out of scope.

## brokkr and the corpus

The corpus workflow is entirely elivagar-side: brokkr wraps none of the
corpus subcommands, and the gate is invoked as the raw elivagar binary.
The corpus machinery enforces its own guards, so any future brokkr
wrapper is convenience, never safety.

## History

Designed and landed 2026-07-15, the same day the stale-ocean-artifact
incident supplied the motivating case. Spec commits `ecef3b8` (digest
gate), `8161ed7` (render core + corpus), `badcaa0` (teardown); landings
`a8c4f84`/`340215a`, `41a953a`/`cb65c2f`, `8b7c4a0`. The full design
record - surveys, review consolidations, calibration bookkeeping - lives
in those commits' documents in git history.
