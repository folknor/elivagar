//! Registering a dataset by appending its TOML tables, as text, to the
//! host's file under `brokkr.d/hosts/`.
//!
//! There is deliberately no config writer: the host files are hand-edited,
//! comment-heavy records, and the only edit this tool makes is to add tables
//! at the end. What was there stays byte-for-byte. After the append the
//! project config is composed again and the new entry resolved through
//! `elivagar_devkit::tilegen::resolve_pbf`, which re-hashes the file against
//! the recorded pin; if anything about that fails (a host file brokkr.toml
//! does not include, a table that collides with one defined in another
//! shape), the file is truncated back to its original length.

use std::io::Write;
use std::path::{Path, PathBuf};

use elivagar_devkit::config::Paths;

/// Where the host files live, relative to the project root.
const HOSTS_DIR: &str = "brokkr.d/hosts";

/// The `origin` value the existing host files record for Geofabrik extracts.
pub(crate) const GEOFABRIK_ORIGIN: &str = "Geofabrik";

/// Letters, digits, `_` and `-`: a TOML bare key, and a safe file stem.
pub(crate) fn is_bare_key(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One `pbf.<variant>` entry as it will be written.
pub(crate) struct Entry<'a> {
    pub(crate) file: &'a str,
    pub(crate) xxhash: &'a str,
    pub(crate) seq: Option<u64>,
}

pub(crate) fn dataset_table(host: &str, key: &str, download_date: &str) -> String {
    format!(
        "[{host}.datasets.{key}]\norigin = {}\ndownload_date = {}\n",
        toml_string(GEOFABRIK_ORIGIN),
        toml_string(download_date)
    )
}

pub(crate) fn pbf_table(host: &str, key: &str, variant: &str, entry: &Entry<'_>) -> String {
    let mut out = format!(
        "[{host}.datasets.{key}.pbf.{variant}]\nfile = {}\nxxhash = {}\n",
        toml_string(entry.file),
        toml_string(entry.xxhash)
    );
    if let Some(seq) = entry.seq {
        out.push_str(&format!("seq = {seq}\n"));
    }
    out
}

/// The host's file, which must already exist: creating one would also need
/// an `include` line in brokkr.toml, which is not this tool's to write.
pub(crate) fn host_file(paths: &Paths) -> Result<PathBuf, String> {
    if !is_bare_key(&paths.hostname) {
        return Err(format!(
            "host '{}' is not a bare TOML key; register it by hand",
            paths.hostname
        ));
    }
    let path = paths
        .root
        .join(HOSTS_DIR)
        .join(format!("{}.toml", paths.hostname));
    if !path.is_file() {
        return Err(format!(
            "{} does not exist; create the host file and include it from brokkr.toml first",
            path.display()
        ));
    }
    Ok(path)
}

/// The composed `datasets.<key>` table of the selected host, if any.
pub(crate) fn dataset<'a>(paths: &'a Paths, key: &str) -> Option<&'a toml::Table> {
    paths
        .host
        .get("datasets")
        .and_then(|d| d.get(key))
        .and_then(toml::Value::as_table)
}

/// Append `text` (whole tables) to `file` after a blank line, then check
/// that every `key`/`variant` resolves from the recomposed project config.
/// On failure the file is cut back to exactly its prior bytes.
pub(crate) fn append_and_verify(
    paths: &Paths,
    file: &Path,
    text: &str,
    key: &str,
    variants: &[&str],
) -> Result<(), String> {
    let at = |e: std::io::Error| format!("{}: {e}", file.display());
    let before = std::fs::read(file).map_err(at)?;
    let separator = match before.last() {
        None => "",
        Some(b'\n') => "\n",
        Some(_) => "\n\n",
    };
    let mut handle = std::fs::OpenOptions::new()
        .append(true)
        .open(file)
        .map_err(at)?;
    let written = handle
        .write_all(format!("{separator}{text}").as_bytes())
        .and_then(|()| handle.sync_all());
    let verified = written
        .map_err(at)
        .and_then(|()| verify(paths, key, variants));
    if let Err(error) = verified {
        let restored = handle
            .set_len(before.len() as u64)
            .and_then(|()| handle.sync_all());
        return Err(match restored {
            Ok(()) => format!("{error}\n  {} left as it was", file.display()),
            Err(e) => format!(
                "{error}\n  and restoring {} failed: {e}; its first {} bytes are the original",
                file.display(),
                before.len()
            ),
        });
    }
    Ok(())
}

fn verify(paths: &Paths, key: &str, variants: &[&str]) -> Result<(), String> {
    let reloaded =
        elivagar_devkit::config::load_from(&paths.root.join("brokkr.toml"), Some(&paths.hostname))
            .map_err(|e| format!("the appended config does not compose: {e}"))?;
    for variant in variants {
        elivagar_devkit::tilegen::resolve_pbf(&reloaded, key, variant)
            .map(drop)
            .map_err(|e| format!("the appended {variant} entry does not resolve: {e}"))?;
    }
    Ok(())
}

/// `register DATASET VARIANT FILE`: pin a hand-made variant (one `download`
/// does not derive, or one it was told to skip with `--no-derive`) of a
/// dataset already registered for this host. `FILE` is a
/// name in the data dir; `seq` is carried over from the raw entry, since a
/// derived file describes the same snapshot.
pub(crate) fn register(paths: &Paths, key: &str, variant: &str, file: &str) -> Result<(), String> {
    if !is_bare_key(key) || !is_bare_key(variant) {
        return Err("dataset and variant must be letters, digits, '_' and '-' only".into());
    }
    if file.contains('/') || file.is_empty() || file.starts_with('.') {
        return Err(format!(
            "'{file}' must be a plain file name in the data dir {}",
            paths.data_dir.display()
        ));
    }
    let table = dataset(paths, key).ok_or_else(|| {
        format!(
            "dataset '{key}' is not registered for host '{}'; download it first",
            paths.hostname
        )
    })?;
    let pbf = table.get("pbf").and_then(toml::Value::as_table);
    if pbf.is_some_and(|p| p.contains_key(variant)) {
        return Err(format!(
            "datasets.{key}.pbf.{variant} is already registered for host '{}'; \
             this tool never rewrites an entry",
            paths.hostname
        ));
    }
    let seq = pbf
        .and_then(|p| p.get("raw"))
        .and_then(|r| r.get("seq"))
        .and_then(toml::Value::as_integer)
        .and_then(|s| u64::try_from(s).ok());
    let path = paths.data_dir.join(file);
    if !path.is_file() {
        return Err(format!("{} does not exist", path.display()));
    }
    let host_file = host_file(paths)?;
    crate::fetch::note(&format!("hashing {}", path.display()));
    let xxhash = elivagar_devkit::tilegen::file_digest(&path).map_err(|e| e.to_string())?;
    let text = pbf_table(
        &paths.hostname,
        key,
        variant,
        &Entry {
            file,
            xxhash: &xxhash,
            seq,
        },
    );
    append_and_verify(paths, &host_file, &text, key, &[variant])?;
    crate::fetch::note(&format!("registered in {}:\n{text}", host_file.display()));
    Ok(())
}

#[cfg(test)]
pub(crate) mod fixture {
    //! A project root with one included host file, under test scratch.

    use std::path::{Path, PathBuf};

    pub(crate) const HOST_HEAD: &str =
        "[h]\ndata = \"data\"\n\n# A comment the append must keep.\n[h.drives]\ndata = \"nvme\"";

    pub(crate) fn project(dir: &Path, included: bool) -> PathBuf {
        std::fs::create_dir_all(dir.join("brokkr.d/hosts")).expect("mkdir");
        std::fs::create_dir_all(dir.join("data")).expect("mkdir");
        let include = if included {
            "include = [\"brokkr.d/hosts/h.toml\"]\n"
        } else {
            ""
        };
        std::fs::write(
            dir.join("brokkr.toml"),
            format!("project = \"elivagar\"\n{include}"),
        )
        .expect("write");
        let host = dir.join("brokkr.d/hosts/h.toml");
        std::fs::write(&host, HOST_HEAD).expect("write");
        host
    }

    pub(crate) fn paths(dir: &Path) -> elivagar_devkit::config::Paths {
        elivagar_devkit::config::load_from(&dir.join("brokkr.toml"), Some("h")).expect("config")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::test_scratch::scratch;

    #[test]
    fn strings_are_escaped_and_tables_rendered() {
        assert_eq!(toml_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(
            pbf_table(
                "h",
                "d",
                "raw",
                &Entry {
                    file: "d.osm.pbf",
                    xxhash: "ab",
                    seq: Some(7)
                }
            ),
            "[h.datasets.d.pbf.raw]\nfile = \"d.osm.pbf\"\nxxhash = \"ab\"\nseq = 7\n"
        );
    }

    #[test]
    fn register_appends_a_variant_and_refuses_a_second_time() {
        let dir = scratch("registry", "register");
        let host = fixture::project(&dir, true);
        let head = format!(
            "{}\n\n[h.datasets.d]\norigin = \"Geofabrik\"\n\n[h.datasets.d.pbf.raw]\nfile = \"r\"\nseq = 9\n",
            fixture::HOST_HEAD
        );
        std::fs::write(&host, &head).unwrap();
        std::fs::write(dir.join("data/r"), b"raw").unwrap();
        std::fs::write(dir.join("data/i"), b"indexed").unwrap();
        let paths = fixture::paths(&dir);
        register(&paths, "d", "indexed", "i").unwrap();
        let after = std::fs::read_to_string(&host).unwrap();
        let tail = after.strip_prefix(&head).unwrap();
        let digest = elivagar_devkit::tilegen::file_digest(&dir.join("data/i")).unwrap();
        assert_eq!(
            tail,
            format!("\n[h.datasets.d.pbf.indexed]\nfile = \"i\"\nxxhash = \"{digest}\"\nseq = 9\n")
        );
        let paths = fixture::paths(&dir);
        for (key, variant, file) in [
            ("d", "indexed", "i"),
            ("d", "raw", "i"),
            ("x", "indexed", "i"),
            ("d", "locations", "missing"),
            ("d", "locations", "../i"),
        ] {
            assert!(
                register(&paths, key, variant, file).is_err(),
                "{key} {variant} {file}"
            );
        }
        assert_eq!(std::fs::read_to_string(&host).unwrap(), after);
    }

    #[test]
    fn a_failed_verification_restores_the_exact_bytes() {
        let dir = scratch("registry", "rollback");
        // Not included from brokkr.toml: the appended entry cannot resolve.
        let host = fixture::project(&dir, false);
        std::fs::write(dir.join("data/r"), b"raw").unwrap();
        let paths = fixture::paths(&dir);
        let text = pbf_table(
            "h",
            "d",
            "raw",
            &Entry {
                file: "r",
                xxhash: &"0".repeat(32),
                seq: None,
            },
        );
        let err = append_and_verify(&paths, &host, &text, "d", &["raw"]).unwrap_err();
        assert!(err.contains("does not resolve"), "{err}");
        assert_eq!(std::fs::read_to_string(&host).unwrap(), fixture::HOST_HEAD);
    }
}
