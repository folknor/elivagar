*Note: Script references below predate the dev tool. Use dev for current equivalents.*

# Performance

Elivagar processes Denmark (483 MB PBF, 52.5M nodes, 6.6M ways) in under
30 seconds on a modern desktop, producing a complete Shortbread PMTiles
archive with 26 layers at z0-14. The pipeline is designed around external
merge sort and memory-mapped indices, keeping RAM usage constant regardless
of input size.

<!-- TODO: update before release -->

## End-to-end benchmarks

Denmark extract (483 MB PBF) to Shortbread PMTiles, best of 3 runs on Linux
6.18, Ryzen 9 7950X, 64 GB RAM, NVMe:

<!-- TODO: update before release -->

| Tool | Total | PBF + Features | Ocean | Sort | Assembly |
|------|-------|----------------|-------|------|----------|
| **elivagar** | **29s** | 20s | 2.8s | 0.7s | 3.4s |
| Tilemaker | 29s | -- | -- | -- | -- |
| Planetiler 0.10 | 41s | -- | -- | -- | -- |

Phase breakdown for elivagar only -- Tilemaker and Planetiler do not expose
per-phase timing. Measured with `scripts/bench.sh`, logged to
`benchmarks.tsv` for tracking over time.

The PBF + Features phase dominates at ~70% of wall time. Within it, the
main thread spends most of its time on node index lookups (random reads into
a memory-mapped coordinate index), while rayon worker threads handle
geometry simplification, clipping, and sort record emission in parallel.

## Output size

Denmark extract (483 MB PBF), gzip level 6, z0-14:

<!-- TODO: update before release -->

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| With ocean | **380 MB** | 406 MB | 308 MB |
| Without ocean | **317 MB** | 406 MB | 308 MB |

Elivagar produces smaller output than Planetiler. The remaining gap versus
Tilemaker comes from three factors: Tilemaker uses more aggressive
per-layer simplification (10-40x at mid-zooms), geometric union for feature
merging (eliminating shared polygon edges), and libdeflate compression which
produces slightly better ratios than zlib-ng.

Output was validated tile-by-tile against both tools. Detailed analysis in
[`tile-comparison-2026-02-24.md`](tile-comparison-2026-02-24.md).

## PMTiles writer

Elivagar includes a hand-rolled PMTiles v3 writer rather than using the
`pmtiles-rs` crate. It writes Hilbert-ordered tile entries directly,
avoiding intermediate data structures and redundant sorting.

Synthetic benchmark (unique gzipped payloads, Hilbert-ordered), best of 5
runs:

<!-- TODO: update before release -->

| Tiles | elivagar | pmtiles-rs 0.20 | Speedup |
|------:|---------:|----------------:|--------:|
| 100K | 28 ms | 68 ms | 2.4x |
| 500K | 110 ms | 309 ms | 2.8x |
| 1M | 223 ms | 646 ms | 2.9x |

Run with `scripts/bench-pmtiles.sh [tiles] [runs]`.

## Optimization highlights

### Memory allocator

The `mimalloc` global allocator replaces the system allocator. This is
critical for rayon-based parallel workloads -- mimalloc's thread-local
heaps and size-class sharding eliminate contention that would otherwise
serialize allocations across worker threads.

### Per-worker scratch pools

Geometry processing functions reuse pre-allocated buffers instead of
allocating per call:

- **Polygon clipping** -- `clip_polygon_into()` takes reusable
  double-buffers, avoiding per-call `Vec` allocation across 8.9M clipping
  operations.
- **Douglas-Peucker simplification** -- `simplify_into()` reuses keep-array
  and result buffers, hoisted outside the zoom loop with `swap` instead of
  re-allocating per level.
- **Feature assembly** -- per-rayon-worker `Vec` pools for geometry commands
  and tag lists, reclaimed after MVT encoding.
- **Geometry merging** -- `MergeScratch` hoists `HashMap` and geometry
  buffers, reused across tiles within each worker thread.

These pools are managed via `rayon::ThreadLocal` and the `map_init` pattern
in `encode_tile_batch`, so each worker thread gets its own scratch space
with no cross-thread synchronization.

### External merge sort

The sort phase uses an external merge sort with chunk files on disk. During
PBF processing, sort records (tile ID + encoded feature) are accumulated in
memory and flushed as sorted chunk files when a batch fills. A k-way merge
via binary heap then produces the final sorted stream. This keeps memory
usage bounded regardless of input size -- at planet scale (73 GB PBF), the
sort phase produces 100+ GB of chunk data without requiring it all in RAM.

### Memory-mapped indices

Node coordinates and way geometries are stored in flat memory-mapped files
(`NodeIndex` and `WayIndex`). The node index is a direct-mapped array keyed
by OSM node ID -- for planet data this reaches ~96 GB, far exceeding
typical RAM. The kernel's page cache handles the working set automatically.

### Projection lookup table

Mercator projection (`project_e7`) uses an 18-bit lookup table (262K
entries, 2 MB) with linear interpolation instead of computing
`tan`/`cos`/`ln` transcendentals per coordinate. Error is 0.03 pixels at
z14. This reduced per-way processing time by 14% on average and 55% at P99.

### Subpixel early exit

Before running Douglas-Peucker simplification at each zoom level, a
bounding-box check (`merc_bbox_is_subpixel`) tests whether the entire
geometry fits within a single pixel. When it does, the zoom loop breaks
early. This reduced simplification CPU time by 35% and eliminated 630K
feature-zoom combinations (3.7% of total).

### Linux kernel I/O hints

The pipeline uses `madvise` and `fadvise` to guide the kernel's page cache
and readahead behavior. These hints are tuned per access pattern:

- **Node index** -- `MADV_RANDOM` disables readahead (each page fault is a
  random 4 KB read; readahead wastes bandwidth). `MADV_HUGEPAGE` requests
  transparent huge pages for TLB efficiency. On machines where the index
  fits in RAM, `MADV_POPULATE_READ` pre-faults all pages before processing
  begins.
- **Way index** -- `MADV_RANDOM` for the same random-access reason.
- **Ocean shapefile** -- `MADV_SEQUENTIAL` for the linear bbox scan.
- **Sort chunks** -- `FADV_SEQUENTIAL` on reads, `FADV_DONTNEED` when a
  chunk is drained (releases pages immediately instead of polluting cache).
  Write-side also uses `FADV_DONTNEED` to avoid evicting hot node index
  pages.
- **PMTiles temp blob** -- `FADV_SEQUENTIAL` + `FADV_DONTNEED` on
  read-back.

On a 32 GB machine processing Denmark (where the 110 GB node index exceeds
RAM by 3.4x), the main thread spends 86% of its time in kernel page fault
handling. These hints do not eliminate the I/O cost, but they prevent
readahead waste and page cache pollution from making it worse.

### Hilbert-ordered PMTiles

Tiles are sorted by Hilbert curve ID before writing. This groups spatially
adjacent tiles together in the output file, improving sequential read
performance for map viewers that request tiles in spatial clusters. The
Hilbert ordering is computed during the sort phase and carried through to
the PMTiles writer, so no re-sorting is needed at write time.
