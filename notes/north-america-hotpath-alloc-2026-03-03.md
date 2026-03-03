# North America Hotpath Alloc Profile (2026-03-03)

Repo: `elivagar`  
Commit: `fb0c5e3`  
Run UUID: `2acb944c`  
Command: `brokkr hotpath --dataset north-america --variant locations --alloc --no-mem-check`

## Summary

- Wall: `900190 ms`
- `phase12_ms`: `691713`
- `phase4_ms`: `189710`
- Peak RSS: `23346.2 MB`
- `max_way_inflight_bytes`: `26756934`

Dataset:

- `north-america-seq4710-locations.osm.pbf` (`19060 MB`)
- Full run with ocean enabled

## Top Allocators (cumulative)

1. `elivagar::pipeline::process_raw_way` — `224.9 GB`
2. `elivagar::geometry::for_each_zoom_simplified` — `148.4 GB`
3. `elivagar::pipeline::emit_polygon_feature` — `85.2 GB`
4. `elivagar::pipeline::process_prepared_relation_into` — `69.9 GB`
5. `elivagar::pipeline::emit_line_feature` — `63.2 GB`
6. `elivagar::geometry::for_each_zoom_simplified_multi` — `60.4 GB`
7. `elivagar::pipeline::emit_multipolygon_feature` — `48.9 GB`
8. `elivagar::geometry::clip_polygon_into` — `44.8 GB`

## Interpretation

- Allocation pressure is dominated by way/relation geometry processing, not PMTiles finalization.
- The largest buckets align with per-feature temporary vectors in line/polygon/multipolygon emission and simplification/clip paths.
- `process_raw_way` cumulative allocation reflects both direct work and nested geometry/simplification calls; reducing transient Vec churn inside emit/simplify paths is the main lever.

## Candidate Next Optimizations

1. Reuse scratch buffers more aggressively in way geometry emission:
   - line/polygon/multipolygon temporary vectors (`geom_buf`, clip buffers, ring vectors)
2. Add/extend `_into` APIs to avoid temporary allocations in clipping/simplification paths where missing.
3. Relation path pass:
   - reduce per-relation transient allocations in multipolygon assembly and clipping.
4. Keep `--way-budget` tuning fixed for now; this profile suggests compute+allocation churn is now a larger limiter than queue throttling.

## Outcome for TODO Step 4b

Step 4b objective ("identify top allocators at scale") is completed by this run.
