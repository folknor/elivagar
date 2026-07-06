# Sort Fanout Analysis - North America vs Germany

Commit `81c4d6b` (plantasjen), locations-on-ways, defaults.

## Aggregate comparison

| Metric | Germany (5.5 GB) | NA (19 GB) | Ratio |
|--------|----------------:|----------:|------:|
| sort_records | 146M | 486M | 3.3x |
| sort_record_bytes | 11.5 GB | 51.2 GB | 4.5x |
| sort_bytes/input_MB | 2.07 | 2.68 | 1.30x |
| records_per_way | 2.1 | 2.3 | 1.10x |
| sort_chunks | 547 | 5,687 | 10.4x |
| features | 146M | 511M | 3.5x |
| ways | 69.6M | 208.9M | 3.0x |

Input grows 3.4x but sort bytes grow 4.5x - 30% superlinear amplification.

## NA per-layer breakdown (top 8 by sort bytes)

| Layer | Records | Bytes | B/rec | Zoom span | % total |
|-------|--------:|------:|------:|-----------|--------:|
| streets | 158.8M | 12.1 GB | 76 | z5-z14 (10) | 23.6% |
| water_polygons | 70.1M | 10.9 GB | 156 | z4-z14 (11) | 21.3% |
| land | 44.5M | 9.3 GB | 209 | z7-z14 (8) | 18.2% |
| buildings | 89.8M | 7.3 GB | 82 | z14 only | 14.3% |
| water_lines | 31.4M | 4.4 GB | 139 | z9-z14 (6) | 8.5% |
| street_labels | 31.5M | 3.9 GB | 123 | z10-z14 (5) | 7.6% |
| addresses | 40.6M | 1.3 GB | 31 | z14 only | 2.5% |
| water_lines_labels | 4.6M | 968 MB | 210 | z12-z14 (3) | 1.9% |

Top 3 layers = 32.3 GB = 63% of all sort data.

## Zoom distribution - polygon layers vs non-polygon

### water_polygons (10.9 GB, 70M records, 11 zoom levels)

| Zoom | Records | Cumul % | Growth from prev |
|------|--------:|--------:|-----------------:|
| z4 | 1,681 | 0.0% | - |
| z5 | 4,765 | 0.0% | 2.8x |
| z6 | 12,310 | 0.0% | 2.6x |
| z7 | 83,909 | 0.1% | 6.8x |
| z8 | 756,531 | 1.2% | 9.0x |
| z9 | 2,371,168 | 4.6% | 3.1x |
| z10 | 5,124,402 | 11.9% | 2.2x |
| z11 | 8,662,477 | 24.3% | 1.7x |
| z12 | 12,511,573 | 42.1% | 1.4x |
| z13 | 16,122,426 | 65.1% | 1.3x |
| z14 | 24,405,136 | 100% | 1.5x |

Pattern: geometric growth z7-z10 (large polygons subdividing across tiles),
tapering z11+ (subpixel rejection kicking in for small features).

### land (9.3 GB, 44.5M records, 8 zoom levels)

| Zoom | Records | Cumul % | Growth from prev |
|------|--------:|--------:|-----------------:|
| z7 | 42,297 | 0.1% | - |
| z8 | 202,184 | 0.5% | 4.8x |
| z9 | 646,054 | 2.0% | 3.2x |
| z10 | 3,101,322 | 9.0% | 4.8x |
| z11 | 6,065,384 | 22.6% | 2.0x |
| z12 | 8,080,333 | 40.7% | 1.3x |
| z13 | 10,219,620 | 63.7% | 1.3x |
| z14 | 16,141,210 | 100% | 1.6x |

Same pattern. z9→z10 is 4.8x - large land polygons exploding across tiles.

### streets (12.1 GB, 158.8M records) - for contrast

| Zoom | Records | % |
|------|--------:|--:|
| z5-z9 | 1.8M | 1.1% |
| z10-z11 | 6.8M | 4.3% |
| z12 | 19.6M | 12.3% |
| z13 | 58.8M | 37.0% |
| z14 | 71.8M | 45.2% |

Streets are z13+z14 dominated (82%). Linear growth, not the superlinear driver.

### buildings (7.3 GB) - 100% z14. Pure linear scaling.

## Root cause

The superlinear amplification comes from **large polygon features × exponential tile
subdivision**. A single water polygon covering one z8 tile generates ~64 records at z11
(it touches 4^3 = 64 tiles three zoom levels later). Each record carries full clipped
geometry at 156 B/rec (water_polygons) or 209 B/rec (land).

NA has more large water bodies and larger land polygons than Germany, so the effect is
amplified. At planet scale, the Great Lakes, Amazon basin, Siberian coastline, and
Pacific islands will push this further.

## Intervention analysis

### Priority 1: Layer/zoom fanout controls

**Target**: Reduce the number of sort records generated, especially for polygon layers
at mid-zoom (z8-z12) where geometric growth is steepest.

**Concrete options** (quality/coverage tradeoffs - not free wins, need visual validation):

1. **Zoom-dependent subpixel area threshold for polygons**
   - Current: uniform 1px threshold across all zooms
   - Proposed: 4px² at z<=10, 2px² at z11-z12, 1px at z13+ for polygon layers
   - Impact: eliminates small-but-technically-visible features at mid-zoom
   - Estimated savings: 2-4 GB (conservative - depends on feature size distribution)

2. **Tile-touch cap per feature per zoom**
   - If a single feature would touch >N tiles at zoom z, skip it at that zoom
   - The feature is already present at z-1 (where it touches N/4 tiles), so overzooming
     provides continuity
   - N=2048 is generous; most features touch <100 tiles even at z14
   - Impact: prevents worst-case blowup from continent-spanning polygons
   - Estimated savings: small for NA, but critical safety valve for planet

3. **Per-layer min_zoom tightening**
   - water_polygons currently starts at z4. Few users zoom to z4-z6 with water detail.
   - Raising water_polygons min_zoom to z7 eliminates 103K records (trivial savings)
   - More impactful: raise land min_zoom from z7 to z8 (saves 42K records, also trivial)
   - Verdict: not the lever - the volume is at z10+

**Best first target**: Option 2 (tile-touch cap) as a guardrail behind a flag
(default off). Dropping a feature at zoom z can create visible pop/flicker if
style differs by zoom - overzoom continuity is not guaranteed. Must validate
with visual/parity samples before any default change.

### Priority 2: Polygon record weight reduction

**Target**: Reduce B/rec for polygon layers, especially at mid-zoom where vertices are
over-resolved relative to pixel density.

**Concrete options**:

1. **Zoom-appropriate pre-simplification**
   - Current: geometry is simplified per `for_each_zoom_simplified` with zoom-appropriate
     tolerance, but the full simplified geometry is serialized per tile
   - Opportunity: at z10, a coastline polygon clipped to one tile still carries hundreds
     of vertices. DP tolerance at z10 is coarse enough that many could be eliminated
   - Expected B/rec reduction: 30-50% at z8-z12 for polygon layers
   - Estimated savings: 3-5 GB

2. **Compact polygon wire format**
   - Current wire format: varint-encoded coordinate pairs
   - Alternative: delta-encoded coordinates with smaller varint overhead
   - Expected savings: 10-20% of polygon bytes = 2-4 GB
   - More engineering effort, touches wire_format.rs and assemble reader

3. **Deferred geometry materialization**
   - Store compact reference (feature_id + tile_id) in sort record instead of geometry
   - Assemble phase re-reads geometry, clips, and encodes on demand
   - Trades CPU (double clip) for sort volume (dramatic reduction)
   - Highest potential savings but largest implementation effort
   - Risk: assemble phase already bottlenecked (158s for NA)

**Best second target**: More aggressive DP tolerance policy for polygon layers at z8-z12.
This is not "zoom-appropriate pre-simplification" (that already exists via
`for_each_zoom_simplified`). The proposal is a tighter tolerance specifically for
polygon layers at mid-zoom, tuned to reduce B/rec for the heaviest geometry.

## Decision framework for next steps

1. Add tiles_touched tail stats and per-layer-per-zoom bytes instrumentation (done)
2. Implement tile-touch cap behind a flag (default off) as a guardrail
3. Run NA and compare visual/parity samples before any default change
4. If still superlinear, tune polygon tolerance policy at z8-z12 (also behind flag)

## Fanout tail stats (Denmark, dirty-tree run)

Key findings from Denmark p50/p95/p99/max tiles_touched:

- **boundaries z14**: p50=16, p95=2864, max=2864 - extreme tail from coastline
- **ferries z14**: p50=8, p95=512, max=1258 - long ferry routes
- **water_polygons z14**: p50=1, p95=2, max=205 - most small, tail moderate
- **land z14**: p50=1, p95=4, max=207 - similar to water_polygons
- **streets z14**: p50=1, p95=2, max=32 - well-behaved
- **buildings z14**: p50=1, p95=1, max=14 - tight distribution

The tail is concentrated in a few layers (boundaries, ferries) and in polygon
layers (water_polygons, land) at the max. The p95 for polygon layers is very
low (1-4) - the amplification comes from the tail, not the median feature.

This means tile-touch cap would primarily affect the tail features. For NA,
the tail will be much larger (Great Lakes, coastlines, major boundaries).
