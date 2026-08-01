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
    #[arg(short, long)]
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

    /// Tile assembly batch budget (e.g. 32M). Minimum 1M.
    #[arg(long, value_parser = parse_byte_size_min_1m)]
    assemble_budget: Option<usize>,

    /// PBF has node coordinates embedded in ways.
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

    /// Polygon layers to apply shared-edge seam reconciliation at low zoom.
    /// Comma-separated, format: layer or layer:maxzoom (default maxzoom: 8).
    /// Example: boundaries,water_polygons:5
    #[arg(long, value_delimiter = ',', default_value = "boundaries")]
    seam_reconcile_layers: Vec<String>,

    /// Default fanout cap for all polygon layers. 0 or omitted = uncapped.
    /// Per-layer overrides via --fanout-cap take precedence.
    #[arg(long)]
    fanout_cap_default: Option<u32>,

    /// Per-layer fanout caps: max bbox tiles a polygon feature may touch.
    /// Comma-separated, format: layer=N. Features exceeding the cap are skipped
    /// at that zoom. Example: water_polygons=2048,boundaries=4096
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
    #[arg(short = 'j', long)]
    threads: Option<usize>,
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
    #[arg(short, long)]
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
            .map(|v| v * 1024 * 1024 * 1024)
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

/// The ocean inputs named by `--ocean`, after parsing and validation.
///
/// `low` is `Some` only for the two-pass z0-z7 + z8-z14 split; a single
/// z0-z14 entry lands in `high` alone and serves every zoom, matching
/// `ocean::selected_pass_grid`, which picks the last grid unless two grids are
/// configured and z <= 7.
#[derive(Debug, Default, PartialEq, Eq)]
struct OceanInputs {
    low: Option<PathBuf>,
    high: Option<PathBuf>,
    artifact: Option<PathBuf>,
}

/// Parse one `--ocean` value into either a zoom-ranged shapefile or an
/// artifact. Nothing here consults the filesystem: a spec means what it says,
/// and a path that does not exist is diagnosed later, by the code that opens
/// it, rather than silently changing what the run is.
fn parse_ocean_spec(spec: &str) -> Result<(Option<(u8, u8)>, PathBuf), String> {
    let Some((range, path)) = spec.split_once(':') else {
        // No range prefix: only the artifact is accepted bare, because it is
        // the one input whose zoom coverage is a property of the file.
        if Path::new(spec).extension().is_some_and(|e| e == "pmtiles") {
            return Ok((None, PathBuf::from(spec)));
        }
        return Err(format!(
            "'{spec}' needs a zoom range, e.g. z0-z14:{spec} (bare paths are accepted only for a .pmtiles artifact)"
        ));
    };
    let parse_z = |t: &str| -> Result<u8, String> {
        t.strip_prefix('z')
            .ok_or_else(|| format!("'{t}' is not a zoom (expected z0-z14)"))?
            .parse::<u8>()
            .map_err(|_| format!("'{t}' is not a zoom (expected z0-z14)"))
    };
    let (lo, hi) = range
        .split_once("-")
        .ok_or_else(|| format!("'{range}' is not a zoom range (expected z0-z7)"))?;
    let (lo, hi) = (parse_z(lo)?, parse_z(hi)?);
    if lo > hi || hi > 14 {
        return Err(format!("'{range}' is not a zoom range within z0-z14"));
    }
    Ok((Some((lo, hi)), PathBuf::from(path)))
}

/// Validate the whole `--ocean` set and map it onto the two shapefile passes.
///
/// The zoom ranges are checked against what the engine can actually do rather
/// than accepted and rounded: `selected_pass_grid` implements exactly one
/// split, at z7/z8, so those are the only partitions expressible. Accepting
/// `z0-z5` and quietly serving a z7 split would put a false statement in the
/// recorded invocation, which is the whole thing this flag exists to prevent.
fn resolve_ocean_inputs(specs: &[String]) -> Result<OceanInputs, String> {
    let mut out = OceanInputs::default();
    let mut ranges: Vec<((u8, u8), PathBuf)> = Vec::new();
    for spec in specs {
        match parse_ocean_spec(spec)? {
            (None, path) => {
                if out.artifact.replace(path).is_some() {
                    return Err("--ocean names more than one .pmtiles artifact".to_string());
                }
            }
            (Some(range), path) => ranges.push((range, path)),
        }
    }
    ranges.sort_by_key(|(r, _)| *r);
    match ranges.as_slice() {
        [] => {}
        [((0, 14), full)] => out.high = Some(full.clone()),
        [((0, 7), low), ((8, 14), high)] => {
            out.low = Some(low.clone());
            out.high = Some(high.clone());
        }
        _ => {
            return Err(
                "--ocean shapefiles must partition z0-z14 as either z0-z14 alone or z0-z7 plus z8-z14 (the engine splits at z7/z8 and nowhere else)"
                    .to_string(),
            );
        }
    }
    if out.artifact.is_some() && out.high.is_none() {
        return Err(
            "--ocean names a .pmtiles artifact but no shapefile: an extract computes its boundary band from the shapefiles, and the artifact's key is validated by re-hashing them"
                .to_string(),
        );
    }
    Ok(out)
}

fn ocean_build_command(args: &OceanBuildArgs) {
    let inputs = match resolve_ocean_inputs(&args.ocean) {
        Ok(inputs) if inputs.artifact.is_some() => {
            eprintln!("error: ocean-build --ocean takes shapefiles, not a .pmtiles artifact");
            std::process::exit(2);
        }
        Ok(inputs) => inputs,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let Some(full) = inputs.high.as_deref() else {
        eprintln!("error: ocean-build needs a full-resolution shapefile");
        std::process::exit(2);
    };
    let threads = args.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(4)
    });
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .expect("failed to configure rayon thread pool");
    if let Err(e) = elivagar::ocean_build(
        full,
        inputs.low.as_deref(),
        &args.output,
        &args.tmp_dir,
        args.compression_level,
        threads,
    ) {
        eprintln!("Error: {e}");
        std::process::exit(1);
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

    let cli = Cli::parse();

    match cli.command {
        Command::Run(args) => run(*args),
        Command::OceanBuild(args) => ocean_build_command(&args),
        Command::Inspect(args) => {
            if let Err(e) = elivagar::inspect::inspect(&args.file) {
                eprintln!("Error: {e}");
                std::process::exit(1);
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
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Svg(args) => {
            let result = if let Some(ref path) = args.output {
                let mut file = std::fs::File::create(path).unwrap_or_else(|e| {
                    eprintln!("Error creating {}: {e}", path.display());
                    std::process::exit(1);
                });
                let layer_filter: Option<Vec<&str>> =
                    args.layers.as_deref().map(|s| s.split(',').collect());
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
                let layer_filter: Option<Vec<&str>> =
                    args.layers.as_deref().map(|s| s.split(',').collect());
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
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Command::Diag(args) => {
            if let Err(e) = diag_ocean_rings(&args.file, args.z, args.x, args.y) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn diag_ocean_rings(path: &Path, z: u8, x: u32, y: u32) -> std::io::Result<()> {
    use elivagar::pmtiles_reader::{PmtilesReader, find_entry};
    use elivagar::pmtiles_writer::xy_to_tile_id;
    use protohoggr::{Cursor, WIRE_LEN};

    let mut reader = PmtilesReader::open(path)?;
    let runs = reader.read_all_runs()?;
    let tile_id = xy_to_tile_id(z, x, y);
    let entry = find_entry(&runs, tile_id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "tile not found"))?;
    let raw = reader.read_tile(&entry)?;

    println!(
        "Tile z{z}/{x}/{y} - {raw_len} bytes decompressed",
        raw_len = raw.len()
    );

    // Parse MVT tile - find all layers
    let mut tc = Cursor::new(&raw);
    while let Ok(Some((field, wire_type))) = tc.read_tag() {
        if field == 3 && wire_type == WIRE_LEN {
            if let Ok(layer_data) = tc.read_len_delimited() {
                diag_layer(layer_data);
            }
        } else {
            drop(tc.skip_field(wire_type));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn diag_layer(data: &[u8]) {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    let mut name = "";

    // First pass: get name and feature byte ranges
    let mut feature_ranges: Vec<&[u8]> = Vec::new();
    let mut lc = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = lc.read_tag() {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                if let Ok(bytes) = lc.read_len_delimited() {
                    name = std::str::from_utf8(bytes).unwrap_or("<invalid>");
                }
            }
            (2, WIRE_LEN) => {
                if let Ok(bytes) = lc.read_len_delimited() {
                    feature_ranges.push(bytes);
                }
            }
            _ => {
                drop(lc.skip_field(wire_type));
            }
        }
    }

    // Count polygon features
    let mut poly_count = 0;
    for fdata in &feature_ranges {
        let mut fc = Cursor::new(fdata);
        let mut gt: u64 = 0;
        while let Ok(Some((ff, fw))) = fc.read_tag() {
            if ff == 3 && fw == WIRE_VARINT {
                gt = fc.read_varint().unwrap_or(0);
            } else {
                drop(fc.skip_field(fw));
            }
        }
        if gt == 3 {
            poly_count += 1;
        }
    }

    println!(
        "  Layer '{name}': {n} features ({poly_count} polygons)",
        n = feature_ranges.len()
    );

    // Parse each feature - only print polygon detail
    for (fi, fdata) in feature_ranges.iter().enumerate() {
        let mut geom_type: u64 = 0;
        let mut fid: u64 = 0;
        let mut geometry: Vec<u32> = Vec::new();

        let mut fc = Cursor::new(fdata);
        while let Ok(Some((ff, fw))) = fc.read_tag() {
            match (ff, fw) {
                (1, WIRE_VARINT) => {
                    fid = fc.read_varint().unwrap_or(0);
                }
                (3, WIRE_VARINT) => {
                    geom_type = fc.read_varint().unwrap_or(0);
                }
                (4, WIRE_LEN) => {
                    if let Ok(bytes) = fc.read_len_delimited() {
                        let mut pc = Cursor::new(bytes);
                        while let Ok(v) = pc.read_varint() {
                            #[allow(clippy::cast_possible_truncation)]
                            geometry.push(v as u32);
                        }
                    }
                }
                _ => {
                    drop(fc.skip_field(fw));
                }
            }
        }

        if geom_type == 3 && !geometry.is_empty() {
            let rings = diag_decode_polygon(&geometry);
            println!(
                "    Feature {fi}: id={fid} geom_type={geom_type} rings={nr}",
                nr = rings.len()
            );
            for (ri, ring) in rings.iter().enumerate() {
                let area = signed_area_ring(ring);
                let winding = if area > 0 {
                    "CW (outer)"
                } else if area < 0 {
                    "CCW (hole)"
                } else {
                    "ZERO"
                };
                let simple = diag_ring_is_simple(ring);
                let simple_str = if simple {
                    ""
                } else {
                    " *** SELF-INTERSECTING ***"
                };
                println!(
                    "      ring {ri}: {nv} verts, area={area}, {winding}{simple_str}",
                    nv = ring.len()
                );
                if ring.len() <= 6 {
                    for &(x, y) in ring {
                        println!("        ({x}, {y})");
                    }
                } else {
                    for &(x, y) in &ring[..3] {
                        println!("        ({x}, {y})");
                    }
                    println!("        ...");
                    for &(x, y) in &ring[ring.len() - 3..] {
                        println!("        ({x}, {y})");
                    }
                }
            }
        }
    }
}

/// Inline MVT polygon decoder (avoids needing pub access to geometry module).
fn diag_decode_polygon(commands: &[u32]) -> Vec<Vec<(i32, i32)>> {
    let mut rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    let mut i = 0;
    while i < commands.len() {
        let cmd = commands[i];
        let cmd_id = cmd & 0x7;
        let cmd_count = cmd >> 3;
        i += 1;
        match cmd_id {
            1 => {
                for _ in 0..cmd_count {
                    if i + 1 >= commands.len() {
                        return rings;
                    }
                    let dx = unzigzag_diag(commands[i]);
                    let dy = unzigzag_diag(commands[i + 1]);
                    i += 2;
                    cx += dx;
                    cy += dy;
                    rings.push(vec![(cx, cy)]);
                }
            }
            2 => {
                let Some(ring) = rings.last_mut() else {
                    i += (cmd_count as usize) * 2;
                    continue;
                };
                for _ in 0..cmd_count {
                    if i + 1 >= commands.len() {
                        return rings;
                    }
                    let dx = unzigzag_diag(commands[i]);
                    let dy = unzigzag_diag(commands[i + 1]);
                    i += 2;
                    cx += dx;
                    cy += dy;
                    ring.push((cx, cy));
                }
            }
            7 => {
                // ClosePath: cursor unchanged per MVT spec 4.3.3.3.
                if let Some(ring) = rings.last_mut()
                    && let Some(&first) = ring.first()
                {
                    ring.push(first);
                }
            }
            _ => {}
        }
    }
    rings
}

#[inline]
fn unzigzag_diag(n: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((n >> 1) as i32) ^ (-((n & 1) as i32))
    }
}

fn diag_ring_is_simple(ring: &[(i32, i32)]) -> bool {
    if ring.len() < 4 {
        return true;
    }
    let n = ring.len() - 1; // exclude closing vertex
    for i in 0..n {
        let a1 = ring[i];
        let a2 = ring[i + 1];
        for j in (i + 2)..n {
            if j + 1 == ring.len() && i == 0 {
                continue;
            }
            let b1 = ring[j];
            let b2 = ring[(j + 1) % ring.len()];
            // segments_cross: proper crossing only
            let d1 = diag_cross_sign(a1, a2, b1);
            let d2 = diag_cross_sign(a1, a2, b2);
            let d3 = diag_cross_sign(b1, b2, a1);
            let d4 = diag_cross_sign(b1, b2, a2);
            if d1 != d2 && d3 != d4 && d1 != 0 && d2 != 0 && d3 != 0 && d4 != 0 {
                return false;
            }
        }
    }
    true
}

fn diag_cross_sign(p1: (i32, i32), p2: (i32, i32), p3: (i32, i32)) -> i8 {
    let cross = i64::from(p2.0 - p1.0) * i64::from(p3.1 - p1.1)
        - i64::from(p2.1 - p1.1) * i64::from(p3.0 - p1.0);
    if cross > 0 {
        1
    } else if cross < 0 {
        -1
    } else {
        0
    }
}

fn signed_area_ring(ring: &[(i32, i32)]) -> i64 {
    if ring.len() < 3 {
        return 0;
    }
    let mut sum: i64 = 0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        sum += i64::from(ring[i].0) * i64::from(ring[j].1);
        sum -= i64::from(ring[j].0) * i64::from(ring[i].1);
    }
    sum / 2
}

#[allow(clippy::too_many_lines)]
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

    // The ocean is exactly what --ocean names. Nothing is inferred from the
    // filesystem: a run used to change its ocean path depending on whether
    // data/ocean-tiles.pmtiles happened to exist, which made two runs of the
    // same binary on the same input differ with nothing in the recorded
    // invocation to say how.
    let ocean_inputs = match resolve_ocean_inputs(&args.ocean) {
        Ok(inputs) => inputs,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    // Refuse an MLT run up front rather than at the first encoded batch, which
    // is minutes of phase12 and sort later. The flag value stays accepted by
    // clap in every build so the error names the missing feature instead of
    // reading as an unknown value.
    #[cfg(not(feature = "mlt"))]
    if matches!(args.tile_format, TileFormatArg::Mlt) {
        eprintln!(
            "error: --tile-format mlt requires a build with the `mlt` cargo feature \
             (cargo build --release --features mlt)"
        );
        std::process::exit(2);
    }

    let config = elivagar::TilegenConfig {
        pbf_path: args.input,
        output_path: args.output,
        tmp_dir: args.tmp_dir,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: ocean_inputs.high,
        ocean_simplified_shapefile: ocean_inputs.low,
        ocean_tiles: ocean_inputs.artifact,
        ocean_artifact_key: None,
        ocean_only_metadata: false,
        skip_to,
        in_memory: args.in_memory,
        compression_level: args.compression_level,
        force_sorted: args.force_sorted,
        allow_unsafe_flat_index: args.allow_unsafe_flat_index,
        threads,
        way_inflight_budget: args.way_budget.unwrap_or(0),
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
            const DEFAULT_MAX_ZOOM: u8 = 8;
            let mut mask = [0u8; elivagar::shortbread::Layer::count()];
            for spec in &args.seam_reconcile_layers {
                let trimmed = spec.trim();
                let (name, max_zoom) = if let Some((n, z)) = trimmed.split_once(':') {
                    let z_val: u8 = z.trim().parse().unwrap_or_else(|_| {
                        eprintln!("Error: invalid zoom in --seam-reconcile-layers: '{trimmed}'");
                        std::process::exit(1);
                    });
                    if z_val > 14 {
                        eprintln!(
                            "Error: zoom must be 0-14 in --seam-reconcile-layers: '{trimmed}'"
                        );
                        std::process::exit(1);
                    }
                    (n.trim(), z_val)
                } else {
                    (trimmed, DEFAULT_MAX_ZOOM)
                };
                match elivagar::shortbread::Layer::from_name(name) {
                    Some(layer) => mask[layer as usize] = max_zoom,
                    None => {
                        eprintln!(
                            "Error: unknown layer name for --seam-reconcile-layers: '{name}'"
                        );
                        std::process::exit(1);
                    }
                }
            }
            mask
        },
        fanout_caps: {
            let default_cap = args.fanout_cap_default.unwrap_or(0);
            let mut caps = [default_cap; elivagar::shortbread::Layer::count()];
            for spec in &args.fanout_cap {
                let trimmed = spec.trim();
                let (name, cap_str) = match trimmed.split_once('=') {
                    Some(pair) => pair,
                    None => {
                        eprintln!(
                            "Error: invalid --fanout-cap format, expected layer=N: '{trimmed}'"
                        );
                        std::process::exit(1);
                    }
                };
                let name = name.trim();
                let cap_val: u32 = cap_str.trim().parse().unwrap_or_else(|_| {
                    eprintln!("Error: invalid cap value in --fanout-cap: '{trimmed}'");
                    std::process::exit(1);
                });
                match elivagar::shortbread::Layer::from_name(name) {
                    Some(layer) => caps[layer as usize] = cap_val,
                    None => {
                        eprintln!("Error: unknown layer name for --fanout-cap: '{name}'");
                        std::process::exit(1);
                    }
                }
            }
            caps
        },
        polygon_simplify_factor: {
            let f = args.polygon_simplify_factor;
            if f < 0.1 || f > 10.0 {
                eprintln!("Error: --polygon-simplify-factor must be between 0.1 and 10.0, got {f}");
                std::process::exit(1);
            }
            f
        },
    };

    let _guard = hotpath::HotpathGuardBuilder::new("elivagar::main")
        .percentiles(&[50.0, 95.0, 99.0])
        .functions_limit(0)
        .build();

    if let Err(e) = elivagar::run(&config) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ocean(specs: &[&str]) -> Result<OceanInputs, String> {
        let owned: Vec<String> = specs.iter().map(|s| (*s).to_string()).collect();
        resolve_ocean_inputs(&owned)
    }

    #[test]
    fn ocean_absent_means_no_ocean() {
        assert_eq!(ocean(&[]).unwrap(), OceanInputs::default());
    }

    #[test]
    fn ocean_single_full_range_serves_every_zoom() {
        let got = ocean(&["z0-z14:full.shp"]).unwrap();
        assert_eq!(got.high, Some(PathBuf::from("full.shp")));
        assert_eq!(got.low, None);
        assert_eq!(got.artifact, None);
    }

    #[test]
    fn ocean_split_maps_low_and_high_passes() {
        // Order of the flags must not matter; the ranges decide the passes.
        let got = ocean(&["z8-z14:full.shp", "z0-z7:simple.shp"]).unwrap();
        assert_eq!(got.low, Some(PathBuf::from("simple.shp")));
        assert_eq!(got.high, Some(PathBuf::from("full.shp")));
    }

    #[test]
    fn ocean_artifact_is_additive_to_the_shapefiles() {
        let got = ocean(&["z0-z14:full.shp", "ocean-tiles.pmtiles"]).unwrap();
        assert_eq!(got.high, Some(PathBuf::from("full.shp")));
        assert_eq!(got.artifact, Some(PathBuf::from("ocean-tiles.pmtiles")));
    }

    #[test]
    fn ocean_artifact_alone_is_rejected() {
        // An extract computes its boundary band from the shapefiles and the
        // artifact key is validated by re-hashing them, so an artifact with no
        // shapefile is an incomplete spec, not a world build.
        let err = ocean(&["ocean-tiles.pmtiles"]).unwrap_err();
        assert!(err.contains("no shapefile"), "{err}");
    }

    #[test]
    fn ocean_rejects_a_split_the_engine_cannot_do() {
        // selected_pass_grid splits at z7/z8 and nowhere else. Accepting this
        // and serving a z7 split would put a false statement in the recorded
        // invocation.
        let err = ocean(&["z0-z5:simple.shp", "z6-z14:full.shp"]).unwrap_err();
        assert!(err.contains("partition z0-z14"), "{err}");
    }

    #[test]
    fn ocean_rejects_ranges_that_leave_a_gap() {
        let err = ocean(&["z0-z7:simple.shp"]).unwrap_err();
        assert!(err.contains("partition z0-z14"), "{err}");
    }

    #[test]
    fn ocean_rejects_a_bare_shapefile() {
        let err = ocean(&["full.shp"]).unwrap_err();
        assert!(err.contains("needs a zoom range"), "{err}");
    }

    #[test]
    fn ocean_rejects_two_artifacts() {
        let err = ocean(&["z0-z14:full.shp", "a.pmtiles", "b.pmtiles"]).unwrap_err();
        assert!(err.contains("more than one"), "{err}");
    }

    #[test]
    fn ocean_rejects_malformed_ranges() {
        assert!(
            ocean(&["0-14:full.shp"])
                .unwrap_err()
                .contains("not a zoom")
        );
        assert!(ocean(&["z9-z2:full.shp"]).unwrap_err().contains("range"));
        assert!(ocean(&["z0-z15:full.shp"]).unwrap_err().contains("range"));
    }

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
