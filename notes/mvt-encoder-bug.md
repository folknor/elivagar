# MVT Encoder Bug: Malformed geometry in 3 ocean tiles

## Summary

3 out of 32,667 tiles in a Denmark PMTiles archive have malformed MVT geometry
command streams. All 3 are ocean layer features with very large polygon rings
(30K+ vertices). The malformed tiles are detected by both our `elivagar verify`
tool and Mapbox's `@mapbox/vtvalidate` (backed by vtzero C++ library).

## Affected tiles

| Tile | Error | Details |
|---|---|---|
| z8/143/73 | LineTo count too large (33123 > 32776) | Feature 3, single ring with 32,766 decoded verts. LineTo command claims 33,123 points but the packed varint field only has room for 32,776. |
| z8/135/75 | too few points in geometry | Feature 2, single ring with 30,996 decoded verts. Command stream runs past end while reading point parameters. |
| z9/287/147 | too few points in geometry | Feature 0, similar large ring. |

## The puzzle

Our `diag` tool (which uses protohoggr's protobuf decoder) successfully decodes
these features and reports correct-looking vertex counts and ring geometry. But
our `verify` tool (which independently decodes packed varints from the raw
geometry bytes) finds the command stream is malformed.

For z8/143/73: diag reports ring 0 has 32,766 vertices. For a single closed ring,
the MVT command stream should be `MoveTo(1) [2 params] LineTo(32764) [65528
params] ClosePath(1)`. But verify's varint decoder reads the LineTo command
integer as having count 33,123 - 359 more than expected, and more than the
remaining byte data can supply.

This discrepancy between the two decoders suggests either:
1. A bug in the protobuf byte encoding (the u32 command values are correct but
   the varint byte serialization is wrong for large arrays)
2. A bug in the length-delimited field framing (the geometry field's byte-length
   prefix is wrong, causing the decoder to read into adjacent data)
3. A bug in our verify tool's independent varint decoder (less likely - it's a
   standard LEB128 implementation and produces correct results for the other
   32,664 tiles)

## Encoding chain

The geometry goes through these steps:

1. `encode_polygon` in `src/mvt/mod.rs:437` - produces `Vec<u32>` command stream.
   Straightforward: MoveTo + zigzag params, LineTo + zigzag params, ClosePath.
   The u32 command integer: `id | (count << 3)`. For LineTo with count 32764:
   `2 | (32764 << 3)` = `0x3FF82`.

2. The u32 array is stored in `Feature.geometry: Vec<u32>`.

3. `encode_packed_uint32` in protohoggr (`lib.rs:839`) - encodes each u32 as a
   varint into a scratch buffer, then wraps as a length-delimited protobuf field.
   The packed field byte format is: `tag varint | length varint | v0 varint | v1
   varint | ...`. For 65,535 u32s averaging ~2.5 bytes each, the packed field is
   ~160KB.

4. The feature protobuf (containing the packed geometry) is itself written as a
   length-delimited field inside the layer protobuf.

## Reproducer

```bash
# Generate the tileset
elivagar run denmark.osm.pbf -o output.pmtiles --locations-on-ways

# Verify (finds the 3 broken tiles)
elivagar verify output.pmtiles

# Inspect the broken tile (decodes successfully despite broken encoding)
elivagar diag output.pmtiles -z 8 -x 143 -y 73

# vtvalidate confirmation (Node.js)
# vtvalidate.isValid(tile_buffer) returns "count too large" for z8/143/73
```

## What to investigate

1. **Varint round-trip test**: Encode a 33K-element u32 array with
   `encode_packed_uint32`, then decode with the verify decoder. Compare counts.
   If they differ, the bug is in encode or decode. If they match, the bug is in
   the protobuf framing (nested length-delimited fields).

2. **Hex dump the boundary**: Dump the raw bytes around the LineTo command in the
   packed geometry field. Check if the varint encoding of the LineTo command
   integer (`0x3FF82` = 262018 decimal, which is a 3-byte varint) is correct.

3. **Check if the length prefix is off**: The packed geometry field has a
   length-varint prefix. If this length is wrong (too short), the decoder would
   stop mid-stream and the next read would misinterpret non-geometry bytes as
   geometry commands.

4. **Test with smaller rings**: Does the bug occur for rings with ~16K vertices?
   ~8K? Finding the threshold would point to a varint size boundary issue.

## Context

- All 3 affected features are ocean polygons from the `water-polygons-split-3857`
  shapefile, processed through `emit_boundary_tile` in `src/ocean.rs`.
- Ocean features are excluded from the `merge_same_attr_geometries` pass, so the
  geometry is directly from the encoder, not post-processed.
- The vast majority of tiles (32,664 out of 32,667) encode correctly.
- The bug only manifests for very large polygon rings (30K+ vertices), which only
  occur in the ocean layer at z8-z9 where detailed coastlines produce dense rings.
