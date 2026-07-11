# Regress engine: optimization hunt (consolidated)

2026-07-11. Two independent hunts over `src/regress.rs` and its support code
(one Fable agent, one codex gpt-5.6-sol xhigh), same framing: structural over
micro, full rewrites named as such, pre-1.0 API breakage allowed, no reading
of prior docs or optimization history. This document deduplicates,
consolidates, and prioritizes both reports. Where both hunters arrived at the
same finding independently, it is marked CONVERGENT - that is the strongest
signal here, and they converged on essentially the entire architecture.

Symptoms being explained:
- denmark-scale run (two ~400 MB archives, ~1.3M addressed tiles/side),
  all-identical fast case: ~4-5 min, decode-bound, one core.
- first diff-heavy run ever executed (pins landing, large fraction of
  features legitimately changed): 20+ min and counting.
- germany-scale runs (two 2.6 GB archives): OOM-killed twice on a 30 GB
  host. (Also filed as performance-backlog item 27; this document is the
  full treatment and supersedes item 27's sketch.)

Measured archive facts (codex, from the actual gate archives):

| archive | addressed tiles | unique blobs | dir entries | compressed data |
|---|---:|---:|---:|---:|
| denmark current | 1,296,996 | 167,886 | 234,617 | 345.8 MB |
| denmark blessed | 1,296,996 | 167,877 | 234,421 | 331.3 MB |
| germany current | 827,010 | 301,123 | 336,467 | 2.6 GB |
| germany blessed | 827,010 | 300,486 | 332,129 | 2.6 GB |

Blast radius (Fable, verified by grep): `regress::` is consumed only by
`main.rs` and `mvt_decode.rs`; tests live in `src/regress/tests.rs` and
mostly exercise compare semantics on `CanonTile` structs. The rewrite breaks
nothing outside the module.

## Diagnosis (CONVERGENT on all five)

1. **The entire run is single-threaded.** `regress()` is one serial
   merge-join over ~1.3M tile pairs; each iteration does seek+read, gzip
   decompress, full MVT decode, canonical sort, canonical serialization, and
   hash, both sides, on one core. `rayon` and `memmap2` are already
   dependencies and are used nowhere on this path. The structural cause is
   `&mut PmtilesReader` (seek-based `File` I/O) threaded through
   `BlobMemo::get` - mutable reader ownership serializes everything
   downstream. ~95% of a 5900X idles for the whole run.
2. **The OOM is the `BlobMemo`, and it is unbounded by design.** It retains
   the fully decoded `CanonTile` for every unique blob of BOTH archives for
   the whole run. Decoded residency is plausibly 5-10x compressed size
   (nested ring Vecs, per-feature cloned attr strings). Germany = ~602K
   retained canonical tile graphs off 5.2 GB compressed; 30 GB has no
   chance. Not a leak - the architecture.
3. **The fast path deep-clones every tile twice.** Memo hits return
   `cached.clone()` (full deep clone) and misses clone again on insert, even
   when the caller only compares two u128 hashes and discards both clones.
   Denmark performs roughly 2.26M deep memo-hit clones on an 87%-deduplicated
   archive.
4. **Hashing materializes a full canonical byte serialization per tile**
   (`canonical_tile_bytes` builds a whole `Vec<u8>` to feed xxh3 once);
   component sort keys are freshly allocated serialized Vecs too.
5. **The diff-heavy path is accidentally cubic.** Anonymous-feature pairing
   recomputes full geometry serializations inside an O(k^2) exact-match
   scan; the residual greedy loop rescans all remaining pairs every
   iteration recomputing full distances = O(k^3) Hausdorff evaluations;
   `discrete_hausdorff` itself is brute-force O(n*m) vertex pairs in i128
   with no early exit, no bbox prefilter (a 2,000x2,000-point ring pair is
   8M i128 multiply-adds per candidate per greedy iteration); `ceil_sqrt`
   is a ~64-iteration binary search per distance. This is the 20+ minute
   run, and every legitimate geometry-changing landing will hit it from now
   on.

Additional structural waste (codex): the PMTiles directories are run-length
encoded precisely because the writer dedups consecutive tiles onto shared
blobs, and `PmtilesReader` immediately EXPANDS every run into one `TileEntry`
per addressed tile - destroying compression the tool then re-derives through
its own hash maps. The decoder holds `RawFeature` and final canonical
representations alive simultaneously and decodes packed geometry twice
(`Vec<u32>` intermediate, then point vectors).

## The rewrite (one coherent unit, not staged probes)

Both hunters independently recommend replacing `regress()`'s execution
engine outright - keep the CLI, the report format, and the classification
semantics; delete the driver, the memo, and the pairing engine. Build the
complete intrusive change, benchmark against the current binary on the live
archive pairs, keep or revert.

### P1 (CONVERGENT, high-conviction rewrite): blob-pair-span engine over mmapped archives + rayon

- mmap both archives (regress-specific immutable `ArchiveView`; this is
  purpose-built for bulk comparison, not a generic `PmtilesReader` cleanup).
  Clean file-backed pages stay reclaimable - materially safer than tens of
  GB of heap graphs.
- Keep the directories' run-length structure (`TileRun { start, end, blob }`)
  instead of expanding to per-tile entries: denmark starts from <0.5M
  intervals instead of 2.6M expanded entries. Merge the two ordered run
  streams by interval intersection into
  `PairSpan { start, end, current: Option<BlobId>, blessed: Option<BlobId> }`,
  group by `(current_blob, blessed_blob)`, carry multiplicity. Comparison of
  a blob pair is tile-independent (tile-local coordinates); only reporting
  needs tile id/zoom, so split spans at zoom boundaries and multiply
  counters. This subsumes the memo entirely - a dedup run collapses into ONE
  work item instead of N memo hits.
- `rayon par_iter` over unique blob-pair work items; decode both sides,
  compare, emit a compact plain-data outcome, drop the decoded tiles
  immediately. Deterministic reduce: sort, select capped examples by lowest
  tile id, replicate displacement values by multiplicity so report semantics
  are preserved exactly.
- Peak memory model becomes: directory runs + compact fingerprints + spans +
  O(workers x largest active tile). Germany stops OOMing structurally.
- Expected: ~20x from parallelism alone on the decode-bound case; both
  hunters project the fast case in seconds-to-~15s territory.
- Risks: multiplicity bookkeeping, zoom-boundary splitting, deterministic
  examples/errors under parallelism, and bounded per-task scratch (fresh or
  capacity-capped, not unbounded thread-locals). None justify the current
  architecture.

### P2 (CONVERGENT, part of the same rewrite): tiered equality before materializing anything

- **Tier 1 - raw compressed equality:** length check + fast digest over the
  raw compressed mmap slices, confirmed by direct memcmp on digest match.
  Identical gzip bytes imply identical tiles - sound one-way, no false
  passes; marks whole spans identical with zero decompression. Free
  downside: on mismatch it cost one early-exiting memcmp. Neither hunter
  measured the raw-equality hit rate; do not justify the rewrite on it, but
  for byte-deterministic unchanged regressions it may collapse the run to an
  I/O-bound sweep.
- **Tier 2 - compact semantic fingerprints:** for raw-different blobs,
  compute one 128-bit canonical fingerprint per blob in parallel and retain
  ONLY the fingerprint, never the `CanonTile`. Equal fingerprints mark spans
  identical. Only fingerprint-different blob pairs proceed to detail, and
  those are re-decoded on demand - a second decode on the exceptional path
  beats retaining every decoded tile on all paths.
- Concurrency shape (codex): three parallel passes over dense arrays (raw
  fingerprints, semantic fingerprints, detail pairs), compact results into
  pre-sized arrays; no central owner handing decoded tiles through channels,
  no `Mutex<PmtilesReader>` (CONVERGENT rejection - mmap removes the
  ownership problem instead of contending on it).

### P3 (codex, high-conviction; folds in Fable's decode hygiene): comparison-native decoder

The existing `CanonTile` graph is convenient for tests, wrong as the primary
representation for ~600K blobs. Build a decoder with two modes:

- `FingerprintMode`: hierarchical fixed-size descriptors - dictionary values
  canonically ordered by bit-exact representation, feature tags resolved to
  compact dictionary ids (no per-feature string clones), geometry varints
  decoded straight into component descriptors (no `Vec<u32>` intermediate),
  streaming component/feature/layer/tile digests (no serialized byte
  buffers anywhere - Fable's streaming-hash point is the same fold).
  A `FeatureDigest { id, geom_type, attrs: Digest, geometry: Digest,
  structure: StructureSummary }` with component/ring/vertex counts and
  bboxes makes the later matcher cheap.
- `DetailMode`: materializes only what is needed to explain a mismatch.
- Canonicalization rules stay explicit and identical: layer/feature/merged-
  component order ignored, polygon ring order preserved, attr types and
  float bits exact (signed zero, NaN payloads).
- Decode hygiene that folds in here rather than standing alone (Fable):
  presize gzip output from the ISIZE trailer, per-worker scratch via the
  existing `map_init` pattern, intern layer strings once per layer.
- Risk and its mitigation (codex): accidentally changing the definition of
  semantic equivalence. Keep the old canonicalizer as a TEST-ONLY
  differential oracle - permuted-order fixtures, bit-distinct floats,
  duplicate ids, anonymous ocean features, holes, multipart geometry; run
  old and new engines over the real archive pairs and compare complete
  reports; delete the old path once equivalence is demonstrated. Never a
  runtime switch.

### P4 (CONVERGENT diagnosis; combined redesign): the changed-feature matcher and Hausdorff itself

Lower priority than P1-P3 (it only runs after tile fingerprints differ) but
it is what any geometry-changing landing hits:

- Exact stage by precomputed digests: hash-bucket features/components once,
  O(k) instead of O(k^2) serializations; with tol=0, hash-equal skips
  distance math entirely.
- Residual pairing: partition by geometry type + structural signature; bbox
  lower bounds prune impossible matches; then either cheap-proxy greedy
  (centroid/bbox-center, computed once, sorted, consumed - Fable) or a
  sparse-candidate-graph deterministic minimum-cost matching (codex, the
  semantically stronger endpoint). Exact Hausdorff only on surviving final
  pairs. Both hunters note the current greedy is already order-dependent and
  not principled: this is a semantics change, not a semantics loss, and the
  unmatched-anonymous behavior should be specified deliberately while
  pre-1.0 allows it. Bit-stability escape hatch (Fable): keep exact-greedy
  for small groups (k^2 <= 64) if classification drift matters.
- Hausdorff internals: bbox-distance early reject; early-break directed scan
  with a rolling start index (rings traverse in order, so near-identical
  rings go ~O(n+m) - exactly the regress workload); i64/u64 arithmetic
  (extent-scale deltas need no i128); integer isqrt instead of the 64-step
  binary search; KD-tree or similar only for genuinely long rings, brute
  force below that.

## Report/aggregation fixes (CONVERGENT, do inside the rewrite)

- `report.diffs` (`Vec<TileDiff>`) is unbounded, retained, and only its
  len() is consumed - replace with counts + compact differing ranges +
  bounded examples. Protects broad-difference runs from a second,
  report-induced blowup.
- Attrs as group keys: hash of attrs, not cloned `Vec<(String, AttrVal)>`
  into BTreeMap keys.
- Displacement distributions via compact frequency tables, multiplied by
  span multiplicity.

## Emergency-only local patch (codex; NOT the plan)

If the OOM must die before the rewrite lands: memo retains
`HashMap<BlobKey, u128>` fingerprints only, re-decoding both blobs on
mismatch (plus `Arc` instead of deep clone on hits). Stops the germany OOM;
does nothing for the serial runtime. Both hunters explicitly reject
capping/LRU-ing the memo as the fix - the memo should not exist.

## Complete minor-wins ledger

Every remaining small finding from both reports, none dropped. Each is a
real win; the only claim above is that none of them substitutes for the
rewrite. Most become natural internals of P1-P4; the rest are
afterwards-if-this-works cleanup.

- Presize the gzip output Vec from the gzip ISIZE trailer (last 4 bytes give
  the exact decompressed size for free); today `gzip_decompress` streams
  into `Vec::new()` with no hint. (Fable)
- Reuse per-worker decode scratch via rayon `map_init` - the assemble phase
  already uses this exact pattern. (Fable)
- Intern layer key/value strings once per layer (`Arc<str>` or index-based
  attrs resolved against the layer table) instead of cloning dictionary
  strings into every feature. (both)
- Kill the per-feature `Vec<u32>` intermediate in `decode_packed_u32`;
  decode geometry varints directly into point/component form. (both)
- Streaming canonical hash: make the canonical writers generic over a
  Write-like sink so `canon_hash` feeds xxh3 directly and
  `feature_geometry_bytes` / `canonical_component_bytes` become digests, not
  allocated buffers. (both)
- Cache component sort keys / use fixed-size descriptors instead of freshly
  serialized `Vec<u8>` sort keys in `sort_components`. (both)
- Merge layers by sorted name directly, without constructing `BTreeMap` /
  `BTreeSet` per tile in `compare_tiles`. (codex)
- Return `Arc<CachedTile>` (or references) instead of deep-cloning on every
  memo hit - enormous churn reduction on an 87%-deduplicated archive even
  though the memo itself is slated for deletion. (both; emergency-tier)
- `ceil_sqrt`: replace the ~64-iteration binary search with an integer
  isqrt. (both)
- i64/u64 distance arithmetic instead of i128 - tile-extent deltas fit
  trivially. (Fable)
- Bbox-distance lower bound and rolling-start-index early exit inside
  `discrete_hausdorff` (near-identical rings go ~O(n+m)). (Fable; P4
  internals)
- KD-tree or similar nearest-point index for genuinely long rings, brute
  force below the crossover. (codex; P4 internals)
- Replace `BTreeMap` with `FxHashMap` where ordering is not load-bearing.
  (codex; afterwards)
- Reuse gzip buffers across decodes. (codex; afterwards, subsumed by
  map_init scratch)
- Stop expanding and re-sorting directory entries in `PmtilesReader` - the
  directories are already ordered and run-length encoded; preserving runs is
  a win on its own and leads naturally into P1. (codex)
- Drop or slim `report.diffs` (unbounded `Vec<TileDiff>`, only its len() is
  printed - dead weight on a 1.3M-differing-tile run). (both)
- Group anonymous features by a hash of attrs instead of cloning
  `Vec<(String, AttrVal)>` into BTreeMap keys. (both)
- Intern layer names in report aggregation; store displacement
  distributions as compact frequency tables rather than appending
  indefinitely. (codex)

## What NOT to do (CONVERGENT)

- No LRU/cap tuning of `BlobMemo` - the memo should not exist.
- No `Mutex<PmtilesReader>` parallelism.
- No env-var gates, experiment routing, or thread-count knobs; one intrusive
  branch, benchmark, keep/revert.
- No generic/async PMTiles reader API or reader/writer symmetry refactor as
  part of this work - afterwards, if this works.

## Validation and instrumentation plan (codex)

Always-on counters in the report (no hidden switches): addressed tiles,
directory runs, unique blobs, unique blob pairs, raw-equal pairs+tiles,
canonical-equal pairs+tiles, detailed pairs+tiles, per-pass wall time, peak
RSS. Benchmark matrix: self-comparison (raw tier), semantically-equal
reordered archives (fingerprint tier), the denmark pair, the germany pair
(the OOM case), and an intentionally broad geometry change (the matcher).
Differential-test the full report against the current binary before deleting
the old path.

## Expected outcome

High confidence on memory: the structure responsible for the germany OOM is
deleted. High confidence on direction for throughput: decode moves from one
core to all cores, identical raw blobs skip decompression, identical
semantics retain only 16-byte fingerprints, PMTiles runs and blob-pair
multiplicity replace per-tile repetition, and full canonical geometry exists
only transiently for true mismatches. Multiple-fold speedup even with a poor
raw-equality rate; order-of-magnitude when unchanged tiles dominate. The
end-state bottleneck should be compressed-data bandwidth and parallel
canonicalization, not serialized ownership, deep cloning, and heap
retention.
