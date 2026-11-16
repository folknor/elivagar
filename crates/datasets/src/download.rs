//! `download REGION`: fetch a Geofabrik extract into the host's data dir,
//! derive its `indexed` and `locations` variants with pbfhogg's library,
//! and register all three.
//!
//! Ported from brokkr's removed generic `download`, cut to what elivagar
//! consumes. Source resolution follows pbfhogg-datasets: an alias or a
//! Geofabrik path, whose last segment is the dataset key, fetched from
//! `https://download.geofabrik.de/<path>-latest.osm.pbf`. The file is named
//! `<key>-<yyyymmdd>-seq<N>.osm.pbf` after the replication sequence in its
//! own header, and the variants `<raw stem>-with-indexdata.osm.pbf` and
//! `<raw stem>-locations-prepass.osm.pbf`, matching the names already
//! registered on every host. `crate::pbf` says how each variant is made.
//!
//! The run is all or nothing. Every table goes into the host file in one
//! append, verified by resolving every variant it names, and on any failure
//! (transfer, header, derivation, hashing, registration) the host file is
//! left byte-identical and every file this run created is removed, so a
//! retry starts clean rather than being refused by its own leftovers.
//! `--no-derive` registers `raw` alone; `register` pins variants made by
//! hand afterwards.
//!
//! What is deliberately not here:
//! - OSC diffs (`--osc-seq`). elivagar has no consumer of them; no host
//!   registers one.
//! - The planet. Its header carries the minutely sequence, not the daily
//!   one hosts record, and elivagar's planet input is a pbfhogg product
//!   anyway (bygg's host file says how it was made).
//! - Editing an existing dataset. A registered key is refused before any
//!   transfer; the tool only ever appends new tables.

use std::path::{Path, PathBuf};

use elivagar_devkit::config::Paths;

use crate::fetch::{Tools, note};
use crate::registry;

const GEOFABRIK: &str = "https://download.geofabrik.de";

/// Short names for the Geofabrik regions elivagar's hosts use, as
/// pbfhogg-datasets spells them.
const ALIASES: &[(&str, &str)] = &[
    ("malta", "europe/malta"),
    (
        "greater-london",
        "europe/united-kingdom/england/greater-london",
    ),
    ("switzerland", "europe/switzerland"),
    ("norway", "europe/norway"),
    ("japan", "asia/japan"),
    ("denmark", "europe/denmark"),
    ("germany", "europe/germany"),
    ("north-america", "north-america"),
    ("europe", "europe"),
];

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Region {
    pub(crate) key: String,
    /// Geofabrik path, e.g. `europe/denmark`.
    pub(crate) path: String,
}

impl Region {
    fn pbf_url(&self) -> String {
        format!("{GEOFABRIK}/{}-latest.osm.pbf", self.path)
    }

    fn stream_url(&self) -> String {
        format!("{GEOFABRIK}/{}-updates", self.path)
    }
}

pub(crate) fn resolve(region: &str) -> Result<Region, String> {
    if region == "planet" {
        return Err(
            "the planet is not downloaded here: elivagar's planet input is a pbfhogg \
             locations-prepass product; fetch it with pbfhogg-datasets and register the \
             derived file by hand"
                .into(),
        );
    }
    let path = if let Some((_, path)) = ALIASES.iter().find(|(n, _)| *n == region) {
        (*path).to_owned()
    } else if region.contains('/') {
        region.trim_matches('/').to_owned()
    } else {
        let names: Vec<&str> = ALIASES.iter().map(|(n, _)| *n).collect();
        return Err(format!(
            "unknown region '{region}': use a Geofabrik path (e.g. europe/france) or one of: {}",
            names.join(", ")
        ));
    };
    if path.split('/').any(|s| !registry::is_bare_key(s)) {
        return Err(format!(
            "region '{region}': path segments must be letters, digits, '_' and '-' only"
        ));
    }
    let key = path.rsplit('/').next().unwrap_or(&path).to_owned();
    Ok(Region { key, path })
}

/// A UTC calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Day {
    pub(crate) year: i64,
    pub(crate) month: u32,
    pub(crate) day: u32,
}

impl Day {
    pub(crate) fn today() -> Result<Self, String> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| format!("system clock: {e}"))?
            .as_secs();
        Ok(Self::from_days(
            i64::try_from(secs / 86_400).map_err(|e| e.to_string())?,
        ))
    }

    /// Days since 1970-01-01 to a proleptic Gregorian date (Hinnant's
    /// `civil_from_days`).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) fn from_days(days: i64) -> Self {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self { year, month, day }
    }

    fn compact(self) -> String {
        format!("{:04}{:02}{:02}", self.year, self.month, self.day)
    }

    fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Refuse when the data dir already holds a file this run could produce:
/// the dated name or any name extending it (`-seq<N>`, the derived
/// variants). Such a file is either registered under another key or left
/// by a run killed before it could clean up, and either way is not this
/// run's to overwrite or remove.
fn refuse_existing(data_dir: &Path, stem: &str) -> Result<(), String> {
    let seq_prefix = format!("{stem}-");
    let exact = format!("{stem}.osm.pbf");
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", data_dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == exact || name.starts_with(&seq_prefix) {
            return Err(format!(
                "{} already exists; register or remove it first",
                data_dir.join(&*name).display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn run(
    paths: &Paths,
    tools: &dyn Tools,
    region: &str,
    today: Day,
    derive: bool,
) -> Result<(), String> {
    let region = resolve(region)?;
    let key = region.key.as_str();
    if registry::dataset(paths, key).is_some() {
        return Err(format!(
            "dataset '{key}' is already registered for host '{}'; this tool never edits an \
             existing dataset (use `register` for a new variant)",
            paths.hostname
        ));
    }
    let host_file = registry::host_file(paths)?;
    let data_dir = &paths.data_dir;
    let stem = format!("{key}-{}", today.compact());
    refuse_existing(data_dir, &stem)?;
    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;

    let job = Job {
        paths,
        region: &region,
        today,
        stem,
        derive,
        host_file: &host_file,
    };
    let mut created = Vec::new();
    match acquire(&job, tools, &mut created) {
        Ok(text) => {
            note(&format!("registered in {}:\n{text}", host_file.display()));
            Ok(())
        }
        Err(mut error) => {
            // Newest first: a derived file before the one it was made from.
            for path in created.iter().rev().filter(|p| p.exists()) {
                match crate::fetch::remove_if_present(path) {
                    Ok(()) => error.push_str(&format!("\n  removed {}", path.display())),
                    Err(e) => error.push_str(&format!("\n  could not remove {e}")),
                }
            }
            Err(error)
        }
    }
}

/// One `download` run's fixed inputs.
struct Job<'a> {
    paths: &'a Paths,
    region: &'a Region,
    today: Day,
    /// `<key>-<yyyymmdd>`.
    stem: String,
    derive: bool,
    host_file: &'a Path,
}

/// Record `path` as this run's to remove on failure. It must not exist
/// yet: a file this run did not create is never removed.
fn claim(created: &mut Vec<PathBuf>, path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "{} already exists; register or remove it first",
            path.display()
        ));
    }
    created.push(path.to_path_buf());
    Ok(())
}

/// Fetch, name, derive, hash and register; returns the appended text. Every
/// file it creates is recorded in `created` before it is created.
fn acquire(job: &Job<'_>, tools: &dyn Tools, created: &mut Vec<PathBuf>) -> Result<String, String> {
    let data_dir = &job.paths.data_dir;
    let key = job.region.key.as_str();
    let fetched = data_dir.join(format!("{}.osm.pbf", job.stem));
    claim(created, &fetched)?;
    let url = job.region.pbf_url();
    note(&format!("GET {url} -> {}", fetched.display()));
    crate::fetch::download(tools, &url, &fetched, None)?;
    let (name, raw, seq) = name_by_seq(job.region, &fetched, data_dir, &job.stem, created)?;

    let mut files = vec![("raw", name)];
    if job.derive {
        let raw_stem = files[0].1.trim_end_matches(".osm.pbf").to_owned();
        let indexed_name = format!("{raw_stem}-with-indexdata.osm.pbf");
        let indexed = data_dir.join(&indexed_name);
        claim(created, &indexed)?;
        note(&format!(
            "indexing {} -> {}",
            raw.display(),
            indexed.display()
        ));
        crate::pbf::index(&raw, &indexed)?;

        let locations_name = format!("{raw_stem}-locations-prepass.osm.pbf");
        let locations = data_dir.join(&locations_name);
        claim(created, &locations)?;
        note(&format!(
            "adding locations to ways (external index, prepass, zstd:1) {} -> {}",
            indexed.display(),
            locations.display()
        ));
        crate::pbf::add_locations(&indexed, &locations, &job.paths.scratch_dir)?;
        files.push(("indexed", indexed_name));
        files.push(("locations", locations_name));
    }

    let host = job.paths.hostname.as_str();
    let mut text = registry::dataset_table(host, key, &job.today.iso());
    for (variant, file) in &files {
        let path = data_dir.join(file);
        note(&format!("hashing {}", path.display()));
        let xxhash = elivagar_devkit::tilegen::file_digest(&path).map_err(|e| e.to_string())?;
        text.push('\n');
        text.push_str(&registry::pbf_table(
            host,
            key,
            variant,
            &registry::Entry {
                file,
                xxhash: &xxhash,
                // A derived file describes the same snapshot as raw.
                seq,
            },
        ));
    }
    let variants: Vec<&str> = files.iter().map(|(variant, _)| *variant).collect();
    registry::append_and_verify(job.paths, job.host_file, &text, key, &variants)?;
    Ok(text)
}

/// Read the replication sequence from the fetched file's header and rename
/// it to its `-seq<N>` name. A file without a header is not a PBF and is
/// refused (the caller removes it); a header naming another stream (or
/// none) keeps the dated name and registers without `seq`, since a wrong
/// seq is worse than none.
fn name_by_seq(
    region: &Region,
    fetched: &Path,
    data_dir: &Path,
    stem: &str,
    created: &mut Vec<PathBuf>,
) -> Result<(String, PathBuf, Option<u64>), String> {
    let header = crate::pbf::replication(fetched)?;
    let stream = region.stream_url();
    match (header.seq, header.base_url.as_deref()) {
        (Some(seq), Some(base)) if base.trim_end_matches('/') == stream => {
            let name = format!("{stem}-seq{seq}.osm.pbf");
            let path = data_dir.join(&name);
            claim(created, &path)?;
            std::fs::rename(fetched, &path)
                .map_err(|e| format!("{} -> {}: {e}", fetched.display(), path.display()))?;
            Ok((name, path, Some(seq)))
        }
        (seq, base) => {
            note(&format!(
                "header replication fields (seq {seq:?}, url {base:?}) do not name {stream}; \
                 registering without seq: add it by hand once known"
            ));
            Ok((format!("{stem}.osm.pbf"), fetched.to_path_buf(), None))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fetch::fake::Fake;
    use crate::registry::fixture;
    use crate::test_scratch::scratch;

    const DAY: Day = Day {
        year: 2026,
        month: 11,
        day: 16,
    };

    #[test]
    fn regions_resolve_like_pbfhogg_datasets() {
        let r = resolve("denmark").unwrap();
        assert_eq!(
            (r.key.as_str(), r.path.as_str()),
            ("denmark", "europe/denmark")
        );
        assert_eq!(
            r.pbf_url(),
            "https://download.geofabrik.de/europe/denmark-latest.osm.pbf"
        );
        let r = resolve("/asia/japan/kanto/").unwrap();
        assert_eq!(
            (r.key.as_str(), r.path.as_str()),
            ("kanto", "asia/japan/kanto")
        );
        for bad in ["planet", "atlantis", "a/b c", "a//b", "../x"] {
            assert!(resolve(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn civil_dates() {
        assert_eq!(Day::from_days(0).iso(), "1970-01-01");
        assert_eq!(Day::from_days(11_016).iso(), "2000-02-29");
        assert_eq!(Day::from_days(20_773).compact(), "20261116");
    }

    const DENMARK_UPDATES: &str = "https://download.geofabrik.de/europe/denmark-updates/";

    fn digest(path: &Path) -> String {
        elivagar_devkit::tilegen::file_digest(path).unwrap()
    }

    /// The data dir's entries, the host's scratch dir excepted when empty.
    fn data_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.join("data"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| {
                n != "scratch"
                    || std::fs::read_dir(dir.join("data/scratch"))
                        .unwrap()
                        .next()
                        .is_some()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn download_derives_and_registers_all_three_variants() {
        let dir = scratch("download", "fresh");
        let host = fixture::project(&dir, true);
        let paths = fixture::paths(&dir);
        let pbf = crate::pbf::build::pbf(Some(4704), Some(DENMARK_UPDATES), false);
        let fake = Fake::new(&pbf, &[]);
        run(&paths, &fake, "denmark", DAY, true).unwrap();
        assert_eq!(
            *fake.urls.borrow(),
            ["https://download.geofabrik.de/europe/denmark-latest.osm.pbf"]
        );
        let raw = dir.join("data/denmark-20261116-seq4704.osm.pbf");
        let indexed = dir.join("data/denmark-20261116-seq4704-with-indexdata.osm.pbf");
        let locations = dir.join("data/denmark-20261116-seq4704-locations-prepass.osm.pbf");
        assert_eq!(std::fs::read(&raw).unwrap(), pbf);
        assert_eq!(
            data_entries(&dir),
            [
                "denmark-20261116-seq4704-locations-prepass.osm.pbf",
                "denmark-20261116-seq4704-with-indexdata.osm.pbf",
                "denmark-20261116-seq4704.osm.pbf",
            ]
        );
        let header = pbfhogg::ElementReader::from_path(&locations).unwrap();
        assert!(header.header().has_locations_on_ways());
        assert!(header.header().has_way_members_v1());

        let after = std::fs::read_to_string(&host).unwrap();
        assert_eq!(
            after.strip_prefix(fixture::HOST_HEAD).unwrap(),
            format!(
                "\n\n[h.datasets.denmark]\norigin = \"Geofabrik\"\ndownload_date = \"2026-11-16\"\n\
                 \n[h.datasets.denmark.pbf.raw]\nfile = \"denmark-20261116-seq4704.osm.pbf\"\n\
                 xxhash = \"{}\"\nseq = 4704\n\
                 \n[h.datasets.denmark.pbf.indexed]\n\
                 file = \"denmark-20261116-seq4704-with-indexdata.osm.pbf\"\n\
                 xxhash = \"{}\"\nseq = 4704\n\
                 \n[h.datasets.denmark.pbf.locations]\n\
                 file = \"denmark-20261116-seq4704-locations-prepass.osm.pbf\"\n\
                 xxhash = \"{}\"\nseq = 4704\n",
                digest(&raw),
                digest(&indexed),
                digest(&locations),
            )
        );

        // Registered now: refused before any transfer, files untouched.
        let paths = fixture::paths(&dir);
        let err = run(&paths, &fake, "denmark", DAY, true).unwrap_err();
        assert!(err.contains("already registered"), "{err}");
        assert_eq!(fake.fetches.get(), 1);
        assert_eq!(std::fs::read_to_string(&host).unwrap(), after);
        assert_eq!(data_entries(&dir).len(), 3);
    }

    #[test]
    fn no_derive_registers_raw_alone_and_a_foreign_header_without_seq() {
        let dir = scratch("download", "noseq");
        let host = fixture::project(&dir, true);
        let paths = fixture::paths(&dir);
        let pbf = crate::pbf::build::pbf(Some(9), Some("https://elsewhere/"), false);
        run(&paths, &Fake::new(&pbf, &[]), "europe/malta", DAY, false).unwrap();
        assert_eq!(data_entries(&dir), ["malta-20261116.osm.pbf"]);
        let after = std::fs::read_to_string(&host).unwrap();
        assert!(after.contains("[h.datasets.malta.pbf.raw]\nfile = \"malta-20261116.osm.pbf\"\n"));
        assert!(!after.contains("seq ="));
        assert!(!after.contains("indexed"));
    }

    #[test]
    fn a_failed_derivation_removes_every_file_and_leaves_the_host_file() {
        let dir = scratch("download", "derive-fails");
        let host = fixture::project(&dir, true);
        let paths = fixture::paths(&dir);
        // cat passes it; add-locations-to-ways refuses the unresolvable ref.
        let pbf = crate::pbf::build::pbf(Some(4704), Some(DENMARK_UPDATES), true);
        let err = run(&paths, &Fake::new(&pbf, &[]), "denmark", DAY, true).unwrap_err();
        assert!(err.contains("add-locations-to-ways"), "{err}");
        assert!(err.contains("removed"), "{err}");
        assert_eq!(data_entries(&dir), Vec::<String>::new());
        assert_eq!(std::fs::read_to_string(&host).unwrap(), fixture::HOST_HEAD);
    }

    #[test]
    fn a_failed_registration_removes_every_file() {
        let dir = scratch("download", "register-fails");
        // Not included from brokkr.toml: the appended entries cannot resolve.
        let host = fixture::project(&dir, false);
        let paths = fixture::paths(&dir);
        let pbf = crate::pbf::build::pbf(Some(4704), Some(DENMARK_UPDATES), false);
        let err = run(&paths, &Fake::new(&pbf, &[]), "denmark", DAY, false).unwrap_err();
        assert!(err.contains("does not resolve"), "{err}");
        assert_eq!(data_entries(&dir), Vec::<String>::new());
        assert_eq!(std::fs::read_to_string(&host).unwrap(), fixture::HOST_HEAD);
    }

    #[test]
    fn leftovers_and_non_pbfs_are_refused() {
        let dir = scratch("download", "refuse");
        let host = fixture::project(&dir, true);
        let paths = fixture::paths(&dir);
        for name in [
            "denmark-20261116-seq1.osm.pbf",
            "denmark-20261116-with-indexdata.osm.pbf",
        ] {
            let leftover = dir.join("data").join(name);
            std::fs::write(&leftover, b"old").unwrap();
            let fake = Fake::new(b"<html>", &[]);
            let err = run(&paths, &fake, "denmark", DAY, true).unwrap_err();
            assert!(err.contains("already exists"), "{err}");
            assert_eq!(fake.fetches.get(), 0);
            assert_eq!(std::fs::read(&leftover).unwrap(), b"old");
            std::fs::remove_file(&leftover).unwrap();
        }

        let err = run(&paths, &Fake::new(b"<html>", &[]), "denmark", DAY, true).unwrap_err();
        assert!(err.contains("removed"), "{err}");
        assert!(
            std::fs::read_dir(dir.join("data"))
                .unwrap()
                .next()
                .is_none()
        );
        assert_eq!(std::fs::read_to_string(&host).unwrap(), fixture::HOST_HEAD);
    }

    #[test]
    fn a_missing_host_file_is_refused_before_transfer() {
        let dir = scratch("download", "nohost");
        let host = fixture::project(&dir, true);
        std::fs::remove_file(host).unwrap();
        std::fs::write(dir.join("brokkr.toml"), "project = \"elivagar\"\n").unwrap();
        let paths = fixture::paths(&dir);
        let fake = Fake::new(b"x", &[]);
        let err = run(&paths, &fake, "denmark", DAY, true).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert_eq!(fake.fetches.get(), 0);
    }
}
