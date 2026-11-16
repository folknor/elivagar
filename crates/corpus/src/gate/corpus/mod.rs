//! The corpus gate - brokkr's adjudication of an elivagar archive against the
//! committed baseline. This is the code that moved out of elivagar wholesale
//! (`elivagar.md`, "Code that leaves the crate"); it reads archives in-process
//! through the linked-crate shim in `super::eliv`.
//!
//! Module map:
//! - `digest`   - leaf runs, the fold, `digest`/`leaves` formats, self-integrity.
//! - `canon_sink` - shared byte writer for the frozen tile hash and regress.
//! - `layout`   - committed corpus paths and tile SVG names.
//! - `diff`     - the localized content diff (leaves / zooms / buckets).
//! - `contract` - the comparability gate (provenance read + gating policy).
//! - `manifest` - the hard-tiles ledger.
//! - `render`   - the canonical SVG render core.
//! - `mutate`   - the calibration instrument.
//!
//! The `check` pipeline follows the ordering fixed in `brokkr.md`: a condition
//! short-circuits the content walk only if it invalidates the walk's meaning.
//! Baseline damage (step 1) and archive incomparability (step 2) do; baseline
//! staleness (step 4) does not and is strictly subordinate to the content
//! verdict, so it can never mask a regression.

pub(crate) mod canon_sink;
pub mod canonical;
pub mod contract;
pub mod diff;
pub mod digest;
#[cfg(test)]
pub(crate) mod fixture;
pub(crate) mod layout;
pub mod manifest;
pub mod mutate;
pub mod render;
pub(crate) mod sort;
pub mod style;

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use rustc_hash::FxHashMap;

use super::eliv::{ArchiveView, BlobRef, next_zoom_boundary, tile_id_to_zxy};
use canonical::{DecodeScratch, semantic_hash};
use contract::{ContractDoc, ContractState};
use digest::{Digest, LeafRun};

// Verdict-facing types the dispatch consumes.
pub use digest::DigestMode;
pub use mutate::MutationOp;

/// The gate verdict and sole owner of its process exit mapping, as pinned by
/// `brokkr.md` (Signals). 0 pass / 1 content changed / 2 archive cannot be
/// judged / 3 the baseline is the problem (damage OR staleness).
///
/// A run that reaches no verdict at all - lock, bootstrap, archive resolution,
/// a genuine IO failure - is none of these: it exits [`INCOMPLETE_EXIT`] via
/// `cmd::operational`, so it can never read as exit 1's "the archive
/// regressed".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Pass,
    ContentMismatch,
    ArchiveRefused,
    BaselineTrouble,
}

impl Outcome {
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Pass => 0,
            Outcome::ContentMismatch => 1,
            Outcome::ArchiveRefused => 2,
            Outcome::BaselineTrouble => 3,
        }
    }
}

/// Exit code for a `pmtiles-corpus` run that could not be completed. Distinct
/// from every verdict code; 2 is taken by `ArchiveRefused` (which clap's usage
/// errors also use - a usage error prints clap's usage text, a refusal prints
/// the gate's reason).
pub const INCOMPLETE_EXIT: i32 = 4;

#[derive(Clone, Debug, Default)]
pub struct CheckReport {
    pub message: String,
    pub contract_diffs: Vec<String>,
    pub warnings: Vec<String>,
    pub changed: u64,
    pub changed_unit: Option<ChangedUnit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangedUnit {
    LeafRuns,
    Buckets,
}

fn report(message: impl Into<String>) -> CheckReport {
    CheckReport {
        message: message.into(),
        ..Default::default()
    }
}

fn baseline_trouble(message: impl Into<String>) -> (Outcome, CheckReport) {
    (Outcome::BaselineTrouble, report(message))
}
fn archive_refused(message: impl Into<String>) -> (Outcome, CheckReport) {
    (Outcome::ArchiveRefused, report(message))
}

fn refused((outcome, report): (Outcome, CheckReport)) -> Gate {
    Gate::Refused(outcome, report)
}

/// Fold a read of committed baseline material into the BASELINE verdict.
///
/// A missing or malformed committed file is damage to the baseline (exit 3),
/// never a statement about the archive: letting the `io::Error` escape makes the
/// dispatch wrap it as `DevError::Io` and exit **1**, which tells an automated
/// caller that the archive's content regressed when in fact the corpus took
/// merge damage. Only `NotFound`/`InvalidData` fold - a genuine IO failure
/// (permissions, a bad disk) is not a verdict about anything and still
/// propagates.
fn baseline_material<T>(what: &str, read: io::Result<T>) -> io::Result<Result<T, String>> {
    match read {
        Ok(v) => Ok(Ok(v)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            Ok(Err(format!("{what}: {e}")))
        }
        Err(e) => Err(e),
    }
}

/// The corpus-global canonical style: `<corpus-root>/style.toml`, where
/// `corpus_dir` is `<corpus-root>/<dataset>`. Anchored at the corpus root so it
/// resolves the same regardless of the invoking cwd.
///
/// The ONE rule for locating the style. `check`'s staleness scan reads it, and
/// the dispatch derives `render-manifest`'s and `render`'s `--style` default
/// from it too: a second rule (the dispatch used to default to
/// `<build-root>/corpus/style.toml`) agrees only while `--corpus` sits under the
/// build root, and otherwise has render-manifest record one style's hash while
/// check compares another's - stale forever.
pub(crate) fn corpus_style_path(corpus_dir: &Path) -> std::path::PathBuf {
    corpus_dir.parent().unwrap_or(corpus_dir).join("style.toml")
}

fn format_guard(a: &ArchiveView) -> Option<String> {
    a.tile_format_refusal()
}

/// Compute the canonical digest and leaf runs from an open archive.
///
/// The blob-level canonical hash is decoded once per unique blob and the runs
/// are split at zoom boundaries then merged within a zoom - a canonical run is a
/// maximal span WITHIN one zoom sharing one hash, so a shared blob straddling a
/// zoom boundary stays two runs. Blob decoding is sequential here; elivagar's
/// original parallelized the per-blob decode.
///
/// `sort_scratch` is the directory the digest's external sort may spill into
/// (`ProjectState::corpus_sort_dir`).
pub fn compute(
    archive: &ArchiveView,
    mode: DigestMode,
    sort_scratch: &Path,
) -> io::Result<(Digest, Vec<LeafRun>)> {
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
    let mut scratch = DecodeScratch::default();
    // Lookup only, never iterated: the map type cannot affect the digest.
    let mut hashes: FxHashMap<BlobRef, u128> = FxHashMap::default();
    for b in &blobs {
        // Regress uses this same canonical function in its parallel pass; only
        // the scheduling differs between the two archive walks.
        let h = semantic_hash(&archive.raw_blob(*b)?, &mut scratch)?;
        hashes.insert(*b, h);
    }
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
            let length =
                u32::try_from(n - at).map_err(|_| digest::invalid("tile run exceeds u32"))?;
            if let Some(last) = leaves.last_mut()
                && last.tile_id + u64::from(last.run_length) == at
                && last.hash == semantic
                && tile_id_to_zxy(last.tile_id).0 == tile_id_to_zxy(at).0
            {
                last.run_length = last
                    .run_length
                    .checked_add(length)
                    .ok_or_else(|| digest::invalid("merged tile run exceeds u32"))?;
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
    let d = digest::fold_leaves(&leaves, mode, sort_scratch)?;
    Ok((d, leaves))
}

// ---------------------------------------------------------------------------
// Provenance read helpers.
// ---------------------------------------------------------------------------

/// Parse the committed `contract.json`. A malformed committed contract is a
/// BASELINE problem (exit 3), so the error string is surfaced as such.
fn contract_from_file(path: &Path) -> io::Result<Result<ContractDoc, String>> {
    let text = match baseline_material("committed corpus contract", std::fs::read_to_string(path))?
    {
        Ok(t) => t,
        Err(msg) => return Ok(Err(msg)),
    };
    Ok(
        match contract::extract_contract(&format!("{{\"elivagar\":{text}}}")) {
            ContractState::Contract(c) => Ok(c),
            s => Err(format!("invalid committed corpus contract: {s}")),
        },
    )
}

/// Read the fresh archive's embedded contract. An absent/invalid block is an
/// ARCHIVE refusal (exit 2) on the archive's own terms.
fn candidate_contract(a: &ArchiveView) -> io::Result<Result<ContractDoc, String>> {
    Ok(match contract::extract_contract(&a.metadata()?) {
        ContractState::Contract(c) => Ok(c),
        s => Err(format!("archive contract refused: {s}")),
    })
}

// ---------------------------------------------------------------------------
// The gate through the walk (steps 1-3), shared by `check` and `render-manifest`.
// ---------------------------------------------------------------------------

/// Steps 1-3 of the pipeline. On a refusal or content mismatch the verdict is
/// returned directly; on success the open archive and its parsed baseline are
/// handed back so the caller can run staleness (`check`) or render (`render-
/// manifest`) against digest-equal content.
enum Gate {
    Refused(Outcome, CheckReport),
    // Boxed: the open archive view + parsed baseline dwarf the Refused variant.
    Passed(Box<Passed>),
}

struct Passed {
    view: ArchiveView,
    warnings: Vec<String>,
    tiles: u64,
    unique: u64,
}

#[allow(clippy::too_many_lines)]
fn gate_through_walk(archive: &Path, corpus_dir: &Path, sort_scratch: &Path) -> io::Result<Gate> {
    let layout = layout::CorpusLayout::new(corpus_dir);
    // STEP 1 - baseline integrity (subject: the baseline; exit 3 on damage).
    // Every read here goes through `baseline_material`, so an absent or
    // corrupted committed file is the exit-3 verdict rather than an escaping
    // io::Error the dispatch would flatten into exit 1 (content mismatch).
    let base = match baseline_material("baseline digest", digest::parse_baseline(&layout.digest()))?
    {
        Ok(b) => b,
        Err(msg) => return Ok(refused(baseline_trouble(msg))),
    };
    let committed_leaves = if base.mode == DigestMode::Leaves {
        match baseline_material("baseline leaves", digest::parse_leaves(&layout.leaves()))? {
            Ok(l) => Some(l),
            Err(msg) => return Ok(refused(baseline_trouble(msg))),
        }
    } else {
        None
    };
    if let Some(msg) =
        digest::baseline_inconsistency(&base, committed_leaves.as_deref(), sort_scratch)?
    {
        return Ok(refused(baseline_trouble(format!(
            "baseline internally inconsistent: {msg}"
        ))));
    }

    // STEP 2 - comparability (subject: the incoming archive; exit 2 on refusal).
    let view = ArchiveView::open(archive)?;
    if let Some(msg) = format_guard(&view) {
        return Ok(refused(archive_refused(msg)));
    }
    let base_contract = match contract_from_file(&layout.contract())? {
        Ok(c) => c,
        Err(msg) => return Ok(refused(baseline_trouble(msg))),
    };
    let candidate = match candidate_contract(&view)? {
        Ok(c) => c,
        Err(msg) => return Ok(refused(archive_refused(msg))),
    };
    let diffs = contract::contract_diff(&base_contract, &candidate);
    if !diffs.is_empty() {
        return Ok(Gate::Refused(
            Outcome::ArchiveRefused,
            CheckReport {
                message: "contract mismatch".into(),
                contract_diffs: diffs,
                ..Default::default()
            },
        ));
    }
    let mut warnings = Vec::new();
    if contract::input_name(&base_contract) != contract::input_name(&candidate) {
        warnings.push(format!(
            "input name differs (diagnostic only): {:?} vs {:?}",
            contract::input_name(&base_contract),
            contract::input_name(&candidate)
        ));
    }

    // STEP 3 - the walk (subject: content; exit 1 on mismatch). Always runs for
    // a comparable archive against an intact baseline; style state is irrelevant.
    let (d, current_leaves) = compute(&view, base.mode, sort_scratch)?;
    let content_ok =
        d.root == base.root && (base.mode != DigestMode::Buckets || d.broot == base.broot);
    if !content_ok {
        let mut lines = vec!["content mismatch".to_string()];
        lines.extend(diff::zoom_delta_lines(&base.zooms, &d.zooms));
        let (changed, changed_unit) = match base.mode {
            DigestMode::Leaves => {
                let (n, diff_lines) =
                    diff::leaf_diff(committed_leaves.as_deref().unwrap_or(&[]), &current_leaves);
                lines.extend(diff_lines);
                (n, ChangedUnit::LeafRuns)
            }
            DigestMode::Buckets => {
                let (n, diff_lines) = diff::bucket_delta_lines(&base.buckets, &d.buckets);
                lines.extend(diff_lines);
                (n, ChangedUnit::Buckets)
            }
        };
        return Ok(Gate::Refused(
            Outcome::ContentMismatch,
            CheckReport {
                message: lines.join("\n"),
                warnings,
                changed,
                changed_unit: Some(changed_unit),
                ..Default::default()
            },
        ));
    }
    Ok(Gate::Passed(Box::new(Passed {
        view,
        warnings,
        tiles: d.tiles,
        unique: d.unique,
    })))
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// The full verdict pipeline. See module header for the ordering guarantee.
pub fn check(
    archive: &Path,
    corpus_dir: &Path,
    sort_scratch: &Path,
) -> io::Result<(Outcome, CheckReport)> {
    let (view, mut warnings, tiles, unique) =
        match gate_through_walk(archive, corpus_dir, sort_scratch)? {
            Gate::Refused(o, r) => return Ok((o, r)),
            Gate::Passed(p) => (p.view, p.warnings, p.tiles, p.unique),
        };

    // STEP 4 - staleness (subject: the baseline; exit 3, strictly subordinate).
    // Only reached with a passing content walk, so it can never mask a mismatch.
    let stale = match svg_staleness(&view, corpus_dir)? {
        Ok(stale) => stale,
        Err(msg) => {
            return Ok((
                Outcome::BaselineTrouble,
                CheckReport {
                    message: msg,
                    warnings,
                    ..Default::default()
                },
            ));
        }
    };
    if !stale.is_empty() {
        let message = stale
            .into_iter()
            .map(|p| format!("svg stale (digest unchanged, re-render corpus): {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        // `changed` is a leaf-diff run count; staleness is not a content diff,
        // so it stays 0 (the report's own lines say "digest unchanged").
        return Ok((
            Outcome::BaselineTrouble,
            CheckReport {
                message,
                warnings,
                ..Default::default()
            },
        ));
    }

    warnings.shrink_to_fit();
    Ok((
        Outcome::Pass,
        CheckReport {
            message: format!("{tiles} tiles, {unique} unique"),
            warnings,
            ..Default::default()
        },
    ))
}

/// The style + SVG staleness scan (empty vec == fresh). Returns the stale
/// reasons: a missing/mismatched recorded style hash and any tile whose fresh
/// render differs from the committed file, plus orphaned tile files. All of
/// these mean "re-render the corpus" - baseline staleness, never an archive
/// verdict. Skipped entirely when the corpus has no manifest.
///
/// The outer `Err(String)` is baseline *damage* met on the way: a missing or
/// malformed `contract.json`, `style.toml` or `manifest.toml`. Those fold
/// through `baseline_material` like step 1's reads, so they report as exit 3
/// rather than escaping as an `io::Error` the dispatch would turn into a
/// non-verdict failure.
fn svg_staleness(view: &ArchiveView, corpus_dir: &Path) -> io::Result<Result<Vec<String>, String>> {
    let layout = layout::CorpusLayout::new(corpus_dir);
    let manifest_path = layout.manifest();
    if !manifest_path.exists() {
        return Ok(Ok(Vec::new()));
    }
    let contract_read = std::fs::read_to_string(layout.contract()).and_then(|t| {
        serde_json::from_str::<serde_json::Value>(&t)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    });
    let raw = match baseline_material("committed corpus contract", contract_read)? {
        Ok(v) => v,
        Err(msg) => return Ok(Err(msg)),
    };
    let recorded = raw
        .pointer("/style/xxh3_128")
        .and_then(serde_json::Value::as_str);
    let Some(recorded) = recorded else {
        return Ok(Ok(vec![
            "contract records no style hash - run render-manifest".to_string(),
        ]));
    };
    // The canonical style is corpus-global, one per corpus root
    // (`<root>/style.toml`), and `corpus_dir` is `<root>/<dataset>`. Anchor there
    // rather than at the cwd-relative path recorded in contract.json.
    let style = match baseline_material(
        "corpus style",
        style::Style::load(&corpus_style_path(corpus_dir)),
    )? {
        Ok(s) => s,
        Err(msg) => return Ok(Err(msg)),
    };
    if recorded != style.hash_hex() {
        return Ok(Ok(vec![
            "style hash differs from contract - run render-manifest".to_string(),
        ]));
    }
    let manifest = match baseline_material("corpus manifest", manifest::load(&manifest_path))? {
        Ok(m) => m,
        Err(msg) => return Ok(Err(msg)),
    };
    let runs = view.read_all_runs()?;
    let tiles = layout.tiles();
    let mut expected = BTreeSet::new();
    let mut stale = Vec::new();
    for entry in &manifest.tile {
        let name = manifest::file_name(entry);
        expected.insert(name.clone());
        let svg = render::render_archive_tile_from_runs(
            view,
            &runs,
            entry.z,
            entry.x,
            entry.y,
            &style,
            (!entry.layers.is_empty()).then_some(entry.layers.as_slice()),
        )?;
        let path = tiles.join(&name);
        let current = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if current.as_deref() != Some(svg.bytes.as_slice()) {
            stale.push(path.display().to_string());
        }
    }
    if tiles.exists() {
        for entry in std::fs::read_dir(&tiles)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && !expected.contains(&entry.file_name().to_string_lossy().to_string())
            {
                stale.push(format!("{} (orphan)", entry.path().display()));
            }
        }
    }
    Ok(Ok(stale))
}

// ---------------------------------------------------------------------------
// render-manifest
// ---------------------------------------------------------------------------

/// The style path as written into the committed `contract.json`: relative to
/// the corpus root, never the caller's absolute path - the contract is a
/// committed file and must not carry machine-specific text. The record is
/// diagnostic; loaders anchor at `<root>/style.toml` (`corpus_style_path`).
fn recorded_style_path(corpus_dir: &Path, style_path: &Path) -> String {
    let root = corpus_dir.parent().unwrap_or(corpus_dir);
    style_path
        .strip_prefix(root)
        .unwrap_or_else(|_| Path::new(style_path.file_name().unwrap_or(style_path.as_os_str())))
        .to_string_lossy()
        .into_owned()
}

/// Re-render every committed manifest target after proving the archive equals
/// the digest baseline (steps 1-3 only - SVG/style staleness are exactly what
/// this command fixes, so they must not block it). Writes the tiles and records
/// the style hash into `contract.json`. Never rotates digest material.
pub fn render_manifest(
    archive: &Path,
    corpus_dir: &Path,
    style_path: &Path,
    sort_scratch: &Path,
) -> io::Result<(Outcome, CheckReport)> {
    let view = match gate_through_walk(archive, corpus_dir, sort_scratch)? {
        Gate::Refused(o, r) => return Ok((o, r)),
        Gate::Passed(p) => p.view,
    };
    let layout = layout::CorpusLayout::new(corpus_dir);
    let manifest_path = layout.manifest();
    if !manifest_path.exists() {
        return Ok(baseline_trouble("manifest.toml is absent"));
    }
    let manifest = match baseline_material("corpus manifest", manifest::load(&manifest_path))? {
        Ok(m) => m,
        Err(msg) => return Ok(baseline_trouble(msg)),
    };
    let style = match baseline_material("corpus style", style::Style::load(style_path))? {
        Ok(s) => s,
        Err(msg) => return Ok(baseline_trouble(msg)),
    };
    let runs = view.read_all_runs()?;
    let mut warnings = Vec::new();
    // `check` judges staleness against the canonical style alone
    // (`corpus_style_path`); rendering with any other records a hash check
    // will never match, so say so rather than leave a corpus stale forever.
    let canonical_style = corpus_style_path(corpus_dir);
    let same_style = style_path == canonical_style
        || matches!(
            (std::fs::canonicalize(style_path), std::fs::canonicalize(&canonical_style)),
            (Ok(a), Ok(b)) if a == b
        );
    if !same_style {
        warnings.push(format!(
            "--style {} is not the corpus style {}: `check` compares against the \
             latter and will report this render as stale",
            style_path.display(),
            canonical_style.display()
        ));
    }
    let tiles = layout.tiles();
    std::fs::create_dir_all(&tiles)?;
    let mut wanted = BTreeSet::new();
    for entry in &manifest.tile {
        let name = manifest::file_name(entry);
        wanted.insert(name.clone());
        let svg = render::render_archive_tile_from_runs(
            &view,
            &runs,
            entry.z,
            entry.x,
            entry.y,
            &style,
            (!entry.layers.is_empty()).then_some(entry.layers.as_slice()),
        )?;
        warnings.extend(svg.warnings);
        std::fs::write(tiles.join(name), svg.bytes)?;
    }
    for entry in std::fs::read_dir(&tiles)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && !wanted.contains(&entry.file_name().to_string_lossy().to_string())
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    // Record the style identity brokkr authors (path + hash) into contract.json.
    let view_contract = match candidate_contract(&view)? {
        Ok(c) => c,
        Err(msg) => return Ok(archive_refused(msg)),
    };
    let style_json = serde_json::json!({
        "path": recorded_style_path(corpus_dir, style_path),
        "xxh3_128": style.hash_hex(),
    });
    crate::atomic_write::replace(
        &layout.contract(),
        contract_text(&view_contract, Some(style_json))?.as_bytes(),
    )?;
    Ok((
        Outcome::Pass,
        CheckReport {
            message: format!("rendered {} manifest SVGs", wanted.len()),
            warnings,
            ..Default::default()
        },
    ))
}

// ---------------------------------------------------------------------------
// render (single tile) + rings
// ---------------------------------------------------------------------------

/// Render one archive tile to canonical SVG. The standalone human/eyeball path
/// (`elivagar.md`, `render`): `-o` is a convenience, and because brokkr owns
/// placement it creates the parent dir; with no `-o` the SVG goes to stdout.
pub fn render_tile(
    archive: &Path,
    z: u8,
    x: u32,
    y: u32,
    style_path: &Path,
    layers: Option<&[String]>,
    output: Option<&Path>,
) -> io::Result<()> {
    use std::io::Write as _;
    let view = ArchiveView::open(archive)?;
    let style = style::Style::load(style_path)?;
    let svg = render::render_archive_tile(&view, z, x, y, &style, layers)?;
    for w in &svg.warnings {
        crate::output::warn(w);
    }
    match output {
        Some(p) => {
            if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(p, &svg.bytes)?;
        }
        None => std::io::stdout().write_all(&svg.bytes)?,
    }
    Ok(())
}

/// Emit ocean ring geometry - the input to the ring-grouping differential oracle
/// (`brokkr.md`, Calibration). Writes to `output` (or stdout when `-`).
pub fn rings(archive: &Path, output: &Path) -> io::Result<()> {
    let view = ArchiveView::open(archive)?;
    let mut out: Box<dyn std::io::Write> = if output == Path::new("-") {
        Box::new(std::io::stdout())
    } else {
        if let Some(parent) = output.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Box::new(std::fs::File::create(output)?)
    };
    render::dump_ring_grouping(&view, &mut out)
}

/// Serialize a contract snapshot to the committed `contract.json` text. Stored
/// whole (gated + diagnostic); the style record is brokkr-authored and carried
/// only when a manifest render produced it.
fn contract_text(c: &ContractDoc, style: Option<serde_json::Value>) -> io::Result<String> {
    let mut v = serde_json::json!({
        "schema": contract::SCHEMA_VERSION,
        "input": c.input,
        "config": c.config,
        "build": c.build,
    });
    if let Some(style) = style {
        v["style"] = style;
    }
    serde_json::to_string_pretty(&v)
        .map(|s| format!("{s}\n"))
        .map_err(io::Error::other)
}

// ---------------------------------------------------------------------------
// bless
// ---------------------------------------------------------------------------

/// Write (or rotate) the committed baseline from an archive. Guards enforced
/// from the typed provenance: refuse dirty builds and non-locations archives.
/// Replacing an existing baseline requires `rotate`; without it, the content
/// delta is named and the write refused (exit 1, unadjudicated rotation).
#[allow(clippy::too_many_lines)]
pub fn bless(
    archive: &Path,
    corpus_dir: &Path,
    mode: DigestMode,
    rotate: bool,
    sort_scratch: &Path,
) -> io::Result<(Outcome, CheckReport)> {
    let layout = layout::CorpusLayout::new(corpus_dir);
    let a = ArchiveView::open(archive)?;
    if let Some(msg) = format_guard(&a) {
        return Ok(archive_refused(msg));
    }
    let contract = match candidate_contract(&a)? {
        Ok(c) => c,
        Err(msg) => return Ok(archive_refused(msg)),
    };
    let mut warnings = Vec::new();
    match contract.build.get("elivagar").and_then(|v| v.get("dirty")) {
        Some(serde_json::Value::Bool(true)) => {
            return Ok(archive_refused("candidate build is dirty: elivagar"));
        }
        Some(serde_json::Value::Bool(false)) => {}
        _ => warnings
            .push("elivagar build dirty flag is null/absent: reproducibility unproven".to_string()),
    }
    if contract
        .input
        .pointer("/features/locations_on_ways")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Ok(archive_refused("candidate is not locations-generated"));
    }
    if layout.digest().exists() && !rotate {
        // A damaged existing baseline is exit 3, like `check`'s step 1 - not
        // an escaping io::Error, and not the exit 1 this branch otherwise
        // returns for "rotation not adjudicated". `--rotate` overwrites it.
        let base =
            match baseline_material("baseline digest", digest::parse_baseline(&layout.digest()))? {
                Ok(b) => b,
                Err(msg) => {
                    return Ok(baseline_trouble(format!(
                        "{msg} (bless --rotate replaces it)"
                    )));
                }
            };
        let committed_leaves = if base.mode == DigestMode::Leaves && layout.leaves().is_file() {
            match baseline_material("baseline leaves", digest::parse_leaves(&layout.leaves()))? {
                Ok(l) => Some(l),
                Err(msg) => {
                    return Ok(baseline_trouble(format!(
                        "{msg} (bless --rotate replaces it)"
                    )));
                }
            }
        } else {
            None
        };
        let (d, current_leaves) = compute(&a, base.mode, sort_scratch)?;
        let mut lines = vec!["rotation requires --rotate".to_string()];
        if let Ok(Ok(base_contract)) = contract_from_file(&layout.contract()) {
            for path in contract::contract_diff(&base_contract, &contract) {
                lines.push(format!("contract {path}"));
            }
        }
        lines.extend(diff::zoom_delta_lines(&base.zooms, &d.zooms));
        match base.mode {
            DigestMode::Leaves => {
                let (_, diff_lines) =
                    diff::leaf_diff(committed_leaves.as_deref().unwrap_or(&[]), &current_leaves);
                lines.extend(diff_lines);
            }
            DigestMode::Buckets => {
                let (_, diff_lines) = diff::bucket_delta_lines(&base.buckets, &d.buckets);
                lines.extend(diff_lines);
            }
        }
        return Ok((
            Outcome::ContentMismatch,
            CheckReport {
                message: lines.join("\n"),
                warnings,
                ..Default::default()
            },
        ));
    }
    let (d, l) = compute(&a, mode, sort_scratch)?;
    std::fs::create_dir_all(corpus_dir)?;
    let digest_path = layout.digest();
    let contract_path = layout.contract();
    let leaves_path = layout.leaves();
    crate::atomic_write::replace(&digest_path, digest::digest_text(&d).as_bytes())?;
    crate::atomic_write::replace(&contract_path, contract_text(&contract, None)?.as_bytes())?;
    if mode == DigestMode::Leaves {
        crate::atomic_write::replace(&leaves_path, digest::leaves_text(&l).as_bytes())?;
    } else if leaves_path.exists() {
        std::fs::remove_file(&leaves_path)?;
    }
    if layout.manifest().exists() {
        let (verdict, r) = render_manifest(
            archive,
            corpus_dir,
            &corpus_style_path(corpus_dir),
            sort_scratch,
        )?;
        if verdict != Outcome::Pass {
            return Ok((verdict, r));
        }
        warnings.extend(r.warnings);
    }
    Ok((
        Outcome::Pass,
        CheckReport {
            message: format!("blessed {} tiles ({})", d.tiles, mode.as_str()),
            warnings,
            ..Default::default()
        },
    ))
}

#[cfg(test)]
mod recorded_style_path_tests {
    //! `contract.json` is committed, so the style record must be corpus-relative
    //! whatever absolute path the caller handed in.

    use super::{Path, recorded_style_path};

    #[test]
    fn canonical_style_records_relative() {
        assert_eq!(
            recorded_style_path(
                Path::new("/home/x/elivagar/corpus/planet"),
                Path::new("/home/x/elivagar/corpus/style.toml")
            ),
            "style.toml"
        );
    }

    #[test]
    fn foreign_style_records_file_name() {
        assert_eq!(
            recorded_style_path(Path::new("/a/corpus/planet"), Path::new("/tmp/other.toml")),
            "other.toml"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod baseline_verdict_tests {
    //! Step 1's verdict mapping: damaged committed material is exit 3, never
    //! the exit 1 that means "the archive's content regressed". Step 1 runs
    //! before the archive is opened, so these need no archive at all - the path
    //! below never exists, and reaching it would itself be the failure.

    use super::super::eliv::xy_to_tile_id;
    use super::digest::{digest_text, fold_leaves, leaves_text};
    use super::fixture::TestDir;
    use super::layout::CorpusLayout;
    use super::{DigestMode, LeafRun, Outcome, Path, check};

    fn intact_baseline(dir: &Path) {
        let layout = CorpusLayout::new(dir);
        let leaves = vec![LeafRun {
            tile_id: xy_to_tile_id(2, 1, 1),
            run_length: 1,
            hash: 0xABCD,
        }];
        let d = fold_leaves(&leaves, DigestMode::Leaves, &dir.join("sort-scratch")).unwrap();
        std::fs::write(layout.digest(), digest_text(&d)).unwrap();
        std::fs::write(layout.leaves(), leaves_text(&leaves)).unwrap();
    }

    fn verdict(corpus_dir: &Path) -> Outcome {
        check(
            &corpus_dir.join("no-such-archive.pmtiles"),
            corpus_dir,
            &corpus_dir.join("sort-scratch"),
        )
        .expect("baseline damage is a verdict, not an escaping io::Error")
        .0
    }

    #[test]
    fn a_missing_digest_is_baseline_trouble() {
        let dir = TestDir::new("corpus-baseline-missing-digest");
        assert_eq!(verdict(&dir.path), Outcome::BaselineTrouble);
    }

    #[test]
    fn a_malformed_digest_is_baseline_trouble() {
        let dir = TestDir::new("corpus-baseline-malformed-digest");
        intact_baseline(&dir.path);
        std::fs::write(
            CorpusLayout::new(&dir.path).digest(),
            "mode leaves\nroot 00\n",
        )
        .unwrap();
        assert_eq!(verdict(&dir.path), Outcome::BaselineTrouble);
    }

    #[test]
    fn missing_and_malformed_leaves_are_both_baseline_trouble() {
        let dir = TestDir::new("corpus-baseline-leaves");
        intact_baseline(&dir.path);
        let leaves_path = CorpusLayout::new(&dir.path).leaves();
        std::fs::remove_file(&leaves_path).unwrap();
        assert_eq!(verdict(&dir.path), Outcome::BaselineTrouble);
        std::fs::write(&leaves_path, format!("{}not a row\n", leaves_text(&[]))).unwrap();
        assert_eq!(verdict(&dir.path), Outcome::BaselineTrouble);
    }

    #[test]
    fn a_real_io_failure_is_not_dressed_up_as_a_verdict() {
        // `baseline_material` folds only NotFound/InvalidData. Anything else -
        // here, the archive open that an intact baseline lets step 2 reach -
        // still propagates as an error rather than becoming a content verdict.
        let dir = TestDir::new("corpus-baseline-intact");
        intact_baseline(&dir.path);
        assert!(
            check(
                &dir.path.join("no-such-archive.pmtiles"),
                &dir.path,
                &dir.path.join("sort-scratch"),
            )
            .is_err()
        );
    }
}
