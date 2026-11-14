#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use elivagar::archive::{Archive, Compression};

struct OceanGolden {
    policy_version: u32,
    archive_xxh3_128: &'static str,
}

// The digest covers the decompressed tile payloads in tile-id order (not the
// archive file, whose metadata varies by build). It is kept beside the policy
// version so a reviewed ocean output change updates the pair in one place.
// The 50 m fixture is smaller than a buffered tile at every built zoom (0-14),
// so it never emits a full fill. Changing the reserved full-fill ID therefore
// leaves this digest unchanged.
const OCEAN_GOLDEN: OceanGolden = OceanGolden {
    policy_version: 8,
    archive_xxh3_128: "8170f01181cbd2b70241c8a7cb41cb90",
};

#[test]
fn synthetic_ocean_archive_matches_policy_golden() {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let shp_path = dir.path().join("synthetic_ocean.shp");
    let archive_path = dir.path().join("synthetic_ocean.pmtiles");
    let scratch_path = dir.path().join("scratch");

    write_synthetic_ocean_shapefile(&shp_path);
    elivagar::ocean_build(
        &shp_path,
        None,
        &archive_path,
        &scratch_path,
        6,
        std::num::NonZeroUsize::MIN,
    )
    .expect("build synthetic ocean archive");

    let archive = Archive::open(&archive_path).expect("open synthetic ocean archive");
    let metadata: serde_json::Value = serde_json::from_str(
        &archive
            .metadata()
            .expect("read synthetic ocean archive metadata"),
    )
    .expect("parse synthetic ocean archive metadata");
    let policy_version = metadata
        .get("ocean_artifact")
        .and_then(|artifact| artifact.get("policy_version"))
        .and_then(serde_json::Value::as_u64)
        .expect("ocean archive metadata includes its policy version");
    // Hash only what ocean geometry and encoding determine: per run, the
    // tile id, run length and the DECOMPRESSED tile payload, in tile-id
    // order. The whole file is deliberately not hashed: its metadata carries
    // build provenance (commit, versions, timestamps, paths), and the gzip
    // bytes depend on the compressor implementation.
    assert_eq!(
        archive.header().tile_compression,
        Compression::Gzip,
        "synthetic archive is gzip"
    );
    let mut folded = Vec::new();
    for run in archive
        .read_all_runs()
        .expect("read archive runs")
        .as_slice()
    {
        let tile = archive
            .read_tile(run.blob())
            .expect("decompress tile payload");
        folded.extend_from_slice(&run.start().raw().to_le_bytes());
        folded.extend_from_slice(&run.run_length().get().to_le_bytes());
        folded.extend_from_slice(
            &u64::try_from(tile.len())
                .expect("len fits u64")
                .to_le_bytes(),
        );
        folded.extend_from_slice(&tile);
    }
    assert!(!folded.is_empty(), "synthetic archive has tiles");
    let archive_xxh3_128 = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&folded));
    let actual = (policy_version, archive_xxh3_128.as_str());
    let expected = (
        u64::from(OCEAN_GOLDEN.policy_version),
        OCEAN_GOLDEN.archive_xxh3_128,
    );
    assert_eq!(
        actual, expected,
        "synthetic ocean output or policy version changed (observed {actual:?}); bump OCEAN_POLICY_VERSION in src/ocean/policy.rs when output behavior changed, then update OCEAN_GOLDEN",
    );
}

fn write_synthetic_ocean_shapefile(path: &Path) {
    const HEADER_BYTES: usize = 100;
    const RECORD_HEADER_BYTES: usize = 8;
    const NUM_PARTS: usize = 1;
    // A 50 meter square centered inside one z14 tile.
    const RING: [(f64, f64); 5] = [
        (1198.5, -1250.5),
        (1248.5, -1250.5),
        (1248.5, -1200.5),
        (1198.5, -1200.5),
        (1198.5, -1250.5),
    ];

    let num_points = RING.len();
    let content_bytes = 4 + 32 + 4 + 4 + NUM_PARTS * 4 + num_points * 16;
    let content_words = u32::try_from(content_bytes / 2).expect("record size fits u32");
    let shp_bytes = HEADER_BYTES + RECORD_HEADER_BYTES + content_bytes;
    let shp_words = u32::try_from(shp_bytes / 2).expect("shapefile size fits u32");
    let shx_bytes = HEADER_BYTES + 8;
    let shx_words = u32::try_from(shx_bytes / 2).expect("shapefile index size fits u32");
    let bounds = (1198.5, -1250.5, 1248.5, -1200.5);

    let mut shp = vec![0_u8; shp_bytes];
    let mut shx = vec![0_u8; shx_bytes];
    write_shapefile_header(&mut shp, shp_words, bounds);
    write_shapefile_header(&mut shx, shx_words, bounds);

    write_u32_be(&mut shx, HEADER_BYTES, 50);
    write_u32_be(&mut shx, HEADER_BYTES + 4, content_words);

    write_u32_be(&mut shp, HEADER_BYTES, 1);
    write_u32_be(&mut shp, HEADER_BYTES + 4, content_words);

    let record = HEADER_BYTES + RECORD_HEADER_BYTES;
    write_u32_le(&mut shp, record, 5);
    write_bounds(&mut shp, record + 4, bounds);
    write_u32_le(
        &mut shp,
        record + 36,
        u32::try_from(NUM_PARTS).expect("part count fits u32"),
    );
    write_u32_le(
        &mut shp,
        record + 40,
        u32::try_from(num_points).expect("point count fits u32"),
    );
    write_u32_le(&mut shp, record + 44, 0);

    let mut point_offset = record + 48;
    for (x, y) in RING {
        write_f64_le(&mut shp, point_offset, x);
        write_f64_le(&mut shp, point_offset + 8, y);
        point_offset += 16;
    }

    fs::write(path, shp).expect("write synthetic ocean shapefile");
    fs::write(path.with_extension("shx"), shx).expect("write synthetic ocean shapefile index");
}

fn write_shapefile_header(bytes: &mut [u8], length_words: u32, bounds: (f64, f64, f64, f64)) {
    write_u32_be(bytes, 0, 9994);
    write_u32_be(bytes, 24, length_words);
    write_u32_le(bytes, 28, 1000);
    write_u32_le(bytes, 32, 5);
    write_bounds(bytes, 36, bounds);
}

fn write_bounds(bytes: &mut [u8], offset: usize, bounds: (f64, f64, f64, f64)) {
    write_f64_le(bytes, offset, bounds.0);
    write_f64_le(bytes, offset + 8, bounds.1);
    write_f64_le(bytes, offset + 16, bounds.2);
    write_f64_le(bytes, offset + 24, bounds.3);
}

fn write_u32_be(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn write_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_f64_le(bytes: &mut [u8], offset: usize, value: f64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
