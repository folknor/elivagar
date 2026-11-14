// Outgoing ocean artifact retention: before `ocean-build` replaces the
// artifact at its path, the outgoing generation is kept one generation deep
// beside it (reference/cli.md, Retention).

use super::key::Xxh128;
use std::path::{Path, PathBuf};

/// UTC calendar date as `YYYYMMDD` for a file timestamp, used to name
/// retained artifact generations. Days-to-civil conversion per Howard
/// Hinnant's algorithm; pre-epoch timestamps clamp to the epoch.
fn yyyymmdd_utc(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}")
}

/// Does `name` match a retained-generation name under `<prefix><version>-...`?
/// Accept the current key-prefixed scheme and the previous date-only scheme so
/// the next retention pass can remove a generation left by an older build.
/// Cleanup deletes only these exact shapes; manual names outside them survive.
fn is_retained_artifact_name(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(".pmtiles") else {
        return false;
    };
    let Some((version, suffix)) = rest.split_once('-') else {
        return false;
    };
    let version_ok = !version.is_empty()
        && (version == "unknown" || version.bytes().all(|b| b.is_ascii_digit()));
    if !version_ok {
        return false;
    }
    let valid_date = |date: &str| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit());
    if valid_date(suffix) {
        return true;
    }
    let Some((key_prefix, date)) = suffix.split_once('-') else {
        return false;
    };
    let key_prefix_ok = (version == "unknown" && key_prefix == "unknown")
        || (key_prefix.len() == 8
            && key_prefix
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    key_prefix_ok && valid_date(date)
}

/// Keep the outgoing ocean artifact one generation deep before a rebuild
/// replaces it. `ocean-build` publishes the artifact at the same path, so without this
/// every policy bump destroys the only comparand for the version it
/// replaces - and the retained generation is exactly what the
/// archive-bracketing idiom (dump the same tile from each generation, diff
/// the same ROI with `scripts/validate/svg-roi.mjs`) needs to answer "did
/// this rebuild change ocean bytes, and where".
///
/// The outgoing file is hard-linked, not renamed: the active artifact path
/// is never empty, a failed rebuild leaves the active artifact unchanged,
/// and the link costs no disk until the writer's finalize rename swaps the inode
/// out. Retained name: `<stem>-v<policy_version>-<key_prefix>-<build_date>.pmtiles`.
/// The prefix is the first eight lowercase hex characters of XXH3-128 over
/// the byte-stable artifact key; it distinguishes source rotations and
/// compression settings. Unreadable keys use `unknown` for both version and
/// prefix, since a damaged artifact is still worth keeping. The date is from
/// its mtime. Older retained generations are removed after the outgoing link
/// is created; anything outside a recognized naming scheme is never touched.
pub(crate) fn retain_outgoing_ocean_artifact(output_path: &Path) -> std::io::Result<()> {
    if !output_path.exists() {
        return Ok(());
    }
    let (version, key_prefix) = match super::artifact::declared_key(output_path) {
        Ok(key) => {
            let bytes = serde_json::to_vec(&key).map_err(std::io::Error::other)?;
            let fingerprint = Xxh128::from_bytes(&bytes).to_string();
            (key.policy_version.to_string(), fingerprint[..8].to_string())
        }
        Err(_) => ("unknown".to_string(), "unknown".to_string()),
    };
    let date = yyyymmdd_utc(std::fs::metadata(output_path)?.modified()?);
    let stem = output_path.file_stem().map_or_else(
        || "ocean-tiles".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let dir = output_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let retained_prefix = format!("{stem}-v");
    let retained_name = format!("{stem}-v{version}-{key_prefix}-{date}.pmtiles");
    let retained_path = dir.join(&retained_name);
    if retained_path.exists() {
        std::fs::remove_file(&retained_path)?;
    }
    std::fs::hard_link(output_path, &retained_path)?;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name != retained_name && is_retained_artifact_name(name, &retained_prefix) {
            std::fs::remove_file(entry.path())?;
        }
    }
    eprintln!(
        "Retained outgoing ocean artifact: {}",
        retained_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yyyymmdd_utc_known_dates() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(yyyymmdd_utc(UNIX_EPOCH), "19700101");
        assert_eq!(
            yyyymmdd_utc(UNIX_EPOCH + Duration::from_secs(1_785_024_000)),
            "20260726"
        );
        // Leap day.
        assert_eq!(
            yyyymmdd_utc(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "20000229"
        );
    }

    #[test]
    fn retained_artifact_name_matcher_is_strict() {
        let p = "ocean-tiles-v";
        assert!(is_retained_artifact_name(
            "ocean-tiles-v4-20260724.pmtiles",
            p
        ));
        assert!(is_retained_artifact_name(
            "ocean-tiles-vunknown-20260724.pmtiles",
            p
        ));
        assert!(is_retained_artifact_name(
            "ocean-tiles-v4-a1b2c3d4-20260724.pmtiles",
            p
        ));
        assert!(is_retained_artifact_name(
            "ocean-tiles-vunknown-unknown-20260724.pmtiles",
            p
        ));
        // Manually kept copies and near-misses survive the cleanup.
        assert!(!is_retained_artifact_name(
            "ocean-tiles-dp-20260712.pmtiles",
            p
        ));
        assert!(!is_retained_artifact_name("ocean-tiles.pmtiles", p));
        assert!(!is_retained_artifact_name("ocean-tiles-verify.pmtiles", p));
        assert!(!is_retained_artifact_name(
            "ocean-tiles-v4-2026072.pmtiles",
            p
        ));
        assert!(!is_retained_artifact_name("ocean-tiles-v4-20260724.txt", p));
        assert!(!is_retained_artifact_name(
            "ocean-tiles-v-20260724.pmtiles",
            p
        ));
        assert!(!is_retained_artifact_name(
            "ocean-tiles-v4-a1b2c3-20260724.pmtiles",
            p
        ));
        assert!(!is_retained_artifact_name(
            "ocean-tiles-v4-A1B2C3D4-20260724.pmtiles",
            p
        ));
        assert!(!is_retained_artifact_name(
            "ocean-tiles-v4-unknown-20260724.pmtiles",
            p
        ));
    }

    #[test]
    fn ocean_artifact_retention_keeps_one_generation() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let output = dir.path().join("ocean-tiles.pmtiles");
        std::fs::write(&output, b"outgoing artifact").expect("write artifact");
        let old_retained = dir.path().join("ocean-tiles-v3-20260101.pmtiles");
        std::fs::write(&old_retained, b"previous generation").expect("write retained");
        let manual_keep = dir.path().join("ocean-tiles-dp-20260712.pmtiles");
        std::fs::write(&manual_keep, b"manual keep").expect("write manual keep");

        retain_outgoing_ocean_artifact(&output).expect("retention");

        assert!(output.exists(), "active artifact must survive retention");
        assert!(!old_retained.exists(), "older retained generation dropped");
        assert!(manual_keep.exists(), "names outside the scheme untouched");
        let retained: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| is_retained_artifact_name(n, "ocean-tiles-v"))
            .collect();
        assert_eq!(retained.len(), 1, "one generation deep: {retained:?}");
        // The fixture bytes are not a readable archive, so the retained name
        // carries `unknown` for both the version and key prefix.
        assert!(
            retained[0].starts_with("ocean-tiles-vunknown-unknown-"),
            "unreadable key retains with unknown version and key prefix: {}",
            retained[0]
        );
        assert_eq!(
            std::fs::read(dir.path().join(&retained[0])).expect("read retained"),
            b"outgoing artifact"
        );

        // Idempotent within one day: a second retention of the same outgoing
        // artifact replaces the retained link instead of accumulating copies.
        retain_outgoing_ocean_artifact(&output).expect("second retention");
        let count = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| is_retained_artifact_name(n, "ocean-tiles-v"))
            .count();
        assert_eq!(count, 1, "re-retention must not accumulate generations");
    }

    #[test]
    fn ocean_artifact_retention_preserves_previous_copy_when_link_cannot_be_created() {
        let dir = tempfile::tempdir().expect("create artifact directory");
        let output = dir.path().join("ocean-tiles.pmtiles");
        std::fs::write(&output, b"outgoing artifact").expect("write active artifact");
        let old_retained = dir.path().join("ocean-tiles-v3-20260101.pmtiles");
        std::fs::write(&old_retained, b"previous generation").expect("write retained artifact");
        let date = yyyymmdd_utc(
            std::fs::metadata(&output)
                .expect("metadata")
                .modified()
                .expect("mtime"),
        );
        let blocked = dir
            .path()
            .join(format!("ocean-tiles-vunknown-unknown-{date}.pmtiles"));
        std::fs::create_dir(&blocked).expect("block retained link path");

        assert!(retain_outgoing_ocean_artifact(&output).is_err());
        assert_eq!(
            std::fs::read(&old_retained).expect("read retained artifact"),
            b"previous generation"
        );
        assert_eq!(
            std::fs::read(&output).expect("read active artifact"),
            b"outgoing artifact"
        );
    }

    #[test]
    fn ocean_artifact_retention_noop_without_artifact() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let output = dir.path().join("ocean-tiles.pmtiles");
        retain_outgoing_ocean_artifact(&output).expect("retention on absent artifact");
        let entries = std::fs::read_dir(dir.path()).expect("read dir").count();
        assert_eq!(entries, 0, "nothing created when no artifact exists");
    }
}
