# Linux I/O Profile — Planet-Scale Analysis

Research notes on mmap tuning, page cache hygiene, and kernel features for
planet-scale tile generation. Target: full planet PBF (~73 GB) on NVMe, Linux 6.x.

## I/O Inventory

elivagar's pipeline generates 300-500 GB of file I/O at planet scale, split across
five distinct access patterns:

| Component | Mechanism | Access Pattern | Planet Size | Current Hints |
|-----------|-----------|----------------|-------------|---------------|
| Node index | mmap (grow 1 GB) | Sequential write → random read | ~96 GB | MADV_RANDOM (exists, not wired up) |
| Way offsets | mmap (grow 1 GB) | Random write → random read | ~12 GB | MADV_RANDOM (unconditional) |
| Way data | BufWriter → mmap | Sequential append → random read | ~50 GB | MADV_RANDOM (unconditional) |
| Ocean shapefile | mmap | Sequential bbox filter → sparse detail | ~500 MB | None |
| Sort chunks | BufWriter / BufReader | Sequential write, sequential k-way read | 100+ GB | None |
| PMTiles temp blob | BufWriter → BufReader | Sequential write → sequential read-back | 100-200 GB | None |
| PMTiles output | BufWriter | Sequential write | 100-200 GB | None |

## The #1 Bottleneck: Node Index Random Reads

Hotpath profiling on Denmark (483 MB PBF) shows the main thread at **56% kernel time**
(10.9s sys / 19.6s total), dominated by mmap page faults on the node index during way
processing. Each way references 2-20+ node IDs; rayon workers resolve coordinates via
random lookups into the mmap'd index.

At planet scale: 8.5 billion nodes, max ID ~12 billion. The index is ~96 GB — exceeding
typical 64 GB hosts. Every cache miss is a random 4 KB read: ~10µs on NVMe, ~5ms on
HDD. With billions of lookups, **NVMe is not optional at planet scale.**

The index has geographical locality (ways in a PBF blob tend to reference nearby nodes),
so the page cache hit rate is decent even when the index exceeds RAM. But readahead
*hurts* at this scale — each page fault triggers readahead of 16+ pages that get evicted
before use. `MADV_RANDOM` disables this waste.

## Page Cache Contention

During the PBF phase, the node index needs maximum page cache residency for random read
hits. But sort chunk writes (100+ GB) pass through the page cache, evicting hot node
index pages. This is the fundamental contention: random-read working set vs
sequential-write pollution.

Solutions ranked by complexity:
1. `fadvise(FADV_DONTNEED)` on sort chunks after write — evicts on demand
2. `O_DIRECT` on sort chunk writes — bypasses cache entirely
3. `O_DIRECT` on PMTiles temp blob — same pattern, same benefit

## io_uring Relevance

**Low.** Unlike pbfhogg where the writer thread can become the bottleneck (erofs +
Compression::None case), elivagar's bottleneck is CPU (DP simplification, S-H clipping,
MVT encoding, gzip) and node index random I/O. The write paths are sequential BufWriter
appends that barely register in profiling. io_uring's batched async writes would save
microseconds on a pipeline that takes minutes. Not worth the complexity.
