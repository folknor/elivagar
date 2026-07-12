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
- **Streaming fingerprint mode.** Landed. Tier 2 now streams geometry points
  into xxh3-128 ring digests, retaining only dictionary entries, small attr
  lists, and fixed-size component/feature digests. Feature and component
  ordering is erased by hashing sorted child digests (not wrapping-add sums,
  which have additive collision structure); layers mirror the detail
  decoder's stable name sort so even duplicate-name layers (invalid MVT,
  accepted by both decoders) keep identical equivalence. Layer names/extents,
  feature ids/types/attrs, ring order, and ring point order remain bound.
  The `DetailTile` decoder and detail/report tiers are unchanged. The
  differential unit test asserts expected verdicts (not just agreement)
  over feature/component/attr permutations, rotated rings, moved vertices,
  dictionary re-indexing, layer permutation, duplicate layer names,
  multipoint order, zero-area rings, hole order, repeated-count line
  MoveTo, and feature multiplicity, all against `detail_tile_hash`.
- **Residual matcher endpoint.** Landed. Large non-exact residual groups now
  form a deterministic sparse same-key candidate graph: eight nearest items
  per side by bbox lower bound, bbox-center proxy, and index, unioned in both
  directions. Real KD/bbox-pruned Hausdorff distance is evaluated only for
  those candidate edges. A deterministic successive-shortest-augmenting-path
  minimum-cost maximum-cardinality matcher replaces proxy-greedy pairing for
  both feature and component call sites. Review-folded hardening: relaxation
  is strictly improving (a tie-relaxing variant could corrupt predecessor
  chains and hang the augment walk - caught with a concrete 4x4
  counterexample, now a fixture), reverse edges ride the forward relaxation
  through cur_match instead of an O of m times n scan per pass, the augment
  walk carries a vertex-bound assert, and a same-key completion sweep
  restores the exhaust-same-key-before-force-zip contract that K-starved
  cluster splits could violate. Exact digest pre-pairing, the k^2 <= 64
  exact short circuit, and final force-zip cardinality fallback are
  unchanged. Unit coverage: greedy-crossing, repeated-run determinism, a
  brute-force min-cost max-cardinality oracle differential over tie-heavy
  and sparse cost matrices including the counterexample, and a two-key
  starvation case proving no cross-key zip.
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
