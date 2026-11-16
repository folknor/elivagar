//! `elivagar-devkit` - the plain (unmeasured) tilegen run and the world ocean
//! artifact build, in-tree.
//!
//! Ported from brokkr's `brokkr tilegen` (run mode) and `brokkr ocean-build`.
//! brokkr builds the product and hands its path in `BROKKR_WITH_EXE`:
//!
//! ```text
//! brokkr run --with elivagar elivagar-devkit -- tilegen --dataset denmark --variant locations
//! brokkr run --with elivagar elivagar-devkit -- ocean-build
//! ```
//!
//! Measured modes stay in brokkr: brokkr benches only measure.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};

use elivagar_devkit::archive_store::{ArchiveKey, ArchiveStore};
use elivagar_devkit::config::{self, Paths};
use elivagar_devkit::tilegen::{self, DEFAULT_TILEGEN};
use elivagar_devkit::{DevError, git};

#[derive(Parser)]
#[command(
    name = "elivagar-devkit",
    about = "tilegen and ocean-build for elivagar"
)]
struct Cli {
    /// Host section of brokkr.toml to resolve paths from (default: the kernel
    /// hostname)
    #[arg(long, global = true)]
    host: Option<String>,

    /// elivagar executable (otherwise BROKKR_WITH_EXE, set by `brokkr run
    /// --with elivagar`). Never builds one.
    #[arg(long, global = true)]
    elivagar: Option<PathBuf>,

    /// Validate config and path resolution and print the argv, without running
    #[arg(long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the full pipeline under [<host>.tilegen.default] and admit the
    /// archive into the store as <dataset>-<variant>-<commit>.pmtiles
    Tilegen {
        /// Dataset name from brokkr.toml
        #[arg(long, default_value = "denmark")]
        dataset: String,
        /// PBF variant (e.g. raw, indexed, locations)
        #[arg(long, default_value = "raw")]
        variant: String,
        /// Resume from a checkpoint of a previous run
        #[arg(long, value_parser = ["ocean", "sort", "assemble"])]
        skip_to: Option<String>,
    },
    /// Build the world ocean artifact from [<host>.tilegen.default].ocean
    #[command(name = "ocean-build")]
    OceanBuild,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Tilegen {
            dataset,
            variant,
            skip_to,
        } => run_tilegen(&cli, dataset, variant, skip_to.as_deref()),
        Command::OceanBuild => run_ocean_build(&cli),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(DevError::ExitCode(code)) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            emit("[error]   ", &e.to_string());
            ExitCode::FAILURE
        }
    }
}

/// brokkr's tagged line format: the tag padded to a 10-column field, stdout.
fn emit(prefix: &str, msg: &str) {
    use std::io::Write;
    let mut out = String::new();
    for line in msg.lines() {
        out.push_str(prefix);
        out.push_str(line);
        out.push('\n');
    }
    let mut stdout = std::io::stdout().lock();
    drop(stdout.write_all(out.as_bytes()));
    drop(stdout.flush());
}

fn run_msg(msg: &str) {
    emit("[run]     ", msg);
}

fn dry_run_msg(msg: &str) {
    emit("[dry-run] ", msg);
}

fn elivagar_exe(cli: &Cli) -> Result<PathBuf, DevError> {
    cli.elivagar
        .clone()
        .or_else(|| std::env::var_os("BROKKR_WITH_EXE").map(PathBuf::from))
        .ok_or_else(|| {
            DevError::Config(
                "supply --elivagar PATH or BROKKR_WITH_EXE (brokkr run --with elivagar)".into(),
            )
        })
}

fn run_tilegen(
    cli: &Cli,
    dataset: &str,
    variant: &str,
    skip_to: Option<&str>,
) -> Result<(), DevError> {
    let paths = config::load(cli.host.as_deref())?;
    // Block first, as brokkr resolved it before the input.
    let tg = tilegen::resolve_tilegen(&paths, DEFAULT_TILEGEN)?;
    let pbf = tilegen::resolve_pbf(&paths, dataset, variant)?;
    let args = tilegen::run_args(&paths, &tg, &pbf, skip_to)?;
    // Refused before the run, not after it: a key brokkr could not name would
    // otherwise leave a finished archive stranded in scratch.
    ArchiveKey::new(dataset, variant, "0")?;

    if cli.dry_run {
        dry_run_msg(&format!("elivagar {}", args.join(" ")));
        dry_run_msg(&format!(
            "output: {}",
            ArchiveStore::new(&paths.output_dir)
                .path_for(&ArchiveKey::new(
                    dataset,
                    variant,
                    git::head(&paths.root)?.short()
                )?)
                .display()
        ));
        dry_run_msg("ok");
        return Ok(());
    }

    let exe = elivagar_exe(cli)?;
    prepare_tilegen(&paths)?;
    run_child(&exe, &args, &paths.root)?;
    admit(&paths, dataset, variant)
}

/// Clear the scratch output and recreate elivagar's `--tmp-dir`. elivagar
/// empties its tmp dir at start anyway; when the tmp dir is the scratch dir
/// itself it is left to elivagar, as brokkr did.
fn prepare_tilegen(paths: &Paths) -> Result<(), DevError> {
    std::fs::create_dir_all(&paths.scratch_dir)?;
    let tmp_dir = paths.tilegen_tmp();
    if tmp_dir != paths.scratch_dir {
        match std::fs::remove_dir_all(&tmp_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    std::fs::create_dir_all(&tmp_dir)?;
    match std::fs::remove_file(paths.tilegen_scratch_output()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Run the child with inherited stdio, printing the argv and the wall as
/// brokkr's run mode does. A nonzero exit is passed through; a signal exits
/// 128 plus its number.
fn run_child(exe: &Path, args: &[String], cwd: &Path) -> Result<(), DevError> {
    use std::os::unix::process::ExitStatusExt;

    run_msg(&format!("{} {}", exe.display(), args.join(" ")));
    let start = Instant::now();
    let status = std::process::Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| DevError::Config(format!("failed to run {}: {e}", exe.display())))?;
    if !status.success() {
        let code = status
            .code()
            .or_else(|| status.signal().map(|s| 128 + s))
            .unwrap_or(1);
        return Err(DevError::ExitCode(code));
    }
    run_msg(&format!("elapsed={}ms", start.elapsed().as_millis()));
    Ok(())
}

/// Move the scratch output into the store under HEAD's short hash. Every way
/// of ending without a durable archive is an error, and the scratch output is
/// left in place on each, so it can be moved by hand.
fn admit(paths: &Paths, dataset: &str, variant: &str) -> Result<(), DevError> {
    let scratch = paths.tilegen_scratch_output();
    // A git failure is fatal rather than a `-unknown` suffix: no resolver
    // constructs that name, so the archive would be unreachable.
    let commit = git::head(&paths.root)
        .map_err(|e| {
            DevError::Io(std::io::Error::other(format!(
                "cannot name the tilegen archive: git commit of {} unavailable ({e}); \
                 output left in {}",
                paths.root.display(),
                paths.scratch_dir.display(),
            )))
        })?
        .short();
    let store = ArchiveStore::new(&paths.output_dir);
    let key = ArchiveKey::new(dataset, variant, commit)?;
    let tmp_dir = paths.tilegen_tmp();
    let admitted = store.admit(&scratch, &key, &[paths.scratch_dir.as_path(), &tmp_dir])?;
    run_msg(&format!("output: {}", admitted.path.display()));
    if admitted.replaced {
        run_msg(&format!(
            "replaced the earlier {variant} archive of {}; the store keeps one per revision, \
             so copy an archive elsewhere and address it with --file to keep both",
            key.commit()
        ));
    }
    Ok(())
}

/// `ocean-build`: one shot per shapefile release. The invocation is derived
/// entirely from `[<host>.tilegen.default].ocean`, the same block tilegen
/// reads, so builder and consumer cannot drift on spelling. Rotating the
/// artifact is an output-changing event: the next corpus check refuses on the
/// artifact key until the corpus is re-blessed.
fn run_ocean_build(cli: &Cli) -> Result<(), DevError> {
    let paths = config::load(cli.host.as_deref())?;
    let tg = tilegen::resolve_tilegen(&paths, DEFAULT_TILEGEN)?;
    let (args, output_path) = tilegen::ocean_build_args(&paths, &tg)?;
    if cli.dry_run {
        dry_run_msg(&format!("elivagar {}", args.join(" ")));
        dry_run_msg(&format!("output: {}", output_path.display()));
        dry_run_msg("ok");
        return Ok(());
    }
    let exe = elivagar_exe(cli)?;
    run_child(&exe, &args, &paths.root)
}
