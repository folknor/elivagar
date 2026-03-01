// hotpath-alloc provides its own #[global_allocator] for allocation tracking,
// so mimalloc must be disabled when that feature is active.
#[cfg(not(feature = "hotpath-alloc"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Parse a byte size string like "256M", "1G", or raw bytes "268435456".
fn parse_byte_size(s: &str) -> Option<usize> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('G').or_else(|| s.strip_suffix('g')) {
        n.trim().parse::<usize>().ok().map(|v| v * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        n.trim().parse::<usize>().ok().map(|v| v * 1024 * 1024)
    } else {
        s.parse::<usize>().ok()
    }
}

#[allow(clippy::too_many_lines)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: elivagar <pbf> <out.pmtiles> [--tmp-dir path] [--ocean path.shp] [--ocean-simplified path.shp] [--skip-to ocean|sort] [--in-memory] [--compression-level 0-10] [--force-sorted] [-j N | --threads N] [--sort-budget bytes]"
        );
        std::process::exit(1);
    }

    let pbf_path = std::path::PathBuf::from(&args[1]);
    let output_path = std::path::PathBuf::from(&args[2]);

    let mut tmp_dir = std::path::PathBuf::from("data/tilegen_tmp");
    let mut ocean_shapefile = None;
    let mut ocean_simplified_shapefile = None;
    let mut skip_to: Option<elivagar::SkipTo> = None;
    let mut in_memory = false;
    let mut compression_level: u32 = 6;
    let mut force_sorted = false;
    let mut threads: usize = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4);
    let mut sort_chunk_size: usize = 0;

    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--tmp-dir" | "--ocean" | "--ocean-simplified" | "--skip-to"
            | "--compression-level" | "-j" | "--threads" | "--sort-budget" => {
                let flag = &args[i];
                i += 1;
                if i >= args.len() {
                    eprintln!("{flag} requires a value");
                    std::process::exit(1);
                }
                match flag.as_str() {
                    "--tmp-dir" => tmp_dir = std::path::PathBuf::from(&args[i]),
                    "--ocean" => ocean_shapefile = Some(std::path::PathBuf::from(&args[i])),
                    "--ocean-simplified" => ocean_simplified_shapefile = Some(std::path::PathBuf::from(&args[i])),
                    "--skip-to" => {
                        skip_to = Some(match args[i].as_str() {
                            "ocean" => elivagar::SkipTo::Ocean,
                            "sort" => elivagar::SkipTo::Sort,
                            other => {
                                eprintln!("Unknown skip-to value: {other} (expected ocean or sort)");
                                std::process::exit(1);
                            }
                        });
                    }
                    "--compression-level" => {
                        compression_level = match args[i].parse() {
                            Ok(v) if v <= 10 => v,
                            _ => {
                                eprintln!("Invalid compression level: {} (expected 0-10)", args[i]);
                                std::process::exit(1);
                            }
                        };
                    }
                    "-j" | "--threads" => {
                        threads = match args[i].parse() {
                            Ok(v) if v >= 1 => v,
                            _ => {
                                eprintln!("Invalid thread count: {} (expected >= 1)", args[i]);
                                std::process::exit(1);
                            }
                        };
                    }
                    "--sort-budget" => {
                        sort_chunk_size = match parse_byte_size(&args[i]) {
                            Some(v) if v >= 64 * 1024 * 1024 => v,
                            _ => {
                                eprintln!("Invalid sort budget: {} (expected >= 64M, e.g. 256M, 1G)", args[i]);
                                std::process::exit(1);
                            }
                        };
                    }
                    _ => unreachable!(),
                }
            }
            "--in-memory" => {
                in_memory = true;
            }
            "--force-sorted" => {
                force_sorted = true;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    // Configure rayon's global thread pool before any rayon work.
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .expect("failed to configure rayon thread pool");

    let config = elivagar::TilegenConfig {
        pbf_path,
        output_path,
        tmp_dir,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile,
        ocean_simplified_shapefile,
        skip_to,
        in_memory,
        compression_level,
        force_sorted,
        threads,
        way_inflight_budget: 0,
        rel_batch_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size,
    };

    let _guard = hotpath::HotpathGuardBuilder::new("elivagar::main")
        .percentiles(&[50, 95, 99])
        .with_functions_limit(0)
        .build();

    if let Err(e) = elivagar::run(&config) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
