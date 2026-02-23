// Tile generation pipeline orchestrator.
//
// Reads an OSM PBF file and produces a Shortbread-schema PMTiles v3 archive.
// Pipeline:
//   Phase 1+2: Single-pass PBF read — build node/way indices AND process features
//   Phase 3:   External merge sort by Hilbert tile ID
//   Phase 4:   Tile assembly (MVT encode + gzip) + PMTiles write


use crate::geometry::{
    self, ClipRect, MercBbox, Point, BUFFER_FRACTION, ensure_cw_tile, ensure_ccw_tile, merc_bbox,
};
use crate::multipolygon::{self, MemberWay};
use crate::mvt::{self, GeomType, LayerBuilder};
use crate::node_index::NodeIndex;
use crate::ocean;
use crate::pmtiles_writer::{self, PmtilesConfig, PmtilesWriter};
use crate::shortbread::{self, AttrValue, GeomExpect, Layer, LayerMatch, OsmGeomType, Tags};
use crate::sort::{self, SortRecord, SortWriter};
use crate::way_index::WayIndex;
use crate::wire_format::{encode_attrs_bytes, encode_feature_data_with_attrs, add_feature_to_layer};

use flate2::write::GzEncoder;
use flate2::Compression;
use pbfhogg::{Element, ElementReader, RelMemberType};

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

pub struct TilegenConfig {
    pub pbf_path: PathBuf,
    pub output_path: PathBuf,
    pub tmp_dir: PathBuf,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub ocean_shapefile: Option<PathBuf>,
    /// Skip to a later phase, reusing checkpoint data from a previous run.
    /// Valid values: "ocean", "sort".
    pub skip_to: Option<String>,
    /// Keep tile blob in memory instead of streaming to a temp file.
    pub in_memory: bool,
}

const CHECKPOINT_FILE: &str = "checkpoint.txt";
const SORT_CHUNKS_DIR: &str = "sort_chunks";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
pub fn run(config: &TilegenConfig) {
    let total_start = Instant::now();
    let skip = config.skip_to.as_deref().unwrap_or("");

    eprintln!("=== Tilegen: {} → {}", config.pbf_path.display(), config.output_path.display());
    eprintln!("    Zoom range: z{}–z{}", config.min_zoom, config.max_zoom);
    eprintln!("    Tmp dir:    {}", config.tmp_dir.display());
    if !skip.is_empty() {
        eprintln!("    Skip to:    {skip}");
    }

    // --- Phase 1+2: PBF read + feature processing ---
    let phase12_elapsed;
    let ocean_elapsed;

    let sort_reader = if skip == "sort" {
        // Skip straight to sort — read all existing chunks
        phase12_elapsed = None;
        ocean_elapsed = None;
        eprintln!("--- Skipping to sort (using existing chunks) ---");
        None
    } else {
        let mut sort_writer = if skip.is_empty() {
            // Full run: clean tmp dir and run PBF phase
            drop(std::fs::remove_dir_all(&config.tmp_dir));
            std::fs::create_dir_all(&config.tmp_dir).expect("failed to create tmp dir");

            let phase12_start = Instant::now();
            let (sw, bounds_out) = phase_read_and_process(config);
            phase12_elapsed = Some(phase12_start.elapsed());
            save_checkpoint(&config.tmp_dir, &bounds_out, sw.chunk_count());
            sw
        } else {
            // --skip-to ocean: load checkpoint, resume from PBF chunks
            let (_, pbf_chunks) = load_checkpoint(&config.tmp_dir);
            eprintln!("--- Skipping PBF phase ({pbf_chunks} chunks from checkpoint) ---");
            phase12_elapsed = None;
            sort::SortWriter::resume(&config.tmp_dir.join(SORT_CHUNKS_DIR), 1024 * 1024 * 1024, pbf_chunks)
                .expect("failed to resume sort writer")
        };

        // Load data_bounds (needed for ocean, always available from checkpoint or just computed)
        let (data_bounds, _) = load_checkpoint(&config.tmp_dir);

        // --- Ocean shapefile processing ---
        ocean_elapsed = if let Some(ref ocean_path) = config.ocean_shapefile {
            let ocean_start = Instant::now();
            eprintln!("--- Ocean shapefile ---");
            let ocean_features = ocean::process_ocean_shapefile(
                ocean_path, &data_bounds, config.min_zoom, config.max_zoom, &mut sort_writer,
            );
            let elapsed = ocean_start.elapsed();
            eprintln!("  {ocean_features} features in {elapsed:.2?}");
            Some((elapsed, ocean_features))
        } else {
            None
        };

        Some(sort_writer)
    };

    // --- Phase 3: Sort ---
    let phase3_start = Instant::now();
    eprintln!("--- Sort ---");
    let mut sort_reader = if let Some(sw) = sort_reader {
        sw.finish().expect("sort finish failed")
    } else {
        sort::SortReader::from_dir(&config.tmp_dir.join(SORT_CHUNKS_DIR)).expect("failed to open sort chunks")
    };
    let phase3_elapsed = phase3_start.elapsed();

    // --- Phase 4: Tile assembly + PMTiles write ---
    let phase4_start = Instant::now();
    eprintln!("--- Tile assembly ---");
    let (features_read, tiles_written) = phase_assemble(&mut sort_reader, config);
    let phase4_elapsed = phase4_start.elapsed();

    let total = total_start.elapsed();

    // Machine-readable summary (all times in milliseconds)
    eprintln!("---");
    eprintln!("total_ms={}", total.as_millis());
    if let Some(p12) = phase12_elapsed {
        eprintln!("phase12_ms={}", p12.as_millis());
    }
    if let Some((oe, of)) = ocean_elapsed {
        eprintln!("ocean_ms={}", oe.as_millis());
        eprintln!("ocean_features={of}");
    }
    eprintln!("phase3_ms={}", phase3_elapsed.as_millis());
    eprintln!("phase4_ms={}", phase4_elapsed.as_millis());
    eprintln!("features={features_read}");
    eprintln!("tiles={tiles_written}");
    if let Ok(meta) = std::fs::metadata(&config.output_path) {
        eprintln!("output_bytes={}", meta.len());
    }
}

fn save_checkpoint(tmp_dir: &std::path::Path, bounds: &MercBbox, chunk_count: usize) {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let content = format!(
        "{} {} {} {} {}",
        bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y, chunk_count
    );
    std::fs::write(path, content).expect("failed to save checkpoint");
}

fn load_checkpoint(tmp_dir: &std::path::Path) -> (MercBbox, usize) {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("No checkpoint in {}: {e}. Run a full tilegen first.", tmp_dir.display()));
    let parts: Vec<&str> = content.split_whitespace().collect();
    assert!(parts.len() == 5, "Invalid checkpoint format");
    let bounds = MercBbox {
        min_x: parts[0].parse().expect("parse min_x"),
        min_y: parts[1].parse().expect("parse min_y"),
        max_x: parts[2].parse().expect("parse max_x"),
        max_y: parts[3].parse().expect("parse max_y"),
    };
    let chunks: usize = parts[4].parse().expect("parse chunk count");
    (bounds, chunks)
}

// ---------------------------------------------------------------------------
// Phase 1+2: Single-pass PBF read + feature processing
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn phase_read_and_process(config: &TilegenConfig) -> (SortWriter, MercBbox) {
    eprintln!("\n--- Phase 1+2: Reading PBF + processing features ---");

    let mut sort_writer =
        SortWriter::new(&config.tmp_dir.join("sort_chunks"), 1_073_741_824)
            .expect("failed to create sort writer");

    let reader =
        ElementReader::from_path(&config.pbf_path).expect("failed to open PBF");

    let idx_dir = &config.tmp_dir;
    let mut node_index =
        NodeIndex::create(&idx_dir.join("nodes.idx")).expect("failed to create node index");
    let mut way_index =
        WayIndex::create(idx_dir).expect("failed to create way index");

    let mut node_count: u64 = 0;
    let mut way_count: u64 = 0;
    let mut rel_count: u64 = 0;
    let mut features_emitted: u64 = 0;
    let mut way_index_finalized = false;

    // Track data extent for ocean shapefile filtering
    let mut min_lat_e7: i32 = i32::MAX;
    let mut max_lat_e7: i32 = i32::MIN;
    let mut min_lon_e7: i32 = i32::MAX;
    let mut max_lon_e7: i32 = i32::MIN;

    let min_z = config.min_zoom;
    let max_z = config.max_zoom;

    // Way batch for parallel geometry processing (P1 optimization)
    let mut way_batch: Vec<MatchedWay> = Vec::with_capacity(WAY_BATCH_SIZE);

    reader
        .for_each_pipelined(|element| match element {
            Element::Node(node) => {
                node_count += 1;
                let lat_e7 = node.decimicro_lat();
                let lon_e7 = node.decimicro_lon();
                node_index.put(node.id(), lat_e7, lon_e7);

                min_lat_e7 = min_lat_e7.min(lat_e7);
                max_lat_e7 = max_lat_e7.max(lat_e7);
                min_lon_e7 = min_lon_e7.min(lon_e7);
                max_lon_e7 = max_lon_e7.max(lon_e7);

                // Process point features (skip tagless nodes)
                if node.tags().next().is_some() {
                    let tags_vec: Vec<(&str, &str)> = node.tags().collect();
                    let mut node_records = Vec::new();
                    #[allow(clippy::cast_sign_loss)]
                    let n = process_node(
                        node.id() as u64, lat_e7, lon_e7,
                        &tags_vec, min_z, max_z, &mut node_records,
                    );
                    for r in node_records {
                        sort_writer.push(r).expect("sort push failed");
                    }
                    features_emitted += n;
                }
            }
            Element::DenseNode(node) => {
                node_count += 1;
                let lat_e7 = node.decimicro_lat();
                let lon_e7 = node.decimicro_lon();
                node_index.put(node.id, lat_e7, lon_e7);

                min_lat_e7 = min_lat_e7.min(lat_e7);
                max_lat_e7 = max_lat_e7.max(lat_e7);
                min_lon_e7 = min_lon_e7.min(lon_e7);
                max_lon_e7 = max_lon_e7.max(lon_e7);

                if node.tags().next().is_some() {
                    let tags_vec: Vec<(&str, &str)> = node.tags().collect();
                    let mut node_records = Vec::new();
                    #[allow(clippy::cast_sign_loss)]
                    let n = process_node(
                        node.id as u64, lat_e7, lon_e7,
                        &tags_vec, min_z, max_z, &mut node_records,
                    );
                    for r in node_records {
                        sort_writer.push(r).expect("sort push failed");
                    }
                    features_emitted += n;
                }
            }
            Element::Way(way) => {
                way_count += 1;

                // Resolve geometry (fast: mmap read)
                let mut coords_e7: Vec<(i32, i32)> = Vec::new();
                for node_id in way.refs() {
                    if let Some(c) = node_index.get(node_id) {
                        coords_e7.push(c);
                    }
                }
                if coords_e7.is_empty() {
                    return;
                }

                // Store for relation resolution (fast: mmap write)
                way_index.put(way.id(), &coords_e7);

                // Tag matching (fast: ~3-5% of time, needs PBF borrowed data)
                let tags_vec: Vec<(&str, &str)> = way.tags().collect();
                if tags_vec.is_empty() {
                    return;
                }

                let is_closed = coords_e7.len() >= 4
                    && coords_e7.first() == coords_e7.last();
                let geom_type = if is_closed { OsmGeomType::ClosedWay } else { OsmGeomType::OpenWay };
                let tag_helper = Tags(&tags_vec);
                let matches = shortbread::match_element(&tag_helper, geom_type);
                if matches.is_empty() {
                    return;
                }

                // Batch for parallel geometry processing
                #[allow(clippy::cast_sign_loss)]
                way_batch.push(MatchedWay {
                    osm_id: way.id() as u64,
                    coords_e7,
                    matches,
                    is_closed,
                });

                if way_batch.len() >= WAY_BATCH_SIZE {
                    let batch = std::mem::replace(&mut way_batch, Vec::with_capacity(WAY_BATCH_SIZE));
                    features_emitted += flush_way_batch(batch, min_z, max_z, &mut sort_writer);
                }
            }
            Element::Relation(rel) => {
                // Flush remaining way batch at the way→relation transition
                if !way_batch.is_empty() {
                    let batch = std::mem::replace(&mut way_batch, Vec::with_capacity(WAY_BATCH_SIZE));
                    features_emitted += flush_way_batch(batch, min_z, max_z, &mut sort_writer);
                }

                if !way_index_finalized {
                    way_index.finish_writing().expect("failed to finalize way index");
                    way_index_finalized = true;
                    eprintln!("  Nodes: {node_count}, Ways: {way_count}, Features so far: {features_emitted}");
                    eprintln!("  Way index finalized, processing relations...");
                }
                rel_count += 1;

                let tags_vec: Vec<(&str, &str)> = rel.tags().collect();
                if tags_vec.is_empty() {
                    return;
                }

                let mut rel_records = Vec::new();
                #[allow(clippy::cast_sign_loss)]
                let n = process_relation(
                    rel.id() as u64, &rel, &tags_vec,
                    &way_index, min_z, max_z, &mut rel_records,
                );
                for r in rel_records {
                    sort_writer.push(r).expect("sort push failed");
                }
                features_emitted += n;
            }
        })
        .expect("PBF read failed");

    // Flush any remaining way batch (in case PBF ends with ways and no relations)
    if !way_batch.is_empty() {
        features_emitted += flush_way_batch(way_batch, min_z, max_z, &mut sort_writer);
    }

    eprintln!("  Nodes: {node_count}, Ways: {way_count}, Relations: {rel_count}");
    eprintln!("  Total features emitted: {features_emitted}");

    // Compute data extent in Mercator [0,1] with generous buffer for ocean overlap
    let data_bounds = if min_lat_e7 < max_lat_e7 {
        let sw = geometry::project_e7(min_lat_e7, min_lon_e7);
        let ne = geometry::project_e7(max_lat_e7, max_lon_e7);
        // Add ~1 degree buffer (in Mercator space, roughly 1/360 ≈ 0.003)
        let buf = 0.01;
        MercBbox {
            min_x: (sw.x - buf).max(0.0),
            min_y: (ne.y - buf).max(0.0),  // ne.y < sw.y in Mercator [0,1]
            max_x: (ne.x + buf).min(1.0),
            max_y: (sw.y + buf).min(1.0),
        }
    } else {
        // No nodes — full world
        MercBbox { min_x: 0.0, min_y: 0.0, max_x: 1.0, max_y: 1.0 }
    };
    eprintln!("  Data bounds (merc): x[{:.4}–{:.4}] y[{:.4}–{:.4}]",
        data_bounds.min_x, data_bounds.max_x, data_bounds.min_y, data_bounds.max_y);

    (sort_writer, data_bounds)
}

// ---------------------------------------------------------------------------
// Node processing (point layers)
// ---------------------------------------------------------------------------

fn process_node(
    osm_id: u64,
    lat_e7: i32,
    lon_e7: i32,
    tags: &[(&str, &str)],
    min_zoom: u8,
    max_zoom: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let tag_helper = Tags(tags);
    let matches = shortbread::match_element(&tag_helper, OsmGeomType::Node);
    if matches.is_empty() {
        return 0;
    }

    let p = geometry::project_e7(lat_e7, lon_e7);
    let mut count: u64 = 0;

    for m in &matches {
        let z_lo = m.min_zoom.max(min_zoom);
        let z_hi = m.max_zoom.min(max_zoom);
        if z_lo > z_hi {
            continue;
        }

        let pbbox = MercBbox { min_x: p.x, min_y: p.y, max_x: p.x, max_y: p.y };
        for z in z_lo..=z_hi {
            let attrs_bytes = encode_attrs_bytes(&m.attrs, z);
            geometry::for_each_tile_in_bbox(&pbbox, z, |tx, ty| {
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                let (px, py) = geometry::merc_to_tile_px(&p, tx, ty, z);
                let geom_cmds = mvt::encode_point(px, py);
                let data = encode_feature_data_with_attrs(osm_id, GeomType::Point, &geom_cmds, &attrs_bytes);
                let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
                records.push(SortRecord { key, data });
                count += 1;
            });
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Way processing (line + polygon layers) — parallel batch processing (P1)
// ---------------------------------------------------------------------------

/// A way that passed tag matching, with owned data ready for parallel geometry processing.
struct MatchedWay {
    osm_id: u64,
    coords_e7: Vec<(i32, i32)>,
    matches: Vec<LayerMatch>,
    is_closed: bool,
}

const WAY_BATCH_SIZE: usize = 8192;

/// Process a batch of matched ways in parallel and push results to sort writer.
fn flush_way_batch(
    batch: Vec<MatchedWay>,
    min_zoom: u8,
    max_zoom: u8,
    sort_writer: &mut SortWriter,
) -> u64 {
    use rayon::prelude::*;

    let results: Vec<Vec<SortRecord>> = batch
        .into_par_iter()
        .map(|mut way| process_matched_way(&mut way, min_zoom, max_zoom))
        .collect();

    let mut count: u64 = 0;
    for way_records in results {
        count += way_records.len() as u64;
        for record in way_records {
            sort_writer.push(record).expect("sort push failed");
        }
    }
    count
}

/// Process a pre-matched way's geometry. Called from rayon worker threads.
/// Does the CPU-heavy work: projection, simplification, clipping, MVT encoding.
fn process_matched_way(
    way: &mut MatchedWay,
    min_zoom: u8,
    max_zoom: u8,
) -> Vec<SortRecord> {
    // Project to Mercator
    let merc: Vec<Point> = way.coords_e7
        .iter()
        .map(|&(lat, lon)| geometry::project_e7(lat, lon))
        .collect();

    let bbox = merc_bbox(&merc);

    // Enrich polygon matches with area-dependent data (way_area, min_zoom overrides)
    if way.is_closed {
        let area_m2 = geometry::area_sq_meters(&merc);
        enrich_polygon_matches(&mut way.matches, area_m2);
    }

    let mut records = Vec::new();

    for m in &way.matches {
        let z_lo = m.min_zoom.max(min_zoom);
        let z_hi = m.max_zoom.min(max_zoom);
        if z_lo > z_hi {
            continue;
        }

        match m.geom_expect {
            GeomExpect::Point => {
                emit_point_feature(way.osm_id, &merc, &bbox, m, z_lo, z_hi, &mut records);
            }
            GeomExpect::PolygonCentroid | GeomExpect::PolygonPointOnSurface => {
                emit_centroid_feature(way.osm_id, &merc, &bbox, m, z_lo, z_hi, &mut records);
            }
            GeomExpect::Line => {
                emit_line_feature(way.osm_id, &merc, &bbox, m, z_lo, z_hi, &mut records);
            }
            GeomExpect::Polygon => {
                emit_polygon_feature(way.osm_id, &merc, &bbox, m, z_lo, z_hi, &mut records);
            }
        }
    }
    records
}

// ---------------------------------------------------------------------------
// Relation processing (multipolygon + boundary lines)
// ---------------------------------------------------------------------------

fn process_relation(
    osm_id: u64,
    rel: &pbfhogg::Relation<'_>,
    tags: &[(&str, &str)],
    way_index: &WayIndex,
    min_zoom: u8,
    max_zoom: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let tag_helper = Tags(tags);

    // Check if it's a multipolygon/boundary type
    let rel_type = tag_helper.get("type").unwrap_or("");
    let is_multipolygon = rel_type == "multipolygon" || rel_type == "boundary";

    if !is_multipolygon {
        return 0;
    }

    // Collect member ways
    let mut member_ways: Vec<MemberWay> = Vec::new();
    let mut boundary_way_coords: Vec<Vec<Point>> = Vec::new();

    for member in rel.members() {
        if member.member_type != RelMemberType::Way {
            continue;
        }
        let role = member.role().unwrap_or("").to_string();
        if let Some(coords_e7) = way_index.get(member.member_id) {
            let merc: Vec<Point> = coords_e7
                .iter()
                .map(|&(lat, lon)| geometry::project_e7(lat, lon))
                .collect();

            // Save for boundary line emission
            if tag_helper.has_value("boundary", "administrative") {
                boundary_way_coords.push(merc.clone());
            }

            member_ways.push(MemberWay { role, coords: merc });
        }
    }

    if member_ways.is_empty() {
        return 0;
    }

    let mut count: u64 = 0;

    // Assemble multipolygon
    let multi = multipolygon::assemble(&member_ways);

    if !multi.polygons.is_empty() {
        // Match as multipolygon
        let mut matches = shortbread::match_element(&tag_helper, OsmGeomType::MultiPolygon);

        // Enrich polygon matches with area-dependent data
        let total_area_m2: f64 = multi.polygons.iter()
            .map(|(outer, _)| geometry::area_sq_meters(outer))
            .sum();
        enrich_polygon_matches(&mut matches, total_area_m2);

        for m in &matches {
            let z_lo = m.min_zoom.max(min_zoom);
            let z_hi = m.max_zoom.min(max_zoom);
            if z_lo > z_hi {
                continue;
            }

            match m.geom_expect {
                GeomExpect::Polygon => {
                    // Use the first polygon's outer ring for bbox/processing
                    for (outer, inners) in &multi.polygons {
                        if outer.len() < 4 {
                            continue;
                        }
                        let bbox = merc_bbox(outer);
                        count += emit_multipolygon_feature(
                            osm_id, outer, inners, &bbox, m,
                            z_lo, z_hi, records,
                        );
                    }
                }
                GeomExpect::PolygonCentroid | GeomExpect::PolygonPointOnSurface => {
                    for (outer, _inners) in &multi.polygons {
                        if outer.len() < 4 {
                            continue;
                        }
                        let bbox = merc_bbox(outer);
                        count += emit_centroid_feature(
                            osm_id, outer, &bbox, m, z_lo, z_hi, records,
                        );
                    }
                }
                GeomExpect::Line => {
                    // Boundary lines: emit each member way as a line
                    for way_coords in &boundary_way_coords {
                        if way_coords.len() < 2 {
                            continue;
                        }
                        let bbox = merc_bbox(way_coords);
                        count += emit_line_feature(
                            osm_id, way_coords, &bbox, m, z_lo, z_hi, records,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    count
}

// ---------------------------------------------------------------------------
// Polygon enrichment (area-dependent attrs + min_zoom overrides)
// ---------------------------------------------------------------------------

/// Enrich polygon matches with geometry-dependent data.
/// - BoundaryLabels: `way_area` in hectares, min_zoom override based on area thresholds.
fn enrich_polygon_matches(matches: &mut [LayerMatch], area_m2: f64) {
    for m in matches.iter_mut() {
        if m.layer == Layer::BoundaryLabels {
            // Add way_area in hectares
            let hectares = area_m2 / 10_000.0;
            m.attrs.push(("way_area", AttrValue::Float(hectares), 0));

            // Override min_zoom based on area (Planetiler thresholds)
            let area_km2 = area_m2 / 1e6;
            let admin_level = m.attrs.iter()
                .find(|(k, _, _)| *k == "admin_level")
                .and_then(|(_, v, _)| if let AttrValue::Int(n) = v { Some(*n) } else { None })
                .unwrap_or(0);

            if admin_level == 2 && area_km2 >= 2_000_000.0 {
                m.min_zoom = 2;
            } else if area_km2 >= 700_000.0 {
                m.min_zoom = 3;
            } else if area_km2 >= 100_000.0 {
                m.min_zoom = 4;
            }
            // else stays at 5 (default from match_boundary_labels)
        }
    }
}

// ---------------------------------------------------------------------------
// Feature emission helpers
// ---------------------------------------------------------------------------

fn emit_point_feature(
    osm_id: u64,
    coords: &[Point],
    _bbox: &MercBbox,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    if coords.is_empty() {
        return 0;
    }
    let centroid = centroid_of(coords);
    let cbbox = MercBbox { min_x: centroid.x, min_y: centroid.y, max_x: centroid.x, max_y: centroid.y };
    let mut count: u64 = 0;
    for z in z_lo..=z_hi {
        let attrs_bytes = encode_attrs_bytes(&m.attrs, z);
        geometry::for_each_tile_in_bbox(&cbbox, z, |tx, ty| {
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let (px, py) = geometry::merc_to_tile_px(&centroid, tx, ty, z);
            let geom_cmds = mvt::encode_point(px, py);
            let data = encode_feature_data_with_attrs(osm_id, GeomType::Point, &geom_cmds, &attrs_bytes);
            let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
            records.push(SortRecord { key, data });
            count += 1;
        });
    }
    count
}

fn emit_centroid_feature(
    osm_id: u64,
    coords: &[Point],
    _bbox: &MercBbox,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let pt = if m.geom_expect == GeomExpect::PolygonPointOnSurface {
        geometry::point_on_surface(coords)
    } else {
        Some(centroid_of(coords))
    };
    let Some(p) = pt else { return 0 };

    let cbbox = MercBbox { min_x: p.x, min_y: p.y, max_x: p.x, max_y: p.y };
    let mut count: u64 = 0;
    for z in z_lo..=z_hi {
        let attrs_bytes = encode_attrs_bytes(&m.attrs, z);
        geometry::for_each_tile_in_bbox(&cbbox, z, |tx, ty| {
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let (px, py) = geometry::merc_to_tile_px(&p, tx, ty, z);
            let geom_cmds = mvt::encode_point(px, py);
            let data = encode_feature_data_with_attrs(osm_id, GeomType::Point, &geom_cmds, &attrs_bytes);
            let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
            records.push(SortRecord { key, data });
            count += 1;
        });
    }
    count
}

fn emit_line_feature(
    osm_id: u64,
    merc: &[Point],
    bbox: &MercBbox,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let mut count: u64 = 0;
    // Cascading simplification (P2): simplify from previous zoom's result
    let mut cascade = merc.to_vec();
    for z in (z_lo..=z_hi).rev() {
        if z < 14 {
            let tol = geometry::simplify_tolerance(z);
            cascade = geometry::simplify(&cascade, tol);
        }
        if cascade.len() < 2 {
            break;
        }
        let simplified = &cascade;
        let attrs_bytes = encode_attrs_bytes(&m.attrs, z);

        geometry::for_each_tile_in_bbox(bbox, z, |tx, ty| {
            let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
            let clipped = geometry::clip_linestring(simplified, &clip);
            for segment in &clipped {
                if segment.len() < 2 {
                    continue;
                }
                let tile_coords = geometry::to_tile_coords(segment, tx, ty, z);
                let geom_cmds = mvt::encode_linestring(&tile_coords);
                if geom_cmds.is_empty() {
                    continue;
                }
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                let data = encode_feature_data_with_attrs(osm_id, GeomType::LineString, &geom_cmds, &attrs_bytes);
                let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
                records.push(SortRecord { key, data });
                count += 1;
            }
        });
    }
    count
}

#[allow(clippy::too_many_arguments)]
fn emit_polygon_feature(
    osm_id: u64,
    merc: &[Point],
    bbox: &MercBbox,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    // Single-ring polygon (no holes)
    let mut count: u64 = 0;
    // Cascading simplification (P2): simplify from previous zoom's result
    let mut cascade = merc.to_vec();
    for z in (z_lo..=z_hi).rev() {
        if z < 14 {
            let tol = geometry::simplify_tolerance(z);
            cascade = geometry::simplify(&cascade, tol);
        }
        if cascade.len() < 4 {
            break;
        }
        let attrs_bytes = encode_attrs_bytes(&m.attrs, z);

        geometry::for_each_tile_in_bbox(bbox, z, |tx, ty| {
            let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
            let clipped = geometry::clip_polygon(&cascade, &clip);
            if clipped.len() < 3 {
                return;
            }
            let tile_coords = geometry::to_tile_coords(&clipped, tx, ty, z);
            let mut ring = tile_coords;
            if ring.first() != ring.last()
                && let Some(&first) = ring.first()
            {
                ring.push(first);
            }
            ensure_cw_tile(&mut ring);

            let geom_cmds = mvt::encode_polygon(&[&ring]);
            if geom_cmds.is_empty() {
                return;
            }
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let data = encode_feature_data_with_attrs(osm_id, GeomType::Polygon, &geom_cmds, &attrs_bytes);
            let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
            records.push(SortRecord { key, data });
            count += 1;
        });
    }
    count
}

#[allow(clippy::too_many_arguments)]
fn emit_multipolygon_feature(
    osm_id: u64,
    outer: &[Point],
    inners: &[Vec<Point>],
    bbox: &MercBbox,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let mut count: u64 = 0;
    // Cascading simplification (P2): simplify from previous zoom's result
    let mut cascade_outer = outer.to_vec();
    let mut cascade_inners: Vec<Vec<Point>> = inners.to_vec();
    for z in (z_lo..=z_hi).rev() {
        let tol = if z < 14 { geometry::simplify_tolerance(z) } else { 0.0 };
        if tol > 0.0 {
            cascade_outer = geometry::simplify(&cascade_outer, tol);
            cascade_inners = cascade_inners
                .iter()
                .map(|r| geometry::simplify(r, tol))
                .filter(|r| r.len() >= 4)
                .collect();
        }
        if cascade_outer.len() < 4 {
            break;
        }
        let simp_outer = &cascade_outer;
        let simp_inners = &cascade_inners;
        let attrs_bytes = encode_attrs_bytes(&m.attrs, z);

        geometry::for_each_tile_in_bbox(bbox, z, |tx, ty| {
            let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);

            let clipped_outer = geometry::clip_polygon(simp_outer, &clip);
            if clipped_outer.len() < 3 {
                return;
            }
            let mut outer_tc = geometry::to_tile_coords(&clipped_outer, tx, ty, z);
            if outer_tc.first() != outer_tc.last()
                && let Some(&first) = outer_tc.first()
            {
                outer_tc.push(first);
            }
            ensure_cw_tile(&mut outer_tc);

            let mut all_rings: Vec<Vec<(i32, i32)>> = vec![outer_tc];
            for inner in simp_inners {
                let clipped_inner = geometry::clip_polygon(inner, &clip);
                if clipped_inner.len() < 3 {
                    continue;
                }
                let mut inner_tc = geometry::to_tile_coords(&clipped_inner, tx, ty, z);
                if inner_tc.first() != inner_tc.last()
                    && let Some(&first) = inner_tc.first()
                {
                    inner_tc.push(first);
                }
                ensure_ccw_tile(&mut inner_tc);
                all_rings.push(inner_tc);
            }

            let ring_refs: Vec<&[(i32, i32)]> = all_rings.iter().map(Vec::as_slice).collect();
            let geom_cmds = mvt::encode_polygon(&ring_refs);
            if geom_cmds.is_empty() {
                return;
            }
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let data = encode_feature_data_with_attrs(osm_id, GeomType::Polygon, &geom_cmds, &attrs_bytes);
            let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
            records.push(SortRecord { key, data });
            count += 1;
        });
    }
    count
}

// ---------------------------------------------------------------------------
// Phase 4: Tile assembly + PMTiles write
// ---------------------------------------------------------------------------

/// A tile's features collected from the sort reader, ready for parallel encoding.
struct PendingTile {
    tile_id: u64,
    features: Vec<(u8, Vec<u8>)>, // (layer_idx, feature_data)
}

#[allow(clippy::too_many_lines)]
fn phase_assemble(sort_reader: &mut sort::SortReader, config: &TilegenConfig) -> (u64, u64) {
    let pmtiles_config = PmtilesConfig {
        min_zoom: config.min_zoom,
        max_zoom: config.max_zoom,
        bounds: (-180.0, -85.05, 180.0, 85.05),
        center: (0.0, 0.0, 2),
    };
    let mut pmtiles = if config.in_memory {
        PmtilesWriter::new(pmtiles_config)
    } else {
        PmtilesWriter::new_streaming(pmtiles_config, &config.tmp_dir)
            .expect("failed to create streaming PMTiles writer")
    };

    let mut tiles_written: u64 = 0;
    let mut features_read: u64 = 0;

    const BATCH_SIZE: usize = 4096;
    let mut batch: Vec<PendingTile> = Vec::with_capacity(BATCH_SIZE);
    let mut current = PendingTile { tile_id: u64::MAX, features: Vec::new() };

    loop {
        let record = sort_reader.next().expect("sort read error");
        let Some(r) = record else {
            if current.tile_id != u64::MAX {
                batch.push(current);
            }
            if !batch.is_empty() {
                tiles_written += flush_tile_batch(&batch, &mut pmtiles);
            }
            break;
        };
        features_read += 1;

        let tile_id = sort::tile_id_from_key(r.key);
        let layer_idx = sort::layer_from_key(r.key);

        if tile_id != current.tile_id {
            if current.tile_id != u64::MAX {
                batch.push(current);
                if batch.len() >= BATCH_SIZE {
                    tiles_written += flush_tile_batch(&batch, &mut pmtiles);
                    batch.clear();
                }
            }
            current = PendingTile { tile_id, features: Vec::new() };
        }
        current.features.push((layer_idx, r.data));
    }

    pmtiles
        .write_to(&config.output_path)
        .expect("failed to write PMTiles");

    (features_read, tiles_written)
}

/// Encode + gzip a batch of tiles in parallel, then add to PMTiles writer.
fn flush_tile_batch(batch: &[PendingTile], pmtiles: &mut PmtilesWriter) -> u64 {
    use rayon::prelude::*;

    let results: Vec<Option<(u64, Vec<u8>)>> = batch
        .par_iter()
        .map(|tile| {
            let mut layers = new_layer_slots();
            for &(layer_idx, ref data) in &tile.features {
                if (layer_idx as usize) < layers.len() {
                    add_feature_to_layer(get_or_create_layer(&mut layers, layer_idx as usize), data);
                }
            }

            let non_empty: Vec<&LayerBuilder> = layers.iter()
                .filter_map(|l| l.as_ref())
                .filter(|l| !l.is_empty())
                .collect();
            if non_empty.is_empty() {
                return None;
            }

            let mvt_data = mvt::encode_tile(&non_empty);
            if mvt_data.is_empty() {
                return None;
            }

            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(&mvt_data).expect("gzip write failed");
            let compressed = encoder.finish().expect("gzip finish failed");

            Some((tile.tile_id, compressed))
        })
        .collect();

    let mut count: u64 = 0;
    for (tile_id, compressed) in results.into_iter().flatten() {
        let (z, x, y) = pmtiles_writer::tile_id_to_zxy(tile_id);
        pmtiles.add_tile(z, x, y, &compressed).expect("failed to write tile");
        count += 1;
    }
    count
}

/// Static layer names indexed by Layer discriminant (A4 optimization).
const LAYER_NAMES: [&str; 26] = [
    "water_polygons", "water_polygons_labels", "water_lines", "water_lines_labels",
    "dam_lines", "dam_polygons", "pier_lines", "pier_polygons",
    "boundaries", "boundary_labels", "place_labels", "land",
    "sites", "buildings", "addresses", "streets",
    "street_polygons", "street_labels", "street_labels_points", "streets_polygons_labels",
    "bridges", "aerialways", "ferries", "public_transport",
    "pois", "ocean",
];

/// Create an empty slot array for lazy layer builder initialization.
fn new_layer_slots() -> Vec<Option<LayerBuilder>> {
    (0..Layer::count()).map(|_| None).collect()
}

/// Get or create a LayerBuilder at the given index.
fn get_or_create_layer(layers: &mut [Option<LayerBuilder>], idx: usize) -> &mut LayerBuilder {
    if layers[idx].is_none() {
        layers[idx] = Some(LayerBuilder::new(LAYER_NAMES[idx]));
    }
    layers[idx].as_mut().expect("just inserted")
}


// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn centroid_of(points: &[Point]) -> Point {
    if points.is_empty() {
        return Point { x: 0.0, y: 0.0 };
    }
    let mut sx = 0.0;
    let mut sy = 0.0;
    for p in points {
        sx += p.x;
        sy += p.y;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = points.len() as f64;
    Point { x: sx / n, y: sy / n }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::shortbread::{AttrValue, GeomExpect, Layer, LayerMatch};
    use std::borrow::Cow;

    /// Helper: build a BoundaryLabels match with the given admin_level and default min_zoom=5.
    fn boundary_labels_match(admin_level: i64) -> LayerMatch {
        LayerMatch {
            layer: Layer::BoundaryLabels,
            min_zoom: 5,
            max_zoom: 14,
            geom_expect: GeomExpect::PolygonPointOnSurface,
            attrs: vec![
                ("admin_level", AttrValue::Int(admin_level), 0),
                ("name", AttrValue::Str(Cow::Borrowed("TestCountry")), 0),
            ],
        }
    }

    /// admin_level=2 with area >= 2,000,000 km^2 -> min_zoom overridden to 2
    #[test]
    fn boundary_label_admin2_large_area() {
        let area_m2 = 2_000_000.0 * 1e6; // exactly 2M km^2
        let mut matches = vec![boundary_labels_match(2)];
        enrich_polygon_matches(&mut matches, area_m2);

        assert_eq!(matches[0].min_zoom, 2);
        // Also verify way_area was added (in hectares)
        let way_area_attr = matches[0].attrs.iter()
            .find(|(k, _, _)| *k == "way_area")
            .expect("way_area attr missing");
        if let AttrValue::Float(h) = way_area_attr.1 {
            let expected_hectares = area_m2 / 10_000.0;
            assert!((h - expected_hectares).abs() < 0.01, "way_area hectares mismatch");
        } else {
            panic!("way_area should be Float");
        }
    }

    /// admin_level=4 with area >= 700,000 km^2 -> min_zoom overridden to 3
    #[test]
    fn boundary_label_admin4_700k_km2() {
        let area_m2 = 700_000.0 * 1e6;
        let mut matches = vec![boundary_labels_match(4)];
        enrich_polygon_matches(&mut matches, area_m2);

        assert_eq!(matches[0].min_zoom, 3);
    }

    /// admin_level=4 with area >= 100,000 km^2 (but < 700,000) -> min_zoom overridden to 4
    #[test]
    fn boundary_label_admin4_100k_km2() {
        let area_m2 = 150_000.0 * 1e6; // 150k km^2
        let mut matches = vec![boundary_labels_match(4)];
        enrich_polygon_matches(&mut matches, area_m2);

        assert_eq!(matches[0].min_zoom, 4);
    }

    /// admin_level=4 with area < 100,000 km^2 -> min_zoom stays at default (5)
    #[test]
    fn boundary_label_admin4_small_area() {
        let area_m2 = 50_000.0 * 1e6; // 50k km^2 — below 100k threshold
        let mut matches = vec![boundary_labels_match(4)];
        enrich_polygon_matches(&mut matches, area_m2);

        assert_eq!(matches[0].min_zoom, 5, "small area should keep default min_zoom=5");
    }

    /// Non-BoundaryLabels layer should be completely unchanged by enrich_polygon_matches.
    #[test]
    fn non_boundary_labels_unchanged() {
        let mut matches = vec![LayerMatch {
            layer: Layer::Buildings,
            min_zoom: 14,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: vec![],
        }];
        let original_min_zoom = matches[0].min_zoom;
        let original_attr_count = matches[0].attrs.len();

        enrich_polygon_matches(&mut matches, 9_999_999_999.0);

        assert_eq!(matches[0].min_zoom, original_min_zoom);
        assert_eq!(matches[0].attrs.len(), original_attr_count, "attrs should not be modified");
    }
}

