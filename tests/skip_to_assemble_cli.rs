#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData, Metadata};
use pbfhogg::MemberId;
use pbfhogg::writer::{Compression as PbfCompression, PbfWriter};

fn write_tiny_pbf(path: &Path) {
    let file = std::fs::File::create(path).expect("create pbf");
    let mut writer = PbfWriter::new(file, PbfCompression::default());
    let header = block_builder::HeaderBuilder::new()
        .bbox(10.0, 59.0, 10.1, 59.1)
        .build()
        .expect("build pbf header");
    writer.write_header(&header).expect("write pbf header");

    let mut bb = BlockBuilder::new();
    let meta = Metadata {
        version: 1,
        timestamp: 1_700_000_123,
        changeset: 7,
        uid: 1,
        user: "test",
        visible: true,
    };
    // A simple POI-like node to guarantee at least one emitted feature/tile.
    bb.add_node(
        1,
        590_500_000,
        100_500_000,
        &[("amenity", "cafe"), ("name", "Test Cafe")],
        Some(&meta),
    );
    if let Some(bytes) = bb.take().expect("take block") {
        writer.write_primitive_block(bytes).expect("write data block");
    }
    writer.flush().expect("flush pbf");
}

fn write_missing_ref_pbf(path: &Path) {
    let file = std::fs::File::create(path).expect("create pbf");
    let mut writer = PbfWriter::new(file, PbfCompression::default());
    let header = block_builder::HeaderBuilder::new()
        .bbox(10.0, 59.0, 10.1, 59.1)
        .build()
        .expect("build pbf header");
    writer.write_header(&header).expect("write pbf header");

    let meta = Metadata {
        version: 1,
        timestamp: 1_700_000_123,
        changeset: 7,
        uid: 1,
        user: "test",
        visible: true,
    };

    // Block 1: one valid node (also guarantees at least one emitted feature).
    let mut bb = BlockBuilder::new();
    bb.add_node(
        1,
        590_500_000,
        100_500_000,
        &[("amenity", "cafe"), ("name", "Test Cafe")],
        Some(&meta),
    );
    if let Some(bytes) = bb.take().expect("take node block") {
        writer.write_primitive_block(bytes).expect("write node block");
    }

    // Block 2: way with one missing node reference.
    bb.add_way(
        10,
        &[("highway", "residential"), ("name", "Broken Way")],
        &[1, 999],
        Some(&meta),
    );
    if let Some(bytes) = bb.take().expect("take way block") {
        writer.write_primitive_block(bytes).expect("write way block");
    }

    // Block 3: multipolygon relation with missing way ref + non-way + nested members.
    let members = [
        MemberData { id: MemberId::Way(555), role: "outer" },
        MemberData { id: MemberId::Node(1), role: "label" },
        MemberData { id: MemberId::Relation(777), role: "sub" },
    ];
    bb.add_relation(
        20,
        &[("type", "multipolygon"), ("building", "yes"), ("name", "Broken Relation")],
        &members,
        Some(&meta),
    );
    if let Some(bytes) = bb.take().expect("take relation block") {
        writer.write_primitive_block(bytes).expect("write relation block");
    }

    writer.flush().expect("flush pbf");
}

fn run_elivagar(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_elivagar"))
        .args(args)
        .output()
        .expect("run elivagar")
}

fn metric_value(stderr: &str, key: &str) -> Option<String> {
    for line in stderr.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            return Some(rest.to_string());
        }
    }
    None
}

#[test]
fn skip_to_assemble_reuses_chunks_and_omits_phase3_metrics() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let pbf_path = dir.path().join("input.osm.pbf");
    let output_path = dir.path().join("out.pmtiles");
    let tmp_dir = dir.path().join("tmp");
    write_tiny_pbf(&pbf_path);

    let pbf_s = pbf_path.to_string_lossy().into_owned();
    let out_s = output_path.to_string_lossy().into_owned();
    let tmp_s = tmp_dir.to_string_lossy().into_owned();

    let first = run_elivagar(&[
        "run",
        &pbf_s,
        "--output",
        &out_s,
        "--tmp-dir",
        &tmp_s,
        "--no-ocean",
        "--threads",
        "1",
    ]);
    assert!(first.status.success(), "first run failed: {}", String::from_utf8_lossy(&first.stderr));
    let first_err = String::from_utf8_lossy(&first.stderr);
    assert!(first_err.contains("phase3_ms="));
    assert!(first_err.contains("tile_bytes_total="));
    assert!(first_err.contains("tile_max_bytes="));
    assert!(first_err.contains("oversize_top_1="));

    let second = run_elivagar(&[
        "run",
        &pbf_s,
        "--output",
        &out_s,
        "--tmp-dir",
        &tmp_s,
        "--no-ocean",
        "--threads",
        "1",
        "--skip-to",
        "assemble",
    ]);
    assert!(second.status.success(), "skip-to assemble failed: {}", String::from_utf8_lossy(&second.stderr));
    let second_err = String::from_utf8_lossy(&second.stderr);
    assert!(second_err.contains("--- Skipping to assemble (using existing chunks) ---"));
    assert!(!second_err.contains("phase3_ms="));
    assert!(second_err.contains("tile_bytes_total="));
    assert!(second_err.contains("tile_max_bytes="));
    assert!(second_err.contains("oversize_top_1="));
}

#[test]
fn skip_to_assemble_fails_on_missing_or_stale_chunk_state() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let pbf_path = dir.path().join("input.osm.pbf");
    let output_path = dir.path().join("out.pmtiles");
    let tmp_dir = dir.path().join("tmp");
    let chunks_dir = tmp_dir.join("sort_chunks");
    std::fs::create_dir_all(&chunks_dir).expect("create chunks dir");
    write_tiny_pbf(&pbf_path);

    // Missing chunk path: checkpoint expects 1 chunk but there are none.
    std::fs::write(tmp_dir.join("sort_chunks.count"), "1").expect("write chunk checkpoint");

    let pbf_s = pbf_path.to_string_lossy().into_owned();
    let out_s = output_path.to_string_lossy().into_owned();
    let tmp_s = tmp_dir.to_string_lossy().into_owned();
    let missing = run_elivagar(&[
        "run",
        &pbf_s,
        "--output",
        &out_s,
        "--tmp-dir",
        &tmp_s,
        "--no-ocean",
        "--threads",
        "1",
        "--skip-to",
        "assemble",
    ]);
    assert!(!missing.status.success(), "missing-state run should fail");
    let missing_err = String::from_utf8_lossy(&missing.stderr);
    assert!(missing_err.contains("chunk count mismatch"));

    // Stale chunk path: checkpoint expects 1 chunk but 2 contiguous chunks exist.
    std::fs::write(chunks_dir.join("chunk_0000.bin"), [0u8; 1]).expect("write stale chunk 0");
    std::fs::write(chunks_dir.join("chunk_0001.bin"), [0u8; 1]).expect("write stale chunk 1");
    std::fs::write(tmp_dir.join("sort_chunks.count"), "1").expect("rewrite chunk checkpoint");

    let stale = run_elivagar(&[
        "run",
        &pbf_s,
        "--output",
        &out_s,
        "--tmp-dir",
        &tmp_s,
        "--no-ocean",
        "--threads",
        "1",
        "--skip-to",
        "assemble",
    ]);
    assert!(!stale.status.success(), "stale-state run should fail");
    let stale_err = String::from_utf8_lossy(&stale.stderr);
    assert!(stale_err.contains("chunk count mismatch"));
}

#[test]
fn missing_ref_metrics_emitted_in_full_run_and_omitted_on_skip_to_sort() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let pbf_path = dir.path().join("missing_refs.osm.pbf");
    let output_path = dir.path().join("out.pmtiles");
    let tmp_dir = dir.path().join("tmp");
    write_missing_ref_pbf(&pbf_path);

    let pbf_s = pbf_path.to_string_lossy().into_owned();
    let out_s = output_path.to_string_lossy().into_owned();
    let tmp_s = tmp_dir.to_string_lossy().into_owned();

    let first = run_elivagar(&[
        "run",
        &pbf_s,
        "--output",
        &out_s,
        "--tmp-dir",
        &tmp_s,
        "--no-ocean",
        "--threads",
        "1",
    ]);
    assert!(first.status.success(), "first run failed: {}", String::from_utf8_lossy(&first.stderr));
    let first_err = String::from_utf8_lossy(&first.stderr);
    assert_eq!(metric_value(&first_err, "missing_way_node_refs="), Some("1".to_string()));
    assert_eq!(metric_value(&first_err, "ways_with_missing_node_refs="), Some("1".to_string()));
    assert_eq!(metric_value(&first_err, "missing_relation_way_refs="), Some("1".to_string()));
    assert_eq!(metric_value(&first_err, "relations_with_missing_way_refs="), Some("1".to_string()));
    assert_eq!(metric_value(&first_err, "relation_non_way_members="), Some("2".to_string()));
    assert_eq!(metric_value(&first_err, "relation_nested_members="), Some("1".to_string()));

    let second = run_elivagar(&[
        "run",
        &pbf_s,
        "--output",
        &out_s,
        "--tmp-dir",
        &tmp_s,
        "--no-ocean",
        "--threads",
        "1",
        "--skip-to",
        "sort",
    ]);
    assert!(second.status.success(), "skip-to sort run failed: {}", String::from_utf8_lossy(&second.stderr));
    let second_err = String::from_utf8_lossy(&second.stderr);
    assert!(second_err.contains("--- Skipping to sort (using existing chunks) ---"));
    assert!(metric_value(&second_err, "missing_way_node_refs=").is_none());
    assert!(metric_value(&second_err, "ways_with_missing_node_refs=").is_none());
    assert!(metric_value(&second_err, "missing_relation_way_refs=").is_none());
    assert!(metric_value(&second_err, "relations_with_missing_way_refs=").is_none());
    assert!(metric_value(&second_err, "relation_non_way_members=").is_none());
    assert!(metric_value(&second_err, "relation_nested_members=").is_none());
}
