//! PMTiles v3 archive inspector.
//!
//! Reads the 127-byte header and optional gzip-compressed metadata from a
//! PMTiles file and prints a human-readable summary.

use std::io::{self, Write};
use std::path::Path;

use crate::pmtiles_reader::PmtilesReader;

/// Inspect a PMTiles file and print its header and metadata.
pub fn inspect(path: &Path) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    inspect_to_writer(path, &mut out)
}

#[allow(clippy::too_many_lines)]
fn inspect_to_writer(path: &Path, out: &mut dyn Write) -> io::Result<()> {
    let mut reader = PmtilesReader::open(path)?;
    let file_size = reader.file_size()?;
    let h = reader.header();
    let version = h[7];

    // Section offsets + lengths.
    let root_dir_offset = reader.root_dir_offset();
    let root_dir_length = reader.root_dir_length();
    let metadata_offset = reader.metadata_offset();
    let metadata_length = reader.metadata_length();
    let leaf_dirs_offset = reader.leaf_dirs_offset();
    let leaf_dirs_length = reader.leaf_dirs_length();
    let data_offset = reader.data_offset();
    let data_length = reader.data_length();

    // Tile counts.
    let num_addressed = reader.num_addressed();
    let num_entries = reader.num_entries();
    let unique_tiles = reader.num_unique();

    // Flags.
    let clustered = h[96] == 1;
    let internal_compression = compression_name(reader.internal_compression());
    let tile_compression = compression_name(reader.tile_compression());
    let tile_type = tile_type_name(reader.tile_type());

    // Zoom + bounds.
    let min_zoom = reader.min_zoom();
    let max_zoom = reader.max_zoom();
    let min_lon = e7_to_f64(read_i32_le(h, 102));
    let min_lat = e7_to_f64(read_i32_le(h, 106));
    let max_lon = e7_to_f64(read_i32_le(h, 110));
    let max_lat = e7_to_f64(read_i32_le(h, 114));
    let center_zoom = h[118];
    let center_lon = e7_to_f64(read_i32_le(h, 119));
    let center_lat = e7_to_f64(read_i32_le(h, 123));

    let dedup_count = num_addressed.saturating_sub(unique_tiles);
    let dedup_pct = if num_addressed > 0 {
        (dedup_count as f64 / num_addressed as f64) * 100.0
    } else {
        0.0
    };

    let metadata_state = read_metadata_state(&mut reader, metadata_length);
    let metadata_json = metadata_state.json().map(ToOwned::to_owned);
    let metadata_payload_format = metadata_json
        .as_deref()
        .and_then(|j| extract_json_string(j, "\"tile_payload_format\":\""));
    let metadata_tile_compression = metadata_json
        .as_deref()
        .and_then(|j| extract_json_string(j, "\"tile_compression\":\""));
    let (
        payload_format_display,
        payload_compression_display,
        payload_format_src,
        payload_compress_src,
    ) = payload_contract_display(
        reader.tile_type(),
        tile_compression,
        metadata_payload_format.as_deref(),
        metadata_tile_compression.as_deref(),
    );

    writeln!(out, "PMTiles v{version}  {}", path.display())?;
    writeln!(out, "  File size:  {}", format_bytes(file_size))?;
    writeln!(out)?;
    writeln!(out, "  Tile type:          {tile_type}")?;
    writeln!(out, "  Tile compression:   {tile_compression}")?;
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
    print_section_layout(
        out,
        root_dir_offset,
        root_dir_length,
        metadata_offset,
        metadata_length,
        leaf_dirs_offset,
        leaf_dirs_length,
        data_offset,
        data_length,
    )?;

    print_provenance(out, &metadata_state)?;

    if let Some(json) = metadata_json {
        writeln!(out)?;
        writeln!(out, "  Metadata:")?;
        // Try to pretty-print if it's valid JSON-like, otherwise raw.
        print_metadata_json(out, &json)?;
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
    Unreadable,
}

impl MetadataState {
    fn json(&self) -> Option<&str> {
        match self {
            Self::Json(j) => Some(j),
            _ => None,
        }
    }
}

fn read_metadata_state(reader: &mut PmtilesReader, metadata_length: u64) -> MetadataState {
    if metadata_length == 0 {
        return MetadataState::Absent;
    }
    if metadata_length > MAX_METADATA_READ {
        return MetadataState::TooLarge(metadata_length);
    }
    match reader.read_metadata() {
        Ok(json) => MetadataState::Json(json),
        Err(_) => MetadataState::Unreadable,
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
fn print_provenance(out: &mut dyn Write, state: &MetadataState) -> io::Result<()> {
    writeln!(out)?;
    let json = match state {
        MetadataState::Json(json) => json,
        MetadataState::Absent => {
            return writeln!(
                out,
                "  Provenance:  unavailable - archive stores no metadata"
            );
        }
        MetadataState::TooLarge(len) => {
            return writeln!(
                out,
                "  Provenance:  unavailable - metadata is {}, over the {} read limit",
                format_bytes(*len),
                format_bytes(MAX_METADATA_READ),
            );
        }
        MetadataState::Unreadable => {
            return writeln!(
                out,
                "  Provenance:  unavailable - metadata could not be read or decompressed"
            );
        }
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(json) else {
        return writeln!(out, "  Provenance:  invalid - metadata is not JSON");
    };
    let Some(p) = doc.get("elivagar") else {
        return writeln!(
            out,
            "  Provenance:  absent - archive predates the elivagar metadata block"
        );
    };

    // A schema bump means an existing field changed meaning, so a block this
    // build does not know cannot be summarised with this build's meanings -
    // that would report confident nonsense. Adding members does not bump, so
    // an equal schema with unknown members is fine and is ignored silently.
    match p.get("schema").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(crate::provenance::SCHEMA_VERSION) => {
            writeln!(out, "  Provenance:  schema {v}")?;
        }
        Some(v) => {
            return writeln!(
                out,
                "  Provenance:  schema {v} - this build understands {}; not interpreted",
                crate::provenance::SCHEMA_VERSION,
            );
        }
        None => {
            return writeln!(
                out,
                "  Provenance:  invalid - block declares no schema version"
            );
        }
    }

    // input and config are the contract. Either missing means the block cannot
    // establish comparability at all, which is a fact worth stating rather
    // than a group to skip.
    let (Some(input), Some(config)) = (p.get("input"), p.get("config")) else {
        writeln!(
            out,
            "    Contract:   INCOMPLETE - block is missing {}",
            match (p.get("input"), p.get("config")) {
                (None, None) => "input and config",
                (None, _) => "input",
                _ => "config",
            }
        )?;
        return Ok(());
    };

    print_contract(out, input, config)?;
    print_diagnostics(out, p)?;
    Ok(())
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
/// contract matches. Each is optional and absence is not an error - effective
/// is absent on an ocean-artifact build, resumed_from on any full run - so
/// unlike the contract, a missing group here is silence by design.
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
fn print_metadata_json(out: &mut dyn Write, json: &str) -> io::Result<()> {
    // Print top-level key=value pairs (simple string/number values).
    // This is a minimal approach - we look for "key":"value" or "key":number patterns.

    // Print the raw JSON indented if it's short, otherwise summarize.
    if json.len() < 500 {
        for line in json.lines() {
            writeln!(out, "    {line}")?;
        }
        return Ok(());
    }

    // For longer metadata, extract key info.
    // Look for vector_layers array and print layer names.
    if let Some(start) = json.find("\"vector_layers\"") {
        // Find the array
        if let Some(arr_start) = json[start..].find('[') {
            let arr_begin = start + arr_start;
            // Count layers by counting "id" occurrences
            let layers_section = &json[arr_begin..];
            let layer_count = layers_section.matches("\"id\"").count();
            writeln!(out, "    Layers: {layer_count}")?;

            // Extract each layer id
            let mut pos = 0;
            let bytes = layers_section.as_bytes();
            while pos < bytes.len() {
                if let Some(id_pos) = layers_section[pos..].find("\"id\":\"") {
                    let name_start = pos + id_pos + 6;
                    if let Some(name_end) = layers_section[name_start..].find('"') {
                        let name = &layers_section[name_start..name_start + name_end];
                        // Try to find minzoom for this layer.
                        // Clamp to a char boundary to avoid panics on non-ASCII metadata.
                        let chunk_end = layers_section.ceil_char_boundary(
                            (name_start + name_end + 200).min(layers_section.len()),
                        );
                        let chunk = &layers_section[name_start..chunk_end];
                        let minzoom = extract_number(chunk, "\"minzoom\":");
                        let maxzoom = extract_number(chunk, "\"maxzoom\":");
                        match (minzoom, maxzoom) {
                            (Some(mn), Some(mx)) => {
                                writeln!(out, "      {name:<24} z{mn}..{mx}")?;
                            }
                            _ => writeln!(out, "      {name}")?,
                        }
                        pos = name_start + name_end + 1;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    } else {
        // No vector_layers - just print raw.
        for line in json.lines() {
            writeln!(out, "    {line}")?;
        }
    }
    Ok(())
}

/// Extract a number value after a JSON key like `"minzoom":`.
fn extract_number(s: &str, key: &str) -> Option<u8> {
    let pos = s.find(key)?;
    let after = &s[pos + key.len()..];
    let after = after.trim_start();
    let end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    after[..end].parse().ok()
}

fn extract_json_string(s: &str, key: &str) -> Option<String> {
    let pos = s.find(key)?;
    let start = pos + key.len();
    let end = s[start..].find('"')?;
    Some(s[start..start + end].to_string())
}

fn infer_payload_format_from_header(tile_type: u8) -> &'static str {
    match tile_type {
        1 => "mvt",
        0 => "unknown",
        _ => "unknown",
    }
}

#[allow(clippy::too_many_arguments)]
fn print_section_layout(
    out: &mut dyn Write,
    root_dir_offset: u64,
    root_dir_length: u64,
    metadata_offset: u64,
    metadata_length: u64,
    leaf_dirs_offset: u64,
    leaf_dirs_length: u64,
    data_offset: u64,
    data_length: u64,
) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  Section layout:")?;
    writeln!(
        out,
        "    Header:      {:>13}  offset {root_dir_offset:>13}",
        format_bytes(127),
    )?;
    writeln!(
        out,
        "    Root dir:    {:>13}  offset {root_dir_offset:>13}",
        format_bytes(root_dir_length),
    )?;
    writeln!(
        out,
        "    Metadata:    {:>13}  offset {metadata_offset:>13}",
        format_bytes(metadata_length),
    )?;
    if leaf_dirs_length > 0 {
        writeln!(
            out,
            "    Leaf dirs:   {:>13}  offset {leaf_dirs_offset:>13}",
            format_bytes(leaf_dirs_length),
        )?;
    }
    writeln!(
        out,
        "    Tile data:   {:>13}  offset {data_offset:>13}",
        format_bytes(data_length),
    )?;
    Ok(())
}

fn payload_contract_display(
    tile_type: u8,
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

fn read_i32_le(buf: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

fn e7_to_f64(val: i32) -> f64 {
    f64::from(val) / 1e7
}

fn compression_name(val: u8) -> &'static str {
    match val {
        0 => "unknown",
        1 => "none",
        2 => "gzip",
        3 => "brotli",
        4 => "zstd",
        _ => "unrecognized",
    }
}

fn tile_type_name(val: u8) -> &'static str {
    match val {
        0 => "unknown",
        1 => "MVT",
        2 => "PNG",
        3 => "JPEG",
        4 => "WebP",
        5 => "AVIF",
        _ => "unrecognized",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{extract_json_string, infer_payload_format_from_header, payload_contract_display};
    use super::{inspect, inspect_to_writer};
    use crate::pmtiles_reader::PmtilesReader;
    use crate::pmtiles_writer::{PmtilesConfig, PmtilesWriter, tile_id_to_zxy, xy_to_tile_id};
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;

    fn add_monotonic_unique_tiles(writer: &mut PmtilesWriter, z: u8, count: usize) {
        let base = xy_to_tile_id(z, 0, 0);
        for i in 0..count {
            let tile_id = base + i as u64;
            let (z2, x, y) = tile_id_to_zxy(tile_id);
            assert_eq!(z2, z);
            let payload = (i as u64).to_le_bytes();
            writer.add_tile(z2, x, y, &payload).unwrap();
        }
    }

    fn inspect_output(path: &Path) -> String {
        let mut out = Vec::new();
        inspect_to_writer(path, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn corrupt_metadata_payload(path: &Path) -> u64 {
        let reader = PmtilesReader::open(path).unwrap();
        let metadata_offset = reader.metadata_offset();
        let metadata_length = reader.metadata_length();
        assert!(metadata_length > 0, "test archive must contain metadata");

        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(metadata_offset)).unwrap();
        let byte_count = metadata_length.min(64) as usize;
        let junk = vec![0xFF; byte_count];
        file.write_all(&junk).unwrap();
        metadata_length
    }

    fn set_header_u64(path: &Path, offset: u64, value: u64) {
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&value.to_le_bytes()).unwrap();
    }

    #[test]
    fn inspect_handles_root_only_archive() {
        let config = PmtilesConfig {
            min_zoom: 0,
            max_zoom: 0,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 0),
        };
        let mut writer = PmtilesWriter::new(config);
        writer.add_tile(0, 0, 0, &[1, 2, 3]).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inspect_root_only.pmtiles");
        writer.write_to(&path).unwrap();

        inspect(&path).unwrap();
    }

    #[test]
    fn inspect_root_only_output_includes_header_and_section_layout() {
        let config = PmtilesConfig {
            min_zoom: 0,
            max_zoom: 0,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 0),
        };
        let mut writer = PmtilesWriter::new(config);
        writer.add_tile(0, 0, 0, &[1, 2, 3]).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inspect_root_only_output.pmtiles");
        writer.write_to(&path).unwrap();

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
        let config = PmtilesConfig {
            min_zoom: 8,
            max_zoom: 8,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 8),
        };
        let mut writer = PmtilesWriter::new(config);
        add_monotonic_unique_tiles(&mut writer, 8, ROOT_THRESHOLD + 1);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inspect_leaf.pmtiles");
        writer.write_to(&path).unwrap();

        inspect(&path).unwrap();
    }

    #[test]
    fn inspect_leaf_output_includes_leaf_dirs_section() {
        const ROOT_THRESHOLD: usize = 16384;
        let config = PmtilesConfig {
            min_zoom: 8,
            max_zoom: 8,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 8),
        };
        let mut writer = PmtilesWriter::new(config);
        add_monotonic_unique_tiles(&mut writer, 8, ROOT_THRESHOLD + 1);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inspect_leaf_output.pmtiles");
        writer.write_to(&path).unwrap();

        let output = inspect_output(&path);
        assert!(output.contains("Section layout:"));
        assert!(output.contains("Leaf dirs:"));
    }

    #[test]
    fn inspect_unreadable_metadata_uses_header_payload_fallback() {
        let config = PmtilesConfig {
            min_zoom: 0,
            max_zoom: 0,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 0),
        };
        let mut writer = PmtilesWriter::new(config);
        writer.add_tile(0, 0, 0, &[7, 8, 9]).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inspect_bad_metadata.pmtiles");
        writer.write_to(&path).unwrap();

        let metadata_length = corrupt_metadata_payload(&path);
        assert!(metadata_length > 0);

        let output = inspect_output(&path);
        assert!(output.contains("Payload format:"));
        assert!(output.contains("Payload compress:"));
        assert!(output.contains("Payload format:     mvt (header)"));
        assert!(output.contains("(header)"));
        assert!(!output.contains("  Metadata:\n"));
    }

    #[test]
    fn inspect_absent_metadata_uses_header_payload_fallback() {
        let config = PmtilesConfig {
            min_zoom: 0,
            max_zoom: 0,
            bounds: (-180.0, -85.0, 180.0, 85.0),
            center: (0.0, 0.0, 0),
        };
        let mut writer = PmtilesWriter::new(config);
        writer.add_tile(0, 0, 0, &[9, 9, 9]).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inspect_no_metadata.pmtiles");
        writer.write_to(&path).unwrap();

        // PMTiles v3 header offset 32 stores metadata_length (u64 LE).
        set_header_u64(&path, 32, 0);

        let output = inspect_output(&path);
        assert!(output.contains("Payload format:     mvt (header)"));
        assert!(output.contains("Payload compress:   gzip (header)"));
        assert!(!output.contains("  Metadata:\n"));
    }

    fn provenance_of(json: &str) -> String {
        let mut out = Vec::new();
        super::print_provenance(&mut out, &super::MetadataState::Json(json.to_string())).unwrap();
        String::from_utf8(out).unwrap()
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
            super::print_provenance(&mut out, state).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert!(
            of(&super::MetadataState::Absent).contains("unavailable - archive stores no metadata"),
            "absent"
        );
        let too_large = of(&super::MetadataState::TooLarge(64 * 1024 * 1024));
        assert!(too_large.contains("unavailable"), "{too_large}");
        assert!(too_large.contains("64.0 MB"), "{too_large}");
        assert!(
            of(&super::MetadataState::Unreadable).contains("could not be read"),
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
        assert!(out.contains("invalid - block declares no schema"), "{out}");
    }

    /// input and config ARE the contract, so a block missing either cannot
    /// establish comparability - a fact to state, not a group to skip.
    #[test]
    fn provenance_reports_an_incomplete_contract() {
        let out = provenance_output(r#"{"schema":1}"#);
        assert!(out.contains("INCOMPLETE"), "{out}");
        assert!(out.contains("input and config"), "{out}");

        let out = provenance_output(r#"{"schema":1,"input":{}}"#);
        assert!(out.contains("INCOMPLETE"), "{out}");
        assert!(out.contains("missing config"), "{out}");
    }

    #[test]
    fn provenance_omits_absent_diagnostic_groups() {
        // effective is absent on an ocean-artifact build and resumed_from on
        // any full run. Both are diagnostics, so neither is an error.
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
        assert_eq!(
            extract_json_string(json, "\"tile_payload_format\":\"").as_deref(),
            Some("mlt")
        );
        assert_eq!(
            extract_json_string(json, "\"tile_compression\":\"").as_deref(),
            Some("none")
        );
        assert_eq!(extract_json_string(json, "\"missing\":\"").as_deref(), None);
    }

    #[test]
    fn header_payload_format_inference_handles_legacy_and_unknown() {
        assert_eq!(infer_payload_format_from_header(1), "mvt");
        assert_eq!(infer_payload_format_from_header(0), "unknown");
        assert_eq!(infer_payload_format_from_header(5), "unknown");
    }

    #[test]
    fn payload_contract_display_prefers_metadata_when_present() {
        let (fmt, comp, fmt_src, comp_src) =
            payload_contract_display(1, "gzip", Some("mlt"), Some("none"));
        assert_eq!(fmt, "mlt");
        assert_eq!(comp, "none");
        assert_eq!(fmt_src, "metadata");
        assert_eq!(comp_src, "metadata");
    }

    #[test]
    fn payload_contract_display_falls_back_to_header_for_legacy() {
        let (fmt, comp, fmt_src, comp_src) = payload_contract_display(1, "gzip", None, None);
        assert_eq!(fmt, "mvt");
        assert_eq!(comp, "gzip");
        assert_eq!(fmt_src, "header");
        assert_eq!(comp_src, "header");
    }
}
