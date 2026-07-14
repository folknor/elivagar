//! Provenance recorded in PMTiles archive metadata under the `elivagar` key.
//!
//! Groups are split by how a consumer must treat them, and the split is the
//! load-bearing part of the design:
//!
//! - `input` and `config` are the COMPARABILITY CONTRACT. Two archives whose
//!   contract differs describe different work, so a geometry diff between them
//!   carries no information about the code. A consumer compares these first
//!   and refuses the diff on mismatch. The motivating case: a raw PBF and a
//!   locations-prepass PBF drive different coordinate, membership and pin
//!   paths and produce legitimately different tiles, which reads as a large
//!   elivagar regression when the inputs are not recorded.
//! - `effective` and `build` are DIAGNOSTIC and must never be equality-gated.
//!   Given identical input and config they are a function of the code, and
//!   regression testing exists to compare revisions - gating them would refuse
//!   exactly the comparisons the gate is for. They explain a diff once the
//!   contract matches.
//!
//! Reproducibility constraint: nothing in this block may vary between two
//! builds of the same commit on the same input. No wall-clock, no hostname,
//! no absolute paths, no thread counts, no timings, no measured durations.
//! Same-commit builds are byte-identical and blessing records a hash over the
//! whole archive; a per-run value here would break both.
//!
//! The `ocean_artifact` member stays a separate top-level key rather than
//! moving under `elivagar`: `ocean::OceanArtifactKey::from_json` reads it at
//! the top level to validate the durable world-ocean artifact, and relocating
//! it would invalidate every artifact already built.

use crate::shortbread::Layer;
use crate::{TileCompression, TilePayloadFormat, TilegenConfig};
use serde_json::{Value, json};
use std::path::Path;

/// Schema version of the `elivagar` metadata member.
///
/// Bump when the meaning of an existing field changes. Adding a member does
/// not require a bump: readers ignore members they do not know, and the
/// contract comparison is defined over named fields, not over the whole
/// object.
pub const SCHEMA_VERSION: u32 = 1;

/// PBF header features observed on the input.
///
/// These are read from the header, not inferred from a filename, and they are
/// what actually determines which paths the pipeline takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PbfFeatures {
    /// `Sort.Type_then_ID` - enables the compact node store.
    pub sort_type_then_id: bool,
    /// `LocationsOnWays` - way elements carry inline node coordinates.
    pub locations_on_ways: bool,
    /// `WayMembers-v1` - injected relation membership, skips the relation scan.
    pub way_members_v1: bool,
    /// `SharedNodePins-v1` - injected global shared-node pins.
    pub shared_node_pins_v1: bool,
}

impl PbfFeatures {
    fn to_json(self) -> Value {
        json!({
            "sort_type_then_id": self.sort_type_then_id,
            "locations_on_ways": self.locations_on_ways,
            "way_members_v1": self.way_members_v1,
            "shared_node_pins_v1": self.shared_node_pins_v1,
        })
    }
}

/// Identity of the input PBF.
///
/// `xxh3_128` is the identity. `name` is a label recorded for humans and must
/// never be used to decide comparability: two files named for the same region
/// and the same commit can be entirely different contracts, which is precisely
/// how a raw-vs-locations comparison gets mistaken for a code regression.
#[derive(Debug, Clone)]
pub struct Input {
    /// File name only, never a path - an absolute path would be machine
    /// dependent and break same-input reproducibility across hosts.
    pub name: String,
    /// XXH3-128 over the whole file, lowercase hex. Matches the `xxhash`
    /// field brokkr records per dataset variant.
    pub xxh3_128: String,
    pub bytes: u64,
    /// Replication timestamp from the PBF header, seconds since epoch. This
    /// describes the OSM snapshot, not the run, so it is reproducible.
    pub replication_timestamp: Option<i64>,
    pub features: PbfFeatures,
}

impl Input {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "xxh3_128": self.xxh3_128,
            "bytes": self.bytes,
            "replication_timestamp": self.replication_timestamp,
            "features": self.features.to_json(),
        })
    }
}

/// Ocean inputs as resolved for this run.
///
/// `mode` is part of the contract because the artifact path and the computed
/// shapefile path produce benignly different tiles for the same extract (see
/// the extract-consumption note in `pipeline/mod.rs`): descent seams depend on
/// each piece's clip extent. Comparing an artifact-active archive against an
/// artifact-absent one is therefore a contract mismatch, not a regression.
#[derive(Debug, Clone)]
pub struct OceanContract {
    /// `none`, `shapefile`, or `artifact`.
    pub mode: &'static str,
    /// False when `--no-ocean-simplify` built a verbatim coverage baseline.
    pub runtime_simplification: bool,
    /// `simplified` when a separate low-zoom shapefile serves z0-7, `full`
    /// when the full-resolution shapefile serves every zoom, `none` when
    /// ocean is disabled. Distinct from `runtime_simplification`: source
    /// selection and runtime simplification are independent, and a single
    /// boolean conflates them.
    pub low_zoom_source: &'static str,
    /// The durable artifact's invalidation key, when the artifact is active.
    pub artifact_key: Option<Value>,
}

impl OceanContract {
    fn to_json(&self) -> Value {
        json!({
            "mode": self.mode,
            "runtime_simplification": self.runtime_simplification,
            "low_zoom_source": self.low_zoom_source,
            "artifact_key": self.artifact_key,
        })
    }
}

/// What the run actually did, as opposed to what it was asked to do.
///
/// Diagnostic only, and deliberately outside the contract: given identical
/// `input` and `config` these are a pure function of the code, so gating on
/// them would refuse comparisons across revisions - exactly the comparisons a
/// regression gate exists to make. Change the path-selection logic and a
/// contract-gated consumer would refuse the diff precisely when it matters.
/// They are what explains a diff once the contract matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    /// `inline` when way elements carried coordinates, `node_store` when a
    /// node coordinate index was built.
    pub coordinate_source: &'static str,
    /// `injected_v1` when WayMembers-v1 supplied relation membership,
    /// `relation_scan` when it was recovered by scanning relations.
    pub way_members: &'static str,
    /// `injected_v1` when SharedNodePins-v1 supplied globally injected pins,
    /// `block_local` when shared nodes were detected per block. Pin scope
    /// changes which vertices survive simplification, so the two produce
    /// materially different geometry from the same OSM data.
    pub shared_node_pins: &'static str,
}

impl Effective {
    fn to_json(self) -> Value {
        json!({
            "coordinate_source": self.coordinate_source,
            "way_members": self.way_members,
            "shared_node_pins": self.shared_node_pins,
        })
    }
}

/// One repository in the build closure.
///
/// `dirty` is decisive for how much the commit is worth: a dirty tree means
/// the commit names the nearest ancestor of the code that ran, not the code
/// that ran. Recording the pair keeps that distinction visible instead of
/// letting a hash imply an identity it does not have. Either field may be
/// `unknown` when git could not be consulted at build time - honest, and the
/// only alternative to guessing.
fn repo_json(commit: &str, dirty: &str) -> Value {
    json!({
        "commit": commit,
        "dirty": match dirty {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::Null,
        },
    })
}

/// The code closure that produced the archive.
///
/// Reported, never equality-gated: comparing two archives built from
/// different revisions is the entire point of a regression gate, so a
/// consumer that refused on a build mismatch would refuse every real
/// comparison.
///
/// protohoggr appears only through `cargo_lock_xxh3_128`. It is a pinned
/// registry dependency, so the lockfile's content-addressed checksum
/// identifies it exactly; it needs no commit or dirty flag because it cannot
/// drift. pbfhogg is a path dependency and can, which is why it is named
/// separately.
fn build_json() -> Value {
    json!({
        "elivagar": repo_json(
            env!("ELIVAGAR_BUILD_ELIVAGAR_COMMIT"),
            env!("ELIVAGAR_BUILD_ELIVAGAR_DIRTY"),
        ),
        "pbfhogg_reader": repo_json(
            env!("ELIVAGAR_BUILD_PBFHOGG_COMMIT"),
            env!("ELIVAGAR_BUILD_PBFHOGG_DIRTY"),
        ),
        "cargo_lock_xxh3_128": env!("ELIVAGAR_BUILD_CARGO_LOCK_XXH3_128"),
        "cargo_features": env!("ELIVAGAR_BUILD_CARGO_FEATURES")
            .split(',')
            .filter(|f| !f.is_empty())
            .collect::<Vec<_>>(),
    })
}

fn tile_format_name(format: TilePayloadFormat) -> &'static str {
    match format {
        TilePayloadFormat::Mvt => "mvt",
        TilePayloadFormat::Mlt => "mlt",
    }
}

fn tile_compression_name(compression: TileCompression) -> &'static str {
    match compression {
        TileCompression::Gzip => "gzip",
        TileCompression::Brotli => "brotli",
    }
}

/// Per-layer values keyed by layer name, omitting zeros.
///
/// Layer arrays are indexed by `Layer` discriminant; emitting names rather
/// than positions keeps the metadata readable and survives a reordering of
/// the enum, which a positional array would silently misattribute.
fn layer_map(values: &[u32], zero_is_default: bool) -> Value {
    let mut map = serde_json::Map::new();
    for layer in Layer::ALL {
        let idx = layer as usize;
        let Some(&value) = values.get(idx) else {
            continue;
        };
        if zero_is_default && value == 0 {
            continue;
        }
        map.insert(layer.name().to_string(), json!(value));
    }
    Value::Object(map)
}

fn config_json(config: &TilegenConfig, ocean: &OceanContract) -> Value {
    let seam: Vec<u32> = config
        .seam_reconcile_layers
        .iter()
        .map(|&v| u32::from(v))
        .collect();
    json!({
        "profile": "shortbread",
        "min_zoom": config.min_zoom,
        "max_zoom": config.max_zoom,
        "tile": {
            "format": tile_format_name(config.tile_format),
            "compression": tile_compression_name(config.tile_compression),
            // The supplied level is a base, not a uniform setting: the encoder
            // clamps low zooms up and caps z13/z14 down. Recording the base
            // alone would misdescribe the output, so the policy that maps base
            // to per-zoom level is named and versioned alongside it.
            "base_compression_level": config.compression_level,
            "compression_policy": "zoom-v1",
        },
        "seam_reconcile_layers": layer_map(&seam, true),
        "fanout_caps": layer_map(&config.fanout_caps, true),
        "polygon_simplify_factor": config.polygon_simplify_factor,
        "ocean": ocean.to_json(),
    })
}

/// The config that determines what phase12 writes into sort chunks.
///
/// This is the subset a `--skip-to` run may NOT change, because the chunks on
/// disk were produced under it and cannot be reinterpreted: zoom range decides
/// which zooms have records at all, fanout caps drop features per zoom, and the
/// simplify factor and seam-reconcile zooms decide which vertices survive.
/// Resuming with any of these altered silently mixes two configs into one
/// archive and then records only the second.
///
/// Deliberately excludes the assemble-side settings - tile format, tile
/// compression, compression level, every memory budget. Those are applied after
/// the chunks are read, so a resumed run may legitimately change them, and
/// including them would reject safe resumes.
///
/// Compared as JSON rather than as a struct so the float factor needs no
/// float-equality dance, and so a mismatch can name the field that differs.
pub fn producer_config(config: &TilegenConfig) -> Value {
    let seam: Vec<u32> = config
        .seam_reconcile_layers
        .iter()
        .map(|&v| u32::from(v))
        .collect();
    json!({
        "min_zoom": config.min_zoom,
        "max_zoom": config.max_zoom,
        "fanout_caps": layer_map(&config.fanout_caps, true),
        "polygon_simplify_factor": config.polygon_simplify_factor,
        "seam_reconcile_layers": layer_map(&seam, true),
    })
}

/// Name the producer-config fields that differ, for a resume error.
pub fn producer_config_diff(checkpoint: &Value, current: &Value) -> Vec<String> {
    let mut diffs = Vec::new();
    let empty = serde_json::Map::new();
    let a = checkpoint.as_object().unwrap_or(&empty);
    let b = current.as_object().unwrap_or(&empty);
    for key in a.keys().chain(b.keys()) {
        let (was, now) = (a.get(key), b.get(key));
        if was != now && !diffs.iter().any(|d: &String| d.starts_with(key.as_str())) {
            diffs.push(format!(
                "{key} (chunks: {}, this run: {})",
                was.unwrap_or(&Value::Null),
                now.unwrap_or(&Value::Null)
            ));
        }
    }
    diffs
}

/// Build the `elivagar` metadata value.
///
/// `effective` is `None` when the run did not itself execute the PBF phase
/// (`--skip-to`) and the checkpoint could not supply what the original run
/// chose. The member is then omitted rather than guessed: re-deriving it from
/// the current config would describe what this invocation *would* have done,
/// not what produced the tiles on disk.
///
/// `resumed_from` names the phase a resumed run started at, and is absent for
/// a full run.
pub fn build(
    input: &Input,
    config: &TilegenConfig,
    ocean: &OceanContract,
    effective: Option<Effective>,
    resumed_from: Option<&str>,
) -> Value {
    let mut value = json!({
        "schema": SCHEMA_VERSION,
        "input": input.to_json(),
        "config": config_json(config, ocean),
        "build": build_json(),
        "execution": { "resumed_from": resumed_from },
    });
    if let Some(effective) = effective
        && let Some(obj) = value.as_object_mut()
    {
        obj.insert("effective".to_string(), effective.to_json());
    }
    value
}

/// Render a value as a `"elivagar":{...}` object member for
/// `PmtilesWriter::add_metadata_member`.
pub fn metadata_member(value: &Value) -> String {
    format!("\"elivagar\":{value}")
}

/// XXH3-128 of a file, lowercase hex, with its byte length.
///
/// mmap and hash in one pass. On a full run the PBF is already in page cache
/// from the read phase, so this costs page-cache bandwidth rather than disk
/// IO: ~0.1s for a 1.2 GB extract. Provenance, not pipeline, so it is not
/// inside any timed phase.
pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    // SAFETY: the input PBF is not modified during a run. A concurrent
    // truncation would be UB, and is the same assumption the PBF reader and
    // the way index already make about this file.
    let map = unsafe { memmap2::Mmap::map(&file)? };
    let digest = xxhash_rust::xxh3::xxh3_128(&map);
    Ok((format!("{digest:032x}"), len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features() -> PbfFeatures {
        PbfFeatures {
            sort_type_then_id: true,
            locations_on_ways: true,
            way_members_v1: true,
            shared_node_pins_v1: true,
        }
    }

    fn input() -> Input {
        Input {
            name: "denmark-locations-prepass.osm.pbf".to_string(),
            xxh3_128: "58c47f32d3a55b04a56813565efc78ac".to_string(),
            bytes: 531_544_047,
            replication_timestamp: Some(1_771_622_445),
            features: features(),
        }
    }

    #[test]
    fn metadata_member_is_a_json_object_member() {
        let value = json!({"schema": SCHEMA_VERSION});
        let member = metadata_member(&value);
        // Must splice into an existing object, so it is a `"key":value` pair
        // with no wrapping braces of its own.
        assert!(member.starts_with("\"elivagar\":{"));
        let wrapped = format!("{{{member}}}");
        let parsed: Value =
            serde_json::from_str(&wrapped).expect("member must splice into a valid object");
        assert_eq!(parsed["elivagar"]["schema"], SCHEMA_VERSION);
    }

    #[test]
    fn input_records_hash_and_observed_features() {
        let value = input().to_json();
        assert_eq!(value["xxh3_128"], "58c47f32d3a55b04a56813565efc78ac");
        assert_eq!(value["bytes"], 531_544_047_u64);
        assert_eq!(value["features"]["locations_on_ways"], true);
        assert_eq!(value["features"]["way_members_v1"], true);
        assert_eq!(value["features"]["shared_node_pins_v1"], true);
        assert_eq!(value["features"]["sort_type_then_id"], true);
    }

    #[test]
    fn raw_and_locations_inputs_differ_in_the_contract() {
        // The incident this schema exists to prevent: same region, same
        // commit, same everything a filename can express, but a different
        // contract. The two must not compare equal.
        let locations = input().to_json();
        let mut raw_input = input();
        raw_input.name = "denmark-raw.osm.pbf".to_string();
        raw_input.xxh3_128 = "aa5bb8650000000000000000deadbeef".to_string();
        raw_input.features = PbfFeatures {
            sort_type_then_id: true,
            ..PbfFeatures::default()
        };
        let raw = raw_input.to_json();
        assert_ne!(raw, locations);
        assert_ne!(raw["features"], locations["features"]);
        assert_ne!(raw["xxh3_128"], locations["xxh3_128"]);
    }

    #[test]
    fn layer_map_omits_defaults_and_keys_by_name() {
        let mut caps = [0_u32; Layer::count()];
        caps[Layer::Boundaries as usize] = 4096;
        let value = layer_map(&caps, true);
        let obj = value.as_object().expect("layer map must be an object");
        assert_eq!(obj.len(), 1, "zero-valued layers must be omitted");
        assert_eq!(value[Layer::Boundaries.name()], 4096);
    }

    #[test]
    fn hash_file_matches_xxh3_of_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("input.bin");
        let bytes = b"elivagar provenance".to_vec();
        std::fs::write(&path, &bytes).expect("write");
        let (hash, len) = hash_file(&path).expect("hash");
        assert_eq!(len, bytes.len() as u64);
        assert_eq!(
            hash,
            format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&bytes))
        );
        assert_eq!(hash.len(), 32, "must be 32 lowercase hex chars like brokkr");
    }
}
