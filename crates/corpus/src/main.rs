//! `elivagar-corpus` - the corpus gate, the two-archive regress diff and the
//! compare-tiles census, in-tree.
//!
//! Ported from brokkr (`src/elivagar/`), where these ran as `brokkr
//! pmtiles-corpus <sub>`, `brokkr regress` and `brokkr compare-tiles`. Invoke
//! as `brokkr run elivagar-corpus -- <sub> ...` from the repo root. The
//! subcommands and flags keep brokkr's spellings, and the gate reads and
//! writes the committed baseline (`corpus/<dataset>/`, `corpus/style.toml`)
//! byte for byte as brokkr did.
//!
//! Exit codes:
//! - corpus subcommands: 0 pass, 1 content mismatch, 2 archive refusal,
//!   3 baseline trouble, 4 could not run (any error that is not a verdict).
//! - regress: 0 pass, 1 regressed, 3 could not complete.
//! - compare-tiles: 0, or 1 on error.
//! - clap usage errors: 2.

mod atomic_write;
mod error;
mod gate;
mod output;
mod support;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgGroup, Args, Parser, Subcommand};

use elivagar_devkit::archive_store::ArchiveStore;
use elivagar_devkit::config;
use error::DevError;
use gate::corpus as corpus_gate;

const DEFAULT_DATASET: &str = "denmark";
const DEFAULT_RAW_VARIANT: &str = "raw";

#[derive(Parser)]
#[command(
    name = "elivagar-corpus",
    about = "Corpus gate, regress and compare-tiles"
)]
struct Cli {
    /// Host section of brokkr.toml to resolve paths from (default: the kernel
    /// hostname)
    #[arg(long, global = true)]
    host: Option<String>,

    #[command(subcommand)]
    command: Command,
}

/// The archive selector shared by every corpus subcommand: the same
/// `[--dataset D] [--variant V] [--commit H | --file P]` resolver brokkr's
/// `pmtiles-inspect`/`diag`/`svg` use.
#[derive(Args)]
struct CorpusArchiveArgs {
    /// Dataset name from brokkr.toml
    #[arg(long, default_value = DEFAULT_DATASET)]
    dataset: String,
    /// PBF variant, selecting which archive to open (e.g. raw, indexed, altw).
    /// Required unless --file is given: the corpus contract pins one variant
    /// (bless accepts only a locations-generated archive), so a default of
    /// `raw` could never bless and failed every check against a locations
    /// corpus with a contract mismatch.
    #[arg(long, required_unless_present = "file")]
    variant: Option<String>,
    /// Commit short hash selecting which archive to open (default: current HEAD)
    #[arg(long)]
    commit: Option<String>,
    /// Explicit PMTiles path, skips dataset/variant/commit resolution
    #[arg(long)]
    file: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Check an archive against the committed baseline (exit 0 pass / 1 content mismatch / 2 archive refusal / 3 baseline trouble / 4 could not run)
    Check {
        #[command(flatten)]
        archive: CorpusArchiveArgs,
        /// Corpus directory (default: corpus/<dataset> under the repo root)
        #[arg(long)]
        corpus: Option<PathBuf>,
    },
    /// Re-derive the digest baseline from an archive (writes into the corpus dir)
    Bless {
        #[command(flatten)]
        archive: CorpusArchiveArgs,
        /// Corpus directory (default: corpus/<dataset> under the repo root)
        #[arg(long)]
        corpus: Option<PathBuf>,
        /// Replace an existing baseline (bless refuses to overwrite otherwise)
        #[arg(long)]
        rotate: bool,
        /// Digest mode for the baseline
        #[arg(long, value_enum, default_value = "leaves")]
        mode: corpus_gate::DigestMode,
    },
    /// Render the SVG corpus manifest for an archive
    #[command(name = "render-manifest")]
    RenderManifest {
        #[command(flatten)]
        archive: CorpusArchiveArgs,
        /// Corpus directory (default: corpus/<dataset> under the repo root)
        #[arg(long)]
        corpus: Option<PathBuf>,
        /// Style TOML for the SVG renderer
        #[arg(long)]
        style: Option<PathBuf>,
    },
    /// Render a single tile from an archive to SVG
    Render {
        #[command(flatten)]
        archive: CorpusArchiveArgs,
        #[arg(short = 'z', long)]
        z: u8,
        #[arg(short = 'x', long)]
        x: u32,
        #[arg(short = 'y', long)]
        y: u32,
        /// Comma-separated layer names to render
        #[arg(long)]
        layers: Option<String>,
        /// Style TOML for the SVG renderer
        #[arg(long)]
        style: Option<PathBuf>,
        /// Output file path
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
    /// Emit ocean ring geometry from an archive
    Rings {
        #[command(flatten)]
        archive: CorpusArchiveArgs,
        /// Output file path
        #[arg(short = 'o', long)]
        output: PathBuf,
    },
    /// Produce a mutated copy of an archive (calibration oracle)
    Mutate {
        #[command(flatten)]
        archive: CorpusArchiveArgs,
        /// Output path for the mutated archive. Default: a calibrand under
        /// data/corpus-calibrands/ that a routine `brokkr clean` clears.
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
        /// Mutation to apply
        #[arg(long, value_enum)]
        op: corpus_gate::MutationOp,
        /// Target tile z/x/y for ops that need one
        #[arg(long)]
        tile: Option<String>,
    },
    /// Diff two explicit tilegen archives (tier-3 attribution)
    #[command(
        long_about = "\
Diff two explicit tilegen output archives.

Both sides are explicit - there is no default baseline, ever. The CURRENT \
side comes from --commit/--file; the COMPARAND from --against-commit/--against \
(one is required, or clap refuses with a usage error).

Comparability is YOUR responsibility. regress reads no provenance contract by \
design - it is the attribution instrument, and cross-contract diffs (artifact-\
active vs computed, an intended config change) are legitimate uses. Read the \
provenance blocks via `brokkr pmtiles-inspect` first: comparing across \
variants or configs reports a six-figure diff on two correct builds.",
        group(
            ArgGroup::new("comparand")
                .args(["against", "against_commit"])
                .required(true)
                .multiple(false)
        ),
    )]
    Regress {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = DEFAULT_DATASET)]
        dataset: String,
        /// PBF variant of the CURRENT output (e.g. raw, indexed, altw)
        #[arg(long, default_value = DEFAULT_RAW_VARIANT)]
        variant: String,
        /// Commit short hash selecting the CURRENT output (default: current HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Explicit CURRENT PMTiles path, skips dataset/variant/commit resolution
        #[arg(long)]
        file: Option<String>,
        /// PBF variant of the COMPARAND output (defaults to raw; set it
        /// independently of --variant - a cross-variant diff is a legitimate use)
        #[arg(long, default_value = DEFAULT_RAW_VARIANT)]
        against_variant: String,
        /// Commit short hash selecting the COMPARAND output from data/tilegen/
        #[arg(long)]
        against_commit: Option<String>,
        /// Explicit COMPARAND PMTiles path
        #[arg(long)]
        against: Option<String>,
        /// Geometry tolerance in the layer's extent units (default 0)
        #[arg(long, default_value_t = 0)]
        tol: i32,
        /// tolerance_moved budget before exit 1 (default 0: any move fails)
        #[arg(long, default_value_t = 0)]
        max_moved: u64,
        /// Per-class example cap
        #[arg(long, default_value_t = gate::constants::DEFAULT_MAX_EXAMPLES)]
        max_examples: usize,
        /// Dump side-by-side overlay pairs for differing tiles to DIR, in report
        /// order, up to the overlay cap
        #[arg(long)]
        overlay: Option<PathBuf>,
        /// Cap on the number of overlay pairs emitted
        #[arg(long)]
        overlay_max: Option<usize>,
        /// Emit a machine-readable report to stdout
        #[arg(long)]
        json: bool,
    },
    /// Compare feature counts between two PMTiles archives
    #[command(name = "compare-tiles")]
    CompareTiles {
        /// First PMTiles file
        file_a: String,
        /// Second PMTiles file
        file_b: String,
        /// Sample size per zoom level
        #[arg(long)]
        sample: Option<usize>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::CompareTiles {
            file_a,
            file_b,
            sample,
        } => gate::compare_tiles::run(file_a, file_b, *sample),
        Command::Regress { .. } => operational(gate::regress::INCOMPLETE_EXIT, || {
            regress(cli.host.as_deref(), &cli.command)
        }),
        _ => operational(corpus_gate::INCOMPLETE_EXIT, || {
            corpus(cli.host.as_deref(), &cli.command)
        }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(DevError::ExitCode(code)) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            output::error(&e.to_string());
            ExitCode::FAILURE
        }
    }
}

/// Run a verdict-bearing command so that a failure to reach any verdict exits
/// `code` rather than the generic 1, which both commands reserve for "the
/// archive regressed" / "content mismatch". The error is printed here.
fn operational<F>(code: i32, f: F) -> Result<(), DevError>
where
    F: FnOnce() -> Result<(), DevError>,
{
    match f() {
        Err(e @ DevError::ExitCode(_)) => Err(e),
        Err(e) => {
            output::error(&e.to_string());
            Err(DevError::ExitCode(code))
        }
        Ok(()) => Ok(()),
    }
}

/// `regress` - resolve two explicit tilegen archives (CURRENT via
/// --commit/--file, COMPARAND via --against-commit/--against) and run the
/// two-archive semantic diff over them. Both sides are explicit: there is no
/// default baseline and no comparability gate.
fn regress(host: Option<&str>, cmd: &Command) -> Result<(), DevError> {
    let Command::Regress {
        dataset,
        variant,
        commit,
        file,
        against_variant,
        against_commit,
        against,
        tol,
        max_moved,
        max_examples,
        overlay,
        overlay_max,
        json,
    } = cmd
    else {
        unreachable!("dispatched on Command::Regress");
    };
    let paths = config::load(host)?;
    let store = ArchiveStore::new(paths.output_dir.as_path());
    let current = store.resolve(
        dataset,
        variant,
        commit.as_deref(),
        file.as_deref(),
        &paths.root,
    )?;
    // The comparand is one of the two --against* flags (clap's required
    // ArgGroup guarantees exactly one is present). An explicit path is checked
    // for existence; a commit resolves through the same resolver as the
    // current side, but with its OWN variant.
    let comparand = match against {
        Some(p) => {
            let path = PathBuf::from(p);
            if !path.exists() {
                return Err(DevError::Config(format!(
                    "comparand archive not found: {}",
                    path.display()
                )));
            }
            path
        }
        None => store.resolve(
            dataset,
            against_variant,
            against_commit.as_deref(),
            None,
            &paths.root,
        )?,
    };
    gate::regress::run(
        &current,
        &comparand,
        *tol,
        *max_moved,
        *max_examples,
        overlay.as_deref(),
        *overlay_max,
        *json,
    )
}

/// The corpus gate. Resolves the archive and corpus dir, runs the gate, prints
/// the report, and maps the verdict to the process exit code (0 pass / 1
/// content mismatch / 2 archive refusal / 3 baseline trouble).
#[allow(clippy::too_many_lines)]
fn corpus(host: Option<&str>, cmd: &Command) -> Result<(), DevError> {
    let paths = config::load(host)?;
    let build_root = paths.root.as_path();
    let store = ArchiveStore::new(paths.output_dir.as_path());

    // `variant` is `None` only alongside `--file` (clap's
    // `required_unless_present`), where the resolver ignores it.
    let resolve = |a: &CorpusArchiveArgs| -> Result<PathBuf, DevError> {
        store.resolve(
            &a.dataset,
            a.variant.as_deref().unwrap_or_default(),
            a.commit.as_deref(),
            a.file.as_deref(),
            build_root,
        )
    };
    // Corpus dir default: corpus/<dataset> under the repo root, where the
    // git-committed corpus lives. Overridable with --corpus.
    let corpus_dir = |a: &CorpusArchiveArgs, over: &Option<PathBuf>| -> PathBuf {
        over.clone()
            .unwrap_or_else(|| build_root.join("corpus").join(&a.dataset))
    };
    // `--style` default: derived from the corpus dir by the gate's own rule,
    // never from the build root independently.
    let style_default = corpus_gate::corpus_style_path;
    // The digest's external sort spills under project state, never /tmp.
    let sort_scratch = paths.corpus_sort_dir();

    match cmd {
        Command::Check { archive, corpus } => {
            let path = resolve(archive)?;
            let (outcome, report) =
                corpus_gate::check(&path, &corpus_dir(archive, corpus), &sort_scratch)?;
            emit_corpus(outcome, &report)
        }
        Command::Bless {
            archive,
            corpus,
            rotate,
            mode,
        } => {
            let path = resolve(archive)?;
            let (outcome, report) = corpus_gate::bless(
                &path,
                &corpus_dir(archive, corpus),
                *mode,
                *rotate,
                &sort_scratch,
            )?;
            emit_corpus(outcome, &report)
        }
        Command::RenderManifest {
            archive,
            corpus,
            style,
        } => {
            let path = resolve(archive)?;
            let dir = corpus_dir(archive, corpus);
            let style_path = style.clone().unwrap_or_else(|| style_default(&dir));
            let (outcome, report) =
                corpus_gate::render_manifest(&path, &dir, &style_path, &sort_scratch)?;
            emit_corpus(outcome, &report)
        }
        Command::Render {
            archive,
            z,
            x,
            y,
            layers,
            style,
            output,
        } => {
            let path = resolve(archive)?;
            // `render` takes no `--corpus`; the default corpus dir stands in.
            let style_path = style
                .clone()
                .unwrap_or_else(|| style_default(&corpus_dir(archive, &None)));
            let layer_list: Option<Vec<String>> = layers
                .as_ref()
                .map(|l| l.split(',').map(str::to_owned).collect());
            corpus_gate::render_tile(
                &path,
                *z,
                *x,
                *y,
                &style_path,
                layer_list.as_deref(),
                output.as_deref(),
            )?;
            Ok(())
        }
        Command::Rings { archive, output } => {
            let path = resolve(archive)?;
            corpus_gate::rings(&path, output)?;
            Ok(())
        }
        Command::Mutate {
            archive,
            output,
            op,
            tile,
        } => {
            let path = resolve(archive)?;
            // Default `-o` to a calibrand under data/corpus-calibrands/; an
            // explicit `-o` is the user's file.
            let out_path = match output {
                Some(o) => o.clone(),
                None => {
                    let dir = paths.corpus_calibrands();
                    std::fs::create_dir_all(&dir)?;
                    // With `--file` and no `--variant`, name the calibrand
                    // after the input file instead.
                    let stem = match &archive.variant {
                        Some(v) => format!("{}-{v}", archive.dataset),
                        None => path.file_stem().map_or_else(
                            || "file".to_owned(),
                            |s| s.to_string_lossy().into_owned(),
                        ),
                    };
                    dir.join(format!("{stem}-{}.pmtiles", op.as_str()))
                }
            };
            let target = tile.as_deref().map(parse_tile).transpose()?;
            corpus_gate::mutate::mutate(&path, &out_path, target, *op)?;
            output::result_msg(&format!("mutated -> {}", output::path(&out_path)));
            Ok(())
        }
        Command::Regress { .. } | Command::CompareTiles { .. } => {
            unreachable!("dispatched separately")
        }
    }
}

/// Print a corpus report and translate the verdict into the process exit code.
fn emit_corpus(
    outcome: corpus_gate::Outcome,
    report: &corpus_gate::CheckReport,
) -> Result<(), DevError> {
    use corpus_gate::Outcome;
    for w in &report.warnings {
        output::warn(w);
    }
    for d in &report.contract_diffs {
        output::run_msg(&format!("contract {d}"));
    }
    if !report.message.is_empty() {
        for line in report.message.lines() {
            output::run_msg(line);
        }
    }
    if report.changed > 0 {
        let (noun, plural) = match report.changed_unit {
            Some(corpus_gate::ChangedUnit::Buckets) => ("bucket", "s"),
            Some(corpus_gate::ChangedUnit::LeafRuns) => ("leaf run", "s"),
            None => ("change", "s"),
        };
        output::run_msg(&format!(
            "{} changed {noun}{}",
            report.changed,
            if report.changed == 1 { "" } else { plural }
        ));
    }
    match outcome {
        Outcome::Pass => {
            output::result_msg("corpus: pass");
            Ok(())
        }
        other => Err(DevError::ExitCode(other.exit_code())),
    }
}

/// Parse a `z/x/y` mutate target.
fn parse_tile(s: &str) -> Result<(u8, u32, u32), DevError> {
    let mut p = s.split('/');
    let bad = || DevError::Config(format!("invalid tile spec (want z/x/y): {s}"));
    let z: u8 = p.next().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
    let x: u32 = p.next().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
    let y: u32 = p.next().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
    if p.next().is_some() {
        return Err(bad());
    }
    Ok((z, x, y))
}

#[cfg(test)]
mod operational_tests {
    use super::{DevError, operational};

    #[test]
    fn a_non_verdict_error_exits_the_given_code() {
        let r = operational(4, || Err(DevError::Config("lock held".into())));
        assert!(matches!(r, Err(DevError::ExitCode(4))));
    }

    #[test]
    fn verdicts_pass_through() {
        assert!(matches!(
            operational(4, || Err(DevError::ExitCode(1))),
            Err(DevError::ExitCode(1))
        ));
        assert!(operational(4, || Ok(())).is_ok());
    }

    #[test]
    fn tile_specs_parse_and_refuse() {
        assert_eq!(super::parse_tile("3/2/1").ok(), Some((3, 2, 1)));
        assert!(super::parse_tile("3/2").is_err());
        assert!(super::parse_tile("3/2/1/0").is_err());
    }
}
