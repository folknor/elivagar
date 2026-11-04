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

use crate::TilegenConfig;
use crate::ocean::Xxh128;
use crate::shortbread::Layer;
use serde_json::{Value, json};
use std::path::Path;

/// Schema version of the `elivagar` metadata member.
///
/// Bump when the meaning of an existing field changes. Adding a member does
/// not require a bump: readers ignore members they do not know, and the
/// contract comparison is defined over named fields, not over the whole
/// object.
pub const SCHEMA_VERSION: u32 = 1;

/// The archive contract consumed by the corpus gate. `input` and `config`
/// establish comparability. Build provenance is optional and diagnostic; it
/// is never part of equality gating.
#[derive(Debug, Clone)]
pub struct ContractDoc {
    pub input: Value,
    pub config: Value,
    /// Diagnostic build provenance, or `Value::Null` when absent.
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
    extract_contract_from_value(&metadata)
}

pub(crate) fn extract_contract_from_value(metadata: &Value) -> ContractState {
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
    ContractState::Contract(ContractDoc {
        input: input.clone(),
        config: config.clone(),
        build: object.get("build").cloned().unwrap_or(Value::Null),
    })
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

/// Stable provenance names for the paths selected from PBF header facts.
/// These same enums encode and decode checkpoint values, so their serde
/// renames are part of both wire formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CoordinateSource {
    #[serde(rename = "inline")]
    Inline,
    #[serde(rename = "node_store")]
    NodeStore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MembershipSource {
    #[serde(rename = "injected_v1")]
    InjectedV1,
    #[serde(rename = "relation_scan")]
    RelationScan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PinScope {
    #[serde(rename = "injected_v1")]
    InjectedV1,
    #[serde(rename = "block_local")]
    BlockLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ActiveLowZoomSource {
    #[serde(rename = "simplified")]
    Simplified,
    #[serde(rename = "full")]
    Full,
}

/// The serde representation shared by checkpoint and provenance. The
/// checkpoint also records this identity so a resume cannot reuse chunks from
/// a different ocean source.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum OceanContractKind {
    None,
    Shapefile {
        low_zoom_source: ActiveLowZoomSource,
        source_key: Value,
    },
    Artifact {
        low_zoom_source: ActiveLowZoomSource,
        artifact_key: Value,
    },
}

/// Content digest and byte length of the input PBF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputIdentity {
    pub xxh3_128: Xxh128,
    pub bytes: u64,
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
    pub identity: InputIdentity,
    /// Replication timestamp from the PBF header, seconds since epoch. This
    /// describes the OSM snapshot, not the run, so it is reproducible.
    pub replication_timestamp: Option<i64>,
    pub features: PbfFeatures,
}

impl Input {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "xxh3_128": self.identity.xxh3_128.to_string(),
            "bytes": self.identity.bytes,
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
    /// Resolved producer mode with its source identity and low-zoom selection.
    pub kind: OceanContractKind,
    /// Whether the runtime pyramid simplifier ran.
    pub runtime_simplification: bool,
}

impl OceanContract {
    fn to_json(&self) -> Value {
        let is_none = matches!(&self.kind, OceanContractKind::None);
        let has_artifact_key = matches!(&self.kind, OceanContractKind::Artifact { .. });
        let mut value =
            serde_json::to_value(&self.kind).expect("ocean contract is JSON serializable");
        // Preserve the established provenance nulls and omission rules. The
        // checkpoint uses the tagged kind directly without these diagnostics.
        if is_none {
            value["low_zoom_source"] = json!("none");
        }
        if !has_artifact_key {
            value["artifact_key"] = Value::Null;
        }
        value["runtime_simplification"] = json!(self.runtime_simplification);
        value
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Effective {
    /// Inline way coordinates or a node-store lookup.
    pub coordinate_source: CoordinateSource,
    /// Injected membership or the relation-scan fallback.
    pub way_members: MembershipSource,
    /// Injected shared-node pins or the block-local fallback.
    pub shared_node_pins: PinScope,
}

impl Effective {
    fn to_json(self) -> Value {
        serde_json::to_value(self).expect("effective paths are JSON serializable")
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

/// Fanout caps keyed by layer name, omitting unsupported layers and zeros.
///
/// Layer arrays are indexed by `Layer` discriminant; emitting names rather
/// than positions keeps the metadata readable and survives a reordering of
/// the enum, which a positional array would silently misattribute.
fn fanout_cap_map(values: &[u32]) -> Value {
    let mut map = serde_json::Map::new();
    for layer in Layer::ALL {
        if !layer.supports_fanout_cap() {
            continue;
        }
        let idx = layer as usize;
        let Some(&value) = values.get(idx) else {
            continue;
        };
        if value == 0 {
            continue;
        }
        map.insert(layer.name().to_string(), json!(value));
    }
    Value::Object(map)
}

fn config_json(config: &TilegenConfig, ocean: &OceanContract) -> Value {
    json!({
        "profile": "shortbread",
        "min_zoom": config.min_zoom,
        "max_zoom": config.max_zoom,
        "tile": crate::pipeline::TileContract::from_config(config).provenance_json(),
        "fanout_caps": fanout_cap_map(&config.fanout_caps),
        "polygon_simplify_factor": config.polygon_simplify_factor,
        "ocean": ocean.to_json(),
    })
}

/// The config that determines what phase12 writes into sort chunks.
///
/// This is the subset a `--skip-to` run may NOT change, because the chunks on
/// disk were produced under it and cannot be reinterpreted: zoom range decides
/// which zooms have records at all, fanout caps drop features per zoom, and the
/// simplify factor decides which vertices survive.
/// Resuming with any of these altered silently mixes two configs into one
/// archive and then records only the second.
///
/// Deliberately excludes the assemble-side settings - tile format, tile
/// compression, compression level, every memory budget, and the dedup cap.
/// Those are applied after the chunks are read, so a resumed run may
/// legitimately change them, and including them would reject safe resumes.
///
/// Compared as JSON rather than as a struct so the float factor needs no
/// float-equality dance, and so a mismatch can name the field that differs.
pub fn producer_config(config: &TilegenConfig) -> Value {
    json!({
        "min_zoom": config.min_zoom,
        "max_zoom": config.max_zoom,
        "fanout_caps": fanout_cap_map(&config.fanout_caps),
        "polygon_simplify_factor": config.polygon_simplify_factor,
    })
}

/// Name the producer-config fields that differ, for a resume error.
pub fn producer_config_diff(checkpoint: &Value, current: &Value) -> Vec<String> {
    let mut diffs = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    let empty = serde_json::Map::new();
    let a = checkpoint.as_object().unwrap_or(&empty);
    let b = current.as_object().unwrap_or(&empty);
    for key in a.keys().chain(b.keys()) {
        let (was, now) = (a.get(key), b.get(key));
        if was != now && seen.insert(key.as_str()) {
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
/// `effective` is omitted when the caller cannot supply the path choices that
/// produced the tiles. They are not guessed from current config, which would
/// describe what this invocation *would* have done instead.
///
/// `resumed_from` names the phase a resumed run started at, and is null for
/// a full run.
pub fn build(
    input: &Input,
    config: &TilegenConfig,
    ocean: &OceanContract,
    effective: Option<Effective>,
    resumed_from: Option<crate::pipeline::SkipTo>,
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

/// XXH3-128 of a file, lowercase hex, with its byte length.
///
/// Streamed through a fixed buffer rather than hashed over an mmap of the
/// whole file: this runs concurrently with phase12 on a full run (see
/// [`BackgroundHash`]), and a whole-file mapping would let file-backed pages
/// count into the process RSS the sidecar samples - a planet-sized input
/// would read as a planet-sized RSS spike in the phase whose memory the
/// 30 GB ledger watches most closely. Buffered reads populate the same page
/// cache without ever holding more than one buffer resident.
pub fn hash_file(path: &Path) -> std::io::Result<InputIdentity> {
    let (xxh3_128, bytes) = Xxh128::hash_file(path)?;
    Ok(InputIdentity { xxh3_128, bytes })
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
    handle: std::thread::JoinHandle<std::io::Result<InputIdentity>>,
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
    pub fn join(self) -> std::io::Result<InputIdentity> {
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
            identity: InputIdentity {
                xxh3_128: "58c47f32d3a55b04a56813565efc78ac"
                    .parse()
                    .expect("valid XXH3-128"),
                bytes: 531_544_047,
            },
            replication_timestamp: Some(1_771_622_445),
            features: features(),
        }
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
        raw_input.identity.xxh3_128 = "aa5bb8650000000000000000deadbeef"
            .parse()
            .expect("valid XXH3-128");
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
    fn typed_paths_and_resume_keep_their_wire_names() {
        for (effective, expected) in [
            (
                Effective {
                    coordinate_source: CoordinateSource::Inline,
                    way_members: MembershipSource::InjectedV1,
                    shared_node_pins: PinScope::InjectedV1,
                },
                r#"{"coordinate_source":"inline","shared_node_pins":"injected_v1","way_members":"injected_v1"}"#,
            ),
            (
                Effective {
                    coordinate_source: CoordinateSource::NodeStore,
                    way_members: MembershipSource::RelationScan,
                    shared_node_pins: PinScope::BlockLocal,
                },
                r#"{"coordinate_source":"node_store","shared_node_pins":"block_local","way_members":"relation_scan"}"#,
            ),
        ] {
            let value = effective.to_json();
            assert_eq!(value.to_string(), expected);
            assert_eq!(
                serde_json::from_value::<Effective>(value).expect("typed paths"),
                effective,
            );
        }
        for (phase, expected) in [
            (None, "null"),
            (Some(crate::pipeline::SkipTo::Ocean), "\"ocean\""),
            (Some(crate::pipeline::SkipTo::Sort), "\"sort\""),
            (Some(crate::pipeline::SkipTo::Assemble), "\"assemble\""),
        ] {
            assert_eq!(
                serde_json::to_string(&phase).expect("resume phase"),
                expected
            );
        }
    }

    #[test]
    fn no_ocean_contract_keeps_null_key_and_omits_source_key() {
        let ocean = OceanContract {
            kind: OceanContractKind::None,
            runtime_simplification: true,
        };
        assert_eq!(
            ocean.to_json().to_string(),
            r#"{"artifact_key":null,"low_zoom_source":"none","mode":"none","runtime_simplification":true}"#,
        );
    }

    #[test]
    fn artifact_contract_keeps_the_flat_json_shape() {
        // Keep the established wire schema local: corpus/ is excluded from
        // the published crate, whose unit tests must still compile.
        let mut expected = json!({
            "mode": "artifact",
            "runtime_simplification": true,
            "low_zoom_source": "simplified",
            "artifact_key": {
                "full_shp_xxh128": "c10be1c7843cf9e59bed0a44c1e232e7",
                "full_shx_xxh128": "c65e7ee5a2027b4e488f741119218a71",
                "simplified_shp_xxh128": "b3417e31c2873dcfd106aa2e7d5e3b2a",
                "simplified_shx_xxh128": "55c4a5d3476116d7b800122a074ba70b",
                "min_zoom": 0,
                "max_zoom": 14,
                "compression_level": 6,
                "policy_version": crate::ocean::OCEAN_POLICY_VERSION,
            },
        });
        let mut key = crate::ocean::OceanArtifactKey::from_json(&expected["artifact_key"])
            .expect("valid artifact key");
        let ocean = OceanContract {
            kind: OceanContractKind::Artifact {
                low_zoom_source: ActiveLowZoomSource::Simplified,
                artifact_key: key.to_json(),
            },
            runtime_simplification: true,
        };
        assert_eq!(
            serde_json::to_string(&ocean.to_json()).expect("serialized ocean contract"),
            serde_json::to_string(&expected).expect("serialized committed ocean contract"),
        );

        key.source.simplified_shp_xxh128 = None;
        key.source.simplified_shx_xxh128 = None;
        expected["artifact_key"]["simplified_shp_xxh128"] = Value::Null;
        expected["artifact_key"]["simplified_shx_xxh128"] = Value::Null;
        let ocean = OceanContract {
            kind: OceanContractKind::Artifact {
                low_zoom_source: ActiveLowZoomSource::Simplified,
                artifact_key: key.to_json(),
            },
            ..ocean
        };
        assert_eq!(
            serde_json::to_string(&ocean.to_json()).expect("serialized ocean contract"),
            serde_json::to_string(&expected).expect("serialized committed ocean contract"),
        );
    }

    #[test]
    fn fanout_cap_map_omits_defaults_and_unsupported_layers() {
        let mut caps = [0_u32; Layer::count()];
        caps[Layer::Buildings as usize] = 4096;
        caps[Layer::Ocean as usize] = 2048;
        let value = fanout_cap_map(&caps);
        let obj = value.as_object().expect("layer map must be an object");
        assert_eq!(
            obj.len(),
            1,
            "zero-valued and unsupported layers must be omitted"
        );
        assert_eq!(value[Layer::Buildings.name()], 4096);
        assert!(value.get(Layer::Ocean.name()).is_none());
    }

    #[test]
    fn hash_file_matches_xxh3_of_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("input.bin");
        let bytes = b"elivagar provenance".to_vec();
        std::fs::write(&path, &bytes).expect("write");
        let identity = hash_file(&path).expect("hash");
        assert_eq!(identity.bytes, bytes.len() as u64);
        assert_eq!(
            identity.xxh3_128.to_string(),
            format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&bytes))
        );
        assert_eq!(identity.xxh3_128.to_string().len(), 32);
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
        let identity = hash_file(&path).expect("hash");
        assert_eq!(identity.bytes, bytes.len() as u64);
        assert_eq!(
            identity.xxh3_128.to_string(),
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
        assert_eq!(background, synchronous);
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
        let ContractState::Contract(contract) =
            extract_contract(r#"{"elivagar": {"schema": 1, "input": {}, "config": {}}}"#)
        else {
            panic!("missing diagnostic build must not make the contract incomplete");
        };
        assert_eq!(contract.build, Value::Null);
    }
}
