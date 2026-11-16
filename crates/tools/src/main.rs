//! `elivagar-tools` - one-time installation of the competitor toolchains the
//! comparison benches run, and the inputs only they need.
//!
//! ```text
//! brokkr run elivagar-tools -- planetiler    # pinned JDK + Planetiler jar
//! brokkr run elivagar-tools -- tilemaker     # pinned tilemaker build + shortbread config,
//!                                            # EPSG:4326 ocean, resolved config
//! brokkr run elivagar-tools -- ocean-4326    # the EPSG:4326 ocean alone
//! ```
//!
//! Ported from brokkr's `src/tools.rs` and the 4326 half of its elivagar
//! `download_ocean.rs`. Everything lands under the host's data dir
//! (`elivagar_devkit::config`), where `benches/compare/*.lua` look for it.
//! Measurement is the benches' (`brokkr bench planetiler`, `brokkr bench
//! tilemaker`); nothing here times anything.

mod fetch;
mod ocean;
mod planetiler;
mod tilemaker;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use elivagar_devkit::{DevError, config};

#[derive(Parser)]
#[command(
    name = "elivagar-tools",
    about = "install the Planetiler and Tilemaker comparison toolchains"
)]
struct Cli {
    /// Host section of brokkr.toml to resolve the data dir from (default: the
    /// kernel hostname)
    #[arg(long, global = true)]
    host: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The pinned Temurin JDK and Planetiler release jar
    Planetiler,
    /// The pinned tilemaker build, the pinned shortbread-tilemaker config,
    /// the EPSG:4326 ocean and the config with absolute shapefile paths
    Tilemaker,
    /// The EPSG:4326 ocean shapefiles (full downloaded, simplified
    /// reprojected with ogr2ogr)
    #[command(name = "ocean-4326")]
    Ocean4326,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = config::load(cli.host.as_deref()).and_then(|paths| {
        let data = paths.data_dir.as_path();
        match cli.command {
            Command::Planetiler => planetiler::install(data),
            Command::Tilemaker => tilemaker::install(data),
            Command::Ocean4326 => ocean::ensure_4326(data),
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(DevError::ExitCode(code)) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            for line in e.to_string().lines() {
                println!("[error]   {line}");
            }
            ExitCode::FAILURE
        }
    }
}
