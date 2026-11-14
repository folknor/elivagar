//! `elivagar diag`: per-ring winding report for one tile.
//!
//! Every polygon feature across all layers, decoded through the shared
//! command decoder under the strict polygon grammar. Rings are printed raw,
//! zero-area ones included - grouping would hide exactly the degenerate rings
//! a winding diagnosis looks for. A feature that does not decode prints its
//! error instead of partial rings.

use crate::PipelineError;
use std::path::Path;

use crate::archive::Archive;
use crate::geometry::polygon::{RingOrientation, ring_is_simple, ring_role, signed_area_2x};
use crate::mvt::decode::{self, Words};
use crate::mvt::message;
use crate::tile_detail::Strictness;

/// Print the ring report for tile `z/x/y` of the archive at `path`.
pub fn diag_tile(path: &Path, z: u8, x: u32, y: u32) -> Result<(), PipelineError> {
    crate::svg::validate_tile_grid_coordinates(z, x, y, 1, 1)
        .map_err(|e| PipelineError::config("invalid tile coordinates").with_cause(e))?;
    let reader = Archive::open(path)?;
    reader.tile_decoder()?;
    let runs = reader.read_all_runs()?;
    let blob = runs
        .find(crate::tiling::TileId::from_coord(z, x, y)?)
        .ok_or_else(|| PipelineError::failed("tile not found"))?;
    let raw = reader.read_tile(blob)?;

    println!(
        "Tile z{z}/{x}/{y} - {raw_len} bytes decompressed",
        raw_len = raw.len()
    );

    // Tolerant like the viewer: any producer's tile, unknown fields skipped,
    // a malformed tile or layer message fails the command.
    for layer in message::parse_tile(&raw, Strictness::Tolerant)? {
        diag_layer(&layer);
    }
    Ok(())
}

fn diag_layer(layer: &message::LayerMessage<'_>) {
    let features: Vec<_> = layer
        .features
        .iter()
        .map(|bytes| message::parse_feature(bytes, Strictness::Tolerant))
        .collect();
    let poly_count = features
        .iter()
        .filter(|feature| matches!(feature, Ok(feature) if feature.geom_type == 3))
        .count();
    println!(
        "  Layer '{name}': {n} features ({poly_count} polygons)",
        name = layer.name,
        n = features.len()
    );

    for (fi, feature) in features.iter().enumerate() {
        let feature = match feature {
            Ok(feature) => feature,
            Err(error) => {
                println!("    Feature {fi}: message decode failed: {error}");
                continue;
            }
        };
        let geom_type = feature.geom_type;
        let fid = feature.id.unwrap_or(0);
        if geom_type == 3 {
            let geometry = feature.geometry.as_deref().unwrap_or_default();
            match decode::decode_polygon_rings(Words::packed(geometry)) {
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

fn print_rings(fi: usize, fid: u64, geom_type: u8, rings: &[Vec<(i32, i32)>]) {
    println!(
        "    Feature {fi}: id={fid} geom_type={geom_type} rings={nr}",
        nr = rings.len()
    );
    for (ri, ring) in rings.iter().enumerate() {
        let area2x = signed_area_2x(ring.iter().copied());
        let area = area2x.get() / 2;
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

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_coordinates_are_config_before_open() {
        let error = super::diag_tile(std::path::Path::new("not-opened.pmtiles"), 0, 1, 0)
            .expect_err("invalid x");
        assert_eq!(error.kind(), crate::ErrorKind::Config);
    }
}
