//! `--overlay`: per-tile attribution SVGs.
//!
//! A counter tells you a tile changed; an overlay tells you *where*. Each SVG
//! superimposes the two archives' geometry for one differing tile, colour-coded
//! by what the diff decided: pink is current, blue is the comparand, grey is
//! geometry the diff matched exactly (the unchanged backdrop the changes are
//! read against), orange marks a feature whose attributes moved but whose
//! geometry did not. A panel beneath the tile lists the attribute changes.
//!
//! This is a second consumer of the same comparison walk (`DiffSink`), not a
//! re-derivation - an overlay always draws the diff the report counted.
//!
//! Ported from elivagar's shed `corpus/overlay.rs` (commit `0129ef3~1`).

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use super::super::constants::{
    OVERLAY_ATTRIBUTE_LIMIT, OVERLAY_ATTRIBUTE_START_Y, OVERLAY_LEGEND_Y, OVERLAY_LINE_HEIGHT,
    OVERLAY_PANEL_HEIGHT, SVG_TILE_SIZE, mvt,
};
use super::super::corpus::layout::tile_svg_file_name;
use super::super::corpus::render::{path_data, xml_attr, xml_text};
use super::super::eliv::{ArchiveView, DetailAttr, Strictness, find_entry, tile_id_to_zxy};
use super::compare::{DiffSink, OutcomeClass, compare_detail_tiles};
use super::engine::invalid_tile;
use super::prepared::{PreparedFeature, PreparedLayer, decode_prepared_tile};
use super::report::{RegressConfig, RegressReport};

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Event {
    layer: Arc<str>,
    class: OutcomeClass,
    displacement: i32,
    current: Option<PreparedFeature>,
    baseline: Option<PreparedFeature>,
}

/// Unlike the engine's sink this keeps the features themselves - it has to draw
/// them - so it is only ever run over the handful of tiles `--overlay-max`
/// admits, never over a whole archive.
#[derive(Default)]
pub(crate) struct OverlayCollector {
    events: Vec<Event>,
}

impl DiffSink for OverlayCollector {
    fn record(
        &mut self,
        layer: &Arc<str>,
        class: OutcomeClass,
        displacement: i32,
        current: Option<&PreparedFeature>,
        baseline: Option<&PreparedFeature>,
    ) {
        self.events.push(Event {
            layer: Arc::clone(layer),
            class,
            displacement,
            current: current.cloned(),
            baseline: baseline.cloned(),
        });
    }
    fn matched(&mut self, layer: &Arc<str>, current: &PreparedFeature, baseline: &PreparedFeature) {
        // Displacement 0 is what marks this as the unchanged backdrop below.
        self.record(
            layer,
            OutcomeClass::ToleranceMoved,
            0,
            Some(current),
            Some(baseline),
        );
    }
    fn layer_event(
        &mut self,
        class: OutcomeClass,
        current: Option<&PreparedLayer>,
        baseline: Option<&PreparedLayer>,
    ) {
        let Some(layer) = current.or(baseline) else {
            return;
        };
        for f in current.map_or(&[][..], |l| &l.features) {
            self.record(&layer.name, class, 0, Some(f), None);
        }
        for f in baseline.map_or(&[][..], |l| &l.features) {
            self.record(&layer.name, class, 0, None, Some(f));
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn feature_id(f: Option<&PreparedFeature>) -> String {
    f.and_then(|f| f.id)
        .map_or_else(|| "none".to_string(), |id| id.to_string())
}

// One panel line per changed attribute key, comparand (old) -> current (new).
// DetailAttr's derived Debug is the pinned canonical, float-free formatting.
fn attr_diff_lines(
    layer: &str,
    current: Option<&PreparedFeature>,
    baseline: Option<&PreparedFeature>,
) -> Vec<String> {
    let id = feature_id(current.or(baseline));
    let empty: &[(Arc<str>, DetailAttr)] = &[];
    let cur = current.map_or(empty, |f| &f.attrs);
    let bl = baseline.map_or(empty, |f| &f.attrs);
    let mut lines = Vec::new();
    for (key, value) in cur {
        match bl.iter().find(|(bk, _)| bk == key) {
            Some((_, bv)) if bv == value => {}
            Some((_, bv)) => lines.push(format!("{layer} id={id} {key}: {bv:?} -> {value:?}")),
            None => lines.push(format!("{layer} id={id} {key}: (absent) -> {value:?}")),
        }
    }
    for (key, bv) in bl {
        if !cur.iter().any(|(ck, _)| ck == key) {
            lines.push(format!("{layer} id={id} {key}: {bv:?} -> (absent)"));
        }
    }
    lines
}

fn draw(out: &mut String, feature: &PreparedFeature, color: &str, class: &str, opacity: &str) {
    let paths: Vec<_> = feature
        .components
        .iter()
        .flat_map(|c| c.rings.iter().map(|r| r.points.as_slice()))
        .collect();
    let close = feature.geom_type == mvt::GEOMETRY_POLYGON;
    let fill = if close { color } else { "none" };
    let stroke = if close { "none" } else { color };
    out.push_str(&format!(
        "<path data-class=\"{class}\" d=\"{}\" fill=\"{fill}\" stroke=\"{stroke}\" opacity=\"{opacity}\"><title>{class}</title></path>\n",
        xml_attr(&path_data(&paths, close))
    ));
}

/// The tile occupies the shared SVG extent; the lower panel lists a bounded
/// number of attributes.
pub(crate) fn render_overlay(collector: &OverlayCollector, background: &str) -> Vec<u8> {
    let view_height = SVG_TILE_SIZE + OVERLAY_PANEL_HEIGHT;
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {SVG_TILE_SIZE} {view_height}\"><rect width=\"{SVG_TILE_SIZE}\" height=\"{SVG_TILE_SIZE}\" fill=\"{}\"/>\n",
        xml_attr(background)
    );
    let mut attrs = Vec::new();
    for e in &collector.events {
        let changed = e
            .current
            .as_ref()
            .zip(e.baseline.as_ref())
            .is_some_and(|(a, b)| a.geometry_digest != b.geometry_digest);
        match e.class {
            OutcomeClass::AddedFeatures | OutcomeClass::LayerAdded => {
                if let Some(f) = &e.current {
                    draw(&mut out, f, "#e91e63", e.class.name(), "0.7");
                }
            }
            OutcomeClass::MissingFeatures | OutcomeClass::LayerRemoved => {
                if let Some(f) = &e.baseline {
                    draw(&mut out, f, "#2196f3", e.class.name(), "0.7");
                }
            }
            OutcomeClass::AttrChanged => {
                if let Some(f) = &e.current {
                    draw(
                        &mut out,
                        f,
                        if changed { "#e91e63" } else { "#ff9800" },
                        e.class.name(),
                        "0.7",
                    );
                }
                if changed && let Some(f) = &e.baseline {
                    draw(&mut out, f, "#2196f3", e.class.name(), "0.7");
                }
                attrs.extend(attr_diff_lines(
                    &e.layer,
                    e.current.as_ref(),
                    e.baseline.as_ref(),
                ));
            }
            // The unchanged backdrop, and only that: `matched()` is the sole
            // producer of a ToleranceMoved at displacement 0 (compare.rs routes
            // a real tolerance move through `record` with distance >= 1). Gating
            // this on displacement alone swept StructuralMoved in with it, and
            // StructuralMoved is *deliberately* recorded at distance 0 for the
            // topology changes - geometry type, component count, ring role,
            // hole containment, force-zipped pairs - where "moved by N" would be
            // a false reassurance. Those drew as grey backdrop while the report
            // counted them as structural.
            OutcomeClass::ToleranceMoved if e.displacement == 0 => {
                if let Some(f) = &e.current {
                    draw(&mut out, f, "#999999", "unchanged", "0.25");
                }
            }
            OutcomeClass::ToleranceMoved | OutcomeClass::StructuralMoved => {
                if let Some(f) = &e.current {
                    draw(&mut out, f, "#e91e63", e.class.name(), "0.7");
                }
                if let Some(f) = &e.baseline {
                    draw(&mut out, f, "#2196f3", e.class.name(), "0.7");
                }
            }
            OutcomeClass::ExtentMismatch | OutcomeClass::Count => {}
        }
    }
    out.push_str(&format!("<g font-family=\"monospace\" font-size=\"20\"><text x=\"20\" y=\"{OVERLAY_LEGEND_Y}\">grey unchanged; pink current; blue comparand; orange attributes</text>"));
    for (i, line) in attrs.iter().take(OVERLAY_ATTRIBUTE_LIMIT).enumerate() {
        out.push_str(&format!(
            "<text x=\"20\" y=\"{}\">{}</text>",
            OVERLAY_ATTRIBUTE_START_Y + i * OVERLAY_LINE_HEIGHT,
            xml_text(line)
        ));
    }
    if attrs.len() > OVERLAY_ATTRIBUTE_LIMIT {
        out.push_str(&format!(
            "<text x=\"20\" y=\"{}\">(+{} more)</text>",
            OVERLAY_ATTRIBUTE_START_Y + OVERLAY_ATTRIBUTE_LIMIT * OVERLAY_LINE_HEIGHT,
            attrs.len() - OVERLAY_ATTRIBUTE_LIMIT
        ));
    }
    out.push_str("</g></svg>\n");
    out.into_bytes()
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// How and where to render. Bundled because the renderer's own knobs are
/// unrelated to the two archives and the report it draws from.
pub(crate) struct OverlayOptions<'a> {
    pub(crate) dir: &'a Path,
    pub(crate) max: usize,
    pub(crate) background: &'a str,
    pub(crate) strictness: Strictness,
}

/// Emit attribution SVGs for the first differing tile ids, capped at `max`.
///
/// A tile absent from one archive decodes as an empty tile rather than an error,
/// so a tile that exists on only one side still renders (everything in it draws
/// as added or missing).
pub(crate) fn dump_overlays(
    current: &Path,
    baseline: &Path,
    report: &RegressReport,
    cfg: &RegressConfig,
    opts: &OverlayOptions<'_>,
) -> io::Result<u64> {
    let OverlayOptions {
        dir,
        max,
        background,
        strictness,
    } = *opts;
    fs::create_dir_all(dir)?;
    let current_view = ArchiveView::open(current)?;
    let baseline_view = ArchiveView::open(baseline)?;
    let current_runs = current_view.read_all_runs()?;
    let baseline_runs = baseline_view.read_all_runs()?;
    let mut tile_ids = Vec::new();
    'ranges: for range in &report.differing_ranges {
        for tile_id in range.start..range.end {
            if tile_ids.len() >= max {
                break 'ranges;
            }
            tile_ids.push(tile_id);
        }
    }
    let mut expected = BTreeSet::new();
    for tile_id in &tile_ids {
        let read =
            |view: &ArchiveView, runs: &[super::super::eliv::RawDirEntry]| -> io::Result<Vec<u8>> {
                match find_entry(runs, *tile_id) {
                    Some(entry) => view.read_tile(entry.blob()),
                    None => Ok(Vec::new()),
                }
            };
        let cur = decode_prepared_tile(&read(&current_view, &current_runs)?, strictness)
            .map_err(invalid_tile)?;
        let bl = decode_prepared_tile(&read(&baseline_view, &baseline_runs)?, strictness)
            .map_err(invalid_tile)?;
        let collector = compare_detail_tiles::<OverlayCollector>(&cur, &bl, cfg);
        let (z, x, y) = tile_id_to_zxy(*tile_id);
        let name = tile_svg_file_name(z, x, y, &[]);
        fs::write(dir.join(&name), render_overlay(&collector, background))?;
        expected.insert(name);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_file()
            && is_overlay_tile_svg(&name)
            && !name.to_str().is_some_and(|name| expected.contains(name))
        {
            fs::remove_file(entry.path())?;
        }
    }
    u64::try_from(tile_ids.len())
        .map_err(|_| io::Error::other("overlay tile count does not fit u64"))
}

fn is_overlay_tile_svg(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(rest) = name.strip_prefix('z') else {
        return false;
    };
    let Some((zoom, rest)) = rest.split_once("-x") else {
        return false;
    };
    let Some((x, y)) = rest.split_once("-y") else {
        return false;
    };
    let Some(y) = y.strip_suffix(".svg") else {
        return false;
    };
    let (Ok(zoom), Ok(x), Ok(y)) = (zoom.parse::<u8>(), x.parse::<u32>(), y.parse::<u32>()) else {
        return false;
    };
    zoom <= 31
        && u64::from(x) < (1u64 << zoom)
        && u64::from(y) < (1u64 << zoom)
        && tile_svg_file_name(zoom, x, y, &[]) == name
}
