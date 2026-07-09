// The ported engine keeps i_overlay's original integer narrowing/widening
// casts verbatim; their value ranges are guaranteed by the snap-rounding
// arithmetic, not visible to the lint.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub(crate) mod prim;
pub(crate) mod scan;
pub(crate) mod shape;
pub(crate) mod sort;

pub(crate) mod bind {
    pub(crate) mod segment;
    pub(crate) mod solver;
}

pub(crate) mod build {
    pub(crate) mod boolean;
    pub(crate) mod builder;
    mod graph;
    pub(crate) mod sweep;
    mod util;
}

pub(crate) mod core {
    pub mod extract;
    pub mod fill_rule;
    pub mod graph;
    pub(crate) mod link;
    pub(crate) mod nearest_vector;
    pub mod overlay;
    pub mod overlay_rule;
    pub mod simplify;
    pub mod solver;
}

pub(crate) mod geom {
    pub(crate) mod end;
    pub(crate) mod id_point;
    pub(crate) mod line_range;
    pub(crate) mod v_segment;
    pub(crate) mod x_segment;
}

pub(crate) mod segm {
    pub mod boolean;
    pub(crate) mod build;
    pub(crate) mod merge;
    pub mod segment;
    pub(crate) mod sort;
    pub(crate) mod winding;
}

pub(crate) mod split {
    pub(crate) mod solver;

    mod cross_solver;
    mod fragment;
    mod grid_layout;
    mod line_mark;
    mod snap_radius;
    mod solver_fragment;
    mod solver_list;
    mod solver_tree;
}

pub(crate) mod util {
    pub(crate) mod log;
}
