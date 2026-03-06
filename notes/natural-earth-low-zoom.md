# Natural Earth Low-Zoom Layers

Investigated 2026-03-06.

## Problem

Elivagar simplifies full-resolution OSM geometry for z0-5 features (boundaries, lakes,
etc.) via Douglas-Peucker. This is expensive and produces worse results than using
pre-generalized cartographic data. Natural Earth provides public-domain vector data
at three scales specifically designed for low-zoom mapping.

## Natural Earth data

### Scales and zoom mapping

| NE Scale | Resolution | Zoom Levels | Use Case |
|----------|-----------|-------------|----------|
| 1:110m | ~100 km | z0-1 | Global overview |
| 1:50m | ~50 km | z2-3 | Continental |
| 1:10m | ~10 km | z4-5 | Regional, highest NE detail |

OSM takes over at z6 (4,096 tiles). This aligns with Planetiler's approach.

### Relevant layers

**Physical:**
- `ne_{scale}_ocean` -- ocean polygons
- `ne_{scale}_lakes` -- major lakes and reservoirs
- `ne_{scale}_glaciated_areas` -- glaciers and ice sheets
- `ne_{scale}_rivers_lake_centerlines` -- major rivers (line features)

**Cultural:**
- `ne_{scale}_admin_0_boundary_lines_land` -- country boundaries
- `ne_{scale}_admin_1_states_provinces_lines` -- state/province boundaries (10m only)
- `ne_{scale}_populated_places` -- cities and capitals

### Format and size

- Available as: shapefile, SQLite, GeoPackage
- Individual layer shapefiles: 100 KB - 20 MB each
- Full SQLite: 423 MB zipped (~791 MB unzipped)
- Full GeoPackage: 436 MB zipped
- Elivagar would need ~6-8 individual shapefiles, ~50 MB total
- CRS: **EPSG:4326** (WGS84 lat/lon), unlike the ocean shapefile which is EPSG:3857
- License: public domain, no attribution required
- Current version: v5.1.2 (May 2022). Periodic updates, not fixed schedule.

## How Planetiler uses Natural Earth

Source: `planetiler-core/.../reader/NaturalEarthReader.java`

- Reads from SQLite database via JDBC (deprecated in favor of GeoPackage)
- Iterates all tables matching `ne_[a-z0-9_]+`, reads WKB geometry column
- Single-threaded sequential reads (NE data is small)
- NE features flow through the same pipeline as OSM features
- Profile (OpenMapTiles YAML) maps NE tables to output layers with zoom ranges

### Planetiler's NE water mapping (from custommap sample)

| NE Layer | Min Zoom | Max Zoom |
|----------|----------|----------|
| `ne_110m_ocean` | 0 | 1 |
| `ne_110m_lakes` | 0 | 1 |
| `ne_50m_ocean` | 2 | 4 |
| `ne_50m_lakes` | 2 | 3 |
| `ne_10m_lakes` | 4 | 5 |
| `ne_10m_ocean` | 5 | 5 |

**Note:** Planetiler's own Shortbread profile does NOT use Natural Earth. It only
uses the ocean shapefile + OSM, same as elivagar. The NE integration is in the
OpenMapTiles profile. So this would be a differentiation point.

## How Tilemaker uses Natural Earth

Selective use for landcover only:
- `ne_10m_urban_areas` -> `landuse` at z4-z8
- `ne_10m_antarctic_ice_shelves_polys` -> `landcover` at z0-z9
- `ne_10m_glaciated_areas` -> `landcover` at z2-z9

Reads as individual shapefiles, configured in JSON config.
The Shortbread tilemaker profile does NOT use Natural Earth.

## Shortbread layers that benefit

### Strong candidates

1. **`water_polygons` (z0-5)** -- Major lakes, glaciers. Currently from OSM with heavy
   simplification. NE `ne_*_lakes` + `ne_*_glaciated_areas` would be pre-generalized
   and cartographically cleaner.

2. **`boundaries` (z0-5)** -- Country boundaries. Currently from OSM relations with
   heavy simplification. NE `ne_*_admin_0_boundary_lines_land` are cartographically
   generalized with clean topology.

### Keep as-is

3. **`ocean` (z0-7)** -- Already uses simplified ocean shapefile from
   osmdata.openstreetmap.de (weekly updates). NE ocean is less current. No change needed.

4. **`place_labels` / `boundary_labels`** -- Only 2 zoom levels benefit (z4-5). OSM
   data works fine for point features. Not worth the complexity.

## Implementation design

### Data pipeline

Natural Earth phase slots into the pipeline alongside ocean:

```
phase12 (PBF) -> natural_earth -> ocean -> sort -> assemble
```

Or fold into the ocean phase as a second step (same pattern, different data).

### Data format choice

**Individual shapefiles** (recommended):
- Elivagar already has a shapefile reader in `ocean.rs`
- Much smaller download (~50 MB vs 400+ MB for full SQLite)
- No SQLite dependency needed
- Matches existing ocean shapefile approach
- Download via `brokkr download natural-earth`

### Coordinate handling

NE shapefiles are EPSG:4326 (WGS84 lon/lat), not EPSG:3857 like the ocean shapefile.
The reader needs to project to Mercator [0,1] space. This is straightforward:
`geometry::project(lat, lon)` already exists, just needs to be called instead of
`geometry::from_epsg3857()`.

### New module: `natural_earth.rs`

Similar to `ocean.rs`:
1. Read .shp/.shx files (reuse shapefile parsing from ocean.rs, or extract shared reader)
2. Project EPSG:4326 coordinates to Mercator [0,1]
3. Filter by data bounds (bbox check)
4. For each feature: determine Shortbread layer, zoom range, attributes
5. Emit `SortRecord`s to the sort stream via rayon fold (same as ocean)

### Layer mapping

| NE Layer | Shortbread Layer | Kind/Attrs | Zoom |
|----------|-----------------|------------|------|
| `ne_110m_lakes` | `water_polygons` | `kind=water` | z0-1 |
| `ne_50m_lakes` | `water_polygons` | `kind=water` | z2-3 |
| `ne_10m_lakes` | `water_polygons` | `kind=water` | z4-5 |
| `ne_110m_glaciated_areas` | `water_polygons` | `kind=glacier` | z0-1 |
| `ne_50m_glaciated_areas` | `water_polygons` | `kind=glacier` | z2-3 |
| `ne_10m_glaciated_areas` | `water_polygons` | `kind=glacier` | z4-5 |
| `ne_110m_admin_0_boundary_lines_land` | `boundaries` | `admin_level=2` | z0-1 |
| `ne_50m_admin_0_boundary_lines_land` | `boundaries` | `admin_level=2` | z2-4 |
| `ne_10m_admin_0_boundary_lines_land` | `boundaries` | `admin_level=2` | z5 |

### Scale transition (NE -> OSM)

At the z5->z6 boundary, NE features have `max_zoom=5` and OSM features have `min_zoom=6`.
Simple cutoff, same approach as Planetiler. Any visual seam at the transition zoom is
acceptable -- users zoom through it quickly.

To avoid duplicate features at z4-5 (where both NE and OSM currently emit):
- **Option 1**: Increase `min_zoom` of OSM-derived water/boundary features to z6.
  Cleaner, but changes existing behavior.
- **Option 2**: Keep both, let NE override by sort order. Messier, potential duplicates.

Option 1 is recommended. The Shortbread profile `min_zoom` for water_polygons and
boundaries can be split: NE source at z0-5, OSM source at z6-14.

### CLI flags

- `--natural-earth dir/` -- path to directory containing NE shapefiles
- `--no-natural-earth` -- disable (falls back to OSM-only like today)
- Auto-detection from `data/` like the ocean shapefiles

### Performance impact

Negligible. NE data is tiny:
- A few thousand features across all layers
- Processing time: well under 1 second
- Memory: a few MB of parsed geometry
- Actually **saves** time by removing heavy DP simplification of OSM geometry at z0-5

### Prerequisite work

1. Extract shared shapefile reader from `ocean.rs` (or duplicate with modifications
   for EPSG:4326 input)
2. Add EPSG:4326 -> Mercator projection path to the reader (trivial -- call existing
   `geometry::project()`)
3. Decide on Shortbread layer mapping for NE attributes (see table above)

## Status

Research complete. This is a moderate-effort feature with clear benefits for low-zoom
tile quality. The ocean phase provides an exact implementation template. Main work is
the new `natural_earth.rs` module and the z0-5 source switching in the Shortbread profile.
Worth doing before planet-scale release.
