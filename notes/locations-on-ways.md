# Elivagar: Supporting PBFs with Locations on Ways

## Context

The nidhogg production pipeline will add a `pbfhogg add-locations-to-ways` step that pre-resolves node coordinates into way elements. This eliminates elivagar's node store — the single largest memory consumer (~44 GB at planet scale) — making planet runs viable on 64 GB hosts.

Both paths must work:
- **Locations-on-ways PBF** (production path): no node store, coords from way elements
- **Standard PBF** (dev/compat path): current behavior, SortedNodeStore. OOM on planet is acceptable.

## What the node store does today

The SortedNodeStore exists for one purpose: map `node_id → (lat_e7, lon_e7)` during way processing. Ways in a standard PBF contain only node ref IDs; coordinates must be looked up.

Tagged nodes (POIs like shops, restaurants) already carry their own lat/lon in the PBF element — they never use the node store.

## What changes with locations-on-ways

### Skip entirely
- SortedNodeStore construction (12.4 GB for NA, ~44 GB for planet)
- `NodeStore::into_reader()` + `Arc<NodeStoreReader>` in worker thread
- Node ref → coord lookups in `process_raw_way()`

### Unchanged
- Tagged node processing (POIs) — nodes carry own coords
- WayIndex write + mmap read (relations still need way geometry)
- Sort, ocean, assemble phases
- All downstream: shortbread matching, MVT encoding, PMTiles output

### New way processing path
- `RawWay` carries `coords_e7: Vec<(i32, i32)>` directly instead of `node_refs: Vec<i64>`
- `process_raw_way()` skips the `NodeStoreReader` lookup loop
- Everything after coord resolution is identical

## Detection

Options (in order of preference):

1. **PBF header flag** — if pbfhogg's `add-locations-to-ways` sets a header feature flag (e.g. `LocationsOnWays`), elivagar can auto-detect. Cleanest, no user action.
2. **CLI flag** — `--locations-on-ways`. Simple, explicit, works regardless of header. Fallback if no header flag.
3. **Auto-detect from first way** — check if the first way element has lat/lon arrays. Fragile, not recommended.

Recommendation: support both 1 and 2. Auto-detect from header when available, CLI flag as override.

## Pipeline branching point

The branch happens early in `pipeline.rs` phase12, at the block dispatch level:

```
Standard PBF:
  Node blocks → build SortedNodeStore
  Way blocks  → extract node_refs → rayon: NodeStoreReader lookup → coords
  Rel blocks  → WayIndex lookup

Locations-on-ways PBF:
  Node blocks → tagged nodes only (POIs), no node store
  Way blocks  → extract coords directly from way element
  Rel blocks  → WayIndex lookup (same)
```

The worker thread currently receives `PrimitiveBlock`s, extracts `RawWay { way_id, node_refs, tags }`, and resolves coords via rayon + `NodeStoreReader`. With locations-on-ways, the extraction produces coords directly — no `NodeStoreReader` needed, no `Arc<NodeStoreReader>` in the worker, no node store at all.

## pbfhogg API (already landed)

The API is fully available:

```rust
// Existing way API
way.id() -> i64
way.refs() -> impl Iterator<Item = i64>
way.tags() -> impl Iterator<Item = (&str, &str)>

// Location accessors (line 215, 368-386 in pbfhogg)
way.node_locations() -> WayNodeLocationsIter<'a>
// WayNodeLocation has: nano_lat(), nano_lon(), lat(), lon(),
//                      decimicro_lat(), decimicro_lon()
```

The iterator delta-decodes `lat_data`/`lon_data` packed sint64 fields from the way's protobuf — exactly what `add-locations-to-ways` populates. On a standard PBF the iterator yields nothing; on an enriched PBF it yields one `WayNodeLocation` per node ref.

Usage in elivagar: `way.node_locations().zip(way.refs())` gives `(WayNodeLocation, i64)` pairs — coordinates and node IDs together, zero lookup.

Detection: if `way.node_locations().next().is_some()` on the first way encountered, the PBF has locations. No header flag needed.

## Memory impact

| | Standard PBF (planet) | Locations-on-ways (planet) |
|--|----------------------|---------------------------|
| Node store | ~44 GB | 0 |
| WayIndex (mmap'd) | ~6 GB page cache | ~6 GB page cache |
| Sort + working set | ~2-3 GB | ~2-3 GB |
| **Peak RSS** | **~65-75 GB (OOM on 64 GB)** | **~15-20 GB** |

## Preprocessing

```
brokkr run add-locations-to-ways planet.osm.pbf -o planet-with-locs.osm.pbf
```

On an indexdata PBF (from `pbfhogg cat`), the passthrough optimization skips decode/encode for ~80% of blobs — only node blobs need rewriting.

## Implementation order

1. ~~Wait for pbfhogg way API~~ — already landed (`Way::node_locations()`)
2. Wait for `pbfhogg add-locations-to-ways` command to land
3. Add detection (first way probe + CLI flag `--locations-on-ways`)
4. Add coord extraction from way elements in worker thread
5. Skip node store construction when locations detected
6. Validate: Denmark with locations-on-ways PBF, compare output byte-for-byte against standard path
7. Benchmark NA, then planet
