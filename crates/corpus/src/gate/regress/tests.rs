//! Regress engine tests, ported from elivagar's `regress/tests.rs`
//! (commit `0129ef3~1`).
//!
//! Two halves. The matcher tests below drive `remaining_pairs` and
//! `sparse_min_cost_pairs` over synthetic cost matrices - including a brute-force
//! oracle, because a min-cost matching is exactly the kind of code that produces
//! a plausible-looking wrong answer. The engine tests build real PMTiles
//! archives and assert the report the full three-pass diff produces.
//!
//! The equivalence half of the original file (streaming hash vs detail hash) is
//! not here: it landed with the canonical hash and lives in
//! `corpus::canonical`'s `equivalence` module.

use super::super::constants::mvt;
use super::super::corpus::fixture::{
    TestDir, Value, anonymous_ocean_tile, duplicate_id_tile, empty_layer_tile, float_attr_tile,
    line_tile, multiline_tile, polygon_tile, write_archive,
};
use super::super::eliv::Strictness;
use super::compare::OutcomeClass;
use super::engine::regress;
use super::pairing::{remaining_pairs, sparse_min_cost_pairs};
use super::report::RegressConfig;

/// The engine under test always decodes strict here: every fixture is built by
/// brokkr's own encoder, so an unknown field would be a bug in the test.
fn diff(
    current: &std::path::Path,
    baseline: &std::path::Path,
    cfg: &RegressConfig,
) -> super::report::RegressReport {
    regress(current, baseline, cfg, Strictness::Strict).expect("regress")
}

// ---------------------------------------------------------------------------
// Residual matcher
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ResidualMatchPoint(usize);

/// A cost matrix where greedy loses: taking the cheapest edge (0,0)=1 forces
/// (1,1)=100, while the optimum crosses over for 2+2.
fn crossing_residual_cost(ci: usize, bi: usize) -> i32 {
    match (ci, bi) {
        (0, 0) => 1,
        (0, 1) | (1, 0) => 2,
        (1, 1) => 100,
        (ci, bi) if ci == bi => 0,
        _ => 1_000,
    }
}

fn crossing_residual_pairs() -> Vec<(usize, usize)> {
    let current: Vec<_> = (0..9).map(ResidualMatchPoint).collect();
    let baseline: Vec<_> = (0..9).map(ResidualMatchPoint).collect();
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; baseline.len()];
    remaining_pairs(
        &current,
        &baseline,
        &mut cur_used,
        &mut bl_used,
        |_| (),
        |_, _| 0,
        |left, right| {
            u64::try_from(crossing_residual_cost(left.0, right.0)).expect("non-negative proxy cost")
        },
        |left, right| crossing_residual_cost(left.0, right.0),
    )
}

#[test]
fn residual_matcher_uses_minimum_cost_assignment_over_greedy_crossing() {
    let pairs = crossing_residual_pairs();
    assert_eq!(
        pairs,
        vec![
            (0, 1),
            (1, 0),
            (2, 2),
            (3, 3),
            (4, 4),
            (5, 5),
            (6, 6),
            (7, 7),
            (8, 8)
        ]
    );
    let total: i32 = pairs
        .iter()
        .map(|&(ci, bi)| crossing_residual_cost(ci, bi))
        .sum();
    assert_eq!(total, 4);
}

#[test]
fn residual_matcher_is_deterministic() {
    let expected = crossing_residual_pairs();
    for _ in 0..16 {
        assert_eq!(crossing_residual_pairs(), expected);
    }
}

// Exhaustive min-cost max-cardinality reference: try every assignment.
fn brute_force_best(costs: &[Vec<Option<i32>>]) -> (usize, i64) {
    fn recurse(
        costs: &[Vec<Option<i32>>],
        ci: usize,
        used: &mut [bool],
        matched: usize,
        cost: i64,
        best: &mut (usize, i64),
    ) {
        if ci == costs.len() {
            if matched > best.0 || (matched == best.0 && cost < best.1) {
                *best = (matched, cost);
            }
            return;
        }
        recurse(costs, ci + 1, used, matched, cost, best);
        for (bi, slot) in costs[ci].iter().enumerate() {
            if let Some(edge) = slot
                && !used[bi]
            {
                used[bi] = true;
                recurse(
                    costs,
                    ci + 1,
                    used,
                    matched + 1,
                    cost + i64::from(*edge),
                    best,
                );
                used[bi] = false;
            }
        }
    }
    let width = costs.first().map_or(0, Vec::len);
    let mut best = (0, i64::MAX);
    recurse(costs, 0, &mut vec![false; width], 0, 0, &mut best);
    if best.0 == 0 {
        best.1 = 0;
    }
    best
}

fn sparse_pairs_for(costs: &[Vec<Option<i32>>]) -> Vec<(usize, usize)> {
    let width = costs.first().map_or(0, Vec::len);
    let current: Vec<_> = (0..costs.len()).map(ResidualMatchPoint).collect();
    let baseline: Vec<_> = (0..width).map(ResidualMatchPoint).collect();
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; baseline.len()];
    let mut candidates = Vec::new();
    for (ci, row) in costs.iter().enumerate() {
        for (bi, slot) in row.iter().enumerate() {
            if slot.is_some() {
                candidates.push((ci, bi));
            }
        }
    }
    sparse_min_cost_pairs(
        &current,
        &baseline,
        &mut cur_used,
        &mut bl_used,
        &candidates,
        &|l, r| costs[l.0][r.0].expect("distance is only asked for candidate edges"),
    )
}

#[test]
fn sparse_matcher_matches_brute_force_oracle() {
    fn dense(rows: &[&[i32]]) -> Vec<Vec<Option<i32>>> {
        rows.iter()
            .map(|row| row.iter().map(|&cost| Some(cost)).collect())
            .collect()
    }
    let cases: Vec<Vec<Vec<Option<i32>>>> = vec![
        // Review counterexample: equal-cost alternating structure that broke
        // the tie-relaxing Bellman-Ford (predecessor cycle, endless augment).
        dense(&[&[1, 1, 3, 1], &[0, 0, 2, 3], &[2, 2, 4, 3], &[2, 4, 4, 2]]),
        // All-zero ties: any perfect matching, but it must terminate and
        // stay maximum-cardinality.
        dense(&[&[0, 0, 0], &[0, 0, 0], &[0, 0, 0]]),
        // Crossing: greedy takes 1 then 100; optimum is 2 + 2.
        dense(&[&[1, 2], &[2, 100]]),
        // Rectangular with ties on every row.
        dense(&[&[0, 0, 1], &[0, 1, 0]]),
        // Sparse edges force cardinality-first choices.
        vec![
            vec![Some(5), None, None],
            vec![Some(1), Some(1), None],
            vec![None, Some(0), Some(9)],
        ],
        // Zero-cost alternatives: two ways around at equal cost.
        dense(&[&[0, 1, 0], &[1, 0, 0], &[0, 0, 1]]),
    ];
    for costs in cases {
        let pairs = sparse_pairs_for(&costs);
        let (cardinality, best_cost) = brute_force_best(&costs);
        assert_eq!(pairs.len(), cardinality, "cardinality for {costs:?}");
        let total: i64 = pairs
            .iter()
            .map(|&(ci, bi)| i64::from(costs[ci][bi].expect("paired edge exists")))
            .sum();
        assert_eq!(total, best_cost, "cost for {costs:?}");
    }
}

#[derive(Clone, Copy)]
struct StarvedPoint {
    key: u8,
    cluster: u8,
}

#[test]
fn residual_matcher_exhausts_same_key_pairs_before_force_zip() {
    fn push(list: &mut Vec<StarvedPoint>, key: u8, cluster: u8, n: usize) {
        for _ in 0..n {
            list.push(StarvedPoint { key, cluster });
        }
    }
    // Each key holds a 9-current/8-baseline cluster and an 8-current/9-baseline
    // cluster: the K=8 candidate graph cannot bridge the clusters, so
    // min-cost matching strands one current and one baseline per key. Baseline
    // key order is reversed so a key-blind force-zip would pair the
    // leftovers across keys; the same-key completion sweep must not.
    let mut current = Vec::new();
    let mut baseline = Vec::new();
    push(&mut current, 0, 1, 9);
    push(&mut current, 0, 2, 8);
    push(&mut current, 1, 1, 9);
    push(&mut current, 1, 2, 8);
    push(&mut baseline, 1, 1, 8);
    push(&mut baseline, 1, 2, 9);
    push(&mut baseline, 0, 1, 8);
    push(&mut baseline, 0, 2, 9);
    let mut cur_used = vec![false; current.len()];
    let mut bl_used = vec![false; baseline.len()];
    let cluster_cost = |l: &StarvedPoint, r: &StarvedPoint| -> u16 {
        if l.cluster == r.cluster { 1 } else { 1000 }
    };
    let paired = remaining_pairs(
        &current,
        &baseline,
        &mut cur_used,
        &mut bl_used,
        |point| point.key,
        |_, _| 0,
        |l, r| u64::from(cluster_cost(l, r)),
        |l, r| i32::from(cluster_cost(l, r)),
    );
    assert_eq!(paired.len(), 34);
    for (ci, bi) in paired {
        assert_eq!(
            current[ci].key, baseline[bi].key,
            "pair {ci} {bi} crosses keys"
        );
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

#[test]
fn identical_archive_report_passes() {
    let dir = TestDir::new("identical");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(&current, vec![(0, 0, 0, tile.clone())]);
    write_archive(&baseline, vec![(0, 0, 0, tile)]);
    let cfg = RegressConfig::default();
    let report = diff(&current, &baseline, &cfg);
    assert!(report.passed(&cfg));
    assert_eq!(report.identical_tiles, 1);
}

#[test]
fn one_tile_removed_reports_only_in_baseline() {
    let dir = TestDir::new("removed");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(&current, vec![(1, 0, 0, tile.clone())]);
    write_archive(&baseline, vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile)]);
    let cfg = RegressConfig::default();
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.totals.only_in_baseline, 1);
    assert!(!report.passed(&cfg));
}

#[test]
fn moved_vertex_respects_tolerance() {
    let dir = TestDir::new("tolerance");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    write_archive(
        &current,
        vec![(0, 0, 0, line_tile(Some(1), "a", &[(0, 0), (13, 10)]))],
    );
    write_archive(
        &baseline,
        vec![(0, 0, 0, line_tile(Some(1), "a", &[(0, 0), (10, 10)]))],
    );

    let cfg = RegressConfig {
        tol: 4,
        max_moved: 1,
        max_examples: 20,
    };
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.totals.count(OutcomeClass::ToleranceMoved), 1);
    assert!(report.passed(&cfg));

    let cfg = RegressConfig {
        tol: 2,
        max_moved: 1,
        max_examples: 20,
    };
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.totals.count(OutcomeClass::StructuralMoved), 1);
    assert!(!report.passed(&cfg));
}

#[test]
fn attr_change_reports_attr_changed() {
    let dir = TestDir::new("attr");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    write_archive(
        &current,
        vec![(0, 0, 0, line_tile(Some(1), "b", &[(0, 0), (10, 10)]))],
    );
    write_archive(
        &baseline,
        vec![(0, 0, 0, line_tile(Some(1), "a", &[(0, 0), (10, 10)]))],
    );
    let cfg = RegressConfig::default();
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.totals.count(OutcomeClass::AttrChanged), 1);
}

#[test]
fn layer_present_empty_on_one_side_reports_removed() {
    let dir = TestDir::new("empty-layer");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    write_archive(&current, vec![(0, 0, 0, Vec::new())]);
    write_archive(
        &baseline,
        vec![(
            0,
            0,
            0,
            empty_layer_tile("empty", u64::from(mvt::TILE_EXTENT)),
        )],
    );
    let cfg = RegressConfig::default();
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.totals.count(OutcomeClass::LayerRemoved), 1);
}

#[test]
fn extent_mismatch_skips_geometry() {
    let dir = TestDir::new("extent");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    write_archive(&current, vec![(0, 0, 0, empty_layer_tile("roads", 8192))]);
    write_archive(
        &baseline,
        vec![(
            0,
            0,
            0,
            empty_layer_tile("roads", u64::from(mvt::TILE_EXTENT)),
        )],
    );
    let cfg = RegressConfig::default();
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.totals.count(OutcomeClass::ExtentMismatch), 1);
    assert_eq!(report.totals.count(OutcomeClass::StructuralMoved), 0);
}

#[test]
fn polygon_hole_reassigned_at_zero_distance_is_structural() {
    let dir = TestDir::new("hole");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let outer_a = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
    let hole_a = &[(20, 20), (20, 40), (40, 40), (40, 20), (20, 20)][..];
    let outer_b = &[(200, 200), (300, 200), (300, 300), (200, 300), (200, 200)][..];
    write_archive(
        &current,
        vec![(0, 0, 0, polygon_tile(&[outer_a, outer_b, hole_a]))],
    );
    write_archive(
        &baseline,
        vec![(0, 0, 0, polygon_tile(&[outer_a, hole_a, outer_b]))],
    );
    let cfg = RegressConfig {
        tol: 10,
        max_moved: 10,
        max_examples: 20,
    };
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.totals.count(OutcomeClass::StructuralMoved), 1);
    assert_eq!(report.totals.count(OutcomeClass::ToleranceMoved), 0);
}

#[test]
fn polygon_hole_escaping_its_outer_is_structural_not_tolerance() {
    // Equal ring counts, equal roles, displacement 2px under tol 3: only the
    // hole-containment predicate can classify this pair as structural. The
    // hole's first vertex crosses the outer boundary (99 -> 101 with the
    // outer ending at x=100), so containment differs while every other
    // structural signal matches; a tolerance verdict would mean the
    // containment branch was skipped.
    let dir = TestDir::new("hole-containment");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let outer = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
    let hole_inside = &[(99, 50), (89, 50), (89, 60), (99, 60), (99, 50)][..];
    let hole_escaped = &[(101, 50), (91, 50), (91, 60), (101, 60), (101, 50)][..];
    write_archive(
        &current,
        vec![(0, 0, 0, polygon_tile(&[outer, hole_inside]))],
    );
    write_archive(
        &baseline,
        vec![(0, 0, 0, polygon_tile(&[outer, hole_escaped]))],
    );
    let cfg = RegressConfig {
        tol: 3,
        max_moved: 10,
        max_examples: 20,
    };
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.totals.count(OutcomeClass::StructuralMoved), 1);
    assert_eq!(report.totals.count(OutcomeClass::ToleranceMoved), 0);
}

#[test]
fn run_length_directory_preserves_each_addressed_tile() {
    let dir = TestDir::new("run-length");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(
        &current,
        vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile.clone())],
    );
    write_archive(&baseline, vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile)]);
    let view = super::super::eliv::ArchiveView::open(&current).expect("open current");
    let runs = view.read_all_runs().expect("read runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_length, 2);

    let cfg = RegressConfig::default();
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.identical_tiles, 2);
}

#[test]
fn deduplicated_run_collapses_to_one_raw_pair() {
    let dir = TestDir::new("dedup");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let tile = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(
        &current,
        vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile.clone())],
    );
    write_archive(&baseline, vec![(1, 0, 0, tile.clone()), (1, 0, 1, tile)]);
    let cfg = RegressConfig::default();
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.counters.unique_blob_pairs, 1);
    assert_eq!(report.counters.raw_equal_pairs, 1);
    assert_eq!(report.counters.raw_equal_tiles, 2);
    assert_eq!(report.identical_tiles, 2);
}

#[test]
fn detailed_pair_multiplicity_is_reported_per_addressed_tile() {
    let dir = TestDir::new("detailed-pair-multiplicity");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let moved = line_tile(Some(1), "a", &[(0, 0), (13, 10)]);
    let original = line_tile(Some(1), "a", &[(0, 0), (10, 10)]);
    write_archive(&current, vec![(1, 0, 0, moved.clone()), (1, 0, 1, moved)]);
    write_archive(
        &baseline,
        vec![(1, 0, 0, original.clone()), (1, 0, 1, original)],
    );
    let cfg = RegressConfig {
        tol: 4,
        max_moved: 2,
        max_examples: 20,
    };
    let report = diff(&current, &baseline, &cfg);
    assert_eq!(report.counters.unique_blob_pairs, 1);
    assert_eq!(report.counters.detailed_pairs, 1);
    assert_eq!(report.counters.detailed_tiles, 2);
    assert_eq!(report.totals.count(OutcomeClass::ToleranceMoved), 2);
}

#[test]
fn canonical_edge_cases_have_expected_live_engine_outcomes() {
    let dir = TestDir::new("differential-edge-cases");
    let current = dir.path.join("current.pmtiles");
    let baseline = dir.path.join("baseline.pmtiles");
    let p1 = &[(0, 0), (10, 10)][..];
    let p2 = &[(20, 20), (30, 30)][..];
    let outer_a = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
    let hole_a = &[(20, 20), (20, 40), (40, 40), (40, 20), (20, 20)][..];
    let outer_b = &[(200, 200), (300, 200), (300, 300), (200, 300), (200, 200)][..];
    let ocean_a = &[(0, 0), (80, 0), (80, 80), (0, 80), (0, 0)][..];
    let ocean_b = &[(2, 0), (82, 0), (82, 80), (2, 80), (2, 0)][..];
    write_archive(
        &current,
        vec![
            (2, 0, 0, multiline_tile(&[p1, p2])),
            (
                2,
                0,
                1,
                float_attr_tile(Value::Float(f32::from_bits(0x7fc0_0001))),
            ),
            (2, 1, 0, duplicate_id_tile(&[p1, p2])),
            (2, 1, 1, anonymous_ocean_tile(&[ocean_a])),
            (2, 2, 0, polygon_tile(&[outer_a, outer_b, hole_a])),
        ],
    );
    write_archive(
        &baseline,
        vec![
            (2, 0, 0, multiline_tile(&[p2, p1])),
            (
                2,
                0,
                1,
                float_attr_tile(Value::Float(f32::from_bits(0x7fc0_0002))),
            ),
            (2, 1, 0, duplicate_id_tile(&[p2, p1])),
            (2, 1, 1, anonymous_ocean_tile(&[ocean_b])),
            (2, 2, 0, polygon_tile(&[outer_a, hole_a, outer_b])),
        ],
    );
    let cfg = RegressConfig {
        tol: 3,
        max_moved: 10,
        max_examples: 20,
    };
    let report = diff(&current, &baseline, &cfg);
    // Identical: the permuted multiline and the permuted duplicate-id tile.
    // attr_changed: the bit-distinct NaN floats. tolerance_moved: the ocean
    // polygon shifted 2px under tol 3. structural_moved: moving hole_a after
    // outer_b reattaches it to a different outer (MVT holes bind to the
    // preceding outer ring), so hole containment differs between archives.
    assert_eq!(report.identical_tiles, 2);
    assert_eq!(report.diff_count, 3);
    assert_eq!(report.totals.only_in_current, 0);
    assert_eq!(report.totals.only_in_baseline, 0);
    assert_eq!(report.totals.count(OutcomeClass::AttrChanged), 1);
    assert_eq!(report.totals.count(OutcomeClass::ToleranceMoved), 1);
    assert_eq!(report.totals.count(OutcomeClass::StructuralMoved), 1);
}
