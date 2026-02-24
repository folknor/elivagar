#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

//! Compare feature counts per layer per zoom between two PMTiles archives.
//!
//! Reads both files, samples tiles at each zoom level, decodes MVT protobuf,
//! and prints per-layer feature counts side by side.
//!
//! Usage:
//!   cargo run --release --example compare_tiles -- <file_a> <file_b> [--sample N]
//!
//! Default sample: 200 tiles per zoom level (or all if fewer exist).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use elivagar::pmtiles_writer::tile_id_to_zxy;
use flate2::read::GzDecoder;

// ---------------------------------------------------------------------------
// PMTiles reader (minimal, sync, read-only)
// ---------------------------------------------------------------------------

struct PmtilesReader {
    file: File,
    min_zoom: u8,
    max_zoom: u8,
    root_dir_offset: u64,
    root_dir_length: u64,
    leaf_dirs_offset: u64,
    _leaf_dirs_length: u64,
    data_offset: u64,
    internal_compression: u8,
}

struct TileEntry {
    tile_id: u64,
    offset: u64,
    length: u32,
}

impl PmtilesReader {
    fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut header = [0u8; 127];
        file.read_exact(&mut header)?;

        if &header[0..7] != b"PMTiles" || header[7] != 3 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not PMTiles v3"));
        }

        Ok(PmtilesReader {
            file,
            min_zoom: header[100],
            max_zoom: header[101],
            root_dir_offset: read_u64_le(&header, 8),
            root_dir_length: read_u64_le(&header, 16),
            leaf_dirs_offset: read_u64_le(&header, 40),
            _leaf_dirs_length: read_u64_le(&header, 48),
            data_offset: read_u64_le(&header, 56),
            internal_compression: header[97],
        })
    }

    /// Get all tile entries by reading root + leaf directories.
    fn read_all_entries(&mut self) -> io::Result<Vec<TileEntry>> {
        let root_entries = self.read_directory(self.root_dir_offset, self.root_dir_length)?;

        let mut all_entries = Vec::new();
        for entry in &root_entries {
            if entry.run_length == 0 {
                // Leaf directory pointer
                let leaf_offset = self.leaf_dirs_offset + entry.offset;
                let leaf_entries = self.read_directory(leaf_offset, entry.length as u64)?;
                Self::expand_entries(&leaf_entries, &mut all_entries);
            } else {
                // Direct tile entry in root
                Self::expand_single(entry, &mut all_entries);
            }
        }

        Ok(all_entries)
    }

    fn expand_entries(dir_entries: &[RawDirEntry], out: &mut Vec<TileEntry>) {
        for e in dir_entries {
            if e.run_length == 0 {
                continue; // leaf pointer, skip
            }
            Self::expand_single(e, out);
        }
    }

    fn expand_single(e: &RawDirEntry, out: &mut Vec<TileEntry>) {
        for r in 0..e.run_length {
            out.push(TileEntry {
                tile_id: e.tile_id + u64::from(r),
                offset: e.offset + u64::from(e.length) * u64::from(r),
                length: e.length,
            });
        }
    }

    fn read_directory(&mut self, offset: u64, length: u64) -> io::Result<Vec<RawDirEntry>> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut compressed = vec![0u8; length as usize];
        self.file.read_exact(&mut compressed)?;

        let raw = if self.internal_compression == 2 {
            let mut decoder = GzDecoder::new(&compressed[..]);
            let mut buf = Vec::new();
            decoder.read_to_end(&mut buf)?;
            buf
        } else {
            compressed
        };

        Ok(decode_directory(&raw))
    }

    fn read_tile(&mut self, entry: &TileEntry) -> io::Result<Vec<u8>> {
        let abs_offset = self.data_offset + entry.offset;
        self.file.seek(SeekFrom::Start(abs_offset))?;
        let mut compressed = vec![0u8; entry.length as usize];
        self.file.read_exact(&mut compressed)?;

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf)?;
        Ok(buf)
    }
}

struct RawDirEntry {
    tile_id: u64,
    offset: u64,
    length: u32,
    run_length: u32,
}

fn decode_directory(data: &[u8]) -> Vec<RawDirEntry> {
    let mut pos = 0;
    let count = decode_varint(data, &mut pos) as usize;

    // Column 1: delta-encoded tile IDs
    let mut tile_ids = Vec::with_capacity(count);
    let mut prev: u64 = 0;
    for _ in 0..count {
        let delta = decode_varint(data, &mut pos);
        prev += delta;
        tile_ids.push(prev);
    }

    // Column 2: run lengths
    let mut run_lengths = Vec::with_capacity(count);
    for _ in 0..count {
        run_lengths.push(decode_varint(data, &mut pos) as u32);
    }

    // Column 3: lengths
    let mut lengths = Vec::with_capacity(count);
    for _ in 0..count {
        lengths.push(decode_varint(data, &mut pos) as u32);
    }

    // Column 4: offsets
    // PMTiles spec: if value is 0 (and not first entry), offset is contiguous
    // with previous entry. "Contiguous" means prev.offset + prev.length * prev.run_length
    // for tile entries, or prev.offset + prev.length for leaf pointers.
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let v = decode_varint(data, &mut pos);
        let offset = if v == 0 && i > 0 {
            let prev: &RawDirEntry = &entries[i - 1];
            let prev_run = if prev.run_length > 0 { prev.run_length } else { 1 };
            prev.offset + u64::from(prev.length) * u64::from(prev_run)
        } else {
            v - 1
        };
        entries.push(RawDirEntry {
            tile_id: tile_ids[i],
            offset,
            length: lengths[i],
            run_length: run_lengths[i],
        });
    }

    entries
}

fn decode_varint(data: &[u8], pos: &mut usize) -> u64 {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= data.len() {
            return result;
        }
        let byte = data[*pos];
        *pos += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    result
}

fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// MVT protobuf decoder (minimal — just counts features per layer)
// ---------------------------------------------------------------------------

struct LayerStats {
    name: String,
    feature_count: usize,
    point_count: usize,
    line_count: usize,
    polygon_count: usize,
    geom_cmds_total: usize,
}

fn decode_mvt_layers(data: &[u8]) -> Vec<LayerStats> {
    let mut layers = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let (field, wire_type, new_pos) = decode_proto_tag(data, pos);
        pos = new_pos;

        if wire_type == 2 {
            // length-delimited
            let (len, new_pos) = decode_proto_varint_raw(data, pos);
            pos = new_pos;
            let end = pos + len as usize;
            if end > data.len() {
                break;
            }

            if field == 3 {
                // Tile.layers
                layers.push(decode_mvt_layer(&data[pos..end]));
            }
            pos = end;
        } else if wire_type == 0 {
            let (_, new_pos) = decode_proto_varint_raw(data, pos);
            pos = new_pos;
        } else if wire_type == 1 {
            pos += 8;
        } else if wire_type == 5 {
            pos += 4;
        } else {
            break;
        }
    }

    layers
}

fn decode_mvt_layer(data: &[u8]) -> LayerStats {
    let mut stats = LayerStats {
        name: String::new(),
        feature_count: 0,
        point_count: 0,
        line_count: 0,
        polygon_count: 0,
        geom_cmds_total: 0,
    };

    let mut pos = 0;
    while pos < data.len() {
        let (field, wire_type, new_pos) = decode_proto_tag(data, pos);
        pos = new_pos;

        if wire_type == 2 {
            let (len, new_pos) = decode_proto_varint_raw(data, pos);
            pos = new_pos;
            let end = pos + len as usize;
            if end > data.len() {
                break;
            }

            match field {
                1 => {
                    // name
                    stats.name = String::from_utf8_lossy(&data[pos..end]).to_string();
                }
                2 => {
                    // feature
                    let (geom_type, geom_cmd_count) = decode_mvt_feature(&data[pos..end]);
                    stats.feature_count += 1;
                    stats.geom_cmds_total += geom_cmd_count;
                    match geom_type {
                        1 => stats.point_count += 1,
                        2 => stats.line_count += 1,
                        3 => stats.polygon_count += 1,
                        _ => {}
                    }
                }
                _ => {}
            }
            pos = end;
        } else if wire_type == 0 {
            let (_, new_pos) = decode_proto_varint_raw(data, pos);
            pos = new_pos;
        } else if wire_type == 1 {
            pos += 8;
        } else if wire_type == 5 {
            pos += 4;
        } else {
            break;
        }
    }

    stats
}

/// Decode a feature, returning (geom_type, geometry_command_count).
fn decode_mvt_feature(data: &[u8]) -> (u8, usize) {
    let mut geom_type: u8 = 0;
    let mut geom_cmd_count: usize = 0;
    let mut pos = 0;

    while pos < data.len() {
        let (field, wire_type, new_pos) = decode_proto_tag(data, pos);
        pos = new_pos;

        if wire_type == 0 {
            let (val, new_pos) = decode_proto_varint_raw(data, pos);
            pos = new_pos;
            if field == 3 {
                geom_type = val as u8;
            }
        } else if wire_type == 2 {
            let (len, new_pos) = decode_proto_varint_raw(data, pos);
            pos = new_pos;
            let end = pos + len as usize;
            if end > data.len() {
                break;
            }
            if field == 4 {
                // geometry — count varint entries
                let mut gpos = pos;
                while gpos < end {
                    let (_, new_gpos) = decode_proto_varint_raw(data, gpos);
                    gpos = new_gpos;
                    geom_cmd_count += 1;
                }
            }
            pos = end;
        } else if wire_type == 1 {
            pos += 8;
        } else if wire_type == 5 {
            pos += 4;
        } else {
            break;
        }
    }

    (geom_type, geom_cmd_count)
}

fn decode_proto_tag(data: &[u8], pos: usize) -> (u32, u8, usize) {
    let (val, new_pos) = decode_proto_varint_raw(data, pos);
    let field = (val >> 3) as u32;
    let wire_type = (val & 7) as u8;
    (field, wire_type, new_pos)
}

fn decode_proto_varint_raw(data: &[u8], mut pos: usize) -> (u64, usize) {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if pos >= data.len() {
            return (result, pos);
        }
        let byte = data[pos];
        pos += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (result, pos)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: compare_tiles <file_a> <file_b> [--sample N]");
        std::process::exit(1);
    }

    let path_a = &args[1];
    let path_b = &args[2];

    let mut sample_per_zoom: usize = 200;
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--sample" {
            i += 1;
            sample_per_zoom = args[i].parse().expect("invalid --sample");
        }
        i += 1;
    }

    let name_a = Path::new(path_a)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let name_b = Path::new(path_b)
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();

    eprintln!("Reading {name_a}...");
    let mut reader_a = PmtilesReader::open(Path::new(path_a)).expect("open file A");
    let entries_a = reader_a.read_all_entries().expect("read entries A");
    eprintln!("  {} tiles, z{}-{}", entries_a.len(), reader_a.min_zoom, reader_a.max_zoom);

    eprintln!("Reading {name_b}...");
    let mut reader_b = PmtilesReader::open(Path::new(path_b)).expect("open file B");
    let entries_b = reader_b.read_all_entries().expect("read entries B");
    eprintln!("  {} tiles, z{}-{}", entries_b.len(), reader_b.min_zoom, reader_b.max_zoom);

    // Group entries by zoom
    let min_zoom = reader_a.min_zoom.min(reader_b.min_zoom);
    let max_zoom = reader_a.max_zoom.max(reader_b.max_zoom);

    let entries_by_zoom_a = group_by_zoom(&entries_a);
    let entries_by_zoom_b = group_by_zoom(&entries_b);

    // For each zoom, find tiles that exist in both archives (by tile_id),
    // sample some, decode, and compare.
    eprintln!("\nComparing {} sample tiles per zoom...\n", sample_per_zoom);

    // Accumulate totals across all zooms
    let mut grand_totals_a: HashMap<String, (usize, usize, usize, usize, usize)> = HashMap::new();
    let mut grand_totals_b: HashMap<String, (usize, usize, usize, usize, usize)> = HashMap::new();

    for z in min_zoom..=max_zoom {
        let empty = Vec::new();
        let za = entries_by_zoom_a.get(&z).unwrap_or(&empty);
        let zb = entries_by_zoom_b.get(&z).unwrap_or(&empty);

        // Build lookup for file B by tile_id
        let b_map: HashMap<u64, usize> = zb.iter().enumerate().map(|(i, e)| (e.tile_id, i)).collect();

        // Find common tile_ids
        let mut common: Vec<(usize, usize)> = Vec::new();
        for (ai, entry) in za.iter().enumerate() {
            if let Some(&bi) = b_map.get(&entry.tile_id) {
                common.push((ai, bi));
            }
        }

        let sample_count = common.len().min(sample_per_zoom);
        if sample_count == 0 {
            eprintln!("z{z:2}: A={} tiles, B={} tiles, 0 common — skipping", za.len(), zb.len());
            continue;
        }

        // Evenly sample from common tiles
        let step = if common.len() <= sample_per_zoom { 1 } else { common.len() / sample_per_zoom };
        let sampled: Vec<(usize, usize)> = common.iter().step_by(step).take(sample_per_zoom).copied().collect();

        // Decode sampled tiles and accumulate per-layer stats
        let mut layer_stats_a: HashMap<String, (usize, usize, usize, usize, usize)> = HashMap::new(); // (features, points, lines, polys, geom_cmds)
        let mut layer_stats_b: HashMap<String, (usize, usize, usize, usize, usize)> = HashMap::new();
        let mut total_bytes_a: usize = 0;
        let mut total_bytes_b: usize = 0;

        let mut read_errors = 0usize;
        for (ai, bi) in &sampled {
            let tile_data_a = match reader_a.read_tile(&za[*ai]) {
                Ok(d) => d,
                Err(_) => { read_errors += 1; continue; }
            };
            let tile_data_b = match reader_b.read_tile(&zb[*bi]) {
                Ok(d) => d,
                Err(_) => { read_errors += 1; continue; }
            };
            total_bytes_a += tile_data_a.len();
            total_bytes_b += tile_data_b.len();

            for ls in decode_mvt_layers(&tile_data_a) {
                let entry = layer_stats_a.entry(ls.name.clone()).or_default();
                entry.0 += ls.feature_count;
                entry.1 += ls.point_count;
                entry.2 += ls.line_count;
                entry.3 += ls.polygon_count;
                entry.4 += ls.geom_cmds_total;

                let ge = grand_totals_a.entry(ls.name).or_default();
                ge.0 += ls.feature_count;
                ge.1 += ls.point_count;
                ge.2 += ls.line_count;
                ge.3 += ls.polygon_count;
                ge.4 += ls.geom_cmds_total;
            }
            for ls in decode_mvt_layers(&tile_data_b) {
                let entry = layer_stats_b.entry(ls.name.clone()).or_default();
                entry.0 += ls.feature_count;
                entry.1 += ls.point_count;
                entry.2 += ls.line_count;
                entry.3 += ls.polygon_count;
                entry.4 += ls.geom_cmds_total;

                let ge = grand_totals_b.entry(ls.name).or_default();
                ge.0 += ls.feature_count;
                ge.1 += ls.point_count;
                ge.2 += ls.line_count;
                ge.3 += ls.polygon_count;
                ge.4 += ls.geom_cmds_total;
            }
        }

        // Print zoom summary
        let decoded = sampled.len() - read_errors;
        if decoded == 0 {
            eprintln!("z{z:2}: {read_errors} read errors, 0 decoded — skipping");
            continue;
        }
        let avg_bytes_a = total_bytes_a / decoded;
        let avg_bytes_b = total_bytes_b / decoded;
        let err_note = if read_errors > 0 { format!(" ({read_errors} read errors)") } else { String::new() };
        eprintln!(
            "z{z:2}: {sample} tiles sampled (of {common} common, A={ta} B={tb} total)  avg_mvt: A={avg_a} B={avg_b} bytes{err_note}",
            sample = decoded,
            common = common.len(),
            ta = za.len(),
            tb = zb.len(),
            avg_a = avg_bytes_a,
            avg_b = avg_bytes_b,
        );

        // Collect all layer names seen at this zoom
        let mut all_layers: Vec<String> = layer_stats_a.keys().chain(layer_stats_b.keys()).cloned().collect();
        all_layers.sort();
        all_layers.dedup();

        for name in &all_layers {
            let (fa, _pa, la, ga, ca) = layer_stats_a.get(name).copied().unwrap_or_default();
            let (fb, _pb, lb, gb, cb) = layer_stats_b.get(name).copied().unwrap_or_default();

            if fa == 0 && fb == 0 {
                continue;
            }

            let diff_pct = if fa > 0 && fb > 0 {
                let ratio = fb as f64 / fa as f64;
                format!("{:+.0}%", (ratio - 1.0) * 100.0)
            } else if fa == 0 {
                "+inf".to_string()
            } else {
                "-100%".to_string()
            };

            // Show geom type breakdown only if mixed
            let type_info_a = if (la > 0) as u8 + (ga > 0) as u8 > 0 {
                format!(" (L:{la} P:{ga})")
            } else {
                String::new()
            };
            let type_info_b = if (lb > 0) as u8 + (gb > 0) as u8 > 0 {
                format!(" (L:{lb} P:{gb})")
            } else {
                String::new()
            };

            eprintln!(
                "  {name:24} A:{fa:6}{type_info_a:16} B:{fb:6}{type_info_b:16} cmds A:{ca:8} B:{cb:8}  {diff_pct}"
            );
        }
    }

    // Grand totals
    eprintln!("\n=== Grand totals (all sampled tiles) ===\n");

    let mut all_layers: Vec<String> = grand_totals_a.keys().chain(grand_totals_b.keys()).cloned().collect();
    all_layers.sort();
    all_layers.dedup();

    eprintln!("{:24} {:>10} {:>10} {:>8} {:>10} {:>10}", "layer", &name_a, &name_b, "diff", "cmds_A", "cmds_B");
    eprintln!("{}", "-".repeat(78));

    let mut total_a = 0usize;
    let mut total_b = 0usize;
    let mut total_cmds_a = 0usize;
    let mut total_cmds_b = 0usize;

    for name in &all_layers {
        let (fa, _, _, _, ca) = grand_totals_a.get(name).copied().unwrap_or_default();
        let (fb, _, _, _, cb) = grand_totals_b.get(name).copied().unwrap_or_default();

        total_a += fa;
        total_b += fb;
        total_cmds_a += ca;
        total_cmds_b += cb;

        let diff_pct = if fa > 0 && fb > 0 {
            let ratio = fb as f64 / fa as f64;
            format!("{:+.0}%", (ratio - 1.0) * 100.0)
        } else if fa == 0 {
            "only B".to_string()
        } else {
            "only A".to_string()
        };

        eprintln!("{name:24} {fa:10} {fb:10} {diff_pct:>8} {ca:10} {cb:10}");
    }

    eprintln!("{}", "-".repeat(78));
    let total_diff = if total_a > 0 {
        format!("{:+.0}%", (total_b as f64 / total_a as f64 - 1.0) * 100.0)
    } else {
        "N/A".to_string()
    };
    eprintln!("{:24} {total_a:10} {total_b:10} {total_diff:>8} {total_cmds_a:10} {total_cmds_b:10}", "TOTAL");
}

fn group_by_zoom(entries: &[TileEntry]) -> HashMap<u8, Vec<TileEntry>> {
    let mut map: HashMap<u8, Vec<TileEntry>> = HashMap::new();
    for e in entries {
        let (z, _, _) = tile_id_to_zxy(e.tile_id);
        map.entry(z).or_default().push(TileEntry {
            tile_id: e.tile_id,
            offset: e.offset,
            length: e.length,
        });
    }
    map
}
