#![allow(clippy::unwrap_used)]

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

use pbfhogg::MemberId;
use pbfhogg::block_builder::{self, BlockBuilder, MemberData, Metadata};
use pbfhogg::writer::{Compression as PbfCompression, PbfWriter};

/// A v6 checkpoint that a default-flag `--skip-to assemble` run will accept.
///
/// The producer config must equal what the CLI resolves, so it is built from
/// a `TilegenConfig` carrying the documented CLI defaults - zooms 0-14,
/// `--seam-reconcile-layers boundaries` (default max zoom 8), no fanout caps,
/// `--polygon-simplify-factor 1.0` - and run through the same
/// `producer_config` the pipeline uses, rather than hand-encoded JSON that
/// would drift out of agreement silently.
///
/// The version is the one hand-encoded field. Bumping CHECKPOINT_VERSION
/// without updating it fails this test loudly, which is the intent: the
/// version is what a resume checks first, so a test asserting resume
/// behaviour must state which format it is asserting about.
fn checkpoint_json(pbf_hash: &str) -> serde_json::Value {
    let config = elivagar::TilegenConfig {
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
        in_memory: false,
        compression_level: 6,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads: 1,
        way_inflight_budget: 0,
        assemble_batch_budget: 0,
        dedup_cap: 0,
        sort_chunk_size: 0,
        locations_on_ways: false,
        tile_format: elivagar::TilePayloadFormat::Mvt,
        tile_compression: elivagar::TileCompression::Gzip,
        compress_sort_chunks: elivagar::sort::ChunkCompression::None,
        fanout_caps: [0; elivagar::shortbread::Layer::count()],
        polygon_simplify_factor: 1.0,
    };
    serde_json::json!({
        "version": 6,
        "bounds": [0.0, 0.0, 1.0, 1.0],
        "chunks": 1,
        "ocean": "none",
        "input_xxh3_128": pbf_hash,
        "producer_config": elivagar::provenance::producer_config(&config),
        "effective": {
            "coordinate_source": "node_store",
            "way_members": "relation_scan",
            "shared_node_pins": "block_local",
        },
    })
}

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
        [("amenity", "cafe"), ("name", "Test Cafe")],
        Some(&meta),
    );
    if let Some(bytes) = bb.take().expect("take block") {
        writer
            .write_primitive_block(bytes)
            .expect("write data block");
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
        [("amenity", "cafe"), ("name", "Test Cafe")],
        Some(&meta),
    );
    if let Some(bytes) = bb.take().expect("take node block") {
        writer
            .write_primitive_block(bytes)
            .expect("write node block");
    }

    // Block 2: way with one missing node reference.
    bb.add_way(
        10,
        [("highway", "residential"), ("name", "Broken Way")],
        &[1, 999],
        Some(&meta),
    );
    if let Some(bytes) = bb.take().expect("take way block") {
        writer
            .write_primitive_block(bytes)
            .expect("write way block");
    }

    // Block 3: multipolygon relation with missing way ref + non-way + nested members.
    let members = [
        MemberData {
            id: MemberId::Way(555),
            role: "outer",
        },
        MemberData {
            id: MemberId::Node(1),
            role: "label",
        },
        MemberData {
            id: MemberId::Relation(777),
            role: "sub",
        },
    ];
    bb.add_relation(
        20,
        [
            ("type", "multipolygon"),
            ("building", "yes"),
            ("name", "Broken Relation"),
        ],
        &members,
        Some(&meta),
    );
    if let Some(bytes) = bb.take().expect("take relation block") {
        writer
            .write_primitive_block(bytes)
            .expect("write relation block");
    }

    writer.flush().expect("flush pbf");
}

fn run_elivagar(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_elivagar"))
        .args(args)
        .output()
        .expect("run elivagar")
}

fn run_elivagar_with_fifo(args: &[&str], fifo_path: &Path) -> (std::process::Output, String) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    make_fifo(fifo_path);

    // Drain the FIFO on a background thread while the child runs, exactly as
    // brokkr's real sidecar does. Reading only after the child exits would
    // silently lose markers once cumulative output exceeds the ~64 KiB pipe
    // buffer: the child writes O_NONBLOCK and discards on EAGAIN, so an
    // undrained pipe drops data instead of blocking. A bare run is well under
    // the buffer, but ELIVAGAR_LAYER_STATS or a larger fixture pushes counter
    // volume past it, so draining concurrently (not after exit) is the robust
    // pattern - and it mirrors what brokkr's real sidecar does.
    let mut reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(fifo_path)
        .expect("open fifo reader");
    // Hold a writer open ourselves for the lifetime of the run. Without a live
    // writer, a non-blocking read on an empty FIFO returns Ok(0) (EOF), which
    // the drain loop would hit in the window before the child opens its own
    // write end and exit immediately. With this writer present that window
    // reads as WouldBlock instead, so the loop keeps polling until the child
    // has produced its output.
    let keepalive = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(fifo_path)
        .expect("open fifo keepalive writer");

    let stop = Arc::new(AtomicBool::new(false));
    let drain_stop = Arc::clone(&stop);
    let drainer = std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break, // all writers closed
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Only stop on an empty pipe: any pending bytes are read
                    // (Ok(n)) before a WouldBlock is ever observed, so setting
                    // `stop` after the child exits cannot truncate its output.
                    if drain_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => panic!("fifo read: {e}"),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let output = Command::new(env!("CARGO_BIN_EXE_elivagar"))
        .args(args)
        .env("BROKKR_MARKER_FIFO", fifo_path)
        .output()
        .expect("run elivagar");

    stop.store(true, Ordering::Relaxed);
    let fifo = drainer.join().expect("drain thread panicked");
    drop(keepalive);
    (output, fifo)
}

fn make_fifo(path: &Path) {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("fifo path");
    // SAFETY: mkfifo reads the nul-terminated path and does not retain it.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(
        rc,
        0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

fn counter_value(fifo: &str, key: &str) -> Option<String> {
    let prefix = format!("@{key}=");
    for line in fifo.lines() {
        for field in line.split_whitespace().skip(1) {
            if let Some(rest) = field.strip_prefix(&prefix) {
                return Some(rest.to_string());
            }
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

    let first_fifo_path = dir.path().join("first.fifo");
    let (first, first_fifo) = run_elivagar_with_fifo(
        &[
            "run",
            &pbf_s,
            "--output",
            &out_s,
            "--tmp-dir",
            &tmp_s,
            "--threads",
            "1",
        ],
        &first_fifo_path,
    );
    assert!(
        first.status.success(),
        "first run failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_err = String::from_utf8_lossy(&first.stderr);
    assert!(!first_err.contains("phase3_ms="));
    assert!(!first_err.contains("tile_bytes_total="));
    assert!(!first_err.contains("tile_max_bytes="));
    assert!(!first_err.contains("oversize_top_1="));
    assert!(counter_value(&first_fifo, "phase3_ms").is_some());
    assert!(counter_value(&first_fifo, "tile_bytes_total").is_some());
    assert!(counter_value(&first_fifo, "tile_max_bytes").is_some());
    assert!(counter_value(&first_fifo, "oversize_top_1_bytes").is_some());

    let second_fifo_path = dir.path().join("second.fifo");
    let (second, second_fifo) = run_elivagar_with_fifo(
        &[
            "run",
            &pbf_s,
            "--output",
            &out_s,
            "--tmp-dir",
            &tmp_s,
            "--threads",
            "1",
            "--skip-to",
            "assemble",
        ],
        &second_fifo_path,
    );
    assert!(
        second.status.success(),
        "skip-to assemble failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_err = String::from_utf8_lossy(&second.stderr);
    assert!(second_err.contains("--- Skipping to assemble (using existing chunks) ---"));
    assert!(!second_err.contains("phase3_ms="));
    assert!(!second_err.contains("tile_bytes_total="));
    assert!(!second_err.contains("tile_max_bytes="));
    assert!(!second_err.contains("oversize_top_1="));
    assert!(counter_value(&second_fifo, "phase3_ms").is_none());
    assert!(counter_value(&second_fifo, "tile_bytes_total").is_some());
    assert!(counter_value(&second_fifo, "tile_max_bytes").is_some());
    assert!(counter_value(&second_fifo, "oversize_top_1_bytes").is_some());
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

    // A coherent checkpoint, so every resume guard passes and the flow reaches
    // the chunk-state validation this test is about: the ocean-mode guard (the
    // run names no --ocean, matching "none"), the input-identity guard (needs
    // this very PBF's hash), and the producer-config guard (must match what
    // the CLI resolves for the flags below).
    //
    // The producer config is generated rather than hand-encoded so it tracks
    // the CLI defaults instead of silently rotting when one of them changes.
    let (pbf_hash, _) = elivagar::provenance::hash_file(&pbf_path).expect("hash pbf");
    std::fs::write(
        tmp_dir.join("checkpoint.txt"),
        checkpoint_json(&pbf_hash).to_string(),
    )
    .expect("write checkpoint");
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

    let first_fifo_path = dir.path().join("missing-first.fifo");
    let (first, first_fifo) = run_elivagar_with_fifo(
        &[
            "run",
            &pbf_s,
            "--output",
            &out_s,
            "--tmp-dir",
            &tmp_s,
            "--threads",
            "1",
        ],
        &first_fifo_path,
    );
    assert!(
        first.status.success(),
        "first run failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_err = String::from_utf8_lossy(&first.stderr);
    let missing_ref_counters = [
        ("missing_way_node_refs", "1"),
        ("ways_with_missing_node_refs", "1"),
        ("missing_relation_way_refs", "1"),
        ("relations_with_missing_way_refs", "1"),
        ("relation_non_way_members", "2"),
        ("relation_nested_members", "1"),
    ];
    for &(key, expected) in &missing_ref_counters {
        assert!(!first_err.contains(&format!("{key}=")));
        assert_eq!(
            counter_value(&first_fifo, key),
            Some(expected.to_string()),
            "{key}"
        );
    }

    let second_fifo_path = dir.path().join("missing-second.fifo");
    let (second, second_fifo) = run_elivagar_with_fifo(
        &[
            "run",
            &pbf_s,
            "--output",
            &out_s,
            "--tmp-dir",
            &tmp_s,
            "--threads",
            "1",
            "--skip-to",
            "sort",
        ],
        &second_fifo_path,
    );
    assert!(
        second.status.success(),
        "skip-to sort run failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_err = String::from_utf8_lossy(&second.stderr);
    assert!(second_err.contains("--- Skipping to sort (using existing chunks) ---"));
    for &(key, _) in &missing_ref_counters {
        assert!(!second_err.contains(&format!("{key}=")));
        assert!(counter_value(&second_fifo, key).is_none(), "{key}");
    }
}
