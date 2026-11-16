//! The tilegen contract, `[<host>.tilegen.<name>]`, the selected PBF entry,
//! and the elivagar argv they expand to. Ported from brokkr's
//! `elivagar/mod.rs`, `commands.rs`, `tilegen_config.rs` and the elivagar
//! project-config schema, so the same block yields the same argv.
//!
//! The block is the whole of what a tilegen run is configured by. Nothing is
//! inferred from the filesystem and there are no override flags - a run's
//! behaviour is a function of the named block and its input, and of nothing
//! else.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

use crate::config::Paths;
use crate::error::DevError;

/// The tilegen block `tilegen` runs under. There is no selector flag yet.
pub const DEFAULT_TILEGEN: &str = "default";

/// Read buffer for dataset digests.
const DIGEST_BUFFER_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileFormat {
    Mvt,
    Mlt,
}

impl fmt::Display for TileFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mvt => "mvt",
            Self::Mlt => "mlt",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileCompression {
    Gzip,
    Brotli,
}

impl fmt::Display for TileCompression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gzip => "gzip",
            Self::Brotli => "brotli",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortChunkCompression {
    Lz4,
    Snappy,
}

impl fmt::Display for SortChunkCompression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lz4 => "lz4",
            Self::Snappy => "snappy",
        })
    }
}

/// A byte count parsed from a config string with bytes or MiB/GiB suffixes.
/// Displayed as raw bytes, which is what elivagar is passed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteSize(usize);

impl ByteSize {
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.0
    }
}

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let (digits, multiplier) = if let Some(digits) =
            value.strip_suffix('M').or_else(|| value.strip_suffix('m'))
        {
            (digits, MIB)
        } else if let Some(digits) = value.strip_suffix('G').or_else(|| value.strip_suffix('g')) {
            (digits, MIB * 1_024)
        } else {
            (value, 1)
        };
        let digits = digits
            .trim()
            .parse::<usize>()
            .map_err(|_| format!("expected bytes, MiB (M), or GiB (G), got {value:?}"))?;
        digits
            .checked_mul(multiplier)
            .map(Self)
            .ok_or_else(|| format!("byte size overflows this platform: {value:?}"))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A gzip compression level whose config value has already been range checked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompressionLevel(u8);

impl<'de> Deserialize<'de> for CompressionLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let level = u32::deserialize(deserializer)?;
        match u8::try_from(level) {
            Ok(level) if level <= 10 => Ok(Self(level)),
            _ => Err(D::Error::custom(
                "tilegen compression_level must be between 0 and 10",
            )),
        }
    }
}

impl fmt::Display for CompressionLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

const MIB: usize = 1_024 * 1_024;
const SORT_BUDGET_MIN_BYTES: usize = 64 * MIB;
const STREAM_BUDGET_MIN_BYTES: usize = MIB;

fn deserialize_sort_budget<'de, D>(deserializer: D) -> Result<Option<ByteSize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_budget(deserializer, "sort_budget", SORT_BUDGET_MIN_BYTES)
}

fn deserialize_way_budget<'de, D>(deserializer: D) -> Result<Option<ByteSize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_budget(deserializer, "way_budget", STREAM_BUDGET_MIN_BYTES)
}

fn deserialize_assemble_budget<'de, D>(deserializer: D) -> Result<Option<ByteSize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_budget(deserializer, "assemble_budget", STREAM_BUDGET_MIN_BYTES)
}

fn deserialize_budget<'de, D>(
    deserializer: D,
    name: &str,
    minimum: usize,
) -> Result<Option<ByteSize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    let Some(value) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let size = value
        .parse::<ByteSize>()
        .map_err(|error| D::Error::custom(format!("tilegen {name}: {error}")))?;
    if size.bytes() < minimum {
        return Err(D::Error::custom(format!(
            "tilegen {name} must be at least {minimum} bytes"
        )));
    }
    Ok(Some(size))
}

/// One ocean input, in elivagar's `--ocean` spec spelling.
///
/// Shapefiles name the zoom band they serve; the artifact is a bare path.
/// Paths are bare, relative to the host's `data` dir, like every other path
/// in brokkr.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OceanSpec {
    /// `z0-z14:<file.shp>` - one shapefile serves every zoom.
    ShapefileAll(String),
    /// `z0-z7:<file.shp>` - the pre-generalized low-zoom shapefile.
    ShapefileLow(String),
    /// `z8-z14:<file.shp>` - the full-resolution shapefile.
    ShapefileHigh(String),
    /// `<file.pmtiles>` - the precomputed world-ocean artifact.
    Artifact(String),
}

impl OceanSpec {
    /// Parse one `ocean` entry.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.split_once(':') {
            Some(("z0-z14", f)) => Ok(Self::ShapefileAll(f.to_owned())),
            Some(("z0-z7", f)) => Ok(Self::ShapefileLow(f.to_owned())),
            Some(("z8-z14", f)) => Ok(Self::ShapefileHigh(f.to_owned())),
            Some((band, _)) => Err(format!(
                "unknown zoom band '{band}': elivagar implements one split, at \
                 z7/z8, so the only accepted bands are z0-z14, z0-z7 and z8-z14"
            )),
            None if raw.ends_with(".pmtiles") => Ok(Self::Artifact(raw.to_owned())),
            None if raw.ends_with(".shp") => Err(format!(
                "shapefile '{raw}' needs a zoom band prefix (z0-z14:, z0-z7: or z8-z14:)"
            )),
            None => Err(format!(
                "'{raw}' is neither a zoom-banded shapefile nor a .pmtiles artifact"
            )),
        }
    }

    /// The bare path this spec names.
    pub fn file(&self) -> &str {
        match self {
            Self::ShapefileAll(f)
            | Self::ShapefileLow(f)
            | Self::ShapefileHigh(f)
            | Self::Artifact(f) => f,
        }
    }

    /// Re-render as an elivagar `--ocean` value, with `file` substituted.
    pub fn render(&self, file: &str) -> String {
        match self {
            Self::ShapefileAll(_) => format!("z0-z14:{file}"),
            Self::ShapefileLow(_) => format!("z0-z7:{file}"),
            Self::ShapefileHigh(_) => format!("z8-z14:{file}"),
            Self::Artifact(_) => file.to_owned(),
        }
    }
}

impl<'de> Deserialize<'de> for OceanSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let raw = String::deserialize(deserializer)?;
        let spec = OceanSpec::parse(&raw).map_err(D::Error::custom)?;
        if spec.file().trim().is_empty() {
            return Err(D::Error::custom("ocean input path is empty"));
        }
        Ok(spec)
    }
}

/// A named tilegen contract: `[<host>.tilegen.<name>]`.
///
/// `BTreeMap` (not `HashMap`) keeps the expanded argv byte-identical across
/// runs of the same block.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TilegenConfig {
    /// Ocean inputs. Absent or empty means no ocean.
    #[serde(default)]
    pub ocean: Vec<OceanSpec>,
    /// Gzip level 0-10. A *base*: elivagar clamps low zooms up and caps
    /// z13/z14 down, per `config.tile.compression_policy` in provenance.
    pub compression_level: Option<CompressionLevel>,
    pub tile_format: Option<TileFormat>,
    pub tile_compression: Option<TileCompression>,
    pub compress_sort_chunks: Option<SortChunkCompression>,
    #[serde(default)]
    pub in_memory: bool,
    /// `-j`. A host tuning knob, not part of the comparability contract.
    pub threads: Option<u32>,
    /// Sizes accept `256M`, `1G`, or raw bytes.
    #[serde(default, deserialize_with = "deserialize_sort_budget")]
    pub sort_budget: Option<ByteSize>,
    #[serde(default, deserialize_with = "deserialize_way_budget")]
    pub way_budget: Option<ByteSize>,
    #[serde(default, deserialize_with = "deserialize_assemble_budget")]
    pub assemble_budget: Option<ByteSize>,
    /// Entry cap for the PMTiles payload dedup map. Plain integer, no size
    /// suffix. Absent = elivagar's default.
    pub dedup_cap: Option<u64>,
    /// Default fanout cap for all polygon layers. 0 or absent = uncapped.
    pub fanout_cap_default: Option<u32>,
    /// Per-layer fanout caps; take precedence over the default.
    #[serde(default)]
    pub fanout_caps: BTreeMap<String, u32>,
    pub polygon_simplify_factor: Option<f64>,
    /// Expert debugging only; may cause severe IO/RSS degradation.
    #[serde(default)]
    pub allow_unsafe_flat_index: bool,
}

/// Keys brokkr's schema still accepted that elivagar no longer honours.
/// Refused by name rather than by `deny_unknown_fields`, so the message says
/// why.
const RETIRED_KEYS: &[(&str, &str)] = &[(
    "seam_reconcile_layers",
    "elivagar removed --seam-reconcile-layers at 099611b, so no revision this \
     tool builds accepts it; delete the key from the block",
)];

/// Resolve the named tilegen contract for the selected host.
///
/// A host with no `[<host>.tilegen.*]` is an error, not an implicit bare run.
/// A bare run - no ocean, stock everything - already has a spelling: an empty
/// block.
pub fn resolve_tilegen(paths: &Paths, name: &str) -> Result<TilegenConfig, DevError> {
    let host = &paths.hostname;
    let blocks = match paths.host.get("tilegen") {
        None => None,
        Some(toml::Value::Table(t)) => Some(t),
        Some(_) => {
            return Err(DevError::Config(format!(
                "[{host}].tilegen must be a table of named blocks"
            )));
        }
    }
    .filter(|t| !t.is_empty())
    .ok_or_else(|| {
        DevError::Config(format!(
            "no [{host}.tilegen.*] blocks in brokkr.toml; tilegen is configured \
             entirely from a named block, so add [{host}.tilegen.{name}] (an empty \
             block is a valid no-ocean, stock-everything contract)"
        ))
    })?;

    let block = blocks.get(name).ok_or_else(|| {
        let known: Vec<&str> = blocks.keys().map(String::as_str).collect();
        DevError::Config(format!(
            "no [{host}.tilegen.{name}] in brokkr.toml (known: {})",
            known.join(", ")
        ))
    })?;
    let table = block
        .as_table()
        .ok_or_else(|| DevError::Config(format!("[{host}.tilegen.{name}] must be a table")))?;
    for (key, why) in RETIRED_KEYS {
        if table.contains_key(*key) {
            return Err(DevError::Config(format!(
                "[{host}.tilegen.{name}].{key} is retired: {why}"
            )));
        }
    }
    block
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| DevError::Config(format!("[{host}.tilegen.{name}]: {e}")))
}

/// A single `[<host>.datasets.<D>.pbf.<V>]` entry. `locations_on_ways` and
/// `force_sorted` are assertions about this file, so they live with the
/// variant that has the property rather than in a tilegen block.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PbfEntry {
    pub file: String,
    pub xxhash: Option<String>,
    pub seq: Option<u64>,
    /// Force `--locations-on-ways` when the PBF header does not declare it.
    #[serde(default)]
    pub locations_on_ways: bool,
    /// Force the compact node store without the PBF sort header.
    #[serde(default)]
    pub force_sorted: bool,
    /// Accepted for config compatibility; unused here, as in brokkr.
    #[serde(default)]
    pub expect: Option<toml::Value>,
}

/// The selected PBF entry and its validated on-disk path.
#[derive(Debug)]
pub struct ResolvedPbf {
    pub path: PathBuf,
    pub entry: PbfEntry,
}

/// Resolve `dataset`/`variant` from the host's dataset table: the file must
/// exist under the data dir and, when the entry pins an `xxhash`, hash to it.
pub fn resolve_pbf(paths: &Paths, dataset: &str, variant: &str) -> Result<ResolvedPbf, DevError> {
    let datasets = paths.host.get("datasets").and_then(toml::Value::as_table);
    let ds = datasets
        .and_then(|d| d.get(dataset))
        .and_then(toml::Value::as_table)
        .ok_or_else(|| {
            let known: Vec<&str> = datasets
                .map(|d| d.keys().map(String::as_str).collect())
                .unwrap_or_default();
            DevError::Config(format!(
                "unknown dataset '{dataset}' for host '{}' (known: {})",
                paths.hostname,
                known.join(", ")
            ))
        })?;
    let pbf = ds.get("pbf").and_then(toml::Value::as_table);
    let raw = pbf.and_then(|p| p.get(variant)).ok_or_else(|| {
        let known: Vec<&str> = pbf
            .map(|p| p.keys().map(String::as_str).collect())
            .unwrap_or_default();
        DevError::Config(format!(
            "dataset '{dataset}' has no pbf variant '{variant}' (available: {})",
            known.join(", ")
        ))
    })?;
    let entry: PbfEntry = raw.clone().try_into().map_err(|e: toml::de::Error| {
        DevError::Config(format!("datasets.{dataset}.pbf.{variant}: {e}"))
    })?;
    let path = paths.data_dir.join(&entry.file);
    if !path.exists() {
        return Err(DevError::Config(format!(
            "pbf variant file not found: {}",
            path.display()
        )));
    }
    if let Some(expected) = &entry.xxhash {
        let expected = expected.to_ascii_lowercase();
        if expected.len() != 32 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DevError::Config(format!(
                "datasets.{dataset}.pbf.{variant}.xxhash: expected 32 hex digits, got {expected:?}"
            )));
        }
        let observed = file_digest(&path)?;
        if observed != expected {
            return Err(DevError::Config(format!(
                "{}: xxhash mismatch: expected {expected}, observed {observed}",
                path.display()
            )));
        }
    }
    Ok(ResolvedPbf { path, entry })
}

/// XXH3-128 over the file bytes, as 32 lowercase hex digits: brokkr's
/// registered-digest format. Uncached, so a large input is read in full.
pub fn file_digest(path: &Path) -> Result<String, DevError> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = xxhash_rust::xxh3::Xxh3::new();
    let mut buffer = vec![0; DIGEST_BUFFER_BYTES];
    loop {
        let count = match file.read(&mut buffer) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:032x}", hash.digest128()))
}

/// The full `elivagar run` argv for `pbf` under `tg`: input, scratch output,
/// `--tmp-dir`, the optional resume point, then the block's flags. Ocean
/// paths resolve against the data dir; a named ocean input that is missing is
/// an error, never dropped from the argv.
pub fn run_args(
    paths: &Paths,
    tg: &TilegenConfig,
    pbf: &ResolvedPbf,
    skip_to: Option<&str>,
) -> Result<Vec<String>, DevError> {
    let mut args: Vec<String> = vec![
        "run".into(),
        pbf.path.display().to_string(),
        "-o".into(),
        paths.tilegen_scratch_output().display().to_string(),
        "--tmp-dir".into(),
        paths.tilegen_tmp().display().to_string(),
    ];
    if let Some(phase) = skip_to {
        args.push("--skip-to".into());
        args.push(phase.into());
    }
    push_configured(tg, &pbf.entry, &mut args, |spec| {
        let path = paths.data_dir.join(spec.file());
        if !path.exists() {
            return Err(DevError::Config(format!(
                "tilegen ocean input not found: {} (named by brokkr.toml as '{}'); \
                 omitting it would silently change the ocean geometry this run \
                 produces",
                path.display(),
                spec.file()
            )));
        }
        Ok(spec.render(&path.display().to_string()))
    })?;
    Ok(args)
}

/// Append the flags the block and the input assertions configure. The order
/// is brokkr's, flag for flag.
fn push_configured(
    tg: &TilegenConfig,
    entry: &PbfEntry,
    args: &mut Vec<String>,
    ocean: impl Fn(&OceanSpec) -> Result<String, DevError>,
) -> Result<(), DevError> {
    if let Some(level) = tg.compression_level {
        args.push("--compression-level".into());
        args.push(level.to_string());
    }
    if entry.force_sorted {
        args.push("--force-sorted".into());
    }
    if entry.locations_on_ways {
        args.push("--locations-on-ways".into());
    }
    if tg.allow_unsafe_flat_index {
        args.push("--allow-unsafe-flat-index".into());
    }
    if let Some(fmt) = &tg.tile_format {
        args.push("--tile-format".into());
        args.push(fmt.to_string());
    }
    if let Some(comp) = &tg.tile_compression {
        args.push("--tile-compression".into());
        args.push(comp.to_string());
    }
    if let Some(algo) = &tg.compress_sort_chunks {
        args.push("--compress-sort-chunks".into());
        args.push(algo.to_string());
    }
    if tg.in_memory {
        args.push("--in-memory".into());
    }
    if let Some(n) = tg.threads {
        args.push("-j".into());
        args.push(n.to_string());
    }
    if let Some(s) = &tg.sort_budget {
        args.push("--sort-budget".into());
        args.push(s.to_string());
    }
    if let Some(s) = &tg.way_budget {
        args.push("--way-budget".into());
        args.push(s.to_string());
    }
    if let Some(s) = &tg.assemble_budget {
        args.push("--assemble-budget".into());
        args.push(s.to_string());
    }
    if let Some(n) = tg.dedup_cap {
        args.push("--dedup-cap".into());
        args.push(n.to_string());
    }
    if let Some(n) = tg.fanout_cap_default {
        args.push("--fanout-cap-default".into());
        args.push(n.to_string());
    }
    if !tg.fanout_caps.is_empty() {
        let spec: Vec<String> = tg
            .fanout_caps
            .iter()
            .map(|(layer, cap)| format!("{layer}={cap}"))
            .collect();
        args.push("--fanout-cap".into());
        args.push(spec.join(","));
    }
    if let Some(f) = tg.polygon_simplify_factor {
        args.push("--polygon-simplify-factor".into());
        args.push(f.to_string());
    }
    for spec in &tg.ocean {
        let rendered = ocean(spec)?;
        args.push("--ocean".into());
        args.push(rendered);
    }
    Ok(())
}

/// The `elivagar ocean-build` argv derived from the block's `ocean` list: the
/// shapefile entries become banded `--ocean` specs exactly as tilegen passes
/// them, and the single `.pmtiles` entry becomes `-o`. Returns the argv and
/// the output path.
pub fn ocean_build_args(
    paths: &Paths,
    tg: &TilegenConfig,
) -> Result<(Vec<String>, PathBuf), DevError> {
    let mut shapefiles: Vec<&OceanSpec> = Vec::new();
    let mut artifact: Option<&OceanSpec> = None;
    for spec in &tg.ocean {
        match spec {
            OceanSpec::Artifact(_) => {
                if artifact.is_some() {
                    return Err(DevError::Config(
                        "[<host>.tilegen.default].ocean names more than one .pmtiles \
                         artifact; ocean-build writes exactly one output"
                            .to_owned(),
                    ));
                }
                artifact = Some(spec);
            }
            _ => shapefiles.push(spec),
        }
    }
    let artifact = artifact.ok_or_else(|| {
        DevError::Config(
            "[<host>.tilegen.default].ocean has no .pmtiles artifact entry, so \
             ocean-build has nowhere to write its output; add the artifact path \
             to the block"
                .to_owned(),
        )
    })?;
    if shapefiles.is_empty() {
        return Err(DevError::Config(
            "[<host>.tilegen.default].ocean has no shapefile entries, so \
             ocean-build has nothing to build from; add the zoom-banded \
             shapefile specs to the block"
                .to_owned(),
        ));
    }
    let mut args: Vec<String> = vec!["ocean-build".to_owned()];
    for spec in &shapefiles {
        let path = paths.data_dir.join(spec.file());
        if !path.exists() {
            return Err(DevError::Config(format!(
                "ocean shapefile not found: {} (named by brokkr.toml as '{}')",
                path.display(),
                spec.file()
            )));
        }
        args.push("--ocean".to_owned());
        args.push(spec.render(&path.display().to_string()));
    }
    let output_path = paths.data_dir.join(artifact.file());
    args.push("-o".to_owned());
    args.push(output_path.display().to_string());
    Ok((args, output_path))
}
