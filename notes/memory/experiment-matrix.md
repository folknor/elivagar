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

## Phase 0: Baseline Attribution
### E0.1 Per-phase memory attribution
- Hypothesis: largest peaks occur in relation batching, sort buffering, and assemble batches.
- Method: instrument high-water marks at phase and subphase boundaries.
- Exit criteria: top-2 memory contributors quantified.

### E0.2 In-flight queue/batch sensitivity
- Hypothesis: fixed queue sizes and batch sizes dominate peak variability.
- Method: log live sizes of:
- way pipeline queues (`PrimitiveBlock`, `Vec<ProcessedWay>`)
- relation batch occupancy
- assemble batches (`PendingTile`, `EncodedTile`)
- Exit criteria: identify which in-flight structures correlate with RSS spikes.

## Phase 1: High-ROI Controls
### E1.1 Byte-budgeted sort buffering
- Current: fixed 1 GB chunk target in sort writer.
- Hypothesis: adaptive budget tied to host/phase pressure reduces global RSS peaks.
- Change: introduce configurable/auto chunk budget (not hardcoded only).
- Exit criteria: lower peak RSS with acceptable sort overhead.

### E1.2 Relation batch memory cap
- Current: `REL_BATCH_SIZE=1024` with `Vec<Vec<SortRecord>>` materialization.
- Hypothesis: relation batches can burst due to multipolygon complexity.
- Change: flush by byte budget (estimated geometry+record bytes), not count only.
- Exit criteria: reduced relation-phase spikes without major throughput loss.

### E1.3 Assemble batch memory cap
- Current: `BATCH_SIZE=4096` in assemble with one batch ahead.
- Hypothesis: feature-heavy tiles make fixed count unstable.
- Change: replace count cap with byte-aware cap for `PendingTile` and encoded output.
- Exit criteria: phase4 RSS flattening across dense urban windows.

## Phase 2: Refactor Hot Spots
### E2.1 Remove redundant compressed tile cloning in assembly
- Current behavior suggests compressed tile bytes are cloned into thread-local scratch before return.
- Hypothesis: this duplicates large buffers per tile and inflates transient memory.
- Change: reuse/move strategy that avoids full clone per tile.
- Exit criteria: measurable allocator and RSS drop in phase4.

### E2.2 Stream relation outputs instead of full `Vec<Vec<SortRecord>>` collect
- Hypothesis: collecting all parallel relation outputs before push creates avoidable peak.
- Change: incremental drain from workers into sort writer with ordering-neutral merge.
- Exit criteria: lower peak in relation-heavy windows.

### E2.3 Way pipeline byte-based inflight limits
- Current: token count (`MAX_INFLIGHT`) approximates memory.
- Hypothesis: way complexity variance breaks count-based assumptions.
- Change: bound in-flight way work by estimated bytes (refs, tags, coords, records).
- Exit criteria: tighter way-phase RSS envelope.

## Phase 3: Structural Redesign (if needed)
### E3.1 Streaming root/leaf directory construction without full entry materialization
- Current streaming mode still materializes all dir entries during `write_to`.
- Hypothesis: large directory vectors create late-phase memory spikes.
- Change: externalized multi-pass or on-disk merge strategy for PMTiles directories.
- Exit criteria: remove `write_to` memory spike while preserving output correctness.

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
