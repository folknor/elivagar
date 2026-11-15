# Coordinate spaces

Geometry moves through geographic, projected, integer, tile-local, and archive
ordering spaces. Keep rendered pixels distinct from MVT extent units: one tile
is 256 rendered pixels wide but 4096 MVT extent units wide, so one rendered
pixel is 16 extent units.

## Spaces

| Space | Representation | Meaning |
|---|---|---|
| Geographic e7 | Signed integer latitude and longitude, degrees multiplied by 10,000,000 | The fixed-point coordinates read from OSM data. |
| Geographic degrees | Floating-point latitude and longitude | The input to the exact projection helper and the degree values represented by e7 coordinates. |
| EPSG:3857 | Floating-point metres | Web Mercator metres, centered on the world origin. |
| Normalized Mercator, wrapped | `Point { x, y }` floats | World coordinates where x runs west to east from 0 to 1 and y runs north to south from 0 to 1. Wrapped x is reduced modulo 1. |
| Normalized Mercator, unwrapped | `Point { x, y }` floats | The same projected coordinates with x allowed outside 0 to 1 so a path crossing the antimeridian can take the short route. |
| Base pixel grid, or base integer grid | Signed integer x and y | Normalized Mercator quantized at the grid's maximum zoom, in MVT extent units at that zoom: the profile's top zoom for OSM polygons, the pass maximum for ocean. These units are not 256-pixel rendered pixels. |
| Zoom pixel grid, or zoom integer grid | Signed integer x and y | Base integer coordinates rescaled to zoom z. A tile spans 4096 units on each axis. |
| Tile-local MVT coordinates | Signed integer x and y | Zoom-grid coordinates minus the selected tile origin. Buffered geometry may lie outside the usual 0 to 4096 tile square. |
| Tile address | `(z, x, y)` | A zoom and a coordinate in that zoom's `2^z` by `2^z` tile matrix. |
| Hilbert tile id | Unsigned integer | PMTiles ordering id derived from a tile address. It orders tiles along a Hilbert curve within each zoom and places zooms in consecutive ranges. |
| Sort key | Unsigned 64-bit integer | A Hilbert tile id followed by the layer byte and paint-priority byte. |
| Sort partition id | Unsigned integer | An ordered bucket derived from the zoom and the high-order Hilbert id range. It groups sort records for storage and assembly; it is not a tile address. |
| Vertex identity key | Integer pair | A lookup key for recognizing shared vertices. The float-space key uses scaled normalized Mercator coordinates; polygon pinning also uses quantized base-grid coordinates. |

## Geographic and projected coordinates

The e7 reader values represent degrees as `value / 10,000,000`. Projection
maps longitude to `x = (longitude + 180) / 360`. Latitude is clamped to the
Web Mercator limit, then maps to
`y = 0.5 - ln(tan(phi) + sec(phi)) / (2 * pi)`, where `phi` is latitude in
radians. The hot e7 path uses a latitude lookup table with interpolation; the
exact floating-point helper uses the formula directly.

EPSG:3857 metres convert to normalized Mercator using the Web Mercator world
circumference `C`:

```text
x = (east_m + C / 2) / C
y = (C / 2 - north_m) / C
```

For a path crossing the antimeridian, unwrapping adjusts each x by whole world
widths so consecutive x differences are at most half a world. A closed path's
last coordinate is then set to its first. Where a point must be wrapped,
`x.rem_euclid(1)` returns it to one world copy.

Relation members are joined in their original wrapped coordinates before
unwrapping each complete ring. Containment and hole pairing then compare each
inner ring in the world copy nearest the candidate shell. This keeps shared
endpoints joined and avoids choosing a hole's world copy from an unrelated
member of a multipart relation. Shared-vertex pin lookup accepts whole-world
translations of the source coordinates.

## Tile coordinates and integer polygons

For a normalized Mercator point and tile address `(z, tx, ty)`, the floating
point path converts to tile-local MVT extent units as follows:

```text
local_x = round((point.x * 2^z - tx) * 4096)
local_y = round((point.y * 2^z - ty) * 4096)
```

Tile enumeration floors the scaled Mercator coordinate to get tile indices,
then clamps those indices to the world edge. Bounding-box covers are
inclusive, so a box on a tile boundary can include both sides. A box that
misses the world `[-1e-8, 1 + 1e-8]` on either axis covers no tile rather
than clamping onto the edge tile; the slack admits the projected latitude
clamp, which overshoots `[0, 1]` in y by about `7.1e-9`. Lines and points
emit only their unshifted copy and the copies shifted one world width either
way, so an unwrapped way winding further than that loses the content beyond
them. Every emitted line or point coordinate therefore lies near its tile:
inside it on the single-tile path, inside the buffered rectangle on the
clipped path, and within one extent unit of the tile for points.

The integer polygon path quantizes a normalized coordinate `v` to the base
grid with `round(v * 2^(M + 12))`, where `M` is the maximum zoom used to build
that grid. This quantization does not clamp coordinates to the world, so an
unwrapped x coordinate remains unwrapped. Because it does not clamp, every
rounded base coordinate is checked against the overlay engine's integer
interval `[-2^30, 2^30 - 1]`, and a polygon with any vertex outside it (or
non-finite) is refused whole rather than quantized: OSM polygons skip that
world copy and count it, while ocean never reaches the check because the
shapefile reader refuses points outside normalized `[-1, 2]`. `M` is at most
z14, which bounds tile sizes and buffers but not input coordinates; an
antimeridian unwrap follows each previous vertex and has no fixed span.
Since `4096 = 2^12`, one tile at
zoom `z` spans `4096 * 2^(M - z)` base units. Rescaling to zoom z rounds the
signed base coordinate to the nearest integer, with exact half values rounded
away from zero. The tile origin is `(tx * 4096, ty * 4096)` in zoom-grid
units; subtract it to get tile-local MVT coordinates. This rescaling rule
does not describe every clip intersection: the pyramid's axis crossings
order the segment endpoints by the clipped axis, then round the displacement
from the endpoint with the smaller coordinate to nearest, ties away from zero.
Subtractions and rational products use i128 arithmetic before converting the
crossing back to i32, so the calculation also covers the full i32 input range.
The endpoint order is independent of traversal direction and the kept
half-plane, so opposite traversals of a shared edge produce the same integer
crossing. The topology engine has its own snap arithmetic, including
truncation for axis-aligned segments.

Floating-point projection-to-tile conversion and initial polygon quantization
also round to nearest, with exact half values rounded away from zero.

The buffer is 8 rendered pixels, or 128 MVT extent units, on each tile edge.
The point path selects tiles from the point's bounding box and encodes the
point without a clipping buffer. The line path selects candidate tiles from
the unbuffered simplified line bounding box. When the line spans multiple
tiles, it clips each segment to the selected tile's buffered rectangle; a
bounding box whose unclamped tile indices name one in-world tile uses a fast
path without clipping. The polygon
pyramid selects its root cells from the unbuffered shape bounds. As it
descends, it intersects parent fragments with every child's buffered
rectangle. A descendant can therefore contain geometry in its buffer even
when that geometry does not enter the descendant tile's proper area. Only
line tiles selected from the unbuffered simplified bbox can carry line buffer
geometry; polygon descent can emit buffer-only descendants, while points are
never clipped.

## Archive and sort ordering

For a tile address `(z, x, y)`, the PMTiles id is
`(4^z - 1) / 3 + H(2^z, x, y)`, where `H` is the Hilbert distance in that
zoom's tile matrix. Zoom zero has tile id zero. This id is not an x/y encoding.
Ids exist for zooms 0 through 31; the first z32 id, `(4^32 - 1) / 3`, still
fits in 64 bits and is the exclusive end of the address space. A directory
entry or run reaching past it is malformed.

Two zoom domains share these ids. The archive domain is z0 through z31: the
tools that read any archive work in it. The profile domain is z0 through z14,
the Shortbread schema's top zoom. Every "last zoom" policy (no
simplification, no size filter, no line merging, the top compression tier)
asks about z14, and a run's configured zoom range is a truncation window over
the profile domain: lines and polygons are constructed over each layer's own
zoom range and only emission is restricted, so a narrowed archive holds
exactly the tiles a full build holds at those zooms. Computed ocean follows
the same rule on its own fixed grids: the low pass quantizes and descends on
the z7 grid from z0, the high pass on the z14 grid from z8, and the window
only decides which of those zooms are written.

The sort key is:

```text
(tile_id << 16) | (layer << 8) | paint_priority
```

The tile id field is 48 bits, so a key can be built only for a profile-domain
tile; anything beyond z14 is refused when the key is built and when it is
partitioned, never clamped. The layer byte is a Shortbread layer id, typed as
a layer when the key is built; a key read back from a sort chunk whose layer
byte names no layer is refused when assembly groups it into a tile.

Sort partitioning first identifies the zoom range containing the tile id,
then uses its high-order Hilbert range to select an ordered partition. The
split zoom is an implementation constant. Partition ids are for sort and
assembly grouping; use the tile address or Hilbert id when identifying a tile.

## Tolerance, area, and vertex identity

Floating-point simplification tolerance is measured in normalized Mercator
units. One rendered pixel at zoom z is `1 / (256 * 2^z)` in that space.
Lines simplify their source independently at each zoom below z14, with any
pinned vertices required, and are never simplified from another zoom's
result, so each zoom's line deviates from the source by at most that zoom's
tolerance before clipping and quantization. A closed line that collapses
within tolerance onto its farthest-pair chord is kept as the closed
out-and-back along that chord, never dropped, whichever vertex its source
starts at; simplification removes a closed line only when all its points
coincide. Rounding to tile coordinates can produce the same out-and-back
from a thin loop, and it is kept the same way.

A closed line's simplified vertex set depends on its vertex cycle and its
required vertices, not on which vertex the source starts at: the anchor
search and DP run from a canonical rotation of the cycle. The source start is
therefore not kept for its own sake. With injected shared-node pins it needs
no special case, because a start shared with another way is pinned like any
junction. Block-local pins (raw input) cannot see junctions with ways in
other blocks, so on that path a closed line also requires its source start,
which makes raw-input output start-dependent by construction. A ring whose
coordinates cross the antimeridian is also outside the guarantee: unwrapping
runs from the source start, so different starts can produce different
unwrapped coordinates.

Integer polygon simplification works after rescaling to a zoom grid. Its
tolerance is measured in zoom-grid MVT extent units; 16 units equal one
rendered pixel. Integer polygon area filters compare square extent units.
The signed integer shoelace helper returns twice the signed area, while the
area filter compares the absolute area after dividing by two. The
Visvalingam threshold is in doubled triangle-area units. Do not compare these
integer quantities directly with normalized Mercator area or physical square
metres.

The `area_sq_meters` helper converts normalized Mercator ring area to
approximate square metres using a latitude correction along each edge. This
physical area is separate from the integer grid area used by polygon filters.

Shared-vertex lookup in normalized Mercator rounds each coordinate multiplied
by 1,000,000,000,000 into an integer pair. Polygon pinning after quantization
uses integer base-grid coordinate pairs. These keys are equality lookup
coordinates, not persistent feature identifiers or archive hashes.

The implementation for these conversions lives in `geometry/projection.rs`,
`geometry/tiles.rs`, `geometry/int_ocean.rs`, `geometry/pyramid.rs`,
`pipeline/emit.rs`, `tiling.rs` (zoom domains, tile ids and the Hilbert
curve), and `sort/key.rs`.
