//! PMTiles v3 archive inspector.
//!
//! Reads the 127-byte header and optional gzip-compressed metadata from a
//! PMTiles file and prints a human-readable summary.

use std::io::{self, Write};
use std::path::Path;

use crate::archive::header::e7_to_degrees;
use crate::archive::{Archive, Clustering, HEADER_LEN, Header, TileType};

/// Inspect a PMTiles file and print its header and metadata.
pub fn inspect(path: &Path) -> Result<(), crate::PipelineError> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    inspect_to_writer(path, &mut out)
}

#[allow(clippy::too_many_lines)]
fn inspect_to_writer(path: &Path, out: &mut dyn Write) -> Result<(), crate::PipelineError> {
    let reader = Archive::open(path)?;
    let file_size = reader.file_len();
    let h = *reader.header();

    // Tile counts.
    let num_addressed = h.addressed_tiles;
    let num_entries = h.tile_entries;
    let unique_tiles = h.tile_contents;

    // Flags.
    let clustered = match h.clustering {
        Clustering::Clustered => "true".to_string(),
        Clustering::Unclustered => "false".to_string(),
        Clustering::Other(byte) => format!("unrecognized ({byte})"),
    };
    let internal_compression = h.internal_compression.to_string();
    let tile_compression = h.tile_compression.to_string();
    let tile_type = h.tile_type.to_string();
    let mvt_validation = match reader.tile_decoder() {
        Ok(decoder) => format!("supported ({})", decoder.label()),
        Err(error) => format!("unsupported: {error}"),
    };

    // Zoom + bounds.
    let min_zoom = h.min_zoom;
    let max_zoom = h.max_zoom;
    let min_lon = e7_to_degrees(h.bounds.west);
    let min_lat = e7_to_degrees(h.bounds.south);
    let max_lon = e7_to_degrees(h.bounds.east);
    let max_lat = e7_to_degrees(h.bounds.north);
    let center_zoom = h.center.zoom;
    let center_lon = e7_to_degrees(h.center.lon);
    let center_lat = e7_to_degrees(h.center.lat);

    let dedup_count = num_addressed.saturating_sub(unique_tiles);
    let dedup_pct = if num_addressed > 0 {
        (dedup_count as f64 / num_addressed as f64) * 100.0
    } else {
        0.0
    };

    let metadata_state = read_metadata_state(&reader, h.metadata.length);
    let metadata_json = metadata_state.json().map(ToOwned::to_owned);
    let metadata_value = metadata_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok());
    let metadata_payload_format = metadata_value
        .as_ref()
        .and_then(|doc| extract_json_string(doc, "tile_payload_format"));
    let metadata_tile_compression = metadata_value
        .as_ref()
        .and_then(|doc| extract_json_string(doc, "tile_compression"));
    let (
        payload_format_display,
        payload_compression_display,
        payload_format_src,
        payload_compress_src,
    ) = payload_contract_display(
        h.tile_type,
        &tile_compression,
        metadata_payload_format.as_deref(),
        metadata_tile_compression.as_deref(),
    );

    writeln!(out, "PMTiles v3  {}", path.display())?;
    writeln!(out, "  File size:  {}", format_bytes(file_size))?;
    writeln!(out)?;
    writeln!(out, "  Tile type:          {tile_type}")?;
    writeln!(out, "  Tile compression:   {tile_compression}")?;
    writeln!(out, "  MVT validation:     {mvt_validation}")?;
    writeln!(out, "  Internal compress:  {internal_compression}")?;
    writeln!(
        out,
        "  Payload format:     {payload_format_display} ({payload_format_src})"
    )?;
    writeln!(
        out,
        "  Payload compress:   {payload_compression_display} ({payload_compress_src})"
    )?;
    writeln!(out, "  Clustered:          {clustered}")?;
    writeln!(out)?;
    writeln!(out, "  Zoom:    {min_zoom}..{max_zoom}")?;
    writeln!(
        out,
        "  Bounds:  [{min_lon:.7}, {min_lat:.7}] to [{max_lon:.7}, {max_lat:.7}]"
    )?;
    writeln!(
        out,
        "  Center:  [{center_lon:.7}, {center_lat:.7}] z{center_zoom}"
    )?;
    writeln!(out)?;
    writeln!(out, "  Tiles addressed:  {num_addressed:>14}")?;
    writeln!(out, "  Unique tiles:     {unique_tiles:>14}")?;
    writeln!(
        out,
        "  Deduplicated:     {dedup_count:>14} ({dedup_pct:.1}%)"
    )?;
    writeln!(out, "  Directory entries: {num_entries:>13}")?;
    print_section_layout(out, &h)?;

    print_provenance(out, &metadata_state, metadata_value.as_ref())?;

    if let Some(json) = metadata_json {
        writeln!(out)?;
        writeln!(out, "  Metadata:")?;
        // Preserve short metadata verbatim and summarize long layer lists.
        print_metadata_json(out, &json, metadata_value.as_ref())?;
    }

    if let MetadataState::Unreadable(error) = metadata_state {
        return Err(error.into());
    }
    Ok(())
}

/// Metadata is not read above this size. A tile archive's metadata is a layer
/// list and a provenance block; anything larger is not something to page into
/// memory on an inspect.
const MAX_METADATA_READ: u64 = 10 * 1024 * 1024;

/// Why the metadata is or is not available.
///
/// The three failures are kept apart rather than collapsed into one `None`,
/// because provenance must be able to say WHICH of them happened. "No block"
/// and "the block could not be read" are different facts about an archive, and
/// reporting either as silence is what this whole section exists to prevent.
enum MetadataState {
    Json(String),
    /// The archive stores no metadata at all.
    Absent,
    /// Present but too large to read here.
    TooLarge(u64),
    /// Present, sized sanely, and the read or decompression failed.
    Unreadable(crate::DecodeError),
}

impl MetadataState {
    fn json(&self) -> Option<&str> {
        match self {
            Self::Json(j) => Some(j),
            _ => None,
        }
    }
}

fn read_metadata_state(reader: &Archive, metadata_length: u64) -> MetadataState {
    if metadata_length == 0 {
        return MetadataState::Absent;
    }
    if metadata_length > MAX_METADATA_READ {
        return MetadataState::TooLarge(metadata_length);
    }
    match reader.metadata() {
        Ok(json) => MetadataState::Json(json),
        Err(error) => MetadataState::Unreadable(error),
    }
}

/// Print a summary of the `elivagar` provenance member.
///
/// Prints the WHOLE comparability contract - `input` plus every field of
/// `config` - not a selection from it. A partial contract summary is worse
/// than none: two archives differing only in `polygon_simplify_factor` or a
/// fanout cap would display identically, and a reader who has been told these
/// lines are the contract would conclude a geometry diff between them means
/// something about the code. The contract is cheap to print in full because
/// `layer_map` omits zeros, so the fanout map is a line at most.
/// `build`, `effective` and `execution` follow as diagnostics, which
/// explain a diff once the contract matches and must never be equality-gated.
///
/// Every path that cannot produce a contract says which one it is. An archive
/// with no block, a block this build cannot interpret, and metadata that could
/// not be read are three different facts, and reporting any of them as silence
/// would read as "nothing to report" - the exact ambiguity this section exists
/// to remove.
///
/// Reporting is not enforcement here: `inspect` prints the block, it never
/// compares it against a reference. The comparison - reading this contract
/// back and refusing a mismatch before content is walked - lives in the gate,
/// which now decodes elivagar in-process from brokkr rather than in this
/// binary. See reference/metadata.md.
fn print_provenance(
    out: &mut dyn Write,
    state: &MetadataState,
    metadata: Option<&serde_json::Value>,
) -> io::Result<()> {
    writeln!(out)?;
    let contract_state = match state {
        MetadataState::Json(_) => metadata.map_or(
            crate::provenance::ContractState::Invalid,
            crate::provenance::extract_contract_from_value,
        ),
        MetadataState::Absent => {
            crate::provenance::ContractState::Unavailable("archive stores no metadata".to_string())
        }
        MetadataState::TooLarge(len) => crate::provenance::ContractState::Unavailable(format!(
            "metadata is {}, over the {} read limit",
            format_bytes(*len),
            format_bytes(MAX_METADATA_READ),
        )),
        MetadataState::Unreadable(_) => crate::provenance::ContractState::Unavailable(
            "metadata could not be read or decompressed".to_string(),
        ),
    };

    match contract_state {
        crate::provenance::ContractState::Unavailable(reason) => {
            writeln!(out, "  Provenance:  unavailable - {reason}")
        }
        crate::provenance::ContractState::Absent => writeln!(
            out,
            "  Provenance:  absent - archive predates the elivagar metadata block"
        ),
        crate::provenance::ContractState::Invalid => {
            let reason = if metadata.is_none() {
                "metadata is not JSON"
            } else {
                "elivagar block is not an object"
            };
            writeln!(out, "  Provenance:  invalid - {reason}")
        }
        crate::provenance::ContractState::UnknownSchema(schema) => writeln!(
            out,
            "  Provenance:  schema {schema} - this build understands {}; not interpreted",
            crate::provenance::SCHEMA_VERSION,
        ),
        crate::provenance::ContractState::Incomplete(missing) => {
            if missing == "schema" {
                writeln!(
                    out,
                    "  Provenance:  INCOMPLETE - block is missing schema version"
                )
            } else {
                writeln!(
                    out,
                    "  Provenance:  schema {}",
                    crate::provenance::SCHEMA_VERSION
                )?;
                writeln!(
                    out,
                    "    Contract:   INCOMPLETE - block is missing {missing}"
                )
            }
        }
        crate::provenance::ContractState::Contract(contract) => {
            writeln!(
                out,
                "  Provenance:  schema {}",
                crate::provenance::SCHEMA_VERSION
            )?;
            print_contract(out, &contract.input, &contract.config)?;
            // The shared provenance decoder owns the comparability-state
            // rules. Remaining fields are diagnostics, so render them without
            // making their presence part of contract completeness.
            if let Some(p) = metadata.and_then(|doc| doc.get("elivagar")) {
                print_diagnostics(out, p)?;
            }
            Ok(())
        }
    }
}

/// The comparability contract: `input` plus every field of `config`.
fn print_contract(
    out: &mut dyn Write,
    input: &serde_json::Value,
    config: &serde_json::Value,
) -> io::Result<()> {
    let name = json_str(input.get("name"));
    writeln!(out, "    Input:      {name}")?;
    let hash = json_str(input.get("xxh3_128"));
    match input.get("bytes").and_then(serde_json::Value::as_u64) {
        Some(bytes) => writeln!(out, "                xxh3 {hash}  {}", format_bytes(bytes))?,
        None => writeln!(out, "                xxh3 {hash}")?,
    }
    // The PBF header features, not the filename: these decide which
    // coordinate, membership and pin paths the run actually took, and a
    // name like "-locations-prepass" is a label that can lie.
    writeln!(
        out,
        "                features: {}",
        pbf_feature_list(input.get("features"))
    )?;

    let profile = json_str(config.get("profile"));
    let min_zoom = json_num(config.get("min_zoom"));
    let max_zoom = json_num(config.get("max_zoom"));
    let simplify = config
        .get("polygon_simplify_factor")
        .and_then(serde_json::Value::as_f64)
        .map_or_else(|| "unknown".to_string(), |v| format!("x{v}"));
    writeln!(
        out,
        "    Config:     {profile}, z{min_zoom}-z{max_zoom}, polygon simplify {simplify}"
    )?;
    if let Some(tile) = config.get("tile") {
        writeln!(
            out,
            "                tile: {} {}, base level {}, policy {}",
            json_str(tile.get("format")),
            json_str(tile.get("compression")),
            json_num(tile.get("base_compression_level")),
            json_str(tile.get("compression_policy")),
        )?;
    }
    writeln!(
        out,
        "                fanout: {}",
        layer_map_display(config.get("fanout_caps"))
    )?;

    if let Some(ocean) = config.get("ocean") {
        writeln!(
            out,
            "    Ocean:      {}, low zoom {}, simplifier {}",
            json_str(ocean.get("mode")),
            json_str(ocean.get("low_zoom_source")),
            match ocean
                .get("runtime_simplification")
                .and_then(serde_json::Value::as_bool)
            {
                Some(true) => "on",
                Some(false) => "off",
                None => "unknown",
            },
        )?;
        // The shapefile identities live only here, so an artifact-active
        // archive's contract is incomplete without them.
        if let Some(key) = ocean.get("artifact_key").filter(|k| !k.is_null()) {
            writeln!(
                out,
                "                key: shp {} simplified {} level {} policy {}",
                abbreviate_hash(&json_str(key.get("full_shp_xxh128"))),
                abbreviate_hash(&json_str(key.get("simplified_shp_xxh128"))),
                json_num(key.get("compression_level")),
                json_num(key.get("policy_version")),
            )?;
        }
    }

    Ok(())
}

/// `build`, `effective` and `execution`: what explains a diff once the
/// contract matches. These are diagnostics, so a missing group is omitted
/// without making the comparability contract incomplete. `resumed_from` is
/// absent on a full run.
fn print_diagnostics(out: &mut dyn Write, p: &serde_json::Value) -> io::Result<()> {
    if let Some(build) = p.get("build") {
        let elivagar = repo_display(build.get("elivagar"));
        let pbfhogg = json_str(
            build
                .get("pbfhogg_reader")
                .and_then(|reader| reader.get("version")),
        );
        writeln!(
            out,
            "    Build:      elivagar {elivagar}, pbfhogg {pbfhogg}"
        )?;
    }

    if let Some(effective) = p.get("effective") {
        writeln!(
            out,
            "    Effective:  coords {}, way members {}, pins {}",
            json_str(effective.get("coordinate_source")),
            json_str(effective.get("way_members")),
            json_str(effective.get("shared_node_pins")),
        )?;
    }

    if let Some(phase) = p
        .get("execution")
        .and_then(|e| e.get("resumed_from"))
        .and_then(serde_json::Value::as_str)
    {
        writeln!(out, "    Resumed:    from {phase}")?;
    }

    Ok(())
}

/// A JSON string field, or `unknown` when absent, null, or not a string.
fn json_str(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

/// A JSON number field, or `unknown`.
fn json_num(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(serde_json::Value::as_u64)
        .map_or_else(|| "unknown".to_string(), |v| v.to_string())
}

/// A `layer_map` object rendered as `name=value`, or `none` when empty.
///
/// `layer_map` omits zeros, so an empty object means every layer is at its
/// default - which is why the whole contract fits on a few lines.
fn layer_map_display(value: Option<&serde_json::Value>) -> String {
    let Some(map) = value.and_then(serde_json::Value::as_object) else {
        return "unknown".to_string();
    };
    if map.is_empty() {
        return "none".to_string();
    }
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The true PBF header features, comma separated, or `none`.
fn pbf_feature_list(features: Option<&serde_json::Value>) -> String {
    let Some(features) = features else {
        return "unknown".to_string();
    };
    let names = [
        ("sort_type_then_id", "sorted"),
        ("locations_on_ways", "locations-on-ways"),
        ("way_members_v1", "way-members-v1"),
        ("shared_node_pins_v1", "shared-node-pins-v1"),
    ];
    // Every flag must be present. A missing one defaulted to false would let a
    // malformed block print "features: none", which is a positive claim about
    // the PBF - a default wearing a fact's clothes, and the exact shape of
    // every other bug this pipeline has had.
    let mut set = Vec::new();
    for (key, label) in names {
        match features.get(key).and_then(serde_json::Value::as_bool) {
            Some(true) => set.push(label),
            Some(false) => {}
            None => return format!("unknown - block declares no {key}"),
        }
    }
    if set.is_empty() {
        "none".to_string()
    } else {
        set.join(", ")
    }
}

/// `<commit>`, or `<commit> (dirty)` - a dirty tree means the commit names the
/// nearest ancestor of the code that ran, not the code that ran, so the flag is
/// what decides how much the hash is worth.
///
/// The hash is abbreviated for reading. The block stores it in full and that
/// remains the identity; this line is a summary, and 12 hex digits is what a
/// human matches against a git log.
fn repo_display(repo: Option<&serde_json::Value>) -> String {
    let Some(repo) = repo else {
        return "unknown".to_string();
    };
    let commit = repo
        .get("commit")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let commit = abbreviate_hash(commit);
    match repo.get("dirty").and_then(serde_json::Value::as_bool) {
        Some(true) => format!("{commit} (dirty)"),
        Some(false) => commit,
        None => format!("{commit} (dirty unknown)"),
    }
}

/// First 12 characters of a hex hash, unchanged if it is shorter or is a
/// non-hex sentinel such as `unknown`.
fn abbreviate_hash(hash: &str) -> String {
    if hash.len() > 12 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        hash[..12].to_string()
    } else {
        hash.to_string()
    }
}

/// Print metadata JSON in a readable format: extract vector_layers names and
/// zoom ranges.
fn print_metadata_json(
    out: &mut dyn Write,
    json: &str,
    metadata: Option<&serde_json::Value>,
) -> io::Result<()> {
    // Print the raw JSON indented if it's short, otherwise summarize.
    if json.len() < 500 {
        for line in json.lines() {
            writeln!(out, "    {line}")?;
        }
        return Ok(());
    }

    let Some(layers) = metadata
        .and_then(|doc| doc.get("vector_layers"))
        .and_then(serde_json::Value::as_array)
    else {
        for line in json.lines() {
            writeln!(out, "    {line}")?;
        }
        return Ok(());
    };

    let layers = layers
        .iter()
        .filter(|layer| {
            layer
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some()
        })
        .collect::<Vec<_>>();
    writeln!(out, "    Layers: {}", layers.len())?;
    for layer in layers {
        let Some(name) = layer.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let minzoom = extract_number(layer.get("minzoom"));
        let maxzoom = extract_number(layer.get("maxzoom"));
        match (minzoom, maxzoom) {
            (Some(mn), Some(mx)) => writeln!(out, "      {name:<24} z{mn}..{mx}")?,
            _ => writeln!(out, "      {name}")?,
        }
    }
    Ok(())
}

/// Read a JSON string member when it is actually a string.
fn extract_json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

/// Read an unsigned JSON zoom value that fits in the metadata's zoom range.
fn extract_number(value: Option<&serde_json::Value>) -> Option<u8> {
    u8::try_from(value?.as_u64()?).ok()
}

fn infer_payload_format_from_header(tile_type: TileType) -> &'static str {
    match tile_type {
        TileType::Mvt => "mvt",
        _ => "unknown",
    }
}

fn print_section_layout(out: &mut dyn Write, h: &Header) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  Section layout:")?;
    writeln!(
        out,
        "    Header:      {:>13}  offset {:>13}",
        format_bytes(HEADER_LEN as u64),
        0,
    )?;
    // File order of elivagar's writer, not header field order.
    for (label, section) in [
        ("Root dir: ", h.root_dir),
        ("Tile data:", h.tile_data),
        ("Metadata: ", h.metadata),
        ("Leaf dirs:", h.leaf_dirs),
    ] {
        if label == "Leaf dirs:" && section.length == 0 {
            continue;
        }
        writeln!(
            out,
            "    {label}   {:>13}  offset {:>13}",
            format_bytes(section.length),
            section.offset,
        )?;
    }
    Ok(())
}

fn payload_contract_display(
    tile_type: TileType,
    header_tile_compression: &str,
    metadata_payload_format: Option<&str>,
    metadata_tile_compression: Option<&str>,
) -> (String, String, &'static str, &'static str) {
    let payload_format_src = if metadata_payload_format.is_some() {
        "metadata"
    } else {
        "header"
    };
    let payload_compress_src = if metadata_tile_compression.is_some() {
        "metadata"
    } else {
        "header"
    };
    let payload_format_display = metadata_payload_format
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| infer_payload_format_from_header(tile_type).to_string());
    let payload_compression_display = metadata_tile_compression
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| header_tile_compression.to_string());
    (
        payload_format_display,
        payload_compression_display,
        payload_format_src,
        payload_compress_src,
    )
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::TileType;
    use super::{extract_json_string, infer_payload_format_from_header, payload_contract_display};
    use super::{inspect, inspect_to_writer};
    use crate::archive::{Archive, Header, LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter};
    use crate::tiling::{TileId, zoom_ids};
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;

    fn add_monotonic_unique_tiles(writer: &mut PmtilesWriter, z: u8, count: usize) {
        let base = zoom_ids(z).0;
        for i in 0..count {
            let tile_id = base + i as u64;
            let (z2, x, y) = TileId::new(tile_id).expect("test value").coord();
            assert_eq!(z2, z);
            let payload = (i as u64).to_le_bytes();
            writer.add_tile(z2, x, y, &payload).expect("test value");
        }
    }

    fn inspect_output(path: &Path) -> String {
        let mut out = Vec::new();
        inspect_to_writer(path, &mut out).expect("test value");
        String::from_utf8(out).expect("test value")
    }

    fn corrupt_metadata_payload(path: &Path) -> u64 {
        let reader = Archive::open(path).expect("test value");
        let metadata_offset = reader.header().metadata.offset;
        let metadata_length = reader.header().metadata.length;
        assert!(metadata_length > 0, "test archive must contain metadata");

        let mut file = OpenOptions::new()
            .write(true)
            .open(path)
            .expect("test value");
        file.seek(SeekFrom::Start(metadata_offset))
            .expect("test value");
        let byte_count = metadata_length.min(64) as usize;
        let junk = vec![0xFF; byte_count];
        file.write_all(&junk).expect("test value");
        metadata_length
    }

    fn edit_header(path: &Path, edit: impl FnOnce(&mut Header)) {
        let mut header = *Archive::open(path).expect("test value").header();
        edit(&mut header);
        let mut file = OpenOptions::new()
            .write(true)
            .open(path)
            .expect("test value");
        file.write_all(&header.encode()).expect("test value");
    }

    fn world(min_zoom: u8, max_zoom: u8, center_zoom: u8) -> PmtilesConfig {
        PmtilesConfig::new(
            min_zoom,
            max_zoom,
            LonLatBbox {
                west: -180.0,
                south: -85.0,
                east: 180.0,
                north: 85.0,
            },
            MapCenter {
                lon: 0.0,
                lat: 0.0,
                zoom: center_zoom,
            },
        )
        .expect("test value")
    }

    #[test]
    fn inspect_handles_root_only_archive() {
        let config = world(0, 0, 0);
        let mut writer = PmtilesWriter::new(config);
        writer.add_tile(0, 0, 0, &[1, 2, 3]).expect("test value");

        let dir = tempfile::tempdir().expect("test value");
        let path = dir.path().join("inspect_root_only.pmtiles");
        writer.write_to(&path).expect("test value");

        inspect(&path).expect("test value");
    }

    #[test]
    fn inspect_root_only_output_includes_header_and_section_layout() {
        let config = world(0, 0, 0);
        let mut writer = PmtilesWriter::new(config);
        writer.add_tile(0, 0, 0, &[1, 2, 3]).expect("test value");

        let dir = tempfile::tempdir().expect("test value");
        let path = dir.path().join("inspect_root_only_output.pmtiles");
        writer.write_to(&path).expect("test value");

        let output = inspect_output(&path);
        assert!(output.contains("PMTiles v3"));
        assert!(output.contains("Tile type:"));
        assert!(output.contains("Zoom:"));
        assert!(output.contains("Section layout:"));
        assert!(output.contains("Header:"));
        assert!(output.contains("Root dir:"));
        assert!(output.contains("Metadata:"));
        assert!(output.contains("Tile data:"));
        assert!(!output.contains("Leaf dirs:"));
    }

    #[test]
    fn inspect_handles_leaf_directory_archive() {
        // Above MAX_ROOT_ENTRIES (16384) to force leaf directory layout.
        const ROOT_THRESHOLD: usize = 16384;
        let config = world(8, 8, 8);
        let mut writer = PmtilesWriter::new(config);
        add_monotonic_unique_tiles(&mut writer, 8, ROOT_THRESHOLD + 1);

        let dir = tempfile::tempdir().expect("test value");
        let path = dir.path().join("inspect_leaf.pmtiles");
        writer.write_to(&path).expect("test value");

        inspect(&path).expect("test value");
    }

    #[test]
    fn inspect_leaf_output_includes_leaf_dirs_section() {
        const ROOT_THRESHOLD: usize = 16384;
        let config = world(8, 8, 8);
        let mut writer = PmtilesWriter::new(config);
        add_monotonic_unique_tiles(&mut writer, 8, ROOT_THRESHOLD + 1);

        let dir = tempfile::tempdir().expect("test value");
        let path = dir.path().join("inspect_leaf_output.pmtiles");
        writer.write_to(&path).expect("test value");

        let output = inspect_output(&path);
        assert!(output.contains("Section layout:"));
        assert!(output.contains("Leaf dirs:"));
    }

    #[test]
    fn inspect_unreadable_metadata_uses_header_payload_fallback() {
        let config = world(0, 0, 0);
        let mut writer = PmtilesWriter::new(config);
        writer.add_tile(0, 0, 0, &[7, 8, 9]).expect("test value");

        let dir = tempfile::tempdir().expect("test value");
        let path = dir.path().join("inspect_bad_metadata.pmtiles");
        writer.write_to(&path).expect("test value");

        let metadata_length = corrupt_metadata_payload(&path);
        assert!(metadata_length > 0);

        let mut out = Vec::new();
        let error = inspect_to_writer(&path, &mut out).expect_err("bad metadata rejected");
        assert_eq!(error.kind(), crate::ErrorKind::InputRefused);
        let output = String::from_utf8(out).expect("text output");
        assert!(output.contains("Payload format:"));
        assert!(output.contains("Payload compress:"));
        assert!(output.contains("Payload format:     mvt (header)"));
        assert!(output.contains("(header)"));
        assert!(!output.contains("  Metadata:\n"));
    }

    #[test]
    fn inspect_absent_metadata_uses_header_payload_fallback() {
        let config = world(0, 0, 0);
        let mut writer = PmtilesWriter::new(config);
        writer.add_tile(0, 0, 0, &[9, 9, 9]).expect("test value");

        let dir = tempfile::tempdir().expect("test value");
        let path = dir.path().join("inspect_no_metadata.pmtiles");
        writer.write_to(&path).expect("test value");

        edit_header(&path, |header| header.metadata.length = 0);

        let output = inspect_output(&path);
        assert!(output.contains("Payload format:     mvt (header)"));
        assert!(output.contains("Payload compress:   gzip (header)"));
        assert!(!output.contains("  Metadata:\n"));
    }

    fn provenance_of(json: &str) -> String {
        let mut out = Vec::new();
        let metadata = serde_json::from_str::<serde_json::Value>(json).ok();
        super::print_provenance(
            &mut out,
            &super::MetadataState::Json(json.to_string()),
            metadata.as_ref(),
        )
        .expect("test value");
        String::from_utf8(out).expect("test value")
    }

    fn provenance_output(member: &str) -> String {
        provenance_of(&format!(
            "{{\"name\":\"Shortbread\",\"elivagar\":{member}}}"
        ))
    }

    /// Every field of the contract, so the reader can compare two archives on
    /// all of it rather than on a selection that hides the field they differ in.
    const FULL_BLOCK: &str = r#"{
        "schema": 1,
        "input": {
            "name": "denmark-locations-prepass.osm.pbf",
            "xxh3_128": "aa5bb865deadbeef",
            "bytes": 2048,
            "features": {
                "sort_type_then_id": true,
                "locations_on_ways": true,
                "way_members_v1": true,
                "shared_node_pins_v1": false
            }
        },
        "config": {
            "profile": "shortbread",
            "min_zoom": 0,
            "max_zoom": 14,
            "tile": {
                "format": "mvt",
                "compression": "gzip",
                "base_compression_level": 6,
                "compression_policy": "zoom-v1"
            },
            "fanout_caps": {},
            "polygon_simplify_factor": 1.0,
            "ocean": {
                "mode": "artifact",
                "runtime_simplification": true,
                "low_zoom_source": "simplified",
                "artifact_key": {
                    "full_shp_xxh128": "8122bcc83873ef95349e6a3522827fd9",
                    "simplified_shp_xxh128": "4a1c2de9900177889900112233445566",
                    "compression_level": 6,
                    "policy_version": 1
                }
            }
        },
        "build": {
            "elivagar": { "commit": "b833fc8", "dirty": false },
            "pbfhogg_reader": { "version": "0.5.0" }
        },
        "effective": {
            "coordinate_source": "inline",
            "way_members": "injected_v1",
            "shared_node_pins": "block_local"
        },
        "execution": { "resumed_from": "sort" }
    }"#;

    #[test]
    fn provenance_prints_the_whole_contract_not_a_selection() {
        let out = provenance_output(FULL_BLOCK);
        assert!(out.contains("Provenance:  schema 1"), "{out}");
        assert!(out.contains("denmark-locations-prepass.osm.pbf"), "{out}");
        assert!(out.contains("xxh3 aa5bb865deadbeef  2.0 KB"), "{out}");
        // Reported from the header bits, so an unset feature is absent from the
        // list rather than listed as false.
        assert!(
            out.contains("features: sorted, locations-on-ways, way-members-v1"),
            "{out}"
        );
        assert!(!out.contains("shared-node-pins-v1"), "{out}");
        // The config fields two archives can silently differ in. Omitting any
        // of these would let incomparable archives display identically.
        assert!(
            out.contains("Config:     shortbread, z0-z14, polygon simplify x1"),
            "{out}"
        );
        assert!(
            out.contains("tile: mvt gzip, base level 6, policy zoom-v1"),
            "{out}"
        );
        assert!(out.contains("fanout: none"), "{out}");
        assert!(
            out.contains("Ocean:      artifact, low zoom simplified, simplifier on"),
            "{out}"
        );
        // The shapefile identities exist nowhere else in the block.
        assert!(
            out.contains("key: shp 8122bcc83873 simplified 4a1c2de99001 level 6 policy 1"),
            "{out}"
        );
        assert!(
            out.contains("Build:      elivagar b833fc8, pbfhogg 0.5.0"),
            "{out}"
        );
        assert!(
            out.contains("coords inline, way members injected_v1, pins block_local"),
            "{out}"
        );
        assert!(out.contains("Resumed:    from sort"), "{out}");
    }

    #[test]
    fn provenance_absence_is_reported_not_omitted() {
        // The case that motivated this: a blessed baseline built before the
        // block existed. Saying nothing would read as nothing to report.
        let out = provenance_of(r#"{"name":"Shortbread"}"#);
        assert!(out.contains("Provenance:  absent"), "{out}");
    }

    /// Each way of having no contract names itself. "No block", "unreadable
    /// metadata" and "not JSON" are different facts about an archive, and all
    /// three used to print nothing at all.
    #[test]
    fn provenance_names_every_unavailable_reason() {
        let of = |state: &super::MetadataState| {
            let mut out = Vec::new();
            let metadata = match state {
                super::MetadataState::Json(json) => {
                    serde_json::from_str::<serde_json::Value>(json).ok()
                }
                _ => None,
            };
            super::print_provenance(&mut out, state, metadata.as_ref()).expect("test value");
            String::from_utf8(out).expect("test value")
        };
        assert!(
            of(&super::MetadataState::Absent).contains("unavailable - archive stores no metadata"),
            "absent"
        );
        let too_large = of(&super::MetadataState::TooLarge(64 * 1024 * 1024));
        assert!(too_large.contains("unavailable"), "{too_large}");
        assert!(too_large.contains("64.0 MB"), "{too_large}");
        assert!(
            of(&super::MetadataState::Unreadable(
                crate::DecodeError::malformed("metadata", "bad bytes")
            ))
            .contains("could not be read"),
            "unreadable"
        );
        assert!(
            provenance_of("not json at all").contains("invalid - metadata is not JSON"),
            "invalid"
        );
    }

    /// A schema bump means an existing field changed meaning, so summarising an
    /// unknown one with this build's meanings would report confident nonsense.
    #[test]
    fn provenance_refuses_a_schema_it_does_not_understand() {
        let out = provenance_output(r#"{"schema":99,"input":{},"config":{}}"#);
        assert!(out.contains("schema 99"), "{out}");
        assert!(out.contains("not interpreted"), "{out}");
        assert!(!out.contains("Input:"), "{out}");

        let out = provenance_output(r#"{"input":{},"config":{}}"#);
        assert!(
            out.contains("INCOMPLETE - block is missing schema version"),
            "{out}"
        );
    }

    /// input and config ARE the contract, so a block missing either cannot
    /// establish comparability - a fact to state, not a group to skip.
    #[test]
    fn provenance_reports_an_incomplete_contract() {
        let out = provenance_output(r#"{"schema":1}"#);
        assert!(out.contains("INCOMPLETE"), "{out}");
        assert!(out.contains("missing input"), "{out}");

        let out = provenance_output(r#"{"schema":1,"input":{}}"#);
        assert!(out.contains("INCOMPLETE"), "{out}");
        assert!(out.contains("missing config"), "{out}");
    }

    #[test]
    fn provenance_omits_absent_diagnostic_groups() {
        // Missing Build and other diagnostic groups do not make the
        // input/config contract incomplete. A full run has no resumed_from.
        let out = provenance_output(
            r#"{"schema":1,"input":{},"config":{},"execution":{"resumed_from":null}}"#,
        );
        assert!(out.contains("schema 1"), "{out}");
        assert!(!out.contains("Effective:"), "{out}");
        assert!(!out.contains("Resumed:"), "{out}");
    }

    #[test]
    fn provenance_reports_a_pbf_with_no_declared_features() {
        let out = provenance_output(
            r#"{"schema":1,"config":{},"input":{"name":"raw.osm.pbf","xxh3_128":"ab","bytes":1,
                "features":{"sort_type_then_id":false,"locations_on_ways":false,
                "way_members_v1":false,"shared_node_pins_v1":false}}}"#,
        );
        assert!(out.contains("features: none"), "{out}");
    }

    /// A missing flag defaulted to false would print "features: none", which is
    /// a positive claim about the PBF derived from missing data.
    #[test]
    fn provenance_will_not_infer_features_from_a_missing_flag() {
        let out = provenance_output(
            r#"{"schema":1,"config":{},"input":{"name":"x.pbf","xxh3_128":"ab","bytes":1,
                "features":{"sort_type_then_id":false}}}"#,
        );
        assert!(!out.contains("features: none"), "{out}");
        assert!(
            out.contains("unknown - block declares no locations_on_ways"),
            "{out}"
        );
    }

    #[test]
    fn provenance_abbreviates_commits_but_not_sentinels() {
        assert_eq!(
            super::abbreviate_hash("b833fc8730cdbb7891b952c116c048876c2ad62c"),
            "b833fc8730cd"
        );
        assert_eq!(super::abbreviate_hash("unknown"), "unknown");
        assert_eq!(super::abbreviate_hash("b833fc8"), "b833fc8");
    }

    #[test]
    fn extract_json_string_returns_expected_value() {
        let json = r#"{"tile_payload_format":"mlt","tile_compression":"none"}"#;
        let doc = serde_json::from_str::<serde_json::Value>(json).expect("test value");
        assert_eq!(
            extract_json_string(&doc, "tile_payload_format").as_deref(),
            Some("mlt")
        );
        assert_eq!(
            extract_json_string(&doc, "tile_compression").as_deref(),
            Some("none")
        );
        assert_eq!(extract_json_string(&doc, "missing").as_deref(), None);
    }

    #[test]
    fn header_payload_format_inference_handles_legacy_and_unknown() {
        assert_eq!(infer_payload_format_from_header(TileType::Mvt), "mvt");
        assert_eq!(
            infer_payload_format_from_header(TileType::Unknown),
            "unknown"
        );
        assert_eq!(infer_payload_format_from_header(TileType::Avif), "unknown");
        assert_eq!(
            infer_payload_format_from_header(TileType::Other(9)),
            "unknown"
        );
    }

    #[test]
    fn payload_contract_display_prefers_metadata_when_present() {
        let (fmt, comp, fmt_src, comp_src) =
            payload_contract_display(TileType::Mvt, "gzip", Some("mlt"), Some("none"));
        assert_eq!(fmt, "mlt");
        assert_eq!(comp, "none");
        assert_eq!(fmt_src, "metadata");
        assert_eq!(comp_src, "metadata");
    }

    #[test]
    fn payload_contract_display_falls_back_to_header_for_legacy() {
        let (fmt, comp, fmt_src, comp_src) =
            payload_contract_display(TileType::Mvt, "gzip", None, None);
        assert_eq!(fmt, "mvt");
        assert_eq!(comp, "gzip");
        assert_eq!(fmt_src, "header");
        assert_eq!(comp_src, "header");
    }
}
