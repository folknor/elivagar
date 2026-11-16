//! Classification: turning two prepared tiles into a stream of typed diff
//! events.
//!
//! The comparison is written against a [`DiffSink`] rather than a concrete
//! result type because two consumers need the same walk with different
//! retention. The engine's `DetailOutcome` keeps counts plus a small event
//! record per difference and discards the features; the overlay renderer keeps
//! the features themselves so it can draw them. Sharing the walk is what
//! guarantees an overlay shows the diff the report counted.
//!
//! Ported verbatim from elivagar's shed `regress.rs` (commit `0129ef3~1`).

use std::cmp::Ordering;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use super::super::constants::mvt;
use super::super::eliv::DetailAttr;
use super::RegressConfig;
use super::geometry::{component_distance, component_structure_matches};
use super::pairing::{pair_detail_components, pair_detail_features};
use super::prepared::{PreparedFeature, PreparedLayer, PreparedTile};

// ---------------------------------------------------------------------------
// Event vocabulary
// ---------------------------------------------------------------------------

/// The full set of differences the diff can report. Everything except
/// `ToleranceMoved` fails a run; `ToleranceMoved` is budgeted by `--max-moved`.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum OutcomeClass {
    LayerAdded,
    LayerRemoved,
    ExtentMismatch,
    MissingFeatures,
    AddedFeatures,
    AttrChanged,
    ToleranceMoved,
    StructuralMoved,
    #[doc(hidden)]
    Count,
}

impl OutcomeClass {
    pub(crate) const COUNT: usize = Self::Count as usize;

    pub(crate) const ALL: [Self; Self::COUNT] = [
        Self::LayerAdded,
        Self::LayerRemoved,
        Self::ExtentMismatch,
        Self::MissingFeatures,
        Self::AddedFeatures,
        Self::AttrChanged,
        Self::ToleranceMoved,
        Self::StructuralMoved,
    ];

    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::LayerAdded => "layer_added",
            Self::LayerRemoved => "layer_removed",
            Self::ExtentMismatch => "extent_mismatch",
            Self::MissingFeatures => "missing_features",
            Self::AddedFeatures => "added_features",
            Self::AttrChanged => "attr_changed",
            Self::ToleranceMoved => "tolerance_moved",
            Self::StructuralMoved => "structural_moved",
            Self::Count => "<count>",
        }
    }

    pub(crate) const fn field_name(self) -> &'static str {
        match self {
            Self::LayerAdded => "layers_added",
            Self::LayerRemoved => "layers_removed",
            Self::ExtentMismatch => "extent_mismatch",
            Self::MissingFeatures => "missing_features",
            Self::AddedFeatures => "added_features",
            Self::AttrChanged => "attr_changed",
            Self::ToleranceMoved => "tolerance_moved",
            Self::StructuralMoved => "structural_moved",
            Self::Count => "<count>",
        }
    }

    pub(crate) const fn text_label(self) -> &'static str {
        match self {
            Self::LayerAdded => "added_layers",
            Self::LayerRemoved => "removed_layers",
            Self::ExtentMismatch => "extent",
            Self::MissingFeatures => "missing",
            Self::AddedFeatures => "added",
            Self::AttrChanged => "attrs",
            Self::ToleranceMoved => "tol",
            Self::StructuralMoved => "structural",
            Self::Count => "<count>",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutcomeCounts([u64; OutcomeClass::COUNT]);

impl OutcomeCounts {
    pub(crate) fn get(&self, class: OutcomeClass) -> u64 {
        self.0[class.index()]
    }

    pub(crate) fn add(&mut self, class: OutcomeClass, count: u64) {
        self.0[class.index()] += count;
    }

    pub(crate) fn any(&self) -> bool {
        OutcomeClass::ALL.iter().any(|class| self.get(*class) > 0)
    }

    pub(crate) fn is_zero(&self) -> bool {
        !self.any()
    }
}

/// Where the comparison walk sends what it finds.
///
/// `matched` is a deliberate hook rather than silence: the engine ignores an
/// exact match, but the overlay draws it in grey as the unchanged backdrop the
/// changed geometry is read against.
pub(crate) trait DiffSink {
    fn record(
        &mut self,
        layer: &Arc<str>,
        class: OutcomeClass,
        displacement: i32,
        current: Option<&PreparedFeature>,
        baseline: Option<&PreparedFeature>,
    );
    fn matched(
        &mut self,
        _layer: &Arc<str>,
        _current: &PreparedFeature,
        _baseline: &PreparedFeature,
    ) {
    }
    fn layer_event(
        &mut self,
        _class: OutcomeClass,
        _current: Option<&PreparedLayer>,
        _baseline: Option<&PreparedLayer>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Engine-side sink
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub(crate) struct DetailOutcome {
    pub(crate) counts: ContentCounts,
    pub(crate) events: Vec<OutcomeEvent>,
}

#[derive(Clone, Debug)]
pub(crate) struct OutcomeEvent {
    pub(crate) layer: Arc<str>,
    pub(crate) id: Option<u64>,
    pub(crate) class: OutcomeClass,
    pub(crate) displacement: i32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ContentCounts {
    outcomes: OutcomeCounts,
}

impl ContentCounts {
    pub(crate) fn any(&self) -> bool {
        self.outcomes.any()
    }
}

impl DiffSink for DetailOutcome {
    fn record(
        &mut self,
        layer: &Arc<str>,
        class: OutcomeClass,
        displacement: i32,
        current: Option<&PreparedFeature>,
        baseline: Option<&PreparedFeature>,
    ) {
        self.counts.outcomes.add(class, 1);
        self.events.push(OutcomeEvent {
            layer: Arc::clone(layer),
            id: current.or(baseline).and_then(|feature| feature.id),
            class,
            displacement,
        });
    }
    fn layer_event(
        &mut self,
        class: OutcomeClass,
        current: Option<&PreparedLayer>,
        baseline: Option<&PreparedLayer>,
    ) {
        let Some(layer) = current.or(baseline) else {
            return;
        };
        self.record(&layer.name, class, 0, None, None);
    }
}

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// Merge-join the two layer lists by name (both are sorted by `prepare`), then
/// compare each shared layer's contents.
///
/// An extent or version mismatch short-circuits the layer: the coordinates mean
/// different things on the two sides, so any geometry comparison under it would
/// report nonsense displacements.
pub(crate) fn compare_detail_tiles<S: DiffSink + Default>(
    current: &PreparedTile,
    baseline: &PreparedTile,
    cfg: &RegressConfig,
) -> S {
    let mut out = S::default();
    let (mut ci, mut bi) = (0usize, 0usize);
    while ci < current.layers.len() || bi < baseline.layers.len() {
        match (current.layers.get(ci), baseline.layers.get(bi)) {
            (Some(cur), Some(bl)) => match cur.name.cmp(&bl.name) {
                Ordering::Less => {
                    out.layer_event(OutcomeClass::LayerAdded, Some(cur), None);
                    ci += 1;
                }
                Ordering::Greater => {
                    out.layer_event(OutcomeClass::LayerRemoved, None, Some(bl));
                    bi += 1;
                }
                Ordering::Equal if cur.extent != bl.extent || cur.version != bl.version => {
                    out.layer_event(OutcomeClass::ExtentMismatch, Some(cur), Some(bl));
                    ci += 1;
                    bi += 1;
                }
                Ordering::Equal => {
                    compare_detail_layer(cur, bl, cfg, &mut out);
                    ci += 1;
                    bi += 1;
                }
            },
            (Some(cur), None) => {
                out.layer_event(OutcomeClass::LayerAdded, Some(cur), None);
                ci += 1;
            }
            (None, Some(bl)) => {
                out.layer_event(OutcomeClass::LayerRemoved, None, Some(bl));
                bi += 1;
            }
            (None, None) => break,
        }
    }
    out
}

/// Compare one layer present on both sides.
///
/// Features with an OSM id are matched by that id - identity is free and exact.
/// Anonymous features are bucketed by their attribute set first and matched
/// geometrically within each bucket, which keeps the matcher from ever pairing
/// two features that a renderer would style differently.
///
/// The `ocean` layer opts out of id matching entirely: its ids are synthetic
/// piece indices assigned during descent, so they are stable within a build and
/// meaningless across two.
pub(crate) fn compare_detail_layer<S: DiffSink>(
    current: &PreparedLayer,
    baseline: &PreparedLayer,
    cfg: &RegressConfig,
    out: &mut S,
) {
    let ocean = current.name.as_ref() == mvt::OCEAN_LAYER;
    let mut cur_ids: IdGroups<'_> = FxHashMap::default();
    let mut bl_ids: IdGroups<'_> = FxHashMap::default();
    let mut cur_anon = AnonymousGroups::default();
    let mut bl_anon = AnonymousGroups::default();

    for feature in &current.features {
        match feature.id {
            Some(id) if !ocean => cur_ids.entry(id).or_default().push(feature),
            _ => cur_anon.push(feature),
        }
    }
    for feature in &baseline.features {
        match feature.id {
            Some(id) if !ocean => bl_ids.entry(id).or_default().push(feature),
            _ => bl_anon.push(feature),
        }
    }

    let mut ids: Vec<u64> = cur_ids.keys().chain(bl_ids.keys()).copied().collect();
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        compare_id_group(
            &current.name,
            &cur_ids.remove(&id).unwrap_or_default(),
            &bl_ids.remove(&id).unwrap_or_default(),
            cfg,
            out,
        );
    }

    // Sorting by the attr digest makes the group visit order independent of the
    // hash map's iteration order, so the report is reproducible run to run.
    let mut groups = cur_anon.merge_with(bl_anon);
    groups.sort_by_key(|group| group.hash);
    for group in groups {
        compare_anonymous_group(&current.name, &group.current, &group.baseline, cfg, out);
    }
}

type IdGroups<'a> = FxHashMap<u64, Vec<&'a PreparedFeature>>;

/// Anonymous features grouped by attr digest (verified against the actual
/// attrs on collision). Each side builds its own instance, pushing into
/// `current`; `merge_with` then folds the other side's features into
/// `baseline`, so the field names are only meaningful after the merge.
#[derive(Default)]
struct AnonymousGroups<'a> {
    buckets: FxHashMap<u128, Vec<AnonymousGroup<'a>>>,
}

struct AnonymousGroup<'a> {
    attrs: &'a [(Arc<str>, DetailAttr)],
    current: Vec<&'a PreparedFeature>,
    baseline: Vec<&'a PreparedFeature>,
}

struct MergedAnonymousGroup<'a> {
    hash: u128,
    current: Vec<&'a PreparedFeature>,
    baseline: Vec<&'a PreparedFeature>,
}

impl<'a> AnonymousGroups<'a> {
    fn push(&mut self, feature: &'a PreparedFeature) {
        let groups = self.buckets.entry(feature.attrs_digest).or_default();
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.attrs == feature.attrs.as_slice())
        {
            group.current.push(feature);
        } else {
            groups.push(AnonymousGroup {
                attrs: &feature.attrs,
                current: vec![feature],
                baseline: Vec::new(),
            });
        }
    }

    fn merge_with(mut self, other: Self) -> Vec<MergedAnonymousGroup<'a>> {
        for (hash, groups) in other.buckets {
            let ours = self.buckets.entry(hash).or_default();
            for group in groups {
                if let Some(existing) = ours
                    .iter_mut()
                    .find(|existing| existing.attrs == group.attrs)
                {
                    existing.baseline.extend(group.current);
                    existing.baseline.extend(group.baseline);
                } else {
                    ours.push(AnonymousGroup {
                        attrs: group.attrs,
                        current: group.baseline,
                        baseline: group.current,
                    });
                }
            }
        }
        self.buckets
            .into_iter()
            .flat_map(|(hash, groups)| {
                groups.into_iter().map(move |group| MergedAnonymousGroup {
                    hash,
                    current: group.current,
                    baseline: group.baseline,
                })
            })
            .collect()
    }
}

/// Features sharing one id. Usually one on each side; a duplicated id pairs
/// positionally, which is the only order available and is stable because both
/// sides keep the producer's emission order.
fn compare_id_group<S: DiffSink>(
    layer: &Arc<str>,
    current: &[&PreparedFeature],
    baseline: &[&PreparedFeature],
    cfg: &RegressConfig,
    out: &mut S,
) {
    let pairs = current.len().min(baseline.len());
    for idx in 0..pairs {
        let cur = current[idx];
        let bl = baseline[idx];
        if cur.attrs != bl.attrs {
            out.record(layer, OutcomeClass::AttrChanged, 0, Some(cur), Some(bl));
        } else {
            classify_detail_geometry(layer, cur, bl, cfg, out);
        }
    }
    for feature in current.iter().skip(pairs) {
        out.record(layer, OutcomeClass::AddedFeatures, 0, Some(feature), None);
    }
    for feature in baseline.iter().skip(pairs) {
        out.record(layer, OutcomeClass::MissingFeatures, 0, None, Some(feature));
    }
}

fn compare_anonymous_group<S: DiffSink>(
    layer: &Arc<str>,
    current: &[&PreparedFeature],
    baseline: &[&PreparedFeature],
    cfg: &RegressConfig,
    out: &mut S,
) {
    let pairs = pair_detail_features(current, baseline);
    for (ci, bi) in pairs.paired {
        classify_detail_geometry(layer, current[ci], baseline[bi], cfg, out);
    }
    for ci in pairs.unpaired_current {
        out.record(
            layer,
            OutcomeClass::AddedFeatures,
            0,
            Some(current[ci]),
            None,
        );
    }
    for bi in pairs.unpaired_baseline {
        out.record(
            layer,
            OutcomeClass::MissingFeatures,
            0,
            None,
            Some(baseline[bi]),
        );
    }
}

/// The tolerance verdict for one matched pair.
///
/// A displacement is only ever reported when the two geometries are structurally
/// the same shape; anything else - a different geometry type, a different
/// component count, an unmatched component, a ring-role or hole-containment
/// change - is `StructuralMoved` at distance 0, because "moved by N" would be a
/// false reassurance about a change that is not a movement.
fn classify_detail_geometry<S: DiffSink>(
    layer: &Arc<str>,
    current: &PreparedFeature,
    baseline: &PreparedFeature,
    cfg: &RegressConfig,
    out: &mut S,
) {
    if current.geom_type != baseline.geom_type {
        out.record(
            layer,
            OutcomeClass::StructuralMoved,
            0,
            Some(current),
            Some(baseline),
        );
        return;
    }
    let Some(distance) = classify_detail_components(current, baseline) else {
        out.record(
            layer,
            OutcomeClass::StructuralMoved,
            0,
            Some(current),
            Some(baseline),
        );
        return;
    };
    if distance == 0 {
        out.matched(layer, current, baseline);
        return;
    }
    let class = if distance <= cfg.tol {
        OutcomeClass::ToleranceMoved
    } else {
        OutcomeClass::StructuralMoved
    };
    out.record(layer, class, distance, Some(current), Some(baseline));
}

/// `None` means "not the same shape"; `Some(d)` is the worst component
/// displacement.
fn classify_detail_components(
    current: &PreparedFeature,
    baseline: &PreparedFeature,
) -> Option<i32> {
    if current.components.len() != baseline.components.len() {
        return None;
    }
    let pairs = pair_detail_components(&current.components, &baseline.components);
    if !pairs.unpaired_current.is_empty() || !pairs.unpaired_baseline.is_empty() {
        return None;
    }
    let mut maximum = 0;
    for (ci, bi) in pairs.paired {
        let cur = &current.components[ci];
        let bl = &baseline.components[bi];
        if !component_structure_matches(cur, bl) {
            return None;
        }
        maximum = maximum.max(component_distance(cur, bl));
    }
    Some(maximum)
}
