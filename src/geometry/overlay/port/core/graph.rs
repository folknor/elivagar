//! This module defines the graph structure that represents the relationships between the paths in
//! subject and clip polygons after boolean operations. The graph helps in extracting final shapes
//! based on the overlay rule applied.

use super::link::OverlayLink;
use crate::geometry::overlay::port::build::builder::GraphNode;
use crate::geometry::overlay::port::core::overlay::IntOverlayOptions;
use alloc::vec::Vec;

/// A representation of geometric shapes organized for efficient boolean operations.
///
/// `OverlayGraph` is a core structure designed to facilitate the execution of boolean operations on shapes, such as union, intersection, and difference. It organizes and preprocesses geometric data, making it optimized for these operations. This struct is the result of compiling shape data into a form where boolean operations can be applied directly, efficiently managing the complex relationships between different geometric entities.
///
/// Use `OverlayGraph` to perform boolean operations on the geometric shapes you've added to an `Overlay`, after it has processed the shapes according to the specified build and overlay rules.
/// [More information](https://ishape-rust.github.io/iShape-js/overlay/overlay_graph/overlay_graph.html) about Overlay Graph.
pub struct OverlayGraph<'a> {
    pub(crate) options: IntOverlayOptions,
    pub(crate) nodes: &'a [OverlayNode],
    pub(crate) node_indices: &'a [u32],
    pub(crate) links: &'a [OverlayLink],
}

#[derive(Clone, Copy)]
pub(crate) struct OverlayNode {
    offset: u32,
    len: u32,
}

impl GraphNode for OverlayNode {
    #[inline]
    fn with_indices(indices: &[usize], node_indices: &mut Vec<u32>) -> Self {
        let offset = node_indices.len();
        node_indices.extend(indices.iter().map(|&index| index as u32));
        Self {
            offset: offset as u32,
            len: indices.len() as u32,
        }
    }
}

impl OverlayNode {
    #[inline]
    pub(crate) fn indices<'a>(&self, node_indices: &'a [u32]) -> &'a [u32] {
        let start = self.offset as usize;
        let end = start + self.len as usize;
        unsafe { node_indices.get_unchecked(start..end) }
    }
}
