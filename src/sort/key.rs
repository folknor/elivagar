use super::*;

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

pub(super) const TILE_ID_BASES: [u64; 16] = {
    let mut bases = [0u64; 16];
    let mut z = 0usize;
    while z < 16 {
        bases[z] = ((1u64 << (2 * z)) - 1) / 3;
        z += 1;
    }
    bases
};

pub(super) const PARTITION_BASES: [usize; 16] = {
    let mut bases = [0usize; 16];
    let mut z = 0usize;
    let mut acc = 0usize;
    while z < 15 {
        bases[z] = acc;
        let split_z = if z < PARTITION_SPLIT_Z as usize {
            z
        } else {
            PARTITION_SPLIT_Z as usize
        };
        acc += 1usize << (2 * split_z);
        z += 1;
    }
    bases[15] = acc;
    bases
};

/// Number of ordered partition ids produced by the `PARTITION_SPLIT_Z` scheme.
pub const SORT_PARTITIONS: usize = PARTITION_BASES[15];

pub(super) const TILE_ID_LIMIT_EXCLUSIVE: u64 = TILE_ID_BASES[15];
// ---------------------------------------------------------------------------
// Sort key helpers
// ---------------------------------------------------------------------------

/// Sort key: u64 encoding `(tile_id << 16) | (layer << 8) | priority`.
pub type SortKey = u64;

/// Build a sort key from tile id, layer index, and priority.
///
/// Packs tile_id into bits 63-16 (48-bit field). Overflows above z23, but
/// max_zoom is validated to 14 at pipeline entry (max tile_id ~358M = 29 bits).
#[inline]
pub fn make_sort_key(tile_id: u64, layer: u8, priority: u8) -> SortKey {
    (tile_id << 16) | (u64::from(layer) << 8) | u64::from(priority)
}

/// Extract tile_id from a sort key.
#[inline]
pub fn tile_id_from_key(key: SortKey) -> u64 {
    key >> 16
}

/// Extract the layer index from a sort key.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn layer_from_key(key: SortKey) -> u8 {
    ((key >> 8) & 0xFF) as u8
}

/// Extract the paint-order priority from a sort key.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn priority_from_key(key: SortKey) -> u8 {
    (key & 0xFF) as u8
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
    records.sort_unstable_by_key(|r| r.key);
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
pub(super) fn sort_payload_records_total(records: &mut [PayloadRecord], payload: &[u8]) {
    records.sort_unstable_by_key(|r| r.0);
    let mut scratch: Vec<(u64, PayloadRecord)> = Vec::new();
    for_each_equal_key_run(
        records,
        |r| r.0,
        |run| {
            scratch.clear();
            scratch.extend(
                run.iter()
                    .map(|&r| (payload_prefix8(&payload[r.1..r.1 + r.2]), r)),
            );
            scratch.sort_unstable_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| payload[a.1.1..a.1.1 + a.1.2].cmp(&payload[b.1.1..b.1.1 + b.1.2]))
            });
            for (dst, &(_, r)) in run.iter_mut().zip(scratch.iter()) {
                *dst = r;
            }
        },
    );
}

/// Extract zoom level from a tile_id. Uses the PMTiles base offset formula:
/// `base(z) = (4^z - 1) / 3`. Zoom is the largest z where `base(z) <= tile_id`.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn zoom_from_tile_id(tile_id: u64) -> u8 {
    let mut z: u8 = 14;
    while z > 0 && tile_id < TILE_ID_BASES[z as usize] {
        z -= 1;
    }
    z
}

/// Return the tile-id range partition for a sort key.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn partition_from_key(key: SortKey) -> usize {
    let tile_id = tile_id_from_key(key).min(TILE_ID_LIMIT_EXCLUSIVE - 1);
    let zoom = zoom_from_tile_id(tile_id);
    let local_id = tile_id - TILE_ID_BASES[zoom as usize];
    let split_z = zoom.min(PARTITION_SPLIT_Z);
    let shift = 2 * u32::from(zoom - split_z);
    let prefix = (local_id >> shift) as usize;
    let partition = PARTITION_BASES[zoom as usize] + prefix;
    debug_assert!(partition < SORT_PARTITIONS);
    partition
}

pub fn partition_start_tile_id(partition: usize) -> u64 {
    debug_assert!(partition < SORT_PARTITIONS);
    let mut zoom = 14usize;
    while partition < PARTITION_BASES[zoom] {
        zoom -= 1;
    }
    let split_z = zoom.min(PARTITION_SPLIT_Z as usize);
    let prefix = partition - PARTITION_BASES[zoom];
    TILE_ID_BASES[zoom] + ((prefix as u64) << (2 * (zoom - split_z)))
}

/// Tile-id interval covered by one partition. This is public because an
/// external PMTiles run may cross a sort-partition boundary even when that
/// partition has no OSM records.
pub fn partition_tile_range(partition: usize) -> (u64, u64) {
    let start = partition_start_tile_id(partition);
    let end = if partition + 1 >= SORT_PARTITIONS {
        TILE_ID_LIMIT_EXCLUSIVE
    } else {
        partition_start_tile_id(partition + 1)
    };
    (start, end)
}

pub(super) fn partition_next_key(partition: usize) -> SortKey {
    if partition + 1 >= SORT_PARTITIONS {
        SortKey::MAX
    } else {
        make_sort_key(partition_start_tile_id(partition + 1), 0, 0)
    }
}
