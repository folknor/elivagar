# Way Budget Calibration for Locations-on-Ways

Date: 2026-03-03  
Repo: `elivagar`  
Code under test: `f70cd1a` (includes deadlock fix `90ad2ef`)

## Scope

Investigate whether way in-flight throttling is too conservative for `--locations-on-ways`, and determine whether fixed or dynamic policy is preferable.

Relevant code:

- `src/pipeline.rs`:
  - `MAX_INFLIGHT = 8`
  - `WAY_OUTPUT_MULTIPLIER = 10`
  - `block_cost = estimate_raw_ways_bytes(raw_ways) * WAY_OUTPUT_MULTIPLIER`
  - gate: `count >= MAX_INFLIGHT || (count > 0 && bytes + block_cost > way_budget)`

## Method

Used `brokkr run` with `--locations-on-ways --no-ocean` and varied `--way-budget`.

Primary signal:

- `phase12_ms` (PBF read/process phase)
- `max_way_inflight_bytes`
- `peak_rss_kb`

## Results

### Denmark locations PBF (`data/denmark-20260220-seq4704-locations.osm.pbf`)

| way_budget | phase12_ms | max_way_inflight_bytes | peak_rss_kb |
|---|---:|---:|---:|
| 32M | 6021 | 6165753 | 1793172 |
| 64M | 5560 | 6165753 | 1830484 |
| 128M | 5511 | 11738489 | 1841952 |
| 256M | 5548 | 15586065 | 1769912 |
| 512M | 5541 | 15586065 | 2055576 |

Finding: only very low budget (32M) hurts. 64M+ is mostly flat on this dataset.

### Germany locations PBF (`data/germany-20260224-seq4704-locations.osm.pbf`)

| way_budget | phase12_ms | max_way_inflight_bytes | peak_rss_kb |
|---|---:|---:|---:|
| 64M | 61122 | 6684734 | 8816772 |
| 128M | 63908 | 13338495 | 7513248 |
| 256M | 60546 | 26784643 | 8047096 |
| 512M | 60796 | 30726427 | 8630696 |

Finding: 128M is not best. 256M is fastest in this sample.

### North America locations PBF (`data/north-america-seq4710-locations.osm.pbf`)

| way_budget | phase12_ms | max_way_inflight_bytes | peak_rss_kb |
|---|---:|---:|---:|
| 128M | 277864 | 14851487 | 19834756 |
| 256M | 268473 | 26822279 | 20859112 |
| 512M | 271190 | 52864214 | 21228992 |

Finding: 256M improves `phase12_ms` by ~3.4% vs 128M. 512M does not improve further.

## Interpretation

1. `128M` is throughput-limiting at North America scale in locations-on-ways mode.
2. Best observed point is in the 256M range for tested large datasets.
3. `WAY_OUTPUT_MULTIPLIER = 10` appears conservative for locations-on-ways.
4. The deadlock fix remains correct and required regardless of calibration.

## Recommendation

Short-term (low risk):

1. Keep deadlock predicate unchanged.
2. Use a mode-aware default for locations-on-ways:
   - either `way_budget = 256M` (keeping multiplier 10),
   - or keep `way_budget = 128M` and lower locations-on-ways multiplier to ~5.
3. Keep node-store path defaults unchanged until separately measured.

Long-term (if needed):

1. Add telemetry for dynamic tuning:
   - byte-gate wait time,
   - number of oversized blocks,
   - block size distribution.
2. Only then consider adaptive runtime policy.

## Conclusion

Current fixed defaults are safe but not optimal for locations-on-ways at North America scale.  
A mode-aware fixed default is justified now; dynamic control can wait for better instrumentation.
