# Regress engine: post-rewrite status

2026-07-11. The blob-pair-span rewrite (mmapped `ArchiveView`, run-preserving
directory merge, three tiered parallel passes, comparison-native `DetailTile`
decoder, digest-bucketed matcher with bbox-pruned/KD-indexed Hausdorff,
bounded report aggregation) is implemented in `src/regress.rs`, validated by
the unit suite plus the test-only legacy differential oracle, and measured on
the live denmark archive pairs. This document records the measured outcome
and what genuinely remains open; the diagnosis, design, and minor-wins ledger
that drove the rewrite are done and removed (see git history for the full
plan).

## Measured before/after (plantasjen, denmark pairs)

Identical pair, `20c8bd7` vs `f683129` (1,296,996 addressed tiles/side):

- before: ~4-5 min, one core, decode-bound.
- after: **1.9 s wall** (34.9 s user, ~18.5 cores), peak RSS ~0.95 GB.
  Zero diffs. Tier split: raw_equal 112,711 pairs / 1,218,907 tiles,
  canonical_equal 56,701 pairs / 77,427 tiles, detailed 662 pairs (all
  semantically identical - ring rotations hash differently but classify at
  distance 0). Pass times: raw 4 ms, canonical ~1.45 s, detail ~0.2 s.
  This pair predates the paint-order-determinism landing (total record
  order + paint-rank key): most of the canonical/detailed share above is
  the record-order race that landing closes, not semantic content. A
  same-commit pair measured after that landing is expected to land almost
  entirely in raw_equal instead; unmeasured here, noted so the split above
  is not misread as the ongoing expected shape.

Diff-heavy pins pair, `acbe400` vs `20c8bd7`, `--tol 24 --max-moved 10000000`:

- before: 30+ min without finishing (the run was killed; this pair had never
  completed under the old engine).
- after: **3.5 s wall** (70.7 s user, ~20 cores), peak RSS ~1.0 GB.
  9,983 differing tiles, tolerance_moved 28,886, structural_moved 204,769,
  full per-zoom/layer and displacement report. 9,984 detailed pairs; detail
  pass 1.8 s.

Both runs: unique_blob_pairs ~170K from ~469K directory runs and ~336K unique
blobs - the span grouping does its job; the memo that caused the germany OOM
no longer exists.

Germany-scale validation (the old engine's OOM case, 2x 2.6 GB archives):
completed post-landing at `7178425` - zero diffs across 827,010 tiles,
~36 s (raw 0.4 s / canonical 13.0 s / detail 22.0 s), peak RSS 5.8 GB.
Tier split: raw_equal 78,683 pairs / 593,931 tiles, canonical_equal
218,675 pairs / 228,205 tiles, detailed 4,874. This also retroactively
completes the injected-prepass Brick 4 germany activation gate that the
old engine could not finish.

## Remaining open
- **Streaming fingerprint mode.** Tier-2 semantic fingerprints currently
  decode a full `DetailTile` per blob and drop it; the canonical pass
  (~1.45 s) dominates the identical-pair wall. A true streaming digest (no
  point-vector materialization) would cut that further. Optional - the
  current shape is already within the plan's peak-memory model.
- **Residual matcher endpoint.** Non-exact anonymous pairing is
  proxy-greedy (bbox lower bound, then bbox-center distance) with exact
  greedy kept for groups where k^2 <= 64, and leftovers force-zipped so a
  type/topology change stays one structural move instead of an added/missing
  pair. The plan's stronger endpoint - sparse-candidate-graph deterministic
  minimum-cost matching - remains unbuilt; revisit only if greedy
  misclassification is observed on a real landing.
- **`PmtilesReader` still expands runs into per-tile entries** for its other
  consumers (inspect, verify, svg, diag, the legacy oracle). The regress path
  no longer touches it. Run-preserving cleanup is the explicitly-deferred
  "afterwards" refactor, outside the rewrite's blast radius.
- **Legacy differential oracle retirement.** The pre-rewrite canonical-graph
  comparator survives as a `#[cfg(test)]` oracle
  (`regress::legacy_oracle`) exercised on synthetic fixtures covering
  permuted order, bit-distinct floats, duplicate ids, anonymous ocean
  features, holes, and multipart geometry. A full-report differential over
  the real diff-heavy pairs is unattainable - the old engine cannot complete
  them - so decide deliberately when the oracle has paid for itself and
  delete it; it is never a runtime switch.
