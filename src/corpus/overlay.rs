use std::sync::Arc;

use crate::corpus::render::{path_data, xml_attr, xml_text};
use crate::regress::{DetailAttr, DetailFeature, DetailLayer, DiffSink, OutcomeClass};

fn feature_id(f: Option<&DetailFeature>) -> String {
    f.and_then(|f| f.id)
        .map_or_else(|| "none".to_string(), |id| id.to_string())
}
// One panel line per changed attribute key, comparand (old) -> current (new).
// DetailAttr's derived Debug is the pinned canonical, float-free formatting.
fn attr_diff_lines(
    layer: &str,
    current: Option<&DetailFeature>,
    blessed: Option<&DetailFeature>,
) -> Vec<String> {
    let id = feature_id(current.or(blessed));
    let empty: &[(Arc<str>, DetailAttr)] = &[];
    let cur = current.map_or(empty, |f| &f.attrs);
    let bl = blessed.map_or(empty, |f| &f.attrs);
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

#[derive(Clone)]
struct Event {
    layer: Arc<str>,
    class: OutcomeClass,
    displacement: i32,
    current: Option<DetailFeature>,
    blessed: Option<DetailFeature>,
}
#[derive(Default)]
pub struct OverlayCollector {
    events: Vec<Event>,
}
impl DiffSink for OverlayCollector {
    fn record(
        &mut self,
        layer: &Arc<str>,
        class: OutcomeClass,
        displacement: i32,
        current: Option<&DetailFeature>,
        blessed: Option<&DetailFeature>,
    ) {
        self.events.push(Event {
            layer: Arc::clone(layer),
            class,
            displacement,
            current: current.cloned(),
            blessed: blessed.cloned(),
        });
    }
    fn matched(&mut self, layer: &Arc<str>, current: &DetailFeature, blessed: &DetailFeature) {
        self.record(
            layer,
            OutcomeClass::ToleranceMoved,
            0,
            Some(current),
            Some(blessed),
        );
    }
    fn layer_event(
        &mut self,
        class: OutcomeClass,
        current: Option<&DetailLayer>,
        blessed: Option<&DetailLayer>,
    ) {
        let layer = current.or(blessed).expect("layer event");
        for f in current.map_or(&[][..], |l| &l.features) {
            self.record(&layer.name, class, 0, Some(f), None);
        }
        for f in blessed.map_or(&[][..], |l| &l.features) {
            self.record(&layer.name, class, 0, None, Some(f));
        }
    }
}
fn draw(out: &mut String, feature: &DetailFeature, color: &str, class: &str, opacity: &str) {
    let paths: Vec<_> = feature
        .components
        .iter()
        .flat_map(|c| c.rings.iter().map(|r| r.points.as_slice()))
        .collect();
    let close = feature.geom_type == 3;
    let fill = if close { color } else { "none" };
    let stroke = if close { "none" } else { color };
    out.push_str(&format!("<path data-class=\"{class}\" d=\"{}\" fill=\"{fill}\" stroke=\"{stroke}\" opacity=\"{opacity}\"><title>{class}</title></path>\n",xml_attr(&path_data(&paths,close))));
}
pub fn render_overlay(collector: &OverlayCollector, background: &str) -> Vec<u8> {
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 4096 5120\"><rect width=\"4096\" height=\"4096\" fill=\"{}\"/>\n",
        xml_attr(background)
    );
    let mut attrs = Vec::new();
    for e in &collector.events {
        let changed = e
            .current
            .as_ref()
            .zip(e.blessed.as_ref())
            .is_some_and(|(a, b)| a.geometry_digest != b.geometry_digest);
        match e.class {
            OutcomeClass::AddedFeatures | OutcomeClass::LayerAdded => {
                if let Some(f) = &e.current {
                    draw(&mut out, f, "#e91e63", e.class.name(), "0.7");
                }
            }
            OutcomeClass::MissingFeatures | OutcomeClass::LayerRemoved => {
                if let Some(f) = &e.blessed {
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
                };
                if changed && let Some(f) = &e.blessed {
                    draw(&mut out, f, "#2196f3", e.class.name(), "0.7");
                };
                attrs.extend(attr_diff_lines(
                    &e.layer,
                    e.current.as_ref(),
                    e.blessed.as_ref(),
                ));
            }
            OutcomeClass::ToleranceMoved | OutcomeClass::StructuralMoved => {
                if e.displacement == 0 {
                    if let Some(f) = &e.current {
                        draw(&mut out, f, "#999999", "unchanged", "0.25");
                    }
                } else {
                    if let Some(f) = &e.current {
                        draw(&mut out, f, "#e91e63", e.class.name(), "0.7");
                    };
                    if let Some(f) = &e.blessed {
                        draw(&mut out, f, "#2196f3", e.class.name(), "0.7");
                    }
                }
            }
            OutcomeClass::ExtentMismatch => {}
        }
    }
    out.push_str("<g font-family=\"monospace\" font-size=\"20\"><text x=\"20\" y=\"4140\">grey unchanged; pink current; blue comparand; orange attributes</text>");
    for (i, line) in attrs.iter().take(24).enumerate() {
        out.push_str(&format!(
            "<text x=\"20\" y=\"{}\">{}</text>",
            4170 + i * 24,
            xml_text(line)
        ));
    }
    if attrs.len() > 24 {
        out.push_str(&format!(
            "<text x=\"20\" y=\"{}\">(+{} more)</text>",
            4170 + 24 * 24,
            attrs.len() - 24
        ));
    }
    out.push_str("</g></svg>\n");
    out.into_bytes()
}
