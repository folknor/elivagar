// The global allocator is the system allocator, on purpose. mimalloc
// predated all measurement here and lost the measured three-way A/B on
// 2026-07-15 at commit 98824b4, after the phase12 scratch-retention fix
// removed the fault-storm confound: glibc beat it on wall on both A/B
// datasets (NA locations 269s vs 282s, germany 60s vs 65s; phase12
// 155.0s vs 170.8s and 31.5s vs 39.3s) with a fraction of the minor
// faults, and mimalloc froze 2-3x the live bytes as never-returned
// commitment (14.2 GB against 4.2 GB anon on germany), address space a
// 30 GB planet-RAM ledger cannot trust. jemalloc measured at par with
// glibc, so it does not pay for its dependency either. pbfhogg reached
// its records on the system allocator the same way.
//
// hotpath 0.14 installed CountingAllocator internally under hotpath-alloc; as
// of 0.20 it is a plain generic type the consumer must declare - the crate no
// longer wires it up on its own. Without this, hotpath-alloc builds fall back
// to the system allocator and track_alloc/track_dealloc never fire, so every
// function silently reports 0 bytes.
#[cfg(feature = "hotpath-alloc")]
#[global_allocator]
static ALLOC: hotpath::CountingAllocator = hotpath::CountingAllocator::new();

mod signals;

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use elivagar::resources::ByteBudget;

/// Shortbread vector tile generator.
#[derive(Parser)]
#[command(name = "elivagar", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate PMTiles from an OSM PBF file.
    Run(Box<RunArgs>),
    /// Build the durable world-ocean PMTiles artifact.
    OceanBuild(OceanBuildArgs),
    /// Inspect a PMTiles archive.
    Inspect(InspectArgs),
    /// Verify a PMTiles archive.
    Verify(VerifyArgs),
    /// Render a single tile as SVG.
    Svg(SvgArgs),
    /// Diagnose ocean ring winding for a specific tile.
    Diag(DiagArgs),
}

/// Arguments for the `diag` subcommand.
#[derive(Parser)]
struct DiagArgs {
    /// PMTiles file to read.
    file: PathBuf,
    /// Zoom level.
    #[arg(short, long, value_parser = parse_tile_zoom)]
    z: u8,
    /// Tile X coordinate.
    #[arg(short, long)]
    x: u32,
    /// Tile Y coordinate.
    #[arg(short, long)]
    y: u32,
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

    /// Ocean input. Repeatable; omit entirely for no ocean.
    ///
    ///   --ocean z0-z14:water_polygons.shp        one shapefile, all zooms
    ///   --ocean z0-z7:simplified_water_polygons.shp
    ///   --ocean z8-z14:water_polygons.shp        the two-pass split
    ///   --ocean ocean-tiles.pmtiles              precomputed artifact
    ///
    /// Shapefile entries must partition z0-z14 exactly. The engine implements
    /// one split, at z7/z8, so the only accepted partitions are a single
    /// z0-z14 entry or the z0-z7 + z8-z14 pair; anything else is rejected
    /// rather than silently rounded to the split it can actually do.
    ///
    /// A .pmtiles entry is a cache over the shapefile entries, not a
    /// substitute for them: an extract still computes its boundary band from
    /// the shapefiles, and validating the artifact's key means re-hashing the
    /// source it claims to be built from. So it is additive, and rejected on
    /// its own.
    #[arg(long, value_name = "SPEC")]
    ocean: Vec<String>,

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
    #[arg(long)]
    allow_unsafe_flat_index: bool,

    /// Thread count, at least 1. Default: logical CPUs.
    #[arg(short = 'j', long = "threads", value_parser = parse_threads)]
    threads: Option<std::num::NonZeroUsize>,

    /// Sort chunk memory budget (e.g. 256M, 1G). Minimum 64M.
    #[arg(long, value_parser = parse_byte_size_min_64m)]
    sort_budget: Option<ByteBudget>,

    /// In-flight way processing budget in raw block+plan bytes. Minimum 1M.
    /// Safety net only; the count ceiling (max(threads, 8)) is the primary control.
    /// Default when omitted: 128M (standard) or 4G (`--locations-on-ways`).
    #[arg(long, value_parser = parse_byte_size_min_1m)]
    way_budget: Option<ByteBudget>,

    /// Tile assembly batch budget (e.g. 32M). Minimum 1M.
    #[arg(long, value_parser = parse_byte_size_min_1m)]
    assemble_budget: Option<ByteBudget>,

    /// Entry cap for the PMTiles payload dedup map. Default 1000000 when omitted.
    /// Explicit zero is rejected; omit this flag to use the default.
    /// Tile content is identical under any cap; past it duplicate payloads
    /// are stored instead of referenced (archive bytes), and the map costs
    /// ~56 bytes per entry of RAM.
    #[arg(long, value_parser = parse_dedup_cap)]
    dedup_cap: Option<std::num::NonZeroUsize>,

    /// Assert the PBF header declares embedded way coordinates.
    /// Header detection is automatic.
    #[arg(long)]
    locations_on_ways: bool,

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

    /// Default fanout cap for supported OSM polygon layers.
    /// 0 or omitted = uncapped.
    /// Per-layer overrides via --fanout-cap take precedence.
    #[arg(long)]
    fanout_cap_default: Option<u32>,

    /// Per-layer fanout caps for OSM polygons: conservative bbox tile count
    /// including the 128-unit buffer on each tile edge.
    /// Comma-separated, format: layer=N. Features exceeding the cap are skipped
    /// at that zoom. Example: water_polygons=2048,buildings=4096
    #[arg(long, value_delimiter = ',')]
    fanout_cap: Vec<String>,

    /// Simplification tolerance multiplier for polygon layers.
    /// 1.0 = same as lines (default). Values > 1.0 simplify polygons more
    /// aggressively, reducing sort record volume. Polygon fills are less
    /// sensitive to vertex precision than stroked lines.
    #[arg(long, default_value_t = 1.0)]
    polygon_simplify_factor: f64,
}

#[derive(Parser)]
struct OceanBuildArgs {
    /// Ocean shapefile input, repeatable, same spelling as `run --ocean`:
    /// either a single `z0-z14:<shp>` or the `z0-z7:<shp>` + `z8-z14:<shp>`
    /// split. A `.pmtiles` entry is rejected here - this is the command that
    /// builds one.
    ///
    /// The zoom split is part of the artifact's identity, so it has to be
    /// stated the same way on both sides: an artifact built here is only valid
    /// for a `run` whose --ocean names the same shapefiles.
    #[arg(long, value_name = "SPEC", required = true)]
    ocean: Vec<String>,
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long, default_value = "data/ocean-build_tmp")]
    tmp_dir: PathBuf,
    #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(0..=10))]
    compression_level: u32,
    #[arg(short = 'j', long, value_parser = parse_threads)]
    threads: Option<std::num::NonZeroUsize>,
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

    /// Collect and print per-zoom ocean geometry statistics (ring vertex
    /// counts, consecutive duplicates, full-tile rectangle features).
    #[arg(long)]
    geometry_stats: bool,

    /// Validate each distinct compressed payload once while retaining
    /// addressed-tile accounting. Intended for very large run-heavy archives.
    #[arg(long)]
    unique_payloads: bool,
}

/// Arguments for the `svg` subcommand.
#[derive(Parser)]
struct SvgArgs {
    /// PMTiles file to read.
    file: PathBuf,

    /// Zoom level.
    #[arg(short, long, value_parser = parse_tile_zoom)]
    z: u8,

    /// Tile X coordinate (top-left).
    #[arg(short, long)]
    x: u32,

    /// Tile Y coordinate (top-left).
    #[arg(short, long)]
    y: u32,

    /// Grid width in tiles (default: 1).
    #[arg(short = 'W', long, default_value = "1")]
    width: u32,

    /// Grid height in tiles (default: 1).
    #[arg(short = 'H', long, default_value = "1")]
    height: u32,

    /// Only render these layers (comma-separated, e.g. "ocean,boundaries").
    #[arg(short, long)]
    layers: Option<String>,

    /// Output SVG path (default: stdout).
    #[arg(short, long)]
    output: Option<PathBuf>,
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
        n.trim()
            .parse::<usize>()
            .ok()
            .and_then(|v| v.checked_mul(1024 * 1024 * 1024))
    } else if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        n.trim()
            .parse::<usize>()
            .ok()
            .and_then(|v| v.checked_mul(1024 * 1024))
    } else {
        s.parse::<usize>().ok()
    }
}

fn parse_dedup_cap(s: &str) -> Result<std::num::NonZeroUsize, String> {
    let value = s
        .parse::<usize>()
        .map_err(|_| format!("invalid dedup cap '{s}', expected a positive integer"))?;
    std::num::NonZeroUsize::new(value).ok_or_else(|| {
        "dedup cap must be greater than zero; omit the flag to use the default".into()
    })
}

fn parse_threads(s: &str) -> Result<std::num::NonZeroUsize, String> {
    s.parse::<std::num::NonZeroUsize>()
        .map_err(|_| format!("invalid thread count '{s}', expected a positive integer"))
}

fn parse_tile_zoom(s: &str) -> Result<u8, String> {
    let z = s
        .parse::<u8>()
        .map_err(|_| format!("invalid tile zoom '{s}', expected 0 through 31"))?;
    if z > elivagar::tiling::ARCHIVE_MAX_ZOOM {
        return Err(format!("invalid tile zoom '{s}', expected 0 through 31"));
    }
    Ok(z)
}

fn parse_byte_size_min_64m(s: &str) -> Result<ByteBudget, String> {
    match parse_byte_size(s) {
        Some(v) if v >= elivagar::resources::MIN_SORT_BUDGET => ByteBudget::new(v),
        _ => None,
    }
    .ok_or_else(|| format!("expected >= 64M (e.g. 256M, 1G), got '{s}'"))
}

fn parse_byte_size_min_1m(s: &str) -> Result<ByteBudget, String> {
    match parse_byte_size(s) {
        Some(v) if v >= elivagar::resources::MIN_BUDGET => ByteBudget::new(v),
        _ => None,
    }
    .ok_or_else(|| format!("expected >= 1M (e.g. 32M, 128M), got '{s}'"))
}

/// Exit for a request the command line or its values make invalid, before
/// any work. The code matches clap's own usage errors.
const EXIT_USAGE: i32 = 64;

/// A verifier that ran and found the archive defective.
const EXIT_NEGATIVE: i32 = 1;

/// Print `message` and exit with `code`. Every failure leaves through here
/// or through clap, so the exit table in `reference/cli.md` is this file.
fn fail(message: impl std::fmt::Display, code: i32) -> ! {
    eprintln!("Error: {message}");
    std::process::exit(code);
}

fn usage(message: impl std::fmt::Display) -> ! {
    fail(message, EXIT_USAGE)
}

/// A pipeline failure exits with its class's code.
fn pipeline_failure(error: &elivagar::PipelineError) -> ! {
    fail(error, error.kind().exit_code())
}

fn ocean_build_command(args: &OceanBuildArgs) {
    let inputs = elivagar::OceanInputs::parse_build_specs(&args.ocean).unwrap_or_else(|e| usage(e));
    let full = &inputs.full;
    let threads = args
        .threads
        .unwrap_or_else(elivagar::resources::default_threads);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.get())
        .build_global()
        .expect("failed to configure rayon thread pool");
    if let Err(e) = elivagar::ocean_build(
        full,
        inputs.simplified.as_deref(),
        &args.output,
        &args.tmp_dir,
        args.compression_level,
        threads,
    ) {
        pipeline_failure(&e);
    }
}

fn main() {
    // Disable hotpath metrics server by default - elivagar is a sync binary
    // with no async runtime, so the metrics server is never useful.
    // Override with HOTPATH_METRICS_SERVER_OFF=false if needed.
    if std::env::var_os("HOTPATH_METRICS_SERVER_OFF").is_none() {
        // SAFETY: called before any threads are spawned.
        unsafe { std::env::set_var("HOTPATH_METRICS_SERVER_OFF", "true") };
    }
    // Before any thread exists, so every thread inherits the blocked mask.
    signals::install();

    let cli = Cli::try_parse().unwrap_or_else(|error| {
        // Help and version go to stdout and exit 0; a usage error joins the
        // other invalid requests on EXIT_USAGE instead of clap's default 2,
        // which is the unclassified-failure code here.
        if !error.use_stderr() {
            error.exit();
        }
        drop(error.print());
        std::process::exit(EXIT_USAGE);
    });

    match cli.command {
        Command::Run(args) => run(*args),
        Command::OceanBuild(args) => ocean_build_command(&args),
        Command::Inspect(args) => {
            if let Err(e) = elivagar::inspect::inspect(&args.file) {
                pipeline_failure(&e);
            }
        }
        Command::Verify(args) => {
            match elivagar::verify::verify_opts_unique(
                &args.file,
                args.geometry_stats,
                args.unique_payloads,
            ) {
                Ok(report) => {
                    report.print_summary();
                    if let Some(stats) = &report.geometry_stats {
                        stats.print_summary();
                    }
                    if !report.passed {
                        // Completed verification found rejected tile contents.
                        std::process::exit(EXIT_NEGATIVE);
                    }
                }
                Err(e) => pipeline_failure(&e.into()),
            }
        }
        Command::Svg(args) => {
            if let Err(e) = elivagar::svg::validate_tile_grid_coordinates(
                args.z,
                args.x,
                args.y,
                args.width,
                args.height,
            ) {
                usage(e);
            }
            let layer_filter: Option<Vec<&str>> =
                args.layers.as_deref().map(|s| s.split(',').collect());
            if let Err(e) = elivagar::svg::validate_layer_filter(layer_filter.as_deref()) {
                usage(e);
            }
            let result = if let Some(ref path) = args.output {
                let mut file = std::fs::File::create(path).unwrap_or_else(|e| {
                    fail(
                        format_args!("creating {}: {e}", path.display()),
                        elivagar::ErrorKind::of_io(&e).exit_code(),
                    )
                });
                elivagar::svg::render_tile_grid_svg(
                    &args.file,
                    args.z,
                    args.x,
                    args.y,
                    args.width,
                    args.height,
                    layer_filter.as_deref(),
                    &mut file,
                )
            } else {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                elivagar::svg::render_tile_grid_svg(
                    &args.file,
                    args.z,
                    args.x,
                    args.y,
                    args.width,
                    args.height,
                    layer_filter.as_deref(),
                    &mut out,
                )
            };
            if let Err(e) = result {
                pipeline_failure(&e);
            }
        }
        Command::Diag(args) => {
            if let Err(e) = elivagar::diag::diag_tile(&args.file, args.z, args.x, args.y) {
                pipeline_failure(&e);
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run(args: RunArgs) {
    let threads = args
        .threads
        .unwrap_or_else(elivagar::resources::default_threads);

    // Configure rayon's global thread pool before any rayon work.
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.get())
        .build_global()
        .expect("failed to configure rayon thread pool");

    let skip_to = args.skip_to.map(|s| match s {
        SkipToArg::Ocean => elivagar::SkipTo::Ocean,
        SkipToArg::Sort => elivagar::SkipTo::Sort,
        SkipToArg::Assemble => elivagar::SkipTo::Assemble,
    });

    let ocean_inputs = elivagar::OceanInputs::parse_specs(&args.ocean).unwrap_or_else(|e| usage(e));
    let config = elivagar::TilegenConfig {
        pbf_path: args.input,
        output_path: args.output,
        tmp_dir: args.tmp_dir,
        min_zoom: 0,
        max_zoom: elivagar::tiling::MAX_ZOOM,
        ocean: ocean_inputs,
        skip_to,
        in_memory: args.in_memory,
        compression_level: args.compression_level,
        force_sorted: args.force_sorted,
        allow_unsafe_flat_index: args.allow_unsafe_flat_index,
        threads,
        way_inflight_budget: args.way_budget,
        assemble_batch_budget: args.assemble_budget,
        dedup_cap: args.dedup_cap,
        sort_chunk_size: args.sort_budget,
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
        fanout_caps: elivagar::FanoutCaps::parse_specs(args.fanout_cap_default, &args.fanout_cap)
            .unwrap_or_else(|e| usage(e)),
        polygon_simplify_factor: elivagar::SimplifyFactor::new(args.polygon_simplify_factor)
            .unwrap_or_else(|e| usage(e)),
    };

    // Held by the signal policy, so a SIGTERM or SIGINT mid-run (brokkr's
    // `stop_at`) still writes the report before exiting 128 + signo.
    signals::hold_profile(
        hotpath::HotpathGuardBuilder::new("elivagar::main")
            .percentiles(&[50.0, 95.0, 99.0])
            .functions_limit(0)
            .build(),
    );

    if let Err(e) = elivagar::run(&config) {
        // The hotpath guard's report is skipped on failure, as it always was:
        // process::exit runs no destructors.
        pipeline_failure(&e);
    }
    signals::finish_profile();
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
    fn test_parse_byte_size_rejects_scaled_overflow() {
        let mebibyte = 1024usize * 1024;
        let gibibyte = mebibyte * 1024;
        let max_m = usize::MAX / mebibyte;
        let max_g = usize::MAX / gibibyte;

        assert_eq!(
            parse_byte_size(&format!("{max_m}M")),
            Some(max_m * mebibyte)
        );
        assert_eq!(parse_byte_size(&format!("{}M", max_m + 1)), None);
        assert_eq!(
            parse_byte_size(&format!("{max_g}G")),
            Some(max_g * gibibyte)
        );
        assert_eq!(parse_byte_size(&format!("{}G", max_g + 1)), None);
    }

    #[test]
    fn tile_zoom_parser_rejects_unsupported_zoom() {
        assert_eq!(parse_tile_zoom("0").unwrap(), 0);
        assert_eq!(parse_tile_zoom("31").unwrap(), 31);
        assert!(parse_tile_zoom("32").is_err());
        assert!(parse_tile_zoom("256").is_err());
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
