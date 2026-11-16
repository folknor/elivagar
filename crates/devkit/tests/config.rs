//! brokkr.toml composition: include merging, host selection and defaults.

mod common;

use common::{scratch, write};
use elivagar_devkit::DevError;
use elivagar_devkit::config::load_from;

fn config_err(result: Result<elivagar_devkit::config::Paths, DevError>) -> String {
    match result {
        Err(DevError::Config(message)) => message,
        Err(other) => panic!("expected a config error, got {other}"),
        Ok(paths) => panic!("expected a config error, got {paths:?}"),
    }
}

#[test]
fn unconfigured_host_gets_brokkr_defaults() {
    let root = scratch("config-defaults");
    write(&root.join("brokkr.toml"), "project = \"elivagar\"\n");
    let paths = load_from(&root.join("brokkr.toml"), Some("nohost")).expect("load");
    assert_eq!(paths.root, root);
    assert_eq!(paths.hostname, "nohost");
    assert_eq!(paths.data_dir, root.join("data"));
    assert_eq!(paths.scratch_dir, root.join("data/scratch"));
    assert_eq!(paths.output_dir, root.join("data/tilegen"));
    assert!(paths.host.is_empty());
    assert_eq!(paths.tilegen_tmp(), root.join("data/tilegen_tmp"));
    assert_eq!(
        paths.tilegen_scratch_output(),
        root.join("data/scratch/bench-self-output.pmtiles")
    );
    assert_eq!(paths.corpus_sort_dir(), root.join(".brokkr/corpus-sort"));
    assert_eq!(
        paths.corpus_calibrands(),
        root.join("data/corpus-calibrands")
    );
}

#[test]
fn legacy_host_table_sets_paths() {
    let root = scratch("config-legacy-host");
    write(
        &root.join("brokkr.toml"),
        "[box]\ndata = \"d\"\nscratch = \"s\"\noutput = \"/abs/out\"\n",
    );
    let paths = load_from(&root.join("brokkr.toml"), Some("box")).expect("load");
    assert_eq!(paths.data_dir, root.join("d"));
    assert_eq!(paths.scratch_dir, root.join("s"));
    assert_eq!(paths.output_dir, std::path::PathBuf::from("/abs/out"));
    assert_eq!(
        paths.host.get("data").and_then(toml::Value::as_str),
        Some("d")
    );
}

#[test]
fn host_dot_table_wins_over_legacy_spelling() {
    let root = scratch("config-host-dot");
    write(
        &root.join("brokkr.toml"),
        "[box]\ndata = \"legacy\"\n[host.box]\ndata = \"modern\"\n",
    );
    let paths = load_from(&root.join("brokkr.toml"), Some("box")).expect("load");
    assert_eq!(paths.data_dir, root.join("modern"));
}

#[test]
fn falls_back_to_legacy_when_host_table_lacks_the_host() {
    let root = scratch("config-host-fallback");
    write(
        &root.join("brokkr.toml"),
        "[host.other]\ndata = \"other\"\n[box]\ndata = \"legacy\"\n",
    );
    let paths = load_from(&root.join("brokkr.toml"), Some("box")).expect("load");
    assert_eq!(paths.data_dir, root.join("legacy"));
}

#[test]
fn non_string_path_key_is_refused() {
    let root = scratch("config-bad-path");
    write(&root.join("brokkr.toml"), "[box]\ndata = 3\n");
    let message = config_err(load_from(&root.join("brokkr.toml"), Some("box")));
    assert!(message.contains("data must be a path string"), "{message}");
}

#[test]
fn includer_wins_and_tables_merge_key_by_key() {
    let root = scratch("config-include-merge");
    write(
        &root.join("brokkr.toml"),
        "include = [\"brokkr.d/a.toml\"]\n[box]\ndata = \"top\"\n[box.datasets.dk]\nbbox = \"top\"\n",
    );
    // a.toml includes b.toml, so b < a < top in precedence.
    write(
        &root.join("brokkr.d/a.toml"),
        "include = [\"b.toml\"]\n[box]\ndata = \"a\"\nscratch = \"a-scratch\"\n\
         [box.datasets.dk]\norigin = \"a\"\n",
    );
    write(
        &root.join("brokkr.d/b.toml"),
        "[box]\nscratch = \"b-scratch\"\noutput = \"b-out\"\n[box.datasets.se]\nbbox = \"b\"\n",
    );
    let paths = load_from(&root.join("brokkr.toml"), Some("box")).expect("load");
    assert_eq!(paths.data_dir, root.join("top"));
    assert_eq!(paths.scratch_dir, root.join("a-scratch"));
    assert_eq!(paths.output_dir, root.join("b-out"));
    let datasets = paths
        .host
        .get("datasets")
        .and_then(toml::Value::as_table)
        .expect("datasets");
    let dk = datasets
        .get("dk")
        .and_then(toml::Value::as_table)
        .expect("dk");
    assert_eq!(dk.get("bbox").and_then(toml::Value::as_str), Some("top"));
    assert_eq!(dk.get("origin").and_then(toml::Value::as_str), Some("a"));
    assert!(datasets.contains_key("se"));
    assert!(!paths.host.contains_key("include"));
}

#[test]
fn include_paths_are_relative_to_the_including_file() {
    let root = scratch("config-include-relative");
    write(&root.join("brokkr.toml"), "include = [\"sub/a.toml\"]\n");
    write(&root.join("sub/a.toml"), "include = [\"b.toml\"]\n");
    write(&root.join("sub/b.toml"), "[box]\ndata = \"from-b\"\n");
    let paths = load_from(&root.join("brokkr.toml"), Some("box")).expect("load");
    assert_eq!(paths.data_dir, root.join("from-b"));
}

#[test]
fn include_cycle_is_refused() {
    let root = scratch("config-include-cycle");
    write(&root.join("brokkr.toml"), "include = [\"a.toml\"]\n");
    write(&root.join("a.toml"), "include = [\"brokkr.toml\"]\n");
    let message = config_err(load_from(&root.join("brokkr.toml"), Some("box")));
    assert!(message.contains("include cycle"), "{message}");
}

#[test]
fn double_include_is_refused() {
    let root = scratch("config-include-twice");
    write(
        &root.join("brokkr.toml"),
        "include = [\"a.toml\", \"b.toml\"]\n",
    );
    write(&root.join("a.toml"), "include = [\"c.toml\"]\n");
    write(&root.join("b.toml"), "include = [\"c.toml\"]\n");
    write(&root.join("c.toml"), "");
    let message = config_err(load_from(&root.join("brokkr.toml"), Some("box")));
    assert!(message.contains("included more than once"), "{message}");
}

#[test]
fn malformed_includes_are_refused() {
    let root = scratch("config-include-malformed");
    write(&root.join("brokkr.toml"), "include = \"a.toml\"\n");
    let message = config_err(load_from(&root.join("brokkr.toml"), Some("box")));
    assert!(message.contains("must be an array"), "{message}");

    write(&root.join("brokkr.toml"), "include = [\"\"]\n");
    let message = config_err(load_from(&root.join("brokkr.toml"), Some("box")));
    assert!(message.contains("non-empty path strings"), "{message}");

    write(&root.join("brokkr.toml"), "include = [\"missing.toml\"]\n");
    let message = config_err(load_from(&root.join("brokkr.toml"), Some("box")));
    assert!(message.contains("missing.toml"), "{message}");
}

#[test]
fn toml_syntax_error_names_the_file() {
    let root = scratch("config-syntax");
    write(&root.join("brokkr.toml"), "include = [\"bad.toml\"]\n");
    write(&root.join("bad.toml"), "[box\n");
    let message = config_err(load_from(&root.join("brokkr.toml"), Some("box")));
    assert!(message.contains("bad.toml"), "{message}");
}
