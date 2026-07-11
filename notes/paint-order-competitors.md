# Within-layer feature ordering: how planetiler, tilemaker, and tippecanoe do it

Survey of the three vendored competitor tile generators, focused on the
questions behind elivagar's upcoming paint-order key: what they sort by,
where in the code, whether the order is physical or attribute-based, how they
stay deterministic under parallelism, and what they learned the hard way.
All pointers are into `research/planetiler/`, `research/tilemaker/`,
`research/tippecanoe/`.

## 1. Planetiler (Java)

### The key

Every feature carries an explicit integer sort key, set by profile code via
`FeatureCollector.Feature.setSortKey(int)` / `setSortKeyDescending(int)`
(`planetiler-core/src/main/java/com/onthegomap/planetiler/FeatureCollector.java`,
around line 598). The key is 22 bits, signed range -2^21 .. 2^21-1
(`FeatureGroup.SORT_KEY_BITS = 22`).

The sort key is not applied per tile at encode time. It is packed directly
into the 64-bit key of the global external merge sort, in
`FeatureGroup.encodeKey`
(`planetiler-core/src/main/java/com/onthegomap/planetiler/collection/FeatureGroup.java`,
line 168):

```
[tile: 33 bits][layer: 8 bits][sortKey: 22 bits][hasGroup: 1 bit]
```

So within one tile, features come off the merge already grouped by layer and
ordered by sort key. `TileFeatures.getVectorTile` (same file, line 527) just
walks the sorted entries, buckets them by layer, runs post-processing, and
encodes in that order. Ordering falls out of the sort by construction; there
is no second sort.

### Composing the key

`util/SortKey.java` is a small builder that packs a multi-field ORDER BY into
the 22-bit int: `orderByInt(rank, min, max).thenByLog(population, max, min,
levels).thenByInt(...)`. Each component declares its range and level count;
`accumulate` multiplies levels together and throws if the product exceeds the
22-bit budget. Descending order is expressed by swapping the range endpoints.
The javadoc gives the canonical example: `ORDER BY rank ASC, population DESC,
length(name) ASC`. This is how OpenMapTiles-style `sort_rank` / label rank /
population ordering gets flattened into one int.

Per-layer configurability: entirely in profile code (each layer handler sets
whatever key it wants), and the declarative `planetiler-custommap` YAML
schema exposes `sort_key` / `sort_key_descending` per feature rule
(`planetiler-custommap/.../configschema/FeatureItem.java`, lines 18-19).

### Renderer contract (documented)

The `setSortKey` javadoc (FeatureCollector.java, lines 598-608) is the
clearest statement any of the three tools makes about renderer expectations:

- "Circles, lines, and polygons are rendered in the order they appear in
  each layer, so features that appear later (higher sort key) show up on top
  of features with a lower sort key."
- "For symbols (text/icons) where clients try to avoid label collisions,
  features are placed in the order they appear in each layer, so features
  that appear earlier (lower sort key) will show up at lower zoom levels than
  features that appear later (higher sort key) in a layer."

So: physical order in the layer IS the paint order for fills/lines, and IS
the placement priority for symbols (inverted: early = wins collisions).
No explicit order attribute is emitted; the ordering is physical.

### Determinism

The external merge sort's k-way merge takes an explicit tiebreaker:
`ExternalMergeSort.iterator` (line 269) passes
`SortableFeature.COMPARE_BYTES` to `LongMerger.mergeIterators`.
`COMPARE_BYTES` (`collection/SortableFeature.java`, line 7) is an unsigned
lexicographic compare of the feature's entire encoded value byte array. So
when two features have identical 64-bit keys (same tile, layer, sort key),
the merge breaks the tie on the full serialized payload - deterministic
regardless of chunk count, thread scheduling, or merge fan-in order.
`SortableFeature.compareTo` does the same for the in-chunk sort. There is no
sequence number; the payload bytes ARE the tiebreak.

### Bonus use of the same key

Per-group point limits reuse sort order for survival: `TileFeatures.add`
(FeatureGroup.java, lines 616-637) counts features per group as they arrive
in sorted order and discards past the limit - so the lowest sort keys (the
highest-priority labels) are the ones kept. Sort key = paint order = drop
priority, one mechanism.

## 2. Tilemaker (C++)

### The key

Lua profiles call `ZOrder(number)` on an emitted feature
(`src/osm_lua_processing.cpp`, line 1009). It is stored as a 16-bit signed
bitfield `z_order` on `OutputObject` (`include/output_object.h`, line 46,
comment: "used for sorting features within layers"). `setZOrder`
(output_object.h, lines 54-62) accepts a double in -50M..50M and compresses
lossily: `z*10` within +-1000 (0.1 resolution), sqrt-compressed beyond. The
docs (docs/CONFIGURATION.md, line 172) admit the lossiness. This contortion
exists only because the Lua API accepts arbitrary doubles (Imposm
compatibility); the useful range in practice is tiny (see below).

### Where the sort happens

Per tile, at output time. `TileDataSource::getObjectsForTile`
(`src/tile_data.cpp`, line 479) collects the tile's objects and calls
`sortOutputObjectIDs` (`src/tile_sorting.cpp`, line 76), a pdqsort with a
full lexicographic comparator:

1. `layer` ascending
2. `z_order` - direction per layer from `sortOrders[layer]`
3. `geomType`
4. `attributes` (attribute-set index)
5. `objectID` (OSM/synthetic id) as final tiebreak

The in-code comment explains why attributes come before objectID: "It is to
arrange objects with the identical attributes continuously. Such objects
will be merged into one object, to reduce the size of output." So the sort
serves both paint order and merge adjacency at once.

### Per-layer configuration

JSON layer config option `z_order_ascending` (`src/shared_data.cpp`, line
325): defaults to true, but flips to descending when the layer sets a
`feature_limit` - because the limit truncates the sorted list, and with
descending order the highest z_order (most important) features survive. Docs
(CONFIGURATION.md, line 88 and 172) document both, plus two interaction
warnings: sorting does not work across layers merged with `write_to`, and
features with different z_order will not be merged by
`combine_points/lines/polygons_below`.

### Renderer contract (documented)

CONFIGURATION.md line 172: "Use this feature to ensure a proper rendering
order if the rendering engine itself does not support sorting." Same
physical-order model as planetiler; no explicit order attribute emitted.

### The shipped recipe

`resources/process-openmaptiles.lua`, `SetZOrder()` (lines 896-932)
implements Imposm's wayzorder for the transportation layer:

- bridge: +10, tunnel: -10
- OSM `layer` tag clamped to +-7, times 10
- highway class: motorway 9, trunk 8, primary 6, secondary 5, tertiary 4,
  everything else 3

Total practical range is roughly -80 .. +89. That is the entire real-world
z-order budget an OpenMapTiles-grade street stack needs: well under 8 bits.

### Determinism

The comparator is a total order (objectID last), so per-tile output order is
deterministic no matter what order the parallel stores delivered objects.
Tilemaker gets determinism from the tiebreak chain, not from controlling
producer order.

## 3. Tippecanoe (C++)

### Default order: spatial, with a sequence tiebreak

Tippecanoe's global feature sort is by spatial index of the centroid
(z-order curve; Hilbert with `--hilbert`, which the README says "should be
the default eventually"). The comparator `indexcmp` (`main.cpp`, line 246)
compares `index`, then `seq` - an input sequence number carried through the
whole pipeline. That tiebreak makes the default within-tile order (spatial)
fully deterministic and is also what powers order restoration later.

### Opt-in reorderings, applied per tile at encode time

All in `tile.cpp`, applied to `layer_features` just before encoding
(lines 2556-2566), both as `std::stable_sort` over already-deterministic
input:

- `--preserve-input-order` (`-pi`): `preservecmp` (tile.cpp, line 94) sorts
  by `seq`. README (line 523) calls this "the drawing order" and notes it is
  implemented as a restoration at the end so that dot-dropping still happens
  geographically.
- `--order-by` / `--order-descending-by` / `--order-smallest-first` /
  `--order-largest-first`: `ordercmp` (tile.cpp, line 342) compares
  user-named attribute values coerced to double, first flag is primary key.
  Comment at line 339: "If there is a tie, the feature with the earlier
  index (centroid) comes first." `--order-smallest-first` works via the
  `ORDER_BY_SIZE` pseudo-attribute that reads the feature's `extent`
  (tile.cpp, line 288).

No explicit order attribute is emitted for the renderer; like the others,
ordering is physical. Tippecanoe is schema-agnostic so it cannot know road
classes; it can only order by attributes the user names, size, or input
order. That is the structural reason its ordering is per-tile re-sort rather
than baked into a sort key: the key material (attributes) is not known to be
an int at write time.

### Hard-learned lessons (CHANGELOG)

- "Stabilize feature order in tippecanoe-overzoom when
  --preserve-feature-order is specified but the sequence attribute is not
  present" (CHANGELOG.md, line 195) - order stability regressions happen
  when the tiebreak token goes missing; carry it everywhere or suffer.
- "Feature coalescing, line-reversing, and reordering by attribute are now
  options, not defaults" (line 1325, v1.16) - reordering for compression
  used to be default behavior and was demoted, because rearranging features
  silently changes drawing order. Size optimizations that touch order must
  be opt-in.
- Lines 230/234: multiple releases spent making the ordering flags cooperate
  with `--retain-points-multiplier` clusters (order clusters by lead
  feature, keep cluster members adjacent) - once order carries meaning,
  every later feature-shuffling mechanism has to be taught about it.
- `--preserve-input-order` explicitly "undoes `-ao`/--reorder" (README line
  523): paint order and coalesce-friendly order are competing sorts; you
  pick one.

## Cross-tool summary

| | key | width | where sorted | tiebreak | configurable |
|---|---|---|---|---|---|
| planetiler | explicit int sort key from profile | 22 bits inside the global sort key | global external merge (by construction) | full encoded value bytes, unsigned lexicographic | per feature in profile code / YAML |
| tilemaker | Lua `ZOrder(n)` | 16-bit signed (lossy beyond +-1000) | per tile at output collection | geomType, attributes, objectID | per layer: z_order_ascending |
| tippecanoe | spatial index (default), seq, or named attributes | 64-bit index + 64-bit seq | global sort + optional per-tile stable_sort | seq (default), index (order-by) | CLI flags, whole-tileset |

All three: physical feature order in the layer is the paint order; none
emits an order attribute for the renderer to sort by. Planetiler documents
the MapLibre/Mapbox GL contract explicitly (fills/lines: later = on top;
symbols: earlier = placement priority). All three achieve determinism the
same way in the end: a total order with an explicit tiebreak that does not
depend on thread scheduling - planetiler via payload bytes, tilemaker via
attributes+objectID, tippecanoe via a carried sequence number.

## What elivagar should do

**Copy planetiler's architecture.** Put the paint priority inside the global
sort key so within-layer order falls out of the existing partitioned
sort/merge by construction - no per-tile re-sort, no decode-and-compare at
assemble time. Our wire format already reserves a priority byte in the sort
key; this is exactly planetiler's `[tile][layer][sortKey]` layout with a
narrower priority field. Tippecanoe's per-tile attribute re-sort is the
model to avoid: it exists because tippecanoe does not know its schema; we
do.

**Copy planetiler's tiebreak, verbatim in spirit.** After
(tile, layer, priority), break ties by unsigned lexicographic compare of the
full encoded record bytes in the k-way merge heap. That single change kills
the run-to-run nondeterminism at its source (parallel merge tie order) and
needs no new state carried through the pipeline - the record bytes already
exist at the comparison site. A sequence number (tippecanoe-style) would
also work but means threading a new field through phase12's parallel
emission, where assigning it deterministically is itself the hard problem.
Byte-compare sidesteps that entirely, and it makes identical records adjacent,
which is tilemaker's deliberate attributes-before-id trick for merge
adjacency - useful for our same-attribute feature merging too.

**One u8 is enough - the evidence is strong.** Tilemaker's shipped
OpenMapTiles recipe (Imposm wayzorder: bridge/tunnel +-10, layer tag +-70,
highway class 3..9) spans about 170 distinct values; a u8 holds it with room
to spare. Planetiler's 22 bits are spent on things we do not need in the
key: log-bucketed population ranks for label ordering across thousands of
levels. For Shortbread polygon stacking (the parks-under-residential bug)
a handful of levels per layer suffices; for street casing order a
class+bridge/tunnel scheme fits in far less than 8 bits. If a layer ever
needs finer grading than 256 levels, that is a labels problem, and the
symbol-priority convention (lower = placed first = wins) can be revisited
then.

**Semantics to pin in the spec** (matching the documented renderer
contract):

- Priority sorts ascending within (tile, layer); lower priority = earlier in
  the layer = painted first = underneath. Parks get a higher priority byte
  than residential landuse; water above both; casings below fills for
  streets if we ever split them.
- For symbol/label layers the same ascending order means lower byte = placed
  first = wins collisions and appears at lower zooms - the inversion is a
  styling fact to document, not a different mechanism.
- Direction is fixed (ascending), per-layer values are assigned in the
  shortbread layer definitions where the schema knowledge lives - we do not
  need tilemaker's per-layer ascending/descending switch because we control
  both ends.

**Things to avoid:**

- Tilemaker's lossy double-to-short compression in `setZOrder` - a wart from
  accepting arbitrary Lua doubles. Assign small integers directly in the
  layer definitions.
- Any size-motivated reordering (tippecanoe `--reorder`/`--coalesce`) that
  is not priority-aware; tippecanoe demoted these from defaults precisely
  because they scramble paint order.
- Emitting an order attribute instead of physically ordering - no competitor
  does it, and the renderer contract everyone relies on is physical order.

**Free wins to note in the spec:** once the priority byte is live, it is
also the natural survival key if we ever add per-tile feature limits or
grouped label limits (planetiler's group-limit path and tilemaker's
feature_limit both reuse the same ordering), and byte-stable merges move us
from "regress-canonical equal" toward byte-reproducible archives.
