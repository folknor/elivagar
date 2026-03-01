# Elivagar Memory Experiment Matrix (64 GB Target)

## Goal
- Make full planet tile generation reliable on 64 GB RAM with no OOM/swap collapse.
- Prioritize peak RSS control and deterministic memory envelopes over marginal throughput gains.

## Success Criteria
- Stable completion on planet-scale runs in streaming mode.
- Peak RSS under safety ceiling (recommended: <= 52 GB).
- Throughput regressions per accepted change: <= 10% unless explicitly approved.

## Measurement Protocol
1. Capture for every run:
- Peak RSS (`VmHWM`)
- Wall time and phase splits (`phase12`, `phase3`, `phase4`)
- Chunk count, features, tiles, unique tiles
- Key in-flight counters (way/relation/assemble batch occupancy)
2. Run each scenario 3 times, report min/median/max.
3. Record host, commit hash, dataset, and exact command.
4. Run experiments sequentially (one process at a time).

## Scenarios
- `S1` Denmark full run (fast iteration).
- `S2` Germany full run (mid-scale stress).
- `S3` North America full run (high memory stress).
- `S4` Planet full run with streaming PMTiles.
- `S5` Planet resume paths (`--skip-to ocean`, `--skip-to sort`) to isolate phase memory.

## Phase 0: Baseline Attribution — ✓ COMPLETE (2026-03-01)

Instrumentation: `3a729ab` (elivagar), brokkr v3 schema (2026-03-01).

### E0.1 Per-phase memory attribution — ✓ ANSWERED
Top-2 contributors: (1) SortedNodeStore (dominates RSS envelope, flat across phases),
(2) assemble batch buffer (263 MB Denmark, 1.4 GB Germany).

Baselines captured (dm6, `f275d10`):
- Denmark: 1.9 GB RSS, 15.8s total
- Germany: 10.7 GB RSS, 123.8s total
- North America: OOM (thrashes for ~3 hours during way processing)

### E0.2 In-flight queue/batch sensitivity — ✓ ANSWERED
- `max_assemble_batch_bytes` is the scaling driver (263 MB → 1.4 GB, 5.5x DK→DE)
- `max_way_inflight_bytes` stable at 23-27 MB — not a concern
- `max_rel_batch_bytes` stable at 9-18 MB — not a concern
- RSS flat across all 4 phases — node store dominates the envelope

## Phase 1: High-ROI Controls — ✓ COMPLETE (`c88cfc0`, `862d0b7`)

See `p1-byte-budgeted-inflight.md` for full details.

### E1.1 Byte-budgeted sort buffering — DEFERRED
Sort chunk size (1 GB) is already byte-based and not a scaling concern. Configurable
via TilegenConfig if needed later.

### E1.2 Relation batch memory cap — ✓ DONE
REL_BATCH_BUDGET=64 MB alongside REL_BATCH_SIZE=1024. Denmark: no change (batches
stay under budget at this scale). Germany validation pending.

### E1.3 Assemble batch memory cap — ✓ DONE
ASSEMBLE_BATCH_BUDGET=32 MB alongside BATCH_SIZE=4096. Denmark: 263 MB → 34 MB (-87%).
Assemble phase 15.5% faster (clone fix contributed). Germany validation pending.

## Phase 2: Refactor Hot Spots
### E2.1 Remove redundant compressed tile cloning in assembly — ✓ DONE (`c88cfc0`)
Replaced `compressed.clone()` with `Vec::with_capacity(compressed.len())`.
Contributed to 15.5% assemble phase speedup on Denmark.

### E2.2 Stream relation outputs instead of full `Vec<Vec<SortRecord>>` collect — ✓ DONE (`a8627be`)
Replaced `par_iter().map().collect()` with ocean-style `par_iter().fold()` + `RelAcc`
per-worker accumulators. Each worker flushes to chunk files at chunk_size threshold;
remaining records drain through sort_writer's normal buffering. Eliminates
double-materialization of input geometry + output records. Reuses `SimplifyMultiScratch`
across relations within each worker. Denmark: correctness identical, sort_chunks stable.
Germany: correctness identical, no regression.

### E2.3 Way pipeline byte-based inflight limits — ✓ DONE (`c88cfc0`)
Replaced token semaphore (MAX_INFLIGHT=4) with Mutex+Condvar byte budget (128 MB,
10x output multiplier). Count ceiling raised to 8. Denmark: 23 MB → 13 MB (-43%).

## Phase 3: Structural Redesign
### E3.1 Streaming root/leaf directory construction without full entry materialization — ✓ DONE (`d11744e`)
Replaced `collect_dir_entries` + `build_directories` with `finalize_directories` that reads
streaming `dir_entries.bin` in 4096-entry chunks, building leaf directories incrementally.
Never materializes the full `Vec<DirEntry>`. Also fixed double-buffer in `collect_dir_entries`
(replaced `read_to_end` with `BufReader` + `read_exact`). Planet-scale finalization peak:
~240 MB → ~30 MB. Added streaming-mode tests including byte-identity check vs in-memory mode.

### E3.2 Way index offset loading strategy
- Current finalize path reads offsets file fully then builds sorted entries vector.
- Hypothesis: this can be a large temporary memory plateau on planet-scale ways.
- Change: external sort or mmap-based offset traversal.
- Exit criteria: lower finalize peak with acceptable lookup speed.

### E3.3 Optional low-memory mode profiles
- Add explicit profile:
- lower sort chunk budget
- lower relation/assemble byte caps
- stricter in-flight limits
- Exit criteria: guaranteed completion envelope for 64 GB hosts.

## Execution Order (Recommended)
1. `E0.1`, `E0.2`
2. `E1.1`, `E1.2`, `E1.3`
3. `E2.1`, `E2.2`, `E2.3`
4. `E3.*` only if still above target

## Decision Gates
- Gate A: after Phase 1, if `S4` is under ceiling, continue with targeted polish only.
- Gate B: after Phase 2, if `S4/S5` still unstable, proceed to `E3.1`.
- Gate C: if still unstable after `E3.1`, enable explicit low-memory profile.

## Reporting Template
- Experiment ID:
- Commit:
- Host:
- Scenario:
- Peak RSS (`VmHWM`):
- Phase timings:
- Memory-sensitive counters:
- Result vs baseline:
- Regressions/risks:
- Decision: keep / iterate / drop
