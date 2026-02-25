# Architecture

Elivagar is a single-pass tile generator: it reads an OpenStreetMap PBF
file, matches features against the Shortbread schema, sorts them by tile
ID, and assembles MVT protobuf tiles into a PMTiles v3 archive. The entire
pipeline runs in four sequential phases, designed so that memory usage stays
bounded regardless of input size.

```
                         ┌──────────────────────────────────┐
                         │          OSM PBF file            │
                         └────────────────┬─────────────────┘
                                          │
                    ┌─────────────────────┤
                    ▼                     ▼
            ┌──────────────┐     ┌──────────────────┐
            │  Node Index  │     │    Way Index      │
            │  (mmap, flat │     │  (mmap, offsets + │
            │   96 GB max) │     │   packed coords)  │
            └──────┬───────┘     └────────┬──────────┘
                   │                      │
                   └──────────┬───────────┘
                              ▼
              ┌───────────────────────────────┐
              │  Phase 1+2: PBF Read +        │
              │  Feature Processing           │
              │  (tag match → project →       │
              │   simplify → clip → encode)   │
              │                               │
              │  Rayon batches: ways + rels   │
              └───────────────┬───────────────┘
                              │ sort records
                              ▼
              ┌───────────────────────────────┐
              │  Ocean Shapefile Processing   │  ← optional
              │  (mmap .shp → bbox filter →   │
              │   clip → encode, parallel)    │
              └───────────────┬───────────────┘
                              │ more sort records
                              ▼
                  ┌───────────────────────┐
                  │  Sorted chunk files   │
                  │  (~1 GB each on disk) │
                  └───────────┬───────────┘
                              │
                              ▼
              ┌───────────────────────────────┐
              │  Phase 3: External Merge Sort │
              │  (k-way merge via BinaryHeap) │
              └───────────────┬───────────────┘
                              │ globally sorted stream
                              ▼
              ┌───────────────────────────────┐
              │  Phase 4: Tile Assembly       │
              │  (MVT encode + gzip + write)  │
              │                               │
              │  Reader → Rayon → Writer      │
              │  (3-thread pipeline)          │
              └───────────────┬───────────────┘
                              │
                              ▼
                   ┌─────────────────────┐
                   │  PMTiles v3 archive │
                   │  (Hilbert-ordered)  │
                   └─────────────────────┘
```

The public API is a single function: `elivagar::run(&TilegenConfig)`. The
`TilegenConfig` struct specifies input PBF path, output path, zoom range,
ocean shapefile paths, and runtime options. Everything else is internal.


## Phase 1+2: PBF Read and Feature Processing

This phase does the most work. It reads the PBF file in a single pass,
building in-memory indices for node coordinates and way geometries, then
processing each matched feature through the full geometry pipeline:
Mercator projection, Douglas-Peucker simplification at each zoom level,
Sutherland-Hodgman polygon clipping to tile boundaries, and MVT command
encoding. The output is a stream of sort records written to chunk files
on disk.

### Single-pass PBF read

OSM PBF files contain three element types in a fixed order: nodes first,
then ways, then relations. Elivagar reads the file exactly once. Nodes are
stored in the node index as they arrive. When the first way element
appears, the node index is finalized and converted to a read-only view.
Ways reference nodes by ID to resolve their coordinates, and relations
reference ways to assemble multipolygon geometry.

This single-pass design avoids re-reading the PBF (which can be 73 GB at
planet scale), at the cost of requiring the node and way indices on disk
during processing.

### Node index

The node index (`NodeIndex` / `NodeIndexReader`) is a flat memory-mapped
file addressed directly by OSM node ID. Each entry is 8 bytes: 4 bytes
latitude + 4 bytes longitude, both as E7 integers. The file grows in 1 GB
increments via `mmap`.

At planet scale, OSM has roughly 8.5 billion nodes with IDs up to about
12 billion, so the index file reaches approximately 96 GB. This exceeds
typical server RAM, making it the single largest I/O bottleneck. The kernel
page cache handles the working set transparently -- geographic locality in
the PBF means that ways within a blob tend to reference nearby nodes, giving
decent cache hit rates even when the index is larger than RAM.

A two-level index (blocks of 4096 nodes) was considered and rejected: with
12 billion possible IDs distributed fairly continuously, a block index
would be nearly the same size as the flat index while adding indirection
overhead.

Stored coordinates are XORed with a mask (`0x55555555`) so that the
all-zeros pattern on disk means "no node here" rather than representing
latitude 0, longitude 0.

### Way index

The way index (`WayIndex`) stores resolved way geometries for relation
processing. It consists of two files: an offset index (keyed by way ID,
12 bytes per entry: 8-byte data offset + 4-byte coordinate count) and a
packed coordinate data file (sequential pairs of E7 lat/lon). During the
way phase, coordinates are appended to the data file via buffered writes.
Before relation processing begins, the data file is finalized and
memory-mapped for random reads.

### Tag matching

The Shortbread schema defines 26 MVT layers with specific tag filters and
attribute mappings. Elivagar compiles these rules into Rust code rather
than interpreting a configuration file at runtime. Each OSM element's tags
are matched against all layers simultaneously. The `match_element` function
returns a `SmallVec` of `LayerMatch` results, each containing the target
layer, zoom range, expected geometry type, and extracted attributes.

Tag matching is split across domain modules (`water.rs`, `boundaries.rs`,
`land.rs`, `streets.rs`, `transport.rs`, `pois.rs`) that each handle
their subset of the 26 layers.

### Parallel way processing

Ways are the most numerous element type (6.6 million for Denmark, hundreds
of millions at planet scale) and the most expensive to process, because
each way requires resolving node coordinates, projecting to Mercator,
simplifying at multiple zoom levels, and clipping to tile boundaries.

The main PBF read thread collects ways into batches of 8192. Each batch is
dispatched to rayon for parallel processing (`flush_raw_way_batch`). Within
rayon, each way independently:

1. Resolves node coordinates from the read-only `NodeIndexReader`
2. Matches tags against the Shortbread schema
3. Projects coordinates from WGS84 E7 to Mercator [0,1] space
4. For each zoom level (z14 down to z_min), runs Douglas-Peucker
   simplification with a zoom-appropriate tolerance
5. Computes which tiles the geometry intersects
6. Clips to each tile's bounding box (with an 8-pixel buffer)
7. Encodes as MVT geometry commands
8. Serializes the feature data into the wire format and emits sort records

Relation batches (1024 elements) follow the same pattern: the main thread
resolves way geometries from the way index, then dispatches to rayon for
multipolygon assembly and per-zoom processing.

### Wire format

Sort records carry an opaque byte payload (the "wire format") between the
PBF processing phase and the assembly phase. The format is:

```
u64   osm_id
u8    geom_type (1=point, 2=line, 3=polygon)
u32   geometry command count
u32*  geometry commands (MVT encoding)
u8    attribute count
per attribute:
  u8    key length
  u8*   key bytes
  u8    value type (0=string, 1=int, 2=bool, 3=float)
  value bytes (type-dependent)
```

This is an ephemeral internal format -- it exists only within a single
pipeline run's temporary files and is never persisted across versions.
There is no version byte.


## Ocean Phase

When ocean shapefiles are provided, elivagar generates water polygon tiles
covering the world's oceans. This phase reads ESRI shapefiles
(`water-polygons-split-3857`) using memory-mapped I/O, filters shapes by
bounding box intersection with the PBF data extent, and processes matching
polygons in parallel with rayon.

Two shapefiles can be provided: a simplified version for zoom levels 0-7
(fewer vertices, faster processing) and a full-resolution version for z8-14.

### Land mask

During PBF processing, elivagar builds a `LandMask` -- a z8-resolution
bitset (256x256 grid = 8 KB) recording which cells contain land features.
The ocean phase uses this mask to skip clipping and encoding water polygons
for cells that are entirely land, avoiding unnecessary work for interior
regions.

### Output

Ocean features are emitted as sort records in the same wire format as PBF
features, written to additional chunk files that join the main sort. Each
rayon worker writes its own chunk files directly to avoid contention on a
shared writer.


## Phase 3: External Merge Sort

All sort records from the PBF and ocean phases are merged into a single
globally sorted stream. The sort key is a 64-bit integer encoding three
fields:

```
bits 63-16:  tile_id (Hilbert curve ID, 48-bit field)
bits 15-8:   layer index (0-25)
bits 7-0:    priority (within layer)
```

This packing ensures that all features for a single tile are adjacent in
the sorted output, grouped by layer -- exactly the order needed by the
assembly phase.

### Why external merge sort

In-memory sorting is not feasible at planet scale. A Denmark extract
(483 MB PBF) produces about 16 million sort records. A full planet
(73 GB PBF) would produce billions of records totaling over 100 GB.
External merge sort keeps memory usage bounded: records are buffered up to
a configurable chunk size (default 1 GB), sorted in-place, and flushed as
chunk files to disk. The final merge reads from all chunk files
simultaneously via a k-way merge.

### Chunk files

Each chunk file is a simple binary format:

```
u32 record_count
per record:
  u64 sort_key
  u32 data_length
  u8* data (wire format payload)
```

The in-memory buffer is sorted with `sort_unstable_by_key` (introsort,
no allocations) before each flush. After writing, the kernel is advised
to evict the chunk's pages from the page cache (`FADV_DONTNEED`) to
prevent sort data from displacing hot node index pages.

### K-way merge

The `SortReader` opens all chunk files and primes a `BinaryHeap` with the
first record from each. On each call to `next()`, it pops the smallest
entry, reads the next record from that chunk, and pushes it onto the heap.
When a chunk is fully consumed, its file descriptor is released with
`FADV_DONTNEED`.

For Denmark, this is typically a 3-5 way merge (a few GB of chunk data).
At planet scale, it would be a 100+ way merge, but the heap operations
remain O(log k) per record where k is the chunk count.


## Phase 4: Tile Assembly

The assembly phase reads the globally sorted stream and produces the final
PMTiles archive. It runs as a three-thread pipeline connected by bounded
channels:

```
┌──────────┐    sync_channel(1)    ┌──────────┐    sync_channel(1)    ┌──────────┐
│  Reader  │ ──────────────────▶   │ Encoder  │ ──────────────────▶   │  Writer  │
│ (thread) │   Vec<PendingTile>    │ (main +  │   Vec<EncodedTile>    │ (thread) │
│          │                       │  rayon)  │                       │          │
│ k-way    │                       │ MVT +    │                       │ PMTiles  │
│ merge    │                       │ gzip     │                       │ add_tile │
└──────────┘                       └──────────┘                       └──────────┘
```

**Reader thread:** Pulls records from the `SortReader`, groups consecutive
records with the same tile ID into `PendingTile` structs, and sends
batches of 4096 tiles to the encoder.

**Encoder (main thread + rayon):** For each batch, uses `rayon::par_iter`
with `map_init` to encode tiles in parallel. Each rayon worker:

1. Deserializes wire format records into `LayerBuilder` feature lists
2. Merges features with identical attributes within each layer (reducing
   feature count in the output MVT)
3. Encodes all non-empty layers into an MVT protobuf tile
4. Gzip-compresses the tile (level 6)

The `map_init` closure initializes per-worker scratch state that persists
across tiles: `EncodeScratch` for protobuf encoding, `MergeScratch` for
attribute-based geometry merging, and `Vec` pools for geometry commands
and tag lists. This avoids re-allocating buffers for every tile.

**Writer thread:** Receives encoded tiles and appends them to the PMTiles
writer in Hilbert order. Because tiles arrive already sorted by Hilbert
ID (from the sort phase), the writer can stream entries directly without
any re-ordering.

### PMTiles writer

The PMTiles v3 writer is hand-rolled rather than using the `pmtiles-rs`
crate. It produces clustered archives with Hilbert-ordered tile entries
and content deduplication (identical tiles share a single copy of the
compressed data).

The writer supports two storage modes:

- **In-memory:** All tile data and directory entries accumulate in `Vec`s.
  Suitable for small extracts.
- **Streaming:** Tile data is appended to a temporary file, and directory
  entries are written to a separate temp file with run-length encoding.
  At finalization, both are read back sequentially. This keeps RAM
  constant at planet scale, where tile data alone would reach several
  gigabytes.

The dedup hash map is capped at 1 million entries (~50 MB) to prevent
unbounded growth at planet scale, where unlimited dedup could reach 7 GB.
Ocean fill tiles (which are identical across large areas) are added early
and benefit most from dedup.


## Key Design Decisions

### Single-pass PBF read

Reading the PBF file once is a hard constraint for performance. At
planet scale, the PBF is 73 GB. A two-pass approach (first pass to build
indices, second pass to process features) would double I/O time.
Elivagar's single-pass design exploits the PBF's guaranteed element
ordering: all nodes before all ways before all relations. The node index
is built during the node section and sealed before way processing begins.

### External merge sort over in-memory sort

Sorting happens after feature processing rather than during it. Each
feature may produce records for multiple tiles across multiple zoom
levels, so the output is larger than the input. External merge sort
decouples memory usage from data volume: records are buffered in 1 GB
chunks, sorted, and flushed to disk. At planet scale this produces 100+
GB of sorted chunks without requiring proportional RAM. The alternative --
sorting everything in memory -- would require hundreds of gigabytes of
RAM or a fundamentally different architecture.

### Memory-mapped node and way indices

Using `mmap` for the node and way indices lets the operating system
manage the working set. When the index fits in RAM (64 GB machine
processing Denmark), pages stay resident and lookups are fast. When it
exceeds RAM (96 GB index on a 32 GB machine), the kernel pages data in
and out transparently. This scales from small extracts to planet without
code changes, at the cost of I/O-dominated performance when RAM is
insufficient.

The madvise hints (`MADV_RANDOM`, `MADV_HUGEPAGE`, `MADV_POPULATE_READ`)
are conditional on system RAM: `MADV_RANDOM` activates when the index
exceeds 50% of RAM (to suppress wasteful readahead), and
`MADV_POPULATE_READ` activates when it fits (to pre-fault all pages
before processing begins).

### Hilbert tile ordering

PMTiles v3 uses Hilbert curve tile IDs to achieve spatial locality in
the archive file. Elivagar computes Hilbert tile IDs during feature
processing and packs them into the sort key. After the external merge
sort, records are already in Hilbert order, so the assembly phase can
write tiles sequentially without re-sorting. This means the sort phase
does double duty: it groups features by tile (needed for MVT encoding)
and orders tiles spatially (needed for PMTiles).

### Per-rayon-worker scratch state

The `encode_tile_batch` function uses rayon's `map_init` pattern to
attach persistent scratch buffers to each worker thread. These include
the MVT protobuf encoding scratch (`EncodeScratch`), geometry merge
scratch (`MergeScratch`), and `Vec` pools for geometry commands and tag
lists. Because `map_init` runs the initializer once per worker and
reuses the result across iterations, each worker accumulates warm
buffers that grow to the maximum needed capacity and then stabilize.
This eliminates millions of small allocations per run without requiring
explicit thread-local storage.

### Wire format for sort records

The wire format is a compact binary encoding of a single feature's
geometry and attributes, designed for fast serialization during PBF
processing and fast deserialization during tile assembly. It is not
self-describing (no version byte, no schema) because it only lives
within a single pipeline run's temporary files.

Attributes are pre-filtered by zoom level at encoding time: each
attribute carries a minimum zoom, and the encoder only includes
attributes visible at the record's target zoom. This avoids carrying
unused attributes through the sort and re-filtering during assembly.

### Projection lookup table

Mercator projection involves transcendental functions (`tan`, `cos`,
`ln`) that cost 250-400 CPU cycles each. Since `project_e7` is called
for every coordinate of every way (billions of times at planet scale),
elivagar replaces the exact computation with an 18-bit lookup table
(262K entries, 2 MB) using linear interpolation. The maximum error is
0.03 pixels at z14 -- imperceptible in rendered output. The table is
initialized once via `OnceLock` and shared across all threads.

### Compiled Shortbread schema

The 26-layer Shortbread tag matching rules are compiled into Rust code
rather than interpreted from a configuration file. Each layer domain
(water, boundaries, land, streets, transport, POIs) is a separate module
with pattern-matching functions. This eliminates parsing overhead and
enables the compiler to optimize tag comparisons, though it means schema
changes require recompilation.
