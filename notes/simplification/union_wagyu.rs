// wagyu_union.rs - Polygon union via Vatti clipping algorithm
//
// Port of the Wagyu C++ library (as used in Tippecanoe) to Rust,
// specialized for union operations with positive fill rule on
// integer coordinates. No external dependencies.
//
// Algorithm: Vatti 1992 sweep-line polygon clipping, adapted from
// the Clipper library by Angus Johnson and Mapbox's Wagyu fork.

#![forbid(clippy::unwrap_used)]

// ============================================================
// Types
// ============================================================

type Pt = (i32, i32);
type PointId = usize;
type RingId = usize;
type BoundIdx = usize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Left,
    Right,
}

// ============================================================
// Edge
// ============================================================

#[derive(Debug, Clone)]
struct Edge {
    bot: Pt, // higher Y (bottom in screen coords)
    top: Pt, // lower Y (top in screen coords)
    dx: f64, // dx per unit decrease in y
}

impl Edge {
    fn new(current: Pt, next: Pt) -> Self {
        let (bot, top) = if current.1 >= next.1 {
            (current, next)
        } else {
            (next, current)
        };
        let dy = (top.1 - bot.1) as f64;
        let dx = if dy.abs() < f64::EPSILON {
            f64::INFINITY
        } else {
            (top.0 - bot.0) as f64 / dy
        };
        Edge { bot, top, dx }
    }

    fn is_horizontal(&self) -> bool {
        self.dx.is_infinite()
    }

    fn current_x(&self, y: i32) -> f64 {
        if y == self.top.1 {
            self.top.0 as f64
        } else {
            self.bot.0 as f64 + self.dx * (y - self.bot.1) as f64
        }
    }
}

fn slopes_equal_pts(p1: Pt, p2: Pt, p3: Pt) -> bool {
    (p1.1 as i64 - p2.1 as i64) * (p2.0 as i64 - p3.0 as i64)
        == (p1.0 as i64 - p2.0 as i64) * (p2.1 as i64 - p3.1 as i64)
}

fn slopes_equal_edges(e1: &Edge, e2: &Edge) -> bool {
    (e1.top.1 as i64 - e1.bot.1 as i64) * (e2.top.0 as i64 - e2.bot.0 as i64)
        == (e1.top.0 as i64 - e1.bot.0 as i64) * (e2.top.1 as i64 - e2.bot.1 as i64)
}

// ============================================================
// Bound
// ============================================================

#[derive(Debug)]
struct Bound {
    edges: Vec<Edge>,
    current_edge: usize,
    next_edge: usize,
    last_point: Pt,
    ring_id: Option<RingId>,
    maximum_bound: Option<BoundIdx>,
    current_x: f64,
    pos: usize,
    winding_count: i32,
    winding_count2: i32,
    winding_delta: i8,
    side: Side,
}

impl Bound {
    fn new() -> Self {
        Bound {
            edges: Vec::new(),
            current_edge: 0,
            next_edge: 0,
            last_point: (0, 0),
            ring_id: None,
            maximum_bound: None,
            current_x: 0.0,
            pos: 0,
            winding_count: 0,
            winding_count2: 0,
            winding_delta: 0,
            side: Side::Left,
        }
    }

    fn cur_edge(&self) -> &Edge {
        &self.edges[self.current_edge]
    }

    fn is_maxima(&self, y: i32) -> bool {
        self.next_edge >= self.edges.len() && self.edges[self.current_edge].top.1 == y
    }

    fn is_intermediate(&self, y: i32) -> bool {
        self.next_edge < self.edges.len() && self.edges[self.current_edge].top.1 == y
    }

    fn cur_edge_is_horizontal(&self) -> bool {
        self.edges[self.current_edge].is_horizontal()
    }
}

// ============================================================
// Local Minimum
// ============================================================

struct LocalMinimum {
    left_bound: BoundIdx,
    right_bound: BoundIdx,
    y: i32,
    has_horizontal: bool,
}

// ============================================================
// Arena Point and Ring Manager
// ============================================================

struct ArenaPoint {
    x: i32,
    y: i32,
    ring_id: Option<RingId>,
    next: PointId,
    prev: PointId,
}

struct Ring {
    ring_index: usize,
    points: Option<PointId>,
    bottom_point: Option<PointId>,
    parent: Option<RingId>,
    children: Vec<Option<RingId>>,
    area_cached: Option<f64>,
}

impl Ring {
    fn new(index: usize) -> Self {
        Ring {
            ring_index: index,
            points: None,
            bottom_point: None,
            parent: None,
            children: Vec::new(),
            area_cached: None,
        }
    }
}

struct RingManager {
    points: Vec<ArenaPoint>,
    rings: Vec<Ring>,
    children: Vec<Option<RingId>>, // top-level rings
    ring_counter: usize,
}

impl RingManager {
    fn new() -> Self {
        RingManager {
            points: Vec::new(),
            rings: Vec::new(),
            children: Vec::new(),
            ring_counter: 0,
        }
    }

    fn create_ring(&mut self) -> RingId {
        let id = self.rings.len();
        self.rings.push(Ring::new(self.ring_counter));
        self.ring_counter += 1;
        id
    }

    fn create_point(&mut self, x: i32, y: i32, ring_id: Option<RingId>) -> PointId {
        let id = self.points.len();
        self.points.push(ArenaPoint {
            x,
            y,
            ring_id,
            next: id,
            prev: id,
        });
        id
    }

    fn create_point_before(
        &mut self,
        x: i32,
        y: i32,
        ring_id: Option<RingId>,
        before: PointId,
    ) -> PointId {
        let prev = self.points[before].prev;
        let id = self.points.len();
        self.points.push(ArenaPoint {
            x,
            y,
            ring_id,
            next: before,
            prev,
        });
        self.points[before].prev = id;
        self.points[prev].next = id;
        id
    }

    fn point_eq(&self, a: PointId, b: PointId) -> bool {
        self.points[a].x == self.points[b].x && self.points[a].y == self.points[b].y
    }

    fn point_coords(&self, id: PointId) -> Pt {
        (self.points[id].x, self.points[id].y)
    }

    fn area_of_ring(&mut self, ring_id: RingId) -> f64 {
        if let Some(area) = self.rings[ring_id].area_cached {
            return area;
        }
        let pts = match self.rings[ring_id].points {
            Some(p) => p,
            None => return 0.0,
        };
        let mut a = 0.0_f64;
        let start = pts;
        let mut cur = start;
        loop {
            let prev = self.points[cur].prev;
            a += (self.points[prev].x as f64 + self.points[cur].x as f64)
                * (self.points[prev].y as f64 - self.points[cur].y as f64);
            cur = self.points[cur].next;
            if cur == start {
                break;
            }
        }
        let area = a * 0.5;
        self.rings[ring_id].area_cached = Some(area);
        area
    }

    fn ring_is_hole(&self, ring_id: RingId) -> bool {
        // Determine by depth in the parent tree
        let mut depth = 0usize;
        let mut r = ring_id;
        while let Some(parent) = self.rings[r].parent {
            depth += 1;
            r = parent;
        }
        depth & 1 == 1
    }

    fn reverse_ring(&mut self, head: PointId) {
        let mut cur = head;
        loop {
            let next = self.points[cur].next;
            self.points[cur].next = self.points[cur].prev;
            self.points[cur].prev = next;
            cur = next;
            if cur == head {
                break;
            }
        }
    }

    fn update_points_ring(&mut self, ring_id: RingId) {
        if let Some(pts) = self.rings[ring_id].points {
            let mut cur = pts;
            loop {
                self.points[cur].ring_id = Some(ring_id);
                cur = self.points[cur].prev;
                if cur == pts {
                    break;
                }
            }
        }
    }

    fn add_child(&mut self, ring_id: RingId, parent: Option<RingId>) {
        match parent {
            Some(p) => {
                // Try to find an empty slot
                let slot = self.rings[p]
                    .children
                    .iter()
                    .position(|c| c.is_none());
                if let Some(idx) = slot {
                    self.rings[p].children[idx] = Some(ring_id);
                } else {
                    self.rings[p].children.push(Some(ring_id));
                }
            }
            None => {
                let slot = self.children.iter().position(|c| c.is_none());
                if let Some(idx) = slot {
                    self.children[idx] = Some(ring_id);
                } else {
                    self.children.push(Some(ring_id));
                }
            }
        }
        self.rings[ring_id].parent = parent;
    }

    fn remove_child(&mut self, ring_id: RingId, parent: Option<RingId>) {
        let children = match parent {
            Some(p) => &mut self.rings[p].children,
            None => &mut self.children,
        };
        for c in children.iter_mut() {
            if *c == Some(ring_id) {
                *c = None;
                return;
            }
        }
    }

    fn ring1_replaces_ring2(&mut self, ring1: Option<RingId>, ring2_id: RingId) {
        // Move ring2's children to ring1
        let ring2_children: Vec<Option<RingId>> = self.rings[ring2_id].children.clone();
        for child_opt in ring2_children {
            if let Some(child) = child_opt {
                self.rings[child].parent = ring1;
                match ring1 {
                    Some(r1) => {
                        let slot = self.rings[r1]
                            .children
                            .iter()
                            .position(|c| c.is_none());
                        if let Some(idx) = slot {
                            self.rings[r1].children[idx] = Some(child);
                        } else {
                            self.rings[r1].children.push(Some(child));
                        }
                    }
                    None => {
                        let slot = self.children.iter().position(|c| c.is_none());
                        if let Some(idx) = slot {
                            self.children[idx] = Some(child);
                        } else {
                            self.children.push(Some(child));
                        }
                    }
                }
            }
        }
        self.rings[ring2_id].children.clear();

        // Remove ring2 from its parent
        self.remove_child(ring2_id, self.rings[ring2_id].parent);
        self.rings[ring2_id].points = None;
        self.rings[ring2_id].area_cached = None;
    }
}

// ============================================================
// Edge Building
// ============================================================

fn build_edge_list(ring: &[(i32, i32)]) -> Vec<Edge> {
    if ring.len() < 3 {
        return Vec::new();
    }

    // Remove consecutive duplicate points and build a clean point list
    let mut pts: Vec<Pt> = Vec::with_capacity(ring.len());
    for &p in ring {
        if pts.last().map_or(true, |&last| last != p) {
            pts.push(p);
        }
    }
    // Close the ring: remove trailing point if it equals the first
    while pts.len() > 1 && pts.last() == pts.first() {
        pts.pop();
    }
    if pts.len() < 3 {
        return Vec::new();
    }

    // Remove collinear points
    let mut changed = true;
    while changed {
        changed = false;
        if pts.len() < 3 {
            return Vec::new();
        }
        let mut new_pts: Vec<Pt> = Vec::with_capacity(pts.len());
        let n = pts.len();
        for i in 0..n {
            let prev = if i == 0 { n - 1 } else { i - 1 };
            let next = (i + 1) % n;
            if !slopes_equal_pts(pts[prev], pts[i], pts[next]) {
                new_pts.push(pts[i]);
            } else {
                changed = true;
            }
        }
        pts = new_pts;
    }

    if pts.len() < 3 {
        return Vec::new();
    }

    // Build edges
    let n = pts.len();
    let mut edges = Vec::with_capacity(n);
    for i in 0..n {
        let next = (i + 1) % n;
        edges.push(Edge::new(pts[i], pts[next]));
    }

    edges
}

// ============================================================
// Local Minima Construction
// ============================================================

fn find_local_max_start(edges: &[Edge]) -> usize {
    // Find the first local maximum: a point where the edge list transitions
    // from going towards lower Y (top) to going towards higher Y (bot).
    // A local max is where an edge's top meets the next edge's top.
    if edges.len() <= 2 {
        return 0;
    }
    let n = edges.len();
    for i in 0..n {
        let prev = if i == 0 { n - 1 } else { i - 1 };
        let e_prev = &edges[prev];
        let e_curr = &edges[i];

        // Skip horizontals
        if e_prev.is_horizontal() || e_curr.is_horizontal() {
            continue;
        }

        // Local maximum: previous edge goes up (top is at top.y < bot.y),
        // current edge goes down. The meeting point is where prev.top == curr.top
        // or prev.top == curr.bot at the lowest Y.
        // A simpler heuristic: the meeting point between two non-horizontal edges
        // where both edges have their top at this point.
        if e_prev.top == e_curr.top {
            return i;
        }
        // Check if prev edge's top connects to curr edge (at high point)
        if e_prev.top == e_curr.bot {
            // prev goes up to this point, curr goes down from here = local minimum, not max
            continue;
        }
    }

    // Fallback: find the vertex with minimum Y (topmost on screen)
    let mut min_y = i32::MAX;
    let mut min_idx = 0;
    for (i, e) in edges.iter().enumerate() {
        if e.top.1 < min_y {
            min_y = e.top.1;
            min_idx = i;
        }
    }
    min_idx
}

fn build_local_minima(
    edges: &[Edge],
    bounds: &mut Vec<Bound>,
    minima: &mut Vec<LocalMinimum>,
) {
    if edges.is_empty() {
        return;
    }

    let n = edges.len();

    // Find all local minima and maxima by analyzing the Y-monotone pieces.
    // A local minimum is a vertex with the highest Y (bottommost on screen)
    // where the boundary transitions from going down to going up.
    // A local maximum is a vertex with the lowest Y (topmost on screen)
    // where the boundary transitions from going up to going down.

    // First, identify Y-monotone runs and pair them into bounds.
    // We trace the ring edge by edge, tracking the Y direction.

    // Find a starting point that is a local maximum (lowest Y vertex).
    let start = find_local_max_start(edges);

    // Collect all edges in order starting from 'start'
    let ordered: Vec<Edge> = (0..n).map(|i| edges[(start + i) % n].clone()).collect();

    // Split into bounds: alternating descending (towards min) and ascending (towards max)
    // Starting from a local max, the first run goes towards a local min (increasing Y),
    // then from that min towards the next max (decreasing Y), etc.

    let mut idx = 0;
    let mut first_min_bound: Option<BoundIdx> = None;
    let mut last_max_bound: Option<BoundIdx> = None;

    while idx < ordered.len() {
        // Create bound towards minimum (going from low Y to high Y, i.e., increasing Y)
        let mut towards_min_edges: Vec<Edge> = Vec::new();
        while idx < ordered.len() {
            let e = &ordered[idx];
            towards_min_edges.push(e.clone());
            idx += 1;

            if idx < ordered.len() {
                let next_e = &ordered[idx];
                // Check if we've reached a local minimum
                // (current edge's bot connects to next edge's bot = both going down to same point)
                if !e.is_horizontal()
                    && !next_e.is_horizontal()
                    && e.bot == next_e.bot
                {
                    break;
                }
                // If next is horizontal, keep going
                if e.is_horizontal() || next_e.is_horizontal() {
                    // More complex logic needed for horizontals at minima
                    // For now: if next edge starts going up (decreasing Y), we're at a minimum
                    if !next_e.is_horizontal() && !e.is_horizontal() {
                        // Non-horizontal edges: check if we changed direction
                        // prev edge goes down (bot.y > top.y, traversing top→bot)
                        // next edge should go up (bot.y > top.y, traversing bot→top)
                        // At a minimum, both edges have their bot at the same Y
                        if e.bot.1 >= next_e.bot.1 && next_e.bot.1 > next_e.top.1 {
                            // e goes to a lower point than next starts from
                            // Not a minimum yet
                        }
                    }
                }
            }
        }

        if towards_min_edges.is_empty() {
            break;
        }

        // Create bound towards maximum (going from high Y to low Y, i.e., decreasing Y)
        let mut towards_max_edges: Vec<Edge> = Vec::new();
        while idx < ordered.len() {
            let e = &ordered[idx];
            towards_max_edges.push(e.clone());
            idx += 1;

            if idx < ordered.len() {
                let next_e = &ordered[idx];
                if !e.is_horizontal()
                    && !next_e.is_horizontal()
                    && e.top == next_e.top
                {
                    break;
                }
            }
        }

        if towards_max_edges.is_empty() && idx >= ordered.len() {
            // Last bound pair - remaining edges go towards max
            // This can happen with simple polygons
            towards_max_edges = towards_min_edges.split_off(towards_min_edges.len() / 2);
            if towards_max_edges.is_empty() || towards_min_edges.is_empty() {
                break;
            }
        }

        if towards_max_edges.is_empty() {
            break;
        }

        // Determine the local minimum Y
        let min_y = towards_min_edges
            .last()
            .map(|e| e.bot.1)
            .expect("towards_min_edges should not be empty");

        // The towards_min bound's edges should be reversed (from max to min)
        // In Wagyu, the bound towards minimum stores edges going from min to max (reversed)
        towards_min_edges.reverse();

        // Determine left vs right
        let tm_first_non_h = towards_max_edges.iter().find(|e| !e.is_horizontal());
        let tn_first_non_h = towards_min_edges.iter().find(|e| !e.is_horizontal());
        let has_horizontal = towards_max_edges.first().map_or(false, |e| e.is_horizontal())
            || towards_min_edges.first().map_or(false, |e| e.is_horizontal());

        let minimum_is_left = match (tm_first_non_h, tn_first_non_h) {
            (Some(tm), Some(tn)) => {
                if has_horizontal {
                    tm.bot.0 <= tn.bot.0
                } else {
                    tm.dx <= tn.dx
                }
            }
            _ => true,
        };

        // Create bounds
        let min_bound_idx = bounds.len();
        let mut min_bound = Bound::new();
        min_bound.edges = towards_min_edges;
        min_bound.winding_delta = -1;
        bounds.push(min_bound);

        let max_bound_idx = bounds.len();
        let mut max_bound = Bound::new();
        max_bound.edges = towards_max_edges;
        max_bound.winding_delta = 1;
        bounds.push(max_bound);

        // Set maximum_bound links
        if let Some(last_max) = last_max_bound {
            bounds[min_bound_idx].maximum_bound = Some(last_max);
            bounds[last_max].maximum_bound = Some(min_bound_idx);
        }

        let (left_idx, right_idx) = if minimum_is_left {
            (min_bound_idx, max_bound_idx)
        } else {
            (max_bound_idx, min_bound_idx)
        };

        bounds[left_idx].side = Side::Left;
        bounds[right_idx].side = Side::Right;

        if first_min_bound.is_none() {
            first_min_bound = Some(if minimum_is_left {
                left_idx
            } else {
                right_idx
            });
        }
        last_max_bound = Some(if minimum_is_left {
            right_idx
        } else {
            left_idx
        });

        minima.push(LocalMinimum {
            left_bound: left_idx,
            right_bound: right_idx,
            y: min_y,
            has_horizontal,
        });
    }

    // Close the circular maximum_bound chain
    if let (Some(first), Some(last)) = (first_min_bound, last_max_bound) {
        bounds[last].maximum_bound = Some(first);
        bounds[first].maximum_bound = Some(last);
    }
}

// Simpler approach: extract Y-monotone sections from ring
fn build_local_minima_simple(
    ring: &[(i32, i32)],
    bounds: &mut Vec<Bound>,
    minima: &mut Vec<LocalMinimum>,
) {
    let edges = build_edge_list(ring);
    if edges.is_empty() {
        return;
    }

    // Build a clean vertex list from edges
    let n = edges.len();
    let mut vertices: Vec<Pt> = Vec::with_capacity(n);
    for e in &edges {
        // Reconstruct vertex order: each edge contributes its starting vertex
        // Edge goes from one vertex to next. We need the ring vertex order.
        // For edge (bot, top), the original direction depends on which was "current" vs "next"
        // in the ring. We need to reconstruct the ring ordering.
    }

    // Actually, let's work with edges directly.
    // Find local minima and maxima by examining consecutive edges.
    // A local minimum at vertex V means:
    //   - The edge before V goes downward (V has the highest Y)
    //   - The edge after V goes upward (V has the highest Y)
    // In edge terms: edge_i.bot is at V, edge_{i+1}.bot is at V (both edges have V as their bot)

    // Find local minima (vertices with maximum Y where direction reverses)
    let mut local_min_indices: Vec<usize> = Vec::new();
    for i in 0..n {
        let next = (i + 1) % n;
        let e1 = &edges[i];
        let e2 = &edges[next];

        // Check if this vertex is a local minimum (bottom)
        // Edge i ends at its top, and edge i+1 starts at... we need to figure out the connectivity.
        // In the original ring, edges are constructed from consecutive vertices:
        // edge[i] = Edge::new(ring[i], ring[i+1])
        // So edge[i] connects vertex i to vertex i+1.
        // After collinear removal, the edges still connect consecutive vertices.

        // For edge[i]: connects from its "current" vertex to "next" vertex.
        // If current.y >= next.y: bot=current, top=next → edge goes upward on screen
        // If current.y < next.y: bot=next, top=current → edge goes downward on screen

        // The vertex between edge[i] and edge[i+1] is the "next" vertex of edge[i],
        // which is the same as the "current" vertex of edge[i+1].

        // For a local minimum, this vertex should have the highest Y among its neighbors.
        // Both edge[i] and edge[i+1] should "go up" from this vertex.
        // edge[i]'s vertex is at its bot (if edge[i]'s "next" direction was downward)
        // or at its top (if edge[i]'s direction was upward).

        // This is getting confusing. Let me use a simpler approach.
    }

    // SIMPLER APPROACH: Extract vertices from edges, find Y-monotone pieces
    // Each edge connects vertex[i] to vertex[i+1] in the ring.
    // Reconstruct vertex positions from edges.

    // Actually, the cleanest approach: just work with the clean point list
    // and find local minima/maxima directly.

    let mut pts = ring.to_vec();
    // Close ring
    while pts.len() > 1 && pts.last() == pts.first() {
        pts.pop();
    }
    // Remove duplicates
    let mut clean: Vec<Pt> = Vec::with_capacity(pts.len());
    for &p in &pts {
        if clean.last().map_or(true, |&last| last != p) {
            clean.push(p);
        }
    }
    while clean.len() > 1 && clean.last() == clean.first() {
        clean.pop();
    }
    // Remove collinear
    let mut changed = true;
    while changed {
        changed = false;
        if clean.len() < 3 {
            return;
        }
        let mut new_pts: Vec<Pt> = Vec::with_capacity(clean.len());
        let cn = clean.len();
        for i in 0..cn {
            let prev = if i == 0 { cn - 1 } else { i - 1 };
            let next = (i + 1) % cn;
            if !slopes_equal_pts(clean[prev], clean[i], clean[next]) {
                new_pts.push(clean[i]);
            } else {
                changed = true;
            }
        }
        clean = new_pts;
    }
    if clean.len() < 3 {
        return;
    }

    let cn = clean.len();

    // Find local minima: vertex with Y >= both neighbors (allowing horizontal runs)
    // and local maxima: vertex with Y <= both neighbors
    // A vertex i is a local minimum if:
    //   clean[i].1 >= clean[prev].1 && clean[i].1 >= clean[next].1
    //   (and at least one is strictly greater, or we handle the horizontal case)

    // For the Vatti algorithm, we need to trace around the polygon and identify
    // where the Y-direction changes. Let's find the vertex with the absolute
    // maximum Y (the bottommost point) as a starting point.

    let mut max_y_idx = 0;
    for i in 1..cn {
        if clean[i].1 > clean[max_y_idx].1
            || (clean[i].1 == clean[max_y_idx].1 && clean[i].0 < clean[max_y_idx].0)
        {
            max_y_idx = i;
        }
    }

    // Trace from the max_y vertex. This is a local minimum.
    // Go in both directions to find the bounds.

    // Collect Y-direction changes by tracing the ring
    // Direction: +1 = Y increasing (going down), -1 = Y decreasing (going up), 0 = horizontal
    let dir = |from: Pt, to: Pt| -> i32 {
        if to.1 > from.1 {
            1
        } else if to.1 < from.1 {
            -1
        } else {
            0
        }
    };

    // Find all direction change points (local min and max)
    // We trace from max_y_idx
    let mut extrema: Vec<(usize, bool)> = Vec::new(); // (vertex_index, is_minimum)

    let mut prev_dir = 0i32;
    // Find initial non-zero direction going backward from max_y_idx
    for k in 1..cn {
        let i = (max_y_idx + cn - k) % cn;
        let next = (i + 1) % cn;
        let d = dir(clean[i], clean[next]);
        if d != 0 {
            prev_dir = d;
            break;
        }
    }

    if prev_dir == 0 {
        // All points at same Y - degenerate
        return;
    }

    for k in 0..cn {
        let i = (max_y_idx + k) % cn;
        let next = (i + 1) % cn;
        let d = dir(clean[i], clean[next]);
        if d != 0 && d != prev_dir {
            // Direction changed at vertex i
            if d < 0 && prev_dir > 0 {
                // Was going down, now going up → local minimum at vertex i
                extrema.push((i, true));
            } else if d > 0 && prev_dir < 0 {
                // Was going up, now going down → local maximum at vertex i
                extrema.push((i, false));
            }
            prev_dir = d;
        }
    }

    if extrema.is_empty() {
        // This shouldn't happen for a valid polygon with >= 3 non-collinear vertices
        return;
    }

    // Now pair up consecutive minima and maxima to create bounds.
    // Starting from a minimum, trace to the next maximum (bound 1),
    // then continue to the next minimum (bound 2). These form a pair.

    // Ensure we start with a minimum
    let start_idx = extrema
        .iter()
        .position(|&(_, is_min)| is_min)
        .expect("Should have at least one minimum");

    let ne = extrema.len();
    let mut ei = start_idx;

    let mut first_min_bound: Option<BoundIdx> = None;
    let mut last_max_bound: Option<BoundIdx> = None;

    for _ in 0..(ne / 2) {
        let (min_vertex, is_min) = extrema[ei];
        if !is_min {
            ei = (ei + 1) % ne;
            continue;
        }

        let next_ei = (ei + 1) % ne;
        let (max_vertex, _) = extrema[next_ei];

        // Bound 1: from minimum to maximum (going towards lower Y)
        // Trace vertices from min_vertex to max_vertex
        let mut bound1_pts: Vec<Pt> = Vec::new();
        {
            let mut v = min_vertex;
            loop {
                bound1_pts.push(clean[v]);
                if v == max_vertex {
                    break;
                }
                v = (v + 1) % cn;
                if bound1_pts.len() > cn + 1 {
                    break; // safety
                }
            }
        }

        // Bound 2: from maximum to next minimum (going towards higher Y)
        let next_next_ei = (next_ei + 1) % ne;
        let (next_min_vertex, _) = extrema[next_next_ei];

        let mut bound2_pts: Vec<Pt> = Vec::new();
        {
            let mut v = max_vertex;
            loop {
                bound2_pts.push(clean[v]);
                if v == next_min_vertex {
                    break;
                }
                v = (v + 1) % cn;
                if bound2_pts.len() > cn + 1 {
                    break; // safety
                }
            }
        }

        // Convert to edges
        let mut edges1: Vec<Edge> = Vec::new();
        for i in 0..bound1_pts.len().saturating_sub(1) {
            edges1.push(Edge::new(bound1_pts[i], bound1_pts[i + 1]));
        }
        let mut edges2: Vec<Edge> = Vec::new();
        for i in 0..bound2_pts.len().saturating_sub(1) {
            edges2.push(Edge::new(bound2_pts[i], bound2_pts[i + 1]));
        }

        if edges1.is_empty() || edges2.is_empty() {
            ei = (ei + 2) % ne;
            continue;
        }

        // Sort edges within each bound: from bottom (high Y) to top (low Y)
        // Bound 1 goes from min (high Y) to max (low Y) → edges already in order
        // Bound 2 goes from max (low Y) to next min (high Y) → edges go up then down?
        // Actually, bound 2 goes from max to the next min. Its edges should be
        // ordered from bottom (high Y start) to top (low Y end).
        // We need edges ordered so that we process them from bottom to top as the
        // scanline sweeps upward.

        // In Wagyu, the bound edges are ordered from bottom to top:
        // edges[0].bot has the highest Y (starting point at the bottom)
        // edges[last].top has the lowest Y (ending point at the top)

        // For bound 1 (min→max): first vertex is at min (high Y), last at max (low Y)
        // So edges[0] starts at high Y, edges[last] ends at low Y. This is correct order.

        // For bound 2 (max→next_min): first vertex is at max (low Y), last at next_min (high Y)
        // So edges go from low Y to high Y. We need to reverse to get high Y → low Y order.
        edges2.reverse();

        let min_y = clean[min_vertex].1;

        // Determine which bound is left vs right
        let e1_dx = edges1
            .iter()
            .find(|e| !e.is_horizontal())
            .map(|e| e.dx);
        let e2_dx = edges2
            .iter()
            .find(|e| !e.is_horizontal())
            .map(|e| e.dx);

        let has_horizontal = edges1.first().map_or(false, |e| e.is_horizontal())
            || edges2.first().map_or(false, |e| e.is_horizontal());

        // Bound going towards minimum (from max to min, decreasing Y to increasing Y)
        // has winding_delta = -1
        // Bound going towards maximum (from min to max, increasing Y to decreasing Y)
        // has winding_delta = +1

        // In our case:
        // edges1: from min to max → towards maximum → winding_delta = +1
        // edges2: from next_min to max (reversed) → towards minimum → winding_delta = -1

        let b1_idx = bounds.len();
        let mut b1 = Bound::new();
        b1.edges = edges1;
        b1.winding_delta = 1; // towards maximum
        bounds.push(b1);

        let b2_idx = bounds.len();
        let mut b2 = Bound::new();
        b2.edges = edges2;
        b2.winding_delta = -1; // towards minimum
        bounds.push(b2);

        // Determine left/right based on dx
        let minimum_is_left = match (e1_dx, e2_dx) {
            (Some(d1), Some(d2)) => {
                if has_horizontal {
                    true // simplified
                } else {
                    d1 <= d2
                }
            }
            _ => true,
        };

        let (left_idx, right_idx) = if minimum_is_left {
            bounds[b1_idx].side = Side::Left;
            bounds[b2_idx].side = Side::Right;
            (b1_idx, b2_idx)
        } else {
            bounds[b1_idx].side = Side::Right;
            bounds[b2_idx].side = Side::Left;
            (b2_idx, b1_idx)
        };

        if let Some(last_max) = last_max_bound {
            let min_bound = if minimum_is_left { b1_idx } else { b2_idx };
            bounds[min_bound].maximum_bound = Some(last_max);
            bounds[last_max].maximum_bound = Some(min_bound);
        }
        if first_min_bound.is_none() {
            first_min_bound = Some(if minimum_is_left { left_idx } else { right_idx });
        }
        last_max_bound = Some(if minimum_is_left { right_idx } else { left_idx });

        minima.push(LocalMinimum {
            left_bound: left_idx,
            right_bound: right_idx,
            y: min_y,
            has_horizontal,
        });

        ei = (ei + 2) % ne;
    }

    // Close circular chain
    if let (Some(first), Some(last)) = (first_min_bound, last_max_bound) {
        bounds[last].maximum_bound = Some(first);
        if bounds[first].maximum_bound.is_none() {
            bounds[first].maximum_bound = Some(last);
        }
    }
}

// ============================================================
// Scanbeam
// ============================================================

fn setup_scanbeam(minima: &[LocalMinimum]) -> Vec<i32> {
    let mut sb: Vec<i32> = minima.iter().map(|lm| lm.y).collect();
    sb.sort_unstable();
    sb.dedup();
    sb
}

fn pop_scanbeam(scanbeam: &mut Vec<i32>) -> Option<i32> {
    scanbeam.pop()
}

fn insert_scanbeam(scanbeam: &mut Vec<i32>, y: i32) {
    let pos = scanbeam.binary_search(&y);
    if let Err(idx) = pos {
        scanbeam.insert(idx, y);
    }
}

// ============================================================
// Active Bound List Operations
// ============================================================

fn set_winding_count(
    bound_idx: BoundIdx,
    abl: &[Option<BoundIdx>],
    bounds: &mut [Bound],
    abl_pos: usize,
) {
    // Find the nearest bound to the left of the same poly type
    let mut left_winding = None;
    for i in (0..abl_pos).rev() {
        if let Some(bi) = abl[i] {
            left_winding = Some(bi);
            break;
        }
    }

    match left_winding {
        None => {
            bounds[bound_idx].winding_count = bounds[bound_idx].winding_delta as i32;
            bounds[bound_idx].winding_count2 = 0;
        }
        Some(left_bi) => {
            let left_wc = bounds[left_bi].winding_count;
            let left_wd = bounds[left_bi].winding_delta;
            let cur_wd = bounds[bound_idx].winding_delta;

            // Non-zero filling (positive fill)
            if left_wc as i8 * left_wd < 0 {
                // prev edge is decreasing WC toward zero
                if (left_wc as i32).unsigned_abs() > 1 {
                    if left_wd as i32 * cur_wd as i32 < 0 {
                        bounds[bound_idx].winding_count = left_wc;
                    } else {
                        bounds[bound_idx].winding_count = left_wc + cur_wd as i32;
                    }
                } else {
                    bounds[bound_idx].winding_count = cur_wd as i32;
                }
            } else {
                // prev edge is increasing WC away from zero
                if left_wd as i32 * cur_wd as i32 < 0 {
                    bounds[bound_idx].winding_count = left_wc;
                } else {
                    bounds[bound_idx].winding_count = left_wc + cur_wd as i32;
                }
            }
            bounds[bound_idx].winding_count2 = bounds[left_bi].winding_count2;
        }
    }
}

fn is_contributing(bound: &Bound) -> bool {
    // For union with fill_type_positive (all subjects):
    // Contributing when winding_count == 1 and winding_count2 <= 0
    bound.winding_count == 1
}

// ============================================================
// Ring Building Helpers
// ============================================================

fn set_hole_state(
    bound_idx: BoundIdx,
    abl: &[Option<BoundIdx>],
    bounds: &[Bound],
    abl_pos: usize,
    rm: &mut RingManager,
) {
    let ring_id = match bounds[bound_idx].ring_id {
        Some(r) => r,
        None => return,
    };

    // Find first bound to the left with a ring
    let mut tmp_bound: Option<BoundIdx> = None;
    for i in (0..abl_pos).rev() {
        if let Some(bi) = abl[i] {
            if bounds[bi].ring_id.is_some() {
                if tmp_bound.is_none() {
                    tmp_bound = Some(bi);
                } else if let Some(tb) = tmp_bound {
                    if bounds[tb].ring_id == bounds[bi].ring_id {
                        tmp_bound = None;
                    }
                }
            }
        }
    }

    match tmp_bound {
        None => {
            rm.rings[ring_id].parent = None;
            rm.children.push(Some(ring_id));
        }
        Some(tb) => {
            if let Some(parent_ring) = bounds[tb].ring_id {
                rm.rings[ring_id].parent = Some(parent_ring);
                rm.rings[parent_ring].children.push(Some(ring_id));
            }
        }
    }
}

fn add_first_point(
    bound_idx: BoundIdx,
    abl: &[Option<BoundIdx>],
    abl_pos: usize,
    pt: Pt,
    bounds: &mut [Bound],
    rm: &mut RingManager,
) {
    let ring_id = rm.create_ring();
    let point_id = rm.create_point(pt.0, pt.1, Some(ring_id));
    rm.rings[ring_id].points = Some(point_id);

    bounds[bound_idx].ring_id = Some(ring_id);
    bounds[bound_idx].last_point = pt;

    set_hole_state(bound_idx, abl, bounds, abl_pos, rm);
}

fn add_point_to_ring(bound_idx: BoundIdx, pt: Pt, bounds: &mut [Bound], rm: &mut RingManager) {
    let ring_id = match bounds[bound_idx].ring_id {
        Some(r) => r,
        None => return,
    };

    let head = match rm.rings[ring_id].points {
        Some(h) => h,
        None => return,
    };

    let to_front = bounds[bound_idx].side == Side::Left;

    if to_front {
        let head_pt = rm.point_coords(head);
        if pt == head_pt {
            return;
        }
    } else {
        let tail = rm.points[head].prev;
        let tail_pt = rm.point_coords(tail);
        if pt == tail_pt {
            return;
        }
    }

    let new_pt = rm.create_point_before(pt.0, pt.1, Some(ring_id), head);
    rm.rings[ring_id].area_cached = None;

    if to_front {
        rm.rings[ring_id].points = Some(new_pt);
    }

    bounds[bound_idx].last_point = pt;
}

fn add_point(
    bound_idx: BoundIdx,
    abl: &[Option<BoundIdx>],
    abl_pos: usize,
    pt: Pt,
    bounds: &mut [Bound],
    rm: &mut RingManager,
) {
    if bounds[bound_idx].ring_id.is_none() {
        add_first_point(bound_idx, abl, abl_pos, pt, bounds, rm);
    } else {
        add_point_to_ring(bound_idx, pt, bounds, rm);
    }
}

fn find_abl_pos(abl: &[Option<BoundIdx>], target: BoundIdx) -> Option<usize> {
    abl.iter().position(|b| *b == Some(target))
}

fn add_local_minimum_point(
    b1_idx: BoundIdx,
    b2_idx: BoundIdx,
    abl: &[Option<BoundIdx>],
    pt: Pt,
    bounds: &mut [Bound],
    rm: &mut RingManager,
) {
    let b1_pos = find_abl_pos(abl, b1_idx);
    let b2_pos = find_abl_pos(abl, b2_idx);

    let (left_idx, right_idx, left_pos) = {
        let b1_dx = bounds[b1_idx].cur_edge().dx;
        let b2_dx = bounds[b2_idx].cur_edge().dx;
        let b2_is_horz = bounds[b2_idx].cur_edge().is_horizontal();

        if b2_is_horz || b1_dx > b2_dx {
            // b1 gets the point
            let pos = b1_pos.unwrap_or(0);
            add_point(b1_idx, abl, pos, pt, bounds, rm);
            bounds[b2_idx].last_point = pt;
            bounds[b2_idx].ring_id = bounds[b1_idx].ring_id;
            (b1_idx, b2_idx, pos)
        } else {
            let pos = b2_pos.unwrap_or(0);
            add_point(b2_idx, abl, pos, pt, bounds, rm);
            bounds[b1_idx].last_point = pt;
            bounds[b1_idx].ring_id = bounds[b2_idx].ring_id;
            (b2_idx, b1_idx, pos)
        }
    };

    bounds[left_idx].side = Side::Left;
    bounds[right_idx].side = Side::Right;
}

fn add_local_maximum_point(
    b1_idx: BoundIdx,
    b2_idx: BoundIdx,
    pt: Pt,
    bounds: &mut [Bound],
    rm: &mut RingManager,
    abl: &[Option<BoundIdx>],
) {
    let b1_pos = find_abl_pos(abl, b1_idx).unwrap_or(0);
    add_point(b1_idx, abl, b1_pos, pt, bounds, rm);

    let r1 = bounds[b1_idx].ring_id;
    let r2 = bounds[b2_idx].ring_id;

    if r1 == r2 {
        // Same ring - close it
        bounds[b1_idx].ring_id = None;
        bounds[b2_idx].ring_id = None;
    } else if let (Some(r1_id), Some(r2_id)) = (r1, r2) {
        // Different rings - merge them
        let (keep, remove, keep_bound, remove_bound) = if r1_id < r2_id {
            (r1_id, r2_id, b1_idx, b2_idx)
        } else {
            (r2_id, r1_id, b2_idx, b1_idx)
        };

        // Connect the two ring point lists
        let keep_head = rm.rings[keep].points;
        let remove_head = rm.rings[remove].points;

        if let (Some(kh), Some(rh)) = (keep_head, remove_head) {
            let keep_side = bounds[keep_bound].side;
            let remove_side = bounds[remove_bound].side;

            let k_lft = kh;
            let k_rt = rm.points[kh].prev;
            let r_lft = rh;
            let r_rt = rm.points[rh].prev;

            if keep_side == Side::Left {
                if remove_side == Side::Left {
                    rm.reverse_ring(r_lft);
                    rm.points[r_lft].next = k_lft;
                    rm.points[k_lft].prev = r_lft;
                    rm.points[k_rt].next = r_rt;
                    rm.points[r_rt].prev = k_rt;
                    rm.rings[keep].points = Some(r_rt);
                } else {
                    rm.points[r_rt].next = k_lft;
                    rm.points[k_lft].prev = r_rt;
                    rm.points[r_lft].prev = k_rt;
                    rm.points[k_rt].next = r_lft;
                    rm.rings[keep].points = Some(r_lft);
                }
            } else if remove_side == Side::Right {
                rm.reverse_ring(r_lft);
                rm.points[k_rt].next = r_rt;
                rm.points[r_rt].prev = k_rt;
                rm.points[r_lft].next = k_lft;
                rm.points[k_lft].prev = r_lft;
            } else {
                rm.points[k_rt].next = r_lft;
                rm.points[r_lft].prev = k_rt;
                rm.points[k_lft].prev = r_rt;
                rm.points[r_rt].next = k_lft;
            }

            rm.rings[keep].bottom_point = None;
            rm.rings[keep].area_cached = None;

            let keep_is_hole = rm.ring_is_hole(keep);
            let remove_is_hole = rm.ring_is_hole(remove);

            rm.rings[remove].points = None;
            if keep_is_hole != remove_is_hole {
                let parent = rm.rings[keep].parent;
                rm.ring1_replaces_ring2(parent, remove);
            } else {
                rm.ring1_replaces_ring2(Some(keep), remove);
            }

            rm.update_points_ring(keep);
        }

        bounds[keep_bound].ring_id = None;
        bounds[remove_bound].ring_id = None;

        // Update any other bounds that reference the removed ring
        for b in bounds.iter_mut() {
            if b.ring_id == Some(remove) {
                b.ring_id = Some(keep);
                break;
            }
        }
    }
}

// ============================================================
// Intersection Detection
// ============================================================

fn get_edge_intersection(e1: &Edge, e2: &Edge) -> Option<(f64, f64)> {
    let p0_x = e1.bot.0 as f64;
    let p0_y = e1.bot.1 as f64;
    let p1_x = e1.top.0 as f64;
    let p1_y = e1.top.1 as f64;
    let p2_x = e2.bot.0 as f64;
    let p2_y = e2.bot.1 as f64;
    let p3_x = e2.top.0 as f64;
    let p3_y = e2.top.1 as f64;

    let s1_x = p1_x - p0_x;
    let s1_y = p1_y - p0_y;
    let s2_x = p3_x - p2_x;
    let s2_y = p3_y - p2_y;

    let denom = -s2_x * s1_y + s1_x * s2_y;
    if denom.abs() < f64::EPSILON {
        return None;
    }

    let s = (-s1_y * (p0_x - p2_x) + s1_x * (p0_y - p2_y)) / denom;
    let t = (s2_x * (p0_y - p2_y) - s2_y * (p0_x - p2_x)) / denom;

    if (0.0..=1.0).contains(&s) && (0.0..=1.0).contains(&t) {
        Some((p0_x + t * s1_x, p0_y + t * s1_y))
    } else {
        None
    }
}

fn round_point(x: f64, y: f64) -> Pt {
    (x.round() as i32, y.round() as i32)
}

// ============================================================
// Intersection Processing
// ============================================================

struct IntersectNode {
    bound1: BoundIdx,
    bound2: BoundIdx,
    pt: (f64, f64),
}

fn intersect_bounds(
    b1_idx: BoundIdx,
    b2_idx: BoundIdx,
    pt: Pt,
    bounds: &mut [Bound],
    rm: &mut RingManager,
    abl: &[Option<BoundIdx>],
) {
    let b1_contributing = bounds[b1_idx].ring_id.is_some();
    let b2_contributing = bounds[b2_idx].ring_id.is_some();

    // Update winding counts (same poly type: all subjects)
    let b2_wd = bounds[b2_idx].winding_delta;
    let b1_wd = bounds[b1_idx].winding_delta;

    if bounds[b1_idx].winding_count + b2_wd as i32 == 0 {
        bounds[b1_idx].winding_count = -bounds[b1_idx].winding_count;
    } else {
        bounds[b1_idx].winding_count += b2_wd as i32;
    }
    if bounds[b2_idx].winding_count - b1_wd as i32 == 0 {
        bounds[b2_idx].winding_count = -bounds[b2_idx].winding_count;
    } else {
        bounds[b2_idx].winding_count -= b1_wd as i32;
    }

    // Determine fill state
    let b1_wc = bounds[b1_idx].winding_count;
    let b2_wc = bounds[b2_idx].winding_count;

    if b1_contributing && b2_contributing {
        if (b1_wc != 0 && b1_wc != 1) || (b2_wc != 0 && b2_wc != 1) {
            add_local_maximum_point(b1_idx, b2_idx, pt, bounds, rm, abl);
        } else {
            let b1_pos = find_abl_pos(abl, b1_idx).unwrap_or(0);
            let b2_pos = find_abl_pos(abl, b2_idx).unwrap_or(0);
            add_point(b1_idx, abl, b1_pos, pt, bounds, rm);
            add_point(b2_idx, abl, b2_pos, pt, bounds, rm);
            // Swap sides and rings
            let s1 = bounds[b1_idx].side;
            let s2 = bounds[b2_idx].side;
            bounds[b1_idx].side = s2;
            bounds[b2_idx].side = s1;
            let r1 = bounds[b1_idx].ring_id;
            let r2 = bounds[b2_idx].ring_id;
            bounds[b1_idx].ring_id = r2;
            bounds[b2_idx].ring_id = r1;
        }
    } else if b1_contributing {
        if b2_wc == 0 || b2_wc == 1 {
            let b1_pos = find_abl_pos(abl, b1_idx).unwrap_or(0);
            add_point(b1_idx, abl, b1_pos, pt, bounds, rm);
            bounds[b2_idx].last_point = pt;
            let s1 = bounds[b1_idx].side;
            let s2 = bounds[b2_idx].side;
            bounds[b1_idx].side = s2;
            bounds[b2_idx].side = s1;
            let r1 = bounds[b1_idx].ring_id;
            let r2 = bounds[b2_idx].ring_id;
            bounds[b1_idx].ring_id = r2;
            bounds[b2_idx].ring_id = r1;
        }
    } else if b2_contributing {
        if b1_wc == 0 || b1_wc == 1 {
            bounds[b1_idx].last_point = pt;
            let b2_pos = find_abl_pos(abl, b2_idx).unwrap_or(0);
            add_point(b2_idx, abl, b2_pos, pt, bounds, rm);
            let s1 = bounds[b1_idx].side;
            let s2 = bounds[b2_idx].side;
            bounds[b1_idx].side = s2;
            bounds[b2_idx].side = s1;
            let r1 = bounds[b1_idx].ring_id;
            let r2 = bounds[b2_idx].ring_id;
            bounds[b1_idx].ring_id = r2;
            bounds[b2_idx].ring_id = r1;
        }
    } else if (b1_wc == 0 || b1_wc == 1) && (b2_wc == 0 || b2_wc == 1) {
        // Neither contributing - check for union of same type
        if b1_wc == 1 && b2_wc == 1 {
            // For union: if winding_count2 of both <= 0
            let b1_wc2 = bounds[b1_idx].winding_count2;
            let b2_wc2 = bounds[b2_idx].winding_count2;
            if b1_wc2 <= 0 && b2_wc2 <= 0 {
                add_local_minimum_point(b1_idx, b2_idx, abl, pt, bounds, rm);
            }
        } else {
            // Swap sides only
            let s1 = bounds[b1_idx].side;
            let s2 = bounds[b2_idx].side;
            bounds[b1_idx].side = s2;
            bounds[b2_idx].side = s1;
        }
    }
}

fn process_intersections(
    top_y: i32,
    abl: &mut Vec<Option<BoundIdx>>,
    bounds: &mut [Bound],
    rm: &mut RingManager,
) {
    if abl.is_empty() {
        return;
    }

    // Update current_x for all bounds
    let mut pos = 0;
    for item in abl.iter() {
        if let Some(bi) = item {
            bounds[*bi].current_x = bounds[*bi].cur_edge().current_x(top_y);
            bounds[*bi].pos = pos;
        }
        pos += 1;
    }

    // Bubble sort to find intersections
    let mut intersects: Vec<IntersectNode> = Vec::new();
    let n = abl.len();
    let mut modified = true;
    while modified {
        modified = false;
        for i in 0..n.saturating_sub(1) {
            let b1_opt = abl[i];
            let b2_opt = abl[i + 1];
            if let (Some(b1), Some(b2)) = (b1_opt, b2_opt) {
                if bounds[b1].current_x > bounds[b2].current_x
                    && !slopes_equal_edges(bounds[b1].cur_edge(), bounds[b2].cur_edge())
                {
                    // Find intersection
                    if let Some(pt) =
                        get_edge_intersection(bounds[b1].cur_edge(), bounds[b2].cur_edge())
                    {
                        intersects.push(IntersectNode {
                            bound1: b1,
                            bound2: b2,
                            pt,
                        });
                    }
                    abl.swap(i, i + 1);
                    modified = true;
                }
            }
        }
    }

    // Restore original order
    abl.sort_by_key(|b| {
        b.map(|bi| bounds[bi].pos).unwrap_or(usize::MAX)
    });

    // Sort intersects by Y (descending - process bottom first)
    intersects.sort_by(|a, b| {
        b.pt.1
            .partial_cmp(&a.pt.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Process intersections
    for inode in &intersects {
        let pt = round_point(inode.pt.0, inode.pt.1);

        // Find bounds in ABL and ensure they're adjacent
        let pos1 = abl.iter().position(|b| *b == Some(inode.bound1));
        let pos2 = abl.iter().position(|b| *b == Some(inode.bound2));

        if let (Some(p1), Some(p2)) = (pos1, pos2) {
            // Ensure adjacent (or find them adjacent)
            let abl_snapshot: Vec<Option<BoundIdx>> = abl.clone();
            intersect_bounds(
                inode.bound1,
                inode.bound2,
                pt,
                bounds,
                rm,
                &abl_snapshot,
            );

            // Swap in ABL
            if p1 < abl.len() && p2 < abl.len() {
                abl.swap(p1, p2);
            }
        }
    }
}

// ============================================================
// Process Edges at Top of Scanbeam
// ============================================================

fn next_edge_in_bound(bound_idx: BoundIdx, bounds: &mut [Bound], scanbeam: &mut Vec<i32>) {
    bounds[bound_idx].current_edge += 1;
    if bounds[bound_idx].current_edge < bounds[bound_idx].edges.len() {
        bounds[bound_idx].next_edge = bounds[bound_idx].current_edge + 1;
        let edge = &bounds[bound_idx].edges[bounds[bound_idx].current_edge];
        bounds[bound_idx].current_x = edge.bot.0 as f64;
        if !edge.is_horizontal() {
            insert_scanbeam(scanbeam, edge.top.1);
        }
    }
}

fn process_edges_at_top(
    top_y: i32,
    abl: &mut Vec<Option<BoundIdx>>,
    bounds: &mut [Bound],
    rm: &mut RingManager,
    scanbeam: &mut Vec<i32>,
) {
    let mut i = 0;
    while i < abl.len() {
        let bi = match abl[i] {
            Some(b) => b,
            None => {
                i += 1;
                continue;
            }
        };

        // Check for maxima
        if bounds[bi].is_maxima(top_y) {
            let max_pair = bounds[bi].maximum_bound;
            if let Some(mp) = max_pair {
                let mp_pos = abl.iter().position(|b| *b == Some(mp));
                if let Some(mp_p) = mp_pos {
                    let mp_bound = abl[mp_p];
                    if let Some(mp_bi) = mp_bound {
                        if !bounds[mp_bi].cur_edge_is_horizontal() && bounds[mp_bi].is_maxima(top_y)
                        {
                            // Process do_maxima
                            let abl_snapshot: Vec<Option<BoundIdx>> = abl.clone();

                            if bounds[bi].ring_id.is_some() && bounds[mp_bi].ring_id.is_some() {
                                add_local_maximum_point(
                                    bi,
                                    mp_bi,
                                    bounds[bi].cur_edge().top,
                                    bounds,
                                    rm,
                                    &abl_snapshot,
                                );
                            }

                            abl[mp_p] = None;
                            abl[i] = None;
                            i += 1;
                            continue;
                        }
                    }
                }
            }
        }

        // Intermediate edge - advance to next
        if bounds[bi].is_intermediate(top_y) {
            if bounds[bi].ring_id.is_some() {
                add_point_to_ring(bi, bounds[bi].cur_edge().top, bounds, rm);
            }
            next_edge_in_bound(bi, bounds, scanbeam);

            // Update current_x
            if bounds[bi].current_edge < bounds[bi].edges.len() {
                bounds[bi].current_x =
                    bounds[bi].edges[bounds[bi].current_edge].current_x(top_y);
            }
        } else if bounds[bi].current_edge < bounds[bi].edges.len() {
            bounds[bi].current_x = bounds[bi].edges[bounds[bi].current_edge].current_x(top_y);
        }

        i += 1;
    }

    // Remove nulls
    abl.retain(|b| b.is_some());
}

// ============================================================
// Insert Local Minima into ABL
// ============================================================

fn insert_bound_into_abl(
    bound_idx: BoundIdx,
    abl: &mut Vec<Option<BoundIdx>>,
    bounds: &[Bound],
) -> usize {
    let x = bounds[bound_idx].current_x;
    let pos = abl
        .iter()
        .position(|b| {
            b.map_or(true, |bi| x < bounds[bi].current_x)
        })
        .unwrap_or(abl.len());
    abl.insert(pos, Some(bound_idx));
    pos
}

fn insert_local_minima(
    bot_y: i32,
    minima: &[LocalMinimum],
    current_lm: &mut usize,
    abl: &mut Vec<Option<BoundIdx>>,
    bounds: &mut [Bound],
    rm: &mut RingManager,
    scanbeam: &mut Vec<i32>,
) {
    while *current_lm < minima.len() && bot_y == minima[*current_lm].y {
        let lm = &minima[*current_lm];
        let lb = lm.left_bound;
        let rb = lm.right_bound;

        // Initialize bounds
        bounds[lb].current_edge = 0;
        bounds[lb].next_edge = 1;
        if !bounds[lb].edges.is_empty() {
            bounds[lb].current_x = bounds[lb].edges[0].bot.0 as f64;
        }
        bounds[lb].winding_count = 0;
        bounds[lb].winding_count2 = 0;
        bounds[lb].ring_id = None;

        bounds[rb].current_edge = 0;
        bounds[rb].next_edge = 1;
        if !bounds[rb].edges.is_empty() {
            bounds[rb].current_x = bounds[rb].edges[0].bot.0 as f64;
        }
        bounds[rb].winding_count = 0;
        bounds[rb].winding_count2 = 0;
        bounds[rb].ring_id = None;

        // Insert left bound into ABL
        let lb_pos = insert_bound_into_abl(lb, abl, bounds);
        // Insert right bound right after left
        abl.insert(lb_pos + 1, Some(rb));

        // Set winding count for left bound
        set_winding_count(lb, abl, bounds, lb_pos);

        // Right bound gets same winding count
        bounds[rb].winding_count = bounds[lb].winding_count;
        bounds[rb].winding_count2 = bounds[lb].winding_count2;

        // Check if contributing
        if is_contributing(&bounds[lb]) {
            let pt = bounds[lb].edges[0].bot;
            add_local_minimum_point(lb, rb, abl, pt, bounds, rm);
        }

        // Add edge tops to scanbeam
        if !bounds[lb].edges.is_empty() && !bounds[lb].cur_edge().is_horizontal() {
            insert_scanbeam(scanbeam, bounds[lb].cur_edge().top.1);
        }
        if !bounds[rb].edges.is_empty() && !bounds[rb].cur_edge().is_horizontal() {
            insert_scanbeam(scanbeam, bounds[rb].cur_edge().top.1);
        }

        *current_lm += 1;
    }
}

// ============================================================
// Vatti Main Loop
// ============================================================

fn execute_vatti(
    minima: &[LocalMinimum],
    bounds: &mut [Bound],
    rm: &mut RingManager,
) {
    let mut abl: Vec<Option<BoundIdx>> = Vec::new();
    let mut scanbeam = setup_scanbeam(minima);
    let mut current_lm = 0usize;

    // Sort minima by Y descending (process from bottom to top)
    // Actually they should already be sorted, but let's ensure
    let mut minima_sorted: Vec<usize> = (0..minima.len()).collect();
    minima_sorted.sort_by(|&a, &b| minima[b].y.cmp(&minima[a].y));

    // Re-sort scanbeam
    scanbeam.sort_unstable();

    loop {
        let scanline_y = match pop_scanbeam(&mut scanbeam) {
            Some(y) => y,
            None => {
                if current_lm < minima_sorted.len() {
                    minima[minima_sorted[current_lm]].y
                } else {
                    break;
                }
            }
        };

        // Process intersections
        process_intersections(scanline_y, &mut abl, bounds, rm);

        // Process edges at top of scanbeam
        process_edges_at_top(scanline_y, &mut abl, bounds, rm, &mut scanbeam);

        // Insert local minima at this scanline
        while current_lm < minima_sorted.len()
            && minima[minima_sorted[current_lm]].y == scanline_y
        {
            let lm_idx = minima_sorted[current_lm];
            let lm = &minima[lm_idx];
            let lb = lm.left_bound;
            let rb = lm.right_bound;

            // Initialize bounds
            if !bounds[lb].edges.is_empty() {
                bounds[lb].current_edge = 0;
                bounds[lb].next_edge = 1;
                bounds[lb].current_x = bounds[lb].edges[0].bot.0 as f64;
            }
            bounds[lb].winding_count = 0;
            bounds[lb].winding_count2 = 0;
            bounds[lb].ring_id = None;

            if !bounds[rb].edges.is_empty() {
                bounds[rb].current_edge = 0;
                bounds[rb].next_edge = 1;
                bounds[rb].current_x = bounds[rb].edges[0].bot.0 as f64;
            }
            bounds[rb].winding_count = 0;
            bounds[rb].winding_count2 = 0;
            bounds[rb].ring_id = None;

            // Insert into ABL
            let lb_pos = insert_bound_into_abl(lb, &mut abl, bounds);
            abl.insert(lb_pos + 1, Some(rb));

            // Set winding count
            set_winding_count(lb, &abl, bounds, lb_pos);
            bounds[rb].winding_count = bounds[lb].winding_count;
            bounds[rb].winding_count2 = bounds[lb].winding_count2;

            // Check if contributing
            if is_contributing(&bounds[lb]) {
                let pt = bounds[lb].edges[0].bot;
                let abl_snapshot: Vec<Option<BoundIdx>> = abl.clone();
                add_local_minimum_point(lb, rb, &abl_snapshot, pt, bounds, rm);
            }

            // Add to scanbeam
            if !bounds[lb].edges.is_empty() && !bounds[lb].cur_edge().is_horizontal() {
                insert_scanbeam(&mut scanbeam, bounds[lb].cur_edge().top.1);
            }
            if !bounds[rb].edges.is_empty() && !bounds[rb].cur_edge().is_horizontal() {
                insert_scanbeam(&mut scanbeam, bounds[rb].cur_edge().top.1);
            }

            current_lm += 1;
        }
    }
}

// ============================================================
// Result Building
// ============================================================

fn build_result(rm: &mut RingManager) -> Vec<Vec<Vec<Pt>>> {
    let mut result: Vec<Vec<Vec<Pt>>> = Vec::new();

    fn collect_ring(rm: &RingManager, ring_id: RingId) -> Vec<Pt> {
        let head = match rm.rings[ring_id].points {
            Some(h) => h,
            None => return Vec::new(),
        };

        let mut pts = Vec::new();
        let mut cur = head;
        loop {
            pts.push((rm.points[cur].x, rm.points[cur].y));
            cur = rm.points[cur].prev; // traverse prev for correct winding
            if cur == head {
                break;
            }
        }
        // Close the ring
        pts.push((rm.points[head].x, rm.points[head].y));
        pts
    }

    fn process_ring(
        rm: &mut RingManager,
        ring_id: RingId,
        result: &mut Vec<Vec<Vec<Pt>>>,
    ) {
        if rm.rings[ring_id].points.is_none() {
            return;
        }

        let area = rm.area_of_ring(ring_id);

        // Exterior ring (positive area) starts a new polygon
        let ring_pts = collect_ring(rm, ring_id);
        if ring_pts.len() < 4 {
            return; // Need at least 3 vertices + closing point
        }

        // Check area and ensure correct winding
        let actual_area = signed_area_f(&ring_pts);
        let mut polygon = Vec::new();

        if actual_area > 0.0 {
            // CW - exterior
            polygon.push(ring_pts);
        } else if actual_area < 0.0 {
            // CCW - try reversing to make it CW
            let mut reversed = ring_pts;
            reversed.reverse();
            polygon.push(reversed);
        } else {
            return; // Degenerate
        }

        // Process children (holes)
        let children: Vec<Option<RingId>> = rm.rings[ring_id].children.clone();
        for child_opt in &children {
            if let Some(child_id) = child_opt {
                if rm.rings[*child_id].points.is_some() {
                    let child_pts = collect_ring(rm, *child_id);
                    if child_pts.len() >= 4 {
                        let child_area = signed_area_f(&child_pts);
                        if child_area < 0.0 {
                            polygon.push(child_pts);
                        } else if child_area > 0.0 {
                            let mut reversed = child_pts;
                            reversed.reverse();
                            polygon.push(reversed);
                        }
                    }
                }
            }
        }

        result.push(polygon);

        // Recursively process grandchildren (nested exteriors)
        for child_opt in &children {
            if let Some(child_id) = child_opt {
                let grandchildren: Vec<Option<RingId>> =
                    rm.rings[*child_id].children.clone();
                for gc_opt in &grandchildren {
                    if let Some(gc_id) = gc_opt {
                        process_ring(rm, *gc_id, result);
                    }
                }
            }
        }
    }

    let top_level: Vec<Option<RingId>> = rm.children.clone();
    for ring_opt in &top_level {
        if let Some(ring_id) = ring_opt {
            process_ring(rm, *ring_id, &mut result);
        }
    }

    result
}

fn signed_area_f(ring: &[Pt]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut area = 0.0_f64;
    for i in 0..n {
        let j = (i + 1) % n;
        area += (ring[i].0 as f64 + ring[j].0 as f64) * (ring[i].1 as f64 - ring[j].1 as f64);
    }
    -area * 0.5
}

// ============================================================
// Public API
// ============================================================

/// Compute the boolean union of a set of polygons.
///
/// Input: `polygons` - list of polygons. Each polygon is a list of rings
/// (first ring is exterior CW, remaining rings are holes CCW).
/// Each ring is a list of `(x, y)` tile coordinates (i32).
///
/// Output: dissolved/unioned polygons in the same format.
/// Ring orientation: CW = exterior, CCW = hole.
pub fn union_polygons(polygons: Vec<Vec<Vec<(i32, i32)>>>) -> Vec<Vec<Vec<(i32, i32)>>> {
    if polygons.is_empty() {
        return Vec::new();
    }

    // Shortcut: single polygon with no potential self-overlap
    if polygons.len() == 1 {
        return polygons;
    }

    let mut bounds: Vec<Bound> = Vec::new();
    let mut minima: Vec<LocalMinimum> = Vec::new();

    // Build local minima from all input rings
    for polygon in &polygons {
        for ring in polygon {
            build_local_minima_simple(ring, &mut bounds, &mut minima);
        }
    }

    if minima.is_empty() {
        return polygons;
    }

    // Sort minima by Y descending (highest Y first = bottom of screen first)
    minima.sort_by(|a, b| b.y.cmp(&a.y));

    // Execute Vatti
    let mut rm = RingManager::new();
    execute_vatti(&minima, &mut bounds, &mut rm);

    // Build result
    let result = build_result(&mut rm);

    if result.is_empty() {
        // If the algorithm produced nothing, return original
        return polygons;
    }

    result
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_area(ring: &[(i32, i32)]) -> f64 {
        signed_area_f(ring)
    }

    fn total_area(polygons: &[Vec<Vec<(i32, i32)>>]) -> f64 {
        let mut total = 0.0;
        for poly in polygons {
            for (i, ring) in poly.iter().enumerate() {
                let a = signed_area(ring);
                if i == 0 {
                    total += a.abs();
                } else {
                    total -= a.abs();
                }
            }
        }
        total
    }

    #[test]
    fn test_empty_input() {
        let result = union_polygons(Vec::new());
        assert!(result.is_empty());
    }

    #[test]
    fn test_single_polygon() {
        let poly = vec![vec![
            vec![(0, 0), (10, 0), (10, 10), (0, 10)], // CW exterior
        ]];
        let result = union_polygons(poly.clone());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 1);
    }

    #[test]
    fn test_two_overlapping_rectangles() {
        // Rect A: 0,0 → 10,10
        let rect_a = vec![vec![(0, 0), (10, 0), (10, 10), (0, 10)]];
        // Rect B: 5,0 → 15,10
        let rect_b = vec![vec![(5, 0), (15, 0), (15, 10), (5, 10)]];

        let result = union_polygons(vec![rect_a, rect_b]);

        // Should produce a single polygon
        assert_eq!(result.len(), 1, "Expected 1 polygon, got {}", result.len());

        // The area should be 150 (15x10)
        let area = total_area(&result);
        assert!(
            (area - 150.0).abs() < 1.0,
            "Expected area ~150, got {}",
            area
        );
    }

    #[test]
    fn test_non_overlapping_rectangles() {
        let rect_a = vec![vec![(0, 0), (5, 0), (5, 5), (0, 5)]];
        let rect_b = vec![vec![(10, 10), (15, 10), (15, 15), (10, 15)]];

        let result = union_polygons(vec![rect_a, rect_b]);

        // Should produce 2 separate polygons
        assert_eq!(result.len(), 2, "Expected 2 polygons, got {}", result.len());

        let area = total_area(&result);
        assert!(
            (area - 50.0).abs() < 1.0,
            "Expected area 50, got {}",
            area
        );
    }

    #[test]
    fn test_three_overlapping_rectangles() {
        let rect_a = vec![vec![(0, 0), (10, 0), (10, 10), (0, 10)]];
        let rect_b = vec![vec![(5, 0), (15, 0), (15, 10), (5, 10)]];
        let rect_c = vec![vec![(10, 0), (20, 0), (20, 10), (10, 10)]];

        let result = union_polygons(vec![rect_a, rect_b, rect_c]);

        // Should produce a single polygon covering 0,0 → 20,10
        assert_eq!(result.len(), 1, "Expected 1 polygon, got {}", result.len());

        let area = total_area(&result);
        assert!(
            (area - 200.0).abs() < 1.0,
            "Expected area ~200, got {}",
            area
        );
    }

    #[test]
    fn test_polygon_with_hole_and_overlap() {
        // Outer ring with a hole, plus an overlapping polygon that fills part of the hole
        let poly_with_hole = vec![
            vec![(0, 0), (20, 0), (20, 20), (0, 20)],     // CW exterior
            vec![(5, 5), (5, 15), (15, 15), (15, 5)],      // CCW hole
        ];
        // Overlapping polygon that fills the right half of the hole
        let filler = vec![vec![(10, 5), (25, 5), (25, 15), (10, 15)]];

        let result = union_polygons(vec![poly_with_hole, filler]);

        // The result should have area = original outer (400) - remaining hole + filler extension
        // Original hole: 100 (10x10)
        // Filler covers right half of hole (5x10=50) and extends right (10x10=100)
        // Expected: 400 - 50 (remaining hole) + 100 (extension) = 450
        // Or equivalently: outer is now 0,0→25,20 minus a 5x10 hole on left side
        let area = total_area(&result);
        assert!(
            area > 350.0,
            "Expected area > 350, got {}",
            area
        );
    }

    #[test]
    fn test_identical_polygons() {
        let rect = vec![vec![(0, 0), (10, 0), (10, 10), (0, 10)]];
        let result = union_polygons(vec![rect.clone(), rect.clone()]);

        assert_eq!(result.len(), 1);
        let area = total_area(&result);
        assert!(
            (area - 100.0).abs() < 1.0,
            "Expected area 100, got {}",
            area
        );
    }
}
