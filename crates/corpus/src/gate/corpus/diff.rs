//! The localized content diff: which tiles/zooms/buckets changed between the
//! committed baseline and a fresh archive. Pure over the parsed rows.

use std::collections::{BTreeMap, BTreeSet};

use super::super::constants::DIFF_LINE_LIMIT;
use super::super::eliv::tile_id_to_zxy;
use super::digest::{BucketDigest, LeafRun, ZoomDigest, hex};

#[derive(Default)]
struct LeafDiff {
    // index 0 changed, 1 added, 2 removed
    counts: [u64; 3],
    lines: [Vec<String>; 3],
    cur: Option<(usize, u64, u64, u128, u128)>,
}
impl LeafDiff {
    fn push_range(&mut self, class: usize, segment_start: u64, end: u64, oldh: u128, newh: u128) {
        if segment_start >= end {
            return;
        }
        if let Some((c, run_start, len, o, nh)) = self.cur.as_mut()
            && *c == class
            && *o == oldh
            && *nh == newh
            && *run_start + *len == segment_start
            && tile_id_to_zxy(*run_start).0 == tile_id_to_zxy(segment_start).0
        {
            *len += end - segment_start;
            return;
        }
        self.flush();
        self.cur = Some((class, segment_start, end - segment_start, oldh, newh));
    }
    fn flush(&mut self) {
        let Some((class, start, len, oldh, newh)) = self.cur.take() else {
            return;
        };
        self.counts[class] += 1;
        if self.lines[class].len() < DIFF_LINE_LIMIT {
            let (z, x, y) = tile_id_to_zxy(start);
            let line = match class {
                0 => format!("changed {z} {x} {y} {len} {}->{}", hex(oldh), hex(newh)),
                1 => format!("added {z} {x} {y} {len} {}", hex(newh)),
                _ => format!("removed {z} {x} {y} {len} {}", hex(oldh)),
            };
            self.lines[class].push(line);
        }
    }
}

/// Diff two leaf-run sets without expanding their spans into per-tile vectors.
/// Identical runs are skipped in one step; changed spans are coalesced directly.
/// Returns (changed-run count, capped lines).
#[must_use]
pub fn leaf_diff(committed: &[LeafRun], current: &[LeafRun]) -> (u64, Vec<String>) {
    let mut old_runs: Vec<&LeafRun> = committed.iter().collect();
    old_runs.sort_unstable_by_key(|run| run.tile_id);
    let mut new_runs: Vec<&LeafRun> = current.iter().collect();
    new_runs.sort_unstable_by_key(|run| run.tile_id);
    let mut old = LeafCursor::new(old_runs);
    let mut new = LeafCursor::new(new_runs);
    let mut st = LeafDiff::default();
    loop {
        match (old.range(), new.range()) {
            (None, None) => break,
            (Some((start, end, hash)), None) => {
                st.push_range(2, start, end, hash, 0);
                old.advance(end);
            }
            (None, Some((start, end, hash))) => {
                st.push_range(1, start, end, 0, hash);
                new.advance(end);
            }
            (Some((old_start, old_end, old_hash)), Some((new_start, new_end, new_hash))) => {
                if old_start < new_start {
                    let end = old_end.min(new_start);
                    st.push_range(2, old_start, end, old_hash, 0);
                    old.advance(end);
                } else if new_start < old_start {
                    let end = new_end.min(old_start);
                    st.push_range(1, new_start, end, 0, new_hash);
                    new.advance(end);
                } else {
                    let end = old_end.min(new_end);
                    if old_hash != new_hash {
                        st.push_range(0, old_start, end, old_hash, new_hash);
                    }
                    old.advance(end);
                    new.advance(end);
                }
            }
        }
    }
    st.flush();
    let mut out = Vec::new();
    for (class, label) in [(0, "changed"), (1, "added"), (2, "removed")] {
        out.extend(st.lines[class].iter().cloned());
        let extra = st.counts[class] - st.lines[class].len() as u64;
        if extra > 0 {
            out.push(format!("(+{extra} more {label} runs)"));
        }
    }
    (st.counts.iter().sum(), out)
}

struct LeafCursor<'a> {
    runs: Vec<&'a LeafRun>,
    index: usize,
    position: u64,
}

impl<'a> LeafCursor<'a> {
    fn new(runs: Vec<&'a LeafRun>) -> Self {
        let position = runs.first().map_or(0, |run| run.tile_id);
        Self {
            runs,
            index: 0,
            position,
        }
    }

    fn range(&self) -> Option<(u64, u64, u128)> {
        let run = *self.runs.get(self.index)?;
        let end = run.tile_id.saturating_add(u64::from(run.run_length));
        Some((self.position.max(run.tile_id), end, run.hash))
    }

    fn advance(&mut self, end: u64) {
        let Some(run) = self.runs.get(self.index) else {
            return;
        };
        let run_end = run.tile_id.saturating_add(u64::from(run.run_length));
        if end >= run_end {
            self.index += 1;
            self.position = self.runs.get(self.index).map_or(0, |next| next.tile_id);
        } else {
            self.position = end;
        }
    }
}

#[must_use]
pub fn zoom_delta_lines(committed: &[ZoomDigest], current: &[ZoomDigest]) -> Vec<String> {
    let cmap: BTreeMap<u8, &ZoomDigest> = committed.iter().map(|z| (z.z, z)).collect();
    let nmap: BTreeMap<u8, &ZoomDigest> = current.iter().map(|z| (z.z, z)).collect();
    let mut zs: BTreeSet<u8> = BTreeSet::new();
    zs.extend(cmap.keys().copied());
    zs.extend(nmap.keys().copied());
    let mut out = Vec::new();
    for z in zs {
        let ct = cmap.get(&z).map_or(0, |z| z.tiles);
        let nt = nmap.get(&z).map_or(0, |z| z.tiles);
        let hash_changed = match (cmap.get(&z), nmap.get(&z)) {
            (Some(a), Some(b)) => a.hash != b.hash,
            _ => true,
        };
        if ct != nt || hash_changed {
            let flag = if hash_changed { " hash-changed" } else { "" };
            out.push(format!("zoom {z} tiles {ct}->{nt}{flag}"));
        }
    }
    out
}

#[must_use]
pub fn bucket_delta_lines(
    committed: &[BucketDigest],
    current: &[BucketDigest],
) -> (u64, Vec<String>) {
    let cmap: BTreeMap<(u8, u64), &BucketDigest> =
        committed.iter().map(|b| ((b.z, b.cell), b)).collect();
    let nmap: BTreeMap<(u8, u64), &BucketDigest> =
        current.iter().map(|b| ((b.z, b.cell), b)).collect();
    let mut keys: BTreeSet<(u8, u64)> = BTreeSet::new();
    keys.extend(cmap.keys().copied());
    keys.extend(nmap.keys().copied());
    let mut out = Vec::new();
    let mut changed = 0u64;
    for key @ (z, _cell) in keys {
        let ct = cmap.get(&key).map_or(0, |b| b.tiles);
        let nt = nmap.get(&key).map_or(0, |b| b.tiles);
        let hash_changed = match (cmap.get(&key), nmap.get(&key)) {
            (Some(a), Some(b)) => a.hash != b.hash,
            _ => true,
        };
        if ct != nt || hash_changed {
            changed += 1;
            if out.len() < DIFF_LINE_LIMIT {
                let (cz, x, y) = tile_id_to_zxy(key.1);
                out.push(format!("bucket z={z} cell={cz}/{x}/{y} tiles {ct}->{nt}"));
            }
        }
    }
    if changed > out.len() as u64 {
        out.push(format!(
            "(+{} more changed buckets)",
            changed - out.len() as u64
        ));
    }
    (changed, out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::super::eliv::xy_to_tile_id;
    use super::*;

    fn run(z: u8, x: u32, y: u32, len: u32, hash: u128) -> LeafRun {
        LeafRun {
            tile_id: xy_to_tile_id(z, x, y),
            run_length: len,
            hash,
        }
    }

    #[test]
    fn leaf_diff_names_changed_added_and_removed() {
        // committed: a 2-run at z3 (hash A) and a single tile (hash B).
        let committed = vec![run(3, 0, 0, 2, 0xA), run(3, 4, 4, 1, 0xB)];
        // current: the 2-run re-hashed to C (changed), B gone (removed), a new
        // tile at z3/5/5 (added).
        let current = vec![run(3, 0, 0, 2, 0xC), run(3, 5, 5, 1, 0xD)];
        let (n, lines) = leaf_diff(&committed, &current);
        assert_eq!(n, 3, "one changed run + one removed + one added");
        assert!(lines.iter().any(|l| l.starts_with("changed 3 0 0 2")));
        assert!(lines.iter().any(|l| l.starts_with("added")));
        assert!(lines.iter().any(|l| l.starts_with("removed")));
    }

    #[test]
    fn identical_leaves_diff_empty() {
        let leaves = vec![run(5, 1, 1, 3, 0x11)];
        let (n, lines) = leaf_diff(&leaves, &leaves);
        assert_eq!(n, 0);
        assert!(lines.is_empty());
    }

    #[test]
    fn zoom_delta_flags_hash_change_and_count_change() {
        let committed = vec![
            ZoomDigest {
                z: 3,
                tiles: 10,
                hash: 0x1,
            },
            ZoomDigest {
                z: 4,
                tiles: 20,
                hash: 0x2,
            },
        ];
        let current = vec![
            ZoomDigest {
                z: 3,
                tiles: 10,
                hash: 0x9,
            }, // same count, hash changed
            ZoomDigest {
                z: 4,
                tiles: 21,
                hash: 0x2,
            }, // count changed
        ];
        let lines = zoom_delta_lines(&committed, &current);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("zoom 3") && l.contains("hash-changed"))
        );
        assert!(lines.iter().any(|l| l.contains("zoom 4 tiles 20->21")));
    }

    #[test]
    fn bucket_delta_counts_changed_cells() {
        let committed = vec![BucketDigest {
            z: 8,
            cell: xy_to_tile_id(7, 1, 1),
            tiles: 5,
            hash: 0x1,
        }];
        let current = vec![BucketDigest {
            z: 8,
            cell: xy_to_tile_id(7, 1, 1),
            tiles: 5,
            hash: 0x2,
        }];
        let (changed, lines) = bucket_delta_lines(&committed, &current);
        assert_eq!(changed, 1);
        assert!(lines.iter().any(|l| l.starts_with("bucket z=8")));
    }
}
