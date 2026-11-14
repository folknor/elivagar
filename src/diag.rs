//! `elivagar diag`: per-ring winding report for one tile.
//!
//! Every polygon feature across all layers, decoded through the shared
//! command decoder under the strict polygon grammar. Rings are printed raw,
//! zero-area ones included - grouping would hide exactly the degenerate rings
//! a winding diagnosis looks for. A feature that does not decode prints its
//! error instead of partial rings.

use std::io;
use std::path::Path;

use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

use crate::geometry::polygon::{RingOrientation, ring_is_simple, ring_role};
use crate::mvt::decode::{self, Words};
use crate::pmtiles_reader::{PmtilesReader, find_entry};

/// Print the ring report for tile `z/x/y` of the archive at `path`.
pub fn diag_tile(path: &Path, z: u8, x: u32, y: u32) -> io::Result<()> {
    crate::svg::validate_tile_grid_coordinates(z, x, y, 1, 1)?;
    let mut reader = PmtilesReader::open(path)?;
    let runs = reader.read_all_runs()?;
    let tile_id = crate::tiling::TileId::from_coord(z, x, y)?.raw();
    let entry = find_entry(&runs, tile_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "tile not found"))?;
    let raw = reader.read_tile(&entry)?;

    println!(
        "Tile z{z}/{x}/{y} - {raw_len} bytes decompressed",
        raw_len = raw.len()
    );

    let mut tc = Cursor::new(&raw);
    while let Ok(Some((field, wire_type))) = tc.read_tag() {
        if field == 3 && wire_type == WIRE_LEN {
            if let Ok(layer_data) = tc.read_len_delimited() {
                diag_layer(layer_data);
            }
        } else if tc.skip_field(wire_type).is_err() {
            break;
        }
    }
    Ok(())
}

/// The geometry type field of one MVT feature, 0 when absent.
fn diag_geom_type(fdata: &[u8]) -> u64 {
    let mut fc = Cursor::new(fdata);
    let mut gt: u64 = 0;
    while let Ok(Some((ff, fw))) = fc.read_tag() {
        if ff == 3 && fw == WIRE_VARINT {
            gt = fc.read_varint().unwrap_or(0);
        } else if fc.skip_field(fw).is_err() {
            break;
        }
    }
    gt
}

fn diag_layer(data: &[u8]) {
    let mut name = "";
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
                if lc.skip_field(wire_type).is_err() {
                    break;
                }
            }
        }
    }

    let poly_count = feature_ranges
        .iter()
        .filter(|fdata| diag_geom_type(fdata) == 3)
        .count();
    println!(
        "  Layer '{name}': {n} features ({poly_count} polygons)",
        n = feature_ranges.len()
    );

    for (fi, fdata) in feature_ranges.iter().enumerate() {
        let mut geom_type: u64 = 0;
        let mut fid: u64 = 0;
        // Repeated packed geometry fields concatenate, as in tile_detail.
        let mut geometry: Vec<u8> = Vec::new();
        let mut fc = Cursor::new(fdata);
        while let Ok(Some((ff, fw))) = fc.read_tag() {
            match (ff, fw) {
                (1, WIRE_VARINT) => fid = fc.read_varint().unwrap_or(0),
                (3, WIRE_VARINT) => geom_type = fc.read_varint().unwrap_or(0),
                (4, WIRE_LEN) => {
                    if let Ok(bytes) = fc.read_len_delimited() {
                        geometry.extend_from_slice(bytes);
                    }
                }
                _ => {
                    if fc.skip_field(fw).is_err() {
                        break;
                    }
                }
            }
        }
        if geom_type == 3 {
            match decode::decode_polygon_rings(Words::packed(&geometry)) {
                Ok(rings) => print_rings(fi, fid, geom_type, &rings),
                Err(error) => {
                    println!(
                        "    Feature {fi}: id={fid} geom_type={geom_type} decode failed: {error}"
                    );
                }
            }
        }
    }
}

fn print_rings(fi: usize, fid: u64, geom_type: u64, rings: &[Vec<(i32, i32)>]) {
    println!(
        "    Feature {fi}: id={fid} geom_type={geom_type} rings={nr}",
        nr = rings.len()
    );
    for (ri, ring) in rings.iter().enumerate() {
        let area2x = signed_area_ring(ring);
        let area = area2x / 2;
        let winding = match ring_role(area2x) {
            Some(RingOrientation::Positive) => "CW (outer)",
            Some(RingOrientation::Negative) => "CCW (hole)",
            None => "ZERO",
        };
        let simple_str = if ring_is_simple(ring) {
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

fn signed_area_ring(ring: &[(i32, i32)]) -> i128 {
    if ring.len() < 3 {
        return 0;
    }
    let mut sum: i128 = 0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        sum += i128::from(ring[i].0) * i128::from(ring[j].1);
        sum -= i128::from(ring[j].0) * i128::from(ring[i].1);
    }
    sum
}
