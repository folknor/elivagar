use super::*;
use crate::shortbread::Layer;
use crate::tiling::{MAX_ZOOM, PROFILE_ID_END, TileId, ZOOM_COUNT, zoom_ids};
use std::fmt;

/// Zoom level used to split each zoom block into ordered Hilbert ranges.
///
/// For z14 this makes each partition exactly one z7 Hilbert prefix, or 16,384
/// child tile ids. The previous equal-width split over the whole PMTiles id
/// space left Germany with one 8 GB hot partition, which erased the assemble
/// parallelism the partitioned path was meant to expose. z6 prefixes were
/// still too coarse for country extracts: germany spans only ~8 z6 prefixes
/// at z14, and its fattest partition encoded 921 MB - a third of total
/// output behind one claim-window slot.
pub(super) const PARTITION_SPLIT_Z: u8 = 7;

/// `PARTITION_BASES[z]` is the first partition id of profile zoom `z`;
/// `PARTITION_BASES[ZOOM_COUNT]` is the partition count.
pub(super) const PARTITION_BASES: [usize; ZOOM_COUNT + 1] = {
    let mut bases = [0usize; ZOOM_COUNT + 1];
    let mut z = 0usize;
    let mut acc = 0usize;
    while z < ZOOM_COUNT {
        bases[z] = acc;
        let split_z = if z < PARTITION_SPLIT_Z as usize {
            z
        } else {
            PARTITION_SPLIT_Z as usize
        };
        acc += 1usize << (2 * split_z);
        z += 1;
    }
    bases[ZOOM_COUNT] = acc;
    bases
};

/// Number of ordered partition ids produced by the `PARTITION_SPLIT_Z` scheme.
pub const SORT_PARTITIONS: usize = PARTITION_BASES[ZOOM_COUNT];
// ---------------------------------------------------------------------------
// Sort key helpers
// ---------------------------------------------------------------------------

/// Sort key: a u64 packing `(tile_id << 16) | (layer << 8) | paint_rank`.
///
/// Packed rather than a `{ tile, layer, rank }` struct so the dominant chunk
/// sort stays an integer sort on `raw()` and `SortRecord` stays 24 bytes;
/// numeric order of the packed value is the record order. The layer is a
/// [`Layer`] at mint, so the two adjacent bytes cannot be swapped. The rank
/// stays a plain `u8`: the kind methods that produce it are its only source.
///
/// Keys read back from chunk files come through [`SortKey::from_raw`] and are
/// not validated there, so the decoders that can meet a malformed key
/// ([`SortKey::layer`], [`SortKey::partition`]) refuse rather than trust it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct SortKey(u64);

impl SortKey {
    /// Above every minted key: the open end of the last partition.
    pub const MAX: Self = Self(u64::MAX);

    /// Mint a key for a feature of `layer` on `tile`.
    ///
    /// Packs the tile id into bits 63-16, a 48-bit field that would drop bits
    /// above z23, so the tile must lie in the profile domain
    /// (`z <= MAX_ZOOM`). That is checked here, at the one place keys are
    /// minted, rather than trusted from the zoom validation in `run`: a key
    /// that lost bits would decode to a different, valid-looking tile and be
    /// misfiled silently.
    #[inline]
    pub fn new(tile: TileId, layer: Layer, rank: u8) -> Self {
        assert!(
            tile.is_profile(),
            "sort key minted for tile {tile} beyond z{MAX_ZOOM}"
        );
        Self((tile.raw() << 16) | (u64::from(layer as u8) << 8) | u64::from(rank))
    }

    /// A key as stored on the chunk wire. Unchecked; see the type docs.
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub fn tile(self) -> TileId {
        // 48 bits always fit below the archive id ceiling.
        TileId::new(self.0 >> 16).expect("a 48-bit tile id is addressable")
    }

    /// The layer byte as stored, for diagnostics on a key [`Self::layer`]
    /// refuses.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn layer_byte(self) -> u8 {
        ((self.0 >> 8) & 0xFF) as u8
    }

    /// The layer, or `None` for a layer byte no [`Layer`] has.
    #[inline]
    pub fn layer(self) -> Option<Layer> {
        Layer::ALL.get(usize::from(self.layer_byte())).copied()
    }

    /// The paint rank within the tile and layer.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn rank(self) -> u8 {
        (self.0 & 0xFF) as u8
    }

    /// The sort partition holding this key.
    ///
    /// Partitions cover the profile domain only. A key beyond it cannot have
    /// been minted by [`SortKey::new`], so it is refused loudly rather than
    /// clamped into the last partition.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn partition(self) -> usize {
        let tile = self.tile();
        assert!(
            tile.is_profile(),
            "sort key for tile {tile} lies beyond z{MAX_ZOOM}"
        );
        let zoom = tile.zoom();
        let local_id = tile.raw() - zoom_ids(zoom).0;
        let split_z = zoom.min(PARTITION_SPLIT_Z);
        let shift = 2 * u32::from(zoom - split_z);
        let prefix = (local_id >> shift) as usize;
        let partition = PARTITION_BASES[zoom as usize] + prefix;
        debug_assert!(partition < SORT_PARTITIONS);
        partition
    }
}

impl fmt::Display for SortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Total order on sort records. Payload bytes are a pure function of a
/// feature, so this is independent of producer scheduling and chunk layout.
/// Used by the k-way merge heap, where every comparison already touches the
/// record anyway. The chunk-write funnels must NOT use a fused comparator like
/// this one: see `sort_records_total` / `sort_payload_records_total`.
#[inline]
pub(super) fn record_cmp(a_key: SortKey, a_data: &[u8], b_key: SortKey, b_data: &[u8]) -> Ordering {
    a_key.cmp(&b_key).then_with(|| a_data.cmp(b_data))
}

/// Walk runs of equal keys in a key-sorted slice, handing each multi-record
/// run to `sort_run` for payload ordering. Records with equal (key, payload)
/// serialize to identical bytes, so their relative order is irrelevant and
/// unstable sorting inside a run is fine.
fn for_each_equal_key_run<T>(
    records: &mut [T],
    key_of: impl Fn(&T) -> SortKey,
    mut sort_run: impl FnMut(&mut [T]),
) {
    let mut start = 0;
    while start < records.len() {
        let key = key_of(&records[start]);
        let mut end = start + 1;
        while end < records.len() && key_of(&records[end]) == key {
            end += 1;
        }
        if end - start > 1 {
            sort_run(&mut records[start..end]);
        }
        start = end;
    }
}

/// Sort records into the total record order (key, then payload bytes) in two
/// passes: a pure u64 key sort, then a payload-order pass over each equal-key
/// run. The split keeps the dominant pass on the trivial integer comparator
/// (the `sort_unstable_by_key` fast path); payload indirection is paid only
/// inside actual key ties, where the determinism guarantee needs it. The
/// original single fused comparator regressed phase12 chunk sorting ~28% on
/// the denmark bench because payload access costs applied to every comparison,
/// ties or not.
pub(super) fn sort_records_total(records: &mut [SortRecord]) {
    records.sort_unstable_by_key(|r| r.key.raw());
    for_each_equal_key_run(
        records,
        |r| r.key,
        |run| run.sort_unstable_by(|a, b| a.data.cmp(&b.data)),
    );
}

/// First 8 payload bytes as a big-endian, zero-padded u64. Lex-consistent
/// with full payload comparison: it may collide to equal (resolved by the
/// full compare) but can never invert an ordering, so (prefix, full-lex)
/// equals full-lex.
fn payload_prefix8(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let n = bytes.len().min(8);
    buf[..n].copy_from_slice(&bytes[..n]);
    u64::from_be_bytes(buf)
}

/// `sort_records_total` for arena-backed `(key, offset, len)` records. The
/// tie pass gathers an 8-byte payload prefix once per record (one arena read
/// each) instead of comparing arena slices directly (two random reads into a
/// multi-hundred-MB arena per comparison). Payloads begin with the feature's
/// osm_id, so within a run the prefix resolves nearly every comparison and
/// the full slice compare runs only on true prefix collisions. Measured on
/// the denmark locations bench: direct within-run slice comparison cost
/// ~0.7s of wall; the by-key pass alone is baseline-neutral.
pub(super) fn sort_payload_records_total(records: &mut [PayloadRef], payload: &[u8]) {
    records.sort_unstable_by_key(|r| r.key.raw());
    let mut scratch: Vec<(u64, PayloadRef)> = Vec::new();
    for_each_equal_key_run(
        records,
        |r| r.key,
        |run| {
            scratch.clear();
            scratch.extend(run.iter().map(|&r| (payload_prefix8(r.bytes(payload)), r)));
            scratch.sort_unstable_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| a.1.bytes(payload).cmp(b.1.bytes(payload)))
            });
            for (dst, &(_, r)) in run.iter_mut().zip(scratch.iter()) {
                *dst = r;
            }
        },
    );
}

/// The first tile of a partition.
///
/// Partitions stay bare `usize`: they are dense indexes into per-partition
/// vectors, and the confusion a newtype would guard against (a partition
/// passed where a tile goes) is already a type error because tiles are
/// [`TileId`]s and keys are [`SortKey`]s. A `u16` would not hold them anyway:
/// there are 136,533.
pub fn partition_start_tile_id(partition: usize) -> TileId {
    assert!(
        partition < SORT_PARTITIONS,
        "partition {partition} out of range"
    );
    let mut zoom = MAX_ZOOM as usize;
    while partition < PARTITION_BASES[zoom] {
        zoom -= 1;
    }
    let split_z = zoom.min(PARTITION_SPLIT_Z as usize);
    let prefix = partition - PARTITION_BASES[zoom];
    #[allow(clippy::cast_possible_truncation)]
    let first = zoom_ids(zoom as u8).0;
    TileId::new(first + ((prefix as u64) << (2 * (zoom - split_z))))
        .expect("a partition start is a profile tile")
}

/// Tile-id interval `[start, end)` covered by one partition, as raw ids: the
/// end of the last partition is the first z15 id, a boundary rather than an
/// address of this domain. This is public because an external PMTiles run
/// may cross a sort-partition boundary even when that partition has no OSM
/// records.
pub fn partition_tile_range(partition: usize) -> (u64, u64) {
    let start = partition_start_tile_id(partition).raw();
    let end = if partition + 1 >= SORT_PARTITIONS {
        PROFILE_ID_END
    } else {
        partition_start_tile_id(partition + 1).raw()
    };
    (start, end)
}

/// The smallest key past `partition`. Built from the raw packing rather than
/// [`SortKey::new`] because it needs layer and rank zero, the bottom of the
/// next tile, which `new` would spell with whatever `Layer` has id 0.
pub(super) fn partition_next_key(partition: usize) -> SortKey {
    if partition + 1 >= SORT_PARTITIONS {
        SortKey::MAX
    } else {
        SortKey::from_raw(partition_start_tile_id(partition + 1).raw() << 16)
    }
}
