# North America Memory Plan (Elivagar)

## Context

Goal: make full North America (`north-america-latest.osm.pbf`) runs reliable on 32 GB hosts.

Observed facts from current code and local runs:

- North America element counts (measured with `pbfhogg bench-read`):
  - nodes: `2,380,320,670`
  - ways: `209,686,327`
  - relations: `2,340,772`
- North America node coordinate compression estimate (measured with `pbfhogg node-stats`):
  - `12.44 GB` compressed, `70.1%` of raw 8-byte/node storage
- Germany (4.7 GB PBF) full run in brokkr DB already reaches ~`10.9 GB` peak RSS on a ~30 GB available-memory host.

Main conclusion: this is primarily an elivagar in-memory working-set problem, not an I/O backend problem (`io_uring`/`O_DIRECT` do not remove in-memory state).

---

## Concrete Steps (High Confidence)

## 1) Expose all memory budgets as CLI flags — DONE

Completed: 2026-03-01. Added `--way-budget`, `--rel-budget`, `--assemble-budget` CLI flags (minimum 1M each). Same `parse_byte_size` parsing as `--sort-budget`. All pass through to `TilegenConfig`; 0 = use existing defaults (128M, 64M, 32M respectively).

---

## 2) Add low-memory presets in brokkr

Action:

- Add preset variants in `brokkr.toml` / brokkr commands for:
  - `lowmem-32g`
  - `balanced-64g`
- Example 32 GB preset:
  - `--sort-budget 256M`
  - low way/rel/assemble budgets
  - lower thread count (e.g. 6-8) to reduce allocator and rayon scratch pressure

Why this is high confidence:

- No core code risk; just operational profile control.
- Gives a repeatable mode for constrained hosts.

Validation:

- Benchmark same dataset with and without preset.
- Confirm no OOM and acceptable wall-time regression.

---

## 3) Make `WayIndex` finalize/load out-of-core (biggest concrete win) — DONE

Completed: commit `2e32449`, 2026-03-01, plantasjen.

Replaced `Vec<WayEntry>` + `Vec<u8>` with external merge sort (256 MB budget, 16M entries/chunk) + read-only mmaps for both sorted offsets and compressed data. Eliminates ~6+ GB anonymous heap memory at NA scale, replacing it with file-backed pages the kernel can evict under pressure.

Denmark validation: 12406 ms wall, 1811 MB RSS (-2.3%), output byte-identical (274.7 MB). All 9 unit tests pass.

---

## 4) Lower default sort chunk budget for safer baseline

Status today:

- default sort chunk size is `1 GiB`.

Action:

- Reduce default (e.g. `512M` or `256M`) or make adaptive by available RAM.

Why this is high confidence:

- Very low implementation risk.
- Directly lowers peak memory used by sort record buffering.

Tradeoff:

- More chunk files, slightly slower sort/merge.

Validation:

- Denmark + Germany wall-time vs RSS comparison.
- Confirm no major regression in `phase3_ms`.

---

## 5) Add memory target guardrails before run starts

Action:

- Estimate minimum working-set from:
  - PBF size
  - expected node-store footprint
  - configured budgets
- If estimated memory > threshold (e.g. >85% of available), fail fast with a suggested low-memory command.

Why this is high confidence:

- Safety improvement only.
- Prevents late OOM and wasted run time.

Validation:

- Synthetic tests for accepted/rejected configs.
- Manual check on known small and large datasets.

---

## 6) Add a dedicated NA-scale memory benchmark workflow

Action:

- Standardize one command path for NA memory validation (same args each run).
- Persist and compare:
  - `peak_rss_kb`
  - per-phase RSS
  - phase timings
  - `sort_chunks`

Why this is high confidence:

- Process/documentation change; removes ambiguity.

Validation:

- Add to `README`/`TODO` operational notes after first successful run.

---

## Speculative / Theoretical Steps

These are plausible but require more design/profiling and may have high implementation cost.

## A) Partitioned/tiled processing architecture

Idea:

- Partition by geography or tile-space, process partitions with smaller local indices, then merge outputs.

Potential benefit:

- Could avoid global node/way state in RAM for the full continent.

Risk:

- Complex correctness around cross-partition relations/ways and deduplication.

---

## B) Disk-backed node store with bounded cache

Idea:

- Keep compressed node groups on disk, cache hot groups in RAM (LRU/clock).

Potential benefit:

- Reduces RAM floor from node store.

Risk:

- Could severely increase random I/O and wall time if cache locality is worse than expected.

---

## C) Two-pass or hybrid relation handling

Idea:

- First pass writes relation-member requirements, second pass resolves in a more cache-friendly order.

Potential benefit:

- Lower peak memory around way/relation interaction.

Risk:

- Additional I/O pass; may violate current single-pass performance goals.

---

## D) Reuse pbfhogg-style richer blob metadata in tilegen input path

Idea:

- Use blob-level metadata to cheaply skip/route work earlier in tilegen pipeline.

Potential benefit:

- Lower decode and transient in-flight costs in specific phases.

Risk:

- Metadata model mismatch: tilegen still needs broad feature materialization, unlike merge passthrough-heavy workloads.

---

## E) Adaptive runtime controller

Idea:

- Continuously adjust budgets/threading from live RSS and backpressure signals.

Potential benefit:

- Better resilience across different hosts/datasets without hand tuning.

Risk:

- Control-loop complexity, oscillation risk, harder reproducibility.

---

## Recommended Execution Order

1. Expose CLI memory flags.
2. Add low-memory brokkr presets.
3. Reduce default sort budget (or make adaptive).
4. Implement `WayIndex` out-of-core finalize/read path.
5. Add preflight memory guardrails.
6. Re-run North America and then revisit speculative options only if still memory-bound.

This order gets immediate operability gains first, then addresses the largest structural memory sink.
