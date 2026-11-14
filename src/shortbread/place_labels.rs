// Place label matching for the Shortbread schema.

use super::*;
use smallvec::{SmallVec, smallvec};

pub(super) fn match_place_labels(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if !has_name(tags) {
        return;
    }
    let place = match tags.get("place") {
        Some(v) => v,
        None => return,
    };
    if let Some((kind, min_zoom, pop_default)) = place_label_info(tags, place) {
        let mut attrs = smallvec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        let population = tags
            .get("population")
            .and_then(parse_i64)
            .unwrap_or(pop_default);
        attrs.push(attr_int("population", population));
        out.push(LayerMatch {
            layer: Layer::PlaceLabels,
            min_zoom,
            max_zoom: MAX_ZOOM,
            geom_expect: GeomExpect::Point,
            paint_rank: 0,
            attrs,
        });
    }
}

/// Returns (kind, min_zoom, population_default) for a place label.
fn place_label_info(tags: &Tags<'_>, place: &str) -> Option<(&'static str, u8, i64)> {
    // Capital detection.
    // NOTE: OSM also uses capital=2 for national capitals (admin_level=2),
    // but neither Planetiler nor Tilemaker handle it - consistent gap across
    // all Shortbread implementations. Rarely used in practice (capital=yes
    // is the standard tagging for national capitals).
    let is_capital = tags.has_value("capital", "yes");
    let is_state_capital = tags.has_value("capital", "4");

    match place {
        "city" => {
            let kind = if is_capital {
                "capital"
            } else if is_state_capital {
                "state_capital"
            } else {
                "city"
            };
            let zoom = if is_capital || is_state_capital { 4 } else { 6 };
            Some((kind, zoom, 100_000))
        }
        "town" => Some(("town", if is_capital { 4 } else { 7 }, 5_000)),
        "village" => Some(("village", if is_capital { 4 } else { 10 }, 100)),
        "hamlet" => Some(("hamlet", if is_capital { 4 } else { 10 }, 50)),
        "suburb" => Some(("suburb", 10, 1_000)),
        "quarter" => Some(("quarter", 10, 500)),
        "neighbourhood" => Some(("neighbourhood", 10, 100)),
        "isolated_dwelling" => Some(("isolated_dwelling", 10, 5)),
        "farm" => Some(("farm", 10, 5)),
        "island" => Some(("island", 10, 0)),
        "locality" => Some(("locality", 10, 0)),
        _ => None,
    }
}
