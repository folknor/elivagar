# The Pipeline

Four sequential phases over the same PBF input.

## phase12 - PBF read and feature emission

A single pass reads the PBF, resolves way and relation geometry, matches tags
against the Shortbread profile, and emits sort records into partitioned chunk
files.

Node coordinates come from one of three places. If the PBF declares
locations-on-ways, coordinates are already embedded in the ways and no node
index exists at all - this is the production shape. If the PBF is sorted by
type then ID, nodes go into a compact in-RAM store with FOR compression.
Otherwise there is a flat memory-mapped fallback, behind size guardrails,
because that path is dangerous at scale.

This is the largest phase at every input size, and at planet scale it is about
60% of wall time.

## ocean - coastline emission

Ocean geometry from the `--ocean` inputs, covered in
[Ocean Input](./ocean). With the world artifact named, this phase computes
only the boundary band near the extract's bbox edge; at world bounds the band
is empty and the phase costs nothing.

## sort - partition bookkeeping

Near-zero. The external sort is partitioned by Hilbert tile-id range at
*write* time, so this phase only finalizes bookkeeping; the actual k-way merge
is deferred into assemble and happens per partition, in parallel.

## assemble - encode and write

Parallel per-partition readers stream merged records into a rayon encode
stage, which builds MVT layers, compresses, and hands tiles to the PMTiles
writer under a byte-budgeted claim window.

Hot partitions split further: any partition over twice the split target breaks
into contiguous tile-range pieces with boundaries picked from a byte-quantile
pre-scan, each its own worker job. Without that, one dense metropolitan z14
block sits behind a single slot and starves the writer while the claim window
never binds.

Each worker also overlaps its own reading and encoding - the merge reader
pulls the next batch while the current one encodes, rather than idling through
it.

## Resuming

```sh
elivagar run input.osm.pbf -o out.pmtiles --skip-to assemble
```

`--skip-to ocean` reuses the PBF chunks from a previous run, `--skip-to sort`
reuses PBF and ocean chunks, `--skip-to assemble` jumps straight to tile
assembly.

A resume validates the checkpoint's input identity, producer config and ocean
source against the current run and refuses a mismatch, so `--skip-to` cannot
silently blend two contracts into one archive. Earlier checkpoint versions
recorded computed ocean as a bare string naming no source, which meant chunks
built from one shapefile could be reused by a resume naming another while the
archive's metadata described the second. Both arms now carry their producer's
identity.

## Memory

Every stock is bounded by construction rather than by input size:

| Stock | Bound |
|---|---|
| Sort chunk buffer | `--sort-budget`, default 1G |
| In-flight way blocks | `--way-budget`, and a hard count ceiling of `--threads` blocks |
| Relation blocks | capped, spills to a filtered re-read past it |
| Way index | memory-mapped, disk-backed |
| Encoded-but-unwritten tiles | the assemble claim window, default 2 GiB |
| Tile dedup map | capped at 1M entries |

The planet build peaked at 12.7 GB with 30.5 GiB available.

One lesson is embedded in that table. Every queue between a parallel producer
and an ordered consumer needs an explicit window or byte bound decided at
design time - an unbounded queue plus an ordered-or-slow consumer plus one
straggler is how memory ends up scaling with input after all. That pattern bit
this pipeline three times in a single day before the rule was written down.

A second one: warm scratch whose lifetime exceeds its work item ratchets to
the worst item it ever served, times the pool width. Scratch lives exactly as
long as its work item, and reuse beyond that has to prove its wall-time win
against the retention it buys.
