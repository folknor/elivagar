# Elivagar Memory Research Conclusions

## Scope
- Theoretical memory-focused analysis of current `elivagar` pipeline design.
- Objective: identify highest-leverage paths to reliable planet runs on 64 GB RAM.

## Executive Conclusion
- The main memory risk is **in-flight feature/record buffering across phases**, not PBF decode itself.
- Top pressure points are:
- sort writer chunk buffering
- relation batch materialization
- assemble batch materialization
- PMTiles directory handling during finalization
- The current architecture is strong and already uses streaming in key places, but fixed count-based batching leaves memory spikes dependent on feature complexity.

## Key Findings

### 1) Sort buffer is a deliberate 1 GB memory anchor
- Sort chunk budget is fixed to 1 GB in pipeline config/constants ([src/pipeline.rs](/home/folk/Programs/elivagar/src/pipeline.rs:117)).
- `SortWriter` accumulates `Vec<SortRecord>` plus payload bytes until flush ([src/sort.rs](/home/folk/Programs/elivagar/src/sort.rs:67)).
- Conclusion: this is expected memory, but it must be budgeted against all other in-flight structures.

### 2) Way phase duplicates multiple owned payload forms
- Way blocks are converted into owned `RawWay` (`Vec<i64>` refs + `Vec<(String,String)>` tags), then into `ProcessedWay` (`coords_e7` + `records`) before drain ([src/pipeline.rs](/home/folk/Programs/elivagar/src/pipeline.rs:724), [src/pipeline.rs](/home/folk/Programs/elivagar/src/pipeline.rs:733)).
- In-flight control is count-based (`MAX_INFLIGHT`), not byte-based ([src/pipeline.rs](/home/folk/Programs/elivagar/src/pipeline.rs:489)).
- Conclusion: memory footprint varies strongly with way/tag complexity.

### 3) Relation phase can burst memory on complex multipolygons
- `REL_BATCH_SIZE=1024` with batch parallelization collecting `Vec<Vec<SortRecord>>` before serial push ([src/pipeline.rs](/home/folk/Programs/elivagar/src/pipeline.rs:854), [src/pipeline.rs](/home/folk/Programs/elivagar/src/pipeline.rs:923)).
- `PreparedRelation` holds `member_ways: Vec<MemberWay>`, each with owned coordinate vectors ([src/pipeline.rs](/home/folk/Programs/elivagar/src/pipeline.rs:847)).
- Conclusion: count-based batching can be unsafe for geometry-heavy segments.

### 4) Assemble phase uses high fixed batch count
- `BATCH_SIZE=4096` for tile assembly pipeline ([src/pipeline.rs](/home/folk/Programs/elivagar/src/pipeline.rs:1496)).
- `PendingTile` holds variable-size feature payload vectors; `EncodedTile` holds compressed tile bytes.
- Conclusion: fixed tile count does not control memory when tile complexity varies.

### 5) PMTiles streaming mode still has a final directory memory step
- Even in streaming mode, directory entries are collected into `Vec<DirEntry>` for directory building ([src/pmtiles_writer.rs](/home/folk/Programs/elivagar/src/pmtiles_writer.rs:349)).
- Dedup map is capped (`MAX_DEDUP_ENTRIES=1_000_000`), which is good and likely not the largest risk ([src/pmtiles_writer.rs](/home/folk/Programs/elivagar/src/pmtiles_writer.rs:57)).
- Conclusion: late-phase memory spike risk remains in directory assembly.

### 6) Node store approach is mostly correct for memory at scale
- Sorted node store is compact and designed for this workload; flat mmap fallback is guarded for large unsorted inputs ([src/pipeline.rs](/home/folk/Programs/elivagar/src/pipeline.rs:347)).
- Conclusion: node store is not the first target unless attribution shows otherwise.

## Priority Recommendations

### P1. Replace count-based limits with byte-budgeted in-flight controls
- Apply to:
- way in-flight work
- relation batches
- assemble batches
- Expected impact: biggest immediate RSS stability improvement.

### P2. Stream relation outputs incrementally
- Avoid full `Vec<Vec<SortRecord>>` collection before pushing to sort writer.
- Expected impact: lower relation-phase peaks.

### P3. Tighten assemble memory behavior
- Reduce or adapt `BATCH_SIZE` by byte budget.
- Remove redundant compressed buffer duplication patterns.
- Expected impact: lower phase4 spikes and allocator pressure.

### P4. Introduce configurable sort chunk memory profile
- Keep high-throughput default, add low-memory profile for 64 GB hosts.
- Expected impact: safer operation under constrained RAM at modest time cost.

### P5. Redesign PMTiles directory finalization for strict streaming
- Avoid full directory entry materialization at `write_to` for very large runs.
- Expected impact: eliminate end-of-run memory cliff.

## What Is Most Likely to Unlock 64 GB Reliability
1. Byte-budgeted in-flight controls across way/relation/assemble.
2. Relation output streaming refactor.
3. PMTiles directory finalization memory redesign.

## Risk / Complexity
- Low risk: parameterized memory budgets and batch cap tuning.
- Medium risk: streaming relation output path.
- High risk: PMTiles directory redesign while preserving PMTiles layout guarantees.

## Decision Statement
- First investment should be **in-flight byte budgeting** (broadest protection, lowest complexity).
- Second should be **relation materialization reduction**.
- If late-phase spikes remain, prioritize **PMTiles directory streaming redesign**.

## Related Plan
- Execution matrix and experiment order: [ELIVAGAR_MEMORY_EXPERIMENT_MATRIX.md](/home/folk/Programs/elivagar/ELIVAGAR_MEMORY_EXPERIMENT_MATRIX.md).
