#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

//! Benchmark: SortedNodeStore write + read performance.
//!
//! Builds a SortedNodeStore with synthetic nodes, converts to a reader,
//! and benchmarks way-like (clustered) and random lookup patterns.
//!
//! Run:  cargo run --release --features hotpath --example bench_node_store -- [--nodes N] [--runs R]
//! Or:   dev bench node-store [--nodes N] [--runs N]

use std::hint::black_box;
use std::time::Instant;

use elivagar::node_index::SortedNodeStore;

// Same allocator arbitration as main.rs: hotpath-alloc wins, then the
// mimalloc-alloc default feature; system allocator otherwise.
#[cfg(all(
    feature = "mimalloc-alloc",
    not(feature = "sys-alloc"),
    not(feature = "jemalloc-alloc"),
    not(feature = "hotpath-alloc")
))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(all(feature = "jemalloc-alloc", not(feature = "hotpath-alloc")))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    let _guard = hotpath::HotpathGuardBuilder::new("bench_node_store")
        .percentiles(&[50.0, 95.0, 99.0])
        .build();

    let config = parse_args();

    eprintln!("=== Node Store Benchmark ===");
    eprintln!("  Nodes:       {} M", config.nodes / 1_000_000);
    eprintln!("  Ways:        {} K", config.ways / 1_000);
    eprintln!("  Random hits: {} M", config.random_lookups / 1_000_000);
    eprintln!("  Runs:        {} (best of)", config.runs);
    eprintln!();

    let (node_ids, coords) = generate_and_print_nodes(config.nodes);
    let (build_ms, reader) = build_store(&node_ids, &coords, &config);

    if config.build_only {
        print_build_only_summary(&config, build_ms, node_ids.len());
        return;
    }

    let (way_lookups, total_way_lookups, random_lookups) = generate_lookups(&node_ids, &config);
    let best_way_ms = bench_way_lookups(&reader, &way_lookups, total_way_lookups, config.runs);
    let best_rand_ms = bench_random_lookups(&reader, &random_lookups, config.runs);

    print_summary(
        &config,
        build_ms,
        node_ids.len(),
        best_way_ms,
        total_way_lookups,
        best_rand_ms,
        random_lookups.len(),
    );
}

fn generate_and_print_nodes(count: usize) -> (Vec<i64>, Vec<(i32, i32)>) {
    eprint!("  Generating node IDs + coords... ");
    let gen_start = Instant::now();
    let (node_ids, coords) = generate_nodes(count);
    eprintln!(
        "done in {:.1?} ({} nodes, ID range {}..{})",
        gen_start.elapsed(),
        node_ids.len(),
        node_ids.first().unwrap(),
        node_ids.last().unwrap(),
    );
    (node_ids, coords)
}

fn build_store(
    node_ids: &[i64],
    coords: &[(i32, i32)],
    config: &BenchConfig,
) -> (u128, elivagar::node_index::SortedNodeStoreReader) {
    let build_runs = if config.build_only { config.runs } else { 1 };
    let mut best_build_ms = u128::MAX;
    let mut reader = None;
    for run in 0..build_runs {
        eprint!(
            "  Building SortedNodeStore (run {}/{build_runs})... ",
            run + 1
        );
        let build_start = Instant::now();
        let mut store = SortedNodeStore::new();
        for i in 0..node_ids.len() {
            store.put(node_ids[i], coords[i].0, coords[i].1);
        }
        let ms = build_start.elapsed().as_millis();
        eprintln!("done in {ms} ms");
        if ms < best_build_ms {
            best_build_ms = ms;
        }
        if run == build_runs - 1 {
            eprint!("  Converting to reader... ");
            let convert_start = Instant::now();
            reader = Some(store.into_reader());
            eprintln!("done in {} ms", convert_start.elapsed().as_millis());
        }
    }
    (best_build_ms, reader.expect("reader must be built"))
}

fn print_build_only_summary(config: &BenchConfig, build_ms: u128, num_nodes: usize) {
    eprintln!();
    eprintln!("=== Summary (build-only, best of {}) ===", config.runs);
    eprintln!(
        "  {:14} {:>8} {:>12} {:>10}",
        "", "ms", "nodes/sec", "ns/node"
    );
    eprintln!(
        "  {:14} {:>8} {:>12.0} {:>10.1}",
        "build",
        build_ms,
        num_nodes as f64 / (build_ms as f64 / 1000.0),
        build_ms as f64 * 1_000_000.0 / num_nodes as f64,
    );
}

#[allow(clippy::type_complexity)]
fn generate_lookups(node_ids: &[i64], config: &BenchConfig) -> (Vec<Vec<i64>>, usize, Vec<i64>) {
    eprint!("  Generating way-like lookups... ");
    let way_start = Instant::now();
    let way_lookups = generate_way_lookups(node_ids, config.ways);
    let total_way_lookups: usize = way_lookups.iter().map(Vec::len).sum();
    eprintln!(
        "done in {:.1?} ({} ways, {} lookups)",
        way_start.elapsed(),
        way_lookups.len(),
        total_way_lookups,
    );

    eprint!("  Generating random lookups... ");
    let rand_start = Instant::now();
    let random_lookups = generate_random_lookups(node_ids, config.random_lookups);
    eprintln!(
        "done in {:.1?} ({} lookups)",
        rand_start.elapsed(),
        random_lookups.len(),
    );
    eprintln!();

    (way_lookups, total_way_lookups, random_lookups)
}

fn bench_way_lookups(
    reader: &elivagar::node_index::SortedNodeStoreReader,
    way_lookups: &[Vec<i64>],
    total_way_lookups: usize,
    runs: usize,
) -> u128 {
    let mut best_ms = u128::MAX;
    for run in 0..runs {
        let start = Instant::now();
        let mut found = 0u64;
        for way in way_lookups {
            for &id in way {
                if black_box(reader.get(id)).is_some() {
                    found += 1;
                }
            }
        }
        let ms = start.elapsed().as_millis();
        if run == 0 {
            eprintln!("  Way-like:  {found}/{total_way_lookups} hits");
        }
        if ms < best_ms {
            best_ms = ms;
        }
    }
    best_ms
}

fn bench_random_lookups(
    reader: &elivagar::node_index::SortedNodeStoreReader,
    random_lookups: &[i64],
    runs: usize,
) -> u128 {
    let mut best_ms = u128::MAX;
    for run in 0..runs {
        let start = Instant::now();
        let mut found = 0u64;
        for &id in random_lookups {
            if black_box(reader.get(id)).is_some() {
                found += 1;
            }
        }
        let ms = start.elapsed().as_millis();
        if run == 0 {
            eprintln!("  Random:    {found}/{} hits", random_lookups.len());
        }
        if ms < best_ms {
            best_ms = ms;
        }
    }
    best_ms
}

fn print_summary(
    config: &BenchConfig,
    build_ms: u128,
    num_nodes: usize,
    best_way_ms: u128,
    total_way_lookups: usize,
    best_rand_ms: u128,
    num_random: usize,
) {
    eprintln!();
    eprintln!("=== Summary (best of {}) ===", config.runs);
    eprintln!(
        "  {:14} {:>8} {:>12} {:>10}",
        "", "ms", "lookups/sec", "ns/lookup"
    );
    eprintln!(
        "  {:14} {:>8} {:>12.0} {:>10.1}",
        "build",
        build_ms,
        num_nodes as f64 / (build_ms as f64 / 1000.0),
        build_ms as f64 * 1_000_000.0 / num_nodes as f64,
    );
    eprintln!(
        "  {:14} {:>8} {:>12.0} {:>10.1}",
        "way-like",
        best_way_ms,
        total_way_lookups as f64 / (best_way_ms as f64 / 1000.0),
        best_way_ms as f64 * 1_000_000.0 / total_way_lookups as f64,
    );
    eprintln!(
        "  {:14} {:>8} {:>12.0} {:>10.1}",
        "random",
        best_rand_ms,
        num_random as f64 / (best_rand_ms as f64 / 1000.0),
        best_rand_ms as f64 * 1_000_000.0 / num_random as f64,
    );
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

struct BenchConfig {
    nodes: usize,
    ways: usize,
    random_lookups: usize,
    runs: usize,
    build_only: bool,
}

fn parse_args() -> BenchConfig {
    let args: Vec<String> = std::env::args().collect();
    let mut nodes_m = 50usize;
    let mut ways_k = 4000usize;
    let mut random_m = 10usize;
    let mut runs = 5usize;
    let mut build_only = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--nodes" => {
                i += 1;
                nodes_m = args[i].parse().expect("invalid --nodes value");
            }
            "--ways" => {
                i += 1;
                ways_k = args[i].parse().expect("invalid --ways value");
            }
            "--random" => {
                i += 1;
                random_m = args[i].parse().expect("invalid --random value");
            }
            "--runs" => {
                i += 1;
                runs = args[i].parse().expect("invalid --runs value");
            }
            "--build-only" => {
                build_only = true;
            }
            _ => {}
        }
        i += 1;
    }
    BenchConfig {
        nodes: nodes_m * 1_000_000,
        ways: ways_k * 1_000,
        random_lookups: random_m * 1_000_000,
        runs,
        build_only,
    }
}

// ---------------------------------------------------------------------------
// Synthetic data generation (deterministic LCG PRNG)
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
}

/// Generate strictly ascending node IDs with realistic gap distribution.
/// 80% small gaps (1-50, same "edit band"), 20% large gaps (10K-1M, new band).
fn generate_nodes(count: usize) -> (Vec<i64>, Vec<(i32, i32)>) {
    let mut rng = Rng::new(12345);
    let mut ids = Vec::with_capacity(count);
    let mut coords = Vec::with_capacity(count);
    let mut current_id: i64 = 1_000_000;

    for _ in 0..count {
        ids.push(current_id);

        // Europe-ish coordinates: lat 40-60N, lon -10 to 30E (E7 values)
        let lat = 400_000_000 + (rng.next() % 200_000_000) as i32;
        let lon = -100_000_000 + (rng.next() % 400_000_000) as i32;
        coords.push((lat, lon));

        let r = rng.next();
        if r.is_multiple_of(5) {
            // Large gap: new edit band
            current_id += 10_000 + (rng.next() % 1_000_000).cast_signed();
        } else {
            // Small gap: within same band
            current_id += 1 + (rng.next() % 50).cast_signed();
        }
    }

    (ids, coords)
}

/// Generate way-like lookup patterns: groups of 5-30 consecutive node refs.
fn generate_way_lookups(node_ids: &[i64], num_ways: usize) -> Vec<Vec<i64>> {
    let mut rng = Rng::new(67890);
    let mut ways = Vec::with_capacity(num_ways);

    for _ in 0..num_ways {
        let anchor = (rng.next() as usize) % node_ids.len();
        let way_len = 5 + (rng.next() % 26) as usize;
        let end = (anchor + way_len).min(node_ids.len());
        ways.push(node_ids[anchor..end].to_vec());
    }

    ways
}

/// Generate scattered random lookups from existing node IDs.
fn generate_random_lookups(node_ids: &[i64], count: usize) -> Vec<i64> {
    let mut rng = Rng::new(11111);
    let mut lookups = Vec::with_capacity(count);

    for _ in 0..count {
        let idx = (rng.next() as usize) % node_ids.len();
        lookups.push(node_ids[idx]);
    }

    lookups
}
