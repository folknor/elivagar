//! Format values and defaults shared by the corpus gate, regress and
//! compare-tiles.

pub(crate) mod mvt {
    pub(crate) const FIELD_NUMBER_SHIFT: u32 = 3;
    pub(crate) const FIELD_WIRE_MASK: u64 = 0x7;

    pub(crate) const TILE_LAYER: u32 = 3;

    pub(crate) mod layer {
        pub(crate) const NAME: u32 = 1;
        pub(crate) const FEATURE: u32 = 2;
        pub(crate) const KEY: u32 = 3;
        pub(crate) const VALUE: u32 = 4;
        pub(crate) const EXTENT: u32 = 5;
        pub(crate) const VERSION: u32 = 15;
    }

    pub(crate) mod feature {
        pub(crate) const ID: u32 = 1;
        pub(crate) const TAGS: u32 = 2;
        pub(crate) const GEOMETRY_TYPE: u32 = 3;
        pub(crate) const GEOMETRY: u32 = 4;
    }

    pub(crate) const GEOMETRY_POINT: u8 = 1;
    pub(crate) const GEOMETRY_LINESTRING: u8 = 2;
    pub(crate) const GEOMETRY_POLYGON: u8 = 3;

    pub(crate) const COMMAND_MOVE_TO: u32 = 1;
    pub(crate) const COMMAND_LINE_TO: u32 = 2;
    pub(crate) const COMMAND_CLOSE_PATH: u32 = 7;
    pub(crate) const COMMAND_ID_MASK: u32 = 0x7;
    pub(crate) const COMMAND_COUNT_SHIFT: u32 = 3;

    pub(crate) const TILE_EXTENT: u32 = 4096;
    pub(crate) const DEFAULT_LAYER_VERSION: u32 = 1;
    #[cfg(test)]
    pub(crate) const FIXTURE_LAYER_VERSION: u32 = 2;
    pub(crate) const MUTATION_LAYER_VERSION: u8 = 3;
    pub(crate) const OCEAN_LAYER: &str = "ocean";

    // The value message is encoded by the test fixtures only; the decoders
    // read it through the linked elivagar crate.
    #[cfg(test)]
    pub(crate) mod value {
        pub(crate) const STRING: u32 = 1;
        pub(crate) const FLOAT: u32 = 2;
        pub(crate) const DOUBLE: u32 = 3;
    }
}

pub(crate) const SVG_TILE_SIZE: u32 = mvt::TILE_EXTENT;
pub(crate) const SVG_FALLBACK_COLOR: &str = "#ff00ff";
pub(crate) const REGRESS_OVERLAY_BACKGROUND: &str = "#f2efe9";
pub(crate) const RING_GROUP_LIMIT: usize = 500;
pub(crate) const OVERLAY_ATTRIBUTE_LIMIT: usize = 24;
pub(crate) const OVERLAY_PANEL_HEIGHT: u32 = 1_024;
pub(crate) const OVERLAY_LEGEND_Y: usize = 4_140;
pub(crate) const OVERLAY_ATTRIBUTE_START_Y: usize = 4_170;
pub(crate) const OVERLAY_LINE_HEIGHT: usize = 24;
pub(crate) const BUCKET_ZOOM: u8 = 7;
pub(crate) const MAX_MANIFEST_ZOOM: u8 = 14;

pub(crate) const DEFAULT_MAX_EXAMPLES: usize = 20;
pub(crate) const DEFAULT_OVERLAY_MAX: usize = 32;
pub(crate) const DEFAULT_COMPARE_SAMPLE_PER_ZOOM: usize = 200;
pub(crate) const MAX_COMPARE_SAMPLE_PER_ZOOM: usize = 10_000;
pub(crate) const KD_TREE_THRESHOLD: usize = 64;
pub(crate) const RESIDUAL_CANDIDATES: usize = 8;
pub(crate) const DIFF_LINE_LIMIT: usize = 100;
