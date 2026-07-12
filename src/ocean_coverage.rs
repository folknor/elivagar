//! Same-source, same-zoom one-sided ocean coverage comparison.
use crate::geometry::int_ocean::{
    IntEmitScratch, IntPoint, IntRect, Shape, Shapes, intersect_rect_into, intersect_shapes_into,
    signed_area_2x,
};
use crate::pmtiles_reader::{PmtilesReader, find_entry};
use crate::pmtiles_writer::{tile_id_to_zxy, xy_to_tile_id};
use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};
use std::collections::BTreeMap;
use std::path::Path;
type ZoomStats = (i128, Vec<i128>, (u32, u32));

pub struct CoverageConfig {
    pub zmin: u8,
    pub zmax: u8,
    pub threshold_2x: i128,
    pub layer: String,
}

pub fn coverage(file: &Path, baseline: &Path, cfg: &CoverageConfig) -> Result<bool, String> {
    let mut current = PmtilesReader::open(file).map_err(|e| e.to_string())?;
    let mut reference = PmtilesReader::open(baseline).map_err(|e| e.to_string())?;
    let runs = reference.read_all_runs().map_err(|e| e.to_string())?;
    let current_runs = current.read_all_runs().map_err(|e| e.to_string())?;
    let mut stats: BTreeMap<u8, ZoomStats> = BTreeMap::new();
    let mut failed = false;
    for run in runs {
        for delta in 0..u64::from(run.run_length) {
            let id = run.tile_id + delta;
            let (z, x, y) = tile_id_to_zxy(id);
            if z < cfg.zmin || z > cfg.zmax {
                continue;
            }
            let Some(ref_entry) = find_entry(&[run], id) else {
                continue;
            };
            let ref_tile = reference.read_tile(&ref_entry).map_err(|e| e.to_string())?;
            let ref_shapes = tile_shapes(&ref_tile, &cfg.layer)?;
            if ref_shapes.is_empty() {
                continue;
            }
            let low_shapes = find_entry(&current_runs, xy_to_tile_id(z, x, y))
                .map(|e| current.read_tile(&e).map_err(|e| e.to_string()))
                .transpose()?
                .map(|b| tile_shapes(&b, &cfg.layer))
                .transpose()?
                .unwrap_or_default();
            let lost = lost_area(&ref_shapes, &low_shapes);
            let entry = stats.entry(z).or_insert((0, Vec::new(), (x, y)));
            entry.1.push(lost);
            if lost > entry.0 {
                entry.0 = lost;
                entry.2 = (x, y);
            }
            if lost > cfg.threshold_2x {
                failed = true;
                println!("z{z}/{x}/{y}: lost {lost} 2x-pixel^2");
            }
        }
    }
    for (z, (max, mut values, (x, y))) in stats {
        values.sort_unstable();
        let p99 = values[values.len().saturating_sub(values.len() / 100 + 1)];
        println!("z{z}: max={max} p99={p99} worst={x}/{y}");
    }
    Ok(!failed)
}

fn lost_area(reference: &[Shape], low: &[Shape]) -> i128 {
    let rect = IntRect {
        min_x: 0,
        min_y: 0,
        max_x: 4096,
        max_y: 4096,
    };
    let mut scratch = IntEmitScratch::new();
    // Clip the low (under-test) shapes to the unbuffered extent once, up
    // front, so the reference loop does not re-clip them per reference piece.
    let mut low_clipped: Vec<Shape> = Vec::new();
    let mut buf = Vec::new();
    for l in low {
        intersect_rect_into(&mut scratch, l, rect, 0, &mut buf);
        low_clipped.append(&mut buf);
    }
    let mut ref_area = 0;
    let mut intersection = 0;
    let mut out = Vec::new();
    for s in reference {
        intersect_rect_into(&mut scratch, s, rect, 0, &mut buf);
        for r in &buf {
            ref_area += shape_area(r);
            for c in &low_clipped {
                intersect_shapes_into(&mut scratch, r, c, &mut out);
                intersection += out.iter().map(shape_area).sum::<i128>();
            }
        }
    }
    (ref_area - intersection).max(0)
}
fn shape_area(s: &Shape) -> i128 {
    s.iter()
        .enumerate()
        .map(|(i, r)| {
            if i == 0 {
                signed_area_2x(r).abs()
            } else {
                -signed_area_2x(r).abs()
            }
        })
        .sum()
}
fn tile_shapes(data: &[u8], wanted: &str) -> Result<Shapes, String> {
    let mut out = Vec::new();
    let mut c = Cursor::new(data);
    while let Some((f, w)) = c.read_tag().map_err(|e| e.to_string())? {
        if f == 3 && w == WIRE_LEN {
            let b = c.read_len_delimited().map_err(|e| e.to_string())?;
            decode_layer(b, wanted, &mut out)?;
        } else {
            c.skip_field(w).map_err(|e| e.to_string())?;
        }
    }
    Ok(out)
}
fn decode_layer(data: &[u8], wanted: &str, out: &mut Shapes) -> Result<(), String> {
    let mut name = "";
    let mut features = Vec::new();
    let mut c = Cursor::new(data);
    while let Some((f, w)) = c.read_tag().map_err(|e| e.to_string())? {
        match (f, w) {
            (1, WIRE_LEN) => {
                name = std::str::from_utf8(c.read_len_delimited().map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            }
            (2, WIRE_LEN) => features.push(c.read_len_delimited().map_err(|e| e.to_string())?),
            _ => c.skip_field(w).map_err(|e| e.to_string())?,
        }
    }
    if name == wanted {
        for f in features {
            if let Some(s) = decode_feature(f)? {
                out.push(s);
            }
        }
    }
    Ok(())
}
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn decode_feature(data: &[u8]) -> Result<Option<Shape>, String> {
    let mut kind = 0;
    let mut geom = Vec::new();
    let mut c = Cursor::new(data);
    while let Some((f, w)) = c.read_tag().map_err(|e| e.to_string())? {
        match (f, w) {
            (3, WIRE_VARINT) => kind = c.read_varint().map_err(|e| e.to_string())?,
            (4, WIRE_LEN) => {
                let mut g = Cursor::new(c.read_len_delimited().map_err(|e| e.to_string())?);
                while let Ok(v) = g.read_varint() {
                    geom.push(v as u32);
                }
            }
            _ => c.skip_field(w).map_err(|e| e.to_string())?,
        }
    }
    if kind != 3 {
        return Ok(None);
    }
    let mut rings: Shape = Vec::new();
    let mut current: Vec<IntPoint> = Vec::new();
    let (mut x, mut y, mut i) = (0i32, 0i32, 0usize);
    while i < geom.len() {
        let cmd = geom[i];
        i += 1;
        match cmd & 7 {
            1 => {
                // MoveTo starts a fresh ring; flush the previous one.
                if !current.is_empty() {
                    rings.push(std::mem::take(&mut current));
                }
                for _ in 0..cmd >> 3 {
                    if i + 1 >= geom.len() {
                        break;
                    }
                    x += unzig(geom[i]);
                    y += unzig(geom[i + 1]);
                    i += 2;
                    current.push(IntPoint::new(x, y));
                }
            }
            2 => {
                for _ in 0..cmd >> 3 {
                    if i + 1 >= geom.len() {
                        break;
                    }
                    x += unzig(geom[i]);
                    y += unzig(geom[i + 1]);
                    i += 2;
                    current.push(IntPoint::new(x, y));
                }
            }
            // ClosePath does not move the cursor (MVT spec 4.3.3.3); the ring
            // stays open and is flushed on the next MoveTo or at the end.
            7 => {}
            _ => return Err("invalid MVT polygon command".into()),
        }
    }
    if !current.is_empty() {
        rings.push(current);
    }
    Ok((!rings.is_empty()).then_some(rings))
}
#[allow(clippy::cast_possible_wrap)]
fn unzig(v: u32) -> i32 {
    ((v >> 1) as i32) ^ (-((v & 1) as i32))
}
