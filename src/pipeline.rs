// Tile generation pipeline orchestrator.
//
// Reads an OSM PBF file and produces a Shortbread-schema PMTiles v3 archive.
// Pipeline:
//   Phase 1+2: Single-pass PBF read — build node/way indices AND process features
//   Phase 3:   External merge sort by Hilbert tile ID
//   Phase 4:   Tile assembly (MVT encode + gzip) + PMTiles write


use crate::geometry::{
    self, ClipRect, MercBbox, Point, BUFFER_FRACTION, close_and_orient_cw, close_and_orient_ccw, merc_bbox,
};
use crate::multipolygon::{self, MemberWay, WayRole};
use crate::mvt::{self, GeomType, LayerBuilder};
use crate::node_index::{NodeIndex, NodeIndexReader};
use crate::ocean;
use crate::pmtiles_writer::{self, PmtilesConfig, PmtilesWriter};
use crate::shortbread::{self, AttrValue, GeomExpect, Layer, LayerMatch, OsmGeomType, Tags};
use smallvec::SmallVec;
use crate::sort::{self, SortRecord, SortWriter};
use crate::way_index::WayIndex;
use crate::wire_format::{encode_attrs_bytes, encode_feature_data_with_attrs, add_feature_to_layer};

use flate2::write::GzEncoder;
use flate2::Compression;
use pbfhogg::{Element, ElementReader, MemberId};

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

/// Pipeline error type. Stringly-typed because no caller inspects variants —
/// errors are only displayed or propagated. An enum would add boilerplate for no benefit.
#[derive(Debug)]
pub struct PipelineError(pub String);

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PipelineError {}

impl From<std::io::Error> for PipelineError {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}

impl From<String> for PipelineError {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which pipeline phase to skip to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipTo {
    /// Skip PBF read, reuse PBF chunks, re-run ocean + sort + assemble.
    Ocean,
    /// Skip PBF + ocean, reuse all chunks, re-run sort + assemble.
    Sort,
}

pub struct TilegenConfig {
    pub pbf_path: PathBuf,
    pub output_path: PathBuf,
    pub tmp_dir: PathBuf,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub ocean_shapefile: Option<PathBuf>,
    /// Simplified ocean shapefile for z0-7 (fewer vertices, faster at low zooms).
    /// When set, `ocean_shapefile` is used only for z8+.
    pub ocean_simplified_shapefile: Option<PathBuf>,
    /// Skip to a later phase, reusing checkpoint data from a previous run.
    pub skip_to: Option<SkipTo>,
    /// Keep tile blob in memory instead of streaming to a temp file.
    pub in_memory: bool,
}

const CHECKPOINT_FILE: &str = "checkpoint.txt";
const LAND_MASK_FILE: &str = "land_mask.bin";
const SORT_CHUNKS_DIR: &str = "sort_chunks";
/// Target memory budget per sort chunk (1 GB). Records buffer in memory up to
/// this limit, then flush as a sorted chunk file to disk.
const SORT_CHUNK_SIZE: usize = 1 << 30;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn run(config: &TilegenConfig) -> Result<(), PipelineError> {
    let total_start = Instant::now();
    let skip = config.skip_to;

    eprintln!("=== Tilegen: {} → {}", config.pbf_path.display(), config.output_path.display());
    eprintln!("    Zoom range: z{}–z{}", config.min_zoom, config.max_zoom);
    eprintln!("    Tmp dir:    {}", config.tmp_dir.display());
    if let Some(s) = skip {
        eprintln!("    Skip to:    {s:?}");
    }

    // --- Phase 1+2: PBF read + feature processing ---
    let phase12_elapsed;
    let ocean_elapsed;

    let sort_reader = if skip == Some(SkipTo::Sort) {
        // Skip straight to sort — read all existing chunks
        phase12_elapsed = None;
        ocean_elapsed = None;
        eprintln!("--- Skipping to sort (using existing chunks) ---");
        None
    } else {
        let (mut sort_writer, land_mask) = if skip.is_none() {
            // Full run: clean tmp dir and run PBF phase
            drop(std::fs::remove_dir_all(&config.tmp_dir));
            std::fs::create_dir_all(&config.tmp_dir)?;

            let phase12_start = Instant::now();
            let (sw, bounds_out, mask) = phase_read_and_process(config)?;
            phase12_elapsed = Some(phase12_start.elapsed());
            save_checkpoint(&config.tmp_dir, &bounds_out, sw.chunk_count())?;
            save_land_mask(&config.tmp_dir, &mask)?;
            (sw, Some(mask))
        } else {
            // --skip-to ocean: load checkpoint, resume from PBF chunks
            let (_, pbf_chunks) = load_checkpoint(&config.tmp_dir)?;
            eprintln!("--- Skipping PBF phase ({pbf_chunks} chunks from checkpoint) ---");
            phase12_elapsed = None;
            let sw = sort::SortWriter::resume(&config.tmp_dir.join(SORT_CHUNKS_DIR), SORT_CHUNK_SIZE, pbf_chunks)?;
            let mask = load_land_mask(&config.tmp_dir);
            if mask.is_none() {
                eprintln!("  No land mask found — ocean filtering disabled");
            }
            (sw, mask)
        };

        // Load data_bounds (needed for ocean, always available from checkpoint or just computed)
        let (data_bounds, _) = load_checkpoint(&config.tmp_dir)?;
        let mask_ref = land_mask.as_ref();

        // --- Ocean shapefile processing ---
        // When a simplified shapefile is provided, use it for z0-7 and the
        // full-resolution shapefile for z8+. Otherwise use the full-res for all zooms.
        ocean_elapsed = if let Some(ref ocean_path) = config.ocean_shapefile {
            let ocean_start = Instant::now();
            eprintln!("--- Ocean shapefile ---");
            let mut ocean_features: u64 = 0;

            if let Some(ref simplified_path) = config.ocean_simplified_shapefile {
                let simplified_max = config.max_zoom.min(7);
                if config.min_zoom <= simplified_max {
                    eprintln!("  Simplified (z{}–z{}):", config.min_zoom, simplified_max);
                    ocean_features += ocean::process_ocean_shapefile(
                        simplified_path, &data_bounds, config.min_zoom, simplified_max, mask_ref, &mut sort_writer,
                    );
                }
                if config.max_zoom >= 8 {
                    let full_min = config.min_zoom.max(8);
                    eprintln!("  Full-resolution (z{full_min}–z{}):", config.max_zoom);
                    ocean_features += ocean::process_ocean_shapefile(
                        ocean_path, &data_bounds, full_min, config.max_zoom, mask_ref, &mut sort_writer,
                    );
                }
            } else {
                ocean_features = ocean::process_ocean_shapefile(
                    ocean_path, &data_bounds, config.min_zoom, config.max_zoom, mask_ref, &mut sort_writer,
                );
            }

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
        sw.finish()?
    } else {
        sort::SortReader::from_dir(&config.tmp_dir.join(SORT_CHUNKS_DIR))?
    };
    let phase3_elapsed = phase3_start.elapsed();

    // --- Phase 4: Tile assembly + PMTiles write ---
    let phase4_start = Instant::now();
    eprintln!("--- Tile assembly ---");
    let (features_read, tiles_written, unique_tiles) = phase_assemble(&mut sort_reader, config)?;
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
    eprintln!("unique_tiles={unique_tiles}");
    if let Ok(meta) = std::fs::metadata(&config.output_path) {
        eprintln!("output_bytes={}", meta.len());
    }
    Ok(())
}

fn save_checkpoint(tmp_dir: &std::path::Path, bounds: &MercBbox, chunk_count: usize) -> Result<(), PipelineError> {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let content = format!(
        "{} {} {} {} {}",
        bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y, chunk_count
    );
    std::fs::write(path, content)?;
    Ok(())
}

fn save_land_mask(tmp_dir: &std::path::Path, mask: &geometry::LandMask) -> Result<(), PipelineError> {
    std::fs::write(tmp_dir.join(LAND_MASK_FILE), mask.to_bytes())?;
    Ok(())
}

fn load_land_mask(tmp_dir: &std::path::Path) -> Option<geometry::LandMask> {
    let data = std::fs::read(tmp_dir.join(LAND_MASK_FILE)).ok()?;
    geometry::LandMask::from_bytes(&data)
}

fn load_checkpoint(tmp_dir: &std::path::Path) -> Result<(MercBbox, usize), PipelineError> {
    let path = tmp_dir.join(CHECKPOINT_FILE);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| PipelineError(format!("No checkpoint in {}: {e}. Run a full tilegen first.", tmp_dir.display())))?;
    let parts: Vec<&str> = content.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(PipelineError(format!("Invalid checkpoint format: expected 5 fields, got {}", parts.len())));
    }
    let parse = |s: &str, name: &str| -> Result<f64, PipelineError> {
        s.parse().map_err(|e| PipelineError(format!("checkpoint parse {name}: {e}")))
    };
    let bounds = MercBbox {
        min_x: parse(parts[0], "min_x")?,
        min_y: parse(parts[1], "min_y")?,
        max_x: parse(parts[2], "max_x")?,
        max_y: parse(parts[3], "max_y")?,
    };
    let chunks: usize = parts[4].parse()
        .map_err(|e| PipelineError(format!("checkpoint parse chunk count: {e}")))?;
    Ok((bounds, chunks))
}

// ---------------------------------------------------------------------------
// Phase 1+2: Single-pass PBF read + feature processing
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
#[hotpath::measure]
fn phase_read_and_process(config: &TilegenConfig) -> Result<(SortWriter, MercBbox, geometry::LandMask), PipelineError> {
    eprintln!("\n--- Phase 1+2: Reading PBF + processing features ---");

    let mut sort_writer =
        SortWriter::new(&config.tmp_dir.join(SORT_CHUNKS_DIR), SORT_CHUNK_SIZE)?;

    let reader =
        ElementReader::from_path(&config.pbf_path)
            .map_err(|e| PipelineError(format!("failed to open PBF: {e}")))?;

    let idx_dir = &config.tmp_dir;
    // Option so we can consume it via .take() on first Way element.
    let mut node_index_opt: Option<NodeIndex> =
        Some(NodeIndex::create(&idx_dir.join("nodes.idx"))?);
    let mut node_reader: Option<NodeIndexReader> = None;
    let mut way_index =
        WayIndex::create(idx_dir)?;

    let mut node_count: u64 = 0;
    let mut way_count: u64 = 0;
    let mut rel_count: u64 = 0;
    let mut features_emitted: u64 = 0;
    let mut way_index_finalized = false;
    let land_mask = geometry::LandMask::new();

    // Track data extent for ocean shapefile filtering
    let mut min_lat_e7: i32 = i32::MAX;
    let mut max_lat_e7: i32 = i32::MIN;
    let mut min_lon_e7: i32 = i32::MAX;
    let mut max_lon_e7: i32 = i32::MIN;

    let min_z = config.min_zoom;
    let max_z = config.max_zoom;

    // Batches for parallel processing
    let mut raw_way_batch: Vec<RawWay> = Vec::with_capacity(WAY_BATCH_SIZE);
    let mut rel_batch: Vec<PreparedRelation> = Vec::with_capacity(REL_BATCH_SIZE);

    // Reusable buffer hoisted out of the PBF closure to avoid per-element
    // allocation (~200M allocs at planet scale). Cleared each iteration.
    // tags_vec cannot be hoisted: it holds &str references into PBF elements
    // that don't outlive the closure body (mutable reference invariance).
    let mut node_records: Vec<SortRecord> = Vec::new();

    // Macro to handle Node and DenseNode identically — both types expose the
    // same API (.id(), .decimicro_lat(), .decimicro_lon(), .tags()) but are
    // distinct types, so a generic function would not work without a trait.
    macro_rules! handle_node {
        ($node:expr) => {{
            node_count += 1;
            let lat_e7 = $node.decimicro_lat();
            let lon_e7 = $node.decimicro_lon();
            node_index_opt.as_mut()
                .expect("node_index consumed before all nodes processed")
                .put($node.id(), lat_e7, lon_e7);

            min_lat_e7 = min_lat_e7.min(lat_e7);
            max_lat_e7 = max_lat_e7.max(lat_e7);
            min_lon_e7 = min_lon_e7.min(lon_e7);
            max_lon_e7 = max_lon_e7.max(lon_e7);

            if $node.tags().next().is_some() {
                let tags_vec: Vec<(&str, &str)> = $node.tags().collect();
                node_records.clear();
                #[allow(clippy::cast_sign_loss)]
                let n = process_node(
                    $node.id() as u64, lat_e7, lon_e7,
                    &tags_vec, min_z, max_z, &land_mask, &mut node_records,
                );
                for r in node_records.drain(..) {
                    sort_writer.push(r).expect("sort push failed");
                }
                features_emitted += n;
            }
        }};
    }

    reader
        .for_each_pipelined(|element| match element {
            Element::Node(node) => handle_node!(node),
            Element::DenseNode(node) => handle_node!(node),
            Element::Way(way) => {
                way_count += 1;

                // Convert node index to read-only reader on first way.
                // PBF guarantees all nodes come before ways, so writes are done.
                if node_reader.is_none() {
                    let ni = node_index_opt.take()
                        .expect("node_index already consumed");
                    let reader = ni.into_reader()
                        .expect("failed to convert node index to reader");
                    reader.advise_random();
                    reader.advise_hugepage();
                    reader.advise_populate_read();
                    node_reader = Some(reader);
                    eprintln!("  Node index finalized ({node_count} nodes), processing ways...");
                }

                // Collect raw way data — cheap copies on the main thread.
                // Node coord resolution moves to rayon (the expensive part).
                let node_refs: Vec<i64> = way.refs().collect();
                if node_refs.is_empty() {
                    return;
                }

                let tags: Vec<(String, String)> = way.tags()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();

                raw_way_batch.push(RawWay { way_id: way.id(), node_refs, tags });

                if raw_way_batch.len() >= WAY_BATCH_SIZE {
                    let batch = std::mem::replace(&mut raw_way_batch, Vec::with_capacity(WAY_BATCH_SIZE));
                    features_emitted += flush_raw_way_batch(
                        batch,
                        node_reader.as_ref().expect("node reader not initialized"),
                        &mut way_index, min_z, max_z, &land_mask, &mut sort_writer,
                    );
                }
            }
            Element::Relation(rel) => {
                // Flush remaining raw way batch at the way→relation transition
                if !raw_way_batch.is_empty() {
                    let batch = std::mem::replace(&mut raw_way_batch, Vec::with_capacity(WAY_BATCH_SIZE));
                    features_emitted += flush_raw_way_batch(
                        batch,
                        node_reader.as_ref().expect("node reader not initialized"),
                        &mut way_index, min_z, max_z, &land_mask, &mut sort_writer,
                    );
                }

                if !way_index_finalized {
                    way_index.finish_writing().expect("failed to finalize way index");
                    way_index_finalized = true;
                    eprintln!("  Ways: {way_count}, Features so far: {features_emitted}");
                    eprintln!("  Way index finalized, processing relations...");
                }
                rel_count += 1;

                let tags_vec: Vec<(&str, &str)> = rel.tags().collect();
                if tags_vec.is_empty() {
                    return;
                }

                if let Some(prepared) = prepare_relation(&rel, &tags_vec, &way_index) {
                    rel_batch.push(prepared);
                    if rel_batch.len() >= REL_BATCH_SIZE {
                        let batch = std::mem::replace(&mut rel_batch, Vec::with_capacity(REL_BATCH_SIZE));
                        features_emitted += flush_rel_batch(batch, min_z, max_z, &land_mask, &mut sort_writer);
                    }
                }
            }
        })
        .map_err(|e| PipelineError(format!("PBF read failed: {e}")))?;

    // Flush any remaining batches
    if !raw_way_batch.is_empty() {
        // Handle degenerate PBF with ways but no prior conversion
        if node_reader.is_none() {
            if let Some(ni) = node_index_opt.take() {
                let reader = ni.into_reader().expect("failed to convert node index to reader");
                reader.advise_random();
                reader.advise_hugepage();
                reader.advise_populate_read();
                node_reader = Some(reader);
            }
        }
        if let Some(ref nr) = node_reader {
            features_emitted += flush_raw_way_batch(
                raw_way_batch, nr, &mut way_index, min_z, max_z, &land_mask, &mut sort_writer,
            );
        }
    }
    if !rel_batch.is_empty() {
        features_emitted += flush_rel_batch(rel_batch, min_z, max_z, &land_mask, &mut sort_writer);
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
    eprintln!("  Land mask: {}/65536 z8 cells populated", land_mask.count_set());

    Ok((sort_writer, data_bounds, land_mask))
}

// ---------------------------------------------------------------------------
// Node processing (point layers)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
fn process_node(
    osm_id: u64,
    lat_e7: i32,
    lon_e7: i32,
    tags: &[(&str, &str)],
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let tag_helper = Tags(tags);
    let matches = shortbread::match_element(&tag_helper, OsmGeomType::Node);
    if matches.is_empty() {
        return 0;
    }

    let p = geometry::project_e7(lat_e7, lon_e7);
    let pbbox = MercBbox { min_x: p.x, min_y: p.y, max_x: p.x, max_y: p.y };
    land_mask.mark_bbox(&pbbox);
    let mut count: u64 = 0;
    let mut geom_buf: Vec<u32> = Vec::new();
    let mut attrs_buf: Vec<u8> = Vec::new();

    for m in &matches {
        let z_lo = m.min_zoom.max(min_zoom);
        let z_hi = m.max_zoom.min(max_zoom);
        if z_lo > z_hi {
            continue;
        }

        for z in z_lo..=z_hi {
            encode_attrs_bytes(&mut attrs_buf, &m.attrs, z);
            geometry::for_each_tile_in_bbox(&pbbox, z, |tx, ty| {
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                let (px, py) = geometry::merc_to_tile_px(&p, tx, ty, z);
                mvt::encode_point(&mut geom_buf, px, py);
                let data = encode_feature_data_with_attrs(osm_id, GeomType::Point, &geom_buf, &attrs_buf);
                let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
                records.push(SortRecord { key, data });
                count += 1;
            });
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Way processing (line + polygon layers) — parallel batch processing
//
// Raw way data (node ref IDs + owned tags) is collected on the main thread,
// then dispatched to rayon where workers do the expensive work in parallel:
// node coord resolution (mmap reads — page faults spread across threads),
// tag matching, projection, simplification, clipping, MVT encoding.
// The serial post-rayon phase does only fast sequential I/O:
// way_index.put() + sort_writer.push().
// ---------------------------------------------------------------------------

/// Raw way data copied from PBF on the main thread. Tags are owned because
/// PBF element borrows don't survive the callback (same pattern as PreparedRelation).
struct RawWay {
    way_id: i64,
    node_refs: Vec<i64>,
    tags: Vec<(String, String)>,
}

/// Result of parallel way processing: resolved coords (needed for way_index)
/// and sort records (geometry output).
struct ProcessedWay {
    way_id: i64,
    coords_e7: Vec<(i32, i32)>,
    records: Vec<SortRecord>,
}

const WAY_BATCH_SIZE: usize = 8192;

/// Process a batch of raw ways in parallel: resolve coords, match tags, emit geometry.
/// Then serially write way_index entries and push sort records.
#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
fn flush_raw_way_batch(
    batch: Vec<RawWay>,
    node_reader: &NodeIndexReader,
    way_index: &mut WayIndex,
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
    sort_writer: &mut SortWriter,
) -> u64 {
    use rayon::prelude::*;

    // Phase 1: Parallel — resolve coords, match tags, process geometry.
    // Page faults on node_reader.get() now spread across rayon threads.
    let results: Vec<ProcessedWay> = batch
        .into_par_iter()
        .map(|raw| process_raw_way(raw, node_reader, min_zoom, max_zoom, land_mask))
        .collect();

    // Phase 2: Serial — way_index writes + sort_writer pushes (fast sequential I/O).
    let mut count: u64 = 0;
    for pw in results {
        if !pw.coords_e7.is_empty() {
            way_index.put(pw.way_id, &pw.coords_e7);
        }
        count += pw.records.len() as u64;
        for record in pw.records {
            sort_writer.push(record).expect("sort push failed");
        }
    }
    count
}

/// Process a raw way on a rayon worker thread: resolve node coordinates,
/// match tags, and run geometry processing (projection, simplification,
/// clipping, MVT encoding).
#[hotpath::measure]
fn process_raw_way(
    raw: RawWay,
    node_reader: &NodeIndexReader,
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
) -> ProcessedWay {
    // Resolve node coordinates (the expensive mmap reads — now parallel).
    // Allocates per way (~8 coords avg). Cannot hoist: ownership transfers into
    // ProcessedWay for the serial way_index.put() phase, so a reusable buffer
    // would need .to_vec()/.clone() anyway, defeating the purpose.
    let coords_e7: Vec<(i32, i32)> = raw.node_refs.iter()
        .filter_map(|&id| node_reader.get(id))
        .collect();

    if coords_e7.is_empty() || raw.tags.is_empty() {
        return ProcessedWay { way_id: raw.way_id, coords_e7, records: Vec::new() };
    }

    // Tag matching — convert owned tags to borrowed refs (same pattern as
    // process_prepared_relation, pipeline.rs PreparedRelation handling)
    let is_closed = coords_e7.len() >= 4 && coords_e7.first() == coords_e7.last();
    let geom_type = if is_closed { OsmGeomType::ClosedWay } else { OsmGeomType::OpenWay };
    let tags_ref: Vec<(&str, &str)> = raw.tags.iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let tag_helper = Tags(&tags_ref);
    let mut matches = shortbread::match_element(&tag_helper, geom_type);

    if matches.is_empty() {
        return ProcessedWay { way_id: raw.way_id, coords_e7, records: Vec::new() };
    }

    // Project to Mercator
    let merc: Vec<Point> = coords_e7.iter()
        .map(|&(lat, lon)| geometry::project_e7(lat, lon))
        .collect();

    let bbox = merc_bbox(&merc);
    land_mask.mark_bbox(&bbox);

    // Enrich polygon matches with area-dependent data (way_area, min_zoom overrides)
    if is_closed {
        let area_m2 = geometry::area_sq_meters(&merc);
        enrich_polygon_matches(&mut matches, area_m2);
    }

    #[allow(clippy::cast_sign_loss)]
    let osm_id = raw.way_id as u64;
    let mut records = Vec::new();

    for m in &matches {
        let z_lo = m.min_zoom.max(min_zoom);
        let z_hi = m.max_zoom.min(max_zoom);
        if z_lo > z_hi {
            continue;
        }

        match m.geom_expect {
            GeomExpect::Point | GeomExpect::PolygonCentroid | GeomExpect::PolygonPointOnSurface => {
                emit_point_or_centroid(osm_id, &merc, &bbox, m, z_lo, z_hi, &mut records);
            }
            GeomExpect::Line => {
                emit_line_feature(osm_id, &merc, m, z_lo, z_hi, &mut records);
            }
            GeomExpect::Polygon => {
                emit_polygon_feature(osm_id, &merc, m, z_lo, z_hi, &mut records);
            }
        }
    }

    ProcessedWay { way_id: raw.way_id, coords_e7, records }
}

// ---------------------------------------------------------------------------
// Relation processing (multipolygon + boundary lines) — parallel batch processing
// ---------------------------------------------------------------------------

/// A relation with geometry resolved from way_index, ready for parallel processing.
/// Matches are resolved eagerly in `prepare_relation` while PBF borrows are alive,
/// avoiding cloning all relation tags to owned Strings.
struct PreparedRelation {
    osm_id: u64,
    matches: SmallVec<[LayerMatch; 4]>,
    member_ways: Vec<MemberWay>,
    is_boundary: bool,
}

const REL_BATCH_SIZE: usize = 1024;

/// Resolve relation geometry from way_index (serial I/O). Returns None if
/// the relation is not a multipolygon/boundary or has no resolvable member ways.
/// Tag matching runs here while PBF borrows are alive, eliminating the need to
/// clone all relation tags to owned Strings.
#[hotpath::measure]
fn prepare_relation(
    rel: &pbfhogg::Relation<'_>,
    tags: &[(&str, &str)],
    way_index: &WayIndex,
) -> Option<PreparedRelation> {
    let tag_helper = Tags(tags);

    let rel_type = tag_helper.get("type").unwrap_or("");
    if rel_type != "multipolygon" && rel_type != "boundary" {
        return None;
    }

    // Match while PBF borrows are alive — attrs copy only the relevant tag values
    // into Cow::Owned, avoiding cloning ALL tags to String.
    let matches = shortbread::match_element(&tag_helper, OsmGeomType::MultiPolygon);
    if matches.is_empty() {
        return None;
    }

    let is_boundary = tag_helper.has_value("boundary", "administrative");

    let mut member_ways: Vec<MemberWay> = Vec::new();

    for member in rel.members() {
        let MemberId::Way(way_id) = member.id else {
            continue;
        };
        let role = WayRole::from_str(member.role().unwrap_or(""));
        if let Some(coords_e7) = way_index.get(way_id) {
            let merc: Vec<Point> = coords_e7
                .iter()
                .map(|&(lat, lon)| geometry::project_e7(lat, lon))
                .collect();

            member_ways.push(MemberWay { role, coords: merc });
        }
    }

    if member_ways.is_empty() {
        return None;
    }

    #[allow(clippy::cast_sign_loss)]
    Some(PreparedRelation {
        osm_id: rel.id() as u64,
        matches,
        member_ways,
        is_boundary,
    })
}

/// Process a batch of prepared relations in parallel and push results to sort writer.
#[hotpath::measure]
fn flush_rel_batch(
    batch: Vec<PreparedRelation>,
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
    sort_writer: &mut SortWriter,
) -> u64 {
    use rayon::prelude::*;

    let results: Vec<Vec<SortRecord>> = batch
        .into_par_iter()
        .map(|rel| process_prepared_relation(rel, min_zoom, max_zoom, land_mask))
        .collect();

    let mut count: u64 = 0;
    for rel_records in results {
        count += rel_records.len() as u64;
        for record in rel_records {
            sort_writer.push(record).expect("sort push failed");
        }
    }
    count
}

/// Process a prepared relation's geometry (CPU-bound). Called from rayon worker threads.
#[hotpath::measure]
fn process_prepared_relation(
    rel: PreparedRelation,
    min_zoom: u8,
    max_zoom: u8,
    land_mask: &geometry::LandMask,
) -> Vec<SortRecord> {
    let multi = multipolygon::assemble(&rel.member_ways);

    if multi.polygons.is_empty() {
        return Vec::new();
    }

    let mut matches = rel.matches;

    let total_area_m2: f64 = multi.polygons.iter()
        .map(|(outer, _)| geometry::area_sq_meters(outer))
        .sum();
    enrich_polygon_matches(&mut matches, total_area_m2);

    let mut records = Vec::new();

    for m in &matches {
        let z_lo = m.min_zoom.max(min_zoom);
        let z_hi = m.max_zoom.min(max_zoom);
        if z_lo > z_hi {
            continue;
        }

        match m.geom_expect {
            GeomExpect::Polygon => {
                for (outer, inners) in &multi.polygons {
                    if outer.len() < 4 {
                        continue;
                    }
                    let bbox = merc_bbox(outer);
                    land_mask.mark_bbox(&bbox);
                    emit_multipolygon_feature(
                        rel.osm_id, outer, inners, m,
                        z_lo, z_hi, &mut records,
                    );
                }
            }
            GeomExpect::PolygonCentroid | GeomExpect::PolygonPointOnSurface => {
                for (outer, _inners) in &multi.polygons {
                    if outer.len() < 4 {
                        continue;
                    }
                    let bbox = merc_bbox(outer);
                    land_mask.mark_bbox(&bbox);
                    emit_point_or_centroid(
                        rel.osm_id, outer, &bbox, m, z_lo, z_hi, &mut records,
                    );
                }
            }
            GeomExpect::Line => {
                // Boundary line emission: iterate member_ways directly instead of
                // a separate cloned Vec. multipolygon::assemble() only borrows
                // &[MemberWay], so coords are still available here. GeomExpect::Line
                // is only produced by match_boundaries_line which requires
                // boundary=administrative — same predicate as is_boundary.
                if !rel.is_boundary {
                    continue;
                }
                for mw in &rel.member_ways {
                    if mw.coords.len() < 2 {
                        continue;
                    }
                    let bbox = merc_bbox(&mw.coords);
                    land_mask.mark_bbox(&bbox);
                    emit_line_feature(
                        rel.osm_id, &mw.coords, m, z_lo, z_hi, &mut records,
                    );
                }
            }
            _ => {}
        }
    }
    records
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

#[hotpath::measure]
fn emit_point_or_centroid(
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
    let pt = if m.geom_expect == GeomExpect::PolygonPointOnSurface {
        geometry::point_on_surface(coords)
    } else {
        Some(centroid_of(coords))
    };
    let Some(p) = pt else { return 0 };

    let cbbox = MercBbox { min_x: p.x, min_y: p.y, max_x: p.x, max_y: p.y };
    let mut count: u64 = 0;
    let mut geom_buf: Vec<u32> = Vec::new();
    let mut attrs_buf: Vec<u8> = Vec::new();
    for z in z_lo..=z_hi {
        encode_attrs_bytes(&mut attrs_buf, &m.attrs, z);
        geometry::for_each_tile_in_bbox(&cbbox, z, |tx, ty| {
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let (px, py) = geometry::merc_to_tile_px(&p, tx, ty, z);
            mvt::encode_point(&mut geom_buf, px, py);
            let data = encode_feature_data_with_attrs(osm_id, GeomType::Point, &geom_buf, &attrs_buf);
            let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
            records.push(SortRecord { key, data });
            count += 1;
        });
    }
    count
}

#[hotpath::measure]
fn emit_line_feature(
    osm_id: u64,
    merc: &[Point],
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let mut count: u64 = 0;
    // Reusable buffers: hoisted outside the zoom×tile loops to avoid per-tile allocation.
    let mut geom_buf: Vec<u32> = Vec::new();
    let mut attrs_buf: Vec<u8> = Vec::new();
    let mut tc_buf: Vec<(i32, i32)> = Vec::new();

    geometry::for_each_zoom_simplified(merc, z_lo, z_hi, 2, |z, simplified| {
        encode_attrs_bytes(&mut attrs_buf, &m.attrs, z);

        // Recompute bbox from simplified coords — at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simplified);

        // Skip min-size filtering at max zoom and for boundaries/streets
        let skip_size_filter = z >= 14
            || m.layer == Layer::Boundaries
            || m.layer == Layer::Streets;
        geometry::for_each_tile_in_bbox(&simp_bbox, z, |tx, ty| {
            let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
            let clipped = geometry::clip_linestring(simplified, &clip);
            for segment in &clipped {
                if segment.len() < 2 {
                    continue;
                }
                geometry::to_tile_coords_into(&mut tc_buf, segment, tx, ty, z);
                if !skip_size_filter && geometry::line_is_subpixel(&tc_buf) {
                    continue;
                }
                mvt::encode_linestring(&mut geom_buf, &tc_buf);
                if geom_buf.is_empty() {
                    continue;
                }
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                let data = encode_feature_data_with_attrs(osm_id, GeomType::LineString, &geom_buf, &attrs_buf);
                let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
                records.push(SortRecord { key, data });
                count += 1;
            }
        });
    });
    count
}

#[hotpath::measure]
fn emit_polygon_feature(
    osm_id: u64,
    merc: &[Point],
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    // Single-ring polygon (no holes)
    let mut count: u64 = 0;
    // Reusable buffers: hoisted outside the zoom×tile loops to avoid per-tile allocation.
    let mut geom_buf: Vec<u32> = Vec::new();
    let mut attrs_buf: Vec<u8> = Vec::new();
    let mut tc_buf: Vec<(i32, i32)> = Vec::new();
    let mut clip_a: Vec<Point> = Vec::new();
    let mut clip_b: Vec<Point> = Vec::new();

    geometry::for_each_zoom_simplified(merc, z_lo, z_hi, 4, |z, simplified| {
        encode_attrs_bytes(&mut attrs_buf, &m.attrs, z);

        // Recompute bbox from simplified coords — at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simplified);
        let skip_size_filter = z >= 14;
        geometry::for_each_tile_in_bbox(&simp_bbox, z, |tx, ty| {
            let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
            geometry::clip_polygon_into(simplified, &clip, &mut clip_a, &mut clip_b);
            if clip_a.len() < 3 {
                return;
            }
            geometry::to_tile_coords_into(&mut tc_buf, &clip_a, tx, ty, z);
            if !skip_size_filter && geometry::ring_is_subpixel(&tc_buf) {
                return;
            }
            close_and_orient_cw(&mut tc_buf);

            mvt::encode_polygon(&mut geom_buf, &[&tc_buf]);
            if geom_buf.is_empty() {
                return;
            }
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let data = encode_feature_data_with_attrs(osm_id, GeomType::Polygon, &geom_buf, &attrs_buf);
            let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
            records.push(SortRecord { key, data });
            count += 1;
        });
    });
    count
}

#[hotpath::measure]
fn emit_multipolygon_feature(
    osm_id: u64,
    outer: &[Point],
    inners: &[Vec<Point>],
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let mut count: u64 = 0;
    let mut geom_buf: Vec<u32> = Vec::new();
    let mut attrs_buf: Vec<u8> = Vec::new();
    let mut clip_a: Vec<Point> = Vec::new();
    let mut clip_b: Vec<Point> = Vec::new();

    geometry::for_each_zoom_simplified_multi(outer, inners, z_lo, z_hi, |z, simp_outer, simp_inners| {
        encode_attrs_bytes(&mut attrs_buf, &m.attrs, z);

        // Recompute bbox from simplified coords — at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simp_outer);
        let skip_size_filter = z >= 14;
        geometry::for_each_tile_in_bbox(&simp_bbox, z, |tx, ty| {
            let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);

            geometry::clip_polygon_into(simp_outer, &clip, &mut clip_a, &mut clip_b);
            if clip_a.len() < 3 {
                return;
            }
            let mut outer_tc = geometry::to_tile_coords(&clip_a, tx, ty, z);
            if !skip_size_filter && geometry::ring_is_subpixel(&outer_tc) {
                return;
            }
            close_and_orient_cw(&mut outer_tc);

            let mut all_rings: Vec<Vec<(i32, i32)>> = vec![outer_tc];
            for inner in simp_inners {
                geometry::clip_polygon_into(inner, &clip, &mut clip_a, &mut clip_b);
                if clip_a.len() < 3 {
                    continue;
                }
                let mut inner_tc = geometry::to_tile_coords(&clip_a, tx, ty, z);
                // Also drop sub-pixel inner rings (holes)
                if !skip_size_filter && geometry::ring_is_subpixel(&inner_tc) {
                    continue;
                }
                close_and_orient_ccw(&mut inner_tc);
                all_rings.push(inner_tc);
            }

            let ring_refs: Vec<&[(i32, i32)]> = all_rings.iter().map(Vec::as_slice).collect();
            mvt::encode_polygon(&mut geom_buf, &ring_refs);
            if geom_buf.is_empty() {
                return;
            }
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let data = encode_feature_data_with_attrs(osm_id, GeomType::Polygon, &geom_buf, &attrs_buf);
            let key = sort::make_sort_key(tile_id, m.layer as u8, 0);
            records.push(SortRecord { key, data });
            count += 1;
        });
    });
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

/// An encoded + gzip-compressed tile ready for writing to PMTiles.
struct EncodedTile {
    tile_id: u64,
    compressed: Vec<u8>,
}

#[allow(clippy::too_many_lines)]
#[hotpath::measure]
fn phase_assemble(sort_reader: &mut sort::SortReader, config: &TilegenConfig) -> Result<(u64, u64, u64), PipelineError> {
    use std::sync::mpsc::sync_channel;

    let pmtiles_config = PmtilesConfig {
        min_zoom: config.min_zoom,
        max_zoom: config.max_zoom,
        bounds: (-180.0, -85.05, 180.0, 85.05),
        center: (0.0, 0.0, 2),
    };
    let pmtiles = if config.in_memory {
        PmtilesWriter::new(pmtiles_config)
    } else {
        PmtilesWriter::new_streaming(pmtiles_config, &config.tmp_dir)?
    };

    const BATCH_SIZE: usize = 4096;

    // Double-buffer pipeline: reader → encoder (main/rayon) → writer.
    // sync_channel(1) allows one batch ahead, overlapping read/write I/O
    // with CPU-bound rayon encoding.
    // Error cascade: reader error → drops read_tx → encoder loop ends →
    // drops encode_tx → writer loop ends → scope joins → error propagated.
    let (read_tx, read_rx) = sync_channel::<Vec<PendingTile>>(1);
    let (encode_tx, encode_rx) = sync_channel::<Vec<EncodedTile>>(1);

    let scope_result: Result<_, PipelineError> = std::thread::scope(|s| {
        // --- Reader thread: k-way merge → PendingTile batches ---
        let reader = s.spawn(move || -> Result<u64, PipelineError> {
            let mut features_read: u64 = 0;
            let mut batch: Vec<PendingTile> = Vec::with_capacity(BATCH_SIZE);
            let mut current = PendingTile { tile_id: u64::MAX, features: Vec::new() };

            loop {
                let record = sort_reader.next()?;
                let Some(r) = record else {
                    if current.tile_id != u64::MAX {
                        batch.push(current);
                    }
                    if !batch.is_empty() {
                        drop(read_tx.send(batch)); // ignore: encoder may have exited
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
                            if read_tx.send(batch).is_err() { break; }
                            batch = Vec::with_capacity(BATCH_SIZE);
                        }
                    }
                    current = PendingTile { tile_id, features: Vec::new() };
                }
                current.features.push((layer_idx, r.data));
            }
            Ok(features_read)
        });

        // --- Writer thread: encoded tiles → PMTiles ---
        // move takes ownership of pmtiles; returned via join handle for write_to().
        let writer = s.spawn(move || -> (u64, PmtilesWriter, [u64; 15], [u64; 15], [u64; 15]) {
            let mut pmtiles = pmtiles;
            let mut tiles_written: u64 = 0;
            let mut tiles_per_zoom = [0u64; 15];
            let mut unique_per_zoom = [0u64; 15];
            let mut bytes_per_zoom = [0u64; 15];
            while let Ok(batch) = encode_rx.recv() {
                for tile in batch {
                    let (z, x, y) = pmtiles_writer::tile_id_to_zxy(tile.tile_id);
                    let tile_bytes = tile.compressed.len() as u64;
                    let is_unique = pmtiles.add_tile(z, x, y, &tile.compressed)
                        .expect("failed to write tile");
                    tiles_written += 1;
                    if (z as usize) < 15 {
                        tiles_per_zoom[z as usize] += 1;
                        if is_unique {
                            unique_per_zoom[z as usize] += 1;
                            bytes_per_zoom[z as usize] += tile_bytes;
                        }
                    }
                }
            }
            (tiles_written, pmtiles, tiles_per_zoom, unique_per_zoom, bytes_per_zoom)
        });

        // --- Main thread: receive batches, encode with rayon, forward to writer ---
        for batch in read_rx {
            let encoded = encode_tile_batch(&batch);
            if encode_tx.send(encoded).is_err() { break; }
        }
        drop(encode_tx);

        let features_read = reader.join().expect("reader panicked")?;
        let (tiles_written, pmtiles, tiles_per_zoom, unique_per_zoom, bytes_per_zoom) = writer.join().expect("writer panicked");
        Ok((features_read, tiles_written, pmtiles, tiles_per_zoom, unique_per_zoom, bytes_per_zoom))
    });

    let (features_read, tiles_written, mut pmtiles, tiles_per_zoom, unique_per_zoom, bytes_per_zoom) = scope_result?;
    let unique_tiles = pmtiles.unique_tile_count();
    pmtiles.write_to(&config.output_path)?;

    // Per-zoom tile breakdown
    eprintln!("  Per-zoom tiles (total / unique / unique MB):");
    for z in config.min_zoom..=config.max_zoom {
        let total = tiles_per_zoom[z as usize];
        let unique = unique_per_zoom[z as usize];
        let mb = bytes_per_zoom[z as usize] as f64 / (1024.0 * 1024.0);
        if total > 0 {
            eprintln!("    z{z:2}: {total:>8} / {unique:>8} / {mb:>7.1} MB");
        }
    }

    Ok((features_read, tiles_written, unique_tiles))
}

/// Encode + gzip a batch of tiles in parallel using rayon.
#[hotpath::measure]
fn encode_tile_batch(batch: &[PendingTile]) -> Vec<EncodedTile> {
    use rayon::prelude::*;

    batch
        .par_iter()
        .map_init(
            || (mvt::EncodeScratch::new(), mvt::MergeScratch::new(),
                Vec::<Vec<u32>>::new(), Vec::<Vec<(u16, u16)>>::new()),
            |(encode_scratch, merge_scratch, geom_pool, tags_pool), tile| {
            let mut layers = new_layer_slots();
            for &(layer_idx, ref data) in &tile.features {
                if (layer_idx as usize) < layers.len() {
                    add_feature_to_layer(
                        get_or_create_layer(&mut layers, layer_idx as usize),
                        data,
                        geom_pool,
                        tags_pool,
                    );
                }
            }

            // Merge same-attribute geometries to reduce feature count
            for layer in &mut layers {
                if let Some(lb) = layer.as_mut() {
                    lb.merge_same_attr_geometries(merge_scratch, geom_pool, tags_pool);
                }
            }

            let non_empty: Vec<&LayerBuilder> = layers.iter()
                .filter_map(|l| l.as_ref())
                .filter(|l| !l.is_empty())
                .collect();
            if non_empty.is_empty() {
                // Reclaim feature Vecs before returning
                for layer in &mut layers {
                    if let Some(lb) = layer.as_mut() {
                        lb.reclaim_features(geom_pool, tags_pool);
                    }
                }
                return None;
            }

            let mvt_data = mvt::encode_tile_with(&non_empty, encode_scratch);

            // Reclaim feature Vecs into pools for reuse on next tile
            for layer in &mut layers {
                if let Some(lb) = layer.as_mut() {
                    lb.reclaim_features(geom_pool, tags_pool);
                }
            }

            if mvt_data.is_empty() {
                return None;
            }

            // Gzip level 6: good compression/speed tradeoff for MVT tiles.
            let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
            encoder.write_all(&mvt_data).expect("gzip write failed");
            let compressed = encoder.finish().expect("gzip finish failed");

            Some(EncodedTile { tile_id: tile.tile_id, compressed })
        })
        .flatten()
        .collect()
}

const LAYER_COUNT: usize = Layer::count();

/// Create an empty slot array for lazy layer builder initialization.
fn new_layer_slots() -> [Option<LayerBuilder>; LAYER_COUNT] {
    [const { None }; LAYER_COUNT]
}

/// Get or create a LayerBuilder at the given index.
fn get_or_create_layer(layers: &mut [Option<LayerBuilder>], idx: usize) -> &mut LayerBuilder {
    if layers[idx].is_none() {
        layers[idx] = Some(LayerBuilder::new(Layer::ALL[idx].name()));
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
    use smallvec::smallvec;
    use std::borrow::Cow;

    /// Helper: build a BoundaryLabels match with the given admin_level and default min_zoom=5.
    fn boundary_labels_match(admin_level: i64) -> LayerMatch {
        LayerMatch {
            layer: Layer::BoundaryLabels,
            min_zoom: 5,
            max_zoom: 14,
            geom_expect: GeomExpect::PolygonPointOnSurface,
            attrs: smallvec![
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
            attrs: smallvec![],
        }];
        let original_min_zoom = matches[0].min_zoom;
        let original_attr_count = matches[0].attrs.len();

        enrich_polygon_matches(&mut matches, 9_999_999_999.0);

        assert_eq!(matches[0].min_zoom, original_min_zoom);
        assert_eq!(matches[0].attrs.len(), original_attr_count, "attrs should not be modified");
    }

    // -----------------------------------------------------------------------
    // Helpers for emit tests — decode SortRecord payloads
    // -----------------------------------------------------------------------

    use crate::sort;
    use crate::mvt;

    fn test_layer_match(layer: Layer, geom_expect: GeomExpect) -> LayerMatch {
        LayerMatch {
            layer,
            min_zoom: 0,
            max_zoom: 14,
            geom_expect,
            attrs: smallvec![("kind", AttrValue::Str(Cow::Borrowed("test")), 0)],
        }
    }

    /// Decode the sort key fields from a SortRecord.
    fn decode_key(rec: &SortRecord) -> (u64, u8) {
        let tile_id = sort::tile_id_from_key(rec.key);
        let layer_idx = sort::layer_from_key(rec.key);
        (tile_id, layer_idx)
    }

    /// Decode the wire format header from a SortRecord's data payload.
    /// Returns (osm_id, geom_type_byte, geom_cmd_count).
    fn decode_data_header(data: &[u8]) -> (u64, u8, u32) {
        let osm_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let gt = data[8];
        let cmd_count = u32::from_le_bytes(data[9..13].try_into().unwrap());
        (osm_id, gt, cmd_count)
    }

    /// Decode the attribute count from a SortRecord's data payload.
    fn decode_attr_count(data: &[u8]) -> u8 {
        let cmd_count = u32::from_le_bytes(data[9..13].try_into().unwrap()) as usize;
        let attr_start = 13 + cmd_count * 4;
        data[attr_start]
    }

    /// Decode the full record via add_feature_to_layer and return the layer builder.
    fn decode_to_layer(data: &[u8]) -> mvt::LayerBuilder {
        let mut lb = mvt::LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        crate::wire_format::add_feature_to_layer(&mut lb, data, &mut gp, &mut tp);
        lb
    }

    // -----------------------------------------------------------------------
    // emit_point_or_centroid tests (formerly emit_point_feature)
    // -----------------------------------------------------------------------

    #[test]
    fn emit_point_empty_coords() {
        let m = test_layer_match(Layer::Pois, GeomExpect::Point);
        let bbox = MercBbox { min_x: 0.0, min_y: 0.0, max_x: 1.0, max_y: 1.0 };
        let mut records = Vec::new();
        let count = emit_point_or_centroid(1, &[], &bbox, &m, 0, 0, &mut records);
        assert_eq!(count, 0);
        assert!(records.is_empty());
    }

    #[test]
    fn emit_point_decodes_correctly() {
        let m = test_layer_match(Layer::Pois, GeomExpect::Point);
        let coords = [Point { x: 0.5, y: 0.5 }];
        let bbox = MercBbox { min_x: 0.0, min_y: 0.0, max_x: 1.0, max_y: 1.0 };
        let mut records = Vec::new();
        emit_point_or_centroid(42, &coords, &bbox, &m, 0, 0, &mut records);
        assert_eq!(records.len(), 1);

        let rec = &records[0];

        // Sort key: tile_id for z=0/x=0/y=0, layer = Pois
        let (tile_id, layer_idx) = decode_key(rec);
        assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
        assert_eq!(layer_idx, Layer::Pois as u8);

        // Wire format: osm_id=42, geom_type=Point(1), has geometry commands
        let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
        assert_eq!(osm_id, 42);
        assert_eq!(gt, 1); // Point
        assert!(cmd_count > 0, "point should have geometry commands");

        // Attributes: 1 attr ("kind" = "test")
        assert_eq!(decode_attr_count(&rec.data), 1);

        // Full decode roundtrip
        let lb = decode_to_layer(&rec.data);
        assert_eq!(lb.test_feature_count(), 1);
        let f = lb.test_feature(0);
        assert_eq!(f.id, Some(42));
        assert_eq!(f.geom_type, mvt::GeomType::Point);
        let (k0, v0) = f.tags[0];
        assert_eq!(lb.test_key(k0), "kind");
        assert_eq!(*lb.test_value(v0), mvt::Value::String("test".to_string()));
    }

    #[test]
    fn emit_point_multi_zoom_tile_ids_differ() {
        let m = test_layer_match(Layer::Pois, GeomExpect::Point);
        // Use a point clearly inside one z1 tile (not on a boundary)
        let coords = [Point { x: 0.25, y: 0.25 }];
        let bbox = MercBbox { min_x: 0.25, min_y: 0.25, max_x: 0.25, max_y: 0.25 };
        let mut records = Vec::new();
        emit_point_or_centroid(7, &coords, &bbox, &m, 0, 1, &mut records);

        // Should get 1 record at z=0 and 1 record at z=1 = 2 total
        assert_eq!(records.len(), 2);

        // The tile IDs should differ (z0 vs z1 are different Hilbert IDs)
        let (tid0, _) = decode_key(&records[0]);
        let (tid1, _) = decode_key(&records[1]);
        assert_ne!(tid0, tid1, "z0 and z1 tile IDs should differ");

        // Both should decode to the same osm_id
        let (id0, _, _) = decode_data_header(&records[0].data);
        let (id1, _, _) = decode_data_header(&records[1].data);
        assert_eq!(id0, 7);
        assert_eq!(id1, 7);
    }

    // -----------------------------------------------------------------------
    // emit_line_feature tests
    // -----------------------------------------------------------------------

    #[test]
    fn emit_line_too_few_points() {
        let m = test_layer_match(Layer::Streets, GeomExpect::Line);
        let coords = [Point { x: 0.5, y: 0.5 }];
        let mut records = Vec::new();
        let count = emit_line_feature(101, &coords, &m, 0, 0, &mut records);
        assert_eq!(count, 0);
        assert!(records.is_empty());
    }

    #[test]
    fn emit_line_decodes_correctly() {
        let m = test_layer_match(Layer::Streets, GeomExpect::Line);
        let coords = [
            Point { x: 0.3, y: 0.3 },
            Point { x: 0.7, y: 0.7 },
        ];
        let mut records = Vec::new();
        emit_line_feature(100, &coords, &m, 0, 0, &mut records);
        assert_eq!(records.len(), 1);

        let rec = &records[0];

        // Sort key
        let (tile_id, layer_idx) = decode_key(rec);
        assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
        assert_eq!(layer_idx, Layer::Streets as u8);

        // Wire format
        let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
        assert_eq!(osm_id, 100);
        assert_eq!(gt, 2); // LineString
        assert!(cmd_count >= 2, "linestring needs MoveTo + LineTo commands");

        // Full decode roundtrip
        let lb = decode_to_layer(&rec.data);
        let f = lb.test_feature(0);
        assert_eq!(f.id, Some(100));
        assert_eq!(f.geom_type, mvt::GeomType::LineString);
        assert_eq!(f.tags.len(), 1);
    }

    #[test]
    fn emit_line_cascading_simplification() {
        // A line that should survive at z=14 but may get simplified away at low zoom.
        // At z=0, simplification tolerance is very large, so a short line may vanish.
        let m = test_layer_match(Layer::Streets, GeomExpect::Line);
        let coords = [
            Point { x: 0.500_000, y: 0.500_000 },
            Point { x: 0.500_001, y: 0.500_001 },
        ];
        let mut records = Vec::new();
        emit_line_feature(99, &coords, &m, 0, 14, &mut records);

        // At z=14 this line is ~0.4 pixel which is sub-pixel, but Streets skips
        // the size filter, so it should still produce a record at z=14.
        // At lower zooms, simplification may collapse it.
        let z14_records: Vec<_> = records.iter().filter(|r| {
            let tid = sort::tile_id_from_key(r.key);
            let (z, _, _) = pmtiles_writer::tile_id_to_zxy(tid);
            z == 14
        }).collect();
        assert!(!z14_records.is_empty(), "line should survive at z=14 for Streets layer");
    }

    // -----------------------------------------------------------------------
    // emit_polygon_feature tests
    // -----------------------------------------------------------------------

    #[test]
    fn emit_polygon_too_few_points() {
        let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
        let coords = [
            Point { x: 0.3, y: 0.3 },
            Point { x: 0.7, y: 0.3 },
            Point { x: 0.5, y: 0.7 },
        ];
        let mut records = Vec::new();
        let count = emit_polygon_feature(201, &coords, &m, 0, 0, &mut records);
        assert_eq!(count, 0);
        assert!(records.is_empty());
    }

    #[test]
    fn emit_polygon_decodes_correctly() {
        let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
        let coords = [
            Point { x: 0.3, y: 0.3 },
            Point { x: 0.7, y: 0.3 },
            Point { x: 0.7, y: 0.7 },
            Point { x: 0.3, y: 0.7 },
            Point { x: 0.3, y: 0.3 },
        ];
        let mut records = Vec::new();
        emit_polygon_feature(200, &coords, &m, 0, 0, &mut records);
        assert_eq!(records.len(), 1);

        let rec = &records[0];

        // Sort key
        let (tile_id, layer_idx) = decode_key(rec);
        assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
        assert_eq!(layer_idx, Layer::Buildings as u8);

        // Wire format
        let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
        assert_eq!(osm_id, 200);
        assert_eq!(gt, 3); // Polygon
        // A polygon ring needs MoveTo + LineTo(n-1) + ClosePath = at least 3 commands
        assert!(cmd_count >= 3, "polygon should have MoveTo + LineTo + ClosePath");

        // Attributes
        assert_eq!(decode_attr_count(&rec.data), 1);

        // Full decode roundtrip
        let lb = decode_to_layer(&rec.data);
        let f = lb.test_feature(0);
        assert_eq!(f.id, Some(200));
        assert_eq!(f.geom_type, mvt::GeomType::Polygon);
        let (k0, v0) = f.tags[0];
        assert_eq!(lb.test_key(k0), "kind");
        assert_eq!(*lb.test_value(v0), mvt::Value::String("test".to_string()));
    }

    #[test]
    fn emit_polygon_zoom_dependent_attrs() {
        // Attribute with min_zoom=10 should only appear at z>=10.
        // Use a tiny polygon that fits in a single tile at each test zoom.
        let m_z0 = LayerMatch {
            layer: Layer::Buildings,
            min_zoom: 0,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: smallvec![
                ("kind", AttrValue::Str(Cow::Borrowed("building")), 0),
                ("height", AttrValue::Float(15.0), 10),
            ],
        };
        let coords_z0 = [
            Point { x: 0.3, y: 0.3 },
            Point { x: 0.7, y: 0.3 },
            Point { x: 0.7, y: 0.7 },
            Point { x: 0.3, y: 0.7 },
            Point { x: 0.3, y: 0.3 },
        ];
        // At z=0: only 1 attr ("kind", min_zoom=0)
        let mut records = Vec::new();
        emit_polygon_feature(300, &coords_z0, &m_z0, 0, 0, &mut records);
        assert_eq!(records.len(), 1);
        let lb = decode_to_layer(&records[0].data);
        let f = lb.test_feature(0);
        assert_eq!(f.tags.len(), 1, "at z=0 only the always-on attr should be present");

        // At z=14: both attrs. Use a tiny polygon inside one z=14 tile.
        let m_z14 = LayerMatch {
            layer: Layer::Buildings,
            min_zoom: 14,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: smallvec![
                ("kind", AttrValue::Str(Cow::Borrowed("building")), 0),
                ("height", AttrValue::Float(15.0), 10),
            ],
        };
        let coords_z14 = [
            Point { x: 0.500_00, y: 0.500_00 },
            Point { x: 0.500_05, y: 0.500_00 },
            Point { x: 0.500_05, y: 0.500_05 },
            Point { x: 0.500_00, y: 0.500_05 },
            Point { x: 0.500_00, y: 0.500_00 },
        ];
        records.clear();
        emit_polygon_feature(300, &coords_z14, &m_z14, 14, 14, &mut records);
        assert_eq!(records.len(), 1);
        let lb = decode_to_layer(&records[0].data);
        let f = lb.test_feature(0);
        assert_eq!(f.tags.len(), 2, "at z=14 both attrs should be present");
        let (k1, v1) = f.tags[1];
        assert_eq!(lb.test_key(k1), "height");
        assert_eq!(*lb.test_value(v1), mvt::Value::Double(15.0));
    }
}

