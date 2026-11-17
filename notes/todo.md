# Todo

Things the project might do later. Not defects: each would make the project
claim something it does not claim today.

## `elivagar verify` for MLT and brotli archives

`verify` accepts only MVT + gzip archives, while the writer can produce MLT
(uncompressed) and brotli-compressed MVT. Verifying those would need a
decompressor chosen from the header, and for MLT a separate structural
validator, since the MVT command grammar checks do not apply. Surfaced by the
2026-10-28 design hunt; set aside at triage on 2026-11-04 as a new capability
rather than a fix.

## Planet assemble parallelism and memory

Open after the 2026-11-16 planet round (measurements in the "Planet
assemble and close" section of reference/performance.md).

- Assemble averages about 17.6 of 24 cores. Merge readers are not the
  limit (12 instead of 8 moved wall 9:18 -> 9:15), and the ordered
  writer's ~1M voluntary context switches say it still waits on batches.
  Candidates, in order:
  - Persistent encoder workers instead of one spawned thread per
    partition (about 117K on the planet). Needs a new completion and error
    protocol: today a partition's thread ending is its completion signal,
    and its join carries its error.
  - How encoded batches reach the ordered writer: a slow partition holds
    every later one behind it, since the writer emits in tile-id order.
- Assemble peak anon rose 7.9 -> 9.2 GB with `4c73021` (three recycled
  slabs per worker plus per-source record buffers). Check whether slab
  count or capacity can be capped without giving back the fault savings;
  persistent workers may make the buffers per-worker for free.

## Rebuild the world ocean artifact after the i_overlay 9.2.0 snapping port

The overlay engine's snapping now follows i_overlay 9.2.0, but
`data/ocean-tiles.pmtiles` was built by the old engine. Extract-computed
boundary-band ocean uses the new snapping while artifact-served interior
tiles do not. Both are valid output; the mismatch only matters for
consistency at the band edge. A rebuild forces a corpus rotation with
`--rotate`, so fold it into the next shapefile release unless the band
edge shows a visible seam.

# Leftovers from moving the elivagar tooling out of brokkr

Small items left after the in-tree crates took over from brokkr's elivagar
commands.

## serde_json feature split recompiles the library

crates/corpus enables `serde_json/float_roundtrip`, while crates/lib and
crates/cli use plain `serde_json`. Under resolver 3, a `-p elivagar-corpus`
build (`brokkr run elivagar-corpus`) resolves serde_json with the feature
and every product build resolves it without, so serde_json and crates/lib
build in two variants: one extra library build when switching between the
corpus tool and a product build, not a rebuild per call (two back-to-back
`brokkr run elivagar-corpus` calls rebuild nothing). Fix: enable `float_roundtrip` on crates/lib's
serde_json too, so every graph agrees. The feature changes how the library
parses floats, so check what JSON crates/lib parses and run the corpus gate
before landing it.
