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
//! Same-commit builds are byte-identical and the corpus digest is recomputed
//! from archive content; a per-run value here would break both.
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

/// The archive contract consumed by the corpus gate. Build provenance is kept
/// for diagnosis and corpus-rotation review, never comparison gating.
#[derive(Debug, Clone)]
pub struct ContractDoc {
    pub input: Value,
    pub config: Value,
    pub build: Value,
}

#[derive(Debug, Clone)]
pub enum ContractState {
    Contract(ContractDoc),
    Absent,
    Unavailable(String),
    Invalid,
    UnknownSchema(u64),
    Incomplete(&'static str),
}

pub fn extract_contract(metadata_json: &str) -> ContractState {
    let metadata: Value = match serde_json::from_str(metadata_json) {
        Ok(value) => value,
        Err(_) => return ContractState::Invalid,
    };
    let Some(elivagar) = metadata.get("elivagar") else {
        return ContractState::Absent;
    };
    let Some(object) = elivagar.as_object() else {
        return ContractState::Invalid;
    };
    let Some(schema) = object.get("schema").and_then(Value::as_u64) else {
        return ContractState::Incomplete("schema");
    };
    if schema != u64::from(SCHEMA_VERSION) {
        return ContractState::UnknownSchema(schema);
    }
    let Some(input) = object.get("input") else {
        return ContractState::Incomplete("input");
    };
    let Some(config) = object.get("config") else {
        return ContractState::Incomplete("config");
    };
    let Some(build) = object.get("build") else {
        return ContractState::Incomplete("build");
    };
    ContractState::Contract(ContractDoc {
        input: input.clone(),
        config: config.clone(),
        build: build.clone(),
    })
}

/// Return the named gated paths which differ. `input.name` and `build` are
/// intentionally diagnostic: archives from different revisions are what the
/// gate is meant to compare.
pub fn contract_diff(baseline: &ContractDoc, candidate: &ContractDoc) -> Vec<String> {
    let mut out = Vec::new();
    diff_json("config", &baseline.config, &candidate.config, &mut out);
    let mut left = baseline.input.clone();
    let mut right = candidate.input.clone();
    if let Some(value) = left.as_object_mut() {
        value.remove("name");
    }
    if let Some(value) = right.as_object_mut() {
        value.remove("name");
    }
    diff_json("input", &left, &right, &mut out);
    out
}

fn diff_json(path: &str, left: &Value, right: &Value, out: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<_> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                diff_json(
                    &format!("{path}.{key}"),
                    a.get(key).unwrap_or(&Value::Null),
                    b.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        _ if left != right => out.push(path.to_string()),
        _ => {}
    }
}

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
    /// Whether the ocean VW simplifier ran. Always true on archives built
    /// after `--no-ocean-simplify` was removed; false on older archives that
    /// used it to build a verbatim coverage baseline.
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
/// pbfhogg moved from a path dependency to a pinned registry dependency, so it
/// no longer has a git tree and carries no commit/dirty pair - only its locked
/// semver, which `cargo_lock_xxh3_128` pins exactly but as an opaque checksum a
/// reader cannot resolve to a version. protohoggr appears only through that
/// checksum for the same reason and needs no field of its own. elivagar remains
/// the one git tree here, so only elivagar carries a commit/dirty pair.
fn build_json() -> Value {
    json!({
        "elivagar": repo_json(
            env!("ELIVAGAR_BUILD_ELIVAGAR_COMMIT"),
            env!("ELIVAGAR_BUILD_ELIVAGAR_DIRTY"),
        ),
        "pbfhogg_reader": { "version": env!("ELIVAGAR_BUILD_PBFHOGG_VERSION") },
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
/// Streamed through a fixed buffer rather than hashed over an mmap of the
/// whole file: this runs concurrently with phase12 on a full run (see
/// [`BackgroundHash`]), and a whole-file mapping would let file-backed pages
/// count into the process RSS the sidecar samples - a planet-sized input
/// would read as a planet-sized RSS spike in the phase whose memory the
/// 30 GB ledger watches most closely. Buffered reads populate the same page
/// cache without ever holding more than one buffer resident.
pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0_u8; 8 << 20];
    let mut len: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    let digest = hasher.digest128();
    Ok((format!("{digest:032x}"), len))
}

/// The input-PBF identity hash running on its own thread, overlapped with the
/// phase that reads the same file.
///
/// Identifying the input means reading all of it, and doing that serially is
/// real wall: measured at ~15s on the 19 GB north-america input (the
/// PHASE12_END-to-OCEAN_START gap, 2026-07-15), because by the time phase12
/// has streamed the file its head is long evicted from page cache. Run
/// concurrently the hash hides behind phase12, which processes the same bytes
/// one to two orders of magnitude slower than the hash reads them; the
/// hasher's readahead also warms the cache ahead of the pipeline reader.
pub struct BackgroundHash {
    handle: std::thread::JoinHandle<std::io::Result<(String, u64)>>,
}

impl BackgroundHash {
    /// Start hashing `path` on a named background thread.
    pub fn spawn(path: std::path::PathBuf) -> Self {
        let handle = std::thread::Builder::new()
            .name("input-hash".to_string())
            .spawn(move || hash_file(&path))
            .expect("spawning the input-hash thread");
        Self { handle }
    }

    /// Block until the hash is done and return it. Any residual wait here is
    /// the caller's to instrument; a panic on the hasher thread propagates as
    /// an I/O-shaped error rather than being swallowed.
    pub fn join(self) -> std::io::Result<(String, u64)> {
        self.handle.join().map_err(|_| {
            std::io::Error::other("input-hash thread panicked while hashing the input PBF")
        })?
    }
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

    #[test]
    fn hash_file_streams_across_buffer_boundaries() {
        // Content deliberately larger than one 8 MiB read and not a multiple
        // of it, so the streaming hasher crosses a buffer boundary and
        // finishes on a partial read. Must match the one-shot digest.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("input.bin");
        let mut bytes = vec![0_u8; (8 << 20) + 4097];
        for (i, b) in bytes.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            {
                *b = (i % 251) as u8;
            }
        }
        std::fs::write(&path, &bytes).expect("write");
        let (hash, len) = hash_file(&path).expect("hash");
        assert_eq!(len, bytes.len() as u64);
        assert_eq!(
            hash,
            format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&bytes))
        );
    }

    #[test]
    fn background_hash_matches_synchronous_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("input.bin");
        std::fs::write(&path, b"elivagar provenance").expect("write");
        let background = BackgroundHash::spawn(path.clone()).join().expect("join");
        let synchronous = hash_file(&path).expect("hash");
        assert_eq!(background.0, synchronous.0);
        assert_eq!(background.1, synchronous.1);
    }

    #[test]
    fn background_hash_join_reports_missing_file() {
        let err = BackgroundHash::spawn(std::path::PathBuf::from(
            "/nonexistent/elivagar-provenance-test.pbf",
        ))
        .join()
        .expect_err("missing file must error");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    fn contract_metadata() -> String {
        json!({
            "elivagar": {
                "schema": SCHEMA_VERSION,
                "input": {"name": "denmark", "xxh3_128": "ab", "features": {"locations_on_ways": true}},
                "config": {"ocean": {"artifact_key": {"policy_version": 2}}, "fanout_cap": 8},
                "build": {"elivagar": {"commit": "abc", "dirty": false}}
            }
        })
        .to_string()
    }

    #[test]
    fn extract_contract_names_every_no_contract_case() {
        assert!(matches!(
            extract_contract("not json"),
            ContractState::Invalid
        ));
        assert!(matches!(extract_contract("{}"), ContractState::Absent));
        assert!(matches!(
            extract_contract(r#"{"elivagar": 7}"#),
            ContractState::Invalid
        ));
        assert!(matches!(
            extract_contract(r#"{"elivagar": {}}"#),
            ContractState::Incomplete("schema")
        ));
        assert!(matches!(
            extract_contract(r#"{"elivagar": {"schema": 999}}"#),
            ContractState::UnknownSchema(999)
        ));
        assert!(matches!(
            extract_contract(r#"{"elivagar": {"schema": 1, "config": {}, "build": {}}}"#),
            ContractState::Incomplete("input")
        ));
        assert!(matches!(
            extract_contract(&contract_metadata()),
            ContractState::Contract(_)
        ));
    }

    #[test]
    fn contract_diff_gates_config_and_input_but_never_name_or_build() {
        let ContractState::Contract(base) = extract_contract(&contract_metadata()) else {
            panic!("baseline must parse");
        };
        // Identical contract: no diffs.
        assert!(contract_diff(&base, &base).is_empty());

        // A name-only change and a build-only change are both ignored.
        let mut named = base.clone();
        named.input["name"] = json!("germany");
        named.build["elivagar"]["commit"] = json!("def");
        assert!(contract_diff(&base, &named).is_empty());

        // The ocean policy version is gated and named by path.
        let mut policy = base.clone();
        policy.config["ocean"]["artifact_key"]["policy_version"] = json!(1);
        let diffs = contract_diff(&base, &policy);
        assert_eq!(
            diffs,
            vec!["config.ocean.artifact_key.policy_version".to_string()]
        );
    }
}
