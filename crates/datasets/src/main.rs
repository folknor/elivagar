//! Acquire the inputs elivagar's dev hosts feed tilegen.
//!
//! Run as `brokkr run elivagar-datasets -- download-ocean`,
//! `-- download-natural-earth`, `-- download <region>` or
//! `-- register <dataset> <variant> <file>`. brokkr holds the machine lock
//! for the whole run; this program never calls brokkr. The shapefile
//! commands are ported from brokkr's elivagar `download-ocean` and
//! `download-natural-earth`, same URLs, pins, destinations and completeness
//! rules; `download` replaces brokkr's generic `download <region>` (see
//! `download.rs` for what it no longer does) and derives the indexed and
//! locations variants in-process through pbfhogg's library. The data dir comes from the
//! host table via `elivagar_devkit::config`, so it matches what tilegen
//! resolves.

mod download;
mod fetch;
mod natural_earth;
mod ocean;
mod pbf;
mod registry;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "elivagar-datasets",
    about = "Download the ocean and Natural Earth shapefiles into the host's data dir"
)]
struct Cli {
    /// Config host section (otherwise the kernel's hostname)
    #[arg(long, global = true)]
    host: Option<String>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Download and extract the EPSG:3857 water polygons, full resolution
    /// and simplified (osmdata.openstreetmap.de, unpinned: regenerated daily)
    DownloadOcean,
    /// Download the pinned Natural Earth low-zoom layers into
    /// <data>/natural_earth/
    DownloadNaturalEarth,
    /// Download a Geofabrik extract (an alias such as denmark, or a path
    /// such as europe/france), derive its indexed and locations variants
    /// with pbfhogg, and append all three entries to
    /// brokkr.d/hosts/<host>.toml
    Download {
        region: String,
        /// Register the raw extract alone, skipping the pbfhogg derivations
        #[arg(long)]
        no_derive: bool,
    },
    /// Pin a hand-made variant of a registered dataset: hash FILE in the
    /// data dir and append its entry to the host file
    Register {
        dataset: String,
        variant: String,
        file: String,
    },
}

fn execute(cli: &Cli) -> Result<(), String> {
    let paths = elivagar_devkit::config::load(cli.host.as_deref()).map_err(|e| e.to_string())?;
    let tools = || {
        let path = std::env::var_os("PATH");
        fetch::System::find(path.as_deref())
    };
    match &cli.command {
        Action::DownloadOcean => ocean::run(&paths.data_dir, &tools()?),
        Action::DownloadNaturalEarth => natural_earth::run(&paths.data_dir, &tools()?),
        Action::Download { region, no_derive } => download::run(
            &paths,
            &tools()?,
            region,
            download::Day::today()?,
            !no_derive,
        ),
        Action::Register {
            dataset,
            variant,
            file,
        } => registry::register(&paths, dataset, variant, file),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("elivagar-datasets: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Unit-test scratch directories under the workspace's gitignored target
/// dir, never `/tmp`. `CARGO_TARGET_TMPDIR` is set only for integration
/// tests and benches, so a bin's unit tests derive the path from the
/// manifest dir instead. Each (module, name) pair must be unique: the directory is
/// cleared before it is handed back.
#[cfg(test)]
pub(crate) mod test_scratch {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static TAKEN: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());

    pub(crate) fn scratch(module: &str, name: &str) -> PathBuf {
        let fresh = TAKEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((module.to_owned(), name.to_owned()));
        assert!(
            fresh,
            "scratch name {name:?} used twice in module {module:?}"
        );
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/elivagar-datasets")
            .join(module)
            .join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .unwrap_or_else(|e| panic!("could not clear scratch {}: {e}", dir.display()));
        }
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("could not create scratch {}: {e}", dir.display()));
        dir
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_both_subcommands() {
        Cli::command().debug_assert();
        assert!(matches!(
            Cli::try_parse_from(["x", "download-ocean"])
                .unwrap()
                .command,
            Action::DownloadOcean
        ));
        let parsed = Cli::try_parse_from(["x", "download-natural-earth", "--host", "h"]).unwrap();
        assert!(matches!(parsed.command, Action::DownloadNaturalEarth));
        assert_eq!(parsed.host.as_deref(), Some("h"));
        assert!(Cli::try_parse_from(["x"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["x", "download", "europe/denmark"]).unwrap().command,
            Action::Download { region, no_derive: false } if region == "europe/denmark"
        ));
        assert!(matches!(
            Cli::try_parse_from(["x", "download", "malta", "--no-derive"])
                .unwrap()
                .command,
            Action::Download {
                no_derive: true,
                ..
            }
        ));
        assert!(Cli::try_parse_from(["x", "download"]).is_err());
        assert!(Cli::try_parse_from(["x", "download", "denmark", "--osc-seq", "1"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["x", "register", "denmark", "indexed", "f.osm.pbf"])
                .unwrap()
                .command,
            Action::Register { .. }
        ));
    }
}
