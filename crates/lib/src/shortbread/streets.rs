// Street infrastructure layers: streets, street_polygons, street_labels,
// street_labels_points, streets_polygons_labels, bridges.

use super::*;
use smallvec::smallvec;

const STREET_FLAG_ATTRIBUTE_MIN_ZOOM: u8 = 11;

// ---------------------------------------------------------------------------
// Street kinds
// ---------------------------------------------------------------------------

wire_kinds! {
    /// The class of a street, rail or aeroway line: the `kind` value the streets
    /// and street_polygons layers emit. Declared in paint order, so the class
    /// rank is the discriminant. Street zoom floors are not a class property
    /// (street_labels uses its own floors and rail has a service override), so
    /// they stay in the matcher rows.
    pub enum StreetClass {
        Track = "track",
        Footway = "footway",
        Steps = "steps",
        Path = "path",
        Cycleway = "cycleway",
        Pedestrian = "pedestrian",
        LivingStreet = "living_street",
        Service = "service",
        Busway = "busway",
        BusGuideway = "bus_guideway",
        Taxiway = "taxiway",
        Runway = "runway",
        Unclassified = "unclassified",
        Residential = "residential",
        Tertiary = "tertiary",
        Secondary = "secondary",
        Primary = "primary",
        Trunk = "trunk",
        Motorway = "motorway",
        Funicular = "funicular",
        Monorail = "monorail",
        Tram = "tram",
        LightRail = "light_rail",
        Subway = "subway",
        NarrowGauge = "narrow_gauge",
        Rail = "rail",
    }
}

impl StreetClass {
    /// The class whose wire name is `name`, if the streets layers emit it.
    pub fn from_name(name: &str) -> Option<StreetClass> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }

    /// Whether the class is a railway, which carries the `rail` attribute and
    /// no oneway attributes.
    pub const fn is_rail(self) -> bool {
        use StreetClass::*;
        matches!(
            self,
            Funicular | Monorail | Tram | LightRail | Subway | NarrowGauge | Rail
        )
    }

    /// Within-layer paint rank: class-major, then tunnel below normal below
    /// bridge, then link below its parent. The largest value is 25*6+2*2+1.
    pub const fn paint_rank(self, link: bool, tunnel: bool, bridge: bool) -> u8 {
        let elev = if tunnel {
            0
        } else if bridge {
            2
        } else {
            1
        };
        (self as u8) * 6 + elev * 2 + if link { 0 } else { 1 }
    }
}

wire_kinds! {
    /// The five road classes that have `_link` variants. The wire name is
    /// the raw OSM value, which street_labels keeps as its `kind`.
    pub enum LinkClass {
        Motorway = "motorway_link",
        Trunk = "trunk_link",
        Primary = "primary_link",
        Secondary = "secondary_link",
        Tertiary = "tertiary_link",
    }
}

impl LinkClass {
    /// The parent class, which the streets layer emits as `kind`.
    pub const fn class(self) -> StreetClass {
        match self {
            LinkClass::Motorway => StreetClass::Motorway,
            LinkClass::Trunk => StreetClass::Trunk,
            LinkClass::Primary => StreetClass::Primary,
            LinkClass::Secondary => StreetClass::Secondary,
            LinkClass::Tertiary => StreetClass::Tertiary,
        }
    }
}

/// A street line as matched: a class, or a link road of one of the five
/// classes that have links. The streets layer splits a link into its parent
/// class plus `link=true`; street_labels keeps the raw `_link` name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreetKind {
    Road(StreetClass),
    Link(LinkClass),
}

impl StreetKind {
    pub const fn class(self) -> StreetClass {
        match self {
            StreetKind::Road(class) => class,
            StreetKind::Link(link) => link.class(),
        }
    }

    pub const fn is_link(self) -> bool {
        matches!(self, StreetKind::Link(_))
    }

    /// The `kind` attribute street_labels emits: the class, or the raw
    /// `_link` name.
    pub(crate) fn label_attr(self) -> Attr {
        match self {
            StreetKind::Road(class) => attr_kind(class),
            StreetKind::Link(link) => attr_kind(link),
        }
    }
}

// ---------------------------------------------------------------------------
// Streets (line)
// ---------------------------------------------------------------------------

/// Returns (kind, min_zoom) for the first listed match.
fn street_match(tags: &Tags<'_>) -> Option<(StreetKind, u8)> {
    use StreetClass as C;
    use StreetKind::{Link, Road};
    let (kind, mut zoom) = transport::first_tag_match(
        tags,
        &[
            ("highway", "motorway", (Road(C::Motorway), 5)),
            ("highway", "trunk", (Road(C::Trunk), 6)),
            ("highway", "motorway_link", (Link(LinkClass::Motorway), 9)),
            ("highway", "trunk_link", (Link(LinkClass::Trunk), 9)),
            ("highway", "primary", (Road(C::Primary), 8)),
            ("highway", "primary_link", (Link(LinkClass::Primary), 8)),
            ("highway", "secondary", (Road(C::Secondary), 9)),
            ("highway", "secondary_link", (Link(LinkClass::Secondary), 9)),
            ("highway", "tertiary", (Road(C::Tertiary), 10)),
            ("highway", "tertiary_link", (Link(LinkClass::Tertiary), 10)),
            ("highway", "unclassified", (Road(C::Unclassified), 12)),
            ("highway", "residential", (Road(C::Residential), 12)),
            ("highway", "busway", (Road(C::Busway), 12)),
            ("highway", "bus_guideway", (Road(C::BusGuideway), 12)),
            ("highway", "living_street", (Road(C::LivingStreet), 13)),
            ("highway", "service", (Road(C::Service), 13)),
            ("highway", "pedestrian", (Road(C::Pedestrian), 13)),
            ("highway", "track", (Road(C::Track), 13)),
            ("highway", "footway", (Road(C::Footway), 13)),
            ("highway", "steps", (Road(C::Steps), 13)),
            ("highway", "path", (Road(C::Path), 13)),
            ("highway", "cycleway", (Road(C::Cycleway), 13)),
            ("aeroway", "runway", (Road(C::Runway), 11)),
            ("aeroway", "taxiway", (Road(C::Taxiway), 13)),
            ("railway", "rail", (Road(C::Rail), 10)),
            ("railway", "narrow_gauge", (Road(C::NarrowGauge), 10)),
            ("railway", "light_rail", (Road(C::LightRail), 10)),
            ("railway", "subway", (Road(C::Subway), 10)),
            ("railway", "tram", (Road(C::Tram), 10)),
            ("railway", "funicular", (Road(C::Funicular), 10)),
            ("railway", "monorail", (Road(C::Monorail), 10)),
        ],
    )?;
    // The z8 service override applies to both rail kinds with a non-empty
    // service value. Empty and missing service follow the z10 rule.
    if matches!(kind, Road(C::Rail | C::NarrowGauge))
        && tags
            .get("service")
            .is_some_and(|service| !service.is_empty())
    {
        zoom = 8;
    }
    Some((kind, zoom))
}

pub(super) fn match_streets_line(tags: &Tags<'_>, out: &mut Hits) {
    let (street, min_zoom) = match street_match(tags) {
        Some(v) => v,
        None => return,
    };
    let class = street.class();
    let is_link = street.is_link();
    let is_rail = class.is_rail();
    // kind: always emitted. Boolean attrs only when true (saves ~3-6 tags/feature).
    let mut attrs = smallvec![attr_kind(class)];
    if is_link {
        attrs.push(attr_bool(BoolKey::Link, true).visible_from(STREET_FLAG_ATTRIBUTE_MIN_ZOOM));
    }
    if is_rail {
        attrs.push(attr_bool(BoolKey::Rail, true));
    }
    if is_tunnel(tags) {
        attrs.push(attr_bool(BoolKey::Tunnel, true).visible_from(STREET_FLAG_ATTRIBUTE_MIN_ZOOM));
    }
    if is_bridge(tags) {
        attrs.push(attr_bool(BoolKey::Bridge, true).visible_from(STREET_FLAG_ATTRIBUTE_MIN_ZOOM));
    }
    // Oneway: only for non-railway, only when true
    if !is_rail {
        let oneway_val = tags.get("oneway");
        let is_oneway = matches!(
            oneway_val,
            Some("yes") | Some("1") | Some("true") | Some("-1")
        );
        let is_reverse = oneway_val == Some("-1");
        if is_oneway {
            attrs.push(attr_bool(BoolKey::Oneway, true).visible_from(14));
        }
        if is_reverse {
            attrs.push(attr_bool(BoolKey::OnewayReverse, true).visible_from(14));
        }
    }
    // Optional string attrs
    if let Some(v) = tags.get("tracktype") {
        attrs.push(attr_dyn(StrKey::Tracktype, v).visible_from(11));
    }
    if let Some(v) = tags.get("surface") {
        attrs.push(attr_dyn(StrKey::Surface, v).visible_from(11));
    }
    if let Some(v) = tags.get("service") {
        attrs.push(attr_dyn(StrKey::Service, v).visible_from(11));
    }
    if let Some(v) = tags.get("bicycle") {
        attrs.push(attr_dyn(StrKey::Bicycle, v).visible_from(14));
    }
    if let Some(v) = tags.get("horse") {
        attrs.push(attr_dyn(StrKey::Horse, v).visible_from(14));
    }
    let paint_rank = class.paint_rank(is_link, is_tunnel(tags), is_bridge(tags));
    out.push(LayerMatch {
        layer: Layer::Streets,
        min_zoom,
        geom_expect: GeomExpect::Line,
        paint_rank,
        attrs,
    });
}

// ---------------------------------------------------------------------------
// Street polygons
// ---------------------------------------------------------------------------

pub(super) fn match_street_polygons(tags: &Tags<'_>, out: &mut Hits) {
    if let Some((class, min_zoom)) = street_polygon_match(tags) {
        let mut attrs = smallvec![attr_kind(class)];
        if is_bridge(tags) {
            attrs.push(attr_bool(BoolKey::Bridge, true));
        }
        if is_tunnel(tags) {
            attrs.push(attr_bool(BoolKey::Tunnel, true));
        }
        if let Some(v) = tags.get("service") {
            attrs.push(attr_dyn(StrKey::Service, v));
        }
        if let Some(v) = tags.get("surface") {
            attrs.push(attr_dyn(StrKey::Surface, v));
        }
        let paint_rank = class.paint_rank(false, is_tunnel(tags), is_bridge(tags));
        out.push(LayerMatch {
            layer: Layer::StreetPolygons,
            min_zoom,
            geom_expect: GeomExpect::Polygon,
            paint_rank,
            attrs,
        });
    }
}

fn street_polygon_match(tags: &Tags<'_>) -> Option<(StreetClass, u8)> {
    transport::first_tag_match(
        tags,
        &[
            ("area:aeroway", "runway", (StreetClass::Runway, 11)),
            ("area:aeroway", "taxiway", (StreetClass::Taxiway, 13)),
            ("highway", "pedestrian", (StreetClass::Pedestrian, 14)),
            ("highway", "service", (StreetClass::Service, 14)),
        ],
    )
}

// ---------------------------------------------------------------------------
// Street labels
// ---------------------------------------------------------------------------

pub(super) fn match_street_labels_line(tags: &Tags<'_>, out: &mut Hits) {
    let has_ref = tags.has("ref");
    if !has_name(tags) && !has_ref {
        return;
    }
    let (kind, min_zoom) = match street_label_kind_and_zoom(tags) {
        Some(class) => class,
        None => return,
    };
    let mut attrs = smallvec![kind.label_attr()];
    attrs.extend(name_attrs(tags));
    // ref: semicolons → newlines
    if let Some(r) = tags.get("ref") {
        let replaced = r.replace(';', "\n");
        let ref_rows = replaced.lines().count();
        let ref_cols = replaced
            .lines()
            .map(|s| s.chars().count())
            .max()
            .unwrap_or(0);
        attrs.push(attr_dyn(StrKey::Ref, &replaced));
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        {
            attrs.push(attr_int(IntKey::RefRows, ref_rows as i64));
            attrs.push(attr_int(IntKey::RefCols, ref_cols as i64));
        }
    }
    if is_tunnel(tags) {
        attrs.push(attr_bool(BoolKey::Tunnel, true));
    }
    out.push(LayerMatch {
        layer: Layer::StreetLabels,
        min_zoom,
        geom_expect: GeomExpect::Line,
        paint_rank: 0,
        attrs,
    });
}

fn street_label_kind_and_zoom(tags: &Tags<'_>) -> Option<(StreetKind, u8)> {
    use StreetClass as C;
    use StreetKind::{Link, Road};
    transport::first_tag_match(
        tags,
        &[
            ("highway", "motorway", (Road(C::Motorway), 10)),
            ("highway", "trunk", (Road(C::Trunk), 12)),
            ("highway", "primary", (Road(C::Primary), 12)),
            ("highway", "secondary", (Road(C::Secondary), 13)),
            ("highway", "tertiary", (Road(C::Tertiary), 13)),
            ("highway", "motorway_link", (Link(LinkClass::Motorway), 13)),
            ("highway", "trunk_link", (Link(LinkClass::Trunk), 13)),
            ("highway", "primary_link", (Link(LinkClass::Primary), 13)),
            (
                "highway",
                "secondary_link",
                (Link(LinkClass::Secondary), 13),
            ),
            ("highway", "tertiary_link", (Link(LinkClass::Tertiary), 14)),
            ("highway", "unclassified", (Road(C::Unclassified), 14)),
            ("highway", "residential", (Road(C::Residential), 14)),
            ("highway", "busway", (Road(C::Busway), 14)),
            ("highway", "bus_guideway", (Road(C::BusGuideway), 14)),
            ("highway", "living_street", (Road(C::LivingStreet), 14)),
            ("highway", "service", (Road(C::Service), 14)),
            ("highway", "pedestrian", (Road(C::Pedestrian), 14)),
            ("highway", "track", (Road(C::Track), 14)),
            ("highway", "footway", (Road(C::Footway), 14)),
            ("highway", "steps", (Road(C::Steps), 14)),
            ("highway", "path", (Road(C::Path), 14)),
            ("highway", "cycleway", (Road(C::Cycleway), 14)),
            ("aeroway", "runway", (Road(C::Runway), 11)),
            ("aeroway", "taxiway", (Road(C::Taxiway), 13)),
            ("railway", "rail", (Road(C::Rail), 10)),
            ("railway", "narrow_gauge", (Road(C::NarrowGauge), 10)),
            ("railway", "light_rail", (Road(C::LightRail), 10)),
            ("railway", "subway", (Road(C::Subway), 10)),
            ("railway", "tram", (Road(C::Tram), 10)),
            ("railway", "funicular", (Road(C::Funicular), 10)),
            ("railway", "monorail", (Road(C::Monorail), 10)),
        ],
    )
}

// ---------------------------------------------------------------------------
// Street label points
// ---------------------------------------------------------------------------

wire_kinds! {
    /// `kind` of street_labels_points.
    pub(super) enum StreetLabelPointKind {
        MotorwayJunction = "motorway_junction",
    }
}

pub(super) fn match_street_labels_points(tags: &Tags<'_>, out: &mut Hits) {
    if !tags.has_value("highway", "motorway_junction") {
        return;
    }
    let mut attrs = smallvec![attr_kind(StreetLabelPointKind::MotorwayJunction)];
    if let Some(v) = tags.get("ref") {
        attrs.push(attr_dyn(StrKey::Ref, v));
    }
    attrs.extend(name_attrs(tags));
    out.push(LayerMatch {
        layer: Layer::StreetLabelsPoints,
        min_zoom: 12,
        geom_expect: GeomExpect::Point,
        paint_rank: 0,
        attrs,
    });
}

// ---------------------------------------------------------------------------
// Streets polygons labels
// ---------------------------------------------------------------------------

pub(super) fn match_streets_polygons_labels(tags: &Tags<'_>, out: &mut Hits) {
    if !has_name(tags) {
        return;
    }
    if let Some(kind) = street_polygon_label_kind(tags) {
        let mut attrs = smallvec![attr_kind(kind)];
        attrs.extend(name_attrs(tags));
        out.push(LayerMatch {
            layer: Layer::StreetsPolygonsLabels,
            min_zoom: 14,
            geom_expect: Layer::StreetsPolygonsLabels.polygon_point_geom_expect(),
            paint_rank: 0,
            attrs,
        });
    }
}

fn street_polygon_label_kind(tags: &Tags<'_>) -> Option<StreetClass> {
    transport::first_tag_match(
        tags,
        &[
            ("aeroway", "runway", StreetClass::Runway),
            ("aeroway", "taxiway", StreetClass::Taxiway),
            ("highway", "pedestrian", StreetClass::Pedestrian),
            ("highway", "service", StreetClass::Service),
        ],
    )
}

// ---------------------------------------------------------------------------
// Bridges
// ---------------------------------------------------------------------------

wire_kinds! {
    /// `kind` of bridges.
    pub(super) enum BridgeKind {
        Bridge = "bridge",
    }
}

pub(super) fn match_bridges(tags: &Tags<'_>, out: &mut Hits) {
    if tags.has_value("man_made", "bridge") {
        out.push(LayerMatch {
            layer: Layer::Bridges,
            min_zoom: 12,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs: smallvec![attr_kind(BridgeKind::Bridge)],
        });
    }
}

#[cfg(test)]
mod street_kind_tests {
    use super::{LinkClass, StreetClass};

    #[test]
    fn all_is_every_variant_in_order() {
        assert_eq!(StreetClass::ALL.len(), StreetClass::Rail as usize + 1);
        for (i, class) in StreetClass::ALL.into_iter().enumerate() {
            assert_eq!(class as usize, i);
            assert_eq!(StreetClass::from_name(class.as_str()), Some(class));
        }
        assert_eq!(LinkClass::ALL.len(), LinkClass::Tertiary as usize + 1);
        for (i, link) in LinkClass::ALL.into_iter().enumerate() {
            assert_eq!(link as usize, i);
            assert_eq!(link.as_str(), format!("{}_link", link.class().as_str()));
        }
    }

    /// Independent of the rank method: (name, class rank, is_rail).
    #[test]
    fn class_ranks_match_independent_fixture() {
        let fixture = [
            ("track", 0, false),
            ("footway", 1, false),
            ("steps", 2, false),
            ("path", 3, false),
            ("cycleway", 4, false),
            ("pedestrian", 5, false),
            ("living_street", 6, false),
            ("service", 7, false),
            ("busway", 8, false),
            ("bus_guideway", 9, false),
            ("taxiway", 10, false),
            ("runway", 11, false),
            ("unclassified", 12, false),
            ("residential", 13, false),
            ("tertiary", 14, false),
            ("secondary", 15, false),
            ("primary", 16, false),
            ("trunk", 17, false),
            ("motorway", 18, false),
            ("funicular", 19, true),
            ("monorail", 20, true),
            ("tram", 21, true),
            ("light_rail", 22, true),
            ("subway", 23, true),
            ("narrow_gauge", 24, true),
            ("rail", 25, true),
        ];
        assert_eq!(fixture.len(), StreetClass::ALL.len());
        for (name, rank, rail) in fixture {
            let class = StreetClass::from_name(name).expect("fixture class is a StreetClass");
            assert_eq!(class.paint_rank(true, true, false), rank * 6, "{name}");
            assert_eq!(class.is_rail(), rail, "{name}");
        }
    }

    #[test]
    fn link_below_parent_and_bridge_above_tunnel() {
        let m = StreetClass::Motorway;
        assert!(m.paint_rank(true, false, false) < m.paint_rank(false, false, false));
        assert!(m.paint_rank(false, true, false) < m.paint_rank(false, false, true));
        assert_eq!(
            StreetClass::Rail.paint_rank(false, false, true),
            25 * 6 + 2 * 2 + 1
        );
    }
}
