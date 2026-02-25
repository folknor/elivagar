# Changelog

## 0.1.0

Initial release.

- Full Shortbread schema: all 26 layers, z0-14
- Single-pass PBF reading with parallel feature processing
- Ocean polygon support via ESRI shapefiles (full + simplified for low zoom)
- External merge sort by Hilbert tile ID
- PMTiles v3 output with content deduplication and clustered layout
- Configurable gzip compression level (`--compression-level`, default 6)
- Douglas-Peucker simplification with sub-pixel skip optimization
- Same-attribute geometry merging within tiles
- Linux I/O hints for planet-scale runs (madvise, fadvise)
- `--skip-to` checkpointing for ocean and sort phases
- `--in-memory` mode for small extracts
