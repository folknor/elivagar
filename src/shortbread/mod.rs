// Shortbread vector tile schema - hardcoded Rust implementation.
//
// Reference: <https://shortbread.geofabrik.de/>
// No YAML parsing - every layer, filter, and attribute mapping is compiled code.
//
// Layer matching is split by domain:
//   water.rs      - water polygons/lines/labels, dam, pier
//   boundaries.rs - boundaries, boundary_labels
//   place_labels.rs - place_labels
//   land.rs       - land, sites, buildings, addresses
//   streets.rs    - streets, street_polygons, street_labels, bridges
//   transport.rs  - aerialways, ferries, public_transport
//   pois.rs       - points of interest

#[cfg(test)]
use crate::tiling::MAX_ZOOM;
use smallvec::SmallVec;
use std::borrow::Cow;

/// Declares one family of `kind` attribute values: a `#[repr(u8)]` enum with
/// implicit discriminants in declaration order, `ALL` in that order, the wire
/// name per variant, and the `WireKind` impl `attr_kind` takes. Each
/// variant's dictionary id in `wire_format::KIND_VALUES` is computed in a
/// `const` block (the discriminant is a family-local index, not that id), so
/// a new kind missing from the dictionary fails the build: append it there
/// (never reorder) and bump `CHECKPOINT_VERSION`, since the table's ids land
/// in sort-record bytes. Names are asserted distinct at compile time.
/// Kind families are declared only through this macro; the sealed trait
/// keeps implementations inside `shortbread`, and the convention keeps them
/// inside the macro.
macro_rules! wire_kinds {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$vmeta:meta])* $variant:ident = $wire:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        #[repr(u8)]
        $vis enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Every variant in declaration order.
            pub const ALL: [Self; [$(stringify!($variant)),+].len()] = [$(Self::$variant),+];

            /// The `kind` attribute value on the wire.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }
        }

        impl crate::shortbread::sealed::Sealed for $name {}

        impl crate::shortbread::WireKind for $name {
            fn kind_id(self) -> crate::wire_format::KindId {
                match self {
                    $(Self::$variant => const { crate::wire_format::KindId::interned($wire) },)+
                }
            }
        }

        const _: () = {
            let all = $name::ALL;
            let mut i = 0;
            while i < all.len() {
                let mut j = i + 1;
                while j < all.len() {
                    assert!(
                        !crate::wire_format::const_str_eq(all[i].as_str(), all[j].as_str()),
                        concat!("two ", stringify!($name), " variants share a wire name")
                    );
                    j += 1;
                }
                i += 1;
            }
        };
    };
}

mod sealed {
    pub(crate) trait Sealed {}
}

/// A `kind` value declared by `wire_kinds!`. The only way to build a `kind`
/// attribute is `attr_kind`, which takes one of these.
pub(crate) trait WireKind: sealed::Sealed + Copy {
    fn kind_id(self) -> crate::wire_format::KindId;
}

/// The value type a key declares. Interned kinds are `Str`: the dictionary
/// is a sort-record encoding, and every consumer sees a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueType {
    Str,
    Int,
    Bool,
    Float,
}

/// Declares the attribute key schema once: each key's wire id (its
/// discriminant in every generated enum), output name and value type. The
/// typed key enums are what the `attr_*` constructors take, so a value of the
/// wrong type for its key, or a `kind` built from a string, does not compile.
/// `AttrKey` is the union. Ids land in sort-record bytes, which tie-break
/// equal sort keys: append new keys, never renumber, and moving an id moves
/// `CHECKPOINT_VERSION`.
macro_rules! attr_keys {
    (
        kind: $kind:ident = $kind_id:literal, $kind_name:literal;
        $($group:ident => $vt:ident { $($v:ident = $id:literal, $name:literal;)+ })+
    ) => {
        /// Every attribute key the profile emits.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u8)]
        pub(crate) enum AttrKey {
            $kind = $kind_id,
            $($($v = $id,)+)+
        }

        $(
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #[repr(u8)]
            pub(crate) enum $group {
                $($v = $id,)+
            }

            impl From<$group> for AttrKey {
                fn from(key: $group) -> Self {
                    match key {
                        $($group::$v => Self::$v,)+
                    }
                }
            }

            impl $group {
                /// The key's output name.
                #[allow(dead_code)]
                pub(crate) fn name(self) -> &'static str {
                    AttrKey::from(self).name()
                }
            }
        )+

        impl AttrKey {
            pub(crate) const ALL: [Self; [$kind_name, $($($name,)+)+].len()] =
                [Self::$kind, $($(Self::$v,)+)+];

            pub(crate) const fn name(self) -> &'static str {
                match self {
                    Self::$kind => $kind_name,
                    $($(Self::$v => $name,)+)+
                }
            }

            pub(crate) const fn value_type(self) -> ValueType {
                match self {
                    Self::$kind => ValueType::Str,
                    $($(Self::$v => ValueType::$vt,)+)+
                }
            }

            pub(crate) const fn from_id(id: u8) -> Option<Self> {
                match id {
                    $kind_id => Some(Self::$kind),
                    $($($id => Some(Self::$v),)+)+
                    _ => None,
                }
            }

            #[allow(dead_code)]
            pub(crate) fn from_name(name: &str) -> Option<Self> {
                match name {
                    $kind_name => Some(Self::$kind),
                    $($($name => Some(Self::$v),)+)+
                    _ => None,
                }
            }
        }

        // Ids are dense from 0 and names distinct, so `from_id` covers
        // exactly the declared keys.
        const _: () = {
            let all = AttrKey::ALL;
            assert!(all.len() <= 256, "attribute key ids are u8");
            let mut seen = [false; AttrKey::ALL.len()];
            let mut i = 0;
            while i < all.len() {
                let id = all[i] as usize;
                assert!(id < all.len(), "attribute key ids must be dense from 0");
                assert!(!seen[id], "two attribute keys share an id");
                seen[id] = true;
                let mut j = i + 1;
                while j < all.len() {
                    assert!(
                        !crate::wire_format::const_str_eq(all[i].name(), all[j].name()),
                        "two attribute keys share a name"
                    );
                    j += 1;
                }
                i += 1;
            }
        };
    };
}

attr_keys! {
    kind: Kind = 0, "kind";
    StrKey => Str {
        Name = 1, "name";
        NameEn = 2, "name_en";
        NameDe = 3, "name_de";
        Surface = 9, "surface";
        Tracktype = 10, "tracktype";
        Service = 11, "service";
        Bicycle = 14, "bicycle";
        Horse = 15, "horse";
        Ref = 16, "ref";
        Iata = 22, "iata";
        Housenumber = 23, "housenumber";
        Housename = 24, "housename";
        Amenity = 25, "amenity";
        Highway = 26, "highway";
        Office = 27, "office";
        Leisure = 28, "leisure";
        ManMade = 29, "man_made";
        Tourism = 30, "tourism";
        Cuisine = 31, "cuisine";
        Vending = 32, "vending";
        Religion = 33, "religion";
        Denomination = 34, "denomination";
        Sport = 35, "sport";
        TowerType = 36, "tower:type";
        Information = 37, "information";
        Shop = 43, "shop";
        Emergency = 44, "emergency";
        Historic = 45, "historic";
        Natural = 51, "natural";
    }
    IntKey => Int {
        AdminLevel = 4, "admin_level";
        RefRows = 17, "ref_rows";
        RefCols = 18, "ref_cols";
        Population = 19, "population";
        Ele = 48, "ele";
    }
    BoolKey => Bool {
        Bridge = 5, "bridge";
        Tunnel = 6, "tunnel";
        Link = 7, "link";
        Rail = 8, "rail";
        Oneway = 12, "oneway";
        OnewayReverse = 13, "oneway_reverse";
        Maritime = 20, "maritime";
        Disputed = 21, "disputed";
        Atm = 38, "atm";
        RecyclingGlassBottles = 39, "recycling:glass_bottles";
        RecyclingPaper = 40, "recycling:paper";
        RecyclingClothes = 41, "recycling:clothes";
        RecyclingScrapMetal = 42, "recycling:scrap_metal";
    }
    FloatKey => Float {
        WayArea = 46, "way_area";
        Height = 47, "height";
        MinHeight = 49, "min_height";
        BuildingLevels = 50, "building:levels";
    }
}

pub(crate) mod boundaries;
mod land;
pub(crate) mod osm_value;
mod place_labels;
pub(crate) mod pois;
mod streets;
mod transport;
mod water;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// What geometry the MVT feature should carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GeomExpect {
    Point,
    Line,
    Polygon,
    PolygonCentroid,
    PolygonPointOnSurface,
}

/// Classification of the incoming OSM element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OsmGeomType {
    Node,
    OpenWay,
    ClosedWay,
    MultiPolygon,
}

/// How a layer places a point when its source geometry is an area.
/// `tests/fixtures/shortbread.yml` distinguishes water label centroids,
/// address centroids only for convex polygons, and point-on-surface for the
/// other polygon point layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolygonPointStrategy {
    Centroid,
    CentroidIfConvex,
    PointOnSurface,
}

impl PolygonPointStrategy {
    const fn geom_expect(self) -> GeomExpect {
        match self {
            // The emitter reads this layer policy to distinguish unconditional
            // centroids from the convex-only policy sharing this dispatch arm.
            Self::Centroid | Self::CentroidIfConvex => GeomExpect::PolygonCentroid,
            Self::PointOnSurface => GeomExpect::PolygonPointOnSurface,
        }
    }
}

macro_rules! geometry_matches {
    ($geom_type:expr;) => {
        false
    };
    ($geom_type:expr; $first:ident $(, $rest:ident)*) => {
        matches!($geom_type, OsmGeomType::$first $(| OsmGeomType::$rest)*)
    };
}

// This list is in archived MVT tile order. The explicit order is checked
// against the layer ID because the ID also controls the wire sort key.
macro_rules! define_layers {
    (
        $(
            $variant:ident = $id:literal,
            $name:literal,
            tile_order = $tile_order:literal,
            min_zoom = $min_zoom:literal,
            fanout_cap = $fanout_cap:literal,
            accepts = [$($accepted_geom:ident),*],
            polygon_point = $polygon_point:expr,
            fields = [$($field:ident),*];
        )*
    ) => {
        const LAYER_COUNT: usize = [$(stringify!($variant)),*].len();

        /// MVT layer in the Shortbread schema.
        /// `#[repr(u8)]` makes the wire ID a zero-cost index into layer arrays.
        /// Tile order is explicit in this list and must match each wire ID.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u8)]
        pub enum Layer {
            $($variant = $id,)*
        }

        impl Layer {
            /// All layers in archived MVT tile order.
            pub const ALL: [Layer; LAYER_COUNT] = [$(Self::$variant),*];

            /// MVT tile order. This currently matches the wire ID.
            pub const fn tile_order(self) -> u8 {
                match self {
                    $(Self::$variant => $tile_order,)*
                }
            }

            /// Minimum zoom floor for features in this layer.
            pub const fn min_zoom(self) -> u8 {
                match self {
                    $(Self::$variant => $min_zoom,)*
                }
            }

            /// Whether OSM polygon emission consults this layer's fanout cap.
            pub const fn supports_fanout_cap(self) -> bool {
                match self {
                    $(Self::$variant => $fanout_cap,)*
                }
            }

            /// Whether this layer's matcher accepts this OSM geometry kind.
            pub(crate) const fn accepts(self, geom_type: OsmGeomType) -> bool {
                match self {
                    $(Self::$variant => geometry_matches!(geom_type; $($accepted_geom),*),)*
                }
            }

            /// Area point placement policy for point features derived from polygons.
            pub(crate) const fn polygon_point_strategy(self) -> Option<PolygonPointStrategy> {
                match self {
                    $(Self::$variant => $polygon_point,)*
                }
            }

            /// Current coarse geometry expectation used by the emitter.
            pub(crate) const fn polygon_point_geom_expect(self) -> GeomExpect {
                match self.polygon_point_strategy() {
                    Some(strategy) => strategy.geom_expect(),
                    None => panic!("layer has no polygon point strategy"),
                }
            }

            /// MVT layer name string.
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// Parse an MVT layer name into a Layer variant.
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)*
                    _ => None,
                }
            }

            pub(crate) const fn allowed_keys(self) -> &'static [AttrKey] {
                match self {
                    $(Self::$variant => &[$(AttrKey::$field),*],)*
                }
            }

            /// Canonical TileJSON field map. Numeric types share Number.
            pub fn fields(self) -> std::collections::BTreeMap<String, String> {
                self.allowed_keys().iter().map(|key| {
                    let ty = match key.value_type() {
                        ValueType::Str => "String",
                        ValueType::Int | ValueType::Float => "Number",
                        ValueType::Bool => "Boolean",
                    };
                    (key.name().to_owned(), ty.to_owned())
                }).collect()
            }

            /// Total number of layers.
            pub const fn count() -> usize {
                LAYER_COUNT
            }
        }

        const _: () = {
            $(
                assert!($id == $tile_order);
                assert!(Layer::ALL[$tile_order] as u8 == $id);
            )*
        };
    };
}

define_layers! {
    WaterPolygons = 0, "water_polygons",
        tile_order = 0, min_zoom = 4, fanout_cap = true,
        accepts = [ClosedWay, MultiPolygon], polygon_point = None,
        fields = [Kind];
    WaterPolygonsLabels = 1, "water_polygons_labels",
        tile_order = 1, min_zoom = 4, fanout_cap = false,
        accepts = [ClosedWay, MultiPolygon], polygon_point = Some(PolygonPointStrategy::Centroid),
        fields = [Kind, Name, NameEn, NameDe];
    WaterLines = 2, "water_lines",
        tile_order = 2, min_zoom = 9, fanout_cap = false,
        accepts = [OpenWay, ClosedWay], polygon_point = None,
        fields = [Kind, Tunnel, Bridge];
    WaterLinesLabels = 3, "water_lines_labels",
        tile_order = 3, min_zoom = 9, fanout_cap = false,
        accepts = [OpenWay, ClosedWay], polygon_point = None,
        fields = [Kind, Tunnel, Bridge, Name, NameEn, NameDe];
    DamLines = 4, "dam_lines",
        tile_order = 4, min_zoom = 12, fanout_cap = false,
        accepts = [OpenWay, ClosedWay], polygon_point = None,
        fields = [Kind];
    DamPolygons = 5, "dam_polygons",
        tile_order = 5, min_zoom = 12, fanout_cap = true,
        accepts = [ClosedWay, MultiPolygon], polygon_point = None,
        fields = [Kind];
    PierLines = 6, "pier_lines",
        tile_order = 6, min_zoom = 12, fanout_cap = false,
        accepts = [OpenWay, ClosedWay], polygon_point = None,
        fields = [Kind];
    PierPolygons = 7, "pier_polygons",
        tile_order = 7, min_zoom = 12, fanout_cap = true,
        accepts = [ClosedWay, MultiPolygon], polygon_point = None,
        fields = [Kind];
    Boundaries = 8, "boundaries",
        tile_order = 8, min_zoom = 0, fanout_cap = false,
        accepts = [OpenWay, ClosedWay], polygon_point = None,
        fields = [AdminLevel, Maritime, Disputed];
    BoundaryLabels = 9, "boundary_labels",
        tile_order = 9, min_zoom = 2, fanout_cap = false,
        accepts = [ClosedWay, MultiPolygon], polygon_point = Some(PolygonPointStrategy::PointOnSurface),
        fields = [AdminLevel, WayArea, Name, NameEn, NameDe];
    PlaceLabels = 10, "place_labels",
        tile_order = 10, min_zoom = 4, fanout_cap = false,
        accepts = [Node], polygon_point = None,
        fields = [Kind, Population, Name, NameEn, NameDe];
    Land = 11, "land",
        tile_order = 11, min_zoom = 7, fanout_cap = true,
        accepts = [OpenWay, ClosedWay, MultiPolygon], polygon_point = None,
        fields = [Kind];
    Sites = 12, "sites",
        tile_order = 12, min_zoom = 14, fanout_cap = true,
        accepts = [ClosedWay, MultiPolygon], polygon_point = None,
        fields = [Kind];
    Buildings = 13, "buildings",
        tile_order = 13, min_zoom = 14, fanout_cap = true,
        accepts = [ClosedWay, MultiPolygon], polygon_point = None,
        fields = [Height, MinHeight, BuildingLevels];
    Addresses = 14, "addresses",
        tile_order = 14, min_zoom = 14, fanout_cap = false,
        accepts = [Node, ClosedWay, MultiPolygon], polygon_point = Some(PolygonPointStrategy::CentroidIfConvex),
        fields = [Housename, Housenumber];
    Streets = 15, "streets",
        tile_order = 15, min_zoom = 5, fanout_cap = false,
        accepts = [OpenWay, ClosedWay], polygon_point = None,
        fields = [Kind, Link, Rail, Tunnel, Bridge, Oneway, OnewayReverse, Tracktype, Surface, Service, Bicycle, Horse];
    StreetPolygons = 16, "street_polygons",
        tile_order = 16, min_zoom = 11, fanout_cap = true,
        accepts = [ClosedWay, MultiPolygon], polygon_point = None,
        fields = [Kind, Bridge, Tunnel, Service, Surface];
    StreetLabels = 17, "street_labels",
        tile_order = 17, min_zoom = 10, fanout_cap = false,
        accepts = [OpenWay, ClosedWay], polygon_point = None,
        fields = [Kind, Ref, RefRows, RefCols, Tunnel, Name, NameEn, NameDe];
    StreetLabelsPoints = 18, "street_labels_points",
        tile_order = 18, min_zoom = 12, fanout_cap = false,
        accepts = [Node], polygon_point = None,
        fields = [Kind, Ref, Name, NameEn, NameDe];
    StreetsPolygonsLabels = 19, "streets_polygons_labels",
        tile_order = 19, min_zoom = 14, fanout_cap = false,
        accepts = [ClosedWay, MultiPolygon], polygon_point = Some(PolygonPointStrategy::PointOnSurface),
        fields = [Kind, Name, NameEn, NameDe];
    Bridges = 20, "bridges",
        tile_order = 20, min_zoom = 12, fanout_cap = true,
        accepts = [ClosedWay, MultiPolygon], polygon_point = None,
        fields = [Kind];
    Aerialways = 21, "aerialways",
        tile_order = 21, min_zoom = 12, fanout_cap = false,
        accepts = [OpenWay, ClosedWay], polygon_point = None,
        fields = [Kind];
    Ferries = 22, "ferries",
        tile_order = 22, min_zoom = 10, fanout_cap = false,
        accepts = [OpenWay, ClosedWay], polygon_point = None,
        fields = [Kind, Name, NameEn, NameDe];
    PublicTransport = 23, "public_transport",
        tile_order = 23, min_zoom = 11, fanout_cap = false,
        accepts = [Node, ClosedWay, MultiPolygon], polygon_point = Some(PolygonPointStrategy::PointOnSurface),
        fields = [Kind, Iata, Name, NameEn, NameDe];
    Pois = 24, "pois",
        tile_order = 24, min_zoom = 14, fanout_cap = false,
        accepts = [Node, ClosedWay, MultiPolygon], polygon_point = Some(PolygonPointStrategy::PointOnSurface),
        fields = [Name, NameEn, NameDe, Housename, Housenumber, Amenity, Highway, Office, Leisure, ManMade, Tourism, Cuisine, Vending, Religion, Denomination, Sport, TowerType, Information, Shop, Emergency, Historic, Natural, Ele, Atm, RecyclingGlassBottles, RecyclingPaper, RecyclingClothes, RecyclingScrapMetal];
    Ocean = 25, "ocean",
        tile_order = 25, min_zoom = 0, fanout_cap = false,
        accepts = [], polygon_point = None,
        fields = [];
}

const fn accepted_layer_set(geom_type: OsmGeomType) -> ([Layer; LAYER_COUNT], usize) {
    let mut accepted = [Layer::Ocean; LAYER_COUNT];
    let mut count = 0;
    let mut index = 0;
    while index < LAYER_COUNT {
        let layer = Layer::ALL[index];
        if layer.accepts(geom_type) {
            accepted[count] = layer;
            count += 1;
        }
        index += 1;
    }
    (accepted, count)
}

const NODE_LAYER_SET: ([Layer; LAYER_COUNT], usize) = accepted_layer_set(OsmGeomType::Node);
const OPEN_WAY_LAYER_SET: ([Layer; LAYER_COUNT], usize) = accepted_layer_set(OsmGeomType::OpenWay);
const CLOSED_WAY_LAYER_SET: ([Layer; LAYER_COUNT], usize) =
    accepted_layer_set(OsmGeomType::ClosedWay);
const MULTIPOLYGON_LAYER_SET: ([Layer; LAYER_COUNT], usize) =
    accepted_layer_set(OsmGeomType::MultiPolygon);

/// Tag lookup helper. Uses linear scan - with 3-15 tags per OSM element,
/// this is faster than sorting + binary search. Binary search was tried
/// (ca6f17e) and reverted: sort_unstable_by_key on every element plus
/// binary_search_by_key on every lookup added +55% to the PBF phase on
/// Denmark (20s → 31s). Linear scan wins at this size because the slice
/// fits in a cache line, key comparisons short-circuit on first byte
/// mismatch, and there's zero per-element setup cost.
pub(crate) struct Tags<'a>(pub(crate) &'a [(&'a str, &'a str)]);

impl<'a> Tags<'a> {
    pub(crate) fn get(&self, key: &str) -> Option<&'a str> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| *k == key)
    }

    pub(crate) fn has_value(&self, key: &str, val: &str) -> bool {
        self.0.iter().any(|(k, v)| *k == key && *v == val)
    }

    pub(crate) fn has_any(&self, key: &str, vals: &[&str]) -> bool {
        self.0.iter().any(|(k, v)| *k == key && vals.contains(v))
    }
}

/// Attribute value for a layer match. `Kind` carries the dictionary id of a
/// `wire_kinds!` value; it is a string to every consumer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AttrValue {
    /// Owned when built from tag text: relation matches outlive the PBF
    /// block their tags borrow from.
    Str(Cow<'static, str>),
    Int(i64),
    Bool(bool),
    Float(f64),
    Kind(crate::wire_format::KindId),
}

impl AttrValue {
    /// The string a consumer sees, for `Str` and `Kind` values.
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            Self::Kind(kind) => Some(kind.name()),
            Self::Int(_) | Self::Bool(_) | Self::Float(_) => None,
        }
    }
}

/// One attribute of a match. Fields are private: an `Attr` is built only
/// through the typed constructors below, so its value always has its key's
/// declared type and `kind` always comes from a `wire_kinds!` family.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Attr {
    key: AttrKey,
    /// The first zoom the attribute is emitted at; 0 means always.
    from_zoom: u8,
    value: AttrValue,
}

impl Attr {
    /// Emit this attribute only from zoom `z` up.
    #[must_use]
    pub(crate) fn visible_from(mut self, z: u8) -> Self {
        self.from_zoom = z;
        self
    }

    pub(crate) fn key(&self) -> AttrKey {
        self.key
    }

    pub(crate) fn value(&self) -> &AttrValue {
        &self.value
    }

    #[allow(dead_code)]
    pub(crate) fn min_zoom(&self) -> u8 {
        self.from_zoom
    }

    pub(crate) fn visible_at(&self, zoom: u8) -> bool {
        zoom >= self.from_zoom
    }

    /// A test attribute named by its key string, checked against the schema:
    /// an unknown key, a value of the wrong type or a kind missing from the
    /// dictionary panics. A `kind` string becomes the interned value.
    #[cfg(test)]
    pub(crate) fn for_test(name: &str, value: AttrValue, from_zoom: u8) -> Self {
        let key = AttrKey::from_name(name).unwrap_or_else(|| panic!("unknown key {name:?}"));
        let value = match (key, value) {
            (AttrKey::Kind, AttrValue::Str(s)) => {
                let id = crate::wire_format::KindId::for_test(&s)
                    .unwrap_or_else(|| panic!("kind {s:?} is not interned"));
                AttrValue::Kind(id)
            }
            (_, value) => value,
        };
        let declared = match &value {
            AttrValue::Str(_) | AttrValue::Kind(_) => ValueType::Str,
            AttrValue::Int(_) => ValueType::Int,
            AttrValue::Bool(_) => ValueType::Bool,
            AttrValue::Float(_) => ValueType::Float,
        };
        assert_eq!(declared, key.value_type(), "{name} value type");
        Self {
            key,
            from_zoom,
            value,
        }
    }
}

/// A finished layer match: every decision the profile makes is in it, and
/// emission reads only this. Matching yields [`Hits`], which become
/// `LayerMatch`es only through [`Hits::finish`].
/// SmallVec avoids heap allocation: most matches have 1-6 attrs (inline up to 8),
/// and most elements match 1-3 layers. Every profile layer's range ends at
/// `tiling::MAX_ZOOM`, so a match carries only where it starts.
pub(crate) struct LayerMatch {
    pub(crate) layer: Layer,
    pub(crate) min_zoom: u8,
    pub(crate) geom_expect: GeomExpect,
    /// The sort key's low byte. A bare `u8` on purpose: the only invariant
    /// it carries, that it is the rank `Layer::paint_rank_for` recovers from
    /// the match's own attributes (0 for unordered layers), is relational,
    /// and test builds assert it on every match instead.
    pub(crate) paint_rank: u8,
    pub(crate) attrs: SmallVec<[Attr; 8]>,
}
// Inline in every `Hits` and in relation memory accounting.
const _: () = assert!(std::mem::size_of::<LayerMatch>() == 280);

/// A profile rule that needs a measurement of the element's geometry.
#[derive(Clone, Copy, Debug)]
enum AreaRule {
    /// Boundary labels add `way_area` and lower their zoom by area.
    BoundaryLabel(boundaries::AdminLevel),
}

struct Hit {
    matched: LayerMatch,
    area_rule: Option<AreaRule>,
}

/// The provisional result of matching one element. Nothing here can be
/// emitted: [`Hits::finish`] applies every pending rule and yields the
/// finished matches.
pub(crate) struct Hits(SmallVec<[Hit; 4]>);

impl Hits {
    fn new() -> Self {
        Self(SmallVec::new())
    }

    /// A match with no pending rule.
    fn push(&mut self, matched: LayerMatch) {
        self.0.push(Hit {
            matched,
            area_rule: None,
        });
    }

    fn push_pending(&mut self, matched: LayerMatch, rule: AreaRule) {
        self.0.push(Hit {
            matched,
            area_rule: Some(rule),
        });
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Finish every match. `area_m2` is the element's area in square metres
    /// (projected), called at most once and only when a match needs it.
    pub(crate) fn finish(self, area_m2: impl FnOnce() -> f64) -> SmallVec<[LayerMatch; 4]> {
        let mut area_m2 = Some(area_m2);
        let mut area = None;
        self.0
            .into_iter()
            .map(|hit| {
                let mut matched = hit.matched;
                if let Some(rule) = hit.area_rule {
                    let area = *area.get_or_insert_with(|| {
                        (area_m2.take().expect("area is measured at most once"))()
                    });
                    match rule {
                        AreaRule::BoundaryLabel(level) => {
                            boundaries::finish_boundary_label(&mut matched, level, area);
                        }
                    }
                }
                #[cfg(test)]
                check_declared_keys(&matched);
                matched
            })
            .collect()
    }

    /// Finish an element that has no area (a node or an open way). Only
    /// area-accepting layers carry area rules, so none is pending.
    pub(crate) fn finish_without_area(self) -> SmallVec<[LayerMatch; 4]> {
        self.finish(|| unreachable!("an area rule matched an element without area"))
    }
}

/// Test matches built by hand carry no pending rule.
#[cfg(test)]
impl FromIterator<LayerMatch> for Hits {
    fn from_iter<I: IntoIterator<Item = LayerMatch>>(iter: I) -> Self {
        let mut hits = Self::new();
        for matched in iter {
            hits.push(matched);
        }
        hits
    }
}

impl Hits {
    /// The matches as matched, with pending rules dropped: what the profile
    /// decided before any measurement.
    #[cfg(test)]
    pub(crate) fn into_provisional(self) -> SmallVec<[LayerMatch; 4]> {
        self.0.into_iter().map(|hit| hit.matched).collect()
    }
}

// ---------------------------------------------------------------------------
// Core dispatch
// ---------------------------------------------------------------------------

/// Whether relation members can supply area geometry (including unmatched areas).
pub(crate) fn is_area_relation(tags: &Tags<'_>) -> bool {
    matches!(tags.get("type"), Some("multipolygon" | "boundary"))
}

/// The shared profile gate for storing and consuming area-relation member ways.
pub(crate) fn area_relation_matches(tags: &Tags<'_>) -> Option<Hits> {
    if !is_area_relation(tags) {
        return None;
    }
    let matches = match_element(tags, OsmGeomType::MultiPolygon, None);
    (!matches.is_empty()).then_some(matches)
}

fn layers_for_geometry(geom_type: OsmGeomType) -> &'static [Layer] {
    let (layers, count) = match geom_type {
        OsmGeomType::Node => &NODE_LAYER_SET,
        OsmGeomType::OpenWay => &OPEN_WAY_LAYER_SET,
        OsmGeomType::ClosedWay => &CLOSED_WAY_LAYER_SET,
        OsmGeomType::MultiPolygon => &MULTIPOLYGON_LAYER_SET,
    };
    &layers[..*count]
}

/// Match an OSM element against all Shortbread layers, returning every match.
/// `boundary` is what the element's parent boundary relations fold onto a
/// member way (`None` for nodes, relations and unrelated ways); the
/// boundaries layer resolves it with the way's own tags.
#[hotpath::measure]
pub(crate) fn match_element(
    tags: &Tags<'_>,
    geom_type: OsmGeomType,
    boundary: Option<boundaries::BoundaryMeta>,
) -> Hits {
    let mut out = Hits::new();
    for &layer in layers_for_geometry(geom_type) {
        match_layer(tags, layer, geom_type, boundary, &mut out);
    }
    #[cfg(test)]
    for hit in &out.0 {
        check_match(&hit.matched, geom_type);
    }
    out
}

#[cfg(test)]
fn check_declared_keys(matched: &LayerMatch) {
    for attr in &matched.attrs {
        assert!(
            matched.layer.allowed_keys().contains(&attr.key),
            "{} undeclared key {}",
            matched.layer.name(),
            attr.key.name()
        );
    }
}

/// Profile invariants every match holds, asserted on every test match.
#[cfg(test)]
fn check_match(matched: &LayerMatch, geom_type: OsmGeomType) {
    let name = matched.layer.name();
    assert!(
        matched.layer.accepts(geom_type),
        "{name} emitted for unsupported OSM geometry {geom_type:?}"
    );
    assert!(
        matched.min_zoom >= matched.layer.min_zoom(),
        "{name} emitted at z{} below its layer floor z{}",
        matched.min_zoom,
        matched.layer.min_zoom(),
    );
    check_declared_keys(matched);
    for attr in &matched.attrs {
        assert!(
            attr.from_zoom <= MAX_ZOOM,
            "{name} attribute {} starts past z{MAX_ZOOM}",
            attr.key.name()
        );
    }
    // The rank a match carries is the rank verify recovers from its
    // attributes: ordered layers through their kind methods, others 0.
    let flag = |key: AttrKey| {
        matched
            .attrs
            .iter()
            .any(|a| a.key == key && a.value == AttrValue::Bool(true))
    };
    let kind = matched
        .attrs
        .iter()
        .find(|a| a.key == AttrKey::Kind)
        .and_then(|a| a.value.as_str())
        .unwrap_or("");
    let recovered = matched
        .layer
        .paint_rank_for(&RankAttributes {
            kind,
            link: flag(AttrKey::Link),
            tunnel: flag(AttrKey::Tunnel),
            bridge: flag(AttrKey::Bridge),
        })
        .unwrap_or_else(|e| panic!("{name} rank is unrecoverable: {e:?}"));
    assert_eq!(
        matched.paint_rank,
        recovered.unwrap_or(0),
        "{name} paint rank differs from its attributes"
    );
}

fn match_layer(
    tags: &Tags<'_>,
    layer: Layer,
    geom_type: OsmGeomType,
    boundary: Option<boundaries::BoundaryMeta>,
    out: &mut Hits,
) {
    match layer {
        Layer::WaterPolygons => water::match_water_polygons(tags, out),
        Layer::WaterPolygonsLabels => water::match_water_polygons_labels(tags, out),
        Layer::WaterLines => water::match_water_lines(tags, out),
        Layer::WaterLinesLabels => water::match_water_lines_labels(tags, out),
        Layer::DamLines => water::match_dam_lines(tags, out),
        Layer::DamPolygons => water::match_dam_polygons(tags, out),
        Layer::PierLines => water::match_pier_lines(tags, out),
        Layer::PierPolygons => water::match_pier_polygons(tags, out),
        Layer::Boundaries => boundaries::match_boundaries_line(tags, boundary, out),
        Layer::BoundaryLabels => boundaries::match_boundary_labels(tags, out),
        Layer::PlaceLabels => place_labels::match_place_labels(tags, out),
        Layer::Land => match geom_type {
            OsmGeomType::OpenWay => land::match_land_lines(tags, out),
            OsmGeomType::ClosedWay => {
                land::match_land(tags, out);
                land::match_land_lines(tags, out);
            }
            OsmGeomType::MultiPolygon => land::match_land(tags, out),
            OsmGeomType::Node => unreachable!("Land does not accept nodes"),
        },
        Layer::Sites => land::match_sites(tags, out),
        Layer::Buildings => land::match_buildings(tags, out),
        Layer::Addresses => match geom_type {
            OsmGeomType::Node => land::match_addresses_point(tags, out),
            OsmGeomType::ClosedWay | OsmGeomType::MultiPolygon => {
                land::match_addresses_polygon(tags, out);
            }
            OsmGeomType::OpenWay => unreachable!("Addresses do not accept open ways"),
        },
        Layer::Streets => streets::match_streets_line(tags, out),
        Layer::StreetPolygons => streets::match_street_polygons(tags, out),
        Layer::StreetLabels => streets::match_street_labels_line(tags, out),
        Layer::StreetLabelsPoints => streets::match_street_labels_points(tags, out),
        Layer::StreetsPolygonsLabels => streets::match_streets_polygons_labels(tags, out),
        Layer::Bridges => streets::match_bridges(tags, out),
        Layer::Aerialways => transport::match_aerialways(tags, out),
        Layer::Ferries => transport::match_ferries(tags, out),
        Layer::PublicTransport => match geom_type {
            OsmGeomType::Node => transport::match_public_transport_point(tags, out),
            OsmGeomType::ClosedWay | OsmGeomType::MultiPolygon => {
                transport::match_public_transport_polygon(tags, out);
            }
            OsmGeomType::OpenWay => unreachable!("PublicTransport does not accept open ways"),
        },
        Layer::Pois => match geom_type {
            OsmGeomType::Node => pois::match_pois_point(tags, out),
            OsmGeomType::ClosedWay | OsmGeomType::MultiPolygon => {
                pois::match_pois_polygon(tags, out);
            }
            OsmGeomType::OpenWay => unreachable!("Pois do not accept open ways"),
        },
        Layer::Ocean => unreachable!("Ocean is not an OSM layer"),
    }
}

// ---------------------------------------------------------------------------
// Shared attribute helpers
// ---------------------------------------------------------------------------

// The only constructors of `Attr`. Each takes the key enum of its value
// type, and `kind` is not a `StrKey`: a kind comes only from a
// `wire_kinds!` family through `attr_kind`.

fn attr(key: impl Into<AttrKey>, value: AttrValue) -> Attr {
    Attr {
        key: key.into(),
        from_zoom: 0,
        value,
    }
}

pub(crate) fn attr_kind(kind: impl WireKind) -> Attr {
    attr(AttrKey::Kind, AttrValue::Kind(kind.kind_id()))
}

pub(crate) fn attr_str(key: StrKey, val: &'static str) -> Attr {
    attr(key, AttrValue::Str(Cow::Borrowed(val)))
}

/// A string attribute copied from tag text.
pub(crate) fn attr_dyn(key: StrKey, val: &str) -> Attr {
    attr(key, AttrValue::Str(Cow::Owned(val.to_string())))
}

pub(crate) fn attr_int(key: IntKey, val: i64) -> Attr {
    attr(key, AttrValue::Int(val))
}

pub(crate) fn attr_bool(key: BoolKey, val: bool) -> Attr {
    attr(key, AttrValue::Bool(val))
}

pub(crate) fn attr_float(key: FloatKey, val: f64) -> Attr {
    attr(key, AttrValue::Float(val))
}

fn is_tunnel(tags: &Tags<'_>) -> bool {
    tags.has_any("tunnel", &["yes", "building_passage"]) || tags.has_value("covered", "yes")
}

fn is_bridge(tags: &Tags<'_>) -> bool {
    tags.has_any(
        "bridge",
        &[
            "yes",
            "viaduct",
            "boardwalk",
            "cantilever",
            "covered",
            "low_water_crossing",
            "movable",
            "trestle",
        ],
    )
}

pub(crate) fn name_attrs(tags: &Tags<'_>) -> SmallVec<[Attr; 8]> {
    let mut attrs = SmallVec::new();
    if let Some(v) = tags.get("name")
        && !v.is_empty()
    {
        attrs.push(attr_dyn(StrKey::Name, v));
    }
    if let Some(v) = tags.get("name:en")
        && !v.is_empty()
    {
        attrs.push(attr_dyn(StrKey::NameEn, v));
    }
    if let Some(v) = tags.get("name:de")
        && !v.is_empty()
    {
        attrs.push(attr_dyn(StrKey::NameDe, v));
    }
    attrs
}

fn has_name(tags: &Tags<'_>) -> bool {
    tags.get("name").is_some_and(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Paint-order recovery
// ---------------------------------------------------------------------------

pub use land::LandKind;
pub use streets::{LinkClass, StreetClass, StreetKind};

/// The paint attributes decoded from one feature of an ordered layer.
#[derive(Clone, Copy)]
pub(crate) struct RankAttributes<'a> {
    /// The feature's `kind` string.
    pub kind: &'a str,
    pub link: bool,
    pub tunnel: bool,
    pub bridge: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RankError {
    /// The land feature's kind is not a `LandKind`.
    UnknownLandKind,
    /// The streets or street_polygons feature's kind is not a `StreetClass`.
    UnknownStreetKind,
}

impl Layer {
    /// Whether the layer has a deliberate within-layer paint order: smaller
    /// `LayerMatch::paint_rank` values are emitted first and painted below.
    pub(crate) const fn has_paint_order(self) -> bool {
        match self {
            Layer::Land | Layer::Streets | Layer::StreetPolygons => true,
            Layer::Ocean
            | Layer::WaterPolygons
            | Layer::WaterPolygonsLabels
            | Layer::WaterLines
            | Layer::WaterLinesLabels
            | Layer::DamLines
            | Layer::DamPolygons
            | Layer::PierLines
            | Layer::PierPolygons
            | Layer::Boundaries
            | Layer::BoundaryLabels
            | Layer::PlaceLabels
            | Layer::Sites
            | Layer::Buildings
            | Layer::Addresses
            | Layer::StreetLabels
            | Layer::StreetLabelsPoints
            | Layer::StreetsPolygonsLabels
            | Layer::Bridges
            | Layer::Aerialways
            | Layer::Ferries
            | Layer::PublicTransport
            | Layer::Pois => false,
        }
    }

    /// Recover the producer's paint rank from decoded attributes, through the
    /// same kind methods the matchers call. `Ok(None)` means the layer is
    /// unordered; an ordered layer with an unknown kind is an error. Missing
    /// boolean attributes are false: below their zoom floor they are absent
    /// for the whole layer, leaving class order unchanged.
    pub(crate) fn paint_rank_for(
        self,
        attrs: &RankAttributes<'_>,
    ) -> Result<Option<u8>, RankError> {
        if !self.has_paint_order() {
            return Ok(None);
        }
        if self == Layer::Land {
            let kind = LandKind::from_name(attrs.kind).ok_or(RankError::UnknownLandKind)?;
            return Ok(Some(kind.paint_rank()));
        }
        let class = StreetClass::from_name(attrs.kind).ok_or(RankError::UnknownStreetKind)?;
        Ok(Some(class.paint_rank(
            attrs.link,
            attrs.tunnel,
            attrs.bridge,
        )))
    }
}

/// Parse an integer tag using the locale policy in `osm_value::parse_int`.
fn parse_i64(s: &str) -> Option<i64> {
    osm_value::parse_int(s)
}

// ---------------------------------------------------------------------------
// Tests (see shortbread_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../shortbread_tests.rs"]
mod tests;

impl std::str::FromStr for Layer {
    type Err = String;
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::from_name(name).ok_or_else(|| format!("unknown layer name: '{name}'"))
    }
}
