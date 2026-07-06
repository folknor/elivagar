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

    let metadata_json = if metadata_length > 0 && metadata_length <= 10 * 1024 * 1024 {
        reader.read_metadata().ok()
    } else {
        None
    };
    let metadata_payload_format = metadata_json
        .as_deref()
        .and_then(|j| extract_json_string(j, "\"tile_payload_format\":\""));
    let metadata_tile_compression = metadata_json
        .as_deref()
        .and_then(|j| extract_json_string(j, "\"tile_compression\":\""));
    let (payload_format_display, payload_compression_display, payload_format_src, payload_compress_src) =
        payload_contract_display(
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
    writeln!(out, "  Payload format:     {payload_format_display} ({payload_format_src})")?;
    writeln!(out, "  Payload compress:   {payload_compression_display} ({payload_compress_src})")?;
    writeln!(out, "  Clustered:          {clustered}")?;
    writeln!(out)?;
    writeln!(out, "  Zoom:    {min_zoom}..{max_zoom}")?;
    writeln!(
        out,
        "  Bounds:  [{min_lon:.7}, {min_lat:.7}] to [{max_lon:.7}, {max_lat:.7}]"
    )?;
    writeln!(out, "  Center:  [{center_lon:.7}, {center_lat:.7}] z{center_zoom}")?;
    writeln!(out)?;
    writeln!(out, "  Tiles addressed:  {num_addressed:>14}")?;
    writeln!(out, "  Unique tiles:     {unique_tiles:>14}")?;
    writeln!(out, "  Deduplicated:     {dedup_count:>14} ({dedup_pct:.1}%)")?;
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

    if let Some(json) = metadata_json {
        writeln!(out)?;
        writeln!(out, "  Metadata:")?;
        // Try to pretty-print if it's valid JSON-like, otherwise raw.
        print_metadata_json(out, &json)?;
    }

    Ok(())
}

/// Print metadata JSON in a readable format.
/// We avoid pulling in serde_json as a non-dev dependency by doing minimal
/// parsing: extract vector_layers names + zoom ranges.
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
    use super::{inspect, inspect_to_writer};
    use super::{extract_json_string, infer_payload_format_from_header, payload_contract_display};
    use crate::pmtiles_reader::PmtilesReader;
    use crate::pmtiles_writer::{tile_id_to_zxy, xy_to_tile_id, PmtilesConfig, PmtilesWriter};
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
        assert_eq!(
            extract_json_string(json, "\"missing\":\"").as_deref(),
            None
        );
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
        let (fmt, comp, fmt_src, comp_src) =
            payload_contract_display(1, "gzip", None, None);
        assert_eq!(fmt, "mvt");
        assert_eq!(comp, "gzip");
        assert_eq!(fmt_src, "header");
        assert_eq!(comp_src, "header");
    }
}
