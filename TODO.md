# elivagar TODO

## Release prep

- [ ] Run clippy and fix all warnings
- [ ] Extend test suite — cover geometry, MVT encoding, sort, PMTiles writer, multipolygon assembly
- [ ] Add Cargo.toml metadata for crates.io (`description`, `repository`, `keywords`, `categories`, `readme`)
- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## GitHub

- [ ] Write GitHub repo description and tags (vector-tiles, openstreetmap, pmtiles, shortbread, rust)
- [ ] Add GitHub Actions CI — clippy, tests, `cargo build --release` on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Quality

- [ ] Feature merging — combine adjacent linestrings/polygons with identical attributes to reduce tile size
- [ ] Visual verification — serve tiles and compare against Planetiler in OpenLayers
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server
