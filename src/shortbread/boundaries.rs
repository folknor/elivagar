// Boundary and place label layers: boundaries, boundary_labels, place_labels.

use super::*;
use smallvec::{SmallVec, smallvec};

// ---------------------------------------------------------------------------
// Boundaries
// ---------------------------------------------------------------------------

/// Returns admin_level (int) and min_zoom if boundary matches.
fn boundary_match(tags: &Tags<'_>) -> Option<(i64, u8)> {
    if !tags.has_value("boundary", "administrative") {
        return None;
    }
    let level_str = tags.get("admin_level")?;
    let level = parse_i64(level_str)?;
    match level {
        2 => Some((2, 0)),
        4 => Some((4, 7)),
        _ => None,
    }
}

pub(super) fn match_boundaries_line(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some((admin_level, min_zoom)) = boundary_match(tags) {
        let maritime =
            tags.has("maritime") || tags.has_value("natural", "coastline");
        let disputed = tags.has_value("disputed", "yes");
        out.push(LayerMatch {
            layer: Layer::Boundaries,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Line,
            attrs: smallvec![
                attr_int("admin_level", admin_level),
                attr_bool("maritime", maritime),
                attr_bool("disputed", disputed),
            ],
        });
    }
}

pub(super) fn match_boundary_labels(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some((admin_level, _)) = boundary_match(tags) {
        let mut attrs = smallvec![attr_int("admin_level", admin_level)];
        attrs.extend(name_attrs(tags));
        // Default min_zoom is 5. Area-based overrides (z2-z4 for large
        // territories) are applied later in tilegen when geometry is available.
        out.push(LayerMatch {
            layer: Layer::BoundaryLabels,
            min_zoom: 5,
            max_zoom: 14,
            geom_expect: GeomExpect::PolygonPointOnSurface,
            attrs,
        });
    }
}

// ---------------------------------------------------------------------------
// Place labels
// ---------------------------------------------------------------------------

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
            max_zoom: 14,
            geom_expect: GeomExpect::Point,
            attrs,
        });
    }
}

/// Returns (kind, min_zoom, population_default) for a place label.
fn place_label_info(
    tags: &Tags<'_>,
    place: &str,
) -> Option<(&'static str, u8, i64)> {
    // Capital detection
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
