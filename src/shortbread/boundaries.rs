// Boundary line and label matching for the Shortbread schema.

use super::*;
use smallvec::{SmallVec, smallvec};

// ---------------------------------------------------------------------------
// Boundaries
// ---------------------------------------------------------------------------

/// Supported administrative boundary levels, ordered from country to state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum AdminLevel {
    Country = 2,
    State = 4,
}

impl AdminLevel {
    /// Administrative levels are categorical codes, not locale-formatted counts.
    /// Keep the strict prescan policy for both relations and standalone ways.
    fn parse(raw: &str) -> Option<Self> {
        match raw.parse::<u8>().ok()? {
            2 => Some(Self::Country),
            4 => Some(Self::State),
            _ => None,
        }
    }

    const fn min_zoom(self) -> u8 {
        match self {
            Self::Country => 0,
            Self::State => 7,
        }
    }
}

/// Relation-derived properties folded onto a member way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BoundaryMeta {
    pub(crate) min_admin_level: Option<AdminLevel>,
    pub(crate) disputed: bool,
}

impl BoundaryMeta {
    pub(crate) fn fold_relation(&mut self, relation: Self) {
        self.min_admin_level = match (self.min_admin_level, relation.min_admin_level) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.disputed |= relation.disputed;
    }

    pub(crate) fn from_relation(tags: &Tags<'_>) -> Option<Self> {
        let meta = Self {
            min_admin_level: boundary_level(tags),
            disputed: tags.has_value("boundary", "disputed"),
        };
        (meta.min_admin_level.is_some() || meta.disputed).then_some(meta)
    }
}

fn boundary_level(tags: &Tags<'_>) -> Option<AdminLevel> {
    if !tags.has_value("boundary", "administrative") {
        return None;
    }
    AdminLevel::parse(tags.get("admin_level")?)
}

pub(crate) fn boundary_line(tags: &Tags<'_>, relation: Option<BoundaryMeta>) -> Option<LayerMatch> {
    let mut meta = relation.unwrap_or_default();
    meta.fold_relation(BoundaryMeta {
        min_admin_level: boundary_level(tags),
        disputed: tags.has_value("disputed", "yes"),
    });
    let level = meta.min_admin_level?;
    Some(LayerMatch {
        layer: Layer::Boundaries,
        min_zoom: level.min_zoom(),
        max_zoom: 14,
        geom_expect: GeomExpect::Line,
        paint_rank: 0,
        attrs: smallvec![
            attr_int("admin_level", i64::from(level as u8)),
            attr_bool(
                "maritime",
                tags.has_value("maritime", "yes") || tags.has_value("natural", "coastline")
            ),
            attr_bool("disputed", meta.disputed),
        ],
    })
}

pub(super) fn match_boundaries_line(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(matched) = boundary_line(tags, None) {
        out.push(matched);
    }
}

pub(super) fn match_boundary_labels(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(admin_level) = boundary_level(tags) {
        let mut attrs = smallvec![attr_int("admin_level", i64::from(admin_level as u8))];
        attrs.extend(name_attrs(tags));
        // Default min_zoom is 5; enrich_boundary_label applies area overrides
        // once the emitter has measured the polygon.
        out.push(LayerMatch {
            layer: Layer::BoundaryLabels,
            min_zoom: 5,
            max_zoom: 14,
            geom_expect: Layer::BoundaryLabels.polygon_point_geom_expect(),
            paint_rank: 0,
            attrs,
        });
    }
}

/// Apply the boundary label attributes and zoom policy that depend on area.
pub(crate) fn enrich_boundary_label(matched: &mut LayerMatch, area_m2: f64) {
    let hectares = area_m2 / 10_000.0;
    matched
        .attrs
        .push(("way_area", AttrValue::Float(hectares), 0));

    let area_km2 = area_m2 / 1e6;
    let is_country = matches!(
        matched
            .attrs
            .iter()
            .find(|(key, _, _)| *key == "admin_level")
            .map(|(_, value, _)| value),
        Some(AttrValue::Int(level)) if *level == i64::from(AdminLevel::Country as u8)
    );
    if is_country && area_km2 >= 2_000_000.0 {
        matched.min_zoom = 2;
    } else if area_km2 >= 700_000.0 {
        matched.min_zoom = 3;
    } else if area_km2 >= 100_000.0 {
        matched.min_zoom = 4;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_levels_use_the_same_strict_policy_for_ways_and_relations() {
        for (raw, expected) in [
            ("2", Some(AdminLevel::Country)),
            ("4", Some(AdminLevel::State)),
            ("02", Some(AdminLevel::Country)),
            ("+2", Some(AdminLevel::Country)),
            ("0,2", None),
            (" 2", None),
            ("2 ", None),
            ("6", None),
            ("256", None),
        ] {
            let pairs = [("boundary", "administrative"), ("admin_level", raw)];
            let tags = Tags(&pairs);
            assert_eq!(boundary_level(&tags), expected);
            assert_eq!(
                BoundaryMeta::from_relation(&tags).and_then(|m| m.min_admin_level),
                expected
            );
            assert_eq!(
                boundary_line(&tags, None).map(|m| m.min_zoom),
                expected.map(AdminLevel::min_zoom)
            );
        }
    }
}
