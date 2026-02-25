# madvise + Tags Binary Search Investigation

Date: 2026-02-24

## Summary

Two changes were introduced in commit ca6f17e and later commits:
1. **Tags binary search** — sort tags by key, use `binary_search_by_key` for lookups
2. **NodeIndex `advise_random()`** — call `madvise(MADV_RANDOM)` at the node→way transition

Both caused PBF phase regressions. This document records all findings.

---

## The Key Discovery

**The Denmark node index is 102 GB** despite Denmark being a 483 MB PBF. This is because
OSM node IDs are globally assigned — Denmark contains nodes with IDs up to ~12 billion.
The flat index is addressed at `node_id * 8`, so max_id × 8 bytes ≈ 96-102 GB regardless
of how many nodes are actually in the extract.

This means **even Denmark triggers the planet-scale madvise behavior**. The file is
larger than physical RAM (~64 GB), so `MADV_RANDOM` should theoretically help. But the
benchmarks show it hurts. This needs further investigation.

---

## Bisect Results (best-of-3, Denmark 483 MB PBF)

All runs done back-to-back in one session to minimize load variance.

| Commit | Description | PBF (ms) | Total (ms) |
|--------|-------------|----------|------------|
| 77c217f | Baseline (benchmarks.tsv reference) | 19,009 | 28,454 |
| b63dbd7 | MVT hash + PMTiles memory + alloc pressure | 20,723 | 28,924 |
| ba2a92b | Deduplicate code | 22,304 | 30,229 |
| 998814f | Misc code quality | 21,069 | 29,330 |
| 1797ee4 | Correctness bugs (XOR sentinel + compare_tiles) | 20,585 | 28,602 |
| ca6f17e | **Tags binary search + advise_random** | **33,759** | **42,316** |
| 068a428 | Hoist node_records buffer | 35,069 | 42,875 |

The regression is entirely in ca6f17e which introduced both changes simultaneously.

---

## Isolation Tests

After the bisect, individual changes were tested:

### Test 1: advise_random disabled, Tags reverted (= 1797ee4 equivalent)
- **PBF: 20,026 ms** ✓ matches baseline

### Test 2: advise_random disabled, Tags binary search active
- **PBF: ~31,317 ms** — Tags binary search alone adds +55%

### Test 3: advise_random active (unconditional), Tags reverted
- **PBF: 33,155 ms** — advise_random alone adds +65%

### Test 4: Both active (= ca6f17e)
- **PBF: 33,759 ms** — combined effect (not additive, they compete for same bottleneck)

### Test 5: advise_random conditional (index > RAM/2), Tags reverted
- **PBF: 28,375 ms** — conditional check fires because Denmark index is 102 GB > 32 GB

---

## What We Know

### Tags binary search is a regression
- `sort_unstable_by_key` on 3-15 tags per element adds measurable overhead
- Even though PBF tags are usually pre-sorted (so sort is ~N comparisons), the function
  call overhead per element × millions of elements adds up
- `binary_search_by_key` per lookup has more overhead than a simple `.any()` iterator
  for tiny slices — branch-heavy binary search vs branchless sequential scan
- Linear scan wins because: (a) slice fits in one cache line, (b) key comparison
  short-circuits on first byte mismatch, (c) zero per-element setup cost
- **Reverted.** Comment added to `shortbread.rs` Tags struct documenting this.

### advise_random is complicated
- The Denmark node index is **102 GB** (not 3-4 GB as we assumed)
- This means our "conditional on index > RAM/2" threshold fires even on Denmark
- Unconditional MADV_RANDOM: +65% regression (20s → 33s)
- Conditional MADV_RANDOM (fires because 102 GB > 32 GB): +40% regression (20s → 28s)
- The conditional version is slightly better, possibly because the RAM check introduces
  a delay that changes mmap fault timing, or because the /proc/meminfo read is after
  some pages are already faulted in

### Why MADV_RANDOM hurts even when the index exceeds RAM
- Way references have **locality** — PBF is sorted by ID, and ways reference nearby
  nodes. Even though the index can't fit in page cache entirely, the working set during
  any short time window is much smaller
- Default kernel readahead (128 KB = 32 pages) brings in neighbors that WILL be used
  by the next few ways. MADV_RANDOM kills this, forcing a 4 KB fault per lookup
- This is different from true random access (like relation member lookups via way_index,
  where MADV_RANDOM helps because there's no locality)

### The madvise history repeats itself
- Commit 6724e0a: added MADV_SEQUENTIAL → 2.3× regression, reverted in 4e427b4
- This attempt: added MADV_RANDOM → 1.4-1.65× regression
- Both times, the assumption that "random = bad, sequential = good" was wrong because
  of the nuanced locality in way→node lookups

---

## What We Don't Know

1. **Would MADV_RANDOM help at planet scale where the working set truly exceeds RAM?**
   Denmark's 44M nodes are sparse in a 102 GB index — most pages are zero-filled and
   never written. At planet scale, ~8.5B nodes are spread across ~96 GB with much higher
   density. The access pattern might be truly random enough that readahead wastes I/O.
   **Can only be tested on a planet run.**

2. **What's the right threshold?** The RAM/2 heuristic doesn't work because Denmark's
   index is already >RAM/2. Better heuristics:
   - Track actual node count and compare to index size (density)
   - Track max node ID delta between consecutive way references (locality metric)
   - Use the PBF file size as a proxy (<10 GB = country extract, >50 GB = planet)
   - Simply make it a CLI flag: `--madvise-random`

3. **Is there a middle ground?** `MADV_NORMAL` (the default) with readahead=128 KB
   works well. `MADV_RANDOM` with readahead=0 hurts. Can we set a custom readahead
   size (e.g. 16 KB) via `/proc/self/fd/N` or `posix_fadvise`?

4. **What was the first benchmark inflated by?** The initial 1-run bench showed
   total=55-66s, while 3-run best-of showed 28-42s. The first run is always much
   slower, likely because the PBF file and sort chunks aren't in page cache yet.

---

## Current State of the Code

### Committed (068a428)
- Tags binary search active (ca6f17e)
- advise_random unconditional (ca6f17e)
- node_records hoisted (068a428)

### After cleanup (ready to commit)

- Tags **reverted** to linear scan with documentation comment in `shortbread.rs`
- `shortbread_tests.rs` restored to 1797ee4 (no sort_tags)
- `advise_random()` **not called** — method and `ram_bytes()` kept in node_index.rs for future use
- `node_records` hoist kept (the one improvement that doesn't regress)
- `Cargo.lock` restored to current pbfhogg state
- `TODO.md` updated — Tags and madvise items marked open with investigation notes
- `notes/madvise-investigation.md` — this file (new)

### Remaining
1. **Run a clean 3-run bench** on a quiet system to confirm baseline is restored
2. **Planet-scale testing** to determine if advise_random helps with higher node density

---

## Recommended Next Steps

1. **Disable advise_random entirely for now.** It hurts Denmark, and we can't test
   planet scale yet. Add a `--madvise-random` CLI flag for future planet testing.
   Document the full history in node_index.rs.

2. **Keep Tags as linear scan.** The binary search approach is a dead end for 3-15
   element slices. If tag lookup ever shows up in profiling, the right approach is
   perfect hashing (`phf`) over the ~50 known tag keys, not binary search.

3. **Benchmark the final state** on a quiet system to confirm we match 77c217f baseline.
