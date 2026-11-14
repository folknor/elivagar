//! Way plans: per-block membership and DP pin resolution, built inside each
//! way task before its ways are processed.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::way_index::WayId;
use pbfhogg::{Element, PrimitiveBlock};

use super::extent::CoordinateExtent;

/// Where membership information is supplied for this run.
#[derive(Clone, Copy)]
pub(super) enum MemberSource {
    /// Header declares pbfhogg.WayMembers-v1: every way block carries a bitmap.
    Injected,
    /// The relation prepass's `needed_ways` set.
    Plan,
}

/// Per-block membership input used when constructing way plans.
pub(in crate::pipeline) enum MembersForBlock<'a> {
    Bitmap(&'a [u8]),
    Set(&'a FxHashSet<i64>),
}

/// Where DP pins come from for this run.
#[derive(Clone, Copy)]
pub(in crate::pipeline) enum PinSource {
    /// Header declares pbfhogg.SharedNodePins-v1: read Way field 20.
    Injected,
    /// Count shared refs within each primitive block.
    BlockLocal,
}

pub(super) fn member_bit(bitmap: &[u8], i: usize) -> bool {
    let byte = bitmap
        .get(i / 8)
        .expect("validated way-members bitmap shorter than decoded way count");
    byte & (1 << (i % 8)) != 0
}

pub(super) fn fill_mask_from_pin_bitmap(bitmap: &[u8], mask: &mut [bool]) {
    for (i, pinned) in mask.iter_mut().enumerate() {
        *pinned = bitmap[i / 8] & (1 << (i % 8)) != 0;
    }
}

pub(in crate::pipeline) struct WayPlan {
    pub(in crate::pipeline) way_id: WayId,
    pub(in crate::pipeline) node_refs: Vec<i64>,
    pub(in crate::pipeline) preserve_node_refs: Vec<i64>,
    /// Whether this way is a member of a shortbread-matched multipolygon/
    /// boundary relation, resolved once at plan build from the bitmap or the
    /// relation plan's `needed_ways` set.
    pub(in crate::pipeline) is_member: bool,
}

pub(super) fn estimate_way_plans_bytes(plans: &[WayPlan]) -> usize {
    plans
        .iter()
        .map(|p| {
            std::mem::size_of::<WayPlan>() + p.node_refs.len() * 8 + p.preserve_node_refs.len() * 8
        })
        .sum()
}

pub(in crate::pipeline) fn build_way_plans(
    block: &PrimitiveBlock,
    members: &MembersForBlock<'_>,
    pins: PinSource,
    locations_on_ways: bool,
) -> (Vec<WayPlan>, u64, CoordinateExtent) {
    let mut way_pos = 0usize;
    let mut marked = 0u64;
    let mut location_extent = CoordinateExtent::default();
    let mut plans: Vec<WayPlan> = block
        .elements()
        .filter_map(|element| {
            let Element::Way(way) = element else {
                return None;
            };
            // Emit a plan for EVERY way element (even one with zero node refs).
            // The task loop zips this list positionally against the block's
            // ways; dropping any way here would shift every following plan
            // onto the wrong way. Empty-ref ways resolve to empty coords and
            // are dropped inside `process_planned_way_into`.
            let way_id = way.id();
            let is_member = match members {
                MembersForBlock::Bitmap(bitmap) => member_bit(bitmap, way_pos),
                MembersForBlock::Set(needed_ways) => needed_ways.contains(&way_id),
            };
            // Enriched PBFs may omit untagged node elements, so the inline way
            // coordinates must also bound the extract independently of tags.
            if locations_on_ways {
                for location in way.node_locations() {
                    location_extent.include(location.decimicro_lat(), location.decimicro_lon());
                }
            }
            way_pos += 1;
            marked += u64::from(is_member);
            Some(WayPlan {
                way_id: WayId::new(way_id),
                node_refs: way.refs().collect(),
                preserve_node_refs: Vec::new(),
                is_member,
            })
        })
        .collect();

    if matches!(pins, PinSource::BlockLocal) {
        let counts = shared_node_counts(plans.iter().map(|p| p.node_refs.as_slice()));
        for plan in &mut plans {
            plan.preserve_node_refs = preserve_refs_for_way(&plan.node_refs, &counts);
        }
    }
    (plans, marked, location_extent)
}

/// The node refs of a way that participate in shared-junction detection.
///
/// Ways with <= 2 refs never contribute or receive a pin. For a closed ring the
/// duplicated closing vertex is dropped (it is the same node as the first). For
/// an open line ALL nodes count, endpoints included - shared endpoints are where
/// ways connect and must be pinned so DP simplification does not move them to
/// different positions in each way (which creates visible gaps at junctions).
fn shared_scan_slice(node_refs: &[i64]) -> &[i64] {
    if node_refs.len() <= 2 {
        return &[];
    }
    let is_closed = node_refs.len() >= 4 && node_refs.first() == node_refs.last();
    if is_closed {
        &node_refs[..node_refs.len() - 1]
    } else {
        node_refs
    }
}

/// Count block-local node-ref occurrences across every way's scan slice.
fn shared_node_counts<'a>(ways: impl Iterator<Item = &'a [i64]>) -> FxHashMap<i64, u8> {
    let mut counts: FxHashMap<i64, u8> = FxHashMap::default();
    for refs in ways {
        for &node_id in shared_scan_slice(refs) {
            counts
                .entry(node_id)
                .and_modify(|c| *c = c.saturating_add(1))
                .or_insert(1);
        }
    }
    counts
}

/// Compute the preserve set for one way from block-local shared refs.
fn preserve_refs_for_way(node_refs: &[i64], counts: &FxHashMap<i64, u8>) -> Vec<i64> {
    let mut preserve: Vec<i64> = Vec::new();
    for &node_id in shared_scan_slice(node_refs) {
        let shared = counts.get(&node_id).is_some_and(|&c| c >= 2);
        if shared && !preserve.contains(&node_id) {
            preserve.push(node_id);
        }
    }
    preserve
}

/// Legacy raw way fixture used by phase12 unit tests.
#[cfg(test)]
#[allow(dead_code)]
pub(in crate::pipeline) struct RawWay {
    pub(in crate::pipeline) way_id: i64,
    pub(in crate::pipeline) node_refs: Vec<i64>,
    pub(in crate::pipeline) preserve_node_refs: Vec<i64>,
    pub(in crate::pipeline) coords_e7: Vec<(i32, i32)>,
    pub(in crate::pipeline) tags: Vec<(String, String)>,
}
#[cfg(test)]
const _: () = assert!(std::mem::size_of::<RawWay>() == 104);

/// Mark interior node refs that are shared by at least 2 ways in the same block.
///
/// This preserves common junction vertices during DP simplification without global
/// topology indexing. Block-local detection catches most local road intersections
/// because OSM PBF primitive blocks are spatially clustered.
/// Limitation: cross-block shared nodes are intentionally not detected here.
///
/// Delegates to the same `shared_node_counts` / `preserve_refs_for_way` helpers
/// as the production `build_way_plans`, so this test wrapper cannot drift from
/// the code path the pipeline actually runs.
#[cfg(test)]
pub(in crate::pipeline) fn annotate_block_shared_node_refs(raw_ways: &mut [RawWay]) {
    let counts = shared_node_counts(raw_ways.iter().map(|w| w.node_refs.as_slice()));
    for w in raw_ways.iter_mut() {
        w.preserve_node_refs = preserve_refs_for_way(&w.node_refs, &counts);
    }
}

#[cfg(test)]
mod tests {
    use super::{fill_mask_from_pin_bitmap, member_bit};

    #[test]
    fn member_bit_is_lsb_first() {
        let bitmap = [0b0000_0101, 0b0000_0001];
        assert!(member_bit(&bitmap, 0));
        assert!(!member_bit(&bitmap, 1));
        assert!(member_bit(&bitmap, 2));
        assert!(!member_bit(&bitmap, 3));
        assert!(member_bit(&bitmap, 8));
    }

    #[test]
    fn pin_bitmap_mask_is_lsb_first_across_bytes() {
        let mut mask = vec![false; 10];
        fill_mask_from_pin_bitmap(&[0b0000_0101, 0b0000_0010], &mut mask);
        assert_eq!(
            mask,
            [
                true, false, true, false, false, false, false, false, false, true
            ]
        );

        let mut zeroes = vec![true; 5];
        fill_mask_from_pin_bitmap(&[0], &mut zeroes);
        assert_eq!(zeroes, [false; 5]);
    }
}
