use crate::geometry::overlay::port::segm::segment::{BOTH_TOP, SUBJ_TOP, SegmentFill};

/// The boolean operations the two-op engine supports. i_overlay's Clip / Union
/// / Difference / InverseDifference / Xor are pruned - the engine is only ever
/// driven at Subject (self-union) and Intersect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OverlayRule {
    Subject,
    Intersect,
}

impl OverlayRule {
    #[inline(always)]
    pub(crate) fn is_fill_top(&self, fill: SegmentFill) -> bool {
        match self {
            OverlayRule::Subject => fill & SUBJ_TOP == SUBJ_TOP,
            OverlayRule::Intersect => fill & BOTH_TOP == BOTH_TOP,
        }
    }
}
