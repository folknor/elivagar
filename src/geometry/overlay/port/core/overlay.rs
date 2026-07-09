//! This module contains functionality to construct and manage overlays, which are used to perform
//! boolean operations (union, intersection, etc.) on polygons. It provides structures and methods to
//! manage subject and clip polygons and convert them into graphs for further operations.

use crate::geometry::overlay::port::build::builder::GraphBuilder;
use crate::geometry::overlay::port::core::extract::BooleanExtractionBuffer;
use crate::geometry::overlay::port::core::fill_rule::FillRule;
use crate::geometry::overlay::port::core::overlay_rule::OverlayRule;
use crate::geometry::overlay::port::core::solver::Solver;
use crate::geometry::overlay::port::prim::IntPoint;
use crate::geometry::overlay::port::segm::boolean::ShapeCountBoolean;
use crate::geometry::overlay::port::segm::build::BuildSegments;
use crate::geometry::overlay::port::segm::segment::Segment;
use crate::geometry::overlay::port::shape::{IntContour, IntShape, IntShapes};
use crate::geometry::overlay::port::split::solver::SplitSolver;
use alloc::vec::Vec;

use super::graph::OverlayNode;

/// Configuration options for polygon Boolean operations using [`Overlay`].
///
/// These options control precision, simplification, and contour filtering
/// during the Boolean operation process. You can use this to adjust output
/// direction, eliminate small artifacts, or retain collinear points.
#[derive(Debug, Clone, Copy)]
pub struct IntOverlayOptions {
    /// Preserve collinear points in the input before Boolean operations.
    pub preserve_input_collinear: bool,

    /// Desired direction for output contours (default outer: CCW / hole: CW).
    pub output_direction: ContourDirection,

    /// Preserve collinear points in the output after Boolean operations.
    pub preserve_output_collinear: bool,

    /// Minimum area threshold to include a contour in the result.
    pub min_output_area: u64,
}

/// Specifies the type of shape being processed, influencing how the shape participates in Boolean operations.
/// Note: All operations except for `Difference` are commutative, meaning the order of `Subject` and `Clip` shapes does not impact the outcome.
/// - `Subject`: The primary shape(s) for operations. Acts as the base layer in the operation.
/// - `Clip`: The modifying shape(s) that are applied to the `Subject`. Determines how the `Subject` is altered or intersected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShapeType {
    Subject,
    Clip,
}

/// Represents the winding direction of a contour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContourDirection {
    CounterClockwise,
    Clockwise,
}

/// This struct is essential for describing and uploading the geometry or shapes required to construct an `OverlayGraph`. It prepares the necessary data for boolean operations.
pub struct Overlay {
    pub solver: Solver,
    pub options: IntOverlayOptions,
    pub boolean_buffer: Option<BooleanExtractionBuffer>,
    pub(crate) segments: Vec<Segment<ShapeCountBoolean>>,
    pub(crate) split_solver: SplitSolver,
    pub(crate) graph_builder: GraphBuilder<ShapeCountBoolean, OverlayNode>,
}

impl Overlay {
    /// Constructs a new `Overlay` instance, initializing it with a capacity that should closely match the total count of edges from all shapes being processed.
    /// This pre-allocation helps in optimizing memory usage and performance.
    /// - `capacity`: The initial capacity for storing edge data. Ideally, this should be set to the sum of the edges of all shapes to be added to the overlay, ensuring efficient data management.
    /// - `options`: Adjust custom behavior.
    /// - `solver`: Type of solver to use.
    pub fn new_custom(capacity: usize, options: IntOverlayOptions, solver: Solver) -> Self {
        Self {
            solver,
            options,
            boolean_buffer: Some(Default::default()),
            segments: Vec::with_capacity(capacity),
            split_solver: SplitSolver::new(),
            graph_builder: GraphBuilder::<ShapeCountBoolean, OverlayNode>::new(),
        }
    }

    /// Adds a single path to the overlay as either subject or clip paths.
    /// - `contour`: An array of points that form a closed path.
    /// - `shape_type`: Specifies the role of the added path in the overlay operation, either as `Subject` or `Clip`.
    #[inline]
    pub fn add_contour(&mut self, contour: &[IntPoint], shape_type: ShapeType) {
        self.segments.append_path_iter(
            contour.iter().copied(),
            shape_type,
            self.options.preserve_input_collinear,
        );
    }

    /// Adds multiple paths to the overlay as either subject or clip paths.
    /// - `contours`: An array of `IntContour` instances to be added to the overlay.
    /// - `shape_type`: Specifies the role of the added paths in the overlay operation, either as `Subject` or `Clip`.
    #[inline]
    pub fn add_contours(&mut self, contours: &[IntContour], shape_type: ShapeType) {
        for contour in contours {
            self.add_contour(contour, shape_type);
        }
    }

    /// Adds a single shape to the overlay as either a subject or clip shape.
    /// - `shape`: A reference to a `IntShape` instance to be added.
    /// - `shape_type`: Specifies the role of the added shape in the overlay operation, either as `Subject` or `Clip`.
    #[inline]
    pub fn add_shape(&mut self, shape: &IntShape, shape_type: ShapeType) {
        self.add_contours(shape, shape_type);
    }

    #[inline]
    pub fn clear(&mut self) {
        self.segments.clear();
    }

    pub fn recycle_shapes(&mut self, shapes: &mut IntShapes) {
        let mut buffer = self.boolean_buffer.take().unwrap_or_default();
        buffer.recycle_shapes(shapes);
        self.boolean_buffer = Some(buffer);
    }

    pub fn recycle_shape(&mut self, shape: &mut IntShape) {
        let mut buffer = self.boolean_buffer.take().unwrap_or_default();
        buffer.recycle_shape(shape);
        self.boolean_buffer = Some(buffer);
    }

    pub(crate) fn with_boolean_buffer<R>(
        &mut self,
        f: impl FnOnce(&mut BooleanExtractionBuffer) -> R,
    ) -> R {
        let mut buffer = self.boolean_buffer.take().unwrap_or_default();
        let result = f(&mut buffer);
        self.boolean_buffer = Some(buffer);
        result
    }

    /// Executes a single Boolean operation on the current geometry using the specified overlay and build rules.
    /// This method provides a streamlined approach for performing a Boolean operation without generating
    /// an entire `OverlayGraph`. Ideal for cases where only one Boolean operation is needed, `overlay`
    /// saves on computational resources by building only the necessary links, optimizing CPU usage by 0-20%
    /// compared to a full graph-based approach.
    ///
    /// ### Parameters:
    /// - `overlay_rule`: The boolean operation rule to apply, determining how shapes are combined or subtracted.
    /// - `fill_rule`: Specifies the rule for determining filled areas within the shapes, influencing how the resulting graph represents intersections and unions.
    /// - Returns: A vector of `IntShape` that meet the specified area criteria, representing the cleaned-up geometric result.
    /// # Shape Representation
    /// The output is a `IntShapes`, where:
    /// - The outer `Vec<IntShape>` represents a set of shapes.
    /// - Each shape `Vec<IntContour>` represents a collection of contours, where the first contour is the outer boundary, and all subsequent contours are holes in this boundary.
    /// - Each path `Vec<IntPoint>` is a sequence of points, forming a closed path.
    ///
    /// Note: Outer boundary paths have a counterclockwise order, and holes have a clockwise order.
    ///
    /// Allocating convenience wrapper over `overlay_into_nested`; production
    /// goes through the `_into` path, this survives for the oracle tests.
    #[allow(dead_code)]
    #[inline]
    pub fn overlay(&mut self, overlay_rule: OverlayRule, fill_rule: FillRule) -> IntShapes {
        let mut out = Vec::new();
        self.overlay_into_nested(overlay_rule, fill_rule, &mut out);
        out
    }

    #[inline]
    pub fn overlay_into_nested(
        &mut self,
        overlay_rule: OverlayRule,
        fill_rule: FillRule,
        out: &mut IntShapes,
    ) {
        self.split_solver
            .split_segments(&mut self.segments, &self.solver);
        if self.segments.is_empty() {
            return;
        }
        let mut buffer = self.boolean_buffer.take().unwrap_or_default();
        self.graph_builder
            .build_boolean_overlay(
                fill_rule,
                overlay_rule,
                self.options,
                &self.solver,
                &self.segments,
            )
            .extract_shapes_into(overlay_rule, &mut buffer, out);
        self.boolean_buffer = Some(buffer);
    }
}

impl Default for IntOverlayOptions {
    fn default() -> Self {
        Self {
            preserve_input_collinear: false,
            output_direction: ContourDirection::CounterClockwise,
            preserve_output_collinear: false,
            min_output_area: 0u64,
        }
    }
}
