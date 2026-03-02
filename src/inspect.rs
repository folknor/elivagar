//! PMTiles v3 archive inspector.
//!
//! Reads the 127-byte header and optional gzip-compressed metadata from a
//! PMTiles file and prints a human-readable summary.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use flate2::read::GzDecoder;

/// Inspect a PMTiles file and print its header and metadata.
pub fn inspect(path: &Path) -> io::Result<()> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();

    // Read 127-byte header.
    let mut h = [0u8; 127];
    file.read_exact(&mut h)?;

    // Validate magic.
    if &h[0..7] != b"PMTiles" {
        return Err(io::Error::other("not a PMTiles file (bad magic)"));
    }
    let version = h[7];

    // Section offsets + lengths.
    let root_dir_offset = read_u64_le(&h, 8);
    let root_dir_length = read_u64_le(&h, 16);
    let metadata_offset = read_u64_le(&h, 24);
    let metadata_length = read_u64_le(&h, 32);
    let leaf_dirs_offset = read_u64_le(&h, 40);
    let leaf_dirs_length = read_u64_le(&h, 48);
    let data_offset = read_u64_le(&h, 56);
    let data_length = read_u64_le(&h, 64);

    // Tile counts.
    let num_addressed = read_u64_le(&h, 72);
    let num_entries = read_u64_le(&h, 80);
    let unique_tiles = read_u64_le(&h, 88);

    // Flags.
    let clustered = h[96] == 1;
    let internal_compression = compression_name(h[97]);
    let tile_compression = compression_name(h[98]);
    let tile_type = tile_type_name(h[99]);

    // Zoom + bounds.
    let min_zoom = h[100];
    let max_zoom = h[101];
    let min_lon = e7_to_f64(read_i32_le(&h, 102));
    let min_lat = e7_to_f64(read_i32_le(&h, 106));
    let max_lon = e7_to_f64(read_i32_le(&h, 110));
    let max_lat = e7_to_f64(read_i32_le(&h, 114));
    let center_zoom = h[118];
    let center_lon = e7_to_f64(read_i32_le(&h, 119));
    let center_lat = e7_to_f64(read_i32_le(&h, 123));

    let dedup_count = num_addressed.saturating_sub(unique_tiles);
    let dedup_pct = if num_addressed > 0 {
        (dedup_count as f64 / num_addressed as f64) * 100.0
    } else {
        0.0
    };

    println!("PMTiles v{version}  {}", path.display());
    println!("  File size:  {}", format_bytes(file_size));
    println!();
    println!("  Tile type:          {tile_type}");
    println!("  Tile compression:   {tile_compression}");
    println!("  Internal compress:  {internal_compression}");
    println!("  Clustered:          {clustered}");
    println!();
    println!("  Zoom:    {min_zoom}..{max_zoom}");
    println!(
        "  Bounds:  [{min_lon:.7}, {min_lat:.7}] to [{max_lon:.7}, {max_lat:.7}]"
    );
    println!("  Center:  [{center_lon:.7}, {center_lat:.7}] z{center_zoom}");
    println!();
    println!("  Tiles addressed:  {num_addressed:>14}");
    println!("  Unique tiles:     {unique_tiles:>14}");
    println!("  Deduplicated:     {dedup_count:>14} ({dedup_pct:.1}%)");
    println!("  Directory entries: {num_entries:>13}");
    println!();
    println!("  Section layout:");
    println!(
        "    Header:      {:>13}  offset {root_dir_offset:>13}",
        format_bytes(127),
    );
    println!(
        "    Root dir:    {:>13}  offset {root_dir_offset:>13}",
        format_bytes(root_dir_length),
    );
    println!(
        "    Metadata:    {:>13}  offset {metadata_offset:>13}",
        format_bytes(metadata_length),
    );
    if leaf_dirs_length > 0 {
        println!(
            "    Leaf dirs:   {:>13}  offset {leaf_dirs_offset:>13}",
            format_bytes(leaf_dirs_length),
        );
    }
    println!(
        "    Tile data:   {:>13}  offset {data_offset:>13}",
        format_bytes(data_length),
    );

    // Read and decompress metadata JSON.
    if metadata_length > 0 && metadata_length <= 10 * 1024 * 1024 {
        use std::io::Seek;
        #[allow(clippy::cast_possible_truncation)]
        let mut compressed = vec![0u8; metadata_length as usize];
        file.seek(io::SeekFrom::Start(metadata_offset))?;
        file.read_exact(&mut compressed)?;

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut json = String::new();
        if decoder.read_to_string(&mut json).is_ok() {
            println!();
            println!("  Metadata:");
            // Try to pretty-print if it's valid JSON-like, otherwise raw.
            print_metadata_json(&json);
        }
    }

    Ok(())
}

/// Print metadata JSON in a readable format.
/// We avoid pulling in serde_json as a non-dev dependency by doing minimal
/// parsing: extract vector_layers names + zoom ranges.
fn print_metadata_json(json: &str) {
    // Print top-level key=value pairs (simple string/number values).
    // This is a minimal approach — we look for "key":"value" or "key":number patterns.

    // Print the raw JSON indented if it's short, otherwise summarize.
    if json.len() < 500 {
        for line in json.lines() {
            println!("    {line}");
        }
        return;
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
            println!("    Layers: {layer_count}");

            // Extract each layer id
            let mut pos = 0;
            let bytes = layers_section.as_bytes();
            while pos < bytes.len() {
                if let Some(id_pos) = layers_section[pos..].find("\"id\":\"") {
                    let name_start = pos + id_pos + 6;
                    if let Some(name_end) = layers_section[name_start..].find('"') {
                        let name = &layers_section[name_start..name_start + name_end];
                        // Try to find minzoom for this layer
                        let chunk_end =
                            (name_start + name_end + 200).min(layers_section.len());
                        let chunk = &layers_section[name_start..chunk_end];
                        let minzoom = extract_number(chunk, "\"minzoom\":");
                        let maxzoom = extract_number(chunk, "\"maxzoom\":");
                        match (minzoom, maxzoom) {
                            (Some(mn), Some(mx)) => {
                                println!("      {name:<24} z{mn}..{mx}");
                            }
                            _ => println!("      {name}"),
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
        // No vector_layers — just print raw.
        for line in json.lines() {
            println!("    {line}");
        }
    }
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

fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
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
