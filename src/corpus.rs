//! Advisory, git-committed semantic digest for an explicit PMTiles archive.

#![allow(clippy::possible_missing_else)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Instant;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use protohoggr::encode_varint;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use xxhash_rust::xxh3::Xxh3;

use crate::pmtiles_reader::{ArchiveView, BlobRef, RawDirEntry, read_i32_le};
use crate::pmtiles_writer::{PmtilesConfig, PmtilesWriter, tile_id_to_zxy, xy_to_tile_id};
use crate::provenance::{self, ContractDoc, ContractState};
use crate::regress::{DecodeScratch, next_zoom_boundary, semantic_hash};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DigestMode {
    Leaves,
    Buckets,
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
    /// Bucket root: `Some` in `Buckets` mode only. Committed as the `broot`
    /// line, it is the stronger integrity guard the opaque bucket diffs need -
    /// `multiset_hash` is non-homomorphic, so bucket rows cannot be recombined
    /// into zoom hashes and a damaged bucket line is otherwise undetectable.
    pub broot: Option<u128>,
    pub tiles: u64,
    pub entries: u64,
    pub unique: u64,
    pub zooms: Vec<ZoomDigest>,
    pub buckets: Vec<BucketDigest>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafRun {
    pub tile_id: u64,
    pub run_length: u32,
    pub hash: u128,
}
#[derive(Clone, Debug, Default)]
pub struct CheckReport {
    pub message: String,
    pub contract_diffs: Vec<String>,
    pub warnings: Vec<String>,
    pub changed: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorpusVerdict {
    Pass,
    ContentMismatch,
    Refused,
}

/// Deliberately small set of direct archive mutations used to calibrate the
/// corpus gate. This is not a production tile editing API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationOp {
    DropTile,
    NudgeGeometry,
    LayerVersion,
    Regzip,
}

fn hash(domain: &[u8], items: &[u128]) -> u128 {
    let mut v = items.to_vec();
    v.sort_unstable();
    let mut h = Xxh3::new();
    h.update(domain);
    h.update(&(v.len() as u64).to_le_bytes());
    for x in v {
        h.update(&x.to_le_bytes());
    }
    h.digest128()
}
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
/// `buckets` MUST already be sorted; `fold_leaves` produces them from a
/// `BTreeMap`, so its natural iteration order satisfies that.
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

/// Fold canonical leaf runs into the full digest. This is the sole definition
/// of every committed count and hash, so `compute` (from an archive) and the
/// baseline self-consistency recomputation (from the committed `leaves`) both
/// route through it and cannot drift.
fn fold_leaves(leaves: &[LeafRun], mode: DigestMode) -> Digest {
    let mut zoom_pairs: BTreeMap<u8, Vec<u128>> = BTreeMap::new();
    let mut bucket_pairs: BTreeMap<(u8, u64), Vec<u128>> = BTreeMap::new();
    let mut uniques = BTreeSet::new();
    let mut tiles = 0u64;
    for leaf in leaves {
        uniques.insert(leaf.hash);
        for id in leaf.tile_id..leaf.tile_id + u64::from(leaf.run_length) {
            let (z, x, y) = tile_id_to_zxy(id);
            let p = pair(id, leaf.hash);
            zoom_pairs.entry(z).or_default().push(p);
            if mode == DigestMode::Buckets {
                let cell = if z <= 7 {
                    id
                } else {
                    xy_to_tile_id(7, x >> (z - 7), y >> (z - 7))
                };
                bucket_pairs.entry((z, cell)).or_default().push(p);
            }
            tiles += 1;
        }
    }
    let zooms: Vec<_> = zoom_pairs
        .into_iter()
        .map(|(z, v)| ZoomDigest {
            z,
            tiles: v.len() as u64,
            hash: hash(b"corpus-zoom", &v),
        })
        .collect();
    let buckets: Vec<_> = bucket_pairs
        .into_iter()
        .map(|((z, cell), v)| BucketDigest {
            z,
            cell,
            tiles: v.len() as u64,
            hash: hash(b"corpus-bucket", &v),
        })
        .collect();
    let broot = (mode == DigestMode::Buckets).then(|| bucket_root(&buckets));
    Digest {
        mode,
        root: root(&zooms),
        broot,
        tiles,
        entries: leaves.len() as u64,
        unique: uniques.len() as u64,
        zooms,
        buckets,
    }
}

#[allow(clippy::too_many_lines)]
pub fn compute(archive: &ArchiveView, mode: DigestMode) -> io::Result<(Digest, Vec<LeafRun>)> {
    let runs = archive.read_all_runs()?;
    let mut blobs = Vec::new();
    let mut seen = BTreeSet::new();
    for r in &runs {
        let b = BlobRef {
            offset: r.offset,
            length: r.length,
        };
        if seen.insert((b.offset, b.length)) {
            blobs.push(b);
        }
    }
    let mut values = vec![0u128; blobs.len()];
    blobs
        .par_iter()
        .zip(values.par_iter_mut())
        .try_for_each_init(DecodeScratch::default, |s, (b, v)| {
            *v = semantic_hash(archive.raw_blob(*b)?, s)?;
            Ok::<_, io::Error>(())
        })?;
    let hashes: FxHashMap<BlobRef, u128> = blobs.into_iter().zip(values).collect();
    let mut leaves: Vec<LeafRun> = Vec::new();
    for r in runs {
        let semantic = hashes[&BlobRef {
            offset: r.offset,
            length: r.length,
        }];
        let mut at = r.tile_id;
        let end = at + u64::from(r.run_length);
        while at < end {
            let n = end.min(next_zoom_boundary(at));
            let length = u32::try_from(n - at)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "tile run exceeds u32"))?;
            // Merge with the previous run only when it is contiguous, shares the
            // semantic hash, AND sits in the same zoom: a canonical run is a
            // maximal span WITHIN one zoom, so a shared blob straddling a zoom
            // boundary (e.g. an all-water tile) must stay two runs.
            if let Some(last) = leaves.last_mut()
                && last.tile_id + u64::from(last.run_length) == at
                && last.hash == semantic
                && tile_id_to_zxy(last.tile_id).0 == tile_id_to_zxy(at).0
            {
                last.run_length = last.run_length.checked_add(length).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "merged tile run exceeds u32")
                })?;
            } else {
                leaves.push(LeafRun {
                    tile_id: at,
                    run_length: length,
                    hash: semantic,
                });
            }
            at = n;
        }
    }
    let digest = fold_leaves(&leaves, mode);
    Ok((digest, leaves))
}

fn hex(v: u128) -> String {
    format!("{v:032x}")
}
fn parse_hex(s: &str) -> io::Result<u128> {
    u128::from_str_radix(s, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid digest hash"))
}
fn digest_text(d: &Digest) -> String {
    let mut out = format!(
        "elivagar-corpus-digest v1\nmode {}\nroot {}\n",
        if d.mode == DigestMode::Leaves {
            "leaves"
        } else {
            "buckets"
        },
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
        // The cell tile is at zoom min(z,7): for z<=7 it is the tile itself, so
        // its label carries that zoom, not a hardcoded 7, or the line could not
        // round-trip back to the same cell tile_id.
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
fn leaves_text(leaves: &[LeafRun]) -> String {
    let mut out = "elivagar-corpus-leaves v1\n".to_string();
    for l in leaves {
        let (z, x, y) = tile_id_to_zxy(l.tile_id);
        out.push_str(&format!("{z} {x} {y} {} {}\n", l.run_length, hex(l.hash)));
    }
    out
}
fn write_atomic(path: &Path, text: String) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(tmp, path)
}
fn contract_text(c: &ContractDoc) -> io::Result<String> {
    let v = serde_json::json!({"schema":1,"input":c.input,"config":c.config,"build":c.build});
    serde_json::to_string_pretty(&v)
        .map(|s| format!("{s}\n"))
        .map_err(io::Error::other)
}
fn contract_from_file(path: &Path) -> io::Result<ContractDoc> {
    let text = fs::read_to_string(path)?;
    match provenance::extract_contract(&format!("{{\"elivagar\":{text}}}")) {
        ContractState::Contract(c) => Ok(c),
        s => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid corpus contract: {s:?}"),
        )),
    }
}
fn candidate_contract(a: &ArchiveView) -> io::Result<ContractDoc> {
    match provenance::extract_contract(&a.metadata()?) {
        ContractState::Contract(c) => Ok(c),
        s => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("archive contract refused: {s:?}"),
        )),
    }
}
/// The committed digest parsed back in full, so a baseline can be checked for
/// internal consistency before an archive is ever opened.
struct Baseline {
    mode: DigestMode,
    root: u128,
    broot: Option<u128>,
    tiles: u64,
    entries: u64,
    unique: u64,
    zooms: Vec<ZoomDigest>,
    buckets: Vec<BucketDigest>,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn parse_baseline(path: &Path) -> io::Result<Baseline> {
    let text = fs::read_to_string(path)?;
    let mut mode = None;
    let mut root = None;
    let mut broot = None;
    let mut counts = None;
    let mut zooms = Vec::new();
    let mut buckets = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let p: Vec<&str> = line.split_whitespace().collect();
        match p.first().copied() {
            Some("elivagar-corpus-digest") if n == 0 => {}
            Some("mode") => {
                mode = match p.get(1) {
                    Some(&"leaves") => Some(DigestMode::Leaves),
                    Some(&"buckets") => Some(DigestMode::Buckets),
                    _ => return Err(invalid("digest mode invalid")),
                };
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
                // tiles N entries N unique N
                let t = parse_u64(p.get(1).copied())?;
                let e = parse_u64(p.get(3).copied())?;
                let u = parse_u64(p.get(5).copied())?;
                counts = Some((t, e, u));
            }
            Some("zoom") => {
                // zoom Z tiles N hash HEX
                zooms.push(ZoomDigest {
                    z: parse_u8(p.get(1).copied())?,
                    tiles: parse_u64(p.get(3).copied())?,
                    hash: parse_hex(p.get(5).ok_or_else(|| invalid("zoom missing hash"))?)?,
                });
            }
            Some("bucket") => {
                // bucket z=Z cell=cz/x/y tiles=N hash HEX
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
    Ok(xy_to_tile_id(z, x, y))
}

fn parse_leaves(path: &Path) -> io::Result<Vec<LeafRun>> {
    let text = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if n == 0 {
            if line != "elivagar-corpus-leaves v1" {
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
        out.push(LeafRun {
            tile_id: xy_to_tile_id(z, x, y),
            run_length,
            hash,
        });
    }
    Ok(out)
}
fn parse_u32(token: Option<&str>) -> io::Result<u32> {
    token
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| invalid("expected coordinate field"))
}

/// Refuse a baseline whose committed rows do not reproduce their own roots.
/// Runs in microseconds and closes the hand-edit / merge-damage hole, which is
/// otherwise invisible in the opaque bucket mode.
fn baseline_inconsistency(base: &Baseline, leaves: Option<&[LeafRun]>) -> Option<String> {
    if root(&base.zooms) != base.root {
        return Some("root does not match committed zoom rows".into());
    }
    if base.mode == DigestMode::Buckets {
        match base.broot {
            None => return Some("bucket mode is missing broot".into()),
            Some(br) if bucket_root(&base.buckets) != br => {
                return Some("broot does not match committed bucket rows".into());
            }
            Some(_) => {}
        }
    } else if base.broot.is_some() {
        return Some("leaves mode carries an unexpected broot".into());
    }
    if let Some(leaves) = leaves {
        let recomputed = fold_leaves(leaves, base.mode);
        if recomputed.root != base.root {
            return Some("committed leaves do not reproduce root".into());
        }
        if !zooms_equal(&recomputed.zooms, &base.zooms) {
            return Some("committed leaves do not reproduce zoom rows".into());
        }
        if (recomputed.tiles, recomputed.entries, recomputed.unique)
            != (base.tiles, base.entries, base.unique)
        {
            return Some("committed leaves do not reproduce counts".into());
        }
    }
    None
}
fn zooms_equal(a: &[ZoomDigest], b: &[ZoomDigest]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.z == y.z && x.tiles == y.tiles && x.hash == y.hash)
}

fn format_guard(a: &ArchiveView) -> Option<String> {
    if a.tile_type() != 1 {
        return Some(format!("archive tile type {} is not MVT", a.tile_type()));
    }
    if a.tile_compression() != 2 {
        return Some(format!(
            "archive tile compression {} is not gzip",
            a.tile_compression()
        ));
    }
    None
}

fn refused(message: impl Into<String>) -> (CorpusVerdict, CheckReport) {
    (
        CorpusVerdict::Refused,
        CheckReport {
            message: message.into(),
            ..Default::default()
        },
    )
}

fn input_name(c: &ContractDoc) -> Option<&str> {
    c.input.get("name").and_then(serde_json::Value::as_str)
}

const DIFF_CAP: usize = 100;

#[derive(Default)]
struct LeafDiff {
    // index 0 changed, 1 added, 2 removed
    counts: [u64; 3],
    lines: [Vec<String>; 3],
    cur: Option<(usize, u64, u64, u128, u128)>,
}
impl LeafDiff {
    fn push(&mut self, class: usize, id: u64, oldh: u128, newh: u128) {
        if let Some((c, start, len, o, nh)) = self.cur.as_mut()
            && *c == class
            && *o == oldh
            && *nh == newh
            && id == *start + *len
            && tile_id_to_zxy(*start).0 == tile_id_to_zxy(id).0
        {
            *len += 1;
            return;
        }
        self.flush();
        self.cur = Some((class, id, 1, oldh, newh));
    }
    fn flush(&mut self) {
        let Some((class, start, len, oldh, newh)) = self.cur.take() else {
            return;
        };
        self.counts[class] += 1;
        if self.lines[class].len() < DIFF_CAP {
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

fn leaf_diff(committed: &[LeafRun], current: &[LeafRun]) -> (u64, Vec<String>) {
    let old = expand(committed);
    let new = expand(current);
    let mut st = LeafDiff::default();
    let (mut i, mut j) = (0usize, 0usize);
    while i < old.len() || j < new.len() {
        let take_old = j >= new.len() || (i < old.len() && old[i].0 <= new[j].0);
        let take_new = i >= old.len() || (j < new.len() && new[j].0 <= old[i].0);
        if take_old && take_new {
            let (id, oh) = old[i];
            let nh = new[j].1;
            if oh != nh {
                st.push(0, id, oh, nh);
            }
            i += 1;
            j += 1;
        } else if take_old {
            let (id, oh) = old[i];
            st.push(2, id, oh, 0);
            i += 1;
        } else {
            let (id, nh) = new[j];
            st.push(1, id, 0, nh);
            j += 1;
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
fn expand(leaves: &[LeafRun]) -> Vec<(u64, u128)> {
    let mut out = Vec::new();
    for l in leaves {
        for id in l.tile_id..l.tile_id + u64::from(l.run_length) {
            out.push((id, l.hash));
        }
    }
    out
}

fn zoom_delta_lines(committed: &[ZoomDigest], current: &[ZoomDigest]) -> Vec<String> {
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

fn bucket_delta_lines(committed: &[BucketDigest], current: &[BucketDigest]) -> (u64, Vec<String>) {
    let cmap: BTreeMap<(u8, u64), &BucketDigest> =
        committed.iter().map(|b| ((b.z, b.cell), b)).collect();
    let nmap: BTreeMap<(u8, u64), &BucketDigest> =
        current.iter().map(|b| ((b.z, b.cell), b)).collect();
    let mut keys: BTreeSet<(u8, u64)> = BTreeSet::new();
    keys.extend(cmap.keys().copied());
    keys.extend(nmap.keys().copied());
    let mut out = Vec::new();
    let mut changed = 0u64;
    for key @ (z, cell) in keys {
        let ct = cmap.get(&key).map_or(0, |b| b.tiles);
        let nt = nmap.get(&key).map_or(0, |b| b.tiles);
        let hash_changed = match (cmap.get(&key), nmap.get(&key)) {
            (Some(a), Some(b)) => a.hash != b.hash,
            _ => true,
        };
        if ct != nt || hash_changed {
            changed += 1;
            if out.len() < DIFF_CAP {
                let (cz, x, y) = tile_id_to_zxy(cell);
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

#[allow(clippy::too_many_lines)]
pub fn check(archive: &Path, corpus_dir: &Path) -> io::Result<(CorpusVerdict, CheckReport)> {
    let start = Instant::now();
    let base = parse_baseline(&corpus_dir.join("digest"))?;
    let committed_leaves = if base.mode == DigestMode::Leaves {
        let path = corpus_dir.join("leaves");
        if !path.is_file() {
            return Ok(refused("baseline leaves missing"));
        }
        Some(parse_leaves(&path)?)
    } else {
        None
    };
    if let Some(msg) = baseline_inconsistency(&base, committed_leaves.as_deref()) {
        return Ok(refused(format!("baseline internally inconsistent: {msg}")));
    }
    let a = ArchiveView::open(archive)?;
    if let Some(msg) = format_guard(&a) {
        return Ok(refused(msg));
    }
    let base_contract = contract_from_file(&corpus_dir.join("contract.json"))?;
    let candidate = candidate_contract(&a)?;
    let diffs = provenance::contract_diff(&base_contract, &candidate);
    if !diffs.is_empty() {
        return Ok((
            CorpusVerdict::Refused,
            CheckReport {
                message: "contract mismatch".into(),
                contract_diffs: diffs,
                ..Default::default()
            },
        ));
    }
    let mut warnings = Vec::new();
    if input_name(&base_contract) != input_name(&candidate) {
        warnings.push(format!(
            "input name differs (diagnostic only): {:?} vs {:?}",
            input_name(&base_contract),
            input_name(&candidate)
        ));
    }
    let (d, current_leaves) = compute(&a, base.mode)?;
    let pass = d.root == base.root && (base.mode != DigestMode::Buckets || d.broot == base.broot);
    if pass {
        return Ok((
            CorpusVerdict::Pass,
            CheckReport {
                message: format!(
                    "{} tiles, {} unique, {} ms",
                    d.tiles,
                    d.unique,
                    start.elapsed().as_millis()
                ),
                warnings,
                ..Default::default()
            },
        ));
    }
    let mut lines = vec!["content mismatch".to_string()];
    lines.extend(zoom_delta_lines(&base.zooms, &d.zooms));
    let changed = match base.mode {
        DigestMode::Leaves => {
            let (n, diff_lines) =
                leaf_diff(committed_leaves.as_deref().unwrap_or(&[]), &current_leaves);
            lines.extend(diff_lines);
            n
        }
        DigestMode::Buckets => {
            let (n, diff_lines) = bucket_delta_lines(&base.buckets, &d.buckets);
            lines.extend(diff_lines);
            n
        }
    };
    Ok((
        CorpusVerdict::ContentMismatch,
        CheckReport {
            message: lines.join("\n"),
            warnings,
            changed,
            ..Default::default()
        },
    ))
}

#[allow(clippy::too_many_lines)]
pub fn bless(
    archive: &Path,
    corpus_dir: &Path,
    mode: DigestMode,
    rotate: bool,
) -> io::Result<(CorpusVerdict, CheckReport)> {
    let a = ArchiveView::open(archive)?;
    if let Some(msg) = format_guard(&a) {
        return Ok(refused(msg));
    }
    let contract = candidate_contract(&a)?;
    let mut warnings = Vec::new();
    for repo in ["elivagar", "pbfhogg_reader"] {
        match contract
            .build
            .get(repo)
            .and_then(|value| value.get("dirty"))
        {
            Some(serde_json::Value::Bool(true)) => {
                return Ok(refused(format!("candidate build is dirty: {repo}")));
            }
            Some(serde_json::Value::Bool(false)) => {}
            // A null (or absent) dirty flag means git was unavailable at build
            // time: reproducibility is UNPROVEN, not disproven. Warn and proceed.
            _ => warnings.push(format!(
                "{repo} build dirty flag is null/absent: reproducibility unproven"
            )),
        }
    }
    if contract
        .input
        .pointer("/features/locations_on_ways")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Ok(refused("candidate is not locations-generated"));
    }
    if corpus_dir.join("digest").exists() && !rotate {
        // Rotation comparison: name the contract diffs and content deltas, then
        // refuse to write. bless is the one path allowed to cross a contract
        // boundary, so this is exit 1 (unadjudicated rotation), never exit 2.
        let base = parse_baseline(&corpus_dir.join("digest"))?;
        let committed_leaves =
            if base.mode == DigestMode::Leaves && corpus_dir.join("leaves").is_file() {
                Some(parse_leaves(&corpus_dir.join("leaves"))?)
            } else {
                None
            };
        let (d, current_leaves) = compute(&a, base.mode)?;
        let mut lines = vec!["rotation requires --rotate".to_string()];
        if let Ok(base_contract) = contract_from_file(&corpus_dir.join("contract.json")) {
            for path in provenance::contract_diff(&base_contract, &contract) {
                lines.push(format!("contract {path}"));
            }
        }
        lines.extend(zoom_delta_lines(&base.zooms, &d.zooms));
        match base.mode {
            DigestMode::Leaves => {
                let (_, diff_lines) =
                    leaf_diff(committed_leaves.as_deref().unwrap_or(&[]), &current_leaves);
                lines.extend(diff_lines);
            }
            DigestMode::Buckets => {
                let (_, diff_lines) = bucket_delta_lines(&base.buckets, &d.buckets);
                lines.extend(diff_lines);
            }
        }
        return Ok((
            CorpusVerdict::ContentMismatch,
            CheckReport {
                message: lines.join("\n"),
                warnings,
                ..Default::default()
            },
        ));
    }
    let (d, l) = compute(&a, mode)?;
    fs::create_dir_all(corpus_dir)?;
    let digest_path = corpus_dir.join("digest");
    let contract_path = corpus_dir.join("contract.json");
    let leaves_path = corpus_dir.join("leaves");
    write_atomic(&digest_path, digest_text(&d))?;
    write_atomic(&contract_path, contract_text(&contract)?)?;
    if mode == DigestMode::Leaves {
        write_atomic(&leaves_path, leaves_text(&l))?;
    } else if leaves_path.exists() {
        fs::remove_file(&leaves_path)?;
    }
    let size = |p: &Path| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let leaves_note = if mode == DigestMode::Leaves {
        format!(", leaves {}B", size(&leaves_path))
    } else {
        String::new()
    };
    Ok((
        CorpusVerdict::Pass,
        CheckReport {
            message: format!(
                "blessed {} tiles ({}); digest {}B, contract {}B{}",
                d.tiles,
                if mode == DigestMode::Leaves {
                    "leaves"
                } else {
                    "buckets"
                },
                size(&digest_path),
                size(&contract_path),
                leaves_note
            ),
            warnings,
            ..Default::default()
        },
    ))
}

/// Rewrite an archive with one controlled mutation while preserving its PMTiles
/// header configuration and metadata byte-for-byte after decompression.
pub fn mutate(
    input: &Path,
    output: &Path,
    target: Option<(u8, u32, u32)>,
    op: MutationOp,
) -> io::Result<()> {
    if op != MutationOp::Regzip && target.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--tile is required for this mutation",
        ));
    }
    let archive = ArchiveView::open(input)?;
    let target_id = target.map(|(z, x, y)| xy_to_tile_id(z, x, y));
    let mut found = op == MutationOp::Regzip;
    let header = archive.header();
    let e7 = |offset| f64::from(read_i32_le(header, offset)) / 10_000_000.0;
    let config = PmtilesConfig {
        min_zoom: archive.min_zoom(),
        max_zoom: archive.max_zoom(),
        bounds: (e7(102), e7(106), e7(110), e7(114)),
        center: (e7(119), e7(123), header[118]),
    };
    let mut writer = PmtilesWriter::new(config);
    writer.set_metadata_verbatim(archive.metadata()?);
    for run in archive.read_all_runs()? {
        let is_target = op != MutationOp::Regzip
            && target_id.is_some_and(|id| {
                id >= run.tile_id && id < run.tile_id + u64::from(run.run_length)
            });
        if is_target {
            found = true;
            let id = target_id.expect("checked above");
            copy_run(&mut writer, &archive, run, run.tile_id, id - run.tile_id)?;
            match op {
                MutationOp::DropTile => {}
                MutationOp::NudgeGeometry | MutationOp::LayerVersion => {
                    let raw = archive.raw_blob(BlobRef {
                        offset: run.offset,
                        length: run.length,
                    })?;
                    let edited = mutate_payload(raw, op)?;
                    writer.add_run(id, 1, &edited)?;
                }
                MutationOp::Regzip => unreachable!(),
            }
            let after = id + 1;
            copy_run(
                &mut writer,
                &archive,
                run,
                after,
                run.tile_id + u64::from(run.run_length) - after,
            )?;
        } else if op == MutationOp::Regzip {
            let raw = archive.raw_blob(BlobRef {
                offset: run.offset,
                length: run.length,
            })?;
            let recompressed = regzip(raw)?;
            writer.add_run(run.tile_id, run.run_length, &recompressed)?;
        } else {
            copy_run(
                &mut writer,
                &archive,
                run,
                run.tile_id,
                u64::from(run.run_length),
            )?;
        }
    }
    if !found {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "target tile is not addressed by archive",
        ));
    }
    writer.write_to(output)
}

fn copy_run(
    writer: &mut PmtilesWriter,
    archive: &ArchiveView,
    run: RawDirEntry,
    tile_id: u64,
    length: u64,
) -> io::Result<()> {
    if length != 0 {
        let length = u32::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "run too long"))?;
        writer.add_run(
            tile_id,
            length,
            archive.raw_blob(BlobRef {
                offset: run.offset,
                length: run.length,
            })?,
        )?;
    }
    Ok(())
}

fn regzip(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::new();
    GzDecoder::new(raw).read_to_end(&mut decoded)?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(9));
    encoder.write_all(&decoded)?;
    encoder.finish()
}

#[derive(Clone)]
struct Field {
    number: u32,
    wire: u8,
    value: Vec<u8>,
}

fn read_varint(input: &[u8], at: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *input.get(*at).ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "truncated protobuf varint")
        })?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "protobuf varint overflows u64",
    ))
}
fn fields(input: &[u8]) -> io::Result<Vec<Field>> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < input.len() {
        let tag = read_varint(input, &mut at)?;
        let number = u32::try_from(tag >> 3)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "field number too large"))?;
        let wire = u8::try_from(tag & 7).unwrap_or(u8::MAX);
        let value = match wire {
            0 => {
                let start = at;
                read_varint(input, &mut at)?;
                input[start..at].to_vec()
            }
            1 => take(input, &mut at, 8)?,
            2 => {
                let n = usize::try_from(read_varint(input, &mut at)?)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "length too large"))?;
                take(input, &mut at, n)?
            }
            5 => take(input, &mut at, 4)?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported protobuf wire type {wire}"),
                ));
            }
        };
        out.push(Field {
            number,
            wire,
            value,
        });
    }
    Ok(out)
}
fn take(input: &[u8], at: &mut usize, n: usize) -> io::Result<Vec<u8>> {
    let end = at
        .checked_add(n)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "protobuf length overflows"))?;
    let value = input
        .get(*at..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated protobuf field"))?
        .to_vec();
    *at = end;
    Ok(value)
}
fn encode_fields(fields: &[Field]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        encode_varint(
            &mut out,
            u64::from(field.number) << 3 | u64::from(field.wire),
        );
        if field.wire == 2 {
            encode_varint(&mut out, field.value.len() as u64);
        }
        out.extend_from_slice(&field.value);
    }
    out
}
fn mutate_payload(raw: &[u8], op: MutationOp) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::new();
    GzDecoder::new(raw).read_to_end(&mut decoded)?;
    let mut tile = fields(&decoded)?;
    let layer = tile
        .iter_mut()
        .find(|f| f.number == 3 && f.wire == 2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tile has no layer"))?;
    let mut layer_fields = fields(&layer.value)?;
    match op {
        MutationOp::LayerVersion => {
            if let Some(version) = layer_fields
                .iter_mut()
                .find(|f| f.number == 15 && f.wire == 0)
            {
                version.value = vec![3];
            } else {
                layer_fields.push(Field {
                    number: 15,
                    wire: 0,
                    value: vec![3],
                });
            }
        }
        MutationOp::NudgeGeometry => {
            let feature_index = layer_fields
                .iter()
                .enumerate()
                .find_map(|(index, field)| {
                    (field.number == 2
                        && field.wire == 2
                        && fields(&field.value)
                            .ok()?
                            .iter()
                            .any(|nested| nested.number == 4 && nested.wire == 2))
                    .then_some(index)
                })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "layer has no feature geometry")
                })?;
            let feature = &mut layer_fields[feature_index];
            let mut feature_fields = fields(&feature.value)?;
            let geometry = feature_fields
                .iter_mut()
                .find(|f| f.number == 4 && f.wire == 2)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "feature has no geometry")
                })?;
            geometry.value = nudge_geometry(&geometry.value)?;
            feature.value = encode_fields(&feature_fields);
        }
        _ => unreachable!(),
    }
    layer.value = encode_fields(&layer_fields);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&encode_fields(&tile))?;
    encoder.finish()
}
fn nudge_geometry(input: &[u8]) -> io::Result<Vec<u8>> {
    let mut at = 0;
    while at < input.len() {
        let start = at;
        let command = read_varint(input, &mut at)?;
        if command & 7 == 1 && command >> 3 != 0 {
            let parameter = read_varint(input, &mut at)?;
            let magnitude = i64::try_from(parameter >> 1).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "geometry delta too large")
            })?;
            let delta = if parameter & 1 == 0 {
                magnitude
            } else {
                -magnitude - 1
            };
            let changed_delta = delta.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "geometry delta overflow")
            })?;
            let changed = if changed_delta >= 0 {
                u64::try_from(changed_delta).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "geometry delta conversion failed",
                    )
                })? * 2
            } else {
                changed_delta.unsigned_abs() * 2 - 1
            };
            let mut out = input[..start].to_vec();
            encode_varint(&mut out, command);
            encode_varint(&mut out, changed);
            out.extend_from_slice(&input[at..]);
            return Ok(out);
        }
        let pairs = usize::try_from(command >> 3)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "geometry count too large"))?;
        for _ in 0..pairs.saturating_mul(2) {
            read_varint(input, &mut at)?;
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "geometry has no MoveTo",
    ))
}

#[cfg(test)]
mod mutation_tests {
    #![allow(clippy::unwrap_used, clippy::let_underscore_must_use)]

    use super::*;

    fn fixture_payload() -> Vec<u8> {
        // One point feature: geometry is MoveTo(1), x=0, y=0.
        let feature = encode_fields(&[
            Field {
                number: 3,
                wire: 0,
                value: vec![1],
            },
            Field {
                number: 4,
                wire: 2,
                value: vec![9, 0, 0],
            },
        ]);
        let layer = encode_fields(&[
            Field {
                number: 1,
                wire: 2,
                value: b"test".to_vec(),
            },
            Field {
                number: 2,
                wire: 2,
                value: feature,
            },
            Field {
                number: 5,
                wire: 0,
                value: vec![0x80, 0x20],
            },
            Field {
                number: 15,
                wire: 0,
                value: vec![2],
            },
        ]);
        let tile = encode_fields(&[Field {
            number: 3,
            wire: 2,
            value: layer,
        }]);
        let mut gzip = GzEncoder::new(Vec::new(), Compression::new(6));
        gzip.write_all(&tile).unwrap();
        gzip.finish().unwrap()
    }

    fn test_dir() -> std::path::PathBuf {
        let path = std::path::PathBuf::from("target")
            .join(format!("corpus-mutation-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn source(path: &Path) -> (u64, Vec<u8>) {
        let payload = fixture_payload();
        let mut writer = PmtilesWriter::new(PmtilesConfig {
            min_zoom: 1,
            max_zoom: 1,
            bounds: (0.0, 0.0, 1.0, 1.0),
            center: (0.0, 0.0, 1),
        });
        let first = xy_to_tile_id(1, 0, 0);
        writer.add_run(first, 4, &payload).unwrap();
        writer.write_to(path).unwrap();
        (first + 1, payload)
    }

    fn leaf_hashes(leaves: &[LeafRun]) -> BTreeMap<u64, u128> {
        leaves
            .iter()
            .flat_map(|leaf| {
                (leaf.tile_id..leaf.tile_id + u64::from(leaf.run_length))
                    .map(move |id| (id, leaf.hash))
            })
            .collect()
    }

    #[test]
    fn mutations_are_isolated_and_regzip_is_semantically_neutral() {
        let dir = test_dir();
        let input = dir.join("source.pmtiles");
        let (target, _) = source(&input);
        let before = ArchiveView::open(&input).unwrap();
        let (before_digest, before_leaves) = compute(&before, DigestMode::Leaves).unwrap();
        let before_contract = before.metadata().unwrap();

        for (op, name) in [
            (MutationOp::DropTile, "drop"),
            (MutationOp::NudgeGeometry, "nudge"),
            (MutationOp::LayerVersion, "version"),
        ] {
            let output = dir.join(format!("{name}.pmtiles"));
            mutate(&input, &output, Some(tile_id_to_zxy(target)), op).unwrap();
            let after = ArchiveView::open(&output).unwrap();
            assert_eq!(
                after.metadata().unwrap(),
                before_contract,
                "{name} contract changed"
            );
            let (digest, leaves) = compute(&after, DigestMode::Leaves).unwrap();
            assert_ne!(
                digest.root, before_digest.root,
                "{name} did not change digest"
            );
            let old = leaf_hashes(&before_leaves);
            let new = leaf_hashes(&leaves);
            for (&id, &hash) in &old {
                if id != target {
                    assert_eq!(new.get(&id), Some(&hash), "{name} changed non-target {id}");
                }
            }
        }

        let output = dir.join("regzip.pmtiles");
        mutate(&input, &output, None, MutationOp::Regzip).unwrap();
        let after = ArchiveView::open(&output).unwrap();
        let (digest, leaves) = compute(&after, DigestMode::Leaves).unwrap();
        assert_eq!(after.metadata().unwrap(), before_contract);
        assert_eq!(digest.root, before_digest.root);
        assert_eq!(leaves, before_leaves);
        assert_ne!(fs::read(&input).unwrap(), fs::read(&output).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod format_tests {
    #![allow(clippy::unwrap_used, clippy::let_underscore_must_use)]

    use super::*;

    fn payload() -> Vec<u8> {
        let feature = encode_fields(&[
            Field {
                number: 3,
                wire: 0,
                value: vec![1],
            },
            Field {
                number: 4,
                wire: 2,
                value: vec![9, 0, 0],
            },
        ]);
        let layer = encode_fields(&[
            Field {
                number: 1,
                wire: 2,
                value: b"t".to_vec(),
            },
            Field {
                number: 2,
                wire: 2,
                value: feature,
            },
            Field {
                number: 5,
                wire: 0,
                value: vec![0x80, 0x20],
            },
        ]);
        let tile = encode_fields(&[Field {
            number: 3,
            wire: 2,
            value: layer,
        }]);
        let mut gz = GzEncoder::new(Vec::new(), Compression::new(6));
        gz.write_all(&tile).unwrap();
        gz.finish().unwrap()
    }

    fn dir(tag: &str) -> std::path::PathBuf {
        let p = std::path::PathBuf::from("target")
            .join(format!("corpus-fmt-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn canonical_runs_merge_within_zoom_but_split_at_boundaries() {
        let d = dir("merge");
        let path = d.join("a.pmtiles");
        let mut w = PmtilesWriter::new(PmtilesConfig {
            min_zoom: 0,
            max_zoom: 1,
            bounds: (0.0, 0.0, 1.0, 1.0),
            center: (0.0, 0.0, 0),
        });
        // One shared blob spanning tile ids 0..5: z0 tile (id 0) plus the four
        // z1 tiles (ids 1..5), a single directory run crossing the z0/z1 line.
        w.add_run(0, 5, &payload()).unwrap();
        w.write_to(&path).unwrap();
        let a = ArchiveView::open(&path).unwrap();
        let (digest, leaves) = compute(&a, DigestMode::Leaves).unwrap();
        assert_eq!(digest.tiles, 5);
        assert_eq!(digest.unique, 1);
        // The shared blob must NOT merge into one canonical run across zooms.
        assert_eq!(digest.entries, 2);
        assert_eq!(leaves.len(), 2);
        assert_eq!(tile_id_to_zxy(leaves[0].tile_id).0, 0);
        assert_eq!(leaves[0].run_length, 1);
        assert_eq!(tile_id_to_zxy(leaves[1].tile_id).0, 1);
        assert_eq!(leaves[1].run_length, 4);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn leaves_digest_round_trips_and_self_checks() {
        let d = dir("leaves");
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
        let dg = fold_leaves(&leaves, DigestMode::Leaves);
        write_atomic(&d.join("digest"), digest_text(&dg)).unwrap();
        write_atomic(&d.join("leaves"), leaves_text(&leaves)).unwrap();
        let base = parse_baseline(&d.join("digest")).unwrap();
        let parsed = parse_leaves(&d.join("leaves")).unwrap();
        assert_eq!(parsed, leaves);
        assert_eq!(base.root, dg.root);
        assert!(base.broot.is_none());
        assert!(baseline_inconsistency(&base, Some(&parsed)).is_none());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn bucket_mode_groups_by_z7_ancestor_and_round_trips_broot() {
        let d = dir("bucket");
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
        let dg = fold_leaves(&leaves, DigestMode::Buckets);
        assert!(dg.broot.is_some());
        // z<=7 buckets are per-tile; z>7 folds to the z7 ancestor cell.
        assert!(dg.buckets.iter().any(|b| b.z == 5 && b.cell == z5));
        let ancestor = xy_to_tile_id(7, 100 >> 1, 100 >> 1);
        assert!(dg.buckets.iter().any(|b| b.z == 8 && b.cell == ancestor));
        write_atomic(&d.join("digest"), digest_text(&dg)).unwrap();
        let base = parse_baseline(&d.join("digest")).unwrap();
        assert_eq!(base.broot, dg.broot);
        // The cell label must round-trip through parse (the z7-vs-z<=7 label fix).
        assert!(baseline_inconsistency(&base, None).is_none());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn self_consistency_rejects_a_tampered_root() {
        let d = dir("tamper");
        let leaves = vec![LeafRun {
            tile_id: xy_to_tile_id(2, 1, 1),
            run_length: 1,
            hash: 7,
        }];
        let dg = fold_leaves(&leaves, DigestMode::Leaves);
        let text = digest_text(&dg).replace(&hex(dg.root), &format!("{:032x}", dg.root ^ 1));
        fs::write(d.join("digest"), text).unwrap();
        let base = parse_baseline(&d.join("digest")).unwrap();
        assert!(baseline_inconsistency(&base, Some(&leaves)).is_some());
        fs::remove_dir_all(d).unwrap();
    }
}
