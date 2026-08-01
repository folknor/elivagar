# Correctness Gates

The archive is the only sacred artifact. Three gates define correct.

## verify

```sh
elivagar verify out.pmtiles
```

Container integrity, metadata schema, tile decompression, MVT payload
structure, geometry command validation, layer coverage, and ocean ring
self-intersection checks. Exits non-zero on failure. `--geometry-stats` adds
per-zoom ocean statistics: ring counts, maximum and p99 ring vertices,
consecutive duplicates, full-tile fills.

## The tessellation oracle

```sh
node scripts/validate/earcut-oracle.mjs out.pmtiles all
```

This is the gate that matters most, and the reason is worth stating plainly:
**it decodes tiles the way the renderer does, not the way the encoder meant
them.**

It reads every tile with a consumer-path decoder, groups rings using
maplibre-gl's own verbatim `classifyRings`, tessellates each polygon with
earcut, and reports per-zoom deviation plus misattached holes - a hole whose
bounding box falls outside the outer ring it was assigned to. Passing means
zero over threshold and zero misattached, on every polygon layer.

It earned that status. A ClosePath cursor bug - the MVT spec says ClosePath
does not move the delta cursor - survived three months of internal round-trip
tests, because a symmetric encoder/decoder violation round-trips perfectly. It
was only visible to something that decoded the way MapLibre does.

## The corpus baseline

A committed set of per-tile digests for a fixed extract. A fresh build is
checked against it, so an unintended output change fails loudly and an
intended one is reviewed as a diff before the baseline rotates.

The baseline records the build contract - input identity, config, ocean
artifact key - and refuses to compare across a contract mismatch rather than
reporting a difference that says nothing about the code.

## What makes something a gate

A validator gates work only after it is calibrated in **both** directions:
it fires on a known-bad artifact, and it clears on a known-good one. Until
then it is advisory.

The gates that have ever been authoritative here share three properties:

1. **Consumer-path decode.** Read geometry the way the renderer will.
2. **A categorical defect with near-zero base rate.** Passing means a count of
   zero, and zero is achievable.
3. **Both-direction calibration**, including the arithmetic for what a correct
   build scores, before any threshold is chosen.

A gate that thresholds a **continuous** quantity that is legitimately large on
good output is a false negative waiting to happen. An ocean coverage measure
did exactly that here: it reported a working low-zoom fix as a failure,
because any correct simplifier loses large amounts of sub-pixel coastline
detail against a verbatim baseline. It was retired. A rendered-area
connected-component measure was refused for the same reason - on calibration
it separated defect from legitimate generalization by only 3.29x where the
threshold arithmetic needed 4x.

That is also why aggregate similarity scores are not used for adjudication
here, and why the human visual check remains a standing part of ocean work.

## Additional instruments

Not gates, but the tools for attributing a failure:

- `ring-cap-census.mjs` - rings-per-polygon against MapLibre's 500-ring clamp,
  which silently drops rings the tessellation oracle then never sees.
- `boundary-line-oracle.mjs` - line-fidelity for the boundaries layer:
  palindromes, retrace spurs, and cross-feature duplicates.
- `zoom-overlay.mjs` - a low-zoom tile's fill under the same archive's
  higher-zoom geometry as an outline, for judging generalization.
- `feature-probe.mjs`, `line-probe.mjs`, `winding-probe.mjs`,
  `layer-census.mjs` - single-tile and single-feature drilldowns.

These live in `scripts/validate/`, a deliberately separate Node project with
its own lockfile: the oracles are calibrated against specific versions of
maplibre-gl and `@mapbox/vector-tile`, and sharing a lockfile with the docs
site would let an unrelated dependency bump move the thing the gate measures
against.
