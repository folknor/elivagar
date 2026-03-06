# Non-Mercator Tiling: OGC TileMatrixSet v2 Investigation

Investigated 2026-03-06. Triggered by TODO item:
> Investigate OGC TileMatrixSet v2 / non-Mercator tiling schemes. Nidhogg's API surface
> is expanding, and consumers may need non-WebMercator projections (EPSG:4326, polar, etc.).
> Tippecanoe has an open request for this (tippecanoe #286).

## OGC TileMatrixSet v2 (OGC 17-083r4)

Standard data model for tiling schemes. Instead of hardcoding WebMercator's z/x/y grid,
the tiling scheme itself becomes a configurable object with a registered URI.

Key additions over v1:
- Registry of 13+ well-known TileMatrixSets at `http://www.opengis.net/def/tms`
- Variable-width tile matrices for polar regions
- JSON encoding (v1 was XML-only)
- TileSet Metadata structure

### Registered tiling schemes beyond WebMercator

| TileMatrixSet | CRS | Notes |
|---|---|---|
| WorldCRS84Quad | CRS84 (lon/lat) | 2 tiles at z0 (2:1 aspect), poles included |
| WGS1984Quad | EPSG:4326 | Same, lat/lon axis order |
| WorldMercatorWGS84Quad | EPSG:3395 | Elliptical Mercator (not spherical) |
| EuropeanETRS89_LAEAQuad | EPSG:3035 | Lambert Azimuthal Equal-Area for Europe |
| UPSArcticWGS84Quad | EPSG:5041 | Universal Polar Stereographic, Arctic |
| UPSAntarcticWGS84Quad | EPSG:5042 | Universal Polar Stereographic, Antarctic |
| CanadianNAD83_LCC | EPSG:3978 | Lambert Conformal Conic |
| CDB1GlobalGrid | EPSG:4326 | Variable matrix width |
| GNOSISGlobalGrid | EPSG:4326 | Variable matrix width |
| NZTM2000Quad | EPSG:2193 | New Zealand Transverse Mercator |

Strongest demand: polar mapping (WebMercator clamps at +/-85 degrees) and planetary
science (Moon/Mars/Europa with IAU coordinate systems).

## EPSG:4326 vs WebMercator tiling

The fundamental structural difference:

- **WebMercatorQuad (EPSG:3857)**: World is a square at z0. One tile. Grid is always
  NxN (power-of-two squares). Latitude clamped to ~85.05 degrees. Poles excluded.

- **WorldCRS84Quad (EPSG:4326)**: World is a 2:1 rectangle at z0. Two tiles side by
  side. Grid at zoom z is 2^(z+1) x 2^z (twice as wide as tall). Poles included.
  Projection is trivial (linear scaling of lon/lat) but the non-square grid breaks
  quadtree assumptions throughout the toolchain.

## Polar projections

UPSArcticWGS84Quad (EPSG:5041) and UPSAntarcticWGS84Quad (EPSG:5042) use Universal
Polar Stereographic projection centered on the pole. Important because:
- WebMercator is useless at the poles (clamped at ~85 degrees)
- EPSG:4326 has severe distortion near poles
- Polar stereographic is conformal (preserves shape)

Other polar CRSes in use: EPSG:3031 (Antarctic), EPSG:3413 (NSIDC Sea Ice North),
EPSG:3575 (North Pole LAEA).

## Tippecanoe #286

Filed by AndrewAnnex (planetary science). Request: accept OGC TileMatrixSet JSON instead
of just a projection parameter. MVT v2.1 already allows arbitrary projections.

Proposed approach: read TileMatrixSet, extract CRS constants, use for projection/tiling.
Initial scope restricted to "true quad-tree" schemes (no 2:1 grids). Noted blockers:
PMTiles/mbtiles spec limitations and no renderer support.

Related older issues: mapbox/tippecanoe #770 (custom schemas), #422 (non-Mercator),
#546 (other projections).

Reference implementation: morecantile (Python, implements all OGC TileMatrixSets).

## PMTiles v3 and non-Mercator

PMTiles v3 is structurally WebMercator-locked:

1. **No CRS field** in the 127-byte header.
2. **Hilbert tile IDs assume square grids**: `base = (4^z - 1) / 3`, `n = 2^z`.
   A 2:1 EPSG:4326 grid (2^(z+1) x 2^z) breaks this.
3. Header bounds are WGS84 lon/lat -- semantically fine for most CRS but doesn't
   communicate the projection.
4. GDAL's PMTiles driver explicitly states "SRS is always EPSG:3857".

Non-Mercator PMTiles would require a v4 spec or creative metadata conventions.

## Nidhogg relevance

Nidhogg serves tiles at `GET /api/tiles/{z}/{x}/{y}` from PMTiles archives produced by
elivagar. Its query API uses WGS84 bbox. Non-Mercator support would need coordinated
changes in elivagar (generation), nidhogg (serving), and the frontend renderer.

## WebMercator coupling points in elivagar

12 coupling points identified, concentrated in geometry.rs, ocean.rs, pmtiles_writer.rs:

### Very Hard to abstract

- **Mercator projection math** (geometry.rs:128-196): WGS84-to-Mercator formulas,
  2^18-entry LUT, EARTH_CIRCUMFERENCE constant, +/-85.05 degree clamping.
  Would need complete rewrite per projection.

- **Hilbert curve tile IDs** (pmtiles_writer.rs:944-1037): `n = 2^z` square grid,
  cumulative offset `(4^z - 1) / 3`. Fundamentally assumes square grids.

### Hard to abstract

- **Tile coordinate conversion** (geometry.rs:214-218, 634-647): Tile size = 1/2^z
  Mercator units. ClipRect::for_tile() builds clip rects from this assumption.

- **Simplification tolerance** (geometry.rs:25,43,58,225-228): `SIMPLIFY_PIXELS /
  (256.0 * 2^z)` in Mercator units. Tied to 256px tiles on a 2^z grid.

- **Ocean shapefile** (ocean.rs:2-3, 104-105, 157, 220-222): Assumes EPSG:3857 format
  (`water-polygons-split-3857`), direct `from_epsg3857()` calls, z8 splitting on
  Mercator grid.

- **Land mask** (geometry.rs:1491-1595): z14 = 2^14 x 2^14 bitset, ancestor lookup
  via bit-shift. Completely hardcoded to z14 WebMercator resolution.

- **DDA rasterization** (ocean.rs:602-683): Integer grid floor(p * 2^z). Specific to
  XYZ WebMercator grid enumeration.

### Medium difficulty

- **Tile grid iteration in ocean** (ocean.rs:408-442): 2^z grid enumeration.
- **Zoom range constraints** (pipeline.rs:370-374): Max zoom = 14.
- **Tile extent / pixel constants** (geometry.rs:20,28,32): 4096 extent, 256px tiles,
  8px buffer. These are MVT spec, not projection-specific, but simplification math
  depends on them.
- **Subpixel checks** (geometry.rs:42-96): 256x256px, 4096 extent thresholds.

### Easy

- **PMTiles bounds & center** (pipeline.rs:2862-2863): Hardcoded [-180, 180] x
  [-85.05, 85.05]. Could be computed from projection bounds.

## External blockers

This is not actionable in the near term due to external dependencies:

1. **PMTiles v3 has no CRS field and its Hilbert scheme is square-grid-only.**
   No container format to write non-Mercator tiles into until protomaps ships a
   v4 or a non-Mercator addressing convention.

2. **MapLibre doesn't render non-Mercator vector tiles.** maplibre/maplibre#163
   is an open discussion with no implementation. OpenLayers can do it but that's
   not nidhogg's renderer.

3. **No ocean shapefiles in non-Mercator projections.** OSMdata water-polygons-split
   only ships EPSG:3857 and EPSG:4326.

## Implementation path (when blockers clear)

The concrete trigger would be:
- PMTiles v4 with CRS support, OR
- MapLibre shipping non-Mercator vector tile rendering, OR
- A nidhogg consumer that concretely needs EPSG:4326 or polar tiles

If/when that happens:

1. Define a `TilingScheme` trait: projection (forward/inverse), grid dimensions per
   zoom, tile bounds computation, simplification tolerance derivation.

2. Implement `WebMercatorQuad` as default. Zero runtime cost via monomorphization
   (generic parameter, not trait object).

3. Add `WorldCRS84Quad` as second implementation. Simplest non-Mercator scheme:
   linear lon/lat scaling, 2:1 grid at z0.

4. Thread the scheme through geometry.rs (projection, clipping, simplification),
   ocean.rs (shapefile loading, grid iteration), pmtiles_writer.rs (tile addressing).

5. Solve Hilbert addressing for rectangular grids. Options:
   - Rectangular Hilbert variant (exists in literature but not standard in PMTiles)
   - Z-order curve fallback (simpler, worse locality)
   - Two separate square Hilbert curves for the 2:1 case (one per z0 tile)
   - Wait for PMTiles v4 to specify the addressing scheme

6. Ocean data: for EPSG:4326, the `water-polygons-split-4326` variant exists.
   For polar projections, would need to reproject from one of these.

## Status

**Watch-list item.** Keep monitoring PMTiles spec evolution and MapLibre non-Mercator
progress. No code changes warranted until external blockers clear.
