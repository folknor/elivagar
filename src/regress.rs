use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::Path;

use serde_json::json;
use xxhash_rust::xxh3::xxh3_128;

use crate::pmtiles_reader::{PmtilesReader, TileEntry, gzip_decompress};
use crate::pmtiles_writer::tile_id_to_zxy;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonTile {
    pub layers: Vec<CanonLayer>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonLayer {
    pub name: String,
    pub extent: u32,
    pub features: Vec<CanonFeature>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonFeature {
    pub id: Option<u64>,
    pub geom_type: u8,
    pub attrs: Vec<(String, AttrVal)>,
    pub components: Vec<CanonComponent>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonComponent {
    pub rings: Vec<CanonRing>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonRingRole {
    Point = 0,
    Path = 1,
    Outer = 2,
    Hole = 3,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonRing {
    pub role: CanonRingRole,
    pub points: Vec<(i32, i32)>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AttrVal {
    String(String),
    Float(u32),
    Double(u64),
    Int(i64),
    UInt(u64),
    SInt(i64),
    Bool(bool),
}

pub fn decode_canonical(tile_data: &[u8]) -> Result<CanonTile, String> {
    crate::geometry::mvt_decode::decode_canonical(tile_data)
}

pub fn canon_hash(tile: &CanonTile) -> u128 {
    xxh3_128(&canonical_tile_bytes(tile))
}

#[derive(Clone, Debug)]
pub struct RegressConfig {
    pub tol: i32,
    pub max_moved: u64,
    pub max_examples: usize,
}

impl Default for RegressConfig {
    fn default() -> Self {
        Self {
            tol: 0,
            max_moved: 0,
            max_examples: 20,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TileDiff {
    OnlyInCurrent(u64),
    OnlyInBlessed(u64),
    Content {
        tile_id: u64,
        layers_added: u32,
        layers_removed: u32,
        extent_mismatch: u32,
        missing_features: u32,
        added_features: u32,
        attr_changed: u32,
        tolerance_moved: u32,
        structural_moved: u32,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffTotals {
    pub only_in_current: u64,
    pub only_in_blessed: u64,
    pub layers_added: u64,
    pub layers_removed: u64,
    pub extent_mismatch: u64,
    pub missing_features: u64,
    pub added_features: u64,
    pub attr_changed: u64,
    pub tolerance_moved: u64,
    pub structural_moved: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerCounters {
    pub layers_added: u64,
    pub layers_removed: u64,
    pub extent_mismatch: u64,
    pub missing_features: u64,
    pub added_features: u64,
    pub attr_changed: u64,
    pub tolerance_moved: u64,
    pub structural_moved: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffExample {
    pub tile_id: u64,
    pub layer: String,
    pub id: Option<u64>,
    pub class: String,
}

#[derive(Clone, Debug, Default)]
pub struct RegressReport {
    pub identical_tiles: u64,
    pub diffs: Vec<TileDiff>,
    pub totals: DiffTotals,
    pub per_zoom_layer: BTreeMap<(u8, String), LayerCounters>,
    pub displacement: BTreeMap<(u8, String), Vec<i32>>,
    pub examples: Vec<DiffExample>,
    pub decoded_unique_blobs: u64,
    pub decode_memo_hits: u64,
}

impl RegressReport {
    pub fn passed(&self, cfg: &RegressConfig) -> bool {
        self.totals.only_in_current == 0
            && self.totals.only_in_blessed == 0
            && self.totals.layers_added == 0
            && self.totals.layers_removed == 0
            && self.totals.extent_mismatch == 0
            && self.totals.missing_features == 0
            && self.totals.added_features == 0
            && self.totals.attr_changed == 0
            && self.totals.structural_moved == 0
            && self.totals.tolerance_moved <= cfg.max_moved
    }

    pub fn print_text(&self) {
        println!(
            "identical_tiles={} diffs={} only_current={} only_blessed={} tolerance_moved={} structural_moved={} attr_changed={}",
            self.identical_tiles,
            self.diffs.len(),
            self.totals.only_in_current,
            self.totals.only_in_blessed,
            self.totals.tolerance_moved,
            self.totals.structural_moved,
            self.totals.attr_changed
        );

        for ((z, layer), c) in &self.per_zoom_layer {
            if counters_are_zero(c) {
                continue;
            }
            println!(
                "z{z} {layer} added_layers={} removed_layers={} extent={} missing={} added={} attrs={} tol={} structural={}",
                c.layers_added,
                c.layers_removed,
                c.extent_mismatch,
                c.missing_features,
                c.added_features,
                c.attr_changed,
                c.tolerance_moved,
                c.structural_moved
            );
        }

        for ((z, layer), values) in &self.displacement {
            if values.is_empty() {
                continue;
            }
            let mut sorted = values.clone();
            sorted.sort_unstable();
            let p50 = percentile(&sorted, 50);
            let p95 = percentile(&sorted, 95);
            let max = sorted.last().copied().unwrap_or(0);
            println!("z{z} {layer} displacement p50={p50} p95={p95} max={max}");
        }

        for ex in &self.examples {
            let (z, x, y) = tile_id_to_zxy(ex.tile_id);
            let id = ex
                .id
                .map_or_else(|| "anon".to_string(), |id| id.to_string());
            println!("z{z}/{x}/{y} {} {} {}", ex.layer, id, ex.class);
        }
    }

    pub fn to_json(&self, passed: bool) -> serde_json::Value {
        let per_zoom_layer: Vec<_> = self
            .per_zoom_layer
            .iter()
            .map(|((z, layer), c)| {
                json!({
                    "z": z,
                    "layer": layer,
                    "layers_added": c.layers_added,
                    "layers_removed": c.layers_removed,
                    "extent_mismatch": c.extent_mismatch,
                    "missing_features": c.missing_features,
                    "added_features": c.added_features,
                    "attr_changed": c.attr_changed,
                    "tolerance_moved": c.tolerance_moved,
                    "structural_moved": c.structural_moved,
                })
            })
            .collect();
        let displacement: Vec<_> = self
            .displacement
            .iter()
            .filter(|(_, values)| !values.is_empty())
            .map(|((z, layer), values)| {
                let mut sorted = values.clone();
                sorted.sort_unstable();
                json!({
                    "z": z,
                    "layer": layer,
                    "p50": percentile(&sorted, 50),
                    "p95": percentile(&sorted, 95),
                    "max": sorted.last().copied().unwrap_or(0),
                })
            })
            .collect();
        let examples: Vec<_> = self
            .examples
            .iter()
            .map(|ex| {
                let (z, x, y) = tile_id_to_zxy(ex.tile_id);
                json!({
                    "tile_id": ex.tile_id,
                    "z": z,
                    "x": x,
                    "y": y,
                    "layer": ex.layer,
                    "id": ex.id,
                    "class": ex.class,
                })
            })
            .collect();

        json!({
            "passed": passed,
            "identical_tiles": self.identical_tiles,
            "totals": {
                "only_in_current": self.totals.only_in_current,
                "only_in_blessed": self.totals.only_in_blessed,
                "layers_added": self.totals.layers_added,
                "layers_removed": self.totals.layers_removed,
                "extent_mismatch": self.totals.extent_mismatch,
                "missing_features": self.totals.missing_features,
                "added_features": self.totals.added_features,
                "attr_changed": self.totals.attr_changed,
                "tolerance_moved": self.totals.tolerance_moved,
                "structural_moved": self.totals.structural_moved,
            },
            "per_zoom_layer": per_zoom_layer,
            "displacement": displacement,
            "examples": examples,
            "decoded_unique_blobs": self.decoded_unique_blobs,
            "decode_memo_hits": self.decode_memo_hits,
        })
    }

    pub fn dump_svg_examples(
        &self,
        current: &Path,
        blessed: &Path,
        dir: &Path,
        max_examples: usize,
    ) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        for (idx, ex) in self
            .examples
            .iter()
            .filter(|ex| ex.class.contains("structural"))
            .take(max_examples)
            .enumerate()
        {
            let (z, x, y) = tile_id_to_zxy(ex.tile_id);
            let cur_path = dir.join(format!("{idx:03}-z{z}-x{x}-y{y}-current.svg"));
            let blessed_path = dir.join(format!("{idx:03}-z{z}-x{x}-y{y}-blessed.svg"));
            let mut cur_file = fs::File::create(cur_path)?;
            let mut blessed_file = fs::File::create(blessed_path)?;
            crate::svg::render_tile_svg(current, z, x, y, &mut cur_file)?;
            crate::svg::render_tile_svg(blessed, z, x, y, &mut blessed_file)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BlobKey {
    offset: u64,
    length: u32,
}

#[derive(Clone)]
struct CachedTile {
    hash: u128,
    tile: CanonTile,
}

#[derive(Default)]
struct BlobMemo {
    map: HashMap<BlobKey, CachedTile>,
    hits: u64,
    misses: u64,
}

impl BlobMemo {
    fn get(&mut self, reader: &mut PmtilesReader, entry: &TileEntry) -> io::Result<CachedTile> {
        let key = BlobKey {
            offset: entry.offset,
            length: entry.length,
        };
        if let Some(cached) = self.map.get(&key) {
            self.hits += 1;
            return Ok(cached.clone());
        }

        let raw = reader.read_tile_raw(entry)?;
        let decompressed = gzip_decompress(&raw)?;
        let tile = decode_canonical(&decompressed)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let cached = CachedTile {
            hash: canon_hash(&tile),
            tile,
        };
        self.map.insert(key, cached.clone());
        self.misses += 1;
        Ok(cached)
    }
}

pub fn regress(current: &Path, blessed: &Path, cfg: &RegressConfig) -> io::Result<RegressReport> {
    let mut current_reader = PmtilesReader::open(current)?;
    let mut blessed_reader = PmtilesReader::open(blessed)?;
    validate_supported_archive(&current_reader, current)?;
    validate_supported_archive(&blessed_reader, blessed)?;

    let mut current_entries = current_reader.read_all_entries()?;
    let mut blessed_entries = blessed_reader.read_all_entries()?;
    current_entries.sort_by_key(|e| e.tile_id);
    blessed_entries.sort_by_key(|e| e.tile_id);

    let mut report = RegressReport::default();
    let mut cur_memo = BlobMemo::default();
    let mut blessed_memo = BlobMemo::default();

    let mut ci = 0usize;
    let mut bi = 0usize;
    while ci < current_entries.len() || bi < blessed_entries.len() {
        match (current_entries.get(ci), blessed_entries.get(bi)) {
            (Some(cur), Some(bl)) if cur.tile_id == bl.tile_id => {
                let cur_cached = cur_memo.get(&mut current_reader, cur)?;
                let bl_cached = blessed_memo.get(&mut blessed_reader, bl)?;
                if cur_cached.hash == bl_cached.hash {
                    report.identical_tiles += 1;
                } else {
                    compare_tiles(
                        cur.tile_id,
                        &cur_cached.tile,
                        &bl_cached.tile,
                        cfg,
                        &mut report,
                    );
                }
                ci += 1;
                bi += 1;
            }
            (Some(cur), Some(bl)) if cur.tile_id < bl.tile_id => {
                report.totals.only_in_current += 1;
                report.diffs.push(TileDiff::OnlyInCurrent(cur.tile_id));
                ci += 1;
            }
            (Some(_), Some(bl)) => {
                report.totals.only_in_blessed += 1;
                report.diffs.push(TileDiff::OnlyInBlessed(bl.tile_id));
                bi += 1;
            }
            (Some(cur), None) => {
                report.totals.only_in_current += 1;
                report.diffs.push(TileDiff::OnlyInCurrent(cur.tile_id));
                ci += 1;
            }
            (None, Some(bl)) => {
                report.totals.only_in_blessed += 1;
                report.diffs.push(TileDiff::OnlyInBlessed(bl.tile_id));
                bi += 1;
            }
            (None, None) => break,
        }
    }

    report.decoded_unique_blobs = cur_memo.misses + blessed_memo.misses;
    report.decode_memo_hits = cur_memo.hits + blessed_memo.hits;
    Ok(report)
}

fn validate_supported_archive(reader: &PmtilesReader, path: &Path) -> io::Result<()> {
    if reader.tile_type() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an MVT PMTiles archive", path.display()),
        ));
    }
    if reader.tile_compression() != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} uses non-gzip tile compression", path.display()),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn compare_tiles(
    tile_id: u64,
    current: &CanonTile,
    blessed: &CanonTile,
    cfg: &RegressConfig,
    report: &mut RegressReport,
) {
    let mut tile_counts = ContentCounts::default();
    let current_layers: BTreeMap<&str, &CanonLayer> = current
        .layers
        .iter()
        .map(|layer| (layer.name.as_str(), layer))
        .collect();
    let blessed_layers: BTreeMap<&str, &CanonLayer> = blessed
        .layers
        .iter()
        .map(|layer| (layer.name.as_str(), layer))
        .collect();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    names.extend(current_layers.keys().copied());
    names.extend(blessed_layers.keys().copied());

    for name in names {
        let (z, _, _) = tile_id_to_zxy(tile_id);
        match (current_layers.get(name), blessed_layers.get(name)) {
            (Some(_), None) => {
                tile_counts.layers_added += 1;
                report.totals.layers_added += 1;
                report
                    .per_zoom_layer
                    .entry((z, name.to_string()))
                    .or_default()
                    .layers_added += 1;
                add_example(report, cfg, tile_id, name, None, "layer_added");
            }
            (None, Some(_)) => {
                tile_counts.layers_removed += 1;
                report.totals.layers_removed += 1;
                report
                    .per_zoom_layer
                    .entry((z, name.to_string()))
                    .or_default()
                    .layers_removed += 1;
                add_example(report, cfg, tile_id, name, None, "layer_removed");
            }
            (Some(cur), Some(bl)) if cur.extent != bl.extent => {
                tile_counts.extent_mismatch += 1;
                report.totals.extent_mismatch += 1;
                report
                    .per_zoom_layer
                    .entry((z, name.to_string()))
                    .or_default()
                    .extent_mismatch += 1;
                add_example(report, cfg, tile_id, name, None, "extent_mismatch");
            }
            (Some(cur), Some(bl)) => {
                compare_layer(tile_id, name, cur, bl, cfg, report, &mut tile_counts);
            }
            (None, None) => {}
        }
    }

    if tile_counts.any() {
        report.diffs.push(TileDiff::Content {
            tile_id,
            layers_added: tile_counts.layers_added,
            layers_removed: tile_counts.layers_removed,
            extent_mismatch: tile_counts.extent_mismatch,
            missing_features: tile_counts.missing_features,
            added_features: tile_counts.added_features,
            attr_changed: tile_counts.attr_changed,
            tolerance_moved: tile_counts.tolerance_moved,
            structural_moved: tile_counts.structural_moved,
        });
    } else {
        report.identical_tiles += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_layer(
    tile_id: u64,
    layer_name: &str,
    current: &CanonLayer,
    blessed: &CanonLayer,
    cfg: &RegressConfig,
    report: &mut RegressReport,
    tile_counts: &mut ContentCounts,
) {
    let mut cur_id = Vec::new();
    let mut bl_id = Vec::new();
    let mut cur_anon: BTreeMap<Vec<(String, AttrVal)>, Vec<&CanonFeature>> = BTreeMap::new();
    let mut bl_anon: BTreeMap<Vec<(String, AttrVal)>, Vec<&CanonFeature>> = BTreeMap::new();
    let ocean = layer_name == "ocean";

    for feature in &current.features {
        if !ocean && feature.id.is_some() {
            cur_id.push(feature);
        } else {
            cur_anon
                .entry(feature.attrs.clone())
                .or_default()
                .push(feature);
        }
    }
    for feature in &blessed.features {
        if !ocean && feature.id.is_some() {
            bl_id.push(feature);
        } else {
            bl_anon
                .entry(feature.attrs.clone())
                .or_default()
                .push(feature);
        }
    }

    compare_id_features(
        tile_id,
        layer_name,
        &cur_id,
        &bl_id,
        cfg,
        report,
        tile_counts,
    );

    let mut groups: BTreeSet<Vec<(String, AttrVal)>> = BTreeSet::new();
    groups.extend(cur_anon.keys().cloned());
    groups.extend(bl_anon.keys().cloned());
    for attrs in groups {
        let cur = cur_anon.remove(&attrs).unwrap_or_default();
        let bl = bl_anon.remove(&attrs).unwrap_or_default();
        compare_anonymous_group(tile_id, layer_name, &cur, &bl, cfg, report, tile_counts);
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_id_features(
    tile_id: u64,
    layer_name: &str,
    current: &[&CanonFeature],
    blessed: &[&CanonFeature],
    cfg: &RegressConfig,
    report: &mut RegressReport,
    tile_counts: &mut ContentCounts,
) {
    let mut cur_by_id: BTreeMap<u64, Vec<&CanonFeature>> = BTreeMap::new();
    let mut bl_by_id: BTreeMap<u64, Vec<&CanonFeature>> = BTreeMap::new();
    for feature in current {
        if let Some(id) = feature.id {
            cur_by_id.entry(id).or_default().push(*feature);
        }
    }
    for feature in blessed {
        if let Some(id) = feature.id {
            bl_by_id.entry(id).or_default().push(*feature);
        }
    }

    let mut ids = BTreeSet::new();
    ids.extend(cur_by_id.keys().copied());
    ids.extend(bl_by_id.keys().copied());
    for id in ids {
        let cur = cur_by_id.remove(&id).unwrap_or_default();
        let bl = bl_by_id.remove(&id).unwrap_or_default();
        let pair_count = cur.len().min(bl.len());
        for idx in 0..pair_count {
            let c = cur[idx];
            let b = bl[idx];
            if c.attrs != b.attrs {
                record_feature_class(
                    tile_id,
                    layer_name,
                    c.id,
                    "attr_changed",
                    report,
                    tile_counts,
                    cfg,
                    0,
                );
            } else {
                classify_geometry_pair(tile_id, layer_name, c, b, cfg, report, tile_counts);
            }
        }
        for feature in cur.iter().skip(pair_count) {
            record_feature_class(
                tile_id,
                layer_name,
                feature.id,
                "added_features",
                report,
                tile_counts,
                cfg,
                0,
            );
        }
        for feature in bl.iter().skip(pair_count) {
            record_feature_class(
                tile_id,
                layer_name,
                feature.id,
                "missing_features",
                report,
                tile_counts,
                cfg,
                0,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_anonymous_group(
    tile_id: u64,
    layer_name: &str,
    current: &[&CanonFeature],
    blessed: &[&CanonFeature],
    cfg: &RegressConfig,
    report: &mut RegressReport,
    tile_counts: &mut ContentCounts,
) {
    let pairs = pair_features_by_geometry(current, blessed);
    for (ci, bi) in pairs.paired {
        classify_geometry_pair(
            tile_id,
            layer_name,
            current[ci],
            blessed[bi],
            cfg,
            report,
            tile_counts,
        );
    }
    for ci in pairs.unpaired_current {
        record_feature_class(
            tile_id,
            layer_name,
            current[ci].id,
            "added_features",
            report,
            tile_counts,
            cfg,
            0,
        );
    }
    for bi in pairs.unpaired_blessed {
        record_feature_class(
            tile_id,
            layer_name,
            blessed[bi].id,
            "missing_features",
            report,
            tile_counts,
            cfg,
            0,
        );
    }
}

struct PairResult {
    paired: Vec<(usize, usize)>,
    unpaired_current: Vec<usize>,
    unpaired_blessed: Vec<usize>,
}

fn pair_features_by_geometry(current: &[&CanonFeature], blessed: &[&CanonFeature]) -> PairResult {
    let mut paired = Vec::new();
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; blessed.len()];

    for (ci, cur) in current.iter().enumerate() {
        let cur_key = feature_geometry_bytes(cur);
        if let Some((bi, _)) = blessed
            .iter()
            .enumerate()
            .find(|(bi, bl)| !bl_used[*bi] && cur_key == feature_geometry_bytes(bl))
        {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }

    loop {
        let mut best: Option<(usize, usize, i32)> = None;
        for (ci, cur) in current.iter().enumerate() {
            if cur_used[ci] {
                continue;
            }
            for (bi, bl) in blessed.iter().enumerate() {
                if bl_used[bi] {
                    continue;
                }
                let dist = feature_distance(cur, bl);
                if best.is_none_or(|(_, _, best_dist)| dist < best_dist) {
                    best = Some((ci, bi, dist));
                }
            }
        }
        let Some((ci, bi, _)) = best else {
            break;
        };
        cur_used[ci] = true;
        bl_used[bi] = true;
        paired.push((ci, bi));
    }

    PairResult {
        paired,
        unpaired_current: cur_used
            .iter()
            .enumerate()
            .filter_map(|(idx, used)| (!*used).then_some(idx))
            .collect(),
        unpaired_blessed: bl_used
            .iter()
            .enumerate()
            .filter_map(|(idx, used)| (!*used).then_some(idx))
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
fn classify_geometry_pair(
    tile_id: u64,
    layer_name: &str,
    current: &CanonFeature,
    blessed: &CanonFeature,
    cfg: &RegressConfig,
    report: &mut RegressReport,
    tile_counts: &mut ContentCounts,
) {
    if current.geom_type != blessed.geom_type {
        record_feature_class(
            tile_id,
            layer_name,
            current.id,
            "structural_moved",
            report,
            tile_counts,
            cfg,
            0,
        );
        return;
    }

    let Some(distance) = classify_components(current, blessed) else {
        record_feature_class(
            tile_id,
            layer_name,
            current.id,
            "structural_moved",
            report,
            tile_counts,
            cfg,
            0,
        );
        return;
    };

    if distance == 0 {
        return;
    }
    if distance <= cfg.tol {
        record_feature_class(
            tile_id,
            layer_name,
            current.id,
            "tolerance_moved",
            report,
            tile_counts,
            cfg,
            distance,
        );
    } else {
        record_feature_class(
            tile_id,
            layer_name,
            current.id,
            "structural_moved",
            report,
            tile_counts,
            cfg,
            distance,
        );
    }
}

fn classify_components(current: &CanonFeature, blessed: &CanonFeature) -> Option<i32> {
    if current.components.len() != blessed.components.len() {
        return None;
    }
    let pairs = pair_components(&current.components, &blessed.components);
    if !pairs.unpaired_current.is_empty() || !pairs.unpaired_blessed.is_empty() {
        return None;
    }

    let mut max_dist = 0;
    for (ci, bi) in pairs.paired {
        let cur = &current.components[ci];
        let bl = &blessed.components[bi];
        if !component_structure_matches(cur, bl) {
            return None;
        }
        max_dist = max_dist.max(component_distance(cur, bl));
    }
    Some(max_dist)
}

fn pair_components(current: &[CanonComponent], blessed: &[CanonComponent]) -> PairResult {
    let cur_refs: Vec<_> = current.iter().collect();
    let bl_refs: Vec<_> = blessed.iter().collect();
    let mut paired = Vec::new();
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; blessed.len()];

    for (ci, cur) in cur_refs.iter().enumerate() {
        let cur_key = canonical_component_bytes(cur);
        if let Some((bi, _)) = bl_refs
            .iter()
            .enumerate()
            .find(|(bi, bl)| !bl_used[*bi] && cur_key == canonical_component_bytes(bl))
        {
            cur_used[ci] = true;
            bl_used[bi] = true;
            paired.push((ci, bi));
        }
    }

    loop {
        let mut best: Option<(usize, usize, i32)> = None;
        for (ci, cur) in cur_refs.iter().enumerate() {
            if cur_used[ci] {
                continue;
            }
            for (bi, bl) in bl_refs.iter().enumerate() {
                if bl_used[bi] {
                    continue;
                }
                let dist = component_distance(cur, bl);
                if best.is_none_or(|(_, _, best_dist)| dist < best_dist) {
                    best = Some((ci, bi, dist));
                }
            }
        }
        let Some((ci, bi, _)) = best else {
            break;
        };
        cur_used[ci] = true;
        bl_used[bi] = true;
        paired.push((ci, bi));
    }

    PairResult {
        paired,
        unpaired_current: cur_used
            .iter()
            .enumerate()
            .filter_map(|(idx, used)| (!*used).then_some(idx))
            .collect(),
        unpaired_blessed: bl_used
            .iter()
            .enumerate()
            .filter_map(|(idx, used)| (!*used).then_some(idx))
            .collect(),
    }
}

fn component_structure_matches(current: &CanonComponent, blessed: &CanonComponent) -> bool {
    if current.rings.len() != blessed.rings.len() {
        return false;
    }
    if current
        .rings
        .iter()
        .zip(&blessed.rings)
        .any(|(a, b)| a.role != b.role)
    {
        return false;
    }
    polygon_holes_contained(current) == polygon_holes_contained(blessed)
}

fn polygon_holes_contained(component: &CanonComponent) -> bool {
    let Some(outer) = component
        .rings
        .iter()
        .find(|ring| ring.role == CanonRingRole::Outer)
    else {
        return true;
    };
    component
        .rings
        .iter()
        .filter(|ring| ring.role == CanonRingRole::Hole)
        .all(|hole| {
            hole.points
                .first()
                .is_some_and(|&(x, y)| point_in_ring((x, y), &outer.points))
        })
}

fn point_in_ring(point: (i32, i32), ring: &[(i32, i32)]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let (px, py) = point;
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if (yi > py) != (yj > py) {
            let lhs = i64::from(px - xi) * i64::from(yj - yi);
            let rhs = i64::from(xj - xi) * i64::from(py - yi);
            let crosses = if yj > yi { lhs < rhs } else { lhs > rhs };
            if crosses {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

fn feature_distance(current: &CanonFeature, blessed: &CanonFeature) -> i32 {
    if current.geom_type != blessed.geom_type {
        return i32::MAX;
    }
    let mut best = i32::MAX;
    for cur in &current.components {
        for bl in &blessed.components {
            best = best.min(component_distance(cur, bl));
        }
    }
    best
}

fn component_distance(current: &CanonComponent, blessed: &CanonComponent) -> i32 {
    let mut max_dist = 0;
    for (cur, bl) in current.rings.iter().zip(&blessed.rings) {
        max_dist = max_dist.max(discrete_hausdorff(&cur.points, &bl.points));
    }
    max_dist
}

fn discrete_hausdorff(a: &[(i32, i32)], b: &[(i32, i32)]) -> i32 {
    if a.is_empty() || b.is_empty() {
        return i32::MAX;
    }
    let ab = directed_distance(a, b);
    let ba = directed_distance(b, a);
    ceil_sqrt(ab.max(ba))
}

fn directed_distance(a: &[(i32, i32)], b: &[(i32, i32)]) -> u128 {
    a.iter()
        .map(|&pa| {
            b.iter()
                .map(|&pb| squared_distance(pa, pb))
                .min()
                .unwrap_or(u128::MAX)
        })
        .max()
        .unwrap_or(0)
}

fn squared_distance(a: (i32, i32), b: (i32, i32)) -> u128 {
    let dx = i128::from(a.0) - i128::from(b.0);
    let dy = i128::from(a.1) - i128::from(b.1);
    #[allow(clippy::cast_sign_loss)]
    {
        (dx * dx + dy * dy) as u128
    }
}

fn ceil_sqrt(n: u128) -> i32 {
    if n == 0 {
        return 0;
    }
    let mut lo = 1u128;
    let mut hi = 1u128 << 64;
    while lo < hi {
        let mid = (lo + hi) / 2;
        match mid.saturating_mul(mid).cmp(&n) {
            Ordering::Less => lo = mid + 1,
            Ordering::Equal => return i32::try_from(mid).unwrap_or(i32::MAX),
            Ordering::Greater => hi = mid,
        }
    }
    i32::try_from(lo).unwrap_or(i32::MAX)
}

#[allow(clippy::too_many_arguments)]
fn record_feature_class(
    tile_id: u64,
    layer_name: &str,
    id: Option<u64>,
    class: &str,
    report: &mut RegressReport,
    tile_counts: &mut ContentCounts,
    cfg: &RegressConfig,
    displacement: i32,
) {
    let (z, _, _) = tile_id_to_zxy(tile_id);
    let counters = report
        .per_zoom_layer
        .entry((z, layer_name.to_string()))
        .or_default();
    match class {
        "missing_features" => {
            tile_counts.missing_features += 1;
            report.totals.missing_features += 1;
            counters.missing_features += 1;
        }
        "added_features" => {
            tile_counts.added_features += 1;
            report.totals.added_features += 1;
            counters.added_features += 1;
        }
        "attr_changed" => {
            tile_counts.attr_changed += 1;
            report.totals.attr_changed += 1;
            counters.attr_changed += 1;
        }
        "tolerance_moved" => {
            tile_counts.tolerance_moved += 1;
            report.totals.tolerance_moved += 1;
            counters.tolerance_moved += 1;
            report
                .displacement
                .entry((z, layer_name.to_string()))
                .or_default()
                .push(displacement);
        }
        "structural_moved" => {
            tile_counts.structural_moved += 1;
            report.totals.structural_moved += 1;
            counters.structural_moved += 1;
            report
                .displacement
                .entry((z, layer_name.to_string()))
                .or_default()
                .push(displacement);
        }
        _ => {}
    }
    add_example(report, cfg, tile_id, layer_name, id, class);
}

fn add_example(
    report: &mut RegressReport,
    cfg: &RegressConfig,
    tile_id: u64,
    layer_name: &str,
    id: Option<u64>,
    class: &str,
) {
    if report
        .examples
        .iter()
        .filter(|ex| ex.class == class)
        .count()
        >= cfg.max_examples
    {
        return;
    }
    report.examples.push(DiffExample {
        tile_id,
        layer: layer_name.to_string(),
        id,
        class: class.to_string(),
    });
}

#[derive(Default)]
struct ContentCounts {
    layers_added: u32,
    layers_removed: u32,
    extent_mismatch: u32,
    missing_features: u32,
    added_features: u32,
    attr_changed: u32,
    tolerance_moved: u32,
    structural_moved: u32,
}

impl ContentCounts {
    fn any(&self) -> bool {
        self.layers_added
            + self.layers_removed
            + self.extent_mismatch
            + self.missing_features
            + self.added_features
            + self.attr_changed
            + self.tolerance_moved
            + self.structural_moved
            > 0
    }
}

fn counters_are_zero(c: &LayerCounters) -> bool {
    c.layers_added
        + c.layers_removed
        + c.extent_mismatch
        + c.missing_features
        + c.added_features
        + c.attr_changed
        + c.tolerance_moved
        + c.structural_moved
        == 0
}

fn percentile(sorted: &[i32], pct: usize) -> i32 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * pct) / 100;
    sorted[idx]
}

pub(crate) fn sort_components(components: &mut [CanonComponent]) {
    components.sort_by_key(canonical_component_bytes);
}

pub(crate) fn canonical_component_bytes(component: &CanonComponent) -> Vec<u8> {
    let mut out = Vec::new();
    write_component(&mut out, component);
    out
}

fn feature_geometry_bytes(feature: &CanonFeature) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(feature.geom_type);
    write_len(&mut out, feature.components.len());
    for component in &feature.components {
        write_component(&mut out, component);
    }
    out
}

fn canonical_tile_bytes(tile: &CanonTile) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"elivagar-canon-v1");
    write_len(&mut out, tile.layers.len());
    for layer in &tile.layers {
        write_string(&mut out, &layer.name);
        out.extend_from_slice(&layer.extent.to_le_bytes());
        write_len(&mut out, layer.features.len());
        for feature in &layer.features {
            write_feature(&mut out, feature);
        }
    }
    out
}

fn write_feature(out: &mut Vec<u8>, feature: &CanonFeature) {
    match feature.id {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id.to_le_bytes());
        }
        None => out.push(0),
    }
    out.push(feature.geom_type);
    write_len(out, feature.attrs.len());
    for (key, value) in &feature.attrs {
        write_string(out, key);
        write_attr(out, value);
    }
    write_len(out, feature.components.len());
    for component in &feature.components {
        write_component(out, component);
    }
}

fn write_component(out: &mut Vec<u8>, component: &CanonComponent) {
    write_len(out, component.rings.len());
    for ring in &component.rings {
        out.push(ring.role as u8);
        write_len(out, ring.points.len());
        for &(x, y) in &ring.points {
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
    }
}

fn write_attr(out: &mut Vec<u8>, value: &AttrVal) {
    match value {
        AttrVal::String(s) => {
            out.push(1);
            write_string(out, s);
        }
        AttrVal::Float(bits) => {
            out.push(2);
            out.extend_from_slice(&bits.to_le_bytes());
        }
        AttrVal::Double(bits) => {
            out.push(3);
            out.extend_from_slice(&bits.to_le_bytes());
        }
        AttrVal::Int(v) => {
            out.push(4);
            out.extend_from_slice(&v.to_le_bytes());
        }
        AttrVal::UInt(v) => {
            out.push(5);
            out.extend_from_slice(&v.to_le_bytes());
        }
        AttrVal::SInt(v) => {
            out.push(6);
            out.extend_from_slice(&v.to_le_bytes());
        }
        AttrVal::Bool(v) => {
            out.push(7);
            out.push(u8::from(*v));
        }
    }
}

fn write_string(out: &mut Vec<u8>, value: &str) {
    write_len(out, value.len());
    out.extend_from_slice(value.as_bytes());
}

fn write_len(out: &mut Vec<u8>, len: usize) {
    let n = u64::try_from(len).expect("usize length should fit in u64");
    out.extend_from_slice(&n.to_le_bytes());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
