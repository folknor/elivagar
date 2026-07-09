use rustc_hash::FxHashMap;

use super::{
    GeomType, LayerBuilder, LineMergeScratch, MergeScratch, command, decode_zigzag, zigzag,
};

/// Append a source MVT geometry command stream to a destination buffer,
/// adjusting delta encoding so the commands are relative to the running
/// cursor (`cx`, `cy`). This allows multiple independently-encoded
/// geometries to be concatenated into a valid multi-geometry.
fn append_geometry(dest: &mut Vec<u32>, src: &[u32], cx: &mut i32, cy: &mut i32) {
    // The source feature was encoded assuming cursor starts at (0,0).
    // Track the source's absolute cursor so we can re-encode deltas
    // relative to our running destination cursor.
    let mut src_cx: i32 = 0;
    let mut src_cy: i32 = 0;
    let mut i = 0;
    while i < src.len() {
        let cmd = src[i];
        let cmd_id = cmd & 0x7;
        let cmd_count = cmd >> 3;
        i += 1;

        match cmd_id {
            1 | 2 => {
                // MoveTo or LineTo
                let cmd_pos = dest.len();
                dest.push(cmd);
                let mut actual_count = 0u32;
                for _ in 0..cmd_count {
                    if i + 1 >= src.len() {
                        break;
                    }
                    actual_count += 1;
                    let dx = decode_zigzag(src[i]);
                    let dy = decode_zigzag(src[i + 1]);
                    // Absolute position in source coordinate space
                    src_cx += dx;
                    src_cy += dy;
                    // Delta relative to our running cursor
                    dest.push(zigzag(src_cx - *cx));
                    dest.push(zigzag(src_cy - *cy));
                    *cx = src_cx;
                    *cy = src_cy;
                    i += 2;
                }
                // Patch command header with actual count if truncated.
                if actual_count != cmd_count {
                    dest[cmd_pos] = (actual_count << 3) | cmd_id;
                }
            }
            7 => {
                // ClosePath - per MVT spec 4.3.3.3 the cursor does NOT move
                // (it stays at the last LineTo vertex), in both the source
                // and destination coordinate spaces.
                dest.push(cmd);
            }
            _ => {
                // Unknown command, copy as-is
                dest.push(cmd);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-geometry merging
// ---------------------------------------------------------------------------

impl LayerBuilder {
    /// Merge features that share the same geometry type and identical
    /// attribute tags into a single multi-geometry feature. This reduces
    /// feature counts in the encoded tile without losing any visual
    /// information. Point features are skipped (not merged).
    ///
    /// Uses sort + scan instead of HashMap to avoid per-feature tag cloning.
    /// Sort indices by (geom_type, tags), scan for consecutive runs, merge
    /// each run in-place via scratch buffer + swap. Tombstones secondaries
    /// with empty geometry, then retains non-tombstone features.
    #[hotpath::measure]
    pub fn merge_same_attr_geometries(
        &mut self,
        scratch: &mut MergeScratch,
        geom_pool: &mut Vec<Vec<u32>>,
        tags_pool: &mut Vec<Vec<(u16, u16)>>,
    ) {
        // Build sorted index of non-Point features.
        scratch.indices.clear();
        for (i, f) in self.features.iter().enumerate() {
            if f.geom_type != GeomType::Point {
                scratch.indices.push(i);
            }
        }
        if scratch.indices.len() < 2 {
            return;
        }

        // Sort by (geom_type, tags). Tags are deterministic from shortbread
        // matching - no normalization needed. Rust's sort is stable (Timsort).
        scratch.indices.sort_by(|&a, &b| {
            let fa = &self.features[a];
            let fb = &self.features[b];
            fa.geom_type
                .cmp(&fb.geom_type)
                .then_with(|| fa.tags.cmp(&fb.tags))
        });

        // Scan for consecutive runs and merge each run in-place.
        let mut any_merged = false;
        let mut i = 0;
        while i < scratch.indices.len() {
            let mut j = i + 1;
            let fi = scratch.indices[i];
            while j < scratch.indices.len() {
                let fj = scratch.indices[j];
                if self.features[fj].geom_type != self.features[fi].geom_type
                    || self.features[fj].tags != self.features[fi].tags
                {
                    break;
                }
                j += 1;
            }
            if j - i >= 2 {
                any_merged = true;
                // Concatenate all geometries into scratch buffer
                scratch.geom.clear();
                let mut cx: i32 = 0;
                let mut cy: i32 = 0;
                for k in i..j {
                    let idx = scratch.indices[k];
                    append_geometry(
                        &mut scratch.geom,
                        &self.features[idx].geometry,
                        &mut cx,
                        &mut cy,
                    );
                }
                // Copy the merged stream into a pooled Vec instead of
                // donating scratch.geom via swap: the swap left the scratch
                // to regrow through a doubling-realloc chain on every merged
                // run (310K runs / 3.8 GB exclusive churn on the denmark
                // alloc profile). The copy keeps scratch.geom at high-water
                // capacity forever and pooled destinations warm alongside it.
                let first = scratch.indices[i];
                let mut dest = geom_pool.pop().unwrap_or_default();
                dest.clear();
                dest.extend_from_slice(&scratch.geom);
                let old = std::mem::replace(&mut self.features[first].geometry, dest);
                geom_pool.push(old);
                self.features[first].id = None;
                // Reclaim secondary features' Vecs into pools (mem::take leaves
                // zero-capacity Vecs so retain can identify dead features).
                for k in (i + 1)..j {
                    let idx = scratch.indices[k];
                    geom_pool.push(std::mem::take(&mut self.features[idx].geometry));
                    tags_pool.push(std::mem::take(&mut self.features[idx].tags));
                }
            }
            i = j;
        }

        if any_merged {
            // Remove dead features (zero-capacity Vecs from mem::take).
            // Uses is_empty() as tombstone proxy - safe because all pipeline-emitted
            // features have non-empty geometry (enforced at all emit call sites).
            self.features.retain(|f| !f.geometry.is_empty());
        }
    }

    /// Merge connected LineString segments within each line feature.
    ///
    /// After `merge_same_attr_geometries`, each line feature may contain multiple
    /// sub-linestrings (MoveTo/LineTo sequences). This pass joins segments that
    /// share endpoints through degree-2 nodes (not junctions), reducing feature
    /// complexity and improving gzip compression.
    pub fn merge_connected_lines(&mut self, scratch: &mut LineMergeScratch) {
        for feature in &mut self.features {
            if feature.geom_type != GeomType::LineString {
                continue;
            }
            decode_line_segments(&feature.geometry, &mut scratch.segments);
            if scratch.segments.len() < 2 {
                continue;
            }
            merge_line_segments(
                &mut scratch.segments,
                &mut scratch.merged,
                &mut scratch.visited,
                &mut scratch.starts,
                &mut scratch.chain,
            );
            encode_line_segments(&scratch.merged, &mut scratch.encode_buf);
            std::mem::swap(&mut feature.geometry, &mut scratch.encode_buf);
        }
    }
}

// ---------------------------------------------------------------------------
// Line segment merging
// ---------------------------------------------------------------------------

/// Maximum vertex count for a merged linestring. Prevents pathological cases
/// from blowing up tile size. When exceeded, the current segment is finished
/// (no mid-segment truncation) and a new chain starts.
const MAX_LINE_VERTICES: usize = 6000;

/// Decode MVT line geometry commands into absolute-coordinate segments.
fn decode_line_segments(commands: &[u32], segments: &mut Vec<Vec<(i32, i32)>>) {
    segments.clear();
    let mut i = 0;
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    while i < commands.len() {
        let cmd = commands[i];
        let cmd_id = cmd & 0x7;
        let count = (cmd >> 3) as usize;
        i += 1;
        match cmd_id {
            1 => {
                // MoveTo: start a new segment
                if i + count * 2 > commands.len() {
                    break;
                }
                for _ in 0..count {
                    cx = cx.wrapping_add(unzigzag(commands[i]));
                    cy = cy.wrapping_add(unzigzag(commands[i + 1]));
                    i += 2;
                }
                segments.push(vec![(cx, cy)]);
            }
            2 => {
                // LineTo: extend current segment
                if i + count * 2 > commands.len() {
                    break;
                }
                if let Some(seg) = segments.last_mut() {
                    for _ in 0..count {
                        cx = cx.wrapping_add(unzigzag(commands[i]));
                        cy = cy.wrapping_add(unzigzag(commands[i + 1]));
                        i += 2;
                        seg.push((cx, cy));
                    }
                } else {
                    i += count * 2;
                }
            }
            _ => {
                // Unknown command - skip
                i += count * 2;
            }
        }
    }
    // Drop degenerate segments (< 2 points)
    segments.retain(|s| s.len() >= 2);
}

/// Re-encode absolute-coordinate segments as MVT line geometry commands.
fn encode_line_segments(segments: &[Vec<(i32, i32)>], buf: &mut Vec<u32>) {
    buf.clear();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    for seg in segments {
        if seg.len() < 2 {
            continue;
        }
        // MoveTo first point
        buf.push(command(1, 1));
        buf.push(zigzag(seg[0].0 - cx));
        buf.push(zigzag(seg[0].1 - cy));
        cx = seg[0].0;
        cy = seg[0].1;
        // LineTo remaining, skipping consecutive duplicates
        let lineto_pos = buf.len();
        buf.push(0); // placeholder
        let mut count = 0u32;
        for &(x, y) in &seg[1..] {
            if x == cx && y == cy {
                continue;
            }
            buf.push(zigzag(x - cx));
            buf.push(zigzag(y - cy));
            cx = x;
            cy = y;
            count += 1;
        }
        if count < 1 {
            // Degenerate after dedup
            buf.truncate(lineto_pos - 3);
            continue;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            buf[lineto_pos] = command(2, count);
        }
    }
}

/// Which end of a segment participates at an endpoint.
#[derive(Clone, Copy)]
struct SegEnd {
    seg_idx: usize,
    is_back: bool,
}

/// Merge connected line segments through degree-2 nodes.
///
/// Pass 1: build chains starting from degree != 2 endpoints (dead ends, junctions).
/// Pass 2: collect remaining unvisited segments as pure cycles.
/// Traversal order is deterministic (sorted start points and candidate indices).
fn merge_line_segments(
    segments: &mut Vec<Vec<(i32, i32)>>,
    merged: &mut Vec<Vec<(i32, i32)>>,
    visited: &mut Vec<bool>,
    starts: &mut Vec<(i32, i32, usize, bool)>,
    chain: &mut Vec<(i32, i32)>,
) {
    merged.clear();
    if segments.len() < 2 {
        std::mem::swap(segments, merged);
        return;
    }

    // Build endpoint graph
    let mut endpoints: FxHashMap<(i32, i32), Vec<SegEnd>> = FxHashMap::default();
    for (i, seg) in segments.iter().enumerate() {
        let front = seg[0];
        let back = seg[seg.len() - 1];
        endpoints.entry(front).or_default().push(SegEnd {
            seg_idx: i,
            is_back: false,
        });
        endpoints.entry(back).or_default().push(SegEnd {
            seg_idx: i,
            is_back: true,
        });
    }

    visited.clear();
    visited.resize(segments.len(), false);

    // Pass 1: chains starting from degree != 2 endpoints.
    // Sort starts for deterministic output.
    starts.clear();
    for (&point, ends) in &endpoints {
        if ends.len() != 2 {
            for &se in ends {
                starts.push((point.0, point.1, se.seg_idx, se.is_back));
            }
        }
    }
    starts.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(&b.3))
    });

    for &(_, _, seg_idx, is_back) in starts.iter() {
        if visited[seg_idx] {
            continue;
        }
        build_chain(
            segments, &endpoints, visited, chain, seg_idx, is_back, merged,
        );
    }

    // Pass 2: pure cycles (all unvisited segments).
    // Process in segment-index order for determinism.
    for i in 0..segments.len() {
        if visited[i] {
            continue;
        }
        build_chain(segments, &endpoints, visited, chain, i, false, merged);
    }
}

/// Build one chain starting from `seg_idx` entered at `entering_back`.
/// Walks through degree-2 nodes, respecting the vertex cap.
/// Emits one or more chains into `out`.
fn build_chain(
    segments: &[Vec<(i32, i32)>],
    endpoints: &FxHashMap<(i32, i32), Vec<SegEnd>>,
    visited: &mut [bool],
    chain: &mut Vec<(i32, i32)>,
    start_seg: usize,
    entering_back: bool,
    out: &mut Vec<Vec<(i32, i32)>>,
) {
    chain.clear();
    let mut current_seg = start_seg;
    let mut entering_back = entering_back;

    loop {
        if visited[current_seg] {
            break;
        }

        let seg = &segments[current_seg];

        // Vertex cap: finish current segment then stop.
        if !chain.is_empty() && chain.len() + seg.len() > MAX_LINE_VERTICES {
            // Don't mark as visited - will be picked up as a new chain start.
            break;
        }

        visited[current_seg] = true;

        // Append segment points (possibly reversed).
        if entering_back {
            if chain.is_empty() {
                chain.extend(seg.iter().rev());
            } else {
                chain.extend(seg.iter().rev().skip(1));
            }
        } else if chain.is_empty() {
            chain.extend_from_slice(seg);
        } else {
            chain.extend_from_slice(&seg[1..]);
        }

        // Find exit point.
        let exit_point = if entering_back {
            seg[0]
        } else {
            seg[seg.len() - 1]
        };

        // Look for next segment at exit point.
        let Some(ends) = endpoints.get(&exit_point) else {
            break;
        };
        if ends.len() != 2 {
            // Junction or dead end - stop chaining.
            break;
        }

        // Find the other SegEnd (not the one we arrived through).
        // Our exit SegEnd: (current_seg, is_back = !entering_back).
        let our_exit_is_back = !entering_back;
        let other = ends
            .iter()
            .find(|e| !(e.seg_idx == current_seg && e.is_back == our_exit_is_back));
        let Some(&next) = other else {
            // Self-loop: both ends of same segment at same point.
            break;
        };

        // If the "other" is still the same segment (both ends at same point,
        // but different is_back), it's a closed self-loop - stop.
        if next.seg_idx == current_seg {
            break;
        }

        current_seg = next.seg_idx;
        entering_back = next.is_back;
    }

    if chain.len() >= 2 {
        out.push(std::mem::take(chain));
    }
}

#[allow(clippy::cast_possible_wrap)]
fn unzigzag(n: u32) -> i32 {
    ((n >> 1) as i32) ^ (-((n & 1) as i32))
}

// ---------------------------------------------------------------------------
// Test helpers (pub(super) for use in tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(super) fn test_append_geometry(dest: &mut Vec<u32>, src: &[u32], cx: &mut i32, cy: &mut i32) {
    append_geometry(dest, src, cx, cy);
}

#[cfg(test)]
pub(super) fn test_decode_line_segments(commands: &[u32], segments: &mut Vec<Vec<(i32, i32)>>) {
    decode_line_segments(commands, segments);
}
