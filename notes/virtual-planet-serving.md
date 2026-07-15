# Virtual planet serving: hypotheses for never storing the full artifact

Status: 2026-07-08, hypotheses only - no code, no spec. Companion to
`notes/planet-30gb-roadmap.md` (the batch full-build track). This note
captures a design conversation; it is deliberately not thought through
end-to-end. The nidhogg service architecture owns most of these decisions;
what belongs to elivagar is the library boundary named at the bottom. The
nidhogg-side half (server components, update-cycle revisions, cache
lifecycle, invalidation feed) is `../nidhogg/VIRTUAL-TILES.md`.

## The idea

Production may never need a stored planet-scale PMTiles artifact at all.
Generate tiles at request time on cache miss, cache aggressively, and
pregenerate only the slice of the pyramid where batch amortization
actually wins. The full-build record track (the roadmap) is unchanged by
this: it remains the cold-start story, the disaster-recovery story, and
the committed corpus baseline that the serve path is verified against.

This is a return to form, not an invention: render-on-demand plus cache
was the OSM raster architecture for a decade (mod_tile/renderd). Batch
planet artifacts won for vector tiles because nobody had a feature store
fast enough to assemble a tile at request time. That assumption is the
thing to attack.

## Why the economics work

- Tile demand is an extreme power law. z13-z14 are ~95% of the tile count
  and the overwhelming majority are never requested by anyone, ever.
  Pregenerating a full planet z14 layer is mostly wasted work by
  construction.
- Low zooms invert: z0-z10 is ~1.4M tiles, always hot, and expensive per
  tile (planet-spanning geometry, ocean, aggregation) - exactly where
  batch amortization wins. So the natural split is: pregenerate low/mid
  zooms into a small artifact; on-demand + cache for high zooms. The
  boundary zoom is a measured decision, not a guess (see open questions).
- The compute is affordable: germany assembles 300K unique tiles in 17.4s
  (`92803833`, plantasjen, `11dc159`) - ~58us of amortized work per tile.
  The design crux is read granularity, not CPU: a cold tile must read
  kilobytes, not a partition.
- Freshness becomes the differentiator, not just cost. OSC diff ->
  pbfhogg -> update affected record-store partitions -> invalidate
  affected cached tiles. Minute-fresh planet vector tiles on desktop
  hardware. Planetiler's README lists "only full imports, no real-time
  updates" as a known limitation of the entire batch field - this flanks
  the field rather than racing it.

## Architecture sketch

1. **The durable artifact is the sorted record store, not the archive.**
   P3 already made the intermediate Hilbert-tile-id-ordered and
   partitioned by tile range - it IS a spatial index. Add a per-tile-range
   offset index inside partitions (a directory, in effect) and "assemble
   tile (z,x,y)" becomes: locate slice, read, decode, run the existing
   encode path. Today the record store is a scratch format deleted by
   `brokkr clean`; serving from it gives it a versioning and stability
   story it does not currently have.
2. **elivagar becomes a library with two callers.** The public API grows
   from `elivagar::run(&TilegenConfig)` to also expose
   assemble-tile-range -> MVT bytes (and the record-store reader beneath
   it). The batch binary is one caller; nidhogg tile serving is another.
   Serve-path code that is really tile-domain work - partition slice
   reads, per-tile-range indexing, io_uring read backends, canonical
   full-tile handling - lives in elivagar as library portions; HTTP,
   cache policy, and quotas live in nidhogg.
3. **Descriptor taxonomy for a virtual archive** (the pbfhogg
   apply-changes model, generalized to serve time): every requested tile
   is served from exactly one of {cached blob | copy-range from a
   previous generation | generate-now}. "Never store the artifact" is the
   limit case of the incremental-build idea from
   `../pbfhogg/reference/pbfhogg-techniques-for-elivagar.md` - the
   archive becomes a directory over descriptors instead of a file.
4. **Shared store with the query API (to investigate).** nidhogg ingest
   is already building a queryable feature store for the Overpass-style
   API. Whether the tile record store and the query store can be one
   artifact - or at least one ingest pass - is a nidhogg-side design
   question with large storage and freshness payoff.

## Cache substrate ladder

Adopt in phases; each earns the next only if measurements demand it.

1. **Userspace miss-handler** in the nidhogg tile server. No kernel
   machinery, fully testable, same architecture. This is the default and
   probably the end state for the hot cache.
2. **EROFS + overlayfs generations** for the pregenerated/snapshot
   layers: chunk-level dedup collapses the millions of byte-identical
   ocean full-fill tiles into shared chunks (the pipeline's dedup counter
   already proves the redundancy: 15.5M of norway's 16.3M ocean tiles are
   reused payloads); Linux 7.2 sparse-pcluster holes keep unmaterialized
   tile slots as real holes (SEEK_HOLE correct, no overlayfs copy-up
   ballooning); serving is kernel page-cache zero-copy. Constraint:
   EROFS images are immutable - generations, not mutation.
3. **Optional compatibility facade**: file-backed mounts + fanotify
   pre-content hooks (the mechanism that replaced EROFS's removed FSCACHE
   backend in 7.2) can present a byte-addressable, seemingly complete
   PMTiles file whose ranges materialize on first read. Earns its
   complexity in exactly one scenario: keeping the PMTiles-file-shaped
   ecosystem (off-the-shelf pmtiles servers, range-request static
   hosting, `elivagar inspect`) working against an artifact that does not
   physically exist. Phase-3 material; nothing in phases 1-2 forecloses
   it.

## Kernel watch items

- **io_uring "io-slots" (Axboe, proof-of-concept branch):** registered
  buffers extended to carry a pre-built `struct bio`, DMA-mapped upfront;
  O_DIRECT submission collapses to slot lookup + bio submit. ~60% per-core
  I/O throughput claimed. Touches io_uring + NVMe PCI + block core; NOT
  mainline. This targets exactly the serve path's regime (huge volumes of
  small random O_DIRECT reads) and the roadmap H4's O_DIRECT scratch
  reads. Stance: design the read paths so an io_uring backend is
  adoptable (pbfhogg already maintains uring/direct backends - familiar
  territory), never depend on it.
- **EROFS in Linux 7.2:** optimized chunk mapping for chunk-based inodes;
  sparse pcluster holes; FSCACHE backend removed (replacement:
  file-backed mounts + fanotify pre-content hooks, see ladder above).

## What this demands from elivagar NOW (cheap, directional)

These cost little today and are expensive to retrofit:

- Keep "assemble one partition / tile range" separable as a library
  boundary, not a binary-internal detail. P3's partition readers nearly
  did this already; do not fuse them back into orchestration.
- The per-tile-range offset index inside partitions doubles as a batch
  win (finer assemble scheduling, roadmap H8) - when either track builds
  it, build it once for both.
- A serve-path variant of the correctness gate: on-demand output for a tile
  must match batch output (`elivagar corpus check` for exhaustive semantic
  equality against the committed baseline, or `elivagar regress --against
  <batch archive>` for attribution).
- Record-store format changes should start carrying a version marker the
  moment anything outside one run reads it.

## Open questions

- Cold-tile p99: what does a dense-city z14 cost end-to-end (index
  lookup, slice read, decode, encode) at each candidate index
  granularity? This number picks the pregeneration boundary zoom.
- Scrape economics: a crawler over z14 forces worst-case generation.
  Quotas, generation concurrency caps, detect-and-precompute - nidhogg
  policy, but the library must expose cost signals.
- Storage trade: the record store is larger than the archive it replaces
  (germany: 12.3 GB records vs 2.8 GB PMTiles). Chunk compression
  narrows it; the low/mid-zoom pregenerated artifact adds back. Price at
  NA scale.
- Invalidation granularity: mapping an OSC diff's changed elements to
  affected tile ids cheaply. pbfhogg sees every changed element at merge
  time and could emit changed-bbox/tile-id sets as another injection
  (roadmap H2 family).
- Where exactly the elivagar-library / nidhogg-service boundary lands -
  who owns the cache, who owns the record-store lifecycle.
