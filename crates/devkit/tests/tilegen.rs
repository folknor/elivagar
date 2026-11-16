//! The tilegen block, PBF resolution and the argv they expand to.

mod common;

use std::path::{Path, PathBuf};

use common::{scratch, write};
use elivagar_devkit::DevError;
use elivagar_devkit::config::{Paths, load_from};
use elivagar_devkit::tilegen::{
    DEFAULT_TILEGEN, OceanSpec, ResolvedPbf, TilegenConfig, file_digest, ocean_build_args,
    resolve_pbf, resolve_tilegen, run_args,
};

/// A project at `name` whose host `box` carries `host_toml` (appended after
/// `[box]`), with a data dir holding `files`.
fn project(name: &str, host_toml: &str, files: &[&str]) -> Paths {
    let root = scratch(name);
    write(&root.join("brokkr.toml"), &format!("[box]\n{host_toml}"));
    for file in files {
        write(&root.join("data").join(file), "x");
    }
    load_from(&root.join("brokkr.toml"), Some("box")).expect("load")
}

fn message<T: std::fmt::Debug>(result: Result<T, DevError>) -> String {
    match result {
        Err(DevError::Config(message)) => message,
        Err(other) => panic!("expected a config error, got {other}"),
        Ok(value) => panic!("expected a config error, got {value:?}"),
    }
}

fn s(path: &Path) -> String {
    path.display().to_string()
}

fn pbf(paths: &Paths) -> ResolvedPbf {
    resolve_pbf(paths, "dk", "raw").expect("pbf")
}

const DATASET: &str = "[box.datasets.dk.pbf.raw]\nfile = \"dk.osm.pbf\"\n";

#[test]
fn empty_block_is_the_bare_run() {
    let paths = project(
        "tg-empty",
        &format!("[box.tilegen.default]\n{DATASET}"),
        &["dk.osm.pbf"],
    );
    let tg = resolve_tilegen(&paths, DEFAULT_TILEGEN).expect("block");
    let args = run_args(&paths, &tg, &pbf(&paths), None).expect("args");
    assert_eq!(
        args,
        vec![
            "run".to_owned(),
            s(&paths.data_dir.join("dk.osm.pbf")),
            "-o".into(),
            s(&paths.scratch_dir.join("bench-self-output.pmtiles")),
            "--tmp-dir".into(),
            s(&paths.data_dir.join("tilegen_tmp")),
        ]
    );
}

#[test]
fn full_block_golden_argv() {
    let block = "[box.tilegen.default]\n\
        ocean = [\"z0-z7:low/low.shp\", \"z8-z14:high/high.shp\", \"ocean-tiles.pmtiles\"]\n\
        compression_level = 9\n\
        tile_format = \"mlt\"\n\
        tile_compression = \"brotli\"\n\
        compress_sort_chunks = \"lz4\"\n\
        in_memory = true\n\
        threads = 8\n\
        sort_budget = \"256M\"\n\
        way_budget = \"1048576\"\n\
        assemble_budget = \"2g\"\n\
        dedup_cap = 1000\n\
        fanout_cap_default = 64\n\
        fanout_caps = { water = 2, landuse = 1 }\n\
        polygon_simplify_factor = 0.5\n\
        allow_unsafe_flat_index = true\n\
        [box.datasets.dk.pbf.raw]\n\
        file = \"dk.osm.pbf\"\n\
        locations_on_ways = true\n\
        force_sorted = true\n";
    let paths = project(
        "tg-golden",
        block,
        &[
            "dk.osm.pbf",
            "low/low.shp",
            "high/high.shp",
            "ocean-tiles.pmtiles",
        ],
    );
    let tg = resolve_tilegen(&paths, DEFAULT_TILEGEN).expect("block");
    let args = run_args(&paths, &tg, &pbf(&paths), Some("sort")).expect("args");
    let d = &paths.data_dir;
    let expected: Vec<String> = [
        "run".to_owned(),
        s(&d.join("dk.osm.pbf")),
        "-o".into(),
        s(&paths.scratch_dir.join("bench-self-output.pmtiles")),
        "--tmp-dir".into(),
        s(&d.join("tilegen_tmp")),
        "--skip-to".into(),
        "sort".into(),
        "--compression-level".into(),
        "9".into(),
        "--force-sorted".into(),
        "--locations-on-ways".into(),
        "--allow-unsafe-flat-index".into(),
        "--tile-format".into(),
        "mlt".into(),
        "--tile-compression".into(),
        "brotli".into(),
        "--compress-sort-chunks".into(),
        "lz4".into(),
        "--in-memory".into(),
        "-j".into(),
        "8".into(),
        "--sort-budget".into(),
        "268435456".into(),
        "--way-budget".into(),
        "1048576".into(),
        "--assemble-budget".into(),
        "2147483648".into(),
        "--dedup-cap".into(),
        "1000".into(),
        "--fanout-cap-default".into(),
        "64".into(),
        // BTreeMap order, not declaration order.
        "--fanout-cap".into(),
        "landuse=1,water=2".into(),
        "--polygon-simplify-factor".into(),
        "0.5".into(),
        "--ocean".into(),
        format!("z0-z7:{}", s(&d.join("low/low.shp"))),
        "--ocean".into(),
        format!("z8-z14:{}", s(&d.join("high/high.shp"))),
        "--ocean".into(),
        s(&d.join("ocean-tiles.pmtiles")),
    ]
    .into();
    assert_eq!(args, expected);
}

#[test]
fn variant_assertions_come_from_the_selected_variant_only() {
    let block = "[box.tilegen.default]\n\
        [box.datasets.dk.pbf.raw]\nfile = \"dk.osm.pbf\"\n\
        [box.datasets.dk.pbf.locations]\nfile = \"dk-loc.osm.pbf\"\nlocations_on_ways = true\n\
        [box.datasets.dk.pbf.sorted]\nfile = \"dk-sorted.osm.pbf\"\nforce_sorted = true\n";
    let paths = project(
        "tg-variants",
        block,
        &["dk.osm.pbf", "dk-loc.osm.pbf", "dk-sorted.osm.pbf"],
    );
    let tg = resolve_tilegen(&paths, DEFAULT_TILEGEN).expect("block");
    let tail = |variant: &str| -> Vec<String> {
        let pbf = resolve_pbf(&paths, "dk", variant).expect("pbf");
        assert_eq!(pbf.path, paths.data_dir.join(&pbf.entry.file));
        run_args(&paths, &tg, &pbf, None).expect("args")[6..].to_vec()
    };
    assert!(tail("raw").is_empty());
    assert_eq!(tail("locations"), vec!["--locations-on-ways".to_owned()]);
    assert_eq!(tail("sorted"), vec!["--force-sorted".to_owned()]);
}

#[test]
fn shapefile_all_band_and_absolute_paths_render() {
    let paths = project(
        "tg-ocean-all",
        &format!("[box.tilegen.default]\nocean = [\"z0-z14:w.shp\"]\n{DATASET}"),
        &["dk.osm.pbf", "w.shp"],
    );
    let tg = resolve_tilegen(&paths, DEFAULT_TILEGEN).expect("block");
    let args = run_args(&paths, &tg, &pbf(&paths), None).expect("args");
    assert_eq!(
        args[6..],
        [
            "--ocean".to_owned(),
            format!("z0-z14:{}", s(&paths.data_dir.join("w.shp")))
        ]
    );

    // An absolute path is joined onto the data dir as itself.
    let abs = paths.data_dir.join("w.shp");
    let tg = TilegenConfig {
        ocean: vec![OceanSpec::ShapefileAll(s(&abs))],
        ..TilegenConfig::default()
    };
    let args = run_args(&paths, &tg, &pbf(&paths), None).expect("args");
    assert_eq!(args[7], format!("z0-z14:{}", s(&abs)));
}

#[test]
fn missing_ocean_input_is_an_error_not_a_dropped_flag() {
    let paths = project(
        "tg-ocean-missing",
        &format!("[box.tilegen.default]\nocean = [\"z0-z14:w.shp\", \"gone.pmtiles\"]\n{DATASET}"),
        &["dk.osm.pbf", "w.shp"],
    );
    let tg = resolve_tilegen(&paths, DEFAULT_TILEGEN).expect("block");
    let m = message(run_args(&paths, &tg, &pbf(&paths), None));
    assert!(m.contains("tilegen ocean input not found"), "{m}");
    assert!(m.contains("'gone.pmtiles'"), "{m}");
}

#[test]
fn ocean_spec_parse_and_refusals() {
    assert_eq!(
        OceanSpec::parse("z0-z7:a.shp"),
        Ok(OceanSpec::ShapefileLow("a.shp".into()))
    );
    assert_eq!(
        OceanSpec::parse("z8-z14:a.shp"),
        Ok(OceanSpec::ShapefileHigh("a.shp".into()))
    );
    assert_eq!(
        OceanSpec::parse("z0-z14:a.shp"),
        Ok(OceanSpec::ShapefileAll("a.shp".into()))
    );
    assert_eq!(
        OceanSpec::parse("o.pmtiles"),
        Ok(OceanSpec::Artifact("o.pmtiles".into()))
    );
    let bad_band = OceanSpec::parse("z0-z8:a.shp").expect_err("band");
    assert!(bad_band.contains("unknown zoom band 'z0-z8'"), "{bad_band}");
    let bare = OceanSpec::parse("a.shp").expect_err("bare shp");
    assert!(bare.contains("needs a zoom band prefix"), "{bare}");
    let other = OceanSpec::parse("a.geojson").expect_err("other");
    assert!(other.contains("neither"), "{other}");
}

/// Resolve `body` as `[box.tilegen.default]` and return the error message.
fn block_error(name: &str, body: &str) -> String {
    let paths = project(name, &format!("[box.tilegen.default]\n{body}"), &[]);
    message(resolve_tilegen(&paths, DEFAULT_TILEGEN))
}

#[test]
fn block_value_refusals() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "tg-r-empty-ocean",
            "ocean = [\"z0-z14:\"]",
            "ocean input path is empty",
        ),
        (
            "tg-r-band",
            "ocean = [\"z0-z9:a.shp\"]",
            "unknown zoom band",
        ),
        ("tg-r-level", "compression_level = 11", "between 0 and 10"),
        (
            "tg-r-sort-min",
            "sort_budget = \"63M\"",
            "sort_budget must be at least 67108864",
        ),
        (
            "tg-r-way-min",
            "way_budget = \"1048575\"",
            "way_budget must be at least 1048576",
        ),
        (
            "tg-r-asm-min",
            "assemble_budget = \"0\"",
            "assemble_budget must be at least",
        ),
        (
            "tg-r-size",
            "sort_budget = \"1T\"",
            "tilegen sort_budget: expected bytes",
        ),
        (
            "tg-r-overflow",
            "sort_budget = \"99999999999999999999G\"",
            "sort_budget",
        ),
        (
            "tg-r-format",
            "tile_format = \"png\"",
            "[box.tilegen.default]",
        ),
        ("tg-r-unknown", "ocean_auto = true", "unknown field"),
    ];
    for (name, body, needle) in cases {
        let m = block_error(name, body);
        assert!(m.contains(needle), "{body}: {m}");
    }
}

#[test]
fn retired_seam_reconcile_layers_is_refused_by_name() {
    let m = block_error("tg-r-seam", "seam_reconcile_layers = [\"water\"]");
    assert!(
        m.contains("[box.tilegen.default].seam_reconcile_layers is retired"),
        "{m}"
    );
    assert!(m.contains("099611b"), "{m}");
}

#[test]
fn missing_and_malformed_blocks_are_refused() {
    let paths = project("tg-no-blocks", DATASET, &[]);
    let m = message(resolve_tilegen(&paths, DEFAULT_TILEGEN));
    assert!(m.contains("no [box.tilegen.*] blocks"), "{m}");

    let paths = project("tg-empty-table", "tilegen = {}\n", &[]);
    let m = message(resolve_tilegen(&paths, DEFAULT_TILEGEN));
    assert!(m.contains("no [box.tilegen.*] blocks"), "{m}");

    let paths = project("tg-not-table", "tilegen = 3\n", &[]);
    let m = message(resolve_tilegen(&paths, DEFAULT_TILEGEN));
    assert!(m.contains("must be a table of named blocks"), "{m}");

    let paths = project(
        "tg-other-name",
        "[box.tilegen.fast]\n[box.tilegen.slow]\n",
        &[],
    );
    let m = message(resolve_tilegen(&paths, DEFAULT_TILEGEN));
    assert!(m.contains("no [box.tilegen.default]"), "{m}");
    assert!(m.contains("known: fast, slow"), "{m}");

    let paths = project("tg-block-scalar", "[box.tilegen]\ndefault = 1\n", &[]);
    let m = message(resolve_tilegen(&paths, DEFAULT_TILEGEN));
    assert!(m.contains("[box.tilegen.default] must be a table"), "{m}");
}

#[test]
fn pbf_resolution_refusals() {
    let paths = project(
        "tg-pbf",
        "[box.datasets.dk.pbf.raw]\nfile = \"dk.osm.pbf\"\n\
         [box.datasets.dk.pbf.gone]\nfile = \"gone.osm.pbf\"\n\
         [box.datasets.dk.pbf.typo]\nfile = \"dk.osm.pbf\"\nxhash = \"a\"\n",
        &["dk.osm.pbf"],
    );
    let m = message(resolve_pbf(&paths, "se", "raw"));
    assert!(
        m.contains("unknown dataset 'se'") && m.contains("known: dk"),
        "{m}"
    );
    let m = message(resolve_pbf(&paths, "dk", "fast"));
    assert!(
        m.contains("no pbf variant 'fast'") && m.contains("gone, raw, typo"),
        "{m}"
    );
    let m = message(resolve_pbf(&paths, "dk", "gone"));
    assert!(m.contains("pbf variant file not found"), "{m}");
    let m = message(resolve_pbf(&paths, "dk", "typo"));
    assert!(
        m.contains("datasets.dk.pbf.typo") && m.contains("unknown field"),
        "{m}"
    );
}

#[test]
fn pbf_xxhash_pin_is_checked() {
    let root = scratch("tg-xxhash-probe");
    let probe = root.join("probe");
    write(&probe, "x");
    let digest = file_digest(&probe).expect("digest");
    assert_eq!(digest.len(), 32);
    // The registered XXH3-128 of a single "x" byte, as brokkr env prints it.
    assert_eq!(
        digest,
        format!("{:032x}", xxhash_rust::xxh3::xxh3_128(b"x"))
    );

    let wrong = "0".repeat(32);
    let block = format!(
        "[box.datasets.dk.pbf.good]\nfile = \"dk.osm.pbf\"\nxxhash = \"{}\"\n\
         [box.datasets.dk.pbf.bad]\nfile = \"dk.osm.pbf\"\nxxhash = \"{wrong}\"\n\
         [box.datasets.dk.pbf.short]\nfile = \"dk.osm.pbf\"\nxxhash = \"abc\"\n",
        digest.to_ascii_uppercase()
    );
    let paths = project("tg-xxhash", &block, &["dk.osm.pbf"]);
    resolve_pbf(&paths, "dk", "good").expect("uppercase pin matches");
    let m = message(resolve_pbf(&paths, "dk", "bad"));
    assert!(m.contains("xxhash mismatch") && m.contains(&digest), "{m}");
    let m = message(resolve_pbf(&paths, "dk", "short"));
    assert!(m.contains("expected 32 hex digits"), "{m}");
}

fn ocean_paths(name: &str, ocean: &str, files: &[&str]) -> (Paths, TilegenConfig) {
    let paths = project(
        name,
        &format!("[box.tilegen.default]\nocean = {ocean}\n"),
        files,
    );
    let tg = resolve_tilegen(&paths, DEFAULT_TILEGEN).expect("block");
    (paths, tg)
}

#[test]
fn ocean_build_golden_argv() {
    let (paths, tg) = ocean_paths(
        "ob-golden",
        "[\"z0-z7:low.shp\", \"ocean-tiles.pmtiles\", \"z8-z14:high.shp\"]",
        &["low.shp", "high.shp"],
    );
    let (args, output) = ocean_build_args(&paths, &tg).expect("args");
    let d = &paths.data_dir;
    assert_eq!(output, d.join("ocean-tiles.pmtiles"));
    assert_eq!(
        args,
        vec![
            "ocean-build".to_owned(),
            "--ocean".into(),
            format!("z0-z7:{}", s(&d.join("low.shp"))),
            "--ocean".into(),
            format!("z8-z14:{}", s(&d.join("high.shp"))),
            "-o".into(),
            s(&output),
        ]
    );
}

#[test]
fn ocean_build_refusals() {
    let cases: &[(&str, &str, &[&str], &str)] = &[
        ("ob-none", "[]", &[], "no .pmtiles artifact entry"),
        (
            "ob-no-artifact",
            "[\"z0-z14:w.shp\"]",
            &["w.shp"],
            "no .pmtiles artifact entry",
        ),
        (
            "ob-two",
            "[\"z0-z14:w.shp\", \"a.pmtiles\", \"b.pmtiles\"]",
            &["w.shp"],
            "more than one .pmtiles",
        ),
        ("ob-no-shp", "[\"a.pmtiles\"]", &[], "no shapefile entries"),
        (
            "ob-missing",
            "[\"z0-z14:w.shp\", \"a.pmtiles\"]",
            &[],
            "ocean shapefile not found",
        ),
    ];
    for (name, ocean, files, needle) in cases {
        let (paths, tg) = ocean_paths(name, ocean, files);
        let m = message(ocean_build_args(&paths, &tg));
        assert!(m.contains(needle), "{ocean}: {m}");
    }
}

#[test]
fn ocean_build_does_not_require_the_artifact_to_exist() {
    let (paths, tg) = ocean_paths(
        "ob-fresh",
        "[\"z0-z14:w.shp\", \"new.pmtiles\"]",
        &["w.shp"],
    );
    let (_, output) = ocean_build_args(&paths, &tg).expect("args");
    assert_eq!(output, PathBuf::from(&paths.data_dir).join("new.pmtiles"));
    assert!(!output.exists());
}
