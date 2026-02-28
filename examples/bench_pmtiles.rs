#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

//! Benchmark: elivagar PmtilesWriter vs pmtiles-rs PmTilesStreamWriter
//!
//! Generates synthetic gzipped tiles across zoom levels 0–14, feeds identical
//! data to both writers, and compares wall-clock throughput.
//!
//! Run:  cargo run --release --example bench_pmtiles -- [--tiles N] [--runs R]
//! Or:   dev bench pmtiles [--tiles N] [--runs N]

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::Instant;

use elivagar::pmtiles_writer::{PmtilesConfig, PmtilesWriter, xy_to_tile_id};

struct TestTile {
    z: u8,
    x: u32,
    y: u32,
    data: Vec<u8>, // pre-gzipped
}

fn main() {
    let (num_tiles, runs) = parse_args();

    eprintln!("=== PMTiles Writer Benchmark ===");
    eprintln!("  Target tiles: {num_tiles}");
    eprintln!("  Runs: {runs} (best of)");
    eprintln!();

    eprint!("  Generating tiles... ");
    let gen_start = Instant::now();
    let tiles = generate_tiles(num_tiles);
    let data_bytes: usize = tiles.iter().map(|t| t.data.len()).sum();
    eprintln!(
        "done in {:.1?} ({} tiles, {:.1} MB compressed)",
        gen_start.elapsed(),
        tiles.len(),
        data_bytes as f64 / 1_000_000.0
    );
    eprintln!();

    // Ensure output dir exists
    fs::create_dir_all("data").expect("create data dir");

    // Benchmark elivagar writer
    eprint!("  elivagar:   ");
    let (eli_ms, eli_size) = bench_elivagar(&tiles, runs);
    eprintln!("{eli_ms} ms ({:.1} MB)", eli_size as f64 / 1_000_000.0);

    // Benchmark pmtiles-rs writer
    eprint!("  pmtiles-rs: ");
    let (pm_ms, pm_size) = bench_pmtiles_rs(&tiles, runs);
    eprintln!("{pm_ms} ms ({:.1} MB)", pm_size as f64 / 1_000_000.0);

    eprintln!();
    eprintln!("=== Summary (best of {runs}) ===");
    eprintln!(
        "  {:14} {:>8} {:>10} {:>12}",
        "", "ms", "output_MB", "tiles/sec"
    );
    eprintln!(
        "  {:14} {:>8} {:>10.1} {:>12.0}",
        "elivagar",
        eli_ms,
        eli_size as f64 / 1_000_000.0,
        tiles.len() as f64 / (eli_ms as f64 / 1000.0)
    );
    eprintln!(
        "  {:14} {:>8} {:>10.1} {:>12.0}",
        "pmtiles-rs",
        pm_ms,
        pm_size as f64 / 1_000_000.0,
        tiles.len() as f64 / (pm_ms as f64 / 1000.0)
    );

    let ratio = pm_ms as f64 / eli_ms as f64;
    if ratio > 1.05 {
        eprintln!("  -> elivagar is {ratio:.2}x faster");
    } else if ratio < 0.95 {
        eprintln!("  -> pmtiles-rs is {:.2}x faster", 1.0 / ratio);
    } else {
        eprintln!("  -> roughly equal");
    }
}

fn parse_args() -> (usize, usize) {
    let args: Vec<String> = std::env::args().collect();
    let mut tiles = 500_000usize;
    let mut runs = 5usize;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--tiles" => {
                i += 1;
                tiles = args[i].parse().expect("invalid --tiles value");
            }
            "--runs" => {
                i += 1;
                runs = args[i].parse().expect("invalid --runs value");
            }
            _ => {}
        }
        i += 1;
    }
    (tiles, runs)
}

// ---------------------------------------------------------------------------
// Tile generation
// ---------------------------------------------------------------------------

/// Generate `target` tiles across zoom levels 0–14, sorted by Hilbert tile ID.
fn generate_tiles(target: usize) -> Vec<TestTile> {
    let mut coords: Vec<(u8, u32, u32)> = Vec::with_capacity(target);

    // Fill zoom levels starting from z=0
    'done: for z in 0..=14u8 {
        let n = 1u32 << z;
        for x in 0..n {
            for y in 0..n {
                coords.push((z, x, y));
                if coords.len() >= target {
                    break 'done;
                }
            }
        }
    }

    // Sort by Hilbert tile ID (required by both writers for clustered output)
    coords.sort_by_key(|&(z, x, y)| xy_to_tile_id(z, x, y));

    // Generate compressed tile data
    coords
        .into_iter()
        .map(|(z, x, y)| TestTile {
            z,
            x,
            y,
            data: make_tile_data(z, x, y),
        })
        .collect()
}

/// Generate deterministic gzip-compressed pseudo-random tile data.
fn make_tile_data(z: u8, x: u32, y: u32) -> Vec<u8> {
    let mut hasher = DefaultHasher::new();
    (z, x, y).hash(&mut hasher);
    let seed = hasher.finish();

    // Higher zoom = larger tiles (more detail), 200–3000 bytes uncompressed
    let size = 200 + (z as usize) * 150 + ((seed as usize) % 500);

    let mut raw = Vec::with_capacity(size);
    let mut state = seed;
    for _ in 0..size {
        raw.push((state & 0xFF) as u8);
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
    }

    let mut compressor =
        libdeflater::Compressor::new(libdeflater::CompressionLvl::new(1).unwrap());
    let bound = compressor.gzip_compress_bound(raw.len());
    let mut out = vec![0u8; bound];
    let n = compressor.gzip_compress(&raw, &mut out).unwrap();
    out.truncate(n);
    out
}

// ---------------------------------------------------------------------------
// Elivagar benchmark
// ---------------------------------------------------------------------------

fn bench_elivagar(tiles: &[TestTile], runs: usize) -> (u128, u64) {
    let out_path = Path::new("data/bench_elivagar.pmtiles");
    let mut best_ms = u128::MAX;
    let mut file_size = 0u64;

    for _ in 0..runs {
        let start = Instant::now();

        let config = PmtilesConfig {
            min_zoom: 0,
            max_zoom: 14,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 0),
        };
        let mut writer = PmtilesWriter::new(config);
        for tile in tiles {
            writer
                .add_tile(tile.z, tile.x, tile.y, &tile.data)
                .expect("elivagar add_tile");
        }
        writer.write_to(out_path).expect("elivagar write_to");

        let ms = start.elapsed().as_millis();
        if ms < best_ms {
            best_ms = ms;
            file_size = fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
        }
    }

    (best_ms, file_size)
}

// ---------------------------------------------------------------------------
// pmtiles-rs benchmark
// ---------------------------------------------------------------------------

fn bench_pmtiles_rs(tiles: &[TestTile], runs: usize) -> (u128, u64) {
    let out_path = Path::new("data/bench_pmtiles_rs.pmtiles");
    let mut best_ms = u128::MAX;
    let mut file_size = 0u64;

    for _ in 0..runs {
        let start = Instant::now();

        let file = File::create(out_path).expect("create output");
        let mut writer = pmtiles::PmTilesWriter::new(pmtiles::TileType::Mvt)
            .tile_compression(pmtiles::Compression::Gzip)
            .internal_compression(pmtiles::Compression::Gzip)
            .min_zoom(0)
            .max_zoom(14)
            .bounds(-180.0, -85.0, 180.0, 85.0)
            .create(file)
            .expect("create pmtiles-rs writer");

        for tile in tiles {
            let coord =
                pmtiles::TileCoord::new(tile.z, tile.x, tile.y)
                    .expect("tile coord");
            writer
                .add_raw_tile(coord, &tile.data)
                .expect("pmtiles-rs add_raw_tile");
        }
        writer.finalize().expect("pmtiles-rs finalize");

        let ms = start.elapsed().as_millis();
        if ms < best_ms {
            best_ms = ms;
            file_size = fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
        }
    }

    (best_ms, file_size)
}
