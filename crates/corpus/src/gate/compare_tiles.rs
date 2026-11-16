//! `brokkr compare-tiles` - the lenient per-layer census of two archives.
//!
//! Native brokkr code over the linked elivagar crate. It used to build and run
//! elivagar's `compare_tiles` cargo example, which carried its own hand-rolled
//! PMTiles reader and MVT scanner; both now come from the crate, so there is one
//! decoder in the system rather than three.
//!
//! This is the **lenient** half of the pair `elivagar.md` describes: where
//! `brokkr regress` diffs two archives feature by feature and returns a verdict,
//! this samples tiles and reports per-layer feature counts side by side. It has
//! no pass/fail and gates nothing - it answers "roughly what changed, and
//! where" for two archives that may not be comparable at all (different
//! producers, different configs, different extracts). Bad sampled tiles are
//! skipped; invalid sample sizes and unreadable archives are errors. That is
//! why it decodes **tolerant**: a foreign producer's extra wire fields are
//! skipped rather than treated as corruption, which is exactly wrong for the
//! gate and exactly right here.
//!
//! One reporting change from the shed example: it counted raw geometry-command
//! varints (`cmds`), a number only reachable by scanning the wire bytes
//! directly. Decoding through `tile_detail` gives vertex counts instead, which
//! is what the column now reports (`verts`) - a better-defined quantity that
//! moves for the same reasons.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use super::constants::mvt;
use super::constants::{DEFAULT_COMPARE_SAMPLE_PER_ZOOM, MAX_COMPARE_SAMPLE_PER_ZOOM};
use super::eliv::{
    ArchiveView, BlobRef, Strictness, decode_detail_tile, next_zoom_boundary, tile_id_to_zxy,
};
use crate::error::DevError;
use crate::output;

/// Per-layer accumulator: feature count, the geometry-type split, and vertices.
#[derive(Clone, Copy, Default)]
struct LayerTally {
    features: usize,
    points: usize,
    lines: usize,
    polygons: usize,
    vertices: usize,
}

impl LayerTally {
    fn add(&mut self, other: Self) {
        self.features += other.features;
        self.points += other.points;
        self.lines += other.lines;
        self.polygons += other.polygons;
        self.vertices += other.vertices;
    }
}

type Tallies = BTreeMap<String, LayerTally>;

#[derive(Clone, Copy)]
struct TileSpan {
    start: u64,
    end: u64,
    blob: BlobRef,
}

#[derive(Clone, Copy)]
struct CommonSpan {
    start: u64,
    end: u64,
    a: BlobRef,
    b: BlobRef,
}

#[derive(Clone, Copy)]
struct SamplePair {
    a: BlobRef,
    b: BlobRef,
}

type ZoomRuns = BTreeMap<u8, Vec<TileSpan>>;

fn addressed_runs(archive: &ArchiveView, path: &str) -> Result<(ZoomRuns, u64), DevError> {
    let runs = archive
        .read_all_runs()
        .map_err(|error| archive_failure(path, &error))?;
    let mut by_zoom = ZoomRuns::new();
    let mut addressed = 0u64;
    for run in runs {
        let run_end = run
            .tile_id
            .checked_add(u64::from(run.run_length))
            .ok_or_else(|| DevError::Verify("directory run end overflows".into()))?;
        addressed = addressed
            .checked_add(u64::from(run.run_length))
            .ok_or_else(|| DevError::Verify("addressed tile count overflows".into()))?;
        let blob = run.blob();
        let mut start = run.tile_id;
        while start < run_end {
            let (zoom, _, _) = tile_id_to_zxy(start);
            let end = run_end.min(next_zoom_boundary(start));
            if end <= start {
                return Err(DevError::Verify(
                    "directory run crosses an invalid zoom boundary".into(),
                ));
            }
            by_zoom
                .entry(zoom)
                .or_default()
                .push(TileSpan { start, end, blob });
            start = end;
        }
    }
    Ok((by_zoom, addressed))
}

fn tile_count(runs: &[TileSpan]) -> u64 {
    runs.iter().map(|run| run.end - run.start).sum()
}

fn intersect_runs(a: &[TileSpan], b: &[TileSpan]) -> Vec<CommonSpan> {
    let mut out = Vec::new();
    let (mut ai, mut bi) = (0usize, 0usize);
    while let (Some(ar), Some(br)) = (a.get(ai), b.get(bi)) {
        if ar.end <= br.start {
            ai += 1;
        } else if br.end <= ar.start {
            bi += 1;
        } else {
            out.push(CommonSpan {
                start: ar.start.max(br.start),
                end: ar.end.min(br.end),
                a: ar.blob,
                b: br.blob,
            });
            if ar.end <= br.end {
                ai += 1;
            }
            if br.end <= ar.end {
                bi += 1;
            }
        }
    }
    out
}

fn spread_sample(common: &[CommonSpan], count: u64, take: usize) -> Vec<SamplePair> {
    let mut sampled = Vec::with_capacity(take);
    let mut next = 0usize;
    let mut before = 0u64;
    for span in common {
        let span_count = span.end - span.start;
        let after = before + span_count;
        while next < take {
            let numerator = u128::try_from(next).unwrap_or(u128::MAX) * u128::from(count);
            let denominator = u128::try_from(take).unwrap_or(u128::MAX);
            let ordinal = u64::try_from(numerator / denominator).unwrap_or(u64::MAX);
            if ordinal >= after {
                break;
            }
            if ordinal >= before {
                sampled.push(SamplePair {
                    a: span.a,
                    b: span.b,
                });
            }
            next += 1;
        }
        before = after;
    }
    sampled
}

/// Decode one tile into its own tallies, returning them with the decompressed
/// size. A tile that will not decode is skipped rather than fatal: this is a
/// census over a sample, and one bad tile should not deny the other sampled
/// tiles.
fn tally_tile(
    archive: &ArchiveView,
    blob: BlobRef,
    path: &str,
) -> Result<Option<(Tallies, usize)>, DevError> {
    let bytes = match archive.read_tile(blob) {
        Ok(bytes) => bytes,
        Err(error) if malformed_tile_error(error.kind()) => return Ok(None),
        Err(error) => return Err(archive_failure(path, &error)),
    };
    let Ok(tile) = decode_detail_tile(&bytes, Strictness::Tolerant) else {
        return Ok(None);
    };
    let mut into = Tallies::new();
    for layer in &tile.layers {
        let tally = into.entry(layer.name.to_string()).or_default();
        for feature in &layer.features {
            tally.features += 1;
            match feature.geom_type {
                mvt::GEOMETRY_POINT => tally.points += 1,
                mvt::GEOMETRY_LINESTRING => tally.lines += 1,
                mvt::GEOMETRY_POLYGON => tally.polygons += 1,
                _ => {}
            }
            tally.vertices += feature
                .components
                .iter()
                .flat_map(|c| c.rings.iter())
                .map(|r| r.points.len())
                .sum::<usize>();
        }
    }
    Ok(Some((into, bytes.len())))
}

fn malformed_tile_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
    )
}

fn archive_failure(path: &str, error: &io::Error) -> DevError {
    let message = format!("{path}: {error}");
    if malformed_tile_error(error.kind()) {
        DevError::Verify(message)
    } else {
        DevError::Io(io::Error::new(error.kind(), message))
    }
}

/// Fold one tile's tallies into a running total.
fn merge(into: &mut Tallies, from: &Tallies) {
    for (layer, tally) in from {
        into.entry(layer.clone()).or_default().add(*tally);
    }
}

fn percent(a: usize, b: usize) -> String {
    if a > 0 && b > 0 {
        #[allow(clippy::cast_precision_loss)]
        let ratio = b as f64 / a as f64;
        format!("{:+.0}%", (ratio - 1.0) * 100.0)
    } else if a == 0 {
        "+inf".to_string()
    } else {
        "-100%".to_string()
    }
}

pub fn run(file_a: &str, file_b: &str, sample: Option<usize>) -> Result<(), DevError> {
    let sample_per_zoom = sample.unwrap_or(DEFAULT_COMPARE_SAMPLE_PER_ZOOM);
    if sample_per_zoom == 0 || sample_per_zoom > MAX_COMPARE_SAMPLE_PER_ZOOM {
        return Err(DevError::Config(format!(
            "--sample must be between 1 and {MAX_COMPARE_SAMPLE_PER_ZOOM} tiles per zoom"
        )));
    }
    let open = |path: &str| {
        ArchiveView::open(Path::new(path)).map_err(|error| archive_failure(path, &error))
    };
    let name = |path: &str| {
        Path::new(path)
            .file_stem()
            .map_or_else(|| path.to_string(), |s| s.to_string_lossy().to_string())
    };

    let archive_a = open(file_a)?;
    let archive_b = open(file_b)?;
    let name_a = name(file_a);
    let name_b = name(file_b);

    let (by_zoom_a, addressed_a) = addressed_runs(&archive_a, file_a)?;
    let (by_zoom_b, addressed_b) = addressed_runs(&archive_b, file_b)?;
    output::bench_msg(&format!(
        "{name_a}: {} tiles, z{}-{}",
        addressed_a,
        archive_a.min_zoom(),
        archive_a.max_zoom()
    ));
    output::bench_msg(&format!(
        "{name_b}: {} tiles, z{}-{}",
        addressed_b,
        archive_b.min_zoom(),
        archive_b.max_zoom()
    ));

    let min_zoom = archive_a.min_zoom().min(archive_b.min_zoom());
    let max_zoom = archive_a.max_zoom().max(archive_b.max_zoom());
    let mut grand_a: Tallies = BTreeMap::new();
    let mut grand_b: Tallies = BTreeMap::new();

    for z in min_zoom..=max_zoom {
        let empty: Vec<TileSpan> = Vec::new();
        let (zoom_a, zoom_b) = compare_zoom(
            z,
            (&archive_a, by_zoom_a.get(&z).unwrap_or(&empty)),
            (&archive_b, by_zoom_b.get(&z).unwrap_or(&empty)),
            sample_per_zoom,
            file_a,
            file_b,
        )?;
        merge(&mut grand_a, &zoom_a);
        merge(&mut grand_b, &zoom_b);
    }

    output::plain("\n=== Grand totals (all sampled tiles) ===\n");
    output::plain(&format!(
        "{:24} {name_a:>10} {name_b:>10} {:>8} {:>10} {:>10}",
        "layer", "diff", "verts_A", "verts_B"
    ));
    output::plain(&"-".repeat(78));
    let mut total_a = LayerTally::default();
    let mut total_b = LayerTally::default();
    for layer in layer_names(&grand_a, &grand_b) {
        let a = grand_a.get(&layer).copied().unwrap_or_default();
        let b = grand_b.get(&layer).copied().unwrap_or_default();
        total_a.add(a);
        total_b.add(b);
        let diff = if a.features > 0 && b.features > 0 {
            percent(a.features, b.features)
        } else if a.features == 0 {
            "only B".to_string()
        } else {
            "only A".to_string()
        };
        output::plain(&format!(
            "{layer:24} {:10} {:10} {diff:>8} {:10} {:10}",
            a.features, b.features, a.vertices, b.vertices
        ));
    }
    output::plain(&"-".repeat(78));
    let total_diff = if total_a.features > 0 {
        percent(total_a.features, total_b.features)
    } else {
        "N/A".to_string()
    };
    output::plain(&format!(
        "{:24} {:10} {:10} {total_diff:>8} {:10} {:10}",
        "TOTAL", total_a.features, total_b.features, total_a.vertices, total_b.vertices
    ));
    Ok(())
}

/// Sample and tally one zoom level, printing its summary line and per-layer
/// table. Returns the two side's tallies for the grand total.
fn compare_zoom(
    z: u8,
    a: (&ArchiveView, &[TileSpan]),
    b: (&ArchiveView, &[TileSpan]),
    sample_per_zoom: usize,
    path_a: &str,
    path_b: &str,
) -> Result<(Tallies, Tallies), DevError> {
    let (archive_a, za) = a;
    let (archive_b, zb) = b;
    let (mut zoom_a, mut zoom_b): (Tallies, Tallies) = (BTreeMap::new(), BTreeMap::new());

    // Intersect directory intervals and retain one record per run overlap.
    // Memory follows directory size, not the number of addressed tiles.
    let common = intersect_runs(za, zb);
    let common_count = common
        .iter()
        .try_fold(0u64, |count, span| count.checked_add(span.end - span.start))
        .ok_or_else(|| DevError::Verify("common tile count overflows".into()))?;
    let count_a = tile_count(za);
    let count_b = tile_count(zb);
    if common_count == 0 {
        output::bench_msg(&format!(
            "z{z:2}: A={count_a} tiles, B={count_b} tiles, 0 common - skipping"
        ));
        return Ok((zoom_a, zoom_b));
    }

    // Choose evenly spaced ordinals from the interval intersection. The
    // sample vector is bounded by the configured cap regardless of the
    // number of tile IDs the runs cover.
    let take = usize::try_from(common_count)
        .unwrap_or(usize::MAX)
        .min(sample_per_zoom);
    let sampled = spread_sample(&common, common_count, take);

    let (mut bytes_a, mut bytes_b) = (0usize, 0usize);
    let (mut decoded, mut errors) = (0usize, 0usize);
    for pair in sampled {
        match (
            tally_tile(archive_a, pair.a, path_a)?,
            tally_tile(archive_b, pair.b, path_b)?,
        ) {
            (Some((ta, la)), Some((tb, lb))) => {
                merge(&mut zoom_a, &ta);
                merge(&mut zoom_b, &tb);
                bytes_a += la;
                bytes_b += lb;
                decoded += 1;
            }
            _ => errors += 1,
        }
    }
    if decoded == 0 {
        output::bench_msg(&format!(
            "z{z:2}: {errors} read errors, 0 decoded - skipping"
        ));
        return Ok((BTreeMap::new(), BTreeMap::new()));
    }

    let err_note = if errors > 0 {
        format!(" ({errors} read errors)")
    } else {
        String::new()
    };
    output::bench_msg(&format!(
        "z{z:2}: {decoded} tiles sampled (of {} common, A={} B={} total)  avg_mvt: A={} B={} bytes{err_note}",
        common_count,
        count_a,
        count_b,
        bytes_a / decoded,
        bytes_b / decoded,
    ));
    print_layers(&zoom_a, &zoom_b, "  ");
    Ok((zoom_a, zoom_b))
}

fn layer_names(a: &Tallies, b: &Tallies) -> Vec<String> {
    let mut names: Vec<String> = a.keys().chain(b.keys()).cloned().collect();
    names.sort();
    names.dedup();
    names
}

fn print_layers(a: &Tallies, b: &Tallies, indent: &str) {
    for layer in layer_names(a, b) {
        let ta = a.get(&layer).copied().unwrap_or_default();
        let tb = b.get(&layer).copied().unwrap_or_default();
        if ta.features == 0 && tb.features == 0 {
            continue;
        }
        // The L/P split is only informative on a mixed-geometry layer.
        let split = |t: LayerTally| {
            if t.lines > 0 || t.polygons > 0 {
                format!(" (L:{} P:{})", t.lines, t.polygons)
            } else {
                String::new()
            }
        };
        output::plain(&format!(
            "{indent}{layer:24} A:{:6}{:16} B:{:6}{:16} verts A:{:8} B:{:8}  {}",
            ta.features,
            split(ta),
            tb.features,
            split(tb),
            ta.vertices,
            tb.vertices,
            percent(ta.features, tb.features)
        ));
    }
}
