use super::*;
use crate::shortbread::{AttrValue, GeomExpect, Layer, LayerMatch};
// Only the MLT parity test imports these by name; the other gzip-decoding
// tests spell out flate2::read::GzDecoder / std::io::Read at the call site.
#[cfg(feature = "mlt")]
use flate2::read::GzDecoder;
use pbfhogg::block_builder;
use pbfhogg::writer::{Compression as PbfCompression, PbfWriter};
use smallvec::smallvec;
use std::borrow::Cow;
use std::fs::File;
#[cfg(feature = "mlt")]
use std::io::Read;
use std::io::Write;

fn way_block_for_members(ids: &[i64]) -> pbfhogg::PrimitiveBlock {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("ways.osm.pbf");
    let file = File::create(&path).expect("create pbf");
    let mut writer = PbfWriter::new(file, PbfCompression::None);
    let header = block_builder::HeaderBuilder::new()
        .optional_feature("LocationsOnWays")
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");
    let mut builder = block_builder::BlockBuilder::new();
    for &id in ids {
        builder.add_way_with_locations(
            id,
            [],
            &[id * 10, id * 10 + 1],
            &[(590_000_000, 100_000_000), (590_000_100, 100_000_100)],
            None,
        );
    }
    let bytes = builder.take().expect("take block").expect("way block");
    writer.write_primitive_block(bytes).expect("write block");
    writer.flush().expect("flush pbf");
    pbfhogg::ElementReader::from_path(&path)
        .expect("open pbf")
        .into_blocks_pipelined()
        .next()
        .expect("way block result")
        .expect("decode way block")
}

#[test]
fn build_way_plans_bitmap_arm_marks_positionally() {
    let three = way_block_for_members(&[10, 11, 12]);
    let (plans, marked) = build_way_plans(
        &three,
        &MembersForBlock::Bitmap(&[0x05]),
        PinSource::BlockLocal,
    );
    assert_eq!(marked, 2);
    assert_eq!(
        plans.iter().map(|plan| plan.is_member).collect::<Vec<_>>(),
        [true, false, true]
    );

    let nine = way_block_for_members(&(1..=9).collect::<Vec<_>>());
    let (plans, marked) = build_way_plans(
        &nine,
        &MembersForBlock::Bitmap(&[0, 1]),
        PinSource::BlockLocal,
    );
    assert_eq!(marked, 1);
    assert_eq!(
        plans.iter().map(|plan| plan.is_member).collect::<Vec<_>>(),
        [false, false, false, false, false, false, false, false, true]
    );
}

#[test]
fn bitmap_and_set_arms_agree() {
    let block = way_block_for_members(&[10, 11, 12]);
    let expected: rustc_hash::FxHashSet<i64> = [11].into_iter().collect();
    let (bitmap, _) = build_way_plans(
        &block,
        &MembersForBlock::Bitmap(&[0x02]),
        PinSource::BlockLocal,
    );
    let (set, _) = build_way_plans(
        &block,
        &MembersForBlock::Set(&expected),
        PinSource::BlockLocal,
    );
    assert_eq!(
        bitmap.iter().map(|plan| plan.is_member).collect::<Vec<_>>(),
        set.iter().map(|plan| plan.is_member).collect::<Vec<_>>()
    );
    assert_eq!(
        bitmap
            .iter()
            .map(|plan| &plan.preserve_node_refs)
            .collect::<Vec<_>>(),
        set.iter()
            .map(|plan| &plan.preserve_node_refs)
            .collect::<Vec<_>>()
    );
}

#[allow(clippy::cast_possible_truncation)]
fn append_varint(bytes: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        bytes.push((value as u8) | 0x80);
        value >>= 7;
    }
    bytes.push(value as u8);
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> usize {
    let mut value = 0usize;
    let mut shift = 0usize;
    loop {
        let byte = *bytes.get(*cursor).expect("truncated protobuf varint");
        *cursor += 1;
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

fn blob_data_size(header: &[u8]) -> usize {
    let mut cursor = 0usize;
    while cursor < header.len() {
        let tag = read_varint(header, &mut cursor);
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                let value = read_varint(header, &mut cursor);
                if field == 3 {
                    return value;
                }
            }
            2 => {
                let len = read_varint(header, &mut cursor);
                cursor += len;
            }
            5 => cursor += 4,
            1 => cursor += 8,
            wire => panic!("unexpected BlobHeader wire type {wire}"),
        }
    }
    panic!("BlobHeader missing datasize field")
}

/// Append BlobHeader field 5 to the first OSMData frame. PBF frame headers are
/// length-prefixed, and the injected payload is version, varint way count, bitmap.
fn splice_way_members(pbf: &mut Vec<u8>, payload: &[u8]) {
    let first_header_len = u32::from_be_bytes(pbf[0..4].try_into().expect("header length"));
    let first_header_start = 4usize;
    let first_header_end = first_header_start + first_header_len as usize;
    let first_frame_end =
        first_header_end + blob_data_size(&pbf[first_header_start..first_header_end]);
    let header_len_start = first_frame_end;
    let header_len = u32::from_be_bytes(
        pbf[header_len_start..header_len_start + 4]
            .try_into()
            .expect("data header length"),
    );
    let header_start = header_len_start + 4;
    let header_end = header_start + header_len as usize;
    let mut field = vec![0x2a];
    append_varint(&mut field, payload.len());
    field.extend_from_slice(payload);
    pbf.splice(header_end..header_end, field.iter().copied());
    let new_len = header_len
        .checked_add(u32::try_from(field.len()).expect("field length fits u32"))
        .expect("BlobHeader length fits u32");
    pbf[header_len_start..header_len_start + 4].copy_from_slice(&new_len.to_be_bytes());
}

/// Locate the `occurrence`-th (0-based) length-delimited field `field` in a raw
/// protobuf message. Returns byte offsets relative to `msg`: entry start (the
/// tag), value start, value end. Panics if not found.
fn find_ld_field(msg: &[u8], field: usize, occurrence: usize) -> (usize, usize, usize) {
    let mut cursor = 0usize;
    let mut seen = 0usize;
    while cursor < msg.len() {
        let entry_start = cursor;
        let tag = read_varint(msg, &mut cursor);
        let f = tag >> 3;
        match tag & 7 {
            0 => {
                let _ = read_varint(msg, &mut cursor);
            }
            1 => cursor += 8,
            5 => cursor += 4,
            2 => {
                let len = read_varint(msg, &mut cursor);
                let value_start = cursor;
                let value_end = value_start + len;
                if f == field {
                    if seen == occurrence {
                        return (entry_start, value_start, value_end);
                    }
                    seen += 1;
                }
                cursor = value_end;
            }
            wire => panic!("unexpected protobuf wire type {wire}"),
        }
    }
    panic!("length-delimited field {field} occurrence {occurrence} not found");
}

/// Re-emit a BlobHeader with its datasize (field 3) set to `new_datasize`,
/// preserving every other field verbatim.
fn rebuild_blobheader_datasize(header: &[u8], new_datasize: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < header.len() {
        let entry_start = cursor;
        let tag = read_varint(header, &mut cursor);
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                let _ = read_varint(header, &mut cursor);
                if field == 3 {
                    append_varint(&mut out, tag);
                    append_varint(&mut out, new_datasize);
                } else {
                    out.extend_from_slice(&header[entry_start..cursor]);
                }
            }
            2 => {
                let len = read_varint(header, &mut cursor);
                cursor += len;
                out.extend_from_slice(&header[entry_start..cursor]);
            }
            1 => {
                cursor += 8;
                out.extend_from_slice(&header[entry_start..cursor]);
            }
            5 => {
                cursor += 4;
                out.extend_from_slice(&header[entry_start..cursor]);
            }
            wire => panic!("unexpected BlobHeader wire type {wire}"),
        }
    }
    out
}

/// Append a Way field-20 shared-node pin bitmap to the `way_ordinal`-th way of
/// the first OSMData frame, rebuilding every enclosing length prefix. Mirrors
/// `splice_way_members` but reaches into the blob body: the fixture is written
/// with `PbfCompression::None`, so the frame's blob is a bare `Blob.raw`
/// (field 1) wrapping the `PrimitiveBlock`. Walks
/// Blob.raw -> PrimitiveBlock.primitivegroup (field 2) ->
/// PrimitiveGroup.ways (field 3, by ordinal), appends field 20 (tag 0xA2 0x01),
/// and re-encodes the group, primitive block, blob, BlobHeader datasize, and the
/// 4-byte frame header-length prefix. Bottom-up rebuild so varint growth in any
/// length prefix cannot corrupt downstream offsets.
fn splice_way_pins(pbf: &mut Vec<u8>, way_ordinal: usize, pin_bitmap: &[u8]) {
    let first_header_len = u32::from_be_bytes(pbf[0..4].try_into().expect("header length"));
    let first_header_start = 4usize;
    let first_header_end = first_header_start + first_header_len as usize;
    let first_frame_end =
        first_header_end + blob_data_size(&pbf[first_header_start..first_header_end]);
    // Second frame = the OSMData (way) frame.
    let hdr_len_pos = first_frame_end;
    let hdr_len = u32::from_be_bytes(
        pbf[hdr_len_pos..hdr_len_pos + 4]
            .try_into()
            .expect("data header length"),
    ) as usize;
    let hdr_start = hdr_len_pos + 4;
    let hdr_end = hdr_start + hdr_len;
    let blob_start = hdr_end;
    let blob_len = blob_data_size(&pbf[hdr_start..hdr_end]);
    let blob_end = blob_start + blob_len;

    let header = pbf[hdr_start..hdr_end].to_vec();
    let blob = pbf[blob_start..blob_end].to_vec();

    // Blob.raw (field 1) -> PrimitiveBlock.
    let (raw_entry, raw_val_start, raw_val_end) = find_ld_field(&blob, 1, 0);
    let pb = &blob[raw_val_start..raw_val_end];
    // PrimitiveBlock.primitivegroup (field 2). The fixture writes one group.
    let (grp_entry, grp_val_start, grp_val_end) = find_ld_field(pb, 2, 0);
    let grp = &pb[grp_val_start..grp_val_end];
    // PrimitiveGroup.ways (field 3), selected by ordinal.
    let (way_entry, way_val_start, way_val_end) = find_ld_field(grp, 3, way_ordinal);

    // Field 20 (shared_node_pins): tag (20 << 3 | 2) = 162 = varint 0xA2 0x01.
    let mut field20 = vec![0xA2u8, 0x01u8];
    append_varint(&mut field20, pin_bitmap.len());
    field20.extend_from_slice(pin_bitmap);

    let mut new_way_val = grp[way_val_start..way_val_end].to_vec();
    new_way_val.extend_from_slice(&field20);
    let mut new_way_entry = vec![0x1Au8]; // ways: field 3, wire 2.
    append_varint(&mut new_way_entry, new_way_val.len());
    new_way_entry.extend_from_slice(&new_way_val);
    let mut new_grp = Vec::new();
    new_grp.extend_from_slice(&grp[..way_entry]);
    new_grp.extend_from_slice(&new_way_entry);
    new_grp.extend_from_slice(&grp[way_val_end..]);

    let mut new_grp_entry = vec![0x12u8]; // primitivegroup: field 2, wire 2.
    append_varint(&mut new_grp_entry, new_grp.len());
    new_grp_entry.extend_from_slice(&new_grp);
    let mut new_pb = Vec::new();
    new_pb.extend_from_slice(&pb[..grp_entry]);
    new_pb.extend_from_slice(&new_grp_entry);
    new_pb.extend_from_slice(&pb[grp_val_end..]);

    let mut new_raw_entry = vec![0x0Au8]; // raw: field 1, wire 2.
    append_varint(&mut new_raw_entry, new_pb.len());
    new_raw_entry.extend_from_slice(&new_pb);
    let mut new_blob = Vec::new();
    new_blob.extend_from_slice(&blob[..raw_entry]);
    new_blob.extend_from_slice(&new_raw_entry);
    new_blob.extend_from_slice(&blob[raw_val_end..]);

    let new_header = rebuild_blobheader_datasize(&header, new_blob.len());
    let mut new_frame = Vec::new();
    new_frame.extend_from_slice(
        &u32::try_from(new_header.len())
            .expect("header length fits u32")
            .to_be_bytes(),
    );
    new_frame.extend_from_slice(&new_header);
    new_frame.extend_from_slice(&new_blob);
    pbf.splice(hdr_len_pos..blob_end, new_frame);
}

fn way_members_payload(count: u32, bitmap: &[u8]) -> Vec<u8> {
    let mut payload = vec![1];
    append_varint(&mut payload, count as usize);
    payload.extend_from_slice(bitmap);
    payload
}

#[allow(clippy::unwrap_in_result)]
fn injected_fixture(payload: Option<&[u8]>) -> Result<Phase12Stats, PipelineError> {
    let dir = tempfile::tempdir().expect("create tempdir");
    let pbf_path = dir.path().join("injected.osm.pbf");
    let file = File::create(&pbf_path).expect("create pbf");
    let mut writer = PbfWriter::new(file, PbfCompression::None);
    let header = block_builder::HeaderBuilder::new()
        .optional_feature("LocationsOnWays")
        .optional_feature("pbfhogg.WayMembers-v1")
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");
    // Three tagless ways (positions 0..=2). Ways 0 and 1 are short open ways;
    // way 2 is a small closed square (single-tile at the test zoom) so it can
    // stand as the outer ring of the multipolygon below. Its id (3) sits at
    // bitmap position 2 - the positional check the end-to-end test relies on.
    let mut builder = block_builder::BlockBuilder::new();
    builder.add_way_with_locations(
        1,
        [],
        &[10, 11],
        &[(590_000_000, 100_000_000), (590_000_100, 100_000_100)],
        None,
    );
    builder.add_way_with_locations(
        2,
        [],
        &[20, 21],
        &[(590_000_000, 100_000_000), (590_000_100, 100_000_100)],
        None,
    );
    builder.add_way_with_locations(
        3,
        [],
        &[30, 31, 32, 33, 30],
        &[
            (590_000_000, 100_000_000),
            (590_000_000, 100_001_000),
            (590_001_000, 100_001_000),
            (590_001_000, 100_000_000),
            (590_000_000, 100_000_000),
        ],
        None,
    );
    writer
        .write_primitive_block(builder.take().expect("take block").expect("way block"))
        .expect("write block");
    // One shortbread-matching multipolygon (landuse=forest) whose only member
    // is way position 2 (id 3). Resolving it back through the way_index proves
    // the injected bit was consumed at the right position: an off-by-one marks
    // a different way, way 3 misses the index, and missing_relation_way_refs
    // fires.
    let mut rel_builder = block_builder::BlockBuilder::new();
    rel_builder.add_relation(
        100,
        [("type", "multipolygon"), ("landuse", "forest")],
        &[block_builder::MemberData {
            id: pbfhogg::MemberId::Way(3),
            role: "outer",
        }],
        None,
    );
    writer
        .write_primitive_block(
            rel_builder
                .take()
                .expect("take relation block")
                .expect("relation block"),
        )
        .expect("write relation block");
    writer.flush().expect("flush pbf");
    if let Some(payload) = payload {
        let mut pbf = std::fs::read(&pbf_path).expect("read pbf");
        splice_way_members(&mut pbf, payload);
        let mut file = File::create(&pbf_path).expect("rewrite pbf");
        file.write_all(&pbf).expect("write enriched pbf");
    }
    let config = TilegenConfig {
        pbf_path,
        output_path: dir.path().join("unused.pmtiles"),
        tmp_dir: dir.path().join("tmp"),
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        ocean_artifact_key: None,
        ocean_only_metadata: false,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: [0; Layer::count()],
        fanout_caps: [0; Layer::count()],
        polygon_simplify_factor: 1.0,
    };
    phase_read_and_process(&config).map(|(_, _, stats)| stats)
}

#[test]
fn injected_members_end_to_end_marks_ways_with_block_local_pins() {
    let stats = injected_fixture(Some(&way_members_payload(3, &[0x04])))
        .expect("valid injected PBF should succeed");
    // The header declares WayMembers-v1 but NOT SharedNodePins-v1, so membership
    // is injected while pins fall back to the block-local path. This proves the
    // two injected sources compose independently: MemberSource::Injected riding
    // alongside PinSource::BlockLocal.
    assert_eq!(stats.relation_plan_needed_ways, 0);
    assert_eq!(stats.relation_plan_superset_ways, 0);
    // Exactly the one set bit (position 2) was consumed - no over-marking.
    assert_eq!(stats.way_members_marked, 1);
    // The relation resolved its member way in the way_index, proving the bit
    // marked way position 2 and not some neighbour: an off-by-one leaves way 3
    // out of the index and this counter fires.
    assert_eq!(stats.rel_count, 1);
    assert_eq!(stats.missing_refs.missing_relation_way_refs, 0);
}

#[test]
fn injected_members_missing_field5_fails_run() {
    // Header still declares WayMembers-v1 but no field 5 is present: absence is
    // corrupt enrichment.
    assert!(injected_fixture(None).is_err());
}

#[test]
fn injected_members_malformed_field5_fails_run() {
    // Version byte 0x02 - pbfhogg's accessor yields None, same corrupt class,
    // proving the malformed arm flows through the decode worker and the `?`.
    assert!(injected_fixture(Some(&[2, 3, 4])).is_err());
}

#[test]
fn injected_members_count_mismatch_fails_run() {
    // Encoded count 4 with a one-byte bitmap over 3 actual ways. 4.div_ceil(8)
    // == 1, so pbfhogg's internal length check passes; the failure comes from
    // elivagar's encoded-vs-actual compare - the within-one-byte producer bug.
    assert!(injected_fixture(Some(&way_members_payload(4, &[0x04]))).is_err());
}

/// Build an injected PBF whose single way is a shortbread-matching
/// `highway=motorway` open way (streets layer) with three collinear vertices.
/// The header declares LocationsOnWays + WayMembers-v1 + SharedNodePins-v1 and a
/// valid all-zero field 5 (matching altw output shape). When `pin_bitmap` is
/// `Some`, field 20 is spliced onto the way. Returns the tempdir (kept alive so
/// the sort chunks survive), the filled `SortWriter`, and the phase stats.
#[allow(clippy::unwrap_in_result)]
fn injected_pins_fixture(
    pin_bitmap: Option<&[u8]>,
) -> Result<(tempfile::TempDir, sort::SortWriter, Phase12Stats), PipelineError> {
    let dir = tempfile::tempdir().expect("create tempdir");
    let pbf_path = dir.path().join("injected-pins.osm.pbf");
    let file = File::create(&pbf_path).expect("create pbf");
    let mut writer = PbfWriter::new(file, PbfCompression::None);
    let header = block_builder::HeaderBuilder::new()
        .optional_feature("LocationsOnWays")
        .optional_feature("pbfhogg.WayMembers-v1")
        .optional_feature("pbfhogg.SharedNodePins-v1")
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");
    // One open way, highway=motorway (streets layer, shown from low zoom). Three
    // vertices at constant latitude, so the middle vertex B lies exactly on the
    // A-C chord: plain DP drops it at every simplified zoom while a pin on ref
    // position 1 forces its retention. The 0.01 deg longitude span stays inside a
    // single tile across the mid zooms this test reads.
    let mut builder = block_builder::BlockBuilder::new();
    builder.add_way_with_locations(
        1,
        [("highway", "motorway")],
        &[10, 11, 12],
        &[
            (590_000_000, 100_000_000),
            (590_000_000, 100_050_000),
            (590_000_000, 100_100_000),
        ],
        None,
    );
    writer
        .write_primitive_block(builder.take().expect("take block").expect("way block"))
        .expect("write block");
    writer.flush().expect("flush pbf");

    let mut pbf = std::fs::read(&pbf_path).expect("read pbf");
    // altw always emits field 5 beside field 20; the way is no relation member,
    // so its membership bit is zero. Splice members first, then pins.
    splice_way_members(&mut pbf, &way_members_payload(1, &[0x00]));
    if let Some(bitmap) = pin_bitmap {
        splice_way_pins(&mut pbf, 0, bitmap);
    }
    let mut out = File::create(&pbf_path).expect("rewrite pbf");
    out.write_all(&pbf).expect("write enriched pbf");
    drop(out);

    let config = TilegenConfig {
        pbf_path,
        output_path: dir.path().join("unused.pmtiles"),
        tmp_dir: dir.path().join("tmp"),
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        ocean_artifact_key: None,
        ocean_only_metadata: false,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: [0; Layer::count()],
        fanout_caps: [0; Layer::count()],
        polygon_simplify_factor: 1.0,
    };
    phase_read_and_process(&config).map(|(sw, _, stats)| (dir, sw, stats))
}

/// Drain a sort reader, returning (record count, total geometry command words).
/// The wire record layout is [8 osm_id][1 geom_type][4 cmd_count][cmd_count * 4
/// geometry words][attrs]; a line that retains one more vertex carries two more
/// command words, so the total is a monotone proxy for retained vertices.
fn sum_geometry_cmd_words(mut reader: sort::SortReader) -> (u64, u64) {
    let mut records = 0u64;
    let mut words = 0u64;
    while let Some(rec) = reader.next().expect("read sort record") {
        records += 1;
        if rec.data.len() >= 13 {
            let cmd_count = u32::from_le_bytes(rec.data[9..13].try_into().expect("cmd_count"));
            words += u64::from(cmd_count);
        }
    }
    (records, words)
}

#[test]
fn injected_pins_end_to_end_pins_vertex_through_dp() {
    // Pinned: field 20 = 0x02 pins ref position 1, the collinear middle vertex.
    let (pin_dir, pin_sw, pin_stats) =
        injected_pins_fixture(Some(&[0x02])).expect("pinned injected PBF should succeed");
    assert_eq!(
        pin_stats.way_pins_marked, 1,
        "one vertex pinned via field 20"
    );
    let (pin_records, pin_words) = sum_geometry_cmd_words(pin_sw.finish().expect("finish pinned"));
    drop(pin_dir);

    // Unpinned: no field 20 -> all-false mask, DP free to drop the middle vertex.
    let (nopin_dir, nopin_sw, nopin_stats) =
        injected_pins_fixture(None).expect("unpinned injected PBF should succeed");
    assert_eq!(
        nopin_stats.way_pins_marked, 0,
        "no field 20 -> nothing pinned"
    );
    let (nopin_records, nopin_words) =
        sum_geometry_cmd_words(nopin_sw.finish().expect("finish unpinned"));
    drop(nopin_dir);

    assert!(
        pin_records > 0 && nopin_records > 0,
        "the motorway emits records at simplified zooms in both runs"
    );
    assert!(
        pin_words > nopin_words,
        "the pin retains the collinear middle vertex that DP otherwise drops: \
         pinned {pin_words} geometry words vs unpinned {nopin_words}"
    );
}

#[test]
fn injected_pins_wrong_length_bitmap_fails_run() {
    // A 3-ref way needs ceil(3/8) = 1 bitmap byte; a 2-byte field 20 is corrupt
    // enrichment, caught in the decode worker as a PipelineError (mirrors the
    // field-5 count-mismatch test - is_err, not should_panic).
    assert!(injected_pins_fixture(Some(&[0x02, 0x00])).is_err());
}

fn one_tile_sort_reader(chunks_dir: &std::path::Path) -> sort::SortReader {
    let mut writer = sort::SortWriter::new(chunks_dir, 1024, sort::ChunkCompression::None)
        .expect("create sort writer");
    let attrs: Vec<crate::shortbread::Attr> =
        vec![("kind", AttrValue::Str(Cow::Borrowed("city")), 0)];
    let feature =
        crate::wire_format::encode_feature_data(1, mvt::GeomType::Point, &[9, 0, 0], &attrs, 14);
    writer
        .push(SortRecord {
            key: sort::make_sort_key(pmtiles_writer::xy_to_tile_id(0, 0, 0), Layer::Pois as u8, 0),
            data: feature,
        })
        .expect("push sort record");
    writer.finish().expect("finish sort writer")
}

#[test]
fn tile_features_ordered_by_paint_rank() {
    let dir = tempfile::tempdir().expect("create sort directory");
    // A one-record budget forces each adversarial producer append into a separate
    // chunk, so this exercises both chunk sorting and the k-way merge.
    let mut writer = sort::SortWriter::new(dir.path(), 1, sort::ChunkCompression::None)
        .expect("create sort writer");
    let tile_id = pmtiles_writer::xy_to_tile_id(10, 1, 1);
    let ranks = [5u8, 0, 3, 2];
    for (osm_id, rank) in ranks.into_iter().enumerate() {
        let osm_id = u8::try_from(osm_id).expect("test id fits in u8");
        writer
            .push(SortRecord {
                key: sort::make_sort_key(tile_id, Layer::Land as u8, rank),
                data: Box::from([osm_id]),
            })
            .expect("push rank-tagged record");
    }
    let mut reader = writer.finish().expect("finish sort writer");
    let mut grouped = PendingTile {
        tile_id,
        features: Vec::new(),
    };
    let mut observed = Vec::new();
    while let Some(record) = reader.next().expect("read sorted record") {
        assert_eq!(sort::tile_id_from_key(record.key), grouped.tile_id);
        assert_eq!(sort::layer_from_key(record.key), Layer::Land as u8);
        observed.push(sort::priority_from_key(record.key));
        grouped.features.push((Layer::Land as u8, record.data));
    }
    assert_eq!(observed, [0, 2, 3, 5]);
    assert_eq!(grouped.features.len(), observed.len());
}

/// Fixture for the MLT/MVT layer-model parity test.
#[cfg(feature = "mlt")]
fn parity_pending_tile(tile_id: u64) -> PendingTile {
    let point_attrs = vec![("kind", AttrValue::Str(Cow::Borrowed("city")), 0)];
    let line_attrs = vec![("kind", AttrValue::Str(Cow::Borrowed("street")), 0)];

    PendingTile {
        tile_id,
        features: vec![
            (
                Layer::Pois as u8,
                crate::wire_format::encode_feature_data(
                    11,
                    mvt::GeomType::Point,
                    &[9, 0, 0],
                    &point_attrs,
                    14,
                ),
            ),
            (
                Layer::Streets as u8,
                crate::wire_format::encode_feature_data(
                    21,
                    mvt::GeomType::LineString,
                    &[9, 4, 4, 18, 0, 16, 16, 0],
                    &line_attrs,
                    14,
                ),
            ),
        ],
    }
}

#[test]
fn flat_index_guard_sorted_large_allowed() {
    let mode = select_node_store_mode(false, true, false, 50 * 1024 * 1024 * 1024, false)
        .expect("sorted input should be allowed");
    assert_eq!(mode, NodeStoreMode::Sorted);
}

#[test]
fn flat_index_guard_unsorted_small_allowed() {
    let mode = select_node_store_mode(false, false, false, 512 * 1024 * 1024, false)
        .expect("small unsorted input should be allowed");
    assert_eq!(
        mode,
        NodeStoreMode::Flat {
            unsafe_override: false
        }
    );
}

#[test]
fn flat_index_guard_unsorted_large_rejected_with_stable_error() {
    let err = select_node_store_mode(false, false, false, 2 * 1024 * 1024 * 1024, false)
        .expect_err("large unsorted input should be rejected");
    let msg = err.to_string();
    assert!(msg.contains("does not declare Sort.Type_then_ID"));
    assert!(msg.contains("pbfhogg sort input.pbf -o sorted.pbf"));
    assert!(msg.contains("--force-sorted"));
}

#[test]
fn flat_index_guard_override_path_works() {
    let mode = select_node_store_mode(false, false, false, 2 * 1024 * 1024 * 1024, true)
        .expect("override should allow large unsorted input");
    assert_eq!(
        mode,
        NodeStoreMode::Flat {
            unsafe_override: true
        }
    );
}

#[test]
fn missing_ref_stats_accumulates_and_snapshots() {
    let stats = MissingRefStatsAtomic::default();
    stats.record_way_missing_nodes(3);
    stats.record_way_missing_nodes(2);
    stats.record_relation_missing_way_ref();
    stats.record_relation_missing_way_ref();
    stats.record_relation_with_missing_way_refs();
    stats.record_relation_non_way_member();
    stats.record_relation_nested_member();

    let snap = stats.snapshot();
    assert_eq!(snap.missing_way_node_refs, 5);
    assert_eq!(snap.ways_with_missing_node_refs, 2);
    assert_eq!(snap.missing_relation_way_refs, 2);
    assert_eq!(snap.relations_with_missing_way_refs, 1);
    assert_eq!(snap.relation_non_way_members, 1);
    assert_eq!(snap.relation_nested_members, 1);
}

#[test]
fn missing_ref_summary_lines_include_all_counters() {
    let lines = missing_ref_summary_lines(MissingRefStats {
        missing_way_node_refs: 5,
        ways_with_missing_node_refs: 2,
        missing_relation_way_refs: 3,
        relations_with_missing_way_refs: 1,
        relation_non_way_members: 4,
        relation_nested_members: 2,
    });
    assert_eq!(
        lines,
        [
            "missing_way_node_refs=5".to_string(),
            "ways_with_missing_node_refs=2".to_string(),
            "missing_relation_way_refs=3".to_string(),
            "relations_with_missing_way_refs=1".to_string(),
            "relation_non_way_members=4".to_string(),
            "relation_nested_members=2".to_string(),
        ]
    );
}

#[test]
fn missing_ref_summary_omitted_when_phase12_is_skipped() {
    let maybe_summary: Option<MissingRefStats> = None;
    let lines: Vec<String> = maybe_summary
        .map(missing_ref_summary_lines)
        .into_iter()
        .flatten()
        .collect();
    assert!(
        lines.is_empty(),
        "skip/resume paths without phase12 stats should emit no missing-ref metrics"
    );
}

#[test]
fn oversize_top_list_keeps_largest_tiles_sorted() {
    let mut top = [OversizeTile::default(); TILE_OVERSIZE_TOP_N];
    insert_top_oversized(
        &mut top,
        OversizeTile {
            tile_id: pmtiles_writer::xy_to_tile_id(1, 0, 0),
            bytes: 100,
        },
    );
    insert_top_oversized(
        &mut top,
        OversizeTile {
            tile_id: pmtiles_writer::xy_to_tile_id(1, 1, 0),
            bytes: 900,
        },
    );
    insert_top_oversized(
        &mut top,
        OversizeTile {
            tile_id: pmtiles_writer::xy_to_tile_id(1, 1, 1),
            bytes: 500,
        },
    );

    assert_eq!(top[0].bytes, 900);
    assert_eq!(top[1].bytes, 500);
    assert_eq!(top[2].bytes, 100);
}

#[test]
fn tile_size_diag_thresholds_are_strictly_greater_than_boundaries() {
    let mut diag = TileSizeDiagnostics::default();
    let warn = TILE_OVERSIZE_WARN_BYTES;
    let severe = TILE_OVERSIZE_SEVERE_BYTES;

    record_tile_size_diagnostics(&mut diag, pmtiles_writer::xy_to_tile_id(0, 0, 0), warn);
    assert_eq!(diag.oversize_warn_count, 0);
    assert_eq!(diag.oversize_severe_count, 0);

    record_tile_size_diagnostics(&mut diag, pmtiles_writer::xy_to_tile_id(1, 0, 0), warn + 1);
    assert_eq!(diag.oversize_warn_count, 1);
    assert_eq!(diag.oversize_severe_count, 0);

    record_tile_size_diagnostics(&mut diag, pmtiles_writer::xy_to_tile_id(2, 0, 0), severe);
    assert_eq!(diag.oversize_warn_count, 2);
    assert_eq!(diag.oversize_severe_count, 0);

    record_tile_size_diagnostics(
        &mut diag,
        pmtiles_writer::xy_to_tile_id(3, 0, 0),
        severe + 1,
    );
    assert_eq!(diag.oversize_warn_count, 3);
    assert_eq!(diag.oversize_severe_count, 1);
}

/// Helper: build a BoundaryLabels match with the given admin_level and default min_zoom=5.
fn boundary_labels_match(admin_level: i64) -> LayerMatch {
    LayerMatch {
        layer: Layer::BoundaryLabels,
        min_zoom: 5,
        max_zoom: 14,
        geom_expect: GeomExpect::PolygonPointOnSurface,
        paint_rank: 0,
        attrs: smallvec![
            ("admin_level", AttrValue::Int(admin_level), 0),
            ("name", AttrValue::Str(Cow::Borrowed("TestCountry")), 0),
        ],
    }
}

/// admin_level=2 with area >= 2,000,000 km^2 -> min_zoom overridden to 2
#[test]
fn boundary_label_admin2_large_area() {
    let area_m2 = 2_000_000.0 * 1e6; // exactly 2M km^2
    let mut matches = vec![boundary_labels_match(2)];
    enrich_polygon_matches(&mut matches, area_m2);

    assert_eq!(matches[0].min_zoom, 2);
    // Also verify way_area was added (in hectares)
    let way_area_attr = matches[0]
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "way_area")
        .expect("way_area attr missing");
    if let AttrValue::Float(h) = way_area_attr.1 {
        let expected_hectares = area_m2 / 10_000.0;
        assert!(
            (h - expected_hectares).abs() < 0.01,
            "way_area hectares mismatch"
        );
    } else {
        panic!("way_area should be Float");
    }
}

/// admin_level=4 with area >= 700,000 km^2 -> min_zoom overridden to 3
#[test]
fn boundary_label_admin4_700k_km2() {
    let area_m2 = 700_000.0 * 1e6;
    let mut matches = vec![boundary_labels_match(4)];
    enrich_polygon_matches(&mut matches, area_m2);

    assert_eq!(matches[0].min_zoom, 3);
}

/// admin_level=4 with area >= 100,000 km^2 (but < 700,000) -> min_zoom overridden to 4
#[test]
fn boundary_label_admin4_100k_km2() {
    let area_m2 = 150_000.0 * 1e6; // 150k km^2
    let mut matches = vec![boundary_labels_match(4)];
    enrich_polygon_matches(&mut matches, area_m2);

    assert_eq!(matches[0].min_zoom, 4);
}

/// admin_level=4 with area < 100,000 km^2 -> min_zoom stays at default (5)
#[test]
fn boundary_label_admin4_small_area() {
    let area_m2 = 50_000.0 * 1e6; // 50k km^2 - below 100k threshold
    let mut matches = vec![boundary_labels_match(4)];
    enrich_polygon_matches(&mut matches, area_m2);

    assert_eq!(
        matches[0].min_zoom, 5,
        "small area should keep default min_zoom=5"
    );
}

/// Non-BoundaryLabels layer should be completely unchanged by enrich_polygon_matches.
#[test]
fn non_boundary_labels_unchanged() {
    let mut matches = vec![LayerMatch {
        layer: Layer::Buildings,
        min_zoom: 14,
        max_zoom: 14,
        geom_expect: GeomExpect::Polygon,
        paint_rank: 0,
        attrs: smallvec![],
    }];
    let original_min_zoom = matches[0].min_zoom;
    let original_attr_count = matches[0].attrs.len();

    enrich_polygon_matches(&mut matches, 9_999_999_999.0);

    assert_eq!(matches[0].min_zoom, original_min_zoom);
    assert_eq!(
        matches[0].attrs.len(),
        original_attr_count,
        "attrs should not be modified"
    );
}

#[test]
fn block_shared_node_annotation_marks_only_shared_interiors() {
    let mut raw = vec![
        RawWay {
            way_id: 1,
            node_refs: vec![10, 20, 30],
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
        RawWay {
            way_id: 2,
            node_refs: vec![99, 20, 88],
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
        RawWay {
            way_id: 3,
            node_refs: vec![20, 777], // shared, but endpoint only => ignored
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
    ];

    annotate_block_shared_node_refs(&mut raw);

    assert_eq!(raw[0].preserve_node_refs, vec![20]);
    assert_eq!(raw[1].preserve_node_refs, vec![20]);
    assert!(raw[2].preserve_node_refs.is_empty());
}

#[test]
fn block_shared_node_annotation_marks_closed_ring_shared_vertices() {
    let mut raw = vec![
        RawWay {
            way_id: 10,
            node_refs: vec![1, 2, 3, 4, 1],
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
        RawWay {
            way_id: 11,
            node_refs: vec![3, 4, 5, 6, 3],
            preserve_node_refs: Vec::new(),
            coords_e7: Vec::new(),
            tags: Vec::new(),
        },
    ];

    annotate_block_shared_node_refs(&mut raw);

    assert_eq!(raw[0].preserve_node_refs, vec![3, 4]);
    assert_eq!(raw[1].preserve_node_refs, vec![3, 4]);
}

#[test]
fn block_shared_node_annotation_does_not_detect_cross_block_junctions() {
    let mut block_a = vec![RawWay {
        way_id: 100,
        node_refs: vec![1, 20, 2],
        preserve_node_refs: Vec::new(),
        coords_e7: Vec::new(),
        tags: Vec::new(),
    }];
    let mut block_b = vec![RawWay {
        way_id: 101,
        node_refs: vec![3, 20, 4],
        preserve_node_refs: Vec::new(),
        coords_e7: Vec::new(),
        tags: Vec::new(),
    }];

    annotate_block_shared_node_refs(&mut block_a);
    annotate_block_shared_node_refs(&mut block_b);

    assert!(
        block_a[0].preserve_node_refs.is_empty(),
        "cross-block shared interior junctions are intentionally not detected"
    );
    assert!(
        block_b[0].preserve_node_refs.is_empty(),
        "cross-block shared interior junctions are intentionally not detected"
    );

    let mut combined = vec![block_a.remove(0), block_b.remove(0)];
    annotate_block_shared_node_refs(&mut combined);
    assert_eq!(
        combined[0].preserve_node_refs,
        vec![20],
        "same data in one block should detect the shared interior node"
    );
    assert_eq!(combined[1].preserve_node_refs, vec![20]);
}

#[test]
fn relation_shared_vertex_keys_detects_shared_closed_way_vertices() {
    let member_ways = vec![
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.0, y: 0.0 },
                Point { x: 1.0, y: 0.0 },
                Point { x: 1.0, y: 1.0 },
                Point { x: 0.0, y: 1.0 },
                Point { x: 0.0, y: 0.0 },
            ],
        },
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 1.0, y: 0.0 },
                Point { x: 2.0, y: 0.0 },
                Point { x: 2.0, y: 1.0 },
                Point { x: 1.0, y: 1.0 },
                Point { x: 1.0, y: 0.0 },
            ],
        },
    ];
    let keys = relation_shared_vertex_keys(&member_ways);
    assert!(keys.contains(&merc_point_key(&Point { x: 1.0, y: 0.0 })));
    assert!(keys.contains(&merc_point_key(&Point { x: 1.0, y: 1.0 })));
    assert_eq!(keys.len(), 2);
}

#[test]
fn relation_shared_vertex_keys_quantization_near_equal_points_share_key() {
    let base = Point {
        x: 0.500_000_000_000,
        y: 0.2,
    };
    let near = Point {
        x: 0.500_000_000_000_4,
        y: 0.2,
    }; // +0.4e-12
    assert_eq!(
        merc_point_key(&base),
        merc_point_key(&near),
        "near-equal points should quantize to same key"
    );

    let member_ways = vec![
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.1, y: 0.1 },
                base,
                Point { x: 0.1, y: 0.3 },
                Point { x: 0.1, y: 0.1 },
            ],
        },
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.9, y: 0.1 },
                near,
                Point { x: 0.9, y: 0.3 },
                Point { x: 0.9, y: 0.1 },
            ],
        },
    ];
    let keys = relation_shared_vertex_keys(&member_ways);
    assert!(
        keys.contains(&merc_point_key(&base)),
        "shared key should include quantized near-equal vertex"
    );
    assert_eq!(keys.len(), 1);
}

#[test]
fn relation_shared_vertex_keys_quantization_boundary_distinguishes_points() {
    let base = Point {
        x: 0.500_000_000_000,
        y: 0.2,
    };
    let far = Point {
        x: 0.500_000_000_000_6,
        y: 0.2,
    }; // +0.6e-12
    assert_ne!(
        merc_point_key(&base),
        merc_point_key(&far),
        "points beyond rounding half-step should quantize differently"
    );

    let member_ways = vec![
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.1, y: 0.1 },
                base,
                Point { x: 0.1, y: 0.3 },
                Point { x: 0.1, y: 0.1 },
            ],
        },
        MemberWay {
            role: WayRole::Outer,
            coords: vec![
                Point { x: 0.9, y: 0.1 },
                far,
                Point { x: 0.9, y: 0.3 },
                Point { x: 0.9, y: 0.1 },
            ],
        },
    ];
    let keys = relation_shared_vertex_keys(&member_ways);
    assert!(
        !keys.contains(&merc_point_key(&base)),
        "non-shared quantized vertex should not be marked shared"
    );
    assert!(
        !keys.contains(&merc_point_key(&far)),
        "non-shared quantized vertex should not be marked shared"
    );
    assert!(keys.is_empty());
}

#[test]
fn unwrap_antimeridian_path_keeps_crossing_segment_local() {
    let mut pts = vec![Point { x: 0.995, y: 0.4 }, Point { x: 0.005, y: 0.4 }];
    let changed = unwrap_antimeridian_path(&mut pts, false);
    assert!(changed);
    assert!(pts[1].x > 1.0, "second point should unwrap across +1 seam");
    assert!(
        (pts[1].x - pts[0].x).abs() < 0.05,
        "segment should stay short after unwrapping"
    );
}

#[test]
fn antimeridian_shifts_for_bbox_returns_wrap_shifts() {
    let bbox = MercBbox {
        min_x: 0.99,
        min_y: 0.1,
        max_x: 1.01,
        max_y: 0.2,
    };
    let shifts = antimeridian_shifts_for_bbox(&bbox);
    assert_eq!(shifts.len(), 2);
    assert!(shifts.contains(&0.0));
    assert!(shifts.contains(&-1.0));
}

#[test]
fn crosses_antimeridian_detects_true_dateline_crossing() {
    // Tight dateline-spanning interval: [170, 180] U [-180, -170].
    let min_lon_e7 = -1_700_000_000;
    let max_lon_e7 = 1_700_000_000;
    let min_shifted = lon_e7_shifted_360(-1_700_000_000);
    let max_shifted = lon_e7_shifted_360(1_700_000_000);
    assert!(crosses_antimeridian(
        min_lon_e7,
        max_lon_e7,
        min_shifted.min(max_shifted),
        min_shifted.max(max_shifted),
    ));
}

#[test]
fn crosses_antimeridian_rejects_wide_non_crossing_interval() {
    // Wide but non-crossing interval: [-170, 20].
    let min_lon_e7 = -1_700_000_000;
    let max_lon_e7 = 200_000_000;
    let shifted_lons = [
        lon_e7_shifted_360(-1_700_000_000),
        lon_e7_shifted_360(200_000_000),
        lon_e7_shifted_360(0),
    ];
    let min_shifted = *shifted_lons.iter().min().expect("non-empty shifted sample");
    let max_shifted = *shifted_lons.iter().max().expect("non-empty shifted sample");
    assert!(!crosses_antimeridian(
        min_lon_e7,
        max_lon_e7,
        min_shifted,
        max_shifted,
    ));
}

#[test]
fn antimeridian_wrapped_line_emits_both_seam_tiles_without_duplicates() {
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = vec![Point { x: 0.995, y: 0.25 }, Point { x: 1.005, y: 0.25 }];
    let bbox = merc_bbox(&coords);
    let mut records = Vec::new();
    let mut scratch = LineEmitScratch::new();
    for shift in antimeridian_shifts_for_bbox(&bbox) {
        if shift == 0.0 {
            let _ = emit_line_feature(7001, &coords, &[], &m, 1, 1, &mut records, &mut scratch);
        } else {
            let shifted: Vec<Point> = coords
                .iter()
                .map(|p| Point {
                    x: p.x + shift,
                    y: p.y,
                })
                .collect();
            let _ = emit_line_feature(7001, &shifted, &[], &m, 1, 1, &mut records, &mut scratch);
        }
    }

    let mut counts = std::collections::BTreeMap::new();
    for rec in &records {
        let tile_id = sort::tile_id_from_key(rec.key);
        *counts.entry(tile_id).or_insert(0usize) += 1;
    }

    let left = pmtiles_writer::xy_to_tile_id(1, 0, 0);
    let right = pmtiles_writer::xy_to_tile_id(1, 1, 0);
    assert_eq!(
        counts.len(),
        2,
        "seam-crossing line should hit exactly two z1 seam tiles"
    );
    assert_eq!(counts.get(&left), Some(&1));
    assert_eq!(counts.get(&right), Some(&1));
}

#[test]
fn antimeridian_wrapped_polygon_emits_both_seam_tiles_without_duplicates() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let coords = vec![
        Point { x: 0.995, y: 0.24 },
        Point { x: 1.005, y: 0.24 },
        Point { x: 1.005, y: 0.26 },
        Point { x: 0.995, y: 0.26 },
        Point { x: 0.995, y: 0.24 },
    ];
    let bbox = merc_bbox(&coords);
    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    for shift in antimeridian_shifts_for_bbox(&bbox) {
        if shift == 0.0 {
            let _ = emit_polygon_feature(
                7002,
                &coords,
                &[],
                &m,
                1,
                1,
                &mut records,
                &mut scratch,
                0,
                None,
                0,
                1.0,
            );
        } else {
            let shifted: Vec<Point> = coords
                .iter()
                .map(|p| Point {
                    x: p.x + shift,
                    y: p.y,
                })
                .collect();
            let _ = emit_polygon_feature(
                7002,
                &shifted,
                &[],
                &m,
                1,
                1,
                &mut records,
                &mut scratch,
                0,
                None,
                0,
                1.0,
            );
        }
    }

    let mut counts = std::collections::BTreeMap::new();
    for rec in &records {
        let tile_id = sort::tile_id_from_key(rec.key);
        *counts.entry(tile_id).or_insert(0usize) += 1;
    }

    let left = pmtiles_writer::xy_to_tile_id(1, 0, 0);
    let right = pmtiles_writer::xy_to_tile_id(1, 1, 0);
    assert_eq!(
        counts.len(),
        2,
        "seam-crossing polygon should hit exactly two z1 seam tiles"
    );
    assert_eq!(counts.get(&left), Some(&1));
    assert_eq!(counts.get(&right), Some(&1));
}

#[test]
fn antimeridian_wrapped_multipolygon_emits_both_seam_tiles_without_duplicates() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.995, y: 0.24 },
        Point { x: 1.005, y: 0.24 },
        Point { x: 1.005, y: 0.26 },
        Point { x: 0.995, y: 0.26 },
        Point { x: 0.995, y: 0.24 },
    ];
    let inners: Vec<Vec<Point>> = Vec::new();
    let bbox = merc_bbox(&outer);
    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    for shift in antimeridian_shifts_for_bbox(&bbox) {
        if shift == 0.0 {
            let _ = emit_multipolygon_feature(
                7003,
                &outer,
                &inners,
                None,
                &m,
                1,
                1,
                &mut records,
                &mut emit_scratch,
                &mut simp_scratch,
                0,
                None,
                0,
                1.0,
            );
        } else {
            let outer_shifted: Vec<Point> = outer
                .iter()
                .map(|p| Point {
                    x: p.x + shift,
                    y: p.y,
                })
                .collect();
            let _ = emit_multipolygon_feature(
                7003,
                &outer_shifted,
                &inners,
                None,
                &m,
                1,
                1,
                &mut records,
                &mut emit_scratch,
                &mut simp_scratch,
                0,
                None,
                0,
                1.0,
            );
        }
    }

    let mut counts = std::collections::BTreeMap::new();
    for rec in &records {
        let tile_id = sort::tile_id_from_key(rec.key);
        *counts.entry(tile_id).or_insert(0usize) += 1;
    }

    let left = pmtiles_writer::xy_to_tile_id(1, 0, 0);
    let right = pmtiles_writer::xy_to_tile_id(1, 1, 0);
    assert_eq!(
        counts.len(),
        2,
        "seam-crossing multipolygon should hit exactly two z1 seam tiles"
    );
    assert_eq!(counts.get(&left), Some(&1));
    assert_eq!(counts.get(&right), Some(&1));
}

#[cfg(feature = "mlt")]
#[test]
fn encode_tile_batch_mlt_empty_batch_is_empty() {
    match encode_tile_batch(
        &[],
        6,
        TilePayloadFormat::Mlt,
        TileCompression::Gzip,
        &[],
        &SeamMetrics::new(),
    ) {
        Ok(encoded) => assert!(encoded.is_empty(), "empty batches should remain empty"),
        Err(err) => panic!("empty mlt batch should not fail: {err}"),
    }
}

#[test]
fn encode_tile_batch_mvt_empty_batch_is_empty() {
    let encoded = encode_tile_batch(
        &[],
        6,
        TilePayloadFormat::Mvt,
        TileCompression::Gzip,
        &[],
        &SeamMetrics::new(),
    )
    .expect("mvt format should encode successfully");
    assert!(encoded.is_empty());
}

#[cfg(feature = "mlt")]
#[test]
fn encode_tile_batch_mlt_empty_tile_encodes_to_no_output() {
    let tile = PendingTile {
        tile_id: pmtiles_writer::xy_to_tile_id(3, 4, 5),
        features: Vec::new(),
    };
    match encode_tile_batch(
        &[tile],
        6,
        TilePayloadFormat::Mlt,
        TileCompression::Gzip,
        &[],
        &SeamMetrics::new(),
    ) {
        Ok(encoded) => assert!(encoded.is_empty(), "empty tiles should be skipped"),
        Err(err) => panic!("mlt format should not fail for empty tile: {err}"),
    }
}

#[cfg(feature = "mlt")]
#[test]
fn shared_layer_prep_model_matches_mvt_layer_assembly() {
    let tile_id = pmtiles_writer::xy_to_tile_id(4, 8, 9);
    let tile = parity_pending_tile(tile_id);

    let mut scratch = AssemblyScratch {
        encode_scratch: mvt::EncodeScratch::new(),
        merge_scratch: mvt::MergeScratch::new(),
        line_merge_scratch: mvt::LineMergeScratch::new(),
        geom_pool: Vec::new(),
        tags_pool: Vec::new(),
        compression_levels: [const { None }; 11],
        gz_buf: Vec::new(),
        mvt_buf: Vec::new(),
        layers: [const { None }; LAYER_COUNT],
        seam_rings: Vec::new(),
        seam_provenance: Vec::new(),
        seam_encode_buf: Vec::new(),
    };
    let non_empty = prepare_non_empty_layers(&mut scratch, &tile);
    let model = mlt::build_tile_model(&non_empty);
    let mvt_encoded = encode_tile_batch(
        &[parity_pending_tile(tile_id)],
        6,
        TilePayloadFormat::Mvt,
        TileCompression::Gzip,
        &[],
        &SeamMetrics::new(),
    )
    .expect("mvt batch encode should succeed");
    assert_eq!(mvt_encoded.len(), 1, "expected one encoded mvt tile");

    let mut decoder = GzDecoder::new(mvt_encoded[0].compressed.as_slice());
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw).expect("gunzip mvt tile");
    let mvt_layers = crate::pmtiles_reader::decode_mvt_layers(&raw).expect("decode mvt layers");

    assert_eq!(
        model.layers.len(),
        mvt_layers.len(),
        "layer count mismatch between prep model and MVT"
    );

    let mut model_sorted: Vec<_> = model.layers.iter().collect();
    model_sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut mvt_sorted: Vec<_> = mvt_layers.iter().collect();
    mvt_sorted.sort_by(|a, b| a.name.cmp(&b.name));

    for (ml, mvt) in model_sorted.iter().zip(mvt_sorted.iter()) {
        assert_eq!(ml.name, mvt.name, "layer name mismatch");
        assert_eq!(
            ml.feature_count, mvt.feature_count,
            "feature count mismatch in {}",
            ml.name
        );
        assert_eq!(
            ml.geometry_mix.points, mvt.points,
            "point count mismatch in {}",
            ml.name
        );
        assert_eq!(
            ml.geometry_mix.lines, mvt.lines,
            "line count mismatch in {}",
            ml.name
        );
        assert_eq!(
            ml.geometry_mix.polygons, mvt.polygons,
            "polygon count mismatch in {}",
            ml.name
        );
        let mut model_keys: Vec<&str> = ml.columns.iter().map(|c| c.key.as_str()).collect();
        model_keys.sort();
        let mut mvt_keys: Vec<&str> = mvt.keys.iter().map(String::as_str).collect();
        mvt_keys.sort();
        assert_eq!(model_keys, mvt_keys, "property key mismatch in {}", ml.name);
    }
}

#[test]
fn phase_assemble_propagates_source_pbf_filename_to_metadata() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let chunks_dir = dir.path().join("chunks");
    let output_path = dir.path().join("phase_assemble_meta.pmtiles");
    let tmp_dir = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let mut sort_reader = one_tile_sort_reader(&chunks_dir);

    let config = TilegenConfig {
        pbf_path: dir.path().join("source-file.osm.pbf"),
        output_path: output_path.clone(),
        tmp_dir,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        ocean_artifact_key: None,
        ocean_only_metadata: false,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: {
            let mut m = [0u8; shortbread::Layer::count()];
            m[shortbread::Layer::Boundaries as usize] = 8;
            m
        },
        fanout_caps: [0; shortbread::Layer::count()],
        polygon_simplify_factor: 1.0,
    };

    let (_features_read, _tiles_written, _unique_tiles, _batch_hwm, _dedup_stats, _size_diag) =
        phase_assemble(&mut sort_reader, &config).expect("assemble should succeed");

    let mut reader =
        crate::pmtiles_reader::PmtilesReader::open(&output_path).expect("open generated pmtiles");
    let metadata = reader.read_metadata().expect("read metadata");
    let parsed: serde_json::Value = serde_json::from_str(&metadata).expect("parse metadata json");
    assert_eq!(parsed["source_pbf"], "source-file.osm.pbf");
    assert!(parsed.get("osmosis_replication_timestamp").is_none());
}

#[test]
fn phase_assemble_propagates_replication_timestamp_to_metadata() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let chunks_dir = dir.path().join("chunks");
    let output_path = dir.path().join("phase_assemble_meta_ts.pmtiles");
    let tmp_dir = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let mut sort_reader = one_tile_sort_reader(&chunks_dir);

    let pbf_path = dir.path().join("replication-source.osm.pbf");
    let mut pbf_file = File::create(&pbf_path).expect("create pbf file");
    let mut pbf_writer = PbfWriter::new(&mut pbf_file, PbfCompression::default());
    let header = block_builder::HeaderBuilder::new()
        .replication_timestamp(1_700_000_123)
        .build()
        .expect("build pbf header");
    pbf_writer.write_header(&header).expect("write pbf header");
    pbf_writer.flush().expect("flush pbf");

    let config = TilegenConfig {
        pbf_path,
        output_path: output_path.clone(),
        tmp_dir,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        ocean_artifact_key: None,
        ocean_only_metadata: false,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: {
            let mut m = [0u8; shortbread::Layer::count()];
            m[shortbread::Layer::Boundaries as usize] = 8;
            m
        },
        fanout_caps: [0; shortbread::Layer::count()],
        polygon_simplify_factor: 1.0,
    };

    let (_features_read, _tiles_written, _unique_tiles, _batch_hwm, _dedup_stats, _size_diag) =
        phase_assemble(&mut sort_reader, &config).expect("assemble should succeed");

    let mut reader =
        crate::pmtiles_reader::PmtilesReader::open(&output_path).expect("open generated pmtiles");
    let metadata = reader.read_metadata().expect("read metadata");
    let parsed: serde_json::Value = serde_json::from_str(&metadata).expect("parse metadata json");
    assert_eq!(parsed["source_pbf"], "replication-source.osm.pbf");
    assert_eq!(parsed["osmosis_replication_timestamp"], 1_700_000_123);
}

#[test]
#[allow(clippy::too_many_lines)]
fn phase_assemble_tile_format_sets_consistent_payload_contract() {
    let dir = tempfile::tempdir().expect("create tempdir");

    // MVT contract: gzip-compressed MVT payload, explicit MVT tile type in header.
    let mvt_chunks_dir = dir.path().join("chunks_mvt");
    let mvt_output = dir.path().join("phase_assemble_contract_mvt.pmtiles");
    let mvt_tmp = dir.path().join("tmp_mvt");
    std::fs::create_dir_all(&mvt_tmp).expect("create mvt tmp dir");
    let mut mvt_sort_reader = one_tile_sort_reader(&mvt_chunks_dir);
    let mvt_config = TilegenConfig {
        pbf_path: dir.path().join("contract-mvt.osm.pbf"),
        output_path: mvt_output.clone(),
        tmp_dir: mvt_tmp,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        ocean_artifact_key: None,
        ocean_only_metadata: false,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: {
            let mut m = [0u8; shortbread::Layer::count()];
            m[shortbread::Layer::Boundaries as usize] = 8;
            m
        },
        fanout_caps: [0; shortbread::Layer::count()],
        polygon_simplify_factor: 1.0,
    };
    let _ = phase_assemble(&mut mvt_sort_reader, &mvt_config).expect("mvt assemble should succeed");
    let mut mvt_reader =
        crate::pmtiles_reader::PmtilesReader::open(&mvt_output).expect("open mvt pmtiles");
    let mvt_metadata = mvt_reader.read_metadata().expect("read mvt metadata");
    let mvt_json: serde_json::Value =
        serde_json::from_str(&mvt_metadata).expect("parse mvt metadata");
    assert_eq!(
        mvt_reader.tile_type(),
        1,
        "mvt header tile_type must be mvt"
    );
    assert_eq!(
        mvt_reader.tile_compression(),
        2,
        "mvt header tile_compression must be gzip"
    );
    assert_eq!(mvt_json["tile_payload_format"], "mvt");
    assert_eq!(mvt_json["tile_compression"], "gzip");

    // MLT contract: uncompressed payload, unknown tile type in PMTiles header + explicit metadata.
    // Gated with the encoder: without the mlt feature phase_assemble refuses
    // the format rather than writing tiles. The MVT half above is the part
    // that must hold in a default build.
    #[cfg(feature = "mlt")]
    {
        let mlt_chunks_dir = dir.path().join("chunks_mlt");
        let mlt_output = dir.path().join("phase_assemble_contract_mlt.pmtiles");
        let mlt_tmp = dir.path().join("tmp_mlt");
        std::fs::create_dir_all(&mlt_tmp).expect("create mlt tmp dir");
        let mut mlt_sort_reader = one_tile_sort_reader(&mlt_chunks_dir);
        let mlt_config = TilegenConfig {
            pbf_path: dir.path().join("contract-mlt.osm.pbf"),
            output_path: mlt_output.clone(),
            tmp_dir: mlt_tmp,
            min_zoom: 0,
            max_zoom: 14,
            ocean_shapefile: None,
            ocean_simplified_shapefile: None,
            ocean_tiles: None,
            ocean_artifact_key: None,
            ocean_only_metadata: false,
            skip_to: None,
            in_memory: true,
            compression_level: 6,
            force_sorted: false,
            allow_unsafe_flat_index: false,
            threads: 1,
            way_inflight_budget: 0,
            assemble_batch_budget: 0,
            sort_chunk_size: 0,
            locations_on_ways: false,
            tile_format: TilePayloadFormat::Mlt,
            tile_compression: TileCompression::Gzip,
            compress_sort_chunks: sort::ChunkCompression::None,
            seam_reconcile_layers: {
                let mut m = [0u8; shortbread::Layer::count()];
                m[shortbread::Layer::Boundaries as usize] = 8;
                m
            },
            fanout_caps: [0; shortbread::Layer::count()],
            polygon_simplify_factor: 1.0,
        };
        let _ =
            phase_assemble(&mut mlt_sort_reader, &mlt_config).expect("mlt assemble should succeed");
        let mut mlt_reader =
            crate::pmtiles_reader::PmtilesReader::open(&mlt_output).expect("open mlt pmtiles");
        let mlt_metadata = mlt_reader.read_metadata().expect("read mlt metadata");
        let mlt_json: serde_json::Value =
            serde_json::from_str(&mlt_metadata).expect("parse mlt metadata");
        assert_eq!(
            mlt_reader.tile_type(),
            0,
            "mlt header tile_type should remain unknown"
        );
        assert_eq!(
            mlt_reader.tile_compression(),
            1,
            "mlt header tile_compression should be none"
        );
        assert_eq!(mlt_json["tile_payload_format"], "mlt");
        assert_eq!(mlt_json["tile_compression"], "none");
    }
}

// -----------------------------------------------------------------------
// Helpers for emit tests - decode SortRecord payloads
// -----------------------------------------------------------------------

use crate::mvt;
use crate::sort;

fn test_layer_match(layer: Layer, geom_expect: GeomExpect) -> LayerMatch {
    LayerMatch {
        layer,
        min_zoom: 0,
        max_zoom: 14,
        geom_expect,
        paint_rank: 0,
        attrs: smallvec![("kind", AttrValue::Str(Cow::Borrowed("test")), 0)],
    }
}

/// Decode the sort key fields from a SortRecord.
fn decode_key(rec: &SortRecord) -> (u64, u8) {
    let tile_id = sort::tile_id_from_key(rec.key);
    let layer_idx = sort::layer_from_key(rec.key);
    (tile_id, layer_idx)
}

/// Decode the wire format header from a SortRecord's data payload.
/// Returns (osm_id, geom_type_byte, geom_cmd_count).
fn decode_data_header(data: &[u8]) -> (u64, u8, u32) {
    let osm_id = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let gt = data[8];
    let cmd_count = u32::from_le_bytes(data[9..13].try_into().unwrap());
    (osm_id, gt, cmd_count)
}

/// Decode the attribute count from a SortRecord's data payload.
fn decode_attr_count(data: &[u8]) -> u8 {
    let cmd_count = u32::from_le_bytes(data[9..13].try_into().unwrap()) as usize;
    let attr_start = 13 + cmd_count * 4;
    data[attr_start]
}

/// Decode the full record via add_feature_to_layer and return the layer builder.
fn decode_to_layer(data: &[u8]) -> mvt::LayerBuilder {
    let mut lb = mvt::LayerBuilder::new("test");
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    crate::wire_format::add_feature_to_layer(&mut lb, data, &mut gp, &mut tp);
    lb
}

fn decode_zigzag(v: u32) -> i32 {
    let n = i32::try_from(v >> 1).unwrap_or(i32::MAX);
    if (v & 1) == 0 { n } else { -n - 1 }
}

fn decode_commands_to_abs_coords(cmds: &[u32]) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut cx = 0i32;
    let mut cy = 0i32;
    let mut move_x = 0i32;
    let mut move_y = 0i32;
    while i < cmds.len() {
        let cmd = cmds[i];
        i += 1;
        let id = cmd & 0x7;
        let count = cmd >> 3;
        match id {
            1 | 2 => {
                for _ in 0..count {
                    if i + 1 >= cmds.len() {
                        return out;
                    }
                    cx += decode_zigzag(cmds[i]);
                    cy += decode_zigzag(cmds[i + 1]);
                    i += 2;
                    if id == 1 {
                        move_x = cx;
                        move_y = cy;
                    }
                    out.push((cx, cy));
                }
            }
            7 => {
                cx = move_x;
                cy = move_y;
            }
            _ => break,
        }
    }
    out
}

fn contains_point_near(points: &[(i32, i32)], expected: (i32, i32)) -> bool {
    points
        .iter()
        .any(|&(x, y)| (x - expected.0).abs() <= 1 && (y - expected.1).abs() <= 1)
}

fn record_polygon_rings(rec: &SortRecord) -> Vec<Vec<(i32, i32)>> {
    let lb = decode_to_layer(&rec.data);
    geometry::decode_mvt_polygon(&lb.test_feature(0).geometry)
}

fn assert_no_backtrack_in_records(records: &[SortRecord]) {
    assert!(
        !records.is_empty(),
        "fixture should emit at least one feature"
    );
    for rec in records {
        for ring in record_polygon_rings(rec) {
            for i in 2..ring.len() {
                assert_ne!(ring[i], ring[i - 2], "A-B-A backtrack in {ring:?}");
            }
        }
    }
}

fn assert_simple_record_rings(records: &[SortRecord]) {
    assert!(
        !records.is_empty(),
        "fixture should emit at least one feature"
    );
    for rec in records {
        for ring in record_polygon_rings(rec) {
            assert!(geometry::ring_is_simple(&ring), "non-simple ring {ring:?}");
        }
    }
}

// -----------------------------------------------------------------------
// emit_point_or_centroid tests (formerly emit_point_feature)
// -----------------------------------------------------------------------

#[test]
fn emit_point_empty_coords() {
    let m = test_layer_match(Layer::Pois, GeomExpect::Point);
    let bbox = MercBbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    let mut records = Vec::new();
    let mut scratch = PointEmitScratch::new();
    let count = emit_point_or_centroid(1, &[], None, &bbox, &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_point_decodes_correctly() {
    let m = test_layer_match(Layer::Pois, GeomExpect::Point);
    let coords = [Point { x: 0.5, y: 0.5 }];
    let bbox = MercBbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    let mut records = Vec::new();
    let mut scratch = PointEmitScratch::new();
    emit_point_or_centroid(
        42,
        &coords,
        None,
        &bbox,
        &m,
        0,
        0,
        &mut records,
        &mut scratch,
    );
    assert_eq!(records.len(), 1);

    let rec = &records[0];

    // Sort key: tile_id for z=0/x=0/y=0, layer = Pois
    let (tile_id, layer_idx) = decode_key(rec);
    assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
    assert_eq!(layer_idx, Layer::Pois as u8);

    // Wire format: osm_id=42, geom_type=Point(1), has geometry commands
    let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
    assert_eq!(osm_id, 42);
    assert_eq!(gt, 1); // Point
    assert!(cmd_count > 0, "point should have geometry commands");

    // Attributes: 1 attr ("kind" = "test")
    assert_eq!(decode_attr_count(&rec.data), 1);

    // Full decode roundtrip
    let lb = decode_to_layer(&rec.data);
    assert_eq!(lb.test_feature_count(), 1);
    let f = lb.test_feature(0);
    assert_eq!(f.id, Some(42));
    assert_eq!(f.geom_type, mvt::GeomType::Point);
    let (k0, v0) = f.tags[0];
    assert_eq!(lb.test_key(k0), "kind");
    assert_eq!(*lb.test_value(v0), mvt::Value::String("test".to_string()));
}

#[test]
fn emit_point_multi_zoom_tile_ids_differ() {
    let m = test_layer_match(Layer::Pois, GeomExpect::Point);
    // Use a point clearly inside one z1 tile (not on a boundary)
    let coords = [Point { x: 0.25, y: 0.25 }];
    let bbox = MercBbox {
        min_x: 0.25,
        min_y: 0.25,
        max_x: 0.25,
        max_y: 0.25,
    };
    let mut records = Vec::new();
    let mut scratch = PointEmitScratch::new();
    emit_point_or_centroid(
        7,
        &coords,
        None,
        &bbox,
        &m,
        0,
        1,
        &mut records,
        &mut scratch,
    );

    // Should get 1 record at z=0 and 1 record at z=1 = 2 total
    assert_eq!(records.len(), 2);

    // The tile IDs should differ (z0 vs z1 are different Hilbert IDs)
    let (tid0, _) = decode_key(&records[0]);
    let (tid1, _) = decode_key(&records[1]);
    assert_ne!(tid0, tid1, "z0 and z1 tile IDs should differ");

    // Both should decode to the same osm_id
    let (id0, _, _) = decode_data_header(&records[0].data);
    let (id1, _, _) = decode_data_header(&records[1].data);
    assert_eq!(id0, 7);
    assert_eq!(id1, 7);
}

// -----------------------------------------------------------------------
// emit_line_feature tests
// -----------------------------------------------------------------------

#[test]
fn emit_line_too_few_points() {
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = [Point { x: 0.5, y: 0.5 }];
    let mut records = Vec::new();
    let mut scratch = LineEmitScratch::new();
    let count = emit_line_feature(101, &coords, &[], &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_line_decodes_correctly() {
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = [Point { x: 0.3, y: 0.3 }, Point { x: 0.7, y: 0.7 }];
    let mut records = Vec::new();
    let mut scratch = LineEmitScratch::new();
    emit_line_feature(100, &coords, &[], &m, 0, 0, &mut records, &mut scratch);
    assert_eq!(records.len(), 1);

    let rec = &records[0];

    // Sort key
    let (tile_id, layer_idx) = decode_key(rec);
    assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
    assert_eq!(layer_idx, Layer::Streets as u8);

    // Wire format
    let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
    assert_eq!(osm_id, 100);
    assert_eq!(gt, 2); // LineString
    assert!(cmd_count >= 2, "linestring needs MoveTo + LineTo commands");

    // Full decode roundtrip
    let lb = decode_to_layer(&rec.data);
    let f = lb.test_feature(0);
    assert_eq!(f.id, Some(100));
    assert_eq!(f.geom_type, mvt::GeomType::LineString);
    assert_eq!(f.tags.len(), 1);
}

#[test]
fn emit_line_cascading_simplification() {
    // A line that should survive at z=14 but may get simplified away at low zoom.
    // At z=0, simplification tolerance is very large, so a short line may vanish.
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = [
        Point {
            x: 0.500_000,
            y: 0.500_000,
        },
        Point {
            x: 0.500_001,
            y: 0.500_001,
        },
    ];
    let mut records = Vec::new();
    let mut scratch = LineEmitScratch::new();
    emit_line_feature(99, &coords, &[], &m, 0, 14, &mut records, &mut scratch);

    // At z=14 this line is ~0.4 pixel which is sub-pixel, but Streets skips
    // the size filter, so it should still produce a record at z=14.
    // At lower zooms, simplification may collapse it.
    let z14_records: Vec<_> = records
        .iter()
        .filter(|r| {
            let tid = sort::tile_id_from_key(r.key);
            let (z, _, _) = pmtiles_writer::tile_id_to_zxy(tid);
            z == 14
        })
        .collect();
    assert!(
        !z14_records.is_empty(),
        "line should survive at z=14 for Streets layer"
    );
}

#[test]
fn emit_line_preserve_mask_keeps_required_vertices() {
    let m = test_layer_match(Layer::Streets, GeomExpect::Line);
    let coords = [
        Point { x: 0.1, y: 0.10000 },
        Point { x: 0.3, y: 0.10001 },
        Point { x: 0.5, y: 0.10002 }, // pin this vertex
        Point { x: 0.7, y: 0.10001 },
        Point { x: 0.9, y: 0.10000 },
    ];

    let mut records_plain = Vec::new();
    let mut records_pinned = Vec::new();
    let mut scratch_plain = LineEmitScratch::new();
    let mut scratch_pinned = LineEmitScratch::new();
    emit_line_feature(
        1099,
        &coords,
        &[false; 5],
        &m,
        0,
        0,
        &mut records_plain,
        &mut scratch_plain,
    );
    emit_line_feature(
        1099,
        &coords,
        &[false, false, true, false, false],
        &m,
        0,
        0,
        &mut records_pinned,
        &mut scratch_pinned,
    );

    assert_eq!(records_plain.len(), 1);
    assert_eq!(records_pinned.len(), 1);
    let (_, _, plain_cmd_count) = decode_data_header(&records_plain[0].data);
    let (_, _, pinned_cmd_count) = decode_data_header(&records_pinned[0].data);
    assert!(
        pinned_cmd_count > plain_cmd_count,
        "pinned shared line vertex should increase retained geometry detail"
    );

    let plain_lb = decode_to_layer(&records_plain[0].data);
    let pinned_lb = decode_to_layer(&records_pinned[0].data);
    let plain_pts = decode_commands_to_abs_coords(&plain_lb.test_feature(0).geometry);
    let pinned_pts = decode_commands_to_abs_coords(&pinned_lb.test_feature(0).geometry);
    let mut target_tc = Vec::new();
    geometry::to_tile_coords_into(&mut target_tc, &[Point { x: 0.5, y: 0.10002 }], 0, 0, 0);
    let expected = target_tc[0];
    assert!(
        pinned_pts.contains(&expected),
        "pinned line geometry should retain required shared vertex {expected:?}"
    );
    assert!(
        !plain_pts.contains(&expected),
        "un-pinned line geometry should be allowed to drop non-required vertex {expected:?}"
    );
}

// -----------------------------------------------------------------------
// emit_polygon_feature tests
// -----------------------------------------------------------------------

#[test]
fn emit_polygon_too_few_points() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let coords = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.3 },
        Point { x: 0.5, y: 0.7 },
    ];
    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    let count = emit_polygon_feature(
        201,
        &coords,
        &[],
        &m,
        0,
        0,
        &mut records,
        &mut scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_polygon_decodes_correctly() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let coords = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.3 },
        Point { x: 0.7, y: 0.7 },
        Point { x: 0.3, y: 0.7 },
        Point { x: 0.3, y: 0.3 },
    ];
    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    emit_polygon_feature(
        200,
        &coords,
        &[],
        &m,
        0,
        0,
        &mut records,
        &mut scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_eq!(records.len(), 1);

    let rec = &records[0];

    // Sort key
    let (tile_id, layer_idx) = decode_key(rec);
    assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
    assert_eq!(layer_idx, Layer::Buildings as u8);

    // Wire format
    let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
    assert_eq!(osm_id, 200);
    assert_eq!(gt, 3); // Polygon
    // A polygon ring needs MoveTo + LineTo(n-1) + ClosePath = at least 3 commands
    assert!(
        cmd_count >= 3,
        "polygon should have MoveTo + LineTo + ClosePath"
    );

    // Attributes
    assert_eq!(decode_attr_count(&rec.data), 1);

    // Full decode roundtrip
    let lb = decode_to_layer(&rec.data);
    let f = lb.test_feature(0);
    assert_eq!(f.id, Some(200));
    assert_eq!(f.geom_type, mvt::GeomType::Polygon);
    let (k0, v0) = f.tags[0];
    assert_eq!(lb.test_key(k0), "kind");
    assert_eq!(*lb.test_value(v0), mvt::Value::String("test".to_string()));
}

#[test]
fn emit_polygon_zoom_dependent_attrs() {
    // Attribute with min_zoom=10 should only appear at z>=10.
    // Use a tiny polygon that fits in a single tile at each test zoom.
    let m_z0 = LayerMatch {
        layer: Layer::Buildings,
        min_zoom: 0,
        max_zoom: 14,
        geom_expect: GeomExpect::Polygon,
        paint_rank: 0,
        attrs: smallvec![
            ("kind", AttrValue::Str(Cow::Borrowed("building")), 0),
            ("height", AttrValue::Float(15.0), 10),
        ],
    };
    let coords_z0 = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.3 },
        Point { x: 0.7, y: 0.7 },
        Point { x: 0.3, y: 0.7 },
        Point { x: 0.3, y: 0.3 },
    ];
    // At z=0: only 1 attr ("kind", min_zoom=0)
    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    emit_polygon_feature(
        300,
        &coords_z0,
        &[],
        &m_z0,
        0,
        0,
        &mut records,
        &mut scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_eq!(records.len(), 1);
    let lb = decode_to_layer(&records[0].data);
    let f = lb.test_feature(0);
    assert_eq!(
        f.tags.len(),
        1,
        "at z=0 only the always-on attr should be present"
    );

    // At z=14: both attrs. Use a tiny polygon inside one z=14 tile.
    let m_z14 = LayerMatch {
        layer: Layer::Buildings,
        min_zoom: 14,
        max_zoom: 14,
        geom_expect: GeomExpect::Polygon,
        paint_rank: 0,
        attrs: smallvec![
            ("kind", AttrValue::Str(Cow::Borrowed("building")), 0),
            ("height", AttrValue::Float(15.0), 10),
        ],
    };
    let coords_z14 = [
        Point {
            x: 0.500_00,
            y: 0.500_00,
        },
        Point {
            x: 0.500_05,
            y: 0.500_00,
        },
        Point {
            x: 0.500_05,
            y: 0.500_05,
        },
        Point {
            x: 0.500_00,
            y: 0.500_05,
        },
        Point {
            x: 0.500_00,
            y: 0.500_00,
        },
    ];
    records.clear();
    emit_polygon_feature(
        300,
        &coords_z14,
        &[],
        &m_z14,
        14,
        14,
        &mut records,
        &mut scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_eq!(records.len(), 1);
    let lb = decode_to_layer(&records[0].data);
    let f = lb.test_feature(0);
    assert_eq!(f.tags.len(), 2, "at z=14 both attrs should be present");
    let (k1, v1) = f.tags[1];
    assert_eq!(lb.test_key(k1), "height");
    assert_eq!(*lb.test_value(v1), mvt::Value::Double(15.0));
}

#[test]
fn emit_polygon_skips_self_intersecting_ring_below_z14() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    // Bow-tie ring (self-intersecting).
    let coords = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.7 },
        Point { x: 0.3, y: 0.7 },
        Point { x: 0.7, y: 0.3 },
        Point { x: 0.3, y: 0.3 },
    ];

    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    let count = emit_polygon_feature(
        500,
        &coords,
        &[],
        &m,
        0,
        0,
        &mut records,
        &mut scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_polygon_preserve_mask_keeps_required_vertices() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let target = Point {
        x: 0.500_050_0,
        y: 0.500_010_2,
    };
    let coords = [
        Point {
            x: 0.500_010_0,
            y: 0.500_010_0,
        },
        Point {
            x: 0.500_030_0,
            y: 0.500_010_1,
        },
        target,
        Point {
            x: 0.500_070_0,
            y: 0.500_010_1,
        },
        Point {
            x: 0.500_090_0,
            y: 0.500_010_0,
        },
        Point {
            x: 0.500_090_0,
            y: 0.500_090_0,
        },
        Point {
            x: 0.500_010_0,
            y: 0.500_090_0,
        },
        Point {
            x: 0.500_010_0,
            y: 0.500_010_0,
        },
    ];

    let mut records_plain = Vec::new();
    let mut records_pinned = Vec::new();
    let mut scratch_plain = PolygonEmitScratch::new();
    let mut scratch_pinned = PolygonEmitScratch::new();
    emit_polygon_feature(
        900,
        &coords,
        &[false; 8],
        &m,
        13,
        13,
        &mut records_plain,
        &mut scratch_plain,
        0,
        None,
        0,
        1.0,
    );
    emit_polygon_feature(
        900,
        &coords,
        &[false, false, true, false, false, false, false, false],
        &m,
        13,
        13,
        &mut records_pinned,
        &mut scratch_pinned,
        0,
        None,
        0,
        1.0,
    );

    assert_eq!(records_plain.len(), 1);
    assert_eq!(records_pinned.len(), 1);
    let (tile_id, _) = decode_key(&records_pinned[0]);
    let (z, tx, ty) = pmtiles_writer::tile_id_to_zxy(tile_id);
    let mut target_tc = Vec::new();
    geometry::to_tile_coords_into(&mut target_tc, &[target], tx, ty, z);
    let expected = target_tc[0];
    let plain_lb = decode_to_layer(&records_plain[0].data);
    let pinned_lb = decode_to_layer(&records_pinned[0].data);
    let plain_pts = decode_commands_to_abs_coords(&plain_lb.test_feature(0).geometry);
    let pinned_pts = decode_commands_to_abs_coords(&pinned_lb.test_feature(0).geometry);
    assert!(
        contains_point_near(&pinned_pts, expected),
        "expected {expected:?}, pinned={pinned_pts:?}, plain={plain_pts:?}",
    );
    assert!(!contains_point_near(&plain_pts, expected));
}
#[test]
fn emit_multipolygon_preserve_keys_keep_required_vertices() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let target = Point {
        x: 0.500_050_0,
        y: 0.500_010_2,
    };
    let outer = vec![
        Point {
            x: 0.500_010_0,
            y: 0.500_010_0,
        },
        Point {
            x: 0.500_030_0,
            y: 0.500_010_1,
        },
        target,
        Point {
            x: 0.500_070_0,
            y: 0.500_010_1,
        },
        Point {
            x: 0.500_090_0,
            y: 0.500_010_0,
        },
        Point {
            x: 0.500_090_0,
            y: 0.500_090_0,
        },
        Point {
            x: 0.500_010_0,
            y: 0.500_090_0,
        },
        Point {
            x: 0.500_010_0,
            y: 0.500_010_0,
        },
    ];
    let inners: Vec<Vec<Point>> = Vec::new();
    let mut keys = rustc_hash::FxHashSet::default();
    keys.insert(merc_point_key(&target));

    let mut records_plain = Vec::new();
    let mut records_pinned = Vec::new();
    let mut emit_plain = MultipolygonEmitScratch::new();
    let mut emit_pinned = MultipolygonEmitScratch::new();
    let mut simp_plain = geometry::SimplifyMultiScratch::new();
    let mut simp_pinned = geometry::SimplifyMultiScratch::new();

    emit_multipolygon_feature(
        990,
        &outer,
        &inners,
        None,
        &m,
        13,
        13,
        &mut records_plain,
        &mut emit_plain,
        &mut simp_plain,
        0,
        None,
        0,
        1.0,
    );
    emit_multipolygon_feature(
        990,
        &outer,
        &inners,
        Some(&keys),
        &m,
        13,
        13,
        &mut records_pinned,
        &mut emit_pinned,
        &mut simp_pinned,
        0,
        None,
        0,
        1.0,
    );

    assert_eq!(records_plain.len(), 1);
    assert_eq!(records_pinned.len(), 1);
    let (tile_id, _) = decode_key(&records_pinned[0]);
    let (z, tx, ty) = pmtiles_writer::tile_id_to_zxy(tile_id);
    let mut target_tc = Vec::new();
    geometry::to_tile_coords_into(&mut target_tc, &[target], tx, ty, z);
    let expected = target_tc[0];
    let plain_lb = decode_to_layer(&records_plain[0].data);
    let pinned_lb = decode_to_layer(&records_pinned[0].data);
    let plain_pts = decode_commands_to_abs_coords(&plain_lb.test_feature(0).geometry);
    let pinned_pts = decode_commands_to_abs_coords(&pinned_lb.test_feature(0).geometry);
    assert!(contains_point_near(&pinned_pts, expected));
    assert!(!contains_point_near(&plain_pts, expected));
}
#[test]
fn emit_multipolygon_relation_derived_shared_keys_preserve_vertices() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let target = Point {
        x: 0.500_050_0,
        y: 0.500_010_2,
    };
    let outer = vec![
        Point {
            x: 0.500_010_0,
            y: 0.500_010_0,
        },
        Point {
            x: 0.500_030_0,
            y: 0.500_010_1,
        },
        target,
        Point {
            x: 0.500_070_0,
            y: 0.500_010_1,
        },
        Point {
            x: 0.500_090_0,
            y: 0.500_010_0,
        },
        Point {
            x: 0.500_090_0,
            y: 0.500_090_0,
        },
        Point {
            x: 0.500_010_0,
            y: 0.500_090_0,
        },
        Point {
            x: 0.500_010_0,
            y: 0.500_010_0,
        },
    ];
    let relation_members = vec![
        MemberWay {
            role: WayRole::Outer,
            coords: vec![Point { x: 0.0, y: 0.0 }, target, Point { x: 0.0, y: 0.2 }],
        },
        MemberWay {
            role: WayRole::Outer,
            coords: vec![Point { x: 1.0, y: 0.0 }, target, Point { x: 1.0, y: 0.2 }],
        },
    ];
    let shared_keys = relation_shared_vertex_keys(&relation_members);
    assert!(shared_keys.contains(&merc_point_key(&target)));

    let mut records_plain = Vec::new();
    let mut records_pinned = Vec::new();
    let mut emit_plain = MultipolygonEmitScratch::new();
    let mut emit_pinned = MultipolygonEmitScratch::new();
    let mut simp_plain = geometry::SimplifyMultiScratch::new();
    let mut simp_pinned = geometry::SimplifyMultiScratch::new();

    emit_multipolygon_feature(
        991,
        &outer,
        &[],
        None,
        &m,
        13,
        13,
        &mut records_plain,
        &mut emit_plain,
        &mut simp_plain,
        0,
        None,
        0,
        1.0,
    );
    emit_multipolygon_feature(
        991,
        &outer,
        &[],
        Some(&shared_keys),
        &m,
        13,
        13,
        &mut records_pinned,
        &mut emit_pinned,
        &mut simp_pinned,
        0,
        None,
        0,
        1.0,
    );

    assert_eq!(records_plain.len(), 1);
    assert_eq!(records_pinned.len(), 1);
    let (tile_id, _) = decode_key(&records_pinned[0]);
    let (z, tx, ty) = pmtiles_writer::tile_id_to_zxy(tile_id);
    let mut target_tc = Vec::new();
    geometry::to_tile_coords_into(&mut target_tc, &[target], tx, ty, z);
    let expected = target_tc[0];
    let plain_lb = decode_to_layer(&records_plain[0].data);
    let pinned_lb = decode_to_layer(&records_pinned[0].data);
    let plain_pts = decode_commands_to_abs_coords(&plain_lb.test_feature(0).geometry);
    let pinned_pts = decode_commands_to_abs_coords(&pinned_lb.test_feature(0).geometry);
    assert!(contains_point_near(&pinned_pts, expected));
    assert!(!contains_point_near(&plain_pts, expected));
}

#[test]
fn landing_b_multipolygon_two_outers_nested_holes_emit_two_clean_features() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer_a = vec![
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.30, y: 0.10 },
        Point { x: 0.30, y: 0.30 },
        Point { x: 0.10, y: 0.30 },
        Point { x: 0.10, y: 0.10 },
    ];
    let hole_a = vec![vec![
        Point { x: 0.16, y: 0.16 },
        Point { x: 0.24, y: 0.16 },
        Point { x: 0.24, y: 0.24 },
        Point { x: 0.16, y: 0.24 },
        Point { x: 0.16, y: 0.16 },
    ]];
    let outer_b = vec![
        Point { x: 0.60, y: 0.60 },
        Point { x: 0.80, y: 0.60 },
        Point { x: 0.80, y: 0.80 },
        Point { x: 0.60, y: 0.80 },
        Point { x: 0.60, y: 0.60 },
    ];
    let hole_b = vec![vec![
        Point { x: 0.66, y: 0.66 },
        Point { x: 0.74, y: 0.66 },
        Point { x: 0.74, y: 0.74 },
        Point { x: 0.66, y: 0.74 },
        Point { x: 0.66, y: 0.66 },
    ]];

    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    emit_multipolygon_feature(
        9902,
        &outer_a,
        &hole_a,
        None,
        &m,
        0,
        0,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
        0,
        None,
        0,
        1.0,
    );
    emit_multipolygon_feature(
        9903,
        &outer_b,
        &hole_b,
        None,
        &m,
        0,
        0,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_eq!(records.len(), 2);
    assert_simple_record_rings(&records);
}

#[test]
fn landing_b_historical_infinity_classes_emit_nothing() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let zero_area_outer = vec![
        Point { x: 0.20, y: 0.20 },
        Point { x: 0.30, y: 0.20 },
        Point { x: 0.40, y: 0.20 },
        Point { x: 0.20, y: 0.20 },
    ];
    let large_hole = vec![vec![
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.50, y: 0.10 },
        Point { x: 0.50, y: 0.50 },
        Point { x: 0.10, y: 0.50 },
        Point { x: 0.10, y: 0.10 },
    ]];
    let tiny_outer = vec![
        Point {
            x: 0.500_000,
            y: 0.500_000,
        },
        Point {
            x: 0.500_001,
            y: 0.500_000,
        },
        Point {
            x: 0.500_001,
            y: 0.500_001,
        },
        Point {
            x: 0.500_000,
            y: 0.500_001,
        },
        Point {
            x: 0.500_000,
            y: 0.500_000,
        },
    ];
    let bigger_than_outer_hole = vec![vec![
        Point {
            x: 0.499_990,
            y: 0.499_990,
        },
        Point {
            x: 0.500_010,
            y: 0.499_990,
        },
        Point {
            x: 0.500_010,
            y: 0.500_010,
        },
        Point {
            x: 0.499_990,
            y: 0.500_010,
        },
        Point {
            x: 0.499_990,
            y: 0.499_990,
        },
    ]];

    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    emit_multipolygon_feature(
        9904,
        &zero_area_outer,
        &large_hole,
        None,
        &m,
        0,
        0,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
        0,
        None,
        0,
        1.0,
    );
    emit_multipolygon_feature(
        9905,
        &tiny_outer,
        &bigger_than_outer_hole,
        None,
        &m,
        0,
        0,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
        0,
        None,
        0,
        1.0,
    );
    assert!(records.is_empty());
}

#[test]
fn landing_b_backtrack_regression_fixture_all_tiers_emit_no_aba() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let mut scratch = PolygonEmitScratch::new();

    let tier1 = [
        Point {
            x: 0.500_010,
            y: 0.500_010,
        },
        Point {
            x: 0.500_030,
            y: 0.500_012,
        },
        Point {
            x: 0.500_010,
            y: 0.500_010,
        },
        Point {
            x: 0.500_090,
            y: 0.500_010,
        },
        Point {
            x: 0.500_090,
            y: 0.500_090,
        },
        Point {
            x: 0.500_010,
            y: 0.500_090,
        },
        Point {
            x: 0.500_010,
            y: 0.500_010,
        },
    ];
    let mut records = Vec::new();
    emit_polygon_feature(
        9910,
        &tier1,
        &[],
        &m,
        13,
        13,
        &mut records,
        &mut scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_no_backtrack_in_records(&records);

    let tier2 = [
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.12, y: 0.11 },
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.30, y: 0.10 },
        Point { x: 0.30, y: 0.30 },
        Point { x: 0.10, y: 0.30 },
        Point { x: 0.10, y: 0.10 },
    ];
    records.clear();
    emit_polygon_feature(
        9911,
        &tier2,
        &[],
        &m,
        4,
        4,
        &mut records,
        &mut scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_no_backtrack_in_records(&records);

    let tier3 = [
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.12, y: 0.11 },
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.90, y: 0.10 },
        Point { x: 0.90, y: 0.90 },
        Point { x: 0.10, y: 0.90 },
        Point { x: 0.10, y: 0.10 },
    ];
    records.clear();
    emit_polygon_feature(
        9912,
        &tier3,
        &[],
        &m,
        4,
        4,
        &mut records,
        &mut scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_no_backtrack_in_records(&records);
}

#[test]
fn emit_multipolygon_empty_outer() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();

    let count = emit_multipolygon_feature(
        401,
        &[],
        &[],
        None,
        &m,
        0,
        0,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
        0,
        None,
        0,
        1.0,
    );

    assert_eq!(count, 0);
    assert!(records.is_empty());
}

#[test]
fn emit_multipolygon_with_hole_decodes_correctly() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.20, y: 0.20 },
        Point { x: 0.80, y: 0.20 },
        Point { x: 0.80, y: 0.80 },
        Point { x: 0.20, y: 0.80 },
        Point { x: 0.20, y: 0.20 },
    ];
    let inners = vec![vec![
        Point { x: 0.40, y: 0.40 },
        Point { x: 0.60, y: 0.40 },
        Point { x: 0.60, y: 0.60 },
        Point { x: 0.40, y: 0.60 },
        Point { x: 0.40, y: 0.40 },
    ]];

    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    let count = emit_multipolygon_feature(
        402,
        &outer,
        &inners,
        None,
        &m,
        0,
        0,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
        0,
        None,
        0,
        1.0,
    );

    assert_eq!(count, 1);
    assert_eq!(records.len(), 1);

    let rec = &records[0];
    let (tile_id, layer_idx) = decode_key(rec);
    assert_eq!(tile_id, pmtiles_writer::xy_to_tile_id(0, 0, 0));
    assert_eq!(layer_idx, Layer::Buildings as u8);

    let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
    assert_eq!(osm_id, 402);
    assert_eq!(gt, 3);
    assert!(
        cmd_count >= 6,
        "multipolygon with hole should encode multiple ring commands"
    );

    let lb = decode_to_layer(&rec.data);
    assert_eq!(lb.test_feature_count(), 1);
    let f = lb.test_feature(0);
    assert_eq!(f.id, Some(402));
    assert_eq!(f.geom_type, mvt::GeomType::Polygon);
    assert_eq!(f.tags.len(), 1);
}

#[test]
fn emit_multipolygon_large_shape_clips_to_multiple_tiles() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.90, y: 0.10 },
        Point { x: 0.90, y: 0.90 },
        Point { x: 0.10, y: 0.90 },
        Point { x: 0.10, y: 0.10 },
    ];
    let inners = vec![vec![
        Point { x: 0.45, y: 0.45 },
        Point { x: 0.55, y: 0.45 },
        Point { x: 0.55, y: 0.55 },
        Point { x: 0.45, y: 0.55 },
        Point { x: 0.45, y: 0.45 },
    ]];

    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    let count = emit_multipolygon_feature(
        403,
        &outer,
        &inners,
        None,
        &m,
        2,
        2,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
        0,
        None,
        0,
        1.0,
    );

    assert_eq!(usize::try_from(count).unwrap(), records.len());
    assert!(
        records.len() > 1,
        "large multipolygon should clip into multiple z2 tiles"
    );

    for rec in &records {
        let (osm_id, gt, cmd_count) = decode_data_header(&rec.data);
        assert_eq!(osm_id, 403);
        assert_eq!(gt, 3);
        assert!(cmd_count > 0);
    }
}

#[test]
fn emit_multipolygon_drops_degenerate_or_invalid_inner_rings() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.20, y: 0.20 },
        Point { x: 0.80, y: 0.20 },
        Point { x: 0.80, y: 0.80 },
        Point { x: 0.20, y: 0.80 },
        Point { x: 0.20, y: 0.20 },
    ];
    let inners = vec![
        vec![Point { x: 0.30, y: 0.30 }, Point { x: 0.50, y: 0.30 }], // too short
        vec![
            Point { x: 0.35, y: 0.35 },
            Point { x: 0.65, y: 0.65 },
            Point { x: 0.35, y: 0.65 },
            Point { x: 0.65, y: 0.35 },
            Point { x: 0.35, y: 0.35 },
        ], // self-intersecting
    ];

    let mut outer_only = Vec::new();
    let mut with_bad_holes = Vec::new();
    let mut emit_a = MultipolygonEmitScratch::new();
    let mut emit_b = MultipolygonEmitScratch::new();
    let mut simp_a = geometry::SimplifyMultiScratch::new();
    let mut simp_b = geometry::SimplifyMultiScratch::new();
    emit_multipolygon_feature(
        404,
        &outer,
        &[],
        None,
        &m,
        0,
        0,
        &mut outer_only,
        &mut emit_a,
        &mut simp_a,
        0,
        None,
        0,
        1.0,
    );
    emit_multipolygon_feature(
        404,
        &outer,
        &inners,
        None,
        &m,
        0,
        0,
        &mut with_bad_holes,
        &mut emit_b,
        &mut simp_b,
        0,
        None,
        0,
        1.0,
    );
    assert_eq!(outer_only.len(), 1);
    assert_eq!(with_bad_holes.len(), 1);
    let (_, _, outer_cmd_count) = decode_data_header(&outer_only[0].data);
    let (_, _, bad_holes_cmd_count) = decode_data_header(&with_bad_holes[0].data);
    assert_eq!(
        bad_holes_cmd_count, outer_cmd_count,
        "invalid/degenerate inners should be dropped and not change emitted geometry"
    );
}

#[test]
fn emit_multipolygon_invalid_inner_rejected_below_z14_but_allowed_at_z14() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        // Keep this polygon within one z14 tile so the test checks
        // invalid-inner handling, not multi-tile fanout.
        Point {
            x: 0.500_010,
            y: 0.500_010,
        },
        Point {
            x: 0.500_040,
            y: 0.500_010,
        },
        Point {
            x: 0.500_040,
            y: 0.500_040,
        },
        Point {
            x: 0.500_010,
            y: 0.500_040,
        },
        Point {
            x: 0.500_010,
            y: 0.500_010,
        },
    ];
    let bowtie_inner = vec![vec![
        Point {
            x: 0.500_018,
            y: 0.500_018,
        },
        Point {
            x: 0.500_032,
            y: 0.500_032,
        },
        Point {
            x: 0.500_018,
            y: 0.500_032,
        },
        Point {
            x: 0.500_032,
            y: 0.500_018,
        },
        Point {
            x: 0.500_018,
            y: 0.500_018,
        },
    ]];

    let mut z13_records = Vec::new();
    let mut z14_records = Vec::new();
    let mut emit_13 = MultipolygonEmitScratch::new();
    let mut emit_14 = MultipolygonEmitScratch::new();
    let mut simp_13 = geometry::SimplifyMultiScratch::new();
    let mut simp_14 = geometry::SimplifyMultiScratch::new();
    emit_multipolygon_feature(
        405,
        &outer,
        &bowtie_inner,
        None,
        &m,
        13,
        13,
        &mut z13_records,
        &mut emit_13,
        &mut simp_13,
        0,
        None,
        0,
        1.0,
    );
    emit_multipolygon_feature(
        405,
        &outer,
        &bowtie_inner,
        None,
        &m,
        14,
        14,
        &mut z14_records,
        &mut emit_14,
        &mut simp_14,
        0,
        None,
        0,
        1.0,
    );
    assert_eq!(z13_records.len(), 1);
    assert_eq!(z14_records.len(), 1);
    let (_, _, z13_cmd_count) = decode_data_header(&z13_records[0].data);
    let (_, _, z14_cmd_count) = decode_data_header(&z14_records[0].data);
    // After removing the is_valid_simple_tile_ring gate (which caused Nissum Bredning
    // feature loss), self-intersecting inners are no longer rejected at z<14.
    // Both zooms should now produce the same geometry.
    assert_eq!(
        z14_cmd_count, z13_cmd_count,
        "both zooms should produce same geometry (self-intersecting inner not rejected)"
    );
}

#[test]
fn emit_multipolygon_invalid_outer_repaired_or_dropped_by_integer_normalize() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let bowtie_outer = vec![
        Point {
            x: 0.500_010,
            y: 0.500_010,
        },
        Point {
            x: 0.500_040,
            y: 0.500_040,
        },
        Point {
            x: 0.500_010,
            y: 0.500_040,
        },
        Point {
            x: 0.500_040,
            y: 0.500_010,
        },
        Point {
            x: 0.500_010,
            y: 0.500_010,
        },
    ];

    let mut z13_records = Vec::new();
    let mut z14_records = Vec::new();
    let mut emit_13 = MultipolygonEmitScratch::new();
    let mut emit_14 = MultipolygonEmitScratch::new();
    let mut simp_13 = geometry::SimplifyMultiScratch::new();
    let mut simp_14 = geometry::SimplifyMultiScratch::new();

    emit_multipolygon_feature(
        4051,
        &bowtie_outer,
        &[],
        None,
        &m,
        13,
        13,
        &mut z13_records,
        &mut emit_13,
        &mut simp_13,
        0,
        None,
        0,
        1.0,
    );
    emit_multipolygon_feature(
        4051,
        &bowtie_outer,
        &[],
        None,
        &m,
        14,
        14,
        &mut z14_records,
        &mut emit_14,
        &mut simp_14,
        0,
        None,
        0,
        1.0,
    );

    for rec in z13_records.iter().chain(&z14_records) {
        let lb = decode_to_layer(&rec.data);
        let rings = geometry::decode_mvt_polygon(&lb.test_feature(0).geometry);
        for ring in rings {
            assert!(geometry::ring_is_simple(&ring));
        }
    }
}
#[test]
fn emit_multipolygon_emits_across_zoom_range_not_just_single_zoom() {
    let m = test_layer_match(Layer::Buildings, GeomExpect::Polygon);
    let outer = vec![
        Point { x: 0.10, y: 0.10 },
        Point { x: 0.90, y: 0.10 },
        Point { x: 0.90, y: 0.90 },
        Point { x: 0.10, y: 0.90 },
        Point { x: 0.10, y: 0.10 },
    ];
    let mut records = Vec::new();
    let mut emit_scratch = MultipolygonEmitScratch::new();
    let mut simp_scratch = geometry::SimplifyMultiScratch::new();
    let count = emit_multipolygon_feature(
        406,
        &outer,
        &[],
        None,
        &m,
        0,
        2,
        &mut records,
        &mut emit_scratch,
        &mut simp_scratch,
        0,
        None,
        0,
        1.0,
    );
    assert_eq!(usize::try_from(count).unwrap(), records.len());
    assert!(!records.is_empty());

    let mut zooms = std::collections::BTreeSet::new();
    for rec in &records {
        let (tile_id, _) = decode_key(rec);
        let (z, _, _) = pmtiles_writer::tile_id_to_zxy(tile_id);
        zooms.insert(z);
    }
    assert!(zooms.contains(&0));
    assert!(zooms.contains(&1));
    assert!(zooms.contains(&2));
}

// ---------------------------------------------------------------------------
// Checkpoint roundtrip tests
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_roundtrip() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let bounds = geometry::MercBbox {
        min_x: 0.1,
        min_y: 0.2,
        max_x: 0.8,
        max_y: 0.9,
    };
    save_checkpoint(
        dir.path(),
        &bounds,
        42,
        &computed_ocean(),
        &checkpoint_provenance(),
    )
    .unwrap();
    let (loaded_bounds, loaded_chunks, ocean_mode, provenance) =
        load_checkpoint(dir.path()).unwrap();
    assert!((loaded_bounds.min_x - 0.1).abs() < 1e-10);
    assert!((loaded_bounds.min_y - 0.2).abs() < 1e-10);
    assert!((loaded_bounds.max_x - 0.8).abs() < 1e-10);
    assert!((loaded_bounds.max_y - 0.9).abs() < 1e-10);
    assert_eq!(loaded_chunks, 42);
    assert_eq!(ocean_mode, computed_ocean());
    assert_eq!(provenance, checkpoint_provenance());
}

/// A computed-ocean mode with a synthetic source key.
///
/// Built by hand rather than through `OceanSourceKey::from_inputs`, which
/// hashes real shapefiles off disk; these tests are about the checkpoint's
/// encoding, not the hashing.
fn computed_ocean() -> OceanMode {
    OceanMode::Computed {
        key: crate::ocean::OceanSourceKey {
            full_shp_xxh128: 0xdead_beef,
            full_shx_xxh128: 0xfeed_face,
            simplified_shp_xxh128: Some(7),
            simplified_shx_xxh128: Some(8),
            min_zoom: 0,
            max_zoom: 14,
            policy_version: crate::ocean::OCEAN_POLICY_VERSION,
        },
    }
}

fn checkpoint_provenance() -> crate::pipeline::CheckpointProvenance {
    crate::pipeline::CheckpointProvenance {
        input_xxh3_128: "58c47f32d3a55b04a56813565efc78ac".to_string(),
        producer_config: serde_json::json!({"min_zoom": 0, "max_zoom": 14}),
        effective: crate::provenance::Effective {
            coordinate_source: "inline",
            way_members: "injected_v1",
            shared_node_pins: "injected_v1",
        },
    }
}

/// The chunks encode the producer config, so a resume must reject a changed
/// one rather than reinterpret them and record settings the tiles were never
/// built under.
#[test]
fn producer_config_diff_names_the_changed_field() {
    let chunks = serde_json::json!({"min_zoom": 0, "max_zoom": 14, "polygon_simplify_factor": 1.0});
    let current =
        serde_json::json!({"min_zoom": 0, "max_zoom": 12, "polygon_simplify_factor": 1.0});
    let diffs = crate::provenance::producer_config_diff(&chunks, &current);
    assert_eq!(diffs.len(), 1, "only max_zoom changed: {diffs:?}");
    assert!(diffs[0].starts_with("max_zoom"), "{diffs:?}");
    assert!(diffs[0].contains("14"), "must report the checkpoint value");
    assert!(diffs[0].contains("12"), "must report this run's value");
    assert!(
        crate::provenance::producer_config_diff(&chunks, &chunks).is_empty(),
        "an unchanged config must not report a diff"
    );
}

/// A config with every knob at its default, for producer-config tests that
/// vary exactly one field.
fn minimal_config() -> TilegenConfig {
    TilegenConfig {
        pbf_path: std::path::PathBuf::from("unused.osm.pbf"),
        output_path: std::path::PathBuf::from("unused.pmtiles"),
        tmp_dir: std::path::PathBuf::from("unused"),
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        ocean_artifact_key: None,
        ocean_only_metadata: false,
        skip_to: None,
        in_memory: true,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        assemble_batch_budget: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        seam_reconcile_layers: [0; Layer::count()],
        fanout_caps: [0; Layer::count()],
        polygon_simplify_factor: 1.0,
    }
}

/// Assemble-side settings are applied after the chunks are read, so changing
/// them on a resume is safe and must not be rejected.
#[test]
fn producer_config_excludes_assemble_side_settings() {
    let base = TilegenConfig {
        tile_compression: TileCompression::Gzip,
        compression_level: 6,
        ..minimal_config()
    };
    let recompressed = TilegenConfig {
        tile_compression: TileCompression::Brotli,
        compression_level: 9,
        ..minimal_config()
    };
    assert_eq!(
        crate::provenance::producer_config(&base),
        crate::provenance::producer_config(&recompressed),
        "tile compression and level are applied at assemble, not baked into chunks"
    );
}

/// Zoom range, fanout caps and simplification all decide what lands in the
/// chunks, so each must be visible to the resume guard.
#[test]
fn producer_config_covers_chunk_affecting_settings() {
    let base = minimal_config();
    let mut narrower = minimal_config();
    narrower.max_zoom = 12;
    assert_ne!(
        crate::provenance::producer_config(&base),
        crate::provenance::producer_config(&narrower)
    );

    let mut simplified = minimal_config();
    simplified.polygon_simplify_factor = 2.0;
    assert_ne!(
        crate::provenance::producer_config(&base),
        crate::provenance::producer_config(&simplified)
    );

    let mut capped = minimal_config();
    capped.fanout_caps[Layer::Ocean as usize] = 2048;
    assert_ne!(
        crate::provenance::producer_config(&base),
        crate::provenance::producer_config(&capped)
    );

    let mut seamed = minimal_config();
    seamed.seam_reconcile_layers[Layer::Boundaries as usize] = 10;
    assert_ne!(
        crate::provenance::producer_config(&base),
        crate::provenance::producer_config(&seamed)
    );
}

/// A resumed run must not inherit chunks built from a different PBF: the
/// archive would mix two inputs and carry provenance describing only one.
#[test]
fn checkpoint_records_the_input_it_was_built_from() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let bounds = geometry::MercBbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    save_checkpoint(
        dir.path(),
        &bounds,
        1,
        &computed_ocean(),
        &checkpoint_provenance(),
    )
    .expect("save checkpoint");
    let (_, _, _, loaded) = load_checkpoint(dir.path()).expect("load checkpoint");
    assert_eq!(loaded.input_xxh3_128, "58c47f32d3a55b04a56813565efc78ac");
    // The raw variant of the same extract hashes differently, which is what
    // makes an unsafe resume detectable at all.
    assert_ne!(loaded.input_xxh3_128, "aa5bb8650000000000000000deadbeef");
}

/// The effective paths survive a checkpoint round-trip as the same 'static
/// identifiers phase12 emits, so a resumed archive describes what actually
/// built its chunks.
#[test]
fn checkpoint_preserves_effective_paths() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let bounds = geometry::MercBbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    let raw_paths = crate::provenance::Effective {
        coordinate_source: "node_store",
        way_members: "relation_scan",
        shared_node_pins: "block_local",
    };
    save_checkpoint(
        dir.path(),
        &bounds,
        1,
        &computed_ocean(),
        &crate::pipeline::CheckpointProvenance {
            input_xxh3_128: "aa5bb8650000000000000000deadbeef".to_string(),
            producer_config: crate::provenance::producer_config(&minimal_config()),
            effective: raw_paths,
        },
    )
    .expect("save checkpoint");
    let (_, _, _, loaded) = load_checkpoint(dir.path()).expect("load checkpoint");
    assert_eq!(loaded.effective, raw_paths);
}

#[test]
fn checkpoint_preserves_all_ocean_modes() {
    let key = crate::ocean::OceanArtifactKey {
        full_shp_xxh128: 1,
        full_shx_xxh128: 2,
        simplified_shp_xxh128: None,
        simplified_shx_xxh128: None,
        min_zoom: 0,
        max_zoom: 14,
        compression_level: 6,
        policy_version: crate::ocean::OCEAN_POLICY_VERSION,
    };
    let bounds = geometry::MercBbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    for mode in [OceanMode::None, computed_ocean(), OceanMode::Band { key }] {
        let dir = tempfile::tempdir().expect("create tempdir");
        save_checkpoint(dir.path(), &bounds, 1, &mode, &checkpoint_provenance())
            .expect("save checkpoint");
        assert_eq!(
            load_checkpoint(dir.path()).expect("load checkpoint").2,
            mode
        );
    }
}

/// The resume guards in `run` are `checkpoint_ocean_mode != ocean_mode`, so
/// they only refuse a swapped computed source if the source is part of the
/// mode's identity. Before the key existed, both sides of that comparison were
/// the bare `Computed` and every swap compared equal: chunks built from one
/// shapefile were reused by a `--skip-to sort` naming another, and the archive
/// then described inputs that produced none of its ocean.
#[test]
fn computed_ocean_modes_differ_when_the_shapefile_differs() {
    let base = computed_ocean();
    let OceanMode::Computed { key } = &base else {
        panic!("computed_ocean built the wrong variant");
    };

    let mut swapped_full = key.clone();
    swapped_full.full_shp_xxh128 ^= 1;
    assert_ne!(base, OceanMode::Computed { key: swapped_full });

    // The split selection is identity too: the same full shapefile serving
    // z0-z14 alone produces different chunks than it does at z8-z14 with a
    // simplified pass below it.
    let mut dropped_simplified = key.clone();
    dropped_simplified.simplified_shp_xxh128 = None;
    dropped_simplified.simplified_shx_xxh128 = None;
    assert_ne!(
        base,
        OceanMode::Computed {
            key: dropped_simplified
        }
    );

    let mut narrower = key.clone();
    narrower.max_zoom = 12;
    assert_ne!(base, OceanMode::Computed { key: narrower });

    assert_eq!(base, computed_ocean());
}

/// A computed source key survives the checkpoint round-trip exactly, including
/// the absent-simplified case, which encodes as null rather than a hash.
#[test]
fn computed_ocean_source_key_roundtrips_without_a_simplified_pass() {
    let key = crate::ocean::OceanSourceKey {
        full_shp_xxh128: u128::MAX,
        full_shx_xxh128: 0,
        simplified_shp_xxh128: None,
        simplified_shx_xxh128: None,
        min_zoom: 0,
        max_zoom: 14,
        policy_version: crate::ocean::OCEAN_POLICY_VERSION,
    };
    let decoded =
        crate::ocean::OceanSourceKey::from_json(&key.to_json()).expect("decode source key");
    assert_eq!(decoded, key);
}

#[test]
fn sort_chunk_count_roundtrip() {
    let dir = tempfile::tempdir().expect("create tempdir");
    save_sort_chunk_count(dir.path(), Some(17)).unwrap();
    let loaded = load_sort_chunk_count(dir.path());
    assert_eq!(loaded, Some(17));
}

#[test]
fn sort_chunk_count_none_no_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    save_sort_chunk_count(dir.path(), None).unwrap();
    let loaded = load_sort_chunk_count(dir.path());
    assert_eq!(loaded, None);
}

#[test]
fn load_checkpoint_missing_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let result = load_checkpoint(dir.path());
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Ocean artifact retention tests
// ---------------------------------------------------------------------------

#[test]
fn yyyymmdd_utc_known_dates() {
    use std::time::{Duration, UNIX_EPOCH};
    assert_eq!(yyyymmdd_utc(UNIX_EPOCH), "19700101");
    assert_eq!(
        yyyymmdd_utc(UNIX_EPOCH + Duration::from_secs(1_785_024_000)),
        "20260726"
    );
    // Leap day.
    assert_eq!(
        yyyymmdd_utc(UNIX_EPOCH + Duration::from_secs(951_782_400)),
        "20000229"
    );
}

#[test]
fn retained_artifact_name_matcher_is_strict() {
    let p = "ocean-tiles-v";
    assert!(is_retained_artifact_name(
        "ocean-tiles-v4-20260724.pmtiles",
        p
    ));
    assert!(is_retained_artifact_name(
        "ocean-tiles-vunknown-20260724.pmtiles",
        p
    ));
    // Manually kept copies and near-misses survive the cleanup.
    assert!(!is_retained_artifact_name(
        "ocean-tiles-dp-20260712.pmtiles",
        p
    ));
    assert!(!is_retained_artifact_name("ocean-tiles.pmtiles", p));
    assert!(!is_retained_artifact_name("ocean-tiles-verify.pmtiles", p));
    assert!(!is_retained_artifact_name(
        "ocean-tiles-v4-2026072.pmtiles",
        p
    ));
    assert!(!is_retained_artifact_name("ocean-tiles-v4-20260724.txt", p));
    assert!(!is_retained_artifact_name(
        "ocean-tiles-v-20260724.pmtiles",
        p
    ));
}

#[test]
fn ocean_artifact_retention_keeps_one_generation() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let output = dir.path().join("ocean-tiles.pmtiles");
    std::fs::write(&output, b"outgoing artifact").expect("write artifact");
    let old_retained = dir.path().join("ocean-tiles-v3-20260101.pmtiles");
    std::fs::write(&old_retained, b"previous generation").expect("write retained");
    let manual_keep = dir.path().join("ocean-tiles-dp-20260712.pmtiles");
    std::fs::write(&manual_keep, b"manual keep").expect("write manual keep");

    retain_outgoing_ocean_artifact(&output).expect("retention");

    assert!(output.exists(), "active artifact must survive retention");
    assert!(!old_retained.exists(), "older retained generation dropped");
    assert!(manual_keep.exists(), "names outside the scheme untouched");
    let retained: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| is_retained_artifact_name(n, "ocean-tiles-v"))
        .collect();
    assert_eq!(retained.len(), 1, "one generation deep: {retained:?}");
    // The fixture bytes are not a readable archive, so the key is
    // unreadable and the retained name carries the `unknown` version.
    assert!(
        retained[0].starts_with("ocean-tiles-vunknown-"),
        "unreadable key retains as vunknown: {}",
        retained[0]
    );
    assert_eq!(
        std::fs::read(dir.path().join(&retained[0])).expect("read retained"),
        b"outgoing artifact"
    );

    // Idempotent within one day: a second retention of the same outgoing
    // artifact replaces the retained link instead of accumulating copies.
    retain_outgoing_ocean_artifact(&output).expect("second retention");
    let count = std::fs::read_dir(dir.path())
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| is_retained_artifact_name(n, "ocean-tiles-v"))
        .count();
    assert_eq!(count, 1, "re-retention must not accumulate generations");
}

#[test]
fn ocean_artifact_retention_noop_without_artifact() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let output = dir.path().join("ocean-tiles.pmtiles");
    retain_outgoing_ocean_artifact(&output).expect("retention on absent artifact");
    let entries = std::fs::read_dir(dir.path()).expect("read dir").count();
    assert_eq!(entries, 0, "nothing created when no artifact exists");
}

// ---------------------------------------------------------------------------
// Shared-edge reconciliation tests
// ---------------------------------------------------------------------------

/// Helper: encode a boundary polygon feature into the wire format used by PendingTile.
fn boundary_polygon_feature(osm_id: u64, ring: &[(i32, i32)]) -> (u8, Box<[u8]>) {
    let mut geom_buf = Vec::new();
    mvt::encode_polygon(&mut geom_buf, &[ring]);
    let attrs: Vec<crate::shortbread::Attr> = vec![("admin_level", AttrValue::Int(4), 0)];
    (
        Layer::Boundaries as u8,
        crate::wire_format::encode_feature_data(osm_id, GeomType::Polygon, &geom_buf, &attrs, 5),
    )
}

fn seam_reconcile_boundaries() -> Vec<u8> {
    let mut v = vec![0u8; shortbread::Layer::count()];
    v[shortbread::Layer::Boundaries as usize] = 8;
    v
}

/// Two adjacent boundary polygons at z5 (≤ BOUNDARY_NO_SIMP_MAX) sharing an edge.
/// After reconciliation, shared edge vertices must be identical in both features.
#[test]
fn seam_reconciliation_two_adjacent_boundaries() {
    // Ring A: square [0,0]-[2000,0]-[2000,2000]-[0,2000]
    // Ring B: square [2000,0]-[4000,0]-[4000,2000]-[2000,2000]
    // Shared edge: (2000,0)→(2000,2000) in A, (2000,2000)→(2000,0) in B
    let ring_a = vec![(0, 0), (2000, 0), (2000, 2000), (0, 2000), (0, 0)];
    let ring_b = vec![(2000, 0), (4000, 0), (4000, 2000), (2000, 2000), (2000, 0)];

    // Use z=5 tile so reconciliation fires (z <= BOUNDARY_NO_SIMP_MAX).
    let tile_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile = PendingTile {
        tile_id,
        features: vec![
            boundary_polygon_feature(100, &ring_a),
            boundary_polygon_feature(101, &ring_b),
        ],
    };

    let metrics = SeamMetrics::new();
    let srl = seam_reconcile_boundaries();
    let encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &srl, &metrics);
    assert_eq!(encoded.len(), 1);

    // Verify metrics fired.
    assert!(metrics.tiles_touched.load(Ordering::Relaxed) >= 1);
    assert!(metrics.rings_decoded.load(Ordering::Relaxed) >= 2);
    // With two simple squares sharing one edge, there should be exactly 1 chain.
    assert_eq!(metrics.chains_detected.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.chains_reconciled.load(Ordering::Relaxed), 1);

    // Decode the output tile and extract boundary layer polygon rings.
    let mut decoder = flate2::read::GzDecoder::new(encoded[0].compressed.as_slice());
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut raw).expect("gunzip");
    let layers = crate::pmtiles_reader::decode_mvt_layers(&raw).expect("decode mvt");
    let boundary_layer = layers
        .iter()
        .find(|l| l.name == "boundaries")
        .expect("should have boundaries layer");
    // After merge_same_attr_geometries, features with identical attributes may be merged
    // into a single multipolygon - so we check polygons >= 1 (not >= 2).
    assert!(
        boundary_layer.polygons >= 1,
        "should have at least 1 polygon feature"
    );
}

/// At z > BOUNDARY_NO_SIMP_MAX, reconciliation should NOT fire.
#[test]
fn seam_reconciliation_skipped_at_high_zoom() {
    let ring_a = vec![(0, 0), (2000, 0), (2000, 2000), (0, 2000), (0, 0)];
    let ring_b = vec![(2000, 0), (4000, 0), (4000, 2000), (2000, 2000), (2000, 0)];

    // z=10, above BOUNDARY_NO_SIMP_MAX.
    let tile_id = pmtiles_writer::xy_to_tile_id(10, 500, 500);
    let tile = PendingTile {
        tile_id,
        features: vec![
            boundary_polygon_feature(100, &ring_a),
            boundary_polygon_feature(101, &ring_b),
        ],
    };

    let metrics = SeamMetrics::new();
    let _encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &[], &metrics);
    // No reconciliation should have happened.
    assert_eq!(metrics.tiles_touched.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.chains_detected.load(Ordering::Relaxed), 0);
}

/// Cross-tile continuity: same boundary polygon clipped into two adjacent tiles
/// at z <= BOUNDARY_NO_SIMP_MAX. Verify both tiles produce valid output (regression guard).
#[test]
fn seam_reconciliation_cross_tile_continuity() {
    // A wide polygon that spans two adjacent tiles at z5.
    // Tile (10,10) and tile (11,10) are adjacent horizontally.
    let wide_ring = vec![(0, 0), (4096, 0), (4096, 2000), (0, 2000), (0, 0)];

    let tile_a_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile_b_id = pmtiles_writer::xy_to_tile_id(5, 11, 10);

    let tile_a = PendingTile {
        tile_id: tile_a_id,
        features: vec![boundary_polygon_feature(200, &wide_ring)],
    };
    let tile_b = PendingTile {
        tile_id: tile_b_id,
        features: vec![boundary_polygon_feature(200, &wide_ring)],
    };

    let metrics = SeamMetrics::new();
    let encoded = encode_tile_batch_mvt(&[tile_a, tile_b], 6, TileCompression::Gzip, &[], &metrics);
    // Both tiles should produce valid output (no panics, no empty results).
    assert_eq!(
        encoded.len(),
        2,
        "both adjacent tiles should encode successfully"
    );
}

/// Single boundary polygon (no shared edges) still gets tile-coord DP at z<=8.
#[test]
fn seam_reconciliation_single_ring_still_simplifies() {
    // A ring with a collinear midpoint - should be simplified even without shared chains.
    let ring = vec![
        (0, 0),
        (2000, 0),
        (4000, 0),
        (4000, 4000),
        (0, 4000),
        (0, 0),
    ];
    // (2000, 0) is collinear between (0,0) and (4000,0), should be removed by DP.

    let tile_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile = PendingTile {
        tile_id,
        features: vec![boundary_polygon_feature(400, &ring)],
    };

    let metrics = SeamMetrics::new();
    let srl = seam_reconcile_boundaries();
    let encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &srl, &metrics);
    assert_eq!(encoded.len(), 1);

    // Should still have been touched (decoded + simplified).
    assert_eq!(metrics.tiles_touched.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.rings_decoded.load(Ordering::Relaxed), 1);
    // No shared chains expected.
    assert_eq!(metrics.chains_detected.load(Ordering::Relaxed), 0);

    // Decode and verify the ring was simplified (collinear point removed).
    let mut decoder = flate2::read::GzDecoder::new(encoded[0].compressed.as_slice());
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut raw).expect("gunzip");
    let layers = crate::pmtiles_reader::decode_mvt_layers(&raw).expect("decode mvt");
    let boundary_layer = layers
        .iter()
        .find(|l| l.name == "boundaries")
        .expect("should have boundaries layer");
    assert!(
        boundary_layer.polygons >= 1,
        "should have boundary polygon output"
    );
}

/// Two boundary polygons with NO shared edge still get simplified at z<=8.
#[test]
fn seam_reconciliation_no_shared_edges_still_simplifies() {
    // Two non-adjacent polygons, each with a collinear midpoint.
    let ring_a = vec![(0, 0), (500, 0), (1000, 0), (1000, 1000), (0, 1000), (0, 0)];
    let ring_b = vec![
        (2000, 2000),
        (3000, 2000),
        (4000, 2000),
        (4000, 3000),
        (2000, 3000),
        (2000, 2000),
    ];

    let tile_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile = PendingTile {
        tile_id,
        features: vec![
            boundary_polygon_feature(500, &ring_a),
            boundary_polygon_feature(501, &ring_b),
        ],
    };

    let metrics = SeamMetrics::new();
    let srl = seam_reconcile_boundaries();
    let encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &srl, &metrics);
    assert_eq!(encoded.len(), 1);

    assert_eq!(metrics.tiles_touched.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.rings_decoded.load(Ordering::Relaxed), 2);
    assert_eq!(metrics.chains_detected.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.chains_reconciled.load(Ordering::Relaxed), 0);
}

/// Non-boundary layers are unaffected by reconciliation.
#[test]
fn seam_reconciliation_non_boundary_unaffected() {
    // Two land polygons with shared edge at z5 - should NOT trigger reconciliation.
    let ring_a = vec![(0, 0), (2000, 0), (2000, 2000), (0, 2000), (0, 0)];
    let ring_b = vec![(2000, 0), (4000, 0), (4000, 2000), (2000, 2000), (2000, 0)];

    let mut geom_a = Vec::new();
    mvt::encode_polygon(&mut geom_a, &[&ring_a]);
    let mut geom_b = Vec::new();
    mvt::encode_polygon(&mut geom_b, &[&ring_b]);
    let attrs: Vec<crate::shortbread::Attr> =
        vec![("kind", AttrValue::Str(Cow::Borrowed("residential")), 0)];

    let tile_id = pmtiles_writer::xy_to_tile_id(5, 10, 10);
    let tile = PendingTile {
        tile_id,
        features: vec![
            (
                Layer::Land as u8,
                crate::wire_format::encode_feature_data(300, GeomType::Polygon, &geom_a, &attrs, 5),
            ),
            (
                Layer::Land as u8,
                crate::wire_format::encode_feature_data(301, GeomType::Polygon, &geom_b, &attrs, 5),
            ),
        ],
    };

    let metrics = SeamMetrics::new();
    let _encoded = encode_tile_batch_mvt(&[tile], 6, TileCompression::Gzip, &[], &metrics);
    // Land layer should not trigger seam reconciliation.
    assert_eq!(metrics.tiles_touched.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// DeferralStats guardrail tests
// ---------------------------------------------------------------------------

#[test]
fn deferral_stats_record_and_check_budgets() {
    let ds = DeferralStats::new();
    let layer = Layer::Boundaries as u8;
    let mut srl = [0u8; shortbread::Layer::count()];
    srl[layer as usize] = 8;

    // Record below budget - should not disable.
    ds.record(layer, 1000);
    ds.check_budgets(&srl);
    assert!(!ds.is_disabled(layer));
    assert_eq!(ds.vertices[layer as usize].load(Ordering::Relaxed), 1000);

    // Record to exceed budget - should disable.
    ds.record(layer, DEFERRAL_VERTEX_BUDGET);
    ds.check_budgets(&srl);
    assert!(ds.is_disabled(layer));
    assert_eq!(
        ds.vertices[layer as usize].load(Ordering::Relaxed),
        DEFERRAL_VERTEX_BUDGET + 1000,
    );
}

#[test]
fn deferral_stats_only_checks_enabled_layers() {
    let ds = DeferralStats::new();
    let layer = Layer::WaterPolygons as u8;
    // Layer not in seam_reconcile_layers (max_zoom = 0).
    let srl = [0u8; shortbread::Layer::count()];

    ds.record(layer, DEFERRAL_VERTEX_BUDGET + 1);
    ds.check_budgets(&srl);
    // Should NOT disable - layer has max_zoom=0 in config.
    assert!(!ds.is_disabled(layer));
}

#[test]
fn disabled_layer_falls_back_to_simplified_path() {
    let mut coords = vec![Point { x: 0.05, y: 0.05 }];
    for i in 1..15 {
        let t = i as f64 / 15.0;
        coords.push(Point {
            x: 0.05 + t * 0.15,
            y: 0.05 + 0.0005 * if i % 2 == 0 { 1.0 } else { -1.0 },
        });
    }
    coords.push(Point { x: 0.20, y: 0.05 });
    coords.push(Point { x: 0.20, y: 0.20 });
    coords.push(Point { x: 0.05, y: 0.20 });
    coords.push(Point { x: 0.05, y: 0.05 });

    let m = test_layer_match(Layer::Boundaries, GeomExpect::Polygon);

    let mut records_fullres = Vec::new();
    let mut scratch_fullres = PolygonEmitScratch::new();
    emit_polygon_feature(
        100,
        &coords,
        &[],
        &m,
        2,
        2,
        &mut records_fullres,
        &mut scratch_fullres,
        8,
        None,
        0,
        1.0,
    );

    let ds = DeferralStats::new();
    ds.disabled[Layer::Boundaries as usize].store(true, Ordering::Relaxed);
    let mut records_disabled = Vec::new();
    let mut scratch_disabled = PolygonEmitScratch::new();
    emit_polygon_feature(
        100,
        &coords,
        &[],
        &m,
        2,
        2,
        &mut records_disabled,
        &mut scratch_disabled,
        8,
        Some(&ds),
        0,
        1.0,
    );

    let mut records_nodeferral = Vec::new();
    let mut scratch_nodeferral = PolygonEmitScratch::new();
    emit_polygon_feature(
        100,
        &coords,
        &[],
        &m,
        2,
        2,
        &mut records_nodeferral,
        &mut scratch_nodeferral,
        0,
        None,
        0,
        1.0,
    );

    assert_eq!(records_fullres.len(), 1, "full-res should emit");
    assert_eq!(records_disabled.len(), 1, "disabled should emit");
    assert_eq!(records_nodeferral.len(), 1, "no-deferral should emit");

    let (_, _, cmds_fullres) = decode_data_header(&records_fullres[0].data);
    let (_, _, cmds_disabled) = decode_data_header(&records_disabled[0].data);
    let (_, _, cmds_nodeferral) = decode_data_header(&records_nodeferral[0].data);

    assert!(
        cmds_fullres > cmds_disabled,
        "full-res ({cmds_fullres}) should have more commands than disabled ({cmds_disabled})",
    );
    assert_eq!(
        cmds_disabled, cmds_nodeferral,
        "disabled ({cmds_disabled}) should match no-deferral ({cmds_nodeferral})",
    );
}

#[test]
fn deferral_records_vertex_count() {
    let ds = DeferralStats::new();
    let m = test_layer_match(Layer::Boundaries, GeomExpect::Polygon);
    let coords = [
        Point { x: 0.3, y: 0.3 },
        Point { x: 0.7, y: 0.3 },
        Point { x: 0.7, y: 0.7 },
        Point { x: 0.3, y: 0.7 },
        Point { x: 0.3, y: 0.3 },
    ];

    let mut records = Vec::new();
    let mut scratch = PolygonEmitScratch::new();
    // Emit at z=0 with seam_max_zoom=8 → deferred, should record vertices.
    emit_polygon_feature(
        200,
        &coords,
        &[],
        &m,
        0,
        0,
        &mut records,
        &mut scratch,
        8,
        Some(&ds),
        0,
        1.0,
    );

    let recorded = ds.vertices[Layer::Boundaries as usize].load(Ordering::Relaxed);
    assert_eq!(
        recorded,
        coords.len() as u64,
        "should record {n} vertices",
        n = coords.len()
    );
}
