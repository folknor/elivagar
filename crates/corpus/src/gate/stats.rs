//! Per-zoom directory statistics of one archive, read through the `eliv` seam.
//! Ported from brokkr's `pmtiles-stats` (`src/pmtiles.rs`); the output lines
//! are brokkr's. Header decoding and directory validation belong to
//! elivagar's reader: a corrupt or unsupported directory is an error, never a
//! partial report.

use std::collections::BTreeMap;
use std::path::Path;

use rustc_hash::FxHashSet;

use super::eliv::{ArchiveView, RawDirEntry, tile_id_to_zxy};
use crate::error::DevError;
use crate::output;

#[derive(Default)]
struct ZoomStats {
    tile_count: u64,
    // Only its size is read, so the hasher cannot affect output.
    unique_offsets: FxHashSet<(u64, u32)>,
    min_length: Option<u32>,
    max_length: u32,
    total_length: u64,
}

impl ZoomStats {
    fn record(&mut self, entry: &RawDirEntry) {
        let tiles = u64::from(entry.run_length);
        self.tile_count = self.tile_count.saturating_add(tiles);
        self.unique_offsets.insert((entry.offset, entry.length));
        if entry.length > 0 {
            self.min_length = Some(
                self.min_length
                    .map_or(entry.length, |prev| prev.min(entry.length)),
            );
            self.max_length = self.max_length.max(entry.length);
            self.total_length = self
                .total_length
                .saturating_add(u64::from(entry.length).saturating_mul(tiles));
        }
    }
}

/// Print the archive's header summary and per-zoom tile statistics.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn run(path: &Path) -> Result<(), DevError> {
    let archive_error = |e: &std::io::Error| DevError::Config(format!("{}: {e}", path.display()));
    let archive = ArchiveView::open(path).map_err(|e| archive_error(&e))?;
    let entries = archive.read_all_runs().map_err(|e| archive_error(&e))?;
    let header = *archive.header();

    // Ordered by zoom, which is the print order.
    let mut zoom_stats: BTreeMap<u8, ZoomStats> = BTreeMap::new();
    for entry in &entries {
        let (zoom, _, _) = tile_id_to_zxy(entry.tile_id);
        zoom_stats.entry(zoom).or_default().record(entry);
    }

    output::plain(&format!("\n=== {} ===", path.display()));
    output::plain(&format!(
        "  Zoom: z{}-z{}, internal compression: {}",
        header.min_zoom, header.max_zoom, header.internal_compression,
    ));
    output::plain(&format!(
        "  Addressed: {}, Entries: {}, Unique: {}",
        fmt_int(header.addressed_tiles),
        fmt_int(header.tile_entries),
        fmt_int(header.tile_contents),
    ));
    output::plain(&format!(
        "  Data size: {} bytes ({:.1} MB)",
        fmt_int(header.tile_data.length),
        header.tile_data.length as f64 / 1_048_576.0,
    ));
    output::plain(&format!(
        "  Directory entries: {}",
        fmt_int(entries.len() as u64)
    ));
    output::plain("  Per-zoom tiles:");

    let mut total_tiles = 0u64;
    let mut global_min: Option<u32> = None;
    let mut global_max = 0u32;
    let mut global_total_length = 0u64;
    for (zoom, stats) in &zoom_stats {
        total_tiles = total_tiles.saturating_add(stats.tile_count);
        if let Some(min) = stats.min_length {
            global_min = Some(global_min.map_or(min, |prev| prev.min(min)));
        }
        global_max = global_max.max(stats.max_length);
        global_total_length = global_total_length.saturating_add(stats.total_length);

        let average = match stats.total_length.checked_div(stats.tile_count) {
            Some(value) => format!("{} avg bytes", fmt_int(value)),
            None => String::new(),
        };
        output::plain(&format!(
            "    z{zoom:2}: {:>8} tiles, {:>8} unique, {:>16}",
            fmt_int(stats.tile_count),
            fmt_int(stats.unique_offsets.len() as u64),
            average,
        ));
    }

    output::plain(&format!("  Total: {} tiles", fmt_int(total_tiles)));
    if let Some(average) = global_total_length.checked_div(total_tiles) {
        output::plain(&format!(
            "  Tile sizes: min {} bytes, max {} bytes, avg {} bytes",
            fmt_int(u64::from(global_min.unwrap_or(0))),
            fmt_int(u64::from(global_max)),
            fmt_int(average),
        ));
    }
    Ok(())
}

/// `1234567` -> `1,234,567`.
fn fmt_int(number: u64) -> String {
    let value = number.to_string();
    let len = value.len();
    let mut formatted = String::with_capacity(len + len / 3);
    for (index, digit) in value.chars().enumerate() {
        if index > 0 && (len - index) % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

#[cfg(test)]
mod tests {
    use super::fmt_int;

    #[test]
    fn groups_thousands() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1000), "1,000");
        assert_eq!(fmt_int(123_456), "123,456");
        assert_eq!(fmt_int(1_234_567), "1,234,567");
    }
}
