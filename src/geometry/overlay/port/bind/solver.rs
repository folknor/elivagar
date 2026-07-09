use crate::geometry::overlay::port::bind::segment::{ContourIndex, IdSegment, IdSegments};
use crate::geometry::overlay::port::geom::v_segment::{BottomSegment, VSegment};
use crate::geometry::overlay::port::ksort::sort::key::SortKey;
use crate::geometry::overlay::port::ksort::sort::two_keys_cmp::TwoKeysAndCmpSort;
use crate::geometry::overlay::port::prim::IntPoint;
use crate::geometry::overlay::port::shape::IntPath;
use crate::geometry::overlay::port::shape::{IntContour, IntShape};
use crate::geometry::overlay::port::tree::Expiration;
use crate::geometry::overlay::port::tree::key::exp::KeyExpCollection;
use crate::geometry::overlay::port::tree::key::list::KeyExpList;
use crate::geometry::overlay::port::tree::key::tree::KeyExpTree;
use crate::geometry::overlay::port::util::log::Int;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

pub(crate) struct BinderScratch {
    pub(crate) segments: Vec<IdSegment>,
    pub(crate) parent_for_child: Vec<usize>,
    pub(crate) children_count_for_parent: Vec<usize>,
    sort_buffer: Vec<IdSegment>,
    scan_list: Option<KeyExpList<VSegment, i32, ContourIndex>>,
    scan_tree: Option<KeyExpTree<VSegment, i32, ContourIndex>>,
}

impl Default for BinderScratch {
    fn default() -> Self {
        Self {
            segments: Vec::new(),
            parent_for_child: Vec::new(),
            children_count_for_parent: Vec::new(),
            sort_buffer: Vec::new(),
            scan_list: None,
            scan_tree: None,
        }
    }
}

impl BinderScratch {
    #[inline]
    fn prepare(&mut self, children_count: usize, shape_count: usize) {
        self.parent_for_child.clear();
        #[cfg(debug_assertions)]
        self.parent_for_child.resize(children_count, usize::MAX);
        #[cfg(not(debug_assertions))]
        self.parent_for_child.resize(children_count, 0);

        self.children_count_for_parent.clear();
        self.children_count_for_parent.resize(shape_count, 0);
    }

    #[inline]
    fn take_scan_list(&mut self, capacity: usize) -> KeyExpList<VSegment, i32, ContourIndex> {
        if let Some(mut list) = self.scan_list.take() {
            list.clear();
            list.reserve_capacity(capacity);
            list
        } else {
            KeyExpList::new(capacity)
        }
    }

    #[inline]
    fn take_scan_tree(&mut self, capacity: usize) -> KeyExpTree<VSegment, i32, ContourIndex> {
        if let Some(mut tree) = self.scan_tree.take() {
            tree.clear();
            tree.reserve_capacity(capacity);
            tree
        } else {
            KeyExpTree::new(capacity)
        }
    }
}

pub(crate) struct ShapeBinder;

impl ShapeBinder {
    #[inline]
    pub(crate) fn bind(
        shape_count: usize,
        hole_segments: &[IdSegment],
        segments: &[IdSegment],
        scratch: &mut BinderScratch,
    ) {
        if shape_count < 32 {
            let capacity = segments.len().log2_sqrt().max(4) * 2;
            let mut list = scratch.take_scan_list(capacity);
            Self::private_solve::<KeyExpList<VSegment, i32, ContourIndex>>(
                &mut list,
                shape_count,
                hole_segments,
                segments,
                scratch,
            );
            scratch.scan_list = Some(list);
        } else {
            let capacity = segments.len().log2_sqrt().max(8);
            let mut tree = scratch.take_scan_tree(capacity);
            Self::private_solve::<KeyExpTree<VSegment, i32, ContourIndex>>(
                &mut tree,
                shape_count,
                hole_segments,
                segments,
                scratch,
            );
            scratch.scan_tree = Some(tree);
        }
    }

    fn private_solve<S>(
        scan_list: &mut S,
        shape_count: usize,
        anchors: &[IdSegment],
        segments: &[IdSegment],
        scratch: &mut BinderScratch,
    ) where
        S: KeyExpCollection<VSegment, i32, ContourIndex>,
    {
        let children_count = anchors.len();
        scratch.prepare(children_count, shape_count);

        let mut j = 0;

        for anchor in anchors {
            let p = anchor.v_segment.a;

            while j < segments.len() {
                let id_segment = &segments[j];
                if id_segment.cmp_by_a_then_by_angle(anchor) == Ordering::Greater {
                    break;
                }

                if id_segment.v_segment.b.x > p.x {
                    scan_list.insert(id_segment.v_segment, id_segment.contour_index, p.x);
                }
                j += 1
            }

            let target_id =
                scan_list.first_less(anchor.v_segment.a.x, ContourIndex::EMPTY, anchor.v_segment);
            let parent_index = if target_id.is_hole() {
                // index is a hole index
                // at this moment this hole parent is known
                scratch.parent_for_child[target_id.index()]
            } else {
                target_id.index()
            };

            let child_index = anchor.contour_index.index();

            scratch.parent_for_child[child_index] = parent_index;
            scratch.children_count_for_parent[parent_index] += 1;
        }
    }
}

pub(crate) trait JoinHoles {
    fn join_unsorted_holes(&mut self, holes: Vec<IntContour>, clockwise: bool);
    fn join_sorted_holes(
        &mut self,
        holes: &mut Vec<IntContour>,
        anchors: &mut Vec<IdSegment>,
        clockwise: bool,
        scratch: &mut BinderScratch,
    );
    fn scan_join(
        &mut self,
        holes: &mut Vec<IntPath>,
        hole_segments: &mut Vec<IdSegment>,
        clockwise: bool,
        scratch: &mut BinderScratch,
    );
}

impl JoinHoles for Vec<IntShape> {
    #[inline]
    fn join_unsorted_holes(&mut self, holes: Vec<IntPath>, clockwise: bool) {
        let mut holes = holes;
        if self.is_empty() || holes.is_empty() {
            return;
        }

        if self.len() == 1 {
            self[0].reserve(holes.len());
            let mut hole_paths = holes;
            self[0].append(&mut hole_paths);
            return;
        }

        let mut hole_segments: Vec<_> = holes
            .iter()
            .enumerate()
            .map(|(id, path)| IdSegment {
                contour_index: ContourIndex::new_hole(id),
                v_segment: path.left_bottom_segment(),
            })
            .collect();

        hole_segments.sort_by_a_then_by_angle();

        let mut scratch = BinderScratch::default();
        self.scan_join(&mut holes, &mut hole_segments, clockwise, &mut scratch);
    }

    #[inline]
    fn join_sorted_holes(
        &mut self,
        holes: &mut Vec<IntContour>,
        anchors: &mut Vec<IdSegment>,
        clockwise: bool,
        scratch: &mut BinderScratch,
    ) {
        if self.is_empty() || holes.is_empty() {
            holes.clear();
            anchors.clear();
            return;
        }

        if self.len() == 1 {
            self[0].append(holes);
            anchors.clear();
            return;
        }
        debug_assert!(is_sorted(&anchors));

        anchors.add_sort_by_angle();
        self.scan_join(holes, anchors, clockwise, scratch);
        anchors.clear();
    }

    fn scan_join(
        &mut self,
        holes: &mut Vec<IntPath>,
        hole_segments: &mut Vec<IdSegment>,
        clockwise: bool,
        scratch: &mut BinderScratch,
    ) {
        let x_min = hole_segments[0].v_segment.a.x;
        let x_max = hole_segments[hole_segments.len() - 1].v_segment.a.x;

        let capacity = self.iter().fold(0, |s, it| s + it[0].len()) / 2;
        scratch.segments.clear();
        scratch.segments.reserve(capacity);
        for (i, shape) in self.iter().enumerate() {
            shape[0].append_id_segments(
                &mut scratch.segments,
                ContourIndex::new_shape(i),
                x_min,
                x_max,
                clockwise,
            );
        }

        for (i, hole) in holes.iter().enumerate() {
            hole.append_id_segments(
                &mut scratch.segments,
                ContourIndex::new_hole(i),
                x_min,
                x_max,
                clockwise,
            );
        }

        scratch
            .segments
            .sort_by_a_then_by_angle_and_buffer(&mut scratch.sort_buffer);

        let segments = core::mem::take(&mut scratch.segments);
        ShapeBinder::bind(self.len(), hole_segments, &segments, scratch);
        scratch.segments = segments;

        for (shape_index, &capacity) in scratch.children_count_for_parent.iter().enumerate() {
            self[shape_index].reserve(capacity);
        }

        for (hole_index, hole) in holes.drain(..).enumerate() {
            let shape_index = scratch.parent_for_child[hole_index];
            self[shape_index].push(hole);
        }
        hole_segments.clear();
    }
}

pub(crate) trait LeftBottomSegment {
    fn left_bottom_segment(&self) -> VSegment;
    fn left_bottom_segment_from(&self, a: IntPoint) -> VSegment;
}

impl LeftBottomSegment for IntContour {
    fn left_bottom_segment(&self) -> VSegment {
        let mut a = *self.first().unwrap();
        for &p in self.iter().skip(1) {
            if p < a {
                a = p;
            }
        }

        self.left_bottom_segment_from(a)
    }

    fn left_bottom_segment_from(&self, a: IntPoint) -> VSegment {
        let n = self.len();
        let mut result: Option<VSegment> = None;

        for (i, &p) in self.iter().enumerate() {
            if p != a {
                continue;
            }

            // Self-touching contours can visit the left-bottom point several times.
            // Check every incident edge at that point and keep the lowest anchor edge.
            let b0 = self[(i + 1) % n];
            let b1 = self[(i + n - 1) % n];
            result.update_if_under(VSegment { a, b: b0 });
            result.update_if_under(VSegment { a, b: b1 });
        }

        result.unwrap_or(VSegment { a, b: a })
    }
}

#[inline]
fn is_sorted(segments: &[IdSegment]) -> bool {
    segments
        .windows(2)
        .all(|slice| slice[0].v_segment.a <= slice[1].v_segment.a)
}

impl IdSegment {
    #[inline]
    fn cmp_by_a_then_by_angle(&self, other: &Self) -> Ordering {
        self.v_segment
            .a
            .cmp(&other.v_segment.a)
            .then_with(|| self.v_segment.cmp_by_angle(&other.v_segment))
    }
}

pub(crate) trait SortByAngle {
    fn sort_by_a_then_by_angle(&mut self);
    fn sort_by_a_then_by_angle_and_buffer(&mut self, reusable_buffer: &mut Vec<IdSegment>);
    fn add_sort_by_angle(&mut self);
}

impl SortByAngle for [IdSegment] {
    #[inline]
    fn sort_by_a_then_by_angle(&mut self) {
        let mut reusable_buffer = Vec::new();
        self.sort_by_a_then_by_angle_and_buffer(&mut reusable_buffer);
    }

    #[inline]
    fn sort_by_a_then_by_angle_and_buffer(&mut self, reusable_buffer: &mut Vec<IdSegment>) {
        self.sort_by_two_keys_then_by_and_buffer(
            false,
            reusable_buffer,
            |s| s.v_segment.a.x,
            |s| s.v_segment.a.y,
            |s0, s1| s0.v_segment.cmp_by_angle(&s1.v_segment),
        );
    }

    #[inline]
    fn add_sort_by_angle(&mut self) {
        // there is a very small chance that sort is required that's why we don't use regular sort

        let mut start = 0;
        while start < self.len() {
            let a = self[start].v_segment.a;
            let mut end = start + 1;

            while end < self.len() && self[end].v_segment.a == a {
                end += 1;
            }

            if end > start + 1 {
                self[start..end].sort_by(|s0, s1| s0.v_segment.cmp_by_angle(&s1.v_segment));
            }

            start = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::geometry::overlay::port::bind::solver::JoinHoles;
    use crate::geometry::overlay::port::geom::v_segment::VSegment;
    use crate::geometry::overlay::port::prim::IntPoint;
    use alloc::vec;
    use core::cmp::Ordering;

    #[test]
    fn test_0() {
        let mut shapes = vec![
            vec![vec![
                IntPoint::new(-1, 2),
                IntPoint::new(-1, 4),
                IntPoint::new(-3, 4),
                IntPoint::new(-3, 2),
            ]],
            vec![vec![
                IntPoint::new(6, 0),
                IntPoint::new(6, 6),
                IntPoint::new(3, 6),
                IntPoint::new(2, 3),
                IntPoint::new(3, 0),
            ]],
            vec![vec![
                IntPoint::new(0, -1),
                IntPoint::new(0, -2),
                IntPoint::new(10, -2),
                IntPoint::new(10, -1),
            ]],
        ];

        let holes = vec![
            vec![
                IntPoint::new(2, 3),
                IntPoint::new(4, 4),
                IntPoint::new(4, 3),
            ],
            vec![
                IntPoint::new(2, 3),
                IntPoint::new(4, 2),
                IntPoint::new(3, 1),
            ],
        ];

        shapes.join_unsorted_holes(holes, false);

        assert_eq!(shapes[0].len(), 1);
        assert_eq!(shapes[1].len(), 3);
    }

    #[test]
    fn test_sort() {
        let s0 = VSegment {
            a: IntPoint::new(0, -2),
            b: IntPoint::new(10, -2),
        };
        let s1 = VSegment {
            a: IntPoint::new(2, 3),
            b: IntPoint::new(3, 0),
        };
        let by_a = s0.a.cmp(&s1.a);
        let long_result = match by_a {
            Ordering::Equal => s0.cmp_by_angle(&s1),
            _ => by_a,
        };

        let short_result = s0.a.cmp(&s1.b).then_with(|| s0.cmp_by_angle(&s1));

        assert_eq!(short_result, long_result);
        assert_eq!(Ordering::Less, long_result);
    }
}
