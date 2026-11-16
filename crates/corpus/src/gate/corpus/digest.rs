//! The digest: canonical per-tile leaf runs, the fold into zoom/bucket rollups
//! and roots, the on-disk `digest`/`leaves` text formats, and the baseline
//! self-integrity recomputation.
//!
//! This is pure arithmetic over per-tile hashes plus text (de)serialization -
//! no archive decode - so it is wholly brokkr's (`brokkr.md`, "The fold"). The
//! hash domains and text headers are byte-identical to elivagar's originals so
//! brokkr reads and writes the baselines already committed in elivagar's repo.

use std::fs;
use std::io;
use std::path::Path;

use clap::ValueEnum;
use xxhash_rust::xxh3::Xxh3;

use super::super::constants::BUCKET_ZOOM;
use super::super::eliv::{next_zoom_boundary, tile_id_to_zxy, xy_to_tile_id};
use super::sort::ExternalU128Sorter;

const DIGEST_HEADER: &str = "elivagar-corpus-digest v1";
const LEAVES_HEADER: &str = "elivagar-corpus-leaves v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum DigestMode {
    Leaves,
    Buckets,
}

impl DigestMode {
    /// Canonical spelling shared by digest output and the baseline parser.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Leaves => "leaves",
            Self::Buckets => "buckets",
        }
    }

    /// Parse a mode spelling from a stored corpus baseline.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "leaves" => Some(Self::Leaves),
            "buckets" => Some(Self::Buckets),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ZoomDigest {
    pub z: u8,
    pub tiles: u64,
    pub hash: u128,
}

#[derive(Clone, Debug)]
pub struct BucketDigest {
    pub z: u8,
    pub cell: u64,
    pub tiles: u64,
    pub hash: u128,
}

#[derive(Clone, Debug)]
pub struct Digest {
    pub mode: DigestMode,
    pub root: u128,
    /// Bucket root: `Some` in `Buckets` mode only - the stronger integrity guard
    /// the opaque bucket rows need.
    pub broot: Option<u128>,
    pub tiles: u64,
    pub entries: u64,
    pub unique: u64,
    pub zooms: Vec<ZoomDigest>,
    pub buckets: Vec<BucketDigest>,
}

/// A maximal same-zoom span of tile ids that share one semantic hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafRun {
    pub tile_id: u64,
    pub run_length: u32,
    pub hash: u128,
}

// ---------------------------------------------------------------------------
// Hash primitives (domains verbatim from elivagar).
// ---------------------------------------------------------------------------

fn pair(tile: u64, semantic: u128) -> u128 {
    let mut h = Xxh3::new();
    h.update(b"elivagar-corpus-pair-v1");
    h.update(&tile.to_le_bytes());
    h.update(&semantic.to_le_bytes());
    h.digest128()
}

fn root(zooms: &[ZoomDigest]) -> u128 {
    let mut h = Xxh3::new();
    h.update(b"corpus-root-v1");
    h.update(&(zooms.len() as u64).to_le_bytes());
    for z in zooms {
        h.update(&[z.z]);
        h.update(&z.hash.to_le_bytes());
    }
    h.digest128()
}

/// Bucket root over the (z, cell, hash) rows in ascending (z, cell) order.
fn bucket_root(buckets: &[BucketDigest]) -> u128 {
    let mut h = Xxh3::new();
    h.update(b"corpus-bucket-root-v1");
    h.update(&(buckets.len() as u64).to_le_bytes());
    for b in buckets {
        h.update(&[b.z]);
        h.update(&b.cell.to_le_bytes());
        h.update(&b.hash.to_le_bytes());
    }
    h.digest128()
}

/// Fold canonical leaf runs into the full digest. The single definition of every
/// committed count and hash: `compute` (from an archive) and the baseline
/// self-consistency recomputation (from committed `leaves`) both route through
/// it and cannot drift.
///
/// `sort_scratch` is where the external sort spills once a zoom or bucket
/// outgrows memory; it is created on demand and each spill is removed again.
pub fn fold_leaves(
    leaves: &[LeafRun],
    mode: DigestMode,
    sort_scratch: &Path,
) -> io::Result<Digest> {
    let mut unique_hashes = ExternalU128Sorter::new(sort_scratch);
    let mut ordered: Vec<&LeafRun> = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        unique_hashes.push(leaf.hash)?;
        ordered.push(leaf);
    }
    ordered.sort_unstable_by_key(|leaf| leaf.tile_id);
    let unique = unique_hashes.finish_unique_count()?;
    drop(unique_hashes);

    let mut tiles = 0u64;
    // Every leaf counts toward `unique`, zero-length ones included, as the
    // in-memory fold always did. A run that crosses a zoom boundary is split
    // there: each tile id still lands in its own zoom, exactly as the
    // per-tile walk assigned it. Sorting leaf references preserves the public
    // function's acceptance of unsorted input without materializing a second
    // per-zoom tuple vector.
    let mut zooms = Vec::new();
    let mut buckets = Vec::new();
    let mut active_zoom: Option<ZoomFold> = None;
    for leaf in ordered {
        let mut start = leaf.tile_id;
        let end = start
            .checked_add(u64::from(leaf.run_length))
            .ok_or_else(|| invalid("leaf run end overflows"))?;
        while start < end {
            let segment_end = end.min(next_zoom_boundary(start));
            if segment_end <= start {
                return Err(invalid("leaf run crosses an invalid zoom boundary"));
            }
            let (zoom, _, _) = tile_id_to_zxy(start);
            if active_zoom.as_ref().is_none_or(|fold| fold.zoom != zoom) {
                if let Some(fold) = active_zoom.take() {
                    let digest = fold.finish(mode, &mut buckets)?;
                    tiles = tiles
                        .checked_add(digest.tiles)
                        .ok_or_else(|| invalid("leaf tile count overflows"))?;
                    zooms.push(digest);
                }
                active_zoom = Some(ZoomFold::new(zoom, sort_scratch));
            }
            let fold = active_zoom
                .as_mut()
                .ok_or_else(|| invalid("zoom fold was not initialized"))?;
            if fold.previous_end.is_some_and(|previous| start < previous) {
                return Err(invalid("leaf runs overlap"));
            }
            fold.previous_end = Some(segment_end);
            for id in start..segment_end {
                let (_, x, y) = tile_id_to_zxy(id);
                let value = pair(id, leaf.hash);
                fold.values.push(value)?;
                fold.tiles = fold
                    .tiles
                    .checked_add(1)
                    .ok_or_else(|| invalid("leaf tile count overflows"))?;
                if mode == DigestMode::Buckets {
                    // PMTiles tile ids follow the Hilbert order. Descendants of
                    // one z7 cell are contiguous, so one external sorter can
                    // hash the cell without retaining an expanded tile vector.
                    let cell = if zoom <= BUCKET_ZOOM {
                        id
                    } else {
                        xy_to_tile_id(
                            BUCKET_ZOOM,
                            x >> (zoom - BUCKET_ZOOM),
                            y >> (zoom - BUCKET_ZOOM),
                        )
                    };
                    if fold.bucket_cell.is_some_and(|previous| previous != cell) {
                        finish_bucket(
                            zoom,
                            fold.bucket_cell,
                            fold.bucket_tiles,
                            &mut fold.bucket_values,
                            &mut buckets,
                        )?;
                        fold.bucket_values = ExternalU128Sorter::new(sort_scratch);
                        fold.bucket_tiles = 0;
                    }
                    fold.bucket_cell = Some(cell);
                    fold.bucket_values.push(value)?;
                    fold.bucket_tiles = fold
                        .bucket_tiles
                        .checked_add(1)
                        .ok_or_else(|| invalid("leaf bucket tile count overflows"))?;
                }
            }
            start = segment_end;
        }
    }
    if let Some(fold) = active_zoom {
        let digest = fold.finish(mode, &mut buckets)?;
        tiles = tiles
            .checked_add(digest.tiles)
            .ok_or_else(|| invalid("leaf tile count overflows"))?;
        zooms.push(digest);
    }
    buckets.sort_unstable_by_key(|bucket| (bucket.z, bucket.cell));
    let broot = (mode == DigestMode::Buckets).then(|| bucket_root(&buckets));
    Ok(Digest {
        mode,
        root: root(&zooms),
        broot,
        tiles,
        entries: leaves.len() as u64,
        unique,
        zooms,
        buckets,
    })
}

struct ZoomFold {
    zoom: u8,
    values: ExternalU128Sorter,
    tiles: u64,
    previous_end: Option<u64>,
    bucket_cell: Option<u64>,
    bucket_tiles: u64,
    bucket_values: ExternalU128Sorter,
}

impl ZoomFold {
    fn new(zoom: u8, sort_scratch: &Path) -> Self {
        Self {
            zoom,
            values: ExternalU128Sorter::new(sort_scratch),
            tiles: 0,
            previous_end: None,
            bucket_cell: None,
            bucket_tiles: 0,
            bucket_values: ExternalU128Sorter::new(sort_scratch),
        }
    }

    fn finish(
        mut self,
        mode: DigestMode,
        buckets: &mut Vec<BucketDigest>,
    ) -> io::Result<ZoomDigest> {
        if mode == DigestMode::Buckets {
            finish_bucket(
                self.zoom,
                self.bucket_cell,
                self.bucket_tiles,
                &mut self.bucket_values,
                buckets,
            )?;
        }
        let hash = self.values.finish_hash(b"corpus-zoom", self.tiles)?;
        Ok(ZoomDigest {
            z: self.zoom,
            tiles: self.tiles,
            hash,
        })
    }
}

fn finish_bucket(
    zoom: u8,
    cell: Option<u64>,
    tiles: u64,
    values: &mut ExternalU128Sorter,
    buckets: &mut Vec<BucketDigest>,
) -> io::Result<()> {
    let Some(cell) = cell else {
        return Ok(());
    };
    if tiles == 0 {
        return Err(invalid("leaf bucket has no tiles"));
    }
    let hash = values.finish_hash(b"corpus-bucket", tiles)?;
    buckets.push(BucketDigest {
        z: zoom,
        cell,
        tiles,
        hash,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Text formats.
// ---------------------------------------------------------------------------

#[must_use]
pub fn hex(v: u128) -> String {
    format!("{v:032x}")
}

fn parse_hex(s: &str) -> io::Result<u128> {
    u128::from_str_radix(s, 16).map_err(|_| invalid("invalid digest hash"))
}

#[must_use]
pub fn digest_text(d: &Digest) -> String {
    let mut out = format!(
        "{DIGEST_HEADER}\nmode {}\nroot {}\n",
        d.mode.as_str(),
        hex(d.root),
    );
    if let Some(broot) = d.broot {
        out.push_str(&format!("broot {}\n", hex(broot)));
    }
    out.push_str(&format!(
        "tiles {} entries {} unique {}\n",
        d.tiles, d.entries, d.unique
    ));
    for z in &d.zooms {
        out.push_str(&format!(
            "zoom {} tiles {} hash {}\n",
            z.z,
            z.tiles,
            hex(z.hash)
        ));
    }
    for b in &d.buckets {
        let (cz, x, y) = tile_id_to_zxy(b.cell);
        out.push_str(&format!(
            "bucket z={} cell={cz}/{x}/{y} tiles={} hash {}\n",
            b.z,
            b.tiles,
            hex(b.hash)
        ));
    }
    out
}

#[must_use]
pub fn leaves_text(leaves: &[LeafRun]) -> String {
    let mut out = format!("{LEAVES_HEADER}\n");
    for l in leaves {
        let (z, x, y) = tile_id_to_zxy(l.tile_id);
        out.push_str(&format!("{z} {x} {y} {} {}\n", l.run_length, hex(l.hash)));
    }
    out
}

// ---------------------------------------------------------------------------
// Baseline parse + self-integrity.
// ---------------------------------------------------------------------------

/// The committed digest parsed back in full, so a baseline can be checked for
/// internal consistency before an archive is ever opened.
pub struct Baseline {
    pub mode: DigestMode,
    pub root: u128,
    pub broot: Option<u128>,
    pub tiles: u64,
    pub entries: u64,
    pub unique: u64,
    pub zooms: Vec<ZoomDigest>,
    pub buckets: Vec<BucketDigest>,
}

pub fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub fn parse_baseline(path: &Path) -> io::Result<Baseline> {
    parse_baseline_text(&fs::read_to_string(path)?)
}

fn parse_baseline_text(text: &str) -> io::Result<Baseline> {
    let mut mode = None;
    let mut root = None;
    let mut broot = None;
    let mut counts = None;
    let mut zooms = Vec::new();
    let mut buckets = Vec::new();
    for (n, line) in text.lines().enumerate() {
        // The header is mandatory and version-checked, exactly as in
        // `parse_leaves_text`. Accepting a headerless or wrong-version file
        // would let merge damage that truncated the first line - or a future
        // v2 written by a newer brokkr - parse as a v1 baseline.
        if n == 0 {
            if line != DIGEST_HEADER {
                return Err(invalid("digest header invalid"));
            }
            continue;
        }
        let p: Vec<&str> = line.split_whitespace().collect();
        match p.first().copied() {
            Some("mode") => {
                mode = Some(
                    p.get(1)
                        .and_then(|value| DigestMode::parse(value))
                        .ok_or_else(|| invalid("digest mode invalid"))?,
                );
            }
            Some("root") => {
                root = Some(parse_hex(
                    p.get(1).ok_or_else(|| invalid("root missing hash"))?,
                )?);
            }
            Some("broot") => {
                broot = Some(parse_hex(
                    p.get(1).ok_or_else(|| invalid("broot missing hash"))?,
                )?);
            }
            Some("tiles") => {
                let t = parse_u64(p.get(1).copied())?;
                let e = parse_u64(p.get(3).copied())?;
                let u = parse_u64(p.get(5).copied())?;
                counts = Some((t, e, u));
            }
            Some("zoom") => {
                zooms.push(ZoomDigest {
                    z: parse_u8(p.get(1).copied())?,
                    tiles: parse_u64(p.get(3).copied())?,
                    hash: parse_hex(p.get(5).ok_or_else(|| invalid("zoom missing hash"))?)?,
                });
            }
            Some("bucket") => {
                let z = parse_u8(strip(p.get(1).copied(), "z=").as_deref())?;
                let cell = parse_cell(strip(p.get(2).copied(), "cell=").as_deref())?;
                let tiles = parse_u64(strip(p.get(3).copied(), "tiles=").as_deref())?;
                let hash = parse_hex(p.get(5).ok_or_else(|| invalid("bucket missing hash"))?)?;
                buckets.push(BucketDigest {
                    z,
                    cell,
                    tiles,
                    hash,
                });
            }
            Some("") | None => {}
            Some(other) => return Err(invalid(format!("unrecognized digest line: {other}"))),
        }
    }
    let mode = mode.ok_or_else(|| invalid("digest mode missing"))?;
    let (tiles, entries, unique) = counts.ok_or_else(|| invalid("digest counts missing"))?;
    Ok(Baseline {
        mode,
        root: root.ok_or_else(|| invalid("digest root missing"))?,
        broot,
        tiles,
        entries,
        unique,
        zooms,
        buckets,
    })
}

pub fn parse_leaves(path: &Path) -> io::Result<Vec<LeafRun>> {
    parse_leaves_text(&fs::read_to_string(path)?)
}

fn parse_leaves_text(text: &str) -> io::Result<Vec<LeafRun>> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if n == 0 {
            if line != LEAVES_HEADER {
                return Err(invalid("leaves header invalid"));
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.len() != 5 {
            return Err(invalid("leaves line must be z x y run hash"));
        }
        let z = parse_u8(p.first().copied())?;
        let x = parse_u32(p.get(1).copied())?;
        let y = parse_u32(p.get(2).copied())?;
        let run_length = u32::try_from(parse_u64(p.get(3).copied())?)
            .map_err(|_| invalid("leaf run length out of range"))?;
        let hash = parse_hex(p[4])?;
        let tile_id = checked_tile_id(z, x, y)?;
        // A leaf run is a span WITHIN one zoom (see `compute`), so a committed
        // run that crosses its zoom's end is damage - and left unchecked it
        // hands `fold_leaves` a per-tile loop of up to u32::MAX iterations.
        if run_length == 0 || tile_id + u64::from(run_length) > next_zoom_boundary(tile_id) {
            return Err(invalid("leaf run length out of range for its zoom"));
        }
        out.push(LeafRun {
            tile_id,
            run_length,
            hash,
        });
    }
    Ok(out)
}

/// Refuse a baseline whose committed rows do not reproduce their own roots.
/// Closes the hand-edit / merge-damage hole, invisible otherwise in the opaque
/// bucket mode. In buckets mode this is a weaker guard (no full leaves exist to
/// recompute from - `brokkr.md`, step 1).
pub fn baseline_inconsistency(
    base: &Baseline,
    leaves: Option<&[LeafRun]>,
    sort_scratch: &Path,
) -> io::Result<Option<String>> {
    if root(&base.zooms) != base.root {
        return Ok(Some("root does not match committed zoom rows".into()));
    }
    if base.mode == DigestMode::Buckets {
        match base.broot {
            None => return Ok(Some("bucket mode is missing broot".into())),
            Some(br) if bucket_root(&base.buckets) != br => {
                return Ok(Some("broot does not match committed bucket rows".into()));
            }
            Some(_) => {}
        }
    } else if base.broot.is_some() {
        return Ok(Some("leaves mode carries an unexpected broot".into()));
    }
    if let Some(leaves) = leaves {
        let recomputed = fold_leaves(leaves, base.mode, sort_scratch)?;
        if recomputed.root != base.root {
            return Ok(Some("committed leaves do not reproduce root".into()));
        }
        if !zooms_equal(&recomputed.zooms, &base.zooms) {
            return Ok(Some("committed leaves do not reproduce zoom rows".into()));
        }
        if (recomputed.tiles, recomputed.entries, recomputed.unique)
            != (base.tiles, base.entries, base.unique)
        {
            return Ok(Some("committed leaves do not reproduce counts".into()));
        }
    }
    Ok(None)
}

fn zooms_equal(a: &[ZoomDigest], b: &[ZoomDigest]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.z == y.z && x.tiles == y.tiles && x.hash == y.hash)
}

// ---------------------------------------------------------------------------
// Small token parsers.
// ---------------------------------------------------------------------------

fn strip(token: Option<&str>, prefix: &str) -> Option<String> {
    token
        .and_then(|t| t.strip_prefix(prefix))
        .map(str::to_string)
}
fn parse_u64(token: Option<&str>) -> io::Result<u64> {
    token
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("expected integer field"))
}
fn parse_u8(token: Option<&str>) -> io::Result<u8> {
    token
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("expected u8 field"))
}
fn parse_u32(token: Option<&str>) -> io::Result<u32> {
    token
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("expected coordinate field"))
}
fn parse_cell(token: Option<&str>) -> io::Result<u64> {
    let mut parts = token
        .ok_or_else(|| invalid("bucket cell missing"))?
        .split('/');
    let z: u8 = parts
        .next()
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("cell zoom"))?;
    let x: u32 = parts
        .next()
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("cell x"))?;
    let y: u32 = parts
        .next()
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("cell y"))?;
    checked_tile_id(z, x, y)
}

/// The deepest zoom a committed coordinate may name. Far beyond any zoom
/// elivagar writes (z14), so a deeper one in committed text is damage.
const MAX_COMMITTED_ZOOM: u8 = 29;

/// `xy_to_tile_id` for a coordinate read from committed text: the zoom and the
/// x/y range are checked first, because the addressing math assumes a valid
/// tile and a damaged file is exactly what does not provide one.
fn checked_tile_id(z: u8, x: u32, y: u32) -> io::Result<u64> {
    if z > MAX_COMMITTED_ZOOM || u64::from(x) >= 1u64 << z || u64::from(y) >= 1u64 << z {
        return Err(invalid(format!("tile coordinate {z}/{x}/{y} out of range")));
    }
    Ok(xy_to_tile_id(z, x, y))
}

#[cfg(test)]
mod tests {
    //! Ported from elivagar's `corpus::format_tests` - the pure (archive-free)
    //! half that exercises the brokkr-owned fold, text formats, and baseline
    //! self-integrity. The mutation/canonical-run tests wait on the linked
    //! reader/writer (they need real archives).
    #![allow(clippy::unwrap_used)]

    use super::super::fixture::TestDir;
    use super::*;

    #[test]
    fn committed_digest_domains_and_text_have_frozen_values() {
        let scratch = TestDir::new("digest-frozen-values");
        let leaves = [LeafRun {
            tile_id: 0,
            run_length: 1,
            hash: 0xf009_d6ca_413d_6a83_596d_8ad9_b611_1e24,
        }];

        let leaf_digest = fold_leaves(&leaves, DigestMode::Leaves, &scratch.path).unwrap();
        assert_eq!(
            hex(leaf_digest.zooms[0].hash),
            "ae8de239a0984a01ee78f7c2852ab8f0"
        );
        assert_eq!(hex(leaf_digest.root), "639b1ae66c8508cec5c35f6903946d8a");
        assert_eq!(
            digest_text(&leaf_digest),
            concat!(
                "elivagar-corpus-digest v1\n",
                "mode leaves\n",
                "root 639b1ae66c8508cec5c35f6903946d8a\n",
                "tiles 1 entries 1 unique 1\n",
                "zoom 0 tiles 1 hash ae8de239a0984a01ee78f7c2852ab8f0\n",
            )
        );
        assert_eq!(
            leaves_text(&leaves),
            concat!(
                "elivagar-corpus-leaves v1\n",
                "0 0 0 1 f009d6ca413d6a83596d8ad9b6111e24\n",
            )
        );

        let bucket_digest = fold_leaves(&leaves, DigestMode::Buckets, &scratch.path).unwrap();
        assert_eq!(
            hex(bucket_digest.zooms[0].hash),
            "ae8de239a0984a01ee78f7c2852ab8f0"
        );
        assert_eq!(hex(bucket_digest.root), "639b1ae66c8508cec5c35f6903946d8a");
        assert_eq!(
            hex(bucket_digest.broot.unwrap()),
            "eeaaa54af46b2afceffd8abbd10e74d6"
        );
        assert_eq!(
            hex(bucket_digest.buckets[0].hash),
            "867263213ced0147bcae9a3a9d2e96da"
        );
        assert_eq!(
            digest_text(&bucket_digest),
            concat!(
                "elivagar-corpus-digest v1\n",
                "mode buckets\n",
                "root 639b1ae66c8508cec5c35f6903946d8a\n",
                "broot eeaaa54af46b2afceffd8abbd10e74d6\n",
                "tiles 1 entries 1 unique 1\n",
                "zoom 0 tiles 1 hash ae8de239a0984a01ee78f7c2852ab8f0\n",
                "bucket z=0 cell=0/0/0 tiles=1 hash 867263213ced0147bcae9a3a9d2e96da\n",
            )
        );
    }

    #[test]
    fn leaves_digest_round_trips_and_self_checks() {
        let scratch = TestDir::new("digest-round-trip");
        let leaves = vec![
            LeafRun {
                tile_id: xy_to_tile_id(3, 0, 0),
                run_length: 2,
                hash: 0x1111,
            },
            LeafRun {
                tile_id: xy_to_tile_id(4, 5, 6),
                run_length: 1,
                hash: 0x2222,
            },
        ];
        assert!(leaves[0].tile_id < leaves[1].tile_id);
        let dg = fold_leaves(&leaves, DigestMode::Leaves, &scratch.path).unwrap();
        let base = parse_baseline_text(&digest_text(&dg)).unwrap();
        let parsed = parse_leaves_text(&leaves_text(&leaves)).unwrap();
        assert_eq!(parsed, leaves);
        assert_eq!(base.root, dg.root);
        assert!(base.broot.is_none());
        assert!(
            baseline_inconsistency(&base, Some(&parsed), &scratch.path)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn bucket_mode_groups_by_z7_ancestor_and_round_trips_broot() {
        let scratch = TestDir::new("digest-bucket-groups");
        let z5 = xy_to_tile_id(5, 3, 4);
        let z8 = xy_to_tile_id(8, 100, 100);
        assert!(z5 < z8);
        let leaves = vec![
            LeafRun {
                tile_id: z5,
                run_length: 1,
                hash: 0xAAAA,
            },
            LeafRun {
                tile_id: z8,
                run_length: 1,
                hash: 0xBBBB,
            },
        ];
        let dg = fold_leaves(&leaves, DigestMode::Buckets, &scratch.path).unwrap();
        assert!(dg.broot.is_some());
        assert!(dg.buckets.iter().any(|b| b.z == 5 && b.cell == z5));
        let ancestor = xy_to_tile_id(7, 100 >> 1, 100 >> 1);
        assert!(dg.buckets.iter().any(|b| b.z == 8 && b.cell == ancestor));
        let base = parse_baseline_text(&digest_text(&dg)).unwrap();
        assert_eq!(base.broot, dg.broot);
        // The z7-vs-z<=7 cell label must round-trip through parse.
        assert!(
            baseline_inconsistency(&base, None, &scratch.path)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn self_consistency_rejects_a_tampered_root() {
        let scratch = TestDir::new("digest-tampered-root");
        let leaves = vec![LeafRun {
            tile_id: xy_to_tile_id(2, 1, 1),
            run_length: 1,
            hash: 7,
        }];
        let dg = fold_leaves(&leaves, DigestMode::Leaves, &scratch.path).unwrap();
        let text = digest_text(&dg).replace(&hex(dg.root), &format!("{:032x}", dg.root ^ 1));
        let base = parse_baseline_text(&text).unwrap();
        assert!(
            baseline_inconsistency(&base, Some(&leaves), &scratch.path)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn digest_header_is_mandatory_and_version_checked() {
        let scratch = TestDir::new("digest-header");
        let leaves = vec![LeafRun {
            tile_id: xy_to_tile_id(2, 1, 1),
            run_length: 1,
            hash: 5,
        }];
        let text = digest_text(&fold_leaves(&leaves, DigestMode::Leaves, &scratch.path).unwrap());
        assert!(parse_baseline_text(&text).is_ok());
        // Truncated first line (the shape merge damage takes).
        let headerless = text.split_once('\n').unwrap().1.to_string();
        assert!(parse_baseline_text(&headerless).is_err());
        // A version this parser does not speak is not silently read as v1.
        assert!(parse_baseline_text(&text.replace("v1", "v2")).is_err());
    }

    #[test]
    fn leaves_reject_out_of_range_coordinates_and_zoom_crossing_runs() {
        let head = format!("{LEAVES_HEADER}\n");
        let h = hex(1);
        // x beyond the z2 grid.
        assert!(parse_leaves_text(&format!("{head}2 4 0 1 {h}\n")).is_err());
        // A zoom the addressing math cannot place.
        assert!(parse_leaves_text(&format!("{head}40 0 0 1 {h}\n")).is_err());
        // z1 holds 4 tiles; a run of 5 from its first crosses into z2.
        assert!(parse_leaves_text(&format!("{head}1 0 0 5 {h}\n")).is_err());
        // An empty run.
        assert!(parse_leaves_text(&format!("{head}1 0 0 0 {h}\n")).is_err());
        // The whole of z1 is fine.
        assert!(parse_leaves_text(&format!("{head}1 0 0 4 {h}\n")).is_ok());
    }

    #[test]
    fn leaves_mode_rejects_a_stray_broot() {
        let scratch = TestDir::new("digest-stray-broot");
        let leaves = vec![LeafRun {
            tile_id: xy_to_tile_id(2, 1, 1),
            run_length: 1,
            hash: 9,
        }];
        let dg = fold_leaves(&leaves, DigestMode::Leaves, &scratch.path).unwrap();
        let mut text = digest_text(&dg);
        text.push_str(&format!("broot {}\n", hex(123)));
        let base = parse_baseline_text(&text).unwrap();
        assert!(
            baseline_inconsistency(&base, Some(&leaves), &scratch.path)
                .unwrap()
                .is_some()
        );
    }
}
