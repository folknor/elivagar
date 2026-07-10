# Extract and inline the i_overlay boolean engine

Status: spec, not yet implemented. Written 2026-07-09.

Written against `reference/technical-implementation-spec.md` (the contract
this document must satisfy). Spawned from the H6 OPEN DOOR paragraph in
`notes/planet-30gb-roadmap.md` (user, 2026-07-09): i_overlay is not a fixed
dependency; the two ops we use can be extracted from the crate, inlined,
and optimized for our shapes - caller-provided output buffers, reused
internal graph storage, integer-only paths. Measurement record:
`reference/performance.md` plus `.brokkr/results.db`.

## 1. The prize

Alloc profile `e1afc38d` (commit `d20ddd5`, plantasjen, denmark locations,
`brokkr tilegen --alloc --dataset denmark --variant locations`):

| function | calls | exclusive alloc | thread-time |
|---|---|---|---|
| `int_ocean::intersect_rect_into` | 6,550 | 8.1 GB (avg 1.3 MB) | 16.6 s |
| `int_ocean::normalize_into` | 11,408,510 | 7.0 GB (avg 658 B) | 24.4 s |
| combined | | 15.1 GB (31.6% of tracked) | 41.0 s |

(Thread-time from the alloc-mode timing table: directional ranking only,
per the `reference/performance.md` reading rules - alloc mode runs without
mimalloc and inflates wall. The pre-L1 hotpath baseline brick below
captures verdict-grade rankings.)

Both functions are thin elivagar wrappers whose bodies are i_overlay
calls; the exclusive allocation attributed to them is almost entirely
library-internal (i_overlay frames are not instrumented, so its
allocations land in the elivagar caller frame). This is the churn the H6
caller-side scratch work structurally cannot reach - the roadmap's
"library-owned, not tractable from here" line, now made tractable by
owning the code. Collateral: `ocean::emit_ocean_piece` (8.1 GB exclusive)
holds pyramid fragment `Shapes` whose inner ring Vecs currently die on
every `return_shapes` (which only recycles the outer Vec); the ring pool
in Landing 2 recycles those too.

Current clean bench references (plantasjen): denmark 12.9 s (`811f8222`,
commit `72b7c25`); norway 45.5 s (`b946e82b`, `72b7c25`); germany 50.2 s
(`39d085b8`, `d20ddd5`). Fresh baselines are re-taken at the parent commit
of each landing (brick 1.0) - these rows only size the noise floor.

## 2. Survey of the ground

### 2.1 Production call surface (complete)

i_overlay appears in production code ONLY inside
`src/geometry/int_ocean.rs`:

- `IntEmitScratch.overlay: Overlay<i32>` - persistent engine instance,
  one per scratch, reused across calls (`Overlay::new_custom(0,
  overlay_options(0), Default::default())`).
- `normalize_into(scratch, shape, min_area, out)`:
  - single-contour shape: `overlay.simplify_contour(&shape[0],
    FillRule::NonZero)` - `None` means "already perfect", elivagar then
    runs its own `clean_shape_in_place`;
  - multi-contour shape: `overlay.clear()` + `add_shape(Subject)` +
    `overlay(OverlayRule::Subject, FillRule::NonZero)`.
- `intersect_rect_into(scratch, shape, rect, min_area, out)`:
  `overlay.clear()` + `add_shape(Subject)` + `add_contour(rect_contour,
  Clip)` + `overlay(OverlayRule::Intersect, FillRule::NonZero)`.
- Options: always `ContourDirection::CounterClockwise`,
  `min_output_area: u64` (varies per call), everything else default
  (`preserve_input_collinear: false`, `preserve_output_collinear: false`,
  `ogc: false`). Solver always `Default` (= `Solver::AUTO`, precision
  HIGH). Only `i32` coordinates. Note that `is_parallel_sort_allowed()`
  returns `true` at runtime - it is `self.multithreading.is_some()`
  (`core/solver.rs:192-194`), and `multithreading` is `Some` in every
  preset including `Default`/`AUTO`. The parallel paths are dead not
  because that flag is false but because the `allow_multithreading` Cargo
  feature is OFF (bare `i_overlay = "7.0"` in Cargo.toml), which
  compile-gates the rayon branches (`solver_fragment.rs:86,109` and,
  transitively, i_key_sort's parallel sort). The verbatim port must
  hardcode the serial branch, not port `is_parallel_sort_allowed` as a
  false-returning function.

`src/geometry/pyramid.rs` production code imports only
`i_overlay::i_float::int::point::IntPoint` (line 6) - the coordinate type
behind elivagar's own `Contour`/`Shape`/`Shapes` aliases. Its test module
additionally uses the crate directly (`Overlay` + `OverlayRule::Xor`) as
the area-XOR oracle for the fast rect clip, and `int_ocean.rs` tests use
the convenience wrappers. `src/ocean.rs` mentions i_overlay only in a
comment.

Callers of the two ops (who feels the churn):

- `pyramid::root_fragments` - one `normalize_into` per feature.
- `pyramid::emit_cell` - one `normalize_into` per shape per pyramid cell
  (the 11.4M calls; P50 570 ns = the simplify_contour fast path).
- `pyramid::intersect_shapes_with_rect` - `intersect_rect_into` only as
  the fallback when the guarded integer Sutherland-Hodgman fast path
  (`clip_shape_rect_fast`) refuses (multi-crossing / on-line vertex).
- `ocean::push_quantized_pieces` - `intersect_rect_into` against the
  data-bounds rect for shapefile shapes that cross the extract boundary
  (bbox-inside shapes skip it). These are the multi-MB calls.
- `ocean::parse_ocean_record` builds one fresh `IntEmitScratch` PER
  SHAPEFILE RECORD inside `records.par_iter().map(...)`
  (`src/ocean.rs:456`) - the engine never warms up in the parse phase.

Ownership flow downstream of the ops (matters for Landing 2): `emit_cell`
does `normalized.drain(..)` and passes each `Shape` BY VALUE into
`encode_tile_shape`, which consumes it (`into_iter`) and drops the ring
Vecs. `PyramidScratch::return_shapes` does `shapes.clear()`, which drops
inner `Shape` and `Contour` allocations - the existing `frag_pool` only
recycles the outermost Vec. Ocean `pieces: Vec<Shape>` legitimately owns
long-lived memory and is out of pool scope.

### 2.2 What the crate does per call, and what allocates

Vendored source: `research/iOverlay/iOverlay/` (version 7.0.2 - identical
to the locked production dependency in `Cargo.lock`; the 4.5.2 entry there
is a transitive dep of an unrelated crate). Helper crates are not vendored
but are pinned in Cargo.lock and readable in the local registry
(`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`): i_float
3.0.0, i_shape 3.0.0, i_tree 0.19.0, i_key_sort 0.10.3.

Reference-completeness caveat: roughly half the ported lines have NO copy
under `research/iOverlay` - the four helper crates above live only in the
cargo registry. This matters most for the two highest-risk surfaces: the
i_key_sort bin sort (bit-identical output demands exact tie-break and bin
ordering) and the cross-solver numeric primitives, which are i_float
methods, not i_overlay code - `cross_solver.rs:297-301` calls
`<I::WideUInt as UIntNumber>::Product::multiply(...)` and
`.divide_with_rounding(udiv)` (u128 rounded product/divide, the exact
snap-rounding math). Brick 1.0.5 vendors these four crates into
`research/` so the diff-against-reference is complete, and brick 1.1
ports the numeric primitives with their own dedicated tests before any
solver code depends on them.

Pipeline of one `overlay()` call (`core/overlay.rs`):

1. **Segment append** (`segm/build.rs` `append_path_iter`): pushes
   collinear-filtered `Segment { XSegment{a,b}, ShapeCountBoolean }` into
   the persistent `segments` Vec. Reused; no fresh alloc when warm.
2. **Split** (`split/solver.rs` `split_segments`): `sort_by_ab` +
   `merge_if_needed` + intersection solving (list < 4,000 segments, tree
   4,000-16,000, fragment grid > 16,000; `core/solver.rs` thresholds),
   snap-rounded via `split/cross_solver.rs`, marks applied by
   `SplitSolver::apply`. `marks` is persistent; everything else below
   churns.
3. **Fill sweep** (`build/sweep.rs`, NonZero strategy): scan-line over
   segments computing a 4-bit `SegmentFill` per segment. Scan structure
   (`KeyExpList` < 8,000 segments, else `KeyExpTree`, from i_tree) is
   persisted via `SweepRunner`'s Option take/put.
4. **Link filter + graph** (`build/boolean.rs`, `build/graph.rs`): links,
   ends, fills Vecs persistent in `GraphBuilder`; nodes Vec persistent
   but see below.
5. **Extract** (`core/extract.rs`): walks the graph, validates each
   contour (collinear cleanup via i_shape `simplify_contour`, min-area),
   then hole-joins (`bind/solver.rs` `ShapeBinder` scan over sorted
   x-anchors) into nested `IntShapes`.

Fresh heap allocations per call, in the warm state elivagar already has
(this is the Landing 2 kill list; file references into the vendored tree):

- a. `extract()`: fresh `shapes`, `holes`, `anchors` Vecs per call; one
  `buffer.points.as_slice().to_vec()` per output contour; one
  `vec![contour]` per output hull (`core/extract.rs:110-176`).
- b. Hole binder: `scan_join` allocates `segments` with capacity = half
  the summed outer-ring lengths, plus `parent_for_child` and
  `children_count_for_parent`, plus a fresh `KeyExpList`/`KeyExpTree`
  scan structure per bind (`bind/solver.rs:36-78,171-197`). This is a
  large share of `intersect_rect_into`'s 1.3 MB/call average.
- c. i_key_sort bin sorts: every `sort_by_ab` (twice per split when marks
  fire), `build_ends`, and `test_contour_for_loops` on >= 64-point
  contours routes through `sort_by_two_keys*`, which above 64 elements
  runs a bin sort with an internally allocated `Vec::new()` buffer
  (`i_key_sort src/sort/two_keys_cmp.rs:102`, `two_keys.rs` same
  pattern). A buffer-reusing variant exists upstream
  (`sort_by_two_keys_then_by_and_buffer`) but the call sites do not use
  it.
- d. `OverlayNode::Cross(Vec<usize>)`: one heap Vec per graph node of
  degree >= 3, rebuilt every call (`core/graph.rs:23-37`,
  `build/graph.rs:90`); plus the local `indices` scratch is fine (one
  per call) but the per-node `indices.to_vec()` is not.
- e. Tree split: fresh `SegExpTree` + `reusable_buffer` per call
  (`split/solver_tree.rs:46,52`). Fragment split: fresh `GridLayout` +
  `FragmentBuffer` + `reusable_buffer` per call, and `on_border_split`
  allocates a `points` Vec per border column
  (`split/solver_fragment.rs:27-35,194`). List split: fresh
  `reusable_buffer` per call (`split/solver_list.rs:27`).
- f. `simplify_contour` reverse path: `contour.to_vec()` when the input
  ring is perfect but wrongly wound, plus the nested
  `Some(vec![vec![rev_contour]])` (`core/simplify.rs:104-106`).
- g. Sweep: `node` staging Vec (`Vec::with_capacity(4)`) per sweep run
  (`build/sweep.rs:45`).
- h. `split_segments`'s `D::Store::default()` is `()` for boolean ops -
  free, no action needed.

### 2.3 What we do NOT use (drops out at port time)

Whole subsystems: `float/`, `string/`, `mesh/`, `vector/`,
`core/{divide,edge_overlay,extract_ogc,predicate,relate}.rs`. Overlay
rules other than Subject and Intersect. Fill rules other than NonZero.
`ContourDirection::Clockwise` output. OGC extraction.
`preserve_input_collinear` / `preserve_output_collinear` = true paths.
`overlay_into`/`FlatContoursBuffer` (see 2.5). Generic `D` edge data
(always `()`), generic `I` (always i32, Wide = i64, WideUInt = u64).
Rayon paths (feature-gated off today).

### 2.4 Why bit-identical output is the gate, and why it is achievable

The blessed denmark archive (`data/blessed/denmark-c9362c4.pmtiles`) and
the pyramid fast-clip XOR-budget test
(`pyramid.rs::fast clip diverges...`, tolerance "one unit ALONG the cut
line") both encode i_overlay's exact noding: snap radius progression
(`Precision::HIGH`: 2^0 doubling, cap 2^60), cross-solver rounding, mark
ordering, extraction order, and hole binding. The earcut oracle
(`scripts/validate/earcut-oracle.mjs`) is the standing tessellation gate;
ledger context in `notes/rendering-postmortem.md` (R23: a symmetric
encoder/decoder convention bug invisible to internal round-trips for
three months). A port that is the same algorithm, same arithmetic, same
iteration order produces byte-identical output; anything less shows up in
`brokkr regress` as nonzero diffs and fails the landing. Therefore
Landing 1 is a verbatim-semantics port (monomorphize and prune, never
"improve"), and every Landing 2 change is allocation-structural only
(same values, same order, different storage).

Determinism note: list/tree/fragment split strategies are performance
strategies over the same exact cross solver and the same mark-apply; the
strategy choice is by segment count, which the port preserves, so the
chosen path (and output) per call is unchanged. Reinforcing the
achievability: i_overlay's core carries NO `HashMap`/`HashSet`/`BTreeMap`
/hashbrown - every structure is an ordered Vec or an explicit tree keyed
by geometry, so there is no hash-iteration nondeterminism to reproduce.
Byte-identical output is a matter of preserving arithmetic and Vec
iteration order, nothing subtler.

### 2.5 Alternative considered and rejected

Upstream 7.0 already offers lower-alloc output APIs:
`Overlay::overlay_into` / `simplify_flat_buffer` writing into a
`FlatContoursBuffer`. Rejected as the campaign vehicle: (1)
`extract_contours_into` skips hole joining entirely - output is a flat
contour soup, and rebuilding outer/hole nesting ourselves reintroduces
exactly the misattached-hole failure class the earcut oracle exists to
catch; (2) it leaves every internal churn source in 2.2 (binder, bin-sort
buffers, Cross node Vecs, tree/fragment structures) and the ~41 s of
library thread-time untouched; (3) it keeps the dependency we are trying
to own. The flat-buffer idea itself survives inside Landing 2 as
engine-internal storage.

## 3. Target

### 3.1 New module

`src/geometry/overlay/` - a self-contained, i32-only, two-rule boolean
engine. Public (crate) surface:

```rust
// src/geometry/overlay/mod.rs
pub(crate) struct IntPoint { pub x: i32, pub y: i32 }
// + PartialOrd/Ord (x then y, identical to i_float), Add, Sub->
//   (i64,i64) vector helpers, Hash, Default. Replaces
//   i_overlay::i_float IntPoint everywhere in production.

pub(crate) enum ShapeType { Subject, Clip }
pub(crate) enum BoolRule { Subject, Intersect }

pub(crate) struct BoolOverlay {
    // persistent op state (was Overlay<i32> + GraphBuilder + SplitSolver)
    segments: Vec<Segment>,          // Segment{XSegment, ShapeCount}
    marks: Vec<LineMark>,
    fills: Vec<u8>,                  // SegmentFill
    links: Vec<OverlayLink>,
    ends: Vec<End>,
    // CSR node storage (replaces Vec<OverlayNode> with per-node Vecs)
    node_offsets: Vec<u32>,
    node_indices: Vec<u32>,
    // extraction state (was BooleanExtractionBuffer + extract() locals)
    visited: Vec<VisitState>,
    points: Vec<IntPoint>,
    holes: Vec<Contour>,             // hole staging, rings from pool
    anchors: Vec<IdSegment>,
    // hole binder state (was fresh per bind)
    bind_segments: Vec<IdSegment>,
    parent_for_child: Vec<usize>,
    children_per_parent: Vec<usize>,
    // Scan structures. Three concrete monomorphizations across TWO tree
    // families - the port map row for scan.rs enumerates them. The fill
    // sweep and the hole binder share the KeyExp{List,Tree} family but at
    // DIFFERENT value types, so they cannot share one storage field:
    fill_scan_list: KeyExpList<VSegment, i32, ShapeCount>,   // sweep
    fill_scan_tree: KeyExpTree<VSegment, i32, ShapeCount>,   // sweep
    bind_scan_list: KeyExpList<VSegment, i32, ContourIndex>, // hole binder
    bind_scan_tree: KeyExpTree<VSegment, i32, ContourIndex>, // hole binder
    seg_tree: SegExpTree<i32, i32, IdSegment>, // tree split - distinct family, keyed by integer range
    frag: FragBuffer,                // GridLayout + FragmentBuffer port
    sort_buf: SortScratch,           // bin-sort reusable buffers
    sweep_node: Vec<End>,
    ring_pool: Vec<Contour>,         // Landing 2: recycled ring storage (Vec<IntPoint> bodies)
    shape_pool: Vec<Shape>,          // Landing 2: recycled Shape shells (Vec<Contour>), see 3.3
    pub min_output_area: u64,
}

impl BoolOverlay {
    pub(crate) fn new() -> Self;
    pub(crate) fn clear(&mut self);
    pub(crate) fn add_contour(&mut self, c: &[IntPoint], t: ShapeType);
    pub(crate) fn add_shape(&mut self, s: &Shape, t: ShapeType);
    /// Boolean op; appends nested shapes into `out`, reusing `out`'s
    /// spare capacity and the internal ring pool (Landing 2).
    pub(crate) fn overlay_nested(&mut self, rule: BoolRule, out: &mut Shapes);
    /// Fast-path single-contour simplify. `false` = contour already
    /// perfect and correctly wound (caller keeps input); `true` = result
    /// (possibly empty) written into `out`.
    pub(crate) fn simplify_contour_into(&mut self, c: &[IntPoint], out: &mut Shapes) -> bool;
    /// Return exhausted shapes so their rings re-enter the pool.
    pub(crate) fn recycle(&mut self, shapes: &mut Shapes);
    pub(crate) fn recycle_shape(&mut self, shape: &mut Shape);
}
```

Internal files (port map, vendored path -> new file):

| new file | ports | notes |
|---|---|---|
| `mod.rs` | core/overlay.rs, core/solver.rs | entry points, thresholds (4,000 / 8,000 / 16,000), Precision::HIGH constants |
| `point.rs` | i_float point.rs, triangle.rs subset, + the WideUInt numeric primitives | IntPoint, Triangle::{is_clockwise, clock_order, area_two}, cross/dot in i64, AND the u128 `Product::multiply` / `divide_with_rounding` rounded-integer helpers the cross solver depends on (`cross_solver.rs:297-301`) - these are i_float, not i_overlay, and have no copy under research/. Ported and tested by brick 1.1 before the solver stack. |
| `segment.rs` | segm/{segment,boolean,winding,build,merge,sort}.rs, geom/{x_segment,v_segment,end,id_point,line_range}.rs | DropCollinear filter only |
| `split.rs` | split/{solver,solver_list,solver_tree,solver_fragment,line_mark,snap_radius}.rs | serial only |
| `cross.rs` | split/cross_solver.rs | verbatim arithmetic - the noding heart |
| `grid.rs` | split/{grid_layout,fragment}.rs | fragment path (> 16,000 segments; large coastal pieces and planet-scale relations hit it) |
| `scan.rs` | i_tree key/{list,tree,node,pool,entity,exp}.rs, seg/{tree,layout,heap,chunk,bit,entity,exp}.rs | THREE concrete monomorphizations across TWO distinct tree families, not two. Family A = `KeyExp{List,Tree}<VSegment, i32, V>`: fill sweep instantiates `V = ShapeCount` (`sweep.rs:92-94`), hole binder instantiates `V = ContourIndex` (`bind/solver.rs:38,47`). Family B = `SegExpTree<i32, i32, IdSegment>` in the tree split (`solver_tree.rs:46`) - keyed by an integer range, NOT by VSegment; structurally different from family A. The porter needs both the ShapeCount and ContourIndex KeyExp instantiations plus the SegExpTree - three types, largest single port surface. |
| `sort.rs` | i_key_sort serial two-keys(+cmp) bin sort, bin_layout, buffer | all entry points take `&mut SortScratch`; < 64 elements = std `sort_unstable_by` exactly as upstream |
| `fill.rs` | build/{builder,boolean,sweep,util}.rs | NonZero strategy; Subject + Intersect filters; test_contour_for_loops |
| `graph.rs` | build/graph.rs, core/{graph,link,edge_data}.rs | CSR nodes (Landing 2; Landing 1 ports the enum verbatim) |
| `extract.rs` | core/extract.rs, core/nearest_vector.rs, core/overlay_rule.rs (is_fill_top), bind/{segment,solver}.rs, i_shape simple.rs (simplify_contour) + path.rs (is_clockwise_ordered, unsafe_area) + the `ContourExtension::validate` method | extraction + hole binding. The per-contour cleanup at `extract.rs:147` is `buffer.points.validate(min_output_area, preserve_output_collinear)` - the i_shape method that does the min-area test + collinear cleanup + winding fix at the heart of the extraction loop. Port it explicitly; it returns `(is_valid, is_modified)` and drives the anchor-resort branch. Note: `BooleanExtractionBuffer.contour_visited: Option<Vec<VisitState>>` (`extract.rs:37`) is OGC-only and deliberately absent from the BoolOverlay sketch - OGC is pruned. |
| `simplify.rs` | core/simplify.rs | fast path + contour_direction (NonZero arm) |

Size: the ~4,500-5,500 line estimate is optimistic and should be read as
a floor, not a target. The named iOverlay source files alone are ~8k
lines before pruning, and the in-tree helper subsets add materially on top
- i_tree (three instantiations across two tree families) and i_key_sort
together are ~3.3k lines before pruning, plus the i_shape (`validate`,
`simplify_contour`, path predicates) and i_float (point, triangle, WideUInt
numeric primitives) surfaces. Realistic ported size after aggressive
pruning is more like 6,000-8,000 lines. Plus the relevant upstream unit
tests carried over. `research/iOverlay/` stays untouched as the reference
copy.

### 3.2 Integration points

- `int_ocean.rs`: `IntEmitScratch.overlay` becomes `BoolOverlay`;
  `normalize_into` / `intersect_rect_into` keep their signatures and
  their `#[hotpath::measure]` attributes (profile continuity);
  `overlay_options` folds into setting `overlay.min_output_area`.
  `pub(crate) type Contour = Vec<overlay::IntPoint>` - alias chain
  otherwise unchanged.
- `pyramid.rs` line 6 imports `IntPoint` from the new module (via
  int_ocean re-export).
- Tests keep i_overlay as an independent oracle: `i_overlay = "7.0"`
  moves to `[dev-dependencies]`. Test-side conversion helpers map our
  `IntPoint` <-> `i_overlay` points at the oracle boundary (the XOR area
  test in pyramid.rs and any test still calling the crate directly).
- `Cargo.toml` `[dependencies]` loses i_overlay; `Cargo.lock` rides
  along with the landing commit.

### 3.3 Landing 2 recycling contract

- All 2.2 items a-g get engine-owned storage (fields above), cleared not
  dropped.
- Two-level pooling, because a `Shape` is `Vec<Contour>` and a `Shapes` is
  `Vec<Shape>` - `Shapes::clear()` and the current `drain(..)` flow drop
  BOTH the inner `Contour` bodies AND the `Shape` shells (the `Vec<Contour>`
  containers). Ring pooling alone cannot satisfy "hulls into pooled Shape
  slots"; the shell Vecs would still churn. So:
  - `ring_pool: Vec<Contour>` recycles the `Vec<IntPoint>` ring bodies.
    Extraction takes a pooled `Contour` (swap out of pool, clear, extend).
  - `shape_pool: Vec<Shape>` recycles the `Vec<Contour>` shells. A hull
    takes a pooled `Shape` shell, fills it from pooled rings, and pushes it
    into an `out` slot reused in place (same slot-recycling discipline as
    `quantize_ring_at`, i.e. `out` grows by reused-capacity push and is
    cleared not dropped between calls).
  - `recycle`/`recycle_shape` walk each `Shape` in the exhausted `Shapes`,
    return every ring body to `ring_pool`, then return the now-empty shell
    to `shape_pool` - nothing is dropped. This is the exact non-dropping
    ownership protocol the whole de-churn hinges on.
- Caller changes to close the loop (each mechanical, all in-tree):
  - `encode_tile_shape(tile_shape: Shape, ...)` becomes
    `(&mut Shape, ...)` (it only reads; today's `into_iter` is
    incidental); `emit_cell` iterates `normalized` by `&mut` slot and
    afterwards returns the whole `Shapes` via `scratch.int` recycling
    instead of `drain(..)`.
  - `PyramidScratch::return_shapes` routes through
    `BoolOverlay::recycle` before pushing the husk to `frag_pool`, so
    inner rings survive.
  - `ocean::parse_ocean_record`: hoist the per-record scratch to
    per-worker via `par_iter().map_init(IntEmitScratch::new, ...)`
    (`src/ocean.rs:245` parse phase) - one warm engine per rayon worker
    instead of one cold engine per shapefile record.
- Node storage flips from `Vec<OverlayNode>` (enum with per-node
  `Cross(Vec)`) to CSR: `build_nodes_and_connect_links` writes
  `node_offsets`/`node_indices`; `find_left_top_link`, `next_link`, and
  `find_nearest_link_to` take `(&[u32], &[u32])` and preserve today's
  iteration order over indices exactly (bridge = the degree-2 case, an
  `if` on slice length instead of an enum arm).
- Bin sorts call the ported buffer-reusing variant with
  `sort_buf` - same algorithm, same output order, zero fresh allocation.

## 4. Landing 1: verbatim-subset port, switch, dependency demotion

One coherent, fully intrusive landing: the module lands, production
switches to it, and i_overlay demotes to dev-dependency in the same
commit. Keep/revert is read on the gates below; revert = one commit
revert.

Bricks, in order (all one commit; order is the build order):

- **1.0 Baselines at the parent commit.** Fresh verdict-grade numbers to
  read L1 against (bench) and to anchor the L2 churn delta (alloc,
  hotpath):
  ```
  brokkr tilegen --bench 3 --dataset denmark --variant locations
  brokkr tilegen --alloc --dataset denmark --variant locations
  brokkr tilegen --hotpath --dataset denmark --variant locations
  ```
  All gate runs are `--variant locations`: that is the production input
  shape, the blessed regress references are locations runs, and a
  bench/regress read across variants is not a verdict (see the gate
  discipline in `reference/technical-implementation-spec.md`). Record
  UUIDs + the parent commit hash in this document's Results section when
  run. Also ask the user whether to bless a norway locations reference at
  the parent commit (`brokkr tilegen --dataset norway --variant
  locations` then `brokkr bless --dataset norway --file <output>`) so
  L1/L2 identity is regress-gated on coastal data too - the ops being
  ported are the ocean boolean ops, so coastal coverage is worth more
  here than usual; blessing is user-say-so per AGENTS.md. Without it,
  norway is not separately gated and denmark regress carries the identity
  gate.
- **1.0.5 Vendor the helper crates + record attribution.** Copy the
  pinned i_float 3.0.0, i_shape 3.0.0, i_tree 0.19.0, i_key_sort 0.10.3
  sources from the local registry into `research/` (alongside
  `research/iOverlay/`) so the reference tree the port is diffed against
  is complete - today ~half the ported lines (all four helper crates,
  including the i_float numeric primitives and the i_key_sort bin sort)
  have no pristine copy under `research/`. These stay untouched as
  reference, exactly like `research/iOverlay/`. Same brick: settle the
  licensing. i_overlay and its helpers are `MIT OR Apache-2.0`
  (`research/iOverlay/LICENSE-MIT`, `LICENSE-APACHE`); the MIT text must be
  preserved when we copy and modify the source. Add the upstream copyright
  notice + license reference to the new `src/geometry/overlay/` module
  header (and to `NOTICE`/`THIRD-PARTY` if the repo grows one) before the
  dependency is demoted in 1.7.
- **1.1 `point.rs` + `segment.rs`.** Types, the WideUInt numeric
  primitives, and segment building. `point.rs` ports not just IntPoint /
  Triangle but the i_float u128 `Product::multiply` and
  `divide_with_rounding` rounded-integer helpers that `cross.rs` depends
  on (`cross_solver.rs:297-301`) - these carry their OWN dedicated
  round-trip tests against the vendored i_float reference, because a
  one-ULP divergence here is a snap-rounding divergence that fails
  `brokkr regress` with no closer diagnostic. Port the upstream segm tests
  (build/merge roll tests are the tricky collinear-closure cases).
- **1.2 `sort.rs`.** Serial two-keys bin sort, buffer-taking signatures
  from day one (Landing 1 may still pass a local buffer; the point is
  the signature exists). Port upstream sort tests.
- **1.3 `cross.rs` + `split.rs` + `grid.rs` + `scan.rs`.** The split
  solver stack. Verbatim arithmetic in cross.rs; monomorphized scan
  structures.
- **1.4 `fill.rs` + `graph.rs`.** Sweep (NonZero only) and graph
  building (nodes as the verbatim enum in this landing).
- **1.5 `extract.rs` + `simplify.rs` + `mod.rs`.** Extraction, hole
  binder, fast-path simplify, engine entry points. Port the upstream
  overlay/simplify/extract tests that exercise Subject/Intersect +
  NonZero.
- **1.6 Differential oracle test.** New test in the module: a seeded
  deterministic generator (plain LCG, no new dependency) produces ~2,000
  cases of 1-6 rings x 3-200 points in a +-50,000 box (mix of
  self-intersecting, duplicate-point, collinear-run, and shared-vertex
  rings) plus axis-aligned clip rects; each case runs through both the
  new engine and dev-dep i_overlay (same options) and asserts identical
  nested output point-for-point. Also replays every `int_ocean.rs` and
  `pyramid.rs` geometry fixture through both. This is the instrument for
  behavior no external oracle reaches (gate: the test itself).
  CRITICAL coverage requirement: the 1-6 rings x 3-200 points envelope
  never exceeds ~1,200 segments, so `Solver::AUTO` stays on the LIST split
  and LIST fill for every one of those cases - the TREE split (> 4,000
  segments), the FRAGMENT split (> 16,000), and the tree FILL (> 8,000)
  would go completely untested, yet those are the largest and least-tested
  port surfaces (`grid.rs`, `scan.rs`). Upstream deliberately tests
  LIST/TREE/FRAG/AUTO separately (`tests/overlay_tests.rs:16`). The oracle
  MUST reach them by BOTH means: (1) a batch of large generated cases -
  dense many-ring inputs / fine grids that push segment counts across the
  4,000 and 16,000 thresholds under `AUTO`; and (2) forced-strategy runs
  of the same cases pinning `Strategy::{List,Tree,Frag}` explicitly on
  both engines, so a threshold quirk cannot mask a broken tree/fragment
  path. Without the forced-strategy cases an entire split family could
  ship untested.
- **1.7 Switch production.** `int_ocean.rs` to `BoolOverlay`;
  `pyramid.rs` IntPoint import; Cargo.toml dependency move; test-side
  conversion helpers.

Gates (exact commands, run in this order; every one must pass):

```
brokkr fmt
brokkr check
# commit here (bench requires a clean tree; never benchmark uncommitted)
brokkr tilegen --bench 3 --dataset denmark --variant locations
brokkr regress --dataset denmark
brokkr verify pmtiles --dataset denmark
cd scripts/validate && node earcut-oracle.mjs ../../data/tilegen_tmp/bench-self-output.pmtiles
brokkr regress --dataset norway   # only if a norway locations reference was blessed
```

Denmark carries the full battery (identity + wall + correctness); the
verbatim port's real gate is regress bit-identity, not a three-dataset
wall survey. A blessed norway reference adds coastal-identity coverage,
which the ocean ops being ported make worth having; it is the one
extra run justified here, and only if blessed.

Verdict rules:
- `brokkr regress` (denmark, and norway if blessed): zero diffs, tol 0.
  Any structural diff = revert; there is no "close enough" for a
  verbatim port.
- Earcut oracle: 0 deviant polygons, 0 misattached holes, every polygon
  layer.
- Bench: this landing claims neutrality. Denmark best-of-3 within +-5%
  of the 1.0 baseline. A regression past 5% = investigate; past
  10% = revert.
- `brokkr check` and `elivagar verify` green (the landing is a single
  commit, so the boundary condition is trivially ordered).

## 5. Landing 2: de-churn - pooled output, engine-owned scratch

One landing, one commit, all changes allocation-structural
(value-and-order preserving). Bricks:

- **2.0 Baseline (already recorded at HEAD `8eaa8bf`).** Post-L1
  denmark-locations runs exist and are the L2 reference set (Results
  section): alloc `546b9d58`, hotpath `dec0d7f8`, bench `bcac01ad`. The
  churn keep gate below measures against `546b9d58`'s combined
  `normalize_into` + `intersect_rect_into` = 5.8 + 2.3 = 8.1 GB. L1's
  i32 monomorphization already pulled the pair down from section 1's
  stale 15.1 GB (pre-port `d20ddd5`), so the gate follows the recorded
  8.1 GB. No new baseline run is needed unless the tree moves before
  L2 lands.
- **2.1 Engine-owned temporaries.** Kill list items c, e, g and the
  binder arrays of b: route every bin sort through `sort_buf`; persist
  `seg_tree`, `frag` (GridLayout re-inited in place per call - its
  bucket Vecs survive), the split `reusable_buffer`, the sweep `node`
  staging Vec, `bind_segments`, `parent_for_child`,
  `children_per_parent`, and the binder scan structures.
- **2.2 CSR nodes.** Kill list item d, per 3.3.
- **2.3 Pooled extraction.** Kill list items a and f: extraction
  contours and hull shells from `ring_pool` / recycled `out` slots;
  fast-path reverse writes into a pooled ring instead of
  `contour.to_vec()`.
- **2.4 Caller recycling.** `encode_tile_shape` by reference;
  `emit_cell` returns `normalized` rings via `recycle`;
  `return_shapes` recycles rings before pooling husks; ocean parse phase
  `map_init` per-worker scratch (all per 3.3).

Gates (same order discipline: fmt, check, commit, then measure):

```
brokkr fmt
brokkr check
# commit
brokkr tilegen --bench 3 --dataset denmark --variant locations
brokkr regress --dataset denmark
brokkr verify pmtiles --dataset denmark
cd scripts/validate && node earcut-oracle.mjs ../../data/tilegen_tmp/bench-self-output.pmtiles
brokkr tilegen --alloc --dataset denmark --variant locations
brokkr tilegen --hotpath --dataset denmark --variant locations
brokkr results --compare-last --mode hotpath
```

The churn win is measured on denmark alloc and the identity on denmark
regress; germany adds nothing L2 gates on, so it is dropped. If a norway
locations reference is blessed, `brokkr regress --dataset norway` (after
a `brokkr tilegen --dataset norway --variant locations` run) is the
cheapest guard for the CSR-node-ordering risk on coastal shapes denmark
lacks - but the brick-1.6 differential oracle is the designed instrument
for it and runs inside `brokkr check`.

Verdict rules:
- **The brick-1.6 differential oracle MUST re-run green after L2.** It was
  wired to Landing 1 semantics, but the enum->CSR node flip (brick 2.2)
  and the pooling changes it. A subtle node-index ordering divergence in
  the CSR walk would be invisible to `brokkr regress` on denmark if it only
  bites shapes denmark does not contain, and invisible to the oracle unless
  it is re-run. `brokkr check` executes the module test, so the oracle runs
  automatically - but treat it as a first-class L2 keep gate, not incidental
  coverage: same ~2,000 generated cases plus forced-strategy runs, still
  point-for-point identical against dev-dep i_overlay, now with CSR nodes
  and pooled output live.
- `brokkr regress`: zero diffs, tol 0. Pooling changes storage, never
  values or order; a diff means a bug, not a tolerance question.
- Earcut oracle: clean, as above.
- **Primary keep gate - churn:** measured against the recorded brick-2.0
  alloc baseline `546b9d58` - combined `normalize_into` +
  `intersect_rect_into` = 8.1 GB at HEAD `8eaa8bf`. NOT section 1's stale
  15.1 GB: that was pre-port `d20ddd5`, and L1's i32 monomorphization
  already took the pair to 8.1 GB, which is why the gate follows the
  recorded baseline and not the prize figure. In the fresh L2 alloc
  profile the two functions' combined exclusive allocation must fall
  under 3 GB - a cut of at least ~5 GB (>=60%) from the 8.1 GB baseline.
  Between 3 and 4 GB is a soft pass to diagnose; above 4 GB (less than
  half the baseline removed) the de-churn underdelivered against a kill
  list aimed squarely at these two frames: diagnose or revert.
- Wall neutral-or-better: denmark best-of-3 not worse than
  -5% vs the brick-2.0 baseline; any wall win is a bonus recorded, not the
  gate (thread-time in the 41 s region is expected to fall with the
  churn, but hotpath ranks rather than measures - report the delta from
  `--compare-last --mode hotpath`, gate on nothing).
- Expected collateral, recorded not gated: `emit_ocean_piece` exclusive
  churn down (ring recycling through `return_shapes`), total run churn
  down from 583.3 GB (`e1afc38d`).

After both landings: write the hash-anchored before/after rows
(bench UUIDs, alloc totals) into `reference/performance.md`, and update
the H6 OPEN DOOR block in `notes/planet-30gb-roadmap.md` to point here
with the measured verdict. Update AGENTS.md's `geometry/int_ocean.rs`
architecture bullet (i_overlay -> in-tree engine) in the same commit as
whichever landing ships last.

## 6. Stopping rule and out of scope

- **No algorithmic redesign.** The noding (cross solver, snap radius
  progression), sweep, extraction walk, and hole binder keep their exact
  algorithms and arithmetic. A "smarter rect clip" inside the engine is
  explicitly out: the pyramid's `clip_shape_rect_fast` already skims the
  easy cases before the boolean is reached.
- **`Shape`/`Shapes` representation unchanged** at module boundaries.
  Converting the geometry pipeline to flat point+range buffers
  end-to-end is a separate future campaign; the flat idea lives only
  inside the engine.
- **No allocator work.** The mimalloc/system/jemalloc A/B is the H6
  allocator addendum, already promoted and running separately.
- **research/iOverlay stays** as the pristine reference, as do the four
  helper crates vendored alongside it in brick 1.0.5 (i_float, i_shape,
  i_tree, i_key_sort); the port never modifies any of them.
- **i_overlay remains a dev-dependency** as the differential oracle. If
  a future cleanup wants it fully gone, that is its own decision after
  the oracle has served through at least one more geometry campaign.
- **No planet or NA runs** for this campaign; denmark/norway/germany
  gates only (NA is an explicit user decision per
  `reference/performance.md`).
- Unused ported surface (rules/paths we pruned) is not kept "for
  completeness": if it is not reachable from the two ops, it is not
  ported.

## 7. Results

### Landing 1 (verbatim port + dependency demotion) - LANDED

L1 landed across commits `d570daa` (vendor + in-tree engine, path-deps
removed), `fbca741` (sort + scan monomorphized to i32), `b97cddc`
(dead-surface prune + strict lints), `659a187` (12-file layout collapse).
i_overlay is now a dev-dependency (the 2,000-case differential oracle).

Post-L1 baselines at HEAD `8eaa8bf`, plantasjen, denmark **locations** -
the L2 reference set:

| mode | uuid | elapsed | note |
|---|---|---|---|
| bench | `bcac01ad` | 13.3 s | wall neutrality anchor |
| hotpath | `dec0d7f8` | 16.0 s | thread-time ranks |
| alloc | `546b9d58` | 14.4 s | churn baseline (below) |

Churn baseline (`546b9d58`, exclusive alloc): `normalize_into` 5.8 GB
(13.44%, 11.41M calls) + `intersect_rect_into` 2.3 GB (5.42%, 6,550
calls) = **8.1 GB combined**. Total run churn 578.4 GB alloc /
576.8 GB dealloc, peak RSS 9.2 GB. L1's i32 monomorphization already
cut the pair from section 1's stale 15.1 GB (`e1afc38d`, pre-port
`d20ddd5`) to 8.1 GB; the L2 churn gate measures against 8.1 GB.

Coastal/scale context, also at `8eaa8bf`: norway locations bench
`0e54fdd7` 50.4 s / alloc `982e4021`; germany locations bench
`a6d1cb9e` 84.5 s. Raw-variant runs exist but are not the gate (blessed
references are locations).

### Landing 2 (de-churn)

(Filled at L2 landing: commit + gate readings + churn delta vs
`546b9d58`.)

## 8. Review reconciliation (2026-07-09)

Two step-2 reviews (Opus R1, codex gpt-5.5 R2) were validated against the
vendored 7.0.2 source and folded above. What landed where:

- Scan storage undercount (R1 finding 1 + R2 High 2, same defect): the
  port map claimed two scan value types; there are THREE monomorphizations
  across TWO tree families - `KeyExp*<VSegment,i32,ShapeCount>` (sweep),
  `KeyExp*<VSegment,i32,ContourIndex>` (bind), `SegExpTree<i32,i32,
  IdSegment>` (tree split). Folded into 3.1 struct fields + scan.rs port
  row.
- `validate` unnamed (R1 2): named explicitly in the extract.rs port row.
- Parallel-sort phrasing (R1 3): 2.1 now states `is_parallel_sort_allowed`
  returns true and the paths die on the compile-gated feature.
- `contour_visited` OGC-only (R1 4): noted in the extract.rs row.
- No-hash-structures determinism (R1 verified-accurate): added to 2.4.
- Helper crates unvendored + numeric primitives (R1 risk + R2 Low): new
  brick 1.0.5 vendors them; brick 1.1 ports and tests the i_float WideUInt
  `Product::multiply`/`divide_with_rounding` primitives; 2.2 caveat added.
- Tree/fragment split untested by the oracle (R2 High 1): brick 1.6 now
  requires large + forced-strategy cases across the 4,000/16,000
  thresholds.
- Pooled Shape shells (R2 High 3): 3.1 gains `shape_pool`; 3.3 pins the
  two-level (ring body + shape shell) non-dropping recycle protocol.
- License/attribution (R2 Medium): brick 1.0.5 preserves the
  MIT-OR-Apache notice before the dependency demotes in 1.7.
- L2 alloc gate on a stale number (R2 Medium): the keep gate now measures
  against the recorded 1.0 baseline, not section 1's `e1afc38d`.
- CSR node change invisible to the L1-wired oracle (R1 risk): L2 verdict
  rules now make the 1.6 differential oracle a first-class L2 keep gate.
- Line-count optimism (R1 risk + R2 Low): 3.1 estimate revised to a
  6,000-8,000 line realistic range.

Rejected:

- R1 finding 5 (`emit_ocean_piece` 8.1 GB looks like a transcription slip
  of `intersect_rect_into`'s 8.1 GB). Verified against alloc profile
  `e1afc38d` in `.brokkr/results.db`: `emit_ocean_piece` is genuinely
  8.1 GB exclusive (16.98% of tracked), `intersect_rect_into` is
  8.1 GB (16.96%). Two unrelated functions that legitimately round to the
  same 8.1 GB - a real coincidence, not an error. No change; section 1's
  numbers stand.
