# North America Hotpath Alloc Profile (2026-03-03)

Status: active reference, updated after follow-up passes.

## Baseline Capture (Step 4b)

Repo: `elivagar`  
Commit: `fb0c5e3`  
Run UUID: `2acb944c`  
Command: `brokkr hotpath --dataset north-america --variant locations --alloc --no-mem-check`

Metrics:

- Wall: `900190 ms`
- `phase12_ms`: `691713`
- `phase4_ms`: `189710`
- Peak RSS: `23346.2 MB`
- `max_way_inflight_bytes`: `26756934`

Dataset:

- `north-america-seq4710-locations.osm.pbf` (`19060 MB`)
- Full run with ocean enabled

Top allocators (cumulative):

1. `elivagar::pipeline::process_raw_way` - `224.9 GB`
2. `elivagar::geometry::for_each_zoom_simplified` - `148.4 GB`
3. `elivagar::pipeline::emit_polygon_feature` - `85.2 GB`
4. `elivagar::pipeline::process_prepared_relation_into` - `69.9 GB`
5. `elivagar::pipeline::emit_line_feature` - `63.2 GB`
6. `elivagar::geometry::for_each_zoom_simplified_multi` - `60.4 GB`
7. `elivagar::pipeline::emit_multipolygon_feature` - `48.9 GB`
8. `elivagar::geometry::clip_polygon_into` - `44.8 GB`

Interpretation from baseline:

- Pressure is dominated by way/relation geometry scratch allocation churn, not PMTiles finalization.
- The main lever was reducing transient Vec churn in simplify/clip/emit paths.

## Follow-up Outcomes (Implemented)

### Way-path alloc reduction pass (`ff87135`)

- North America hotpath alloc showed large cumulative allocation drops in top way functions.
- `bench self` on North America locations showed wall essentially flat (`+0.38%`) with slight RSS improvement (`-1.9%`).
- Decision: keep; hotpath alloc mode is intrusive, so throughput decisions use `bench self`.

### Relation-path pass 1 (`86b9fe8`)

- Germany hotpath alloc:
  - wall `-4.0%`
  - peak RSS `-32.5%`
  - `phase12_ms -3.3%`
- Decision: keep.

### Relation-path pass 2 (`1d270e5`)

- Germany hotpath alloc:
  - wall `-0.5%`
  - peak RSS approximately flat
  - `phase12_ms -1.4%`
- Decision: keep (small but positive throughput improvement).

### Relation-path pass 3 (`9b7b046`) - rejected

- Single-outer `pair_rings` fast path regressed Germany wall by `+3.9%` with no RSS benefit.
- Reverted in `4382006`.
- Rejection rationale documented in `d3be1a5`.

## Current Direction

- Biggest practical wins from this profile are already landed.
- Remaining opportunities are likely small and noisy at current scale.
- Prefer:
  - Germany reproducible runs for micro-optimization work.
  - North America reruns only for milestone validation.
  - Focus on peak RSS risk items (relation buffering measurement, large-scale validation) before further hotpath micro-tuning.

## Cross References

- `TODO.md` "Next Steps (Current)" for current priority and benchmark table.
- `notes/north-america-memory-plan.md` for memory strategy/status context.
