// Place label matching for the Shortbread schema.

use super::*;
use smallvec::{SmallVec, smallvec};

wire_kinds! {
    /// `kind` of place_labels. `capital` and `state_capital` are reachable
    /// only from `place=city`; a capital town, village or hamlet keeps its
    /// place kind and takes only the capital zoom.
    pub(super) enum PlaceKind {
        Capital = "capital",
        StateCapital = "state_capital",
        City = "city",
        Town = "town",
        Village = "village",
        Hamlet = "hamlet",
        Suburb = "suburb",
        Quarter = "quarter",
        Neighbourhood = "neighbourhood",
        IsolatedDwelling = "isolated_dwelling",
        Farm = "farm",
        Island = "island",
        Locality = "locality",
    }
}

pub(super) fn match_place_labels(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if !has_name(tags) {
        return;
    }
    let place = match tags.get("place") {
        Some(v) => v,
        None => return,
    };
    if let Some((kind, min_zoom, pop_default)) = place_label_info(tags, place) {
        let mut attrs = smallvec![attr_kind(kind)];
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
fn place_label_info(tags: &Tags<'_>, place: &str) -> Option<(PlaceKind, u8, i64)> {
    use PlaceKind as K;
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
                K::Capital
            } else if is_state_capital {
                K::StateCapital
            } else {
                K::City
            };
            let zoom = if is_capital || is_state_capital { 4 } else { 6 };
            Some((kind, zoom, 100_000))
        }
        "town" => Some((K::Town, if is_capital { 4 } else { 7 }, 5_000)),
        "village" => Some((K::Village, if is_capital { 4 } else { 10 }, 100)),
        "hamlet" => Some((K::Hamlet, if is_capital { 4 } else { 10 }, 50)),
        "suburb" => Some((K::Suburb, 10, 1_000)),
        "quarter" => Some((K::Quarter, 10, 500)),
        "neighbourhood" => Some((K::Neighbourhood, 10, 100)),
        "isolated_dwelling" => Some((K::IsolatedDwelling, 10, 5)),
        "farm" => Some((K::Farm, 10, 5)),
        "island" => Some((K::Island, 10, 0)),
        "locality" => Some((K::Locality, 10, 0)),
        _ => None,
    }
}
