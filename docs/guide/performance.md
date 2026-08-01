# Performance

## Planet

A full planet build, 2026-07-31. The host had 30.5 GiB of RAM available; the
run used 12.7 GB of it.

| | |
|---|---|
| Input | 90.5 GB enriched PBF, locations-on-ways, planet seq 4912 |
| Wall | **571.7s**, 9m32s |
| Peak RSS | 12.7 GB |
| Output | 58.7 GiB PMTiles, z0-z14, 26 layers |
| Tiles | 269.8M addressed, 52.2M unique, 80.7% deduplicated |
| Features | 2.52 billion |

Host: Ryzen 9 9950X3D2, 16c/32t, single NVMe.

By phase: phase12 61% of wall, assemble 38%, sort and ocean effectively free -
ocean because at world bounds the artifact serves everything and the boundary
band is empty by construction.

## Extracts

| Dataset | Wall |
|---|---|
| denmark | 6.9s |
| germany | 26.7s |
| north-america | 130.1s |

Locations-on-ways variants, same host.

## Reading these numbers

**The planet figure is for enriched input.** The preprocessing pass that
embeds node coordinates into ways is not in it. End to end, including
[pbfhogg](https://github.com/folknor/pbfhogg) preprocessing, the honest number
is roughly 20 minutes.

**Profiles differ.** planetiler's published table uses OpenMapTiles; this is
Shortbread. They are not the same work, and a ratio between them is not a
like-for-like speedup.

**Snapshots differ.** Ours is planet seq 4912, 2026-02-23.

For context and not as a claim: planetiler's published table lists 2h38m on a
16 cpu / 32 GB machine, and 19 minutes on 192 cores / 720 GB.

## Configuration that actually moves the number

**`--way-budget`.** The one setting with a factor-of-two on it at planet
scale, and a good illustration of why input properties matter. Admission to
the way stage is charged as decompressed block size times a multiplier, and
extracts pack about 8,000 ways per blob while the planet packs roughly 66,500.
At the old 768 MB default, a planet blob cost several hundred MB of budget, so
two or three admitted concurrently and the machine ran at 7.2 of 32 cores.
Raising it took the planet from 1062.5s to 571.7s.

The default is now 8G under locations-on-ways, which sits past the knee. Note
that admission also stops at a hard ceiling of `--threads` blocks in flight,
so real memory is bounded by that count regardless of the byte budget - which
is why raising it cost about 1.1 GB of peak anonymous memory and nothing in
peak RSS.

**`--compress-sort-chunks lz4`.** Costs about 2% of wall in compression CPU
and cuts scratch I/O by roughly 2.6x. On extracts where sort data fits in page
cache it is overhead without benefit; at planet scale, where merge reads
overflow the cache, the trade is expected to invert.

**`--sort-budget`, `--assemble-budget`.** Memory ceilings rather than speed
knobs. Lowering the sort budget cuts phase12 memory at the cost of more merge
chunks.

## What is not worth tuning

The sort phase itself is a rounding error at every measured scale. Simplify
and rescale internals are not the cost - calling them on small fragments is
the win, and that is structural, already done. Dedup and seam reconciliation
are cheap everywhere. Attempts to micro-tune tag matching and the inner
simplification loops died on DRAM latency.

The allocator question is closed: the system allocator won a measured A/B
against both mimalloc and jemalloc, and the dependency was removed.
