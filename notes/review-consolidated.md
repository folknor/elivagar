# Consolidated Review: Two Independent Reviewers

Both reviewers (Opus agent + outside developer) evaluated the same ~2,070 lines
of unstaged changes. Their verdicts are remarkably aligned. Disagreements are
noted where they exist.

---

## KEEP - Both reviewers agree

| Area | Files | What |
|---|---|---|
| Road junction prepass | phase12.rs | `prepass_shared_nodes`, endpoint pinning, `annotate_global_shared_node_refs` |
| Skip-bbox for roads | simplify.rs, emit.rs | `skip_bbox_check` param, Streets/Boundaries exemption |
| Tile-space DP | geometry/tiles.rs, emit.rs | `simplify_tile_ring`, `tile_dp_recurse`, wiring at emit sites |
| Hole containment filter | geometry/tiles.rs, emit.rs, ocean.rs | `filter_holes_for_outer`, `point_in_ring`, `signed_area_2x` |
| Gap-fill → proper clip | ocean.rs | Remove fill_data/fill_ring, route gap tiles through `emit_boundary_tile` |
| Ocean-only tile skip removal | assemble.rs | Viewer bg = land, ocean-only tiles must be emitted |
| Merge cursor-reset fix | mvt/merge.rs | ClosePath resets src_cx/src_cy to last MoveTo |
| Merge regression tests | mvt/tests.rs | Two tests for the cursor-reset fix |
| SVG grid rendering + cursor fix | svg.rs | Grid rendering, layer filtering, ClosePath cursor fix |
| Diag subcommand | main.rs | MVT ring winding diagnostic tool |
| Verify improvements | verify.rs | Layer names in errors, ocean ring self-intersection checks |
| CLAUDE.md docs | CLAUDE.md | Documents diag, verify, svg changes |
| Closure-based tol_scale | simplify.rs | Signature generalization for zoom-dependent tolerance |
| Test signature updates | geometry_tests.rs | Mechanical `1.0` → `|_| 1.0` updates |
| Ring predicates | geometry/mod.rs | `ring_is_simple`, `segments_cross`, `cross_sign`, `debug_check_rings` |
| simplify_ring_dp wrapper | geometry/seams.rs | Thin wrapper for tile-space DP in ocean |

## REVERT - Both reviewers agree

| Area | Files | What | Why |
|---|---|---|---|
| Test disabling | Cargo.toml | `test = false` on lib+bin | Heat-of-moment shortcut |
| Wagyu/ocean_dissolve | ocean_dissolve.rs, lib.rs | Entire 1,776-line module + mod declaration | "Really REALLY broken" |
| Mercator/f64 geometry | geometry/mod.rs | `ring_is_simple_merc`, `split_figure8_ring_merc`, `segments_cross_f64`, etc. | "Same as before" - no effect |
| Pre-split figure8 repair | ocean.rs (~line 248) | Merc-space ring repair in pre-split loop | Failed experiment (attempt 10) |
| cleanup_tile_ring | geometry/tiles.rs, emit.rs, ocean.rs | `cleanup_tile_ring`, `cleanup_pass`, all call sites | "Fucking identical" - zero demonstrated effect |
| ring_is_simple hard-drop | emit.rs (~line 897, 1045) | Drop outer if !ring_is_simple, drop non-simple holes | Re-introduces the Nissum Bredning feature-loss bug (session section 5) |
| Disabled merge passes | assemble.rs (~line 450, 571) | Commented-out `merge_same_attr_geometries`, `merge_connected_lines` | Isolation experiment, not a fix. Cursor-reset fix makes re-enabling safe. |
| Dissolve infrastructure | assemble.rs | `dissolve_rings`/`dissolve_encode_buf` fields, `dissolve_ocean_polygons`, `cleanup_ocean_rings`, `remove_backtracks`, `remove_near_collinear`, helpers | Dead code from failed Wagyu path |
| results.db churn | .brokkr/results.db | Binary DB diff | Not source code (outside reviewer caught this) |

### Minor disagreement on REVERT

| Area | Opus | Outside | Resolution |
|---|---|---|---|
| mvt/mod.rs visibility | KEEP (needed by encode_mvt_polygon) | REVERT (only supports dead-end re-encoder) | Depends on whether encode_mvt_polygon is kept |
| encode_mvt_polygon | KEEP (useful utility) | REVERT (supports Wagyu path) | Outside reviewer is right that it was added FOR the Wagyu path. But Opus is right that it's a correct, useful utility independent of Wagyu. **Lean KEEP** - it costs nothing and the inverse encoder is handy for future decode-modify-reencode work. |
| pipeline_tests.rs | KEEP/REVERT with dissolve fields | KEEP (mechanical fixture update) | REVERT the dissolve field additions if dissolve fields are reverted from assemble.rs. Keep any other test changes. |

## UNCERTAIN - Both reviewers agree these need a decision

| Area | Files | What | Notes |
|---|---|---|---|
| split_figure8_ring (i32) | geometry/mod.rs, ocean.rs | Figure-8 ring splitting + segment_intersection | Partial success z4-7. Never solved z8+. The "keep only largest sub-hole" refinement was never confirmed. |
| nudge_coincident_hole_vertices | geometry/mod.rs, emit.rs, ocean.rs | Hole vertex nudge against outer ring | Theoretically sound, never confirmed to fix a specific bug. |
| nudge_hole_off_boundary | geometry/mod.rs, emit.rs, ocean.rs | Hole vertex nudge off clip rect edges | Same - theoretical, unconfirmed. |
| emit_boundary_tile rewrite | ocean.rs (~line 603-700) | Kitchen-sink rewrite mixing confirmed + unconfirmed | Contains filter_holes_for_outer (KEEP) but also cleanup, split, nudge (uncertain). Needs untangling. |
| Skip pre-clip DP for ocean | ocean.rs | Bypasses all Mercator-space DP for ocean polygons | Correctness improvement (avoids creating self-intersections) but unknown performance impact at planet scale. |
| debug_check_ocean_layer | assemble.rs | Stderr logging of ocean polygon defects | Useful during dev, noisy in production. Should be behind a flag or env var. |
| Hole nudge in emit.rs multi-tile | emit.rs (~line 911, 1038) | nudge calls in per-tile multipolygon path | Partial evidence only. |

---

## Recommended action plan

### Step 1: Cherry-pick the KEEPs onto a clean branch

All items in the KEEP table above. Re-enable tests. Re-enable merge passes
(safe with cursor-reset fix). This gives a solid baseline with all confirmed
improvements.

### Step 2: Build and validate the baseline

Generate Denmark PMTiles from the clean branch. Verify:
- Roads connected at all zoom levels
- No staircase artifacts at z10
- No earcut pop at Ringkøbing Fjord
- Northern Jutland visible (gap-fill fix)
- Fyn/Mors still missing at z8+ (known, unsolved)

### Step 3: Decide on the UNCERTAINs

The uncertain items fall into two groups:

**Group A - Partial ocean fix (split_figure8_ring + nudge):**
These gave z4-7 island visibility. Worth keeping IF they don't regress
anything. Test by adding them on top of the baseline and checking that z4-7
improves without z8+ getting worse.

**Group B - Ocean DP bypass:**
Skipping pre-clip DP for ocean is a correctness win (no self-intersecting
simplified coastlines) but needs performance validation at scale. Benchmark
on Denmark first.

### Step 4: Attack the real problem fresh

The investigation plan (notes/mvt-rendering-investigation-plan.md) has
approaches that were NEVER TRIED:
- Approach 0: Protobuf field reordering (3-line fix, zero risk)
- Approach 5: MVT compliance validation (vtvalidate)
- Approach 1: Drop-in encoder swap (definitive bisection)
- Approach 2: Binary diff against Planetiler reference

These are systematic diagnostic approaches vs the whack-a-mole geometry
fixes that consumed the March 8 session.
