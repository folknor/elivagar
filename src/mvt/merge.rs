use super::decode::{self, Command, CommandKind, CommandSink, GeometryError, Point, Words};
use super::{GeomType, LayerBuilder, LineMergeScratch, MergeScratch, command, zigzag};
use crate::geometry::line_merge;
use crate::geometry::overlay::IntPoint;

/// Append a source MVT geometry command stream to a destination buffer,
/// adjusting delta encoding so the commands are relative to the running
/// cursor (`cx`, `cy`). This allows multiple independently-encoded
/// geometries to be concatenated into a valid multi-geometry.
///
/// Only the source's first point needs rebasing: after it the source and
/// destination cursors coincide, so every later delta copies through. The
/// source is our own encoder's output, so a stream that does not decode, or a
/// rebased delta outside i32, is an encoder invariant violation.
fn append_geometry(dest: &mut Vec<u32>, src: &[u32], cx: &mut i32, cy: &mut i32) {
    struct Append<'a> {
        dest: &'a mut Vec<u32>,
        cx: &'a mut i32,
        cy: &'a mut i32,
        rebased: bool,
    }
    impl CommandSink for Append<'_> {
        fn command(&mut self, command: Command) -> Result<(), GeometryError> {
            self.dest
                .push(super::command(command.kind.id(), command.count));
            Ok(())
        }
        fn point(&mut self, point: Point) -> Result<(), GeometryError> {
            let (x, y) = point.xy_i32()?;
            if self.rebased {
                self.dest.push(zigzag(point.dx));
                self.dest.push(zigzag(point.dy));
            } else {
                let dx = i32::try_from(i64::from(x) - i64::from(*self.cx))
                    .map_err(|_| GeometryError::CoordinateOverflow("rebased x delta"))?;
                let dy = i32::try_from(i64::from(y) - i64::from(*self.cy))
                    .map_err(|_| GeometryError::CoordinateOverflow("rebased y delta"))?;
                self.dest.push(zigzag(dx));
                self.dest.push(zigzag(dy));
                self.rebased = true;
            }
            *self.cx = x;
            *self.cy = y;
            Ok(())
        }
    }
    let mut sink = Append {
        dest,
        cx,
        cy,
        rebased: false,
    };
    decode::decode(Words::slice(src), None, &mut sink)
        .expect("merged features carry well-formed encoder output");
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
    /// Merged features drop their MVT id; singletons keep theirs. Feature ids
    /// therefore depend on tile-local merging and are not a stable consumer
    /// contract, including for MapLibre feature-state keys.
    ///
    /// Uses sort + scan instead of HashMap to avoid per-feature tag cloning.
    /// Sort indices by (geom_type, tags), scan for consecutive runs, merge
    /// each run in-place via scratch buffer + swap. Tombstones secondaries
    /// with empty geometry, then retains non-tombstone features.
    #[hotpath::measure]
    pub fn merge_same_attr_geometries(&mut self, scratch: &mut MergeScratch) {
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
                // Copy the merged stream into an exact-size Vec instead of
                // donating scratch.geom via swap: the swap left the scratch
                // to regrow through a doubling-realloc chain on every merged
                // run (310K runs / 3.8 GB exclusive churn on a denmark alloc
                // profile). The copy keeps scratch.geom at its high-water
                // capacity for the rest of this tile's runs and layers; the
                // scratch itself lives exactly as long as the tile.
                let first = scratch.indices[i];
                self.features[first].geometry = scratch.geom.clone();
                self.features[first].id = None;
                // Drop secondaries' buffers now rather than at tile end;
                // the emptied geometry is the tombstone `retain` reads.
                for &idx in &scratch.indices[(i + 1)..j] {
                    drop(std::mem::take(&mut self.features[idx].geometry));
                    drop(std::mem::take(&mut self.features[idx].tags));
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
    /// sub-linestrings (MoveTo/LineTo sequences). The geometry module joins segments
    /// through degree-2 nodes while honoring the clipper's integer buffer boundary.
    pub fn merge_connected_lines(&mut self, scratch: &mut LineMergeScratch) {
        for feature in &mut self.features {
            if feature.geom_type != GeomType::LineString {
                continue;
            }
            decode_line_segments(&feature.geometry, &mut scratch.segments);
            if scratch.segments.len() < 2 {
                continue;
            }
            #[cfg(debug_assertions)]
            let input = scratch.segments.clone();
            line_merge::merge_line_segments(&mut scratch.segments, &mut scratch.merge);
            encode_line_segments(&scratch.merge.merged, &mut scratch.encode_buf);
            #[cfg(debug_assertions)]
            {
                let mut decoded = Vec::new();
                decode_line_segments(&scratch.encode_buf, &mut decoded);
                if let Err(violation) = line_merge::check_merge(
                    &input,
                    &scratch.segments,
                    &scratch.merge.merged,
                    &decoded,
                    &scratch.merge.witness,
                    &scratch.merge.chain_ends,
                ) {
                    panic!("line merge invariant violated: {violation}");
                }
            }
            std::mem::swap(&mut feature.geometry, &mut scratch.encode_buf);
        }
    }
}

/// Decode MVT line geometry commands into absolute-coordinate segments,
/// dropping segments of fewer than 2 points. The stream is our own encoder's
/// output, whose line MoveTo count is always 1 (encode_linestring and
/// encode_line_segments; append_geometry preserves headers), so anything else
/// is an encoder invariant violation.
fn decode_line_segments(commands: &[u32], segments: &mut Vec<Vec<IntPoint>>) {
    struct Segments<'a>(&'a mut Vec<Vec<IntPoint>>);
    impl CommandSink for Segments<'_> {
        fn command(&mut self, command: Command) -> Result<(), GeometryError> {
            match command.kind {
                CommandKind::MoveTo if command.count != 1 => Err(GeometryError::Consumer(
                    "line MoveTo count must be 1".to_string(),
                )),
                CommandKind::MoveTo => {
                    self.0.push(Vec::new());
                    Ok(())
                }
                CommandKind::LineTo if self.0.is_empty() => Err(GeometryError::Consumer(
                    "line LineTo without MoveTo".to_string(),
                )),
                CommandKind::LineTo => Ok(()),
                CommandKind::ClosePath => Err(GeometryError::Consumer(
                    "ClosePath in line geometry".to_string(),
                )),
            }
        }
        fn point(&mut self, point: Point) -> Result<(), GeometryError> {
            let (x, y) = point.xy_i32()?;
            if let Some(segment) = self.0.last_mut() {
                segment.push(IntPoint::new(x, y));
            }
            Ok(())
        }
    }
    segments.clear();
    decode::decode(Words::slice(commands), None, &mut Segments(segments))
        .expect("line features carry well-formed encoder output");
    segments.retain(|s| s.len() >= 2);
}
/// Re-encode absolute-coordinate segments as MVT line geometry commands.
/// Deltas outside i32 are encoder invariant violations, just as when rebasing
/// appended geometry. Check even though pipeline coordinates are tile-local:
/// two valid i32 coordinates need not have an i32-representable difference.
fn encode_line_segments(segments: &[Vec<IntPoint>], buf: &mut Vec<u32>) {
    buf.clear();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    for seg in segments {
        if seg.len() < 2 {
            continue;
        }
        // MoveTo first point
        buf.push(command(1, 1));
        buf.push(zigzag(
            seg[0]
                .x
                .checked_sub(cx)
                .expect("line MoveTo x delta fits i32"),
        ));
        buf.push(zigzag(
            seg[0]
                .y
                .checked_sub(cy)
                .expect("line MoveTo y delta fits i32"),
        ));
        cx = seg[0].x;
        cy = seg[0].y;
        // LineTo remaining, skipping consecutive duplicates
        let lineto_pos = buf.len();
        buf.push(0); // placeholder
        let mut count = 0u32;
        for point in &seg[1..] {
            let (x, y) = (point.x, point.y);
            if x == cx && y == cy {
                continue;
            }
            buf.push(zigzag(
                x.checked_sub(cx).expect("line LineTo x delta fits i32"),
            ));
            buf.push(zigzag(
                y.checked_sub(cy).expect("line LineTo y delta fits i32"),
            ));
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

// ---------------------------------------------------------------------------
// Test helpers (pub(super) for use in tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(super) fn test_append_geometry(dest: &mut Vec<u32>, src: &[u32], cx: &mut i32, cy: &mut i32) {
    append_geometry(dest, src, cx, cy);
}

#[cfg(test)]
pub(super) fn test_decode_line_segments(commands: &[u32], segments: &mut Vec<Vec<(i32, i32)>>) {
    let mut typed = Vec::new();
    decode_line_segments(commands, &mut typed);
    segments.clear();
    segments.extend(
        typed
            .into_iter()
            .map(|line| line.into_iter().map(|point| (point.x, point.y)).collect()),
    );
}

/// One merger run on raw segments: (retained, merged, witness, chain ends).
#[cfg(test)]
pub(super) type TestMergeRun = (
    Vec<Vec<(i32, i32)>>,
    Vec<Vec<(i32, i32)>>,
    Vec<(usize, bool)>,
    Vec<usize>,
);

#[cfg(test)]
pub(super) fn test_merge(input: &[Vec<(i32, i32)>]) -> TestMergeRun {
    let mut segments: Vec<Vec<IntPoint>> = input
        .iter()
        .map(|line| line.iter().copied().map(IntPoint::from).collect())
        .collect();
    let mut buf = line_merge::LineMergeBuffers::default();
    line_merge::merge_line_segments(&mut segments, &mut buf);
    let to_tuples = |lines: Vec<Vec<IntPoint>>| {
        lines
            .into_iter()
            .map(|line| line.into_iter().map(|point| (point.x, point.y)).collect())
            .collect()
    };
    (
        to_tuples(segments),
        to_tuples(buf.merged),
        buf.witness,
        buf.chain_ends,
    )
}

/// Runs the invariant checker; `merged` doubles as the decoded output.
#[cfg(test)]
pub(super) fn test_check_merge(
    input: &[Vec<(i32, i32)>],
    retained: &[Vec<(i32, i32)>],
    merged: &[Vec<(i32, i32)>],
    witness: &[(usize, bool)],
    chain_ends: &[usize],
) -> Result<(), line_merge::MergeViolation> {
    let to_points = |lines: &[Vec<(i32, i32)>]| {
        lines
            .iter()
            .map(|line| line.iter().copied().map(IntPoint::from).collect())
            .collect::<Vec<Vec<IntPoint>>>()
    };
    line_merge::check_merge(
        &to_points(input),
        &to_points(retained),
        &to_points(merged),
        &to_points(merged),
        witness,
        chain_ends,
    )
}

#[cfg(test)]
pub(super) const TEST_CLIP_BOUNDARY: (i32, i32) = line_merge::TEST_CLIP_BOUNDARY;

#[cfg(test)]
mod encoding_tests {
    use super::*;

    #[test]
    fn line_deltas_at_i32_limits_roundtrip() {
        let segments = vec![
            vec![IntPoint::new(i32::MIN, i32::MAX), IntPoint::new(-1, 0)],
            vec![IntPoint::new(0, -1), IntPoint::new(i32::MAX, i32::MIN)],
        ];
        let mut encoded = Vec::new();
        encode_line_segments(&segments, &mut encoded);
        let mut decoded = Vec::new();
        decode_line_segments(&encoded, &mut decoded);
        assert_eq!(decoded, segments);
    }

    #[test]
    fn overflowing_line_deltas_are_rejected() {
        for axis in 0..2 {
            for (start, end) in [(i32::MIN, i32::MAX), (i32::MAX, i32::MIN)] {
                let point = |v| {
                    if axis == 0 {
                        IntPoint::new(v, 0)
                    } else {
                        IntPoint::new(0, v)
                    }
                };
                for segments in [
                    vec![vec![point(start), point(end)]],
                    vec![vec![point(0), point(start)], vec![point(end), point(0)]],
                ] {
                    let result = std::panic::catch_unwind(|| {
                        encode_line_segments(&segments, &mut Vec::new());
                    });
                    assert!(
                        result.is_err(),
                        "axis {axis}, {start} to {end}: {segments:?}"
                    );
                }
            }
        }
    }
}
