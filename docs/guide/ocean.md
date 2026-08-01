# Ocean Input

`--ocean` is repeatable and is the only ocean input. **Omit it entirely and
the archive has no ocean.** Each value is one of:

| Spec | Meaning |
|---|---|
| `z0-z14:<file.shp>` | one shapefile serves every zoom |
| `z0-z7:<file.shp>` | low-zoom shapefile, the pre-generalized one |
| `z8-z14:<file.shp>` | full-resolution shapefile |
| `<file.pmtiles>` | precomputed world-ocean artifact |

## The zoom split

Shapefile entries must partition z0-z14 exactly, and the only accepted
partitions are a single `z0-z14` or the `z0-z7` + `z8-z14` pair. That is not
arbitrary: the engine implements one split, at z7/z8, and nowhere else. A
`z0-z5` request is rejected rather than quietly served at z7, because the
alternative is a false statement in the recorded invocation.

The two shapefiles are the standard datasets from
[osmdata.openstreetmap.de](https://osmdata.openstreetmap.de/) - same
coastlines, same projection, differing in vertex density. At z0-z7 the whole
world is at most 256x128 pixels, so full-resolution coastline vertices are
overwhelmingly sub-pixel; the split exists to avoid paying for detail that
cannot be seen. It is an optimization, not a correctness requirement, and
`z0-z14:<full>` alone is a legal and slower way to say the same thing.

## The world artifact

Building ocean geometry for the whole world is the same work every time, so it
can be done once:

```sh
elivagar ocean-build \
  --ocean z0-z7:simplified_water_polygons.shp \
  --ocean z8-z14:water_polygons.shp \
  -o data/ocean-tiles.pmtiles
```

One shot per shapefile release, not per run. A run that names the artifact
then computes only the boundary band near its bbox edge and merges the
artifact for the interior. At world bounds the band is empty by construction,
so a planet build's ocean phase is free.

**The artifact is a cache over the shapefiles, not a substitute for them**,
and is rejected on its own. An extract still needs the shapefiles to compute
its band, and the artifact's key is validated by re-hashing the shapefiles it
claims to have been built from, so both sides must be present for the check to
mean anything.

A named artifact that cannot serve the run is an **error**. It applies only to
the exact MVT + gzip + z0-14 contract at a matching compression level, and a
mismatch on any of those fails the run rather than quietly computing the ocean
instead. This used to print a warning and carry on, which meant a measured run
could switch ocean paths leaving no trace in the results row, the metrics, or
the archive.

### Why the key has a version half

`OCEAN_POLICY_VERSION` is the code half of the artifact key: the shapefile
hashes catch input rotations, the version stands in for "the geometry pipeline
changed". Any change that alters emitted ocean geometry must bump it.

That exists because of a real incident. A simplifier change shipped while an
artifact rebuild was deferred, and the version did not move, so a three-day-old
artifact kept validating and served pre-change coastline spikes worldwide.
Every standing gate stayed green - the tessellation oracle cannot see a spike,
and the archives being compared all shared the same stale artifact. It was
found by a human looking at a tile. Now a stale artifact fails loud at the key
check.

## Low-zoom union

The z0-z7 pass unions its source pieces before descending the tile pyramid.
The osmdata shapefiles arrive pre-split into grid cells comparable to low-zoom
tile sizes, and descending each cell independently simplified shared
coastlines into disagreeing shapes - seam wedges on every cell edge - while
applying the per-zoom minimum-area drop per cell fragment, which deleted the
smaller half of any landform straddling a cell boundary.

The z8-z14 pass keeps per-piece descent: its seam mismatch is sub-pixel at
fixed tolerances, and a world-scale union of the full-resolution polygons
would dominate the artifact build.

## Ring caps

No emitted polygon exceeds 500 contours. MapLibre's `classifyRings` applies a
500-ring clamp to each classified polygon and **silently drops all but the 500
largest rings** past it, so three many-holed ocean polygons were losing their
smallest islands in MapLibre and nowhere else - invisible to every consumer
and to the tessellation oracle, which only validates rings that survived.

Emission now bisects such a shape's clip rect until every piece fits, with the
halves sharing their integer cut coordinate so the pieces abut exactly under
nonzero fill.
