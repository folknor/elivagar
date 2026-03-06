// hotpath-alloc provides its own #[global_allocator] for allocation tracking,
// so mimalloc must be disabled when that feature is active.
#[cfg(not(feature = "hotpath-alloc"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};

/// Shortbread vector tile generator.
#[derive(Parser)]
#[command(name = "elivagar")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate PMTiles from an OSM PBF file.
    Run(Box<RunArgs>),
    /// Inspect a PMTiles archive.
    Inspect(InspectArgs),
    /// Verify a PMTiles archive.
    Verify(VerifyArgs),
}

/// Arguments for the `run` subcommand.
#[derive(Parser)]
struct RunArgs {
    /// Input OSM PBF file.
    input: PathBuf,

    /// Output PMTiles path.
    #[arg(short, long)]
    output: PathBuf,

    /// Temporary directory for sort chunks.
    #[arg(long, default_value = "data/tilegen_tmp")]
    tmp_dir: PathBuf,

    /// Ocean polygon shapefile (water-polygons-split-3857).
    #[arg(long)]
    ocean: Option<PathBuf>,

    /// Simplified ocean shapefile for z0-7.
    #[arg(long)]
    ocean_simplified: Option<PathBuf>,

    /// Resume from a checkpoint.
    #[arg(long)]
    skip_to: Option<SkipToArg>,

    /// Keep tile blob in RAM (faster for small extracts).
    #[arg(long)]
    in_memory: bool,

    /// Gzip compression level (0-10).
    #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(0..=10))]
    compression_level: u32,

    /// Force compact node store even without PBF header flag.
    #[arg(long)]
    force_sorted: bool,

    /// Bypass flat-index safety guardrails (unsafe; may cause severe IO/RSS degradation).
    /// Equivalent env var: ELIVAGAR_ALLOW_UNSAFE_FLAT_INDEX=1
    #[arg(long)]
    allow_unsafe_flat_index: bool,

    /// Thread count.
    #[arg(short = 'j', long = "threads")]
    threads: Option<usize>,

    /// Sort chunk memory budget (e.g. 256M, 1G). Minimum 64M.
    #[arg(long, value_parser = parse_byte_size_min_64m)]
    sort_budget: Option<usize>,

    /// In-flight way processing budget (e.g. 128M or 256M). Minimum 1M.
    /// Default when omitted: 128M (standard) or 256M (`--locations-on-ways`).
    #[arg(long, value_parser = parse_byte_size_min_1m)]
    way_budget: Option<usize>,

    /// Relation batch accumulation budget (e.g. 64M). Minimum 1M.
    #[arg(long, value_parser = parse_byte_size_min_1m)]
    rel_budget: Option<usize>,

    /// Tile assembly batch budget (e.g. 32M). Minimum 1M.
    #[arg(long, value_parser = parse_byte_size_min_1m)]
    assemble_budget: Option<usize>,

    /// PBF has node coordinates embedded in ways.
    #[arg(long)]
    locations_on_ways: bool,

    /// Disable ocean shapefile processing (skip auto-detection).
    #[arg(long)]
    no_ocean: bool,

    /// Tile payload format.
    #[arg(long, value_enum, default_value_t = TileFormatArg::Mvt)]
    tile_format: TileFormatArg,

    /// Tile compression algorithm (MVT only).
    #[arg(long, value_enum, default_value_t = TileCompressionArg::Gzip)]
    tile_compression: TileCompressionArg,

    /// Compress sort chunk files (reduces disk I/O, costs CPU).
    /// Useful at planet scale where sort data exceeds available RAM.
    #[arg(long, value_enum)]
    compress_sort_chunks: Option<SortChunkCompressionArg>,

    /// Polygon layers to apply shared-edge seam reconciliation at low zoom.
    /// Comma-separated layer names. Default: boundaries.
    #[arg(long, value_delimiter = ',', default_value = "boundaries")]
    seam_reconcile_layers: Vec<String>,
}

/// Arguments for the `inspect` subcommand.
#[derive(Parser)]
struct InspectArgs {
    /// PMTiles file to inspect.
    file: PathBuf,
}

/// Arguments for the `verify` subcommand.
#[derive(Parser)]
struct VerifyArgs {
    /// PMTiles file to verify.
    file: PathBuf,
}

#[derive(Clone, ValueEnum)]
enum SkipToArg {
    Ocean,
    Sort,
    Assemble,
}

#[derive(Clone, Copy, ValueEnum)]
enum TileFormatArg {
    Mvt,
    Mlt,
}

#[derive(Clone, Copy, ValueEnum)]
enum TileCompressionArg {
    Gzip,
    Brotli,
}

#[derive(Clone, Copy, ValueEnum)]
enum SortChunkCompressionArg {
    Lz4,
    Snappy,
}

/// Parse a byte size string like "256M", "1G", or raw bytes.
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

fn parse_byte_size_min_64m(s: &str) -> Result<usize, String> {
    let min = 64 * 1024 * 1024;
    match parse_byte_size(s) {
        Some(v) if v >= min => Ok(v),
        _ => Err(format!("expected >= 64M (e.g. 256M, 1G), got '{s}'")),
    }
}

fn parse_byte_size_min_1m(s: &str) -> Result<usize, String> {
    let min = 1024 * 1024;
    match parse_byte_size(s) {
        Some(v) if v >= min => Ok(v),
        _ => Err(format!("expected >= 1M (e.g. 32M, 128M), got '{s}'")),
    }
}

/// Detect ocean shapefiles in the given data directory.
///
/// Returns (full-resolution, simplified) paths if they exist on disk.
fn detect_ocean(data_dir: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let full = data_dir
        .join("water-polygons-split-3857")
        .join("water_polygons.shp");
    let simplified = data_dir
        .join("simplified-water-polygons-split-3857")
        .join("simplified_water_polygons.shp");
    (
        full.exists().then_some(full),
        simplified.exists().then_some(simplified),
    )
}

fn main() {
    // Disable hotpath metrics server by default — elivagar is a sync binary
    // with no async runtime, so the metrics server is never useful.
    // Override with HOTPATH_METRICS_SERVER_OFF=false if needed.
    if std::env::var_os("HOTPATH_METRICS_SERVER_OFF").is_none() {
        // SAFETY: called before any threads are spawned.
        unsafe { std::env::set_var("HOTPATH_METRICS_SERVER_OFF", "true") };
    }

    let cli = Cli::parse();

    match cli.command {
        Command::Run(args) => run(*args),
        Command::Inspect(args) => {
            if let Err(e) = elivagar::inspect::inspect(&args.file) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Command::Verify(args) => {
            match elivagar::verify::verify(&args.file) {
                Ok(report) => {
                    report.print_summary();
                    if !report.passed {
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

fn run(args: RunArgs) {
    let threads = args.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(4)
    });

    // Configure rayon's global thread pool before any rayon work.
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .expect("failed to configure rayon thread pool");

    let skip_to = args.skip_to.map(|s| match s {
        SkipToArg::Ocean => elivagar::SkipTo::Ocean,
        SkipToArg::Sort => elivagar::SkipTo::Sort,
        SkipToArg::Assemble => elivagar::SkipTo::Assemble,
    });

    // Resolve ocean shapefiles: explicit flags take priority, then auto-detect
    // from data/ relative to cwd, unless --no-ocean suppresses it entirely.
    let (ocean, ocean_simplified) = if args.no_ocean {
        (None, None)
    } else {
        let auto = detect_ocean(Path::new("data"));
        (args.ocean.or(auto.0), args.ocean_simplified.or(auto.1))
    };

    let allow_unsafe_flat_index = args.allow_unsafe_flat_index || env_var_true("ELIVAGAR_ALLOW_UNSAFE_FLAT_INDEX");

    let config = elivagar::TilegenConfig {
        pbf_path: args.input,
        output_path: args.output,
        tmp_dir: args.tmp_dir,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: ocean,
        ocean_simplified_shapefile: ocean_simplified,
        skip_to,
        in_memory: args.in_memory,
        compression_level: args.compression_level,
        force_sorted: args.force_sorted,
        allow_unsafe_flat_index,
        threads,
        way_inflight_budget: args.way_budget.unwrap_or(0),
        rel_batch_budget: args.rel_budget.unwrap_or(0),
        assemble_batch_budget: args.assemble_budget.unwrap_or(0),
        sort_chunk_size: args.sort_budget.unwrap_or(0),
        locations_on_ways: args.locations_on_ways,
        tile_format: match args.tile_format {
            TileFormatArg::Mvt => elivagar::TilePayloadFormat::Mvt,
            TileFormatArg::Mlt => elivagar::TilePayloadFormat::Mlt,
        },
        tile_compression: match args.tile_compression {
            TileCompressionArg::Gzip => elivagar::TileCompression::Gzip,
            TileCompressionArg::Brotli => elivagar::TileCompression::Brotli,
        },
        compress_sort_chunks: match args.compress_sort_chunks {
            None => elivagar::sort::ChunkCompression::None,
            Some(SortChunkCompressionArg::Lz4) => elivagar::sort::ChunkCompression::Lz4,
            Some(SortChunkCompressionArg::Snappy) => elivagar::sort::ChunkCompression::Snappy,
        },
        seam_reconcile_layers: {
            let mut mask = [false; elivagar::shortbread::Layer::count()];
            for name in &args.seam_reconcile_layers {
                match elivagar::shortbread::Layer::from_name(name) {
                    Some(layer) => mask[layer as usize] = true,
                    None => {
                        eprintln!("Warning: unknown layer name for --seam-reconcile-layers: {name}");
                    }
                }
            }
            mask
        },
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

fn env_var_true(name: &str) -> bool {
    std::env::var_os(name)
        .map(|v| {
            let v = v.to_string_lossy();
            matches!(v.as_ref(), "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
        })
        .unwrap_or(false)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_byte_size_megabytes() {
        assert_eq!(parse_byte_size("256M"), Some(256 * 1024 * 1024));
        assert_eq!(parse_byte_size("512m"), Some(512 * 1024 * 1024));
        assert_eq!(parse_byte_size("1M"), Some(1024 * 1024));
    }

    #[test]
    fn test_parse_byte_size_gigabytes() {
        assert_eq!(parse_byte_size("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_byte_size("2g"), Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn test_parse_byte_size_raw_bytes() {
        assert_eq!(parse_byte_size("67108864"), Some(67108864));
        assert_eq!(parse_byte_size("0"), Some(0));
    }

    #[test]
    fn test_parse_byte_size_whitespace() {
        assert_eq!(parse_byte_size("  256M  "), Some(256 * 1024 * 1024));
        assert_eq!(parse_byte_size(" 1G"), Some(1024 * 1024 * 1024));
    }

    #[test]
    fn test_parse_byte_size_invalid() {
        assert_eq!(parse_byte_size("abc"), None);
        assert_eq!(parse_byte_size(""), None);
        assert_eq!(parse_byte_size("M"), None);
        assert_eq!(parse_byte_size("G"), None);
    }

    #[test]
    fn test_parse_byte_size_min_64m() {
        assert!(parse_byte_size_min_64m("64M").is_ok());
        assert!(parse_byte_size_min_64m("1G").is_ok());
        assert!(parse_byte_size_min_64m("32M").is_err());
        assert!(parse_byte_size_min_64m("abc").is_err());
    }

    #[test]
    fn test_parse_byte_size_min_1m() {
        assert!(parse_byte_size_min_1m("1M").is_ok());
        assert!(parse_byte_size_min_1m("128M").is_ok());
        assert!(parse_byte_size_min_1m("512").is_err()); // 512 bytes < 1M
        assert!(parse_byte_size_min_1m("abc").is_err());
    }
}
