#![cfg(feature = "mlt")]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use elivagar::pmtiles_reader::{PmtilesReader, TileEntry};
use elivagar::tile_detail::{CanonRingRole, DetailAttr, DetailFeature, DetailTile, Strictness};
use elivagar::tiling::TileId;
use elivagar::{TileCompression, TilePayloadFormat, TilegenConfig};
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::writer::{Compression, PbfWriter};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SemanticTile(Vec<SemanticLayer>);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SemanticLayer {
    name: String,
    extent: u32,
    features: Vec<SemanticFeature>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SemanticFeature {
    id: Option<u64>,
    attrs: Vec<(String, DetailAttr)>,
    geometry: SemanticGeometry,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SemanticGeometry {
    Points(Vec<(i32, i32)>),
    Lines(Vec<Vec<(i32, i32)>>),
    Polygons(Vec<SemanticPolygon>),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SemanticPolygon(Vec<SemanticRing>);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SemanticRing {
    role: CanonRingRole,
    points: Vec<(i32, i32)>,
}

#[test]
fn mvt_and_mlt_encode_the_same_tile_semantics() {
    let project_target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    std::fs::create_dir_all(&project_target).expect("create project target directory");
    let dir = tempfile::Builder::new()
        .prefix("mvt_mlt_semantics_")
        .tempdir_in(project_target)
        .expect("create project-local temporary directory");
    let pbf_path = dir.path().join("synthetic.osm.pbf");
    write_synthetic_pbf(&pbf_path);

    let mvt_path = run_tilegen(&pbf_path, dir.path(), "mvt", TilePayloadFormat::Mvt);
    let mlt_path = run_tilegen(&pbf_path, dir.path(), "mlt", TilePayloadFormat::Mlt);

    let (mvt_tile_id, mvt_payload) = read_single_tile(&mvt_path);
    let (mlt_tile_id, mlt_payload) = read_single_tile(&mlt_path);
    assert_eq!(
        mvt_tile_id, mlt_tile_id,
        "formats must address the same tile"
    );
    assert_eq!(TileId::new(mvt_tile_id).unwrap().zoom(), 14);

    let mvt_semantics = decode_mvt_semantics(&mvt_payload);
    let mlt_semantics = decode_mvt_semantics(&decode_mlt_as_mvt(&mlt_payload));

    assert_eq!(mvt_semantics, mlt_semantics);
    assert!(mvt_semantics.0.iter().any(|layer| layer.name == "pois"));
    assert!(mvt_semantics.0.iter().any(|layer| layer.name == "streets"));
    assert!(
        mvt_semantics
            .0
            .iter()
            .flat_map(|layer| &layer.features)
            .any(|feature| !feature.attrs.is_empty())
    );
    assert!(layer_has_geometry(
        &mvt_semantics,
        "pois",
        |geometry| matches!(
            geometry,
            SemanticGeometry::Points(points) if !points.is_empty()
        )
    ));
    assert!(layer_has_geometry(
        &mvt_semantics,
        "streets",
        |geometry| matches!(
            geometry,
            SemanticGeometry::Lines(paths) if !paths.is_empty()
        )
    ));
    assert!(layer_has_geometry(
        &mvt_semantics,
        "land",
        |geometry| matches!(
            geometry,
            SemanticGeometry::Polygons(polygons) if !polygons.is_empty()
        )
    ));
}

fn write_synthetic_pbf(path: &Path) {
    let file = std::fs::File::create(path).expect("create synthetic PBF");
    let mut writer = PbfWriter::new(file, Compression::default());
    let header = HeaderBuilder::new()
        .bbox(12.569, 55.681, 12.573, 55.684)
        .sorted()
        .build()
        .expect("build PBF header");
    writer.write_header(&header).expect("write PBF header");

    let mut nodes = BlockBuilder::new();
    nodes.add_node(
        1,
        556_820_000,
        125_700_000,
        [("amenity", "cafe"), ("name", "Synthetic Cafe")],
        None,
    );
    nodes.add_node(2, 556_821_000, 125_710_000, [], None);
    nodes.add_node(3, 556_828_000, 125_720_000, [], None);
    nodes.add_node(4, 556_821_000, 125_705_000, [], None);
    nodes.add_node(5, 556_821_000, 125_708_000, [], None);
    nodes.add_node(6, 556_824_000, 125_708_000, [], None);
    nodes.add_node(7, 556_824_000, 125_705_000, [], None);
    let node_bytes = nodes
        .take()
        .expect("serialize PBF node block")
        .expect("synthetic PBF node block is not empty");
    writer
        .write_primitive_block(node_bytes)
        .expect("write PBF node block");

    let mut ways = BlockBuilder::new();
    ways.add_way(
        10,
        [
            ("highway", "residential"),
            ("name", "Synthetic Road"),
            ("oneway", "yes"),
        ],
        &[2, 3],
        None,
    );
    ways.add_way(
        11,
        [("leisure", "park"), ("name", "Synthetic Park")],
        &[4, 5, 6, 7, 4],
        None,
    );
    let way_bytes = ways
        .take()
        .expect("serialize PBF way block")
        .expect("synthetic PBF way block is not empty");
    writer
        .write_primitive_block(way_bytes)
        .expect("write PBF way block");
    writer.flush().expect("finish synthetic PBF");
}

fn run_tilegen(
    pbf_path: &Path,
    dir: &Path,
    suffix: &str,
    tile_format: TilePayloadFormat,
) -> PathBuf {
    let output_path = dir.join(format!("{suffix}.pmtiles"));
    let config = TilegenConfig {
        pbf_path: pbf_path.to_path_buf(),
        output_path: output_path.clone(),
        tmp_dir: dir.join(format!("{suffix}_tmp")),
        min_zoom: 14,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        ocean_artifact_key: None,
        ocean_only_metadata: false,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        assemble_batch_budget: 0,
        dedup_cap: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: elivagar::sort::ChunkCompression::None,
        fanout_caps: [0; elivagar::shortbread::Layer::count()],
        polygon_simplify_factor: 1.0,
    };
    elivagar::run(&config).expect("run synthetic tile generation");
    output_path
}

fn read_single_tile(path: &Path) -> (u64, Vec<u8>) {
    let mut reader = PmtilesReader::open(path).expect("open output archive");
    let runs = reader.read_all_runs().expect("read archive directory");
    assert_eq!(runs.len(), 1, "fixture geometry must fit in one tile");
    let run = runs[0];
    assert_eq!(run.run_length, 1, "fixture must address exactly one tile");
    let entry = TileEntry {
        tile_id: run.tile_id,
        offset: run.offset,
        length: run.length,
    };
    let payload = reader.read_tile(&entry).expect("read tile payload");
    (run.tile_id, payload)
}

fn decode_mlt_as_mvt(payload: &[u8]) -> Vec<u8> {
    let mut parser = mlt_core::Parser::default();
    let layers = parser.parse_layers(payload).expect("parse MLT tile");
    let mut decoder = mlt_core::Decoder::default();
    let decoded_layers: Vec<_> = layers
        .into_iter()
        .map(|layer| {
            layer
                .into_tile(&mut decoder)
                .expect("decode MLT layer")
                .expect("MLT layer tag is supported")
        })
        .collect();
    mlt_core::mvt::tile_layers_to_mvt(decoded_layers).expect("convert decoded MLT layers to MVT")
}

fn decode_mvt_semantics(payload: &[u8]) -> SemanticTile {
    let detail = elivagar::tile_detail::decode_detail_tile(payload, Strictness::Strict)
        .expect("decode MVT tile");
    semantic_tile(detail)
}

fn semantic_tile(tile: DetailTile) -> SemanticTile {
    let mut layers: Vec<_> = tile
        .layers
        .into_iter()
        .map(|layer| {
            let mut features: Vec<_> = layer.features.into_iter().map(semantic_feature).collect();
            features.sort_unstable();
            SemanticLayer {
                name: layer.name.to_string(),
                extent: layer.extent,
                features,
            }
        })
        .collect();
    layers.sort_unstable();
    SemanticTile(layers)
}

fn semantic_feature(feature: DetailFeature) -> SemanticFeature {
    let geometry = semantic_geometry(feature.geom_type, &feature);
    let mut attrs: Vec<_> = feature
        .attrs
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    attrs.sort_unstable();
    SemanticFeature {
        id: feature.id,
        attrs,
        geometry,
    }
}

fn semantic_geometry(geom_type: u8, feature: &DetailFeature) -> SemanticGeometry {
    match geom_type {
        1 => {
            let mut points: Vec<_> = feature
                .components
                .iter()
                .flat_map(|component| &component.rings)
                .flat_map(|ring| ring.points.iter().copied())
                .collect();
            points.sort_unstable();
            SemanticGeometry::Points(points)
        }
        2 => {
            let mut paths: Vec<_> = feature
                .components
                .iter()
                .flat_map(|component| &component.rings)
                .map(|ring| ring.points.clone())
                .collect();
            paths.sort_unstable();
            SemanticGeometry::Lines(paths)
        }
        3 => {
            let mut polygons: Vec<_> = feature
                .components
                .iter()
                .map(|component| {
                    let mut rings: Vec<_> = component
                        .rings
                        .iter()
                        .map(|ring| SemanticRing {
                            role: ring.role,
                            points: canonical_cycle(&ring.points),
                        })
                        .collect();
                    rings.sort_unstable();
                    SemanticPolygon(rings)
                })
                .collect();
            polygons.sort_unstable();
            SemanticGeometry::Polygons(polygons)
        }
        other => panic!("unexpected MVT geometry type {other}"),
    }
}

fn canonical_cycle(points: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut open = points.to_vec();
    if open.len() > 1 && open.first() == open.last() {
        open.pop();
    }
    if open.is_empty() {
        return open;
    }

    let mut best = Vec::new();
    let mut initialized = false;
    for direction in [open.clone(), open.iter().rev().copied().collect()] {
        for (start, _) in direction.iter().enumerate() {
            let candidate: Vec<_> = direction
                .iter()
                .cycle()
                .skip(start)
                .take(direction.len())
                .copied()
                .collect();
            if !initialized || candidate < best {
                best = candidate;
                initialized = true;
            }
        }
    }
    best.push(best[0]);
    best
}

fn layer_has_geometry(
    tile: &SemanticTile,
    layer_name: &str,
    predicate: impl Fn(&SemanticGeometry) -> bool,
) -> bool {
    tile.0
        .iter()
        .find(|layer| layer.name == layer_name)
        .is_some_and(|layer| {
            layer
                .features
                .iter()
                .any(|feature| predicate(&feature.geometry))
        })
}
