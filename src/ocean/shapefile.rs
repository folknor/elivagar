// ESRI Polygon shapefile reader.
//
// Opens a `.shp`/`.shx` pair, validates the index, maps the shape file and
// hands each record's polygons to the caller as projected Web Mercator
// [0,1] rings. It knows nothing about sorting, the pyramid, band ownership
// or quantization: the caller decides from each record's bounds whether the
// record is wanted, and what to do with its polygons.
//
// Validation is deliberately lazy and staged, and the stages are a contract:
// `open` validates only the index; `read_polygons` validates a record's
// framing, shape type and bounds before asking the caller, and its part and
// point arrays only after the caller accepted it. A record the caller rejects
// may therefore carry malformed geometry without failing the run, while a
// malformed type or bounds always fails.

use super::key::{OceanShapefileHashes, ShapefilePair, Xxh128};
use crate::geometry::{self, projection::Point};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const RING_READ_BUFFER_BYTES: usize = 64 * 1024;

/// Why a shapefile read failed. The two cases are the reader's evidence and
/// nothing more: `Malformed` means bytes were read and refused here, so the
/// pipeline classes it as refused input; `Read` means the bytes could not be
/// obtained, and keeps the original `io::Error` so the ordinary errno evidence
/// rule decides its class. A short positional read is `Read`, not
/// `Malformed`: framing was already checked against the mapping, so it says
/// the file changed under us, not that its bytes are bad.
#[derive(Debug)]
pub(crate) enum ShapefileError {
    Malformed(String),
    Read(std::io::Error),
}

impl std::fmt::Display for ShapefileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(message) => f.write_str(message),
            Self::Read(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ShapefileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Malformed(_) => None,
            Self::Read(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for ShapefileError {
    fn from(error: std::io::Error) -> Self {
        Self::Read(error)
    }
}

type Result<T> = std::result::Result<T, ShapefileError>;

/// One `.shx` index entry, with byte offsets already converted from words.
#[derive(Clone, Copy)]
pub(super) struct ShxRecord {
    index: usize,
    offset: usize,
    content_len: usize,
}

/// An open shapefile: the validated index, the `.shx` bytes it came from, and
/// the mapped `.shp`. Ring reads go through the same file handle that was
/// mapped, never a reopened path, so a replaced pathname cannot feed a pass
/// different bytes from the ones hashed.
pub(super) struct Shapefile {
    path: PathBuf,
    shx: Vec<u8>,
    records: Vec<ShxRecord>,
    file: std::fs::File,
    mmap: memmap2::Mmap,
}

impl Shapefile {
    /// Read and validate the `.shx` index, then open and map the `.shp`.
    /// A malformed index is reported before the `.shp` is opened.
    pub(super) fn open(path: &Path) -> Result<Self> {
        let files = ShapefilePair::from_shp(path);
        let shx_path = files.shx();
        let shx = std::fs::read(shx_path)?;
        let records = parse_shx(&shx, shx_path)?;
        eprintln!("  Index: {} shapes", records.len());
        let file = std::fs::File::open(files.shp())?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;
        Ok(Self {
            path: path.to_path_buf(),
            shx,
            records,
            file,
            mmap,
        })
    }

    /// Hashes of the exact `.shp` mapping and `.shx` buffer this value holds.
    pub(super) fn hashes(&self) -> OceanShapefileHashes {
        OceanShapefileHashes {
            shp_xxh128: Xxh128::from_bytes(&self.mmap),
            shx_xxh128: Xxh128::from_bytes(&self.shx),
        }
    }

    /// Advise the kernel that record reads are random access.
    pub(super) fn advise_random(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        self.mmap.advise(memmap2::Advice::Random)?;
        Ok(())
    }

    pub(super) fn mapped_len(&self) -> usize {
        self.mmap.len()
    }

    pub(super) fn records(&self) -> &[ShxRecord] {
        &self.records
    }

    /// Drop the mapping's pages and close it. Call as soon as reading is
    /// done; the mapping can be most of a world shapefile.
    pub(super) fn release(self) -> std::io::Result<()> {
        #[cfg(unix)]
        unsafe {
            self.mmap
                .unchecked_advise(memmap2::UncheckedAdvice::DontNeed)?;
        }
        Ok(())
    }

    /// Read one record. Null shapes return `Ok(false)`. Otherwise the record's
    /// framing, shape type and bounds are validated, and `accept` is asked
    /// with the record's projected bounds (minimum corner, maximum corner).
    /// A rejected record returns `Ok(false)` without its geometry being read.
    /// An accepted one returns `Ok(true)` - even if its polygons later turn
    /// out to contribute nothing - after `polygon` was called once per
    /// polygon, outer ring first, in record order.
    ///
    /// Ring roles follow the split-water input convention: the first ring is
    /// an outer, later rings with nonnegative signed area start a new outer,
    /// and negative rings are holes of the latest outer. The checked-in full
    /// OSMCD input has four zero-area contours, one after the first; zero
    /// stays an outer to preserve that grouping and the ocean bytes built on
    /// it. Each ring is read and validated before it closes the previous
    /// polygon, so an error can follow earlier `polygon` calls for the same
    /// record: callers must treat a record's output as provisional until it
    /// returns `Ok`.
    pub(super) fn read_polygons(
        &self,
        record: ShxRecord,
        accept: impl FnOnce(Point, Point) -> bool,
        mut polygon: impl FnMut(&[Point], &[Vec<Point>]),
    ) -> Result<bool> {
        let header_end = record
            .offset
            .checked_add(8)
            .ok_or_else(|| self.invalid(record, "record header offset overflow"))?;
        let content_end = header_end
            .checked_add(record.content_len)
            .ok_or_else(|| self.invalid(record, "content range overflow"))?;
        if record.offset < 100 || content_end > self.mmap.len() {
            return Err(self.invalid(record, "record content extends past the .shp file"));
        }

        let declared_content_words = be_u32_at(&self.mmap[..header_end], record.offset + 4);
        let declared_content_len = usize::try_from(declared_content_words)
            .ok()
            .and_then(|words| words.checked_mul(2))
            .ok_or_else(|| self.invalid(record, "record content length overflow"))?;
        if declared_content_len != record.content_len {
            return Err(self.invalid(record, "record length differs from the .shx index"));
        }
        if record.content_len < 4 {
            return Err(self.invalid(record, "record is shorter than its shape type"));
        }

        let rec = header_end;
        let shape_type = le_i32_at(&self.mmap[..rec + 4], rec);
        if shape_type == 0 {
            return Ok(false);
        }
        if !matches!(shape_type, 5 | 15 | 25) {
            return Err(self.invalid(record, "shape type is not Polygon, PolygonZ or PolygonM"));
        }
        if record.content_len < 44 {
            return Err(self.invalid(record, "polygon record is shorter than its fixed header"));
        }

        let mut header = [0_u8; 44];
        header.copy_from_slice(&self.mmap[rec..rec + 44]);

        let xmin = le_f64_at(&header, 4);
        let ymin = le_f64_at(&header, 12);
        let xmax = le_f64_at(&header, 20);
        let ymax = le_f64_at(&header, 28);
        if ![xmin, ymin, xmax, ymax]
            .iter()
            .all(|value| value.is_finite())
            || xmin > xmax
            || ymin > ymax
        {
            return Err(self.invalid(record, "polygon bounds are non-finite or inverted"));
        }

        let merc_min = geometry::projection::from_epsg3857(xmin, ymax);
        let merc_max = geometry::projection::from_epsg3857(xmax, ymin);
        if !in_world_domain(merc_min) || !in_world_domain(merc_max) {
            return Err(self.invalid(record, "polygon bounds lie outside the projected world"));
        }
        if !accept(merc_min, merc_max) {
            return Ok(false);
        }

        let source = RecordSource {
            shapefile: self,
            record,
            rec,
            header: &header,
        };
        source.read_polygons(&mut polygon)?;
        Ok(true)
    }

    fn invalid(&self, record: ShxRecord, reason: &str) -> ShapefileError {
        invalid_shapefile_record(&self.path, record, reason)
    }
}

fn parse_shx(shx_data: &[u8], shx_path: &Path) -> Result<Vec<ShxRecord>> {
    if shx_data.len() < 100 {
        return Err(ShapefileError::Malformed(format!(
            "invalid .shx file {}: expected at least 100-byte header, got {} bytes",
            shx_path.display(),
            shx_data.len()
        )));
    }

    let index_bytes = shx_data.len() - 100;
    if index_bytes % 8 != 0 {
        return Err(ShapefileError::Malformed(format!(
            "invalid .shx file {}: {} trailing bytes after complete index records",
            shx_path.display(),
            index_bytes % 8
        )));
    }

    let shape_count = index_bytes / 8;
    let mut records: Vec<ShxRecord> = Vec::with_capacity(shape_count);
    for i in 0..shape_count {
        let base = 100 + i * 8;
        let offset_words = i32::from_be_bytes(field_bytes(shx_data, base));
        let content_words = i32::from_be_bytes(field_bytes(shx_data, base + 4));
        if offset_words < 0 || content_words < 0 {
            return Err(ShapefileError::Malformed(format!(
                "invalid .shx record {} in {}: negative offset or content length",
                i + 1,
                shx_path.display()
            )));
        }
        let words_to_bytes = |words: i32, what: &str| {
            usize::try_from(words)
                .ok()
                .and_then(|words| words.checked_mul(2))
                .ok_or_else(|| {
                    ShapefileError::Malformed(format!(
                        "invalid .shx record {} in {}: {what} overflow",
                        i + 1,
                        shx_path.display()
                    ))
                })
        };
        let offset = words_to_bytes(offset_words, "offset")?;
        let content_len = words_to_bytes(content_words, "content length")?;
        records.push(ShxRecord {
            index: i + 1,
            offset,
            content_len,
        });
    }
    Ok(records)
}

/// The world, with one world width of headroom on every side: normalized
/// Mercator `[-1, 2]` on both axes. Water polygons lie inside `[0, 1]` and
/// are never antimeridian-unwrapped, so this is generous; what it buys is
/// that the integer polygon engine downstream can trust the points it gets.
/// On ocean's z7 and z14 grids the interval quantizes to at most `2^27` in
/// magnitude, inside the overlay engine's `2^30` coordinate interval. Finite
/// points outside it are corrupt input and fail the source, rather than
/// silently dropping water or overflowing the engine.
fn in_world_domain(p: geometry::projection::Point) -> bool {
    (-1.0..=2.0).contains(&p.x) && (-1.0..=2.0).contains(&p.y)
}

// Fixed-width field readers. Callers have already bounds-checked the slice;
// these are plain functions so the expect does not sit in a Result function.
fn field_bytes<const N: usize>(buf: &[u8], off: usize) -> [u8; N] {
    buf[off..off + N].try_into().expect("slice length equals N")
}

fn be_u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(field_bytes(buf, off))
}

fn le_i32_at(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(field_bytes(buf, off))
}

fn le_f64_at(buf: &[u8], off: usize) -> f64 {
    f64::from_le_bytes(field_bytes(buf, off))
}

fn u64_from_usize(v: usize) -> u64 {
    u64::try_from(v).expect("shapefile offset fits u64")
}

fn invalid_shapefile_record(path: &Path, record: ShxRecord, reason: &str) -> ShapefileError {
    ShapefileError::Malformed(format!(
        "invalid shapefile record {} at offset {} in {}: {reason}",
        record.index,
        record.offset,
        path.display()
    ))
}

/// One accepted polygon record: its index entry, absolute content offset and
/// the fixed 44-byte header already copied out of the mapping.
struct RecordSource<'a> {
    shapefile: &'a Shapefile,
    record: ShxRecord,
    rec: usize,
    header: &'a [u8; 44],
}

impl RecordSource<'_> {
    fn invalid(&self, reason: &str) -> ShapefileError {
        self.shapefile.invalid(self.record, reason)
    }

    fn read_polygons(&self, polygon: &mut impl FnMut(&[Point], &[Vec<Point>])) -> Result<()> {
        let num_parts_i32 = le_i32_at(self.header, 36);
        let num_points_i32 = le_i32_at(self.header, 40);
        if num_parts_i32 < 0 || num_points_i32 < 0 {
            return Err(self.invalid("negative part or point count"));
        }
        let num_parts =
            usize::try_from(num_parts_i32).map_err(|_| self.invalid("part count overflow"))?;
        let num_points =
            usize::try_from(num_points_i32).map_err(|_| self.invalid("point count overflow"))?;

        let parts_start = 44usize;
        let parts_bytes = num_parts
            .checked_mul(4)
            .ok_or_else(|| self.invalid("part index range overflow"))?;
        let points_start = parts_start
            .checked_add(parts_bytes)
            .ok_or_else(|| self.invalid("point range offset overflow"))?;
        let points_bytes = num_points
            .checked_mul(16)
            .ok_or_else(|| self.invalid("point range overflow"))?;
        let record_end = points_start
            .checked_add(points_bytes)
            .ok_or_else(|| self.invalid("record content range overflow"))?;
        let absolute_end = self
            .rec
            .checked_add(record_end)
            .ok_or_else(|| self.invalid("record file range overflow"))?;
        if record_end > self.record.content_len || absolute_end > self.shapefile.mmap.len() {
            return Err(self.invalid("part and point arrays extend past the record"));
        }
        if num_parts == 0 || num_points == 0 {
            return Err(self.invalid("polygon record has no parts or points"));
        }

        let mut ring_starts = self.read_ring_starts(num_parts)?;
        if ring_starts.first() != Some(&0)
            || ring_starts.last().is_some_and(|&last| last >= num_points)
            || ring_starts.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(self.invalid(
                "part indices are not strictly increasing from zero within the point array",
            ));
        }
        ring_starts.push(num_points);

        let mut current_outer: Option<Vec<Point>> = None;
        let mut current_inners: Vec<Vec<Point>> = Vec::new();

        for (w, window) in ring_starts.windows(2).enumerate() {
            let ring = self.read_ring_points(points_start, window[0], window[1])?;
            // Ring roles: see `Shapefile::read_polygons`.
            let is_outer = if w == 0 {
                true
            } else {
                let signed_area = geometry::polygon::signed_area(&ring);
                match geometry::polygon::ring_role(signed_area) {
                    Some(geometry::polygon::RingOrientation::Positive) => true,
                    Some(geometry::polygon::RingOrientation::Negative) => false,
                    None => signed_area == 0.0,
                }
            };

            if is_outer {
                if let Some(outer) = current_outer.take() {
                    polygon(&outer, &std::mem::take(&mut current_inners));
                }
                current_outer = Some(ring);
            } else if current_outer.is_some() {
                current_inners.push(ring);
            }
        }

        if let Some(outer) = current_outer {
            polygon(&outer, &current_inners);
        }
        Ok(())
    }

    fn read_ring_starts(&self, num_parts: usize) -> Result<Vec<usize>> {
        let mut parts = vec![0_u8; num_parts * 4];
        if !parts.is_empty() {
            let read_offset = u64_from_usize(self.rec + 44);
            self.shapefile.file.read_exact_at(&mut parts, read_offset)?;
        }
        (0..num_parts)
            .map(|j| {
                let v = le_i32_at(&parts, j * 4);
                usize::try_from(v).map_err(|_| self.invalid("negative part index"))
            })
            .collect()
    }

    fn read_ring_points(
        &self,
        points_start: usize,
        start: usize,
        end: usize,
    ) -> Result<Vec<Point>> {
        let point_count = end - start;
        if point_count < 4 {
            return Err(self.invalid("polygon ring has fewer than four points including closure"));
        }
        // The record's declared bounds already decided admission; a point
        // outside them means the header lied. Exact in file units: the full
        // and simplified OSMCD inputs (78M and 1.8M points) have no point
        // outside its record's bounds. Only admitted records are read, so a
        // header that understates its points can still have its record
        // skipped unseen - the prefilter trusts the header by design, and
        // this check catches the lie whenever the record is admitted.
        let (xmin, ymin) = (le_f64_at(self.header, 4), le_f64_at(self.header, 12));
        let (xmax, ymax) = (le_f64_at(self.header, 20), le_f64_at(self.header, 28));
        let mut ring = Vec::with_capacity(point_count);
        let mut buf = vec![0_u8; RING_READ_BUFFER_BYTES];
        let mut points_read = 0usize;

        while points_read < point_count {
            let points_this_read = ((point_count - points_read) * 16).min(buf.len()) / 16;
            let byte_len = points_this_read * 16;
            let read_offset = u64_from_usize(self.rec + points_start + (start + points_read) * 16);
            self.shapefile
                .file
                .read_exact_at(&mut buf[..byte_len], read_offset)?;
            for idx in 0..points_this_read {
                let b = idx * 16;
                let x = le_f64_at(&buf, b);
                let y = le_f64_at(&buf, b + 8);
                if !x.is_finite() || !y.is_finite() {
                    return Err(self.invalid("polygon point coordinate is non-finite"));
                }
                if !(xmin..=xmax).contains(&x) || !(ymin..=ymax).contains(&y) {
                    return Err(self.invalid("polygon point lies outside the record bounds"));
                }
                let point = geometry::projection::from_epsg3857(x, y);
                // Implied by the bounds check above: the record bounds were
                // world-checked, and the projection is monotone in each axis
                // under IEEE rounding, so a point inside them projects inside
                // the world. Kept as the explicit premise that the integer
                // quantizer's `expect` in `int_ocean::quantize_polygon`
                // names, so that premise does not ride on that argument.
                if !in_world_domain(point) {
                    return Err(self.invalid("polygon point lies outside the projected world"));
                }
                ring.push(point);
            }
            points_read += points_this_read;
        }
        // Shapefile closure repeats the endpoint exactly; it is not a proximity
        // check like the later geometry simplification tolerances.
        #[allow(clippy::float_cmp)]
        let closed = ring
            .first()
            .zip(ring.last())
            .is_some_and(|(first, last)| first.x == last.x && first.y == last.y);
        if !closed {
            return Err(self.invalid("polygon ring is not closed"));
        }
        Ok(ring)
    }
}

#[cfg(test)]
pub(super) mod test_files {
    //! Shapefile writers shared by the reader and emission tests.
    #![allow(clippy::unwrap_used)]
    use std::fs;
    use std::path::Path;

    pub(crate) fn write_u32_be(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn write_u32_le(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn write_f64_le(buf: &mut [u8], off: usize, v: f64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn write_single_record_polygon_shapefile(path: &Path, parts: &[Vec<(f64, f64)>]) {
        let num_parts = parts.len();
        let num_points: usize = parts.iter().map(std::vec::Vec::len).sum();
        assert!(num_parts > 0);
        assert!(num_points > 0);

        let mut xmin = f64::INFINITY;
        let mut ymin = f64::INFINITY;
        let mut xmax = f64::NEG_INFINITY;
        let mut ymax = f64::NEG_INFINITY;
        for ring in parts {
            for &(x, y) in ring {
                xmin = xmin.min(x);
                ymin = ymin.min(y);
                xmax = xmax.max(x);
                ymax = ymax.max(y);
            }
        }

        // Record content length (bytes):
        // shape_type(4) + bbox(32) + num_parts(4) + num_points(4) + parts(num_parts*4) + points(num_points*16)
        let record_content_bytes = 4 + 32 + 4 + 4 + num_parts * 4 + num_points * 16;
        let record_content_words =
            u32::try_from(record_content_bytes / 2).expect("record content length fits u32");
        let shp_file_len_words =
            u32::try_from((100 + 8 + record_content_bytes) / 2).expect("shp length fits u32");
        let shx_file_len_words = ((100 + 8) / 2) as u32;

        let mut shp = vec![0u8; 100 + 8 + record_content_bytes];
        let mut shx = vec![0u8; 108];

        // Common 100-byte file header (shp + shx)
        // Big-endian section
        write_u32_be(&mut shp, 0, 9994);
        write_u32_be(&mut shp, 24, shp_file_len_words);
        write_u32_be(&mut shx, 0, 9994);
        write_u32_be(&mut shx, 24, shx_file_len_words);
        // Little-endian section
        write_u32_le(&mut shp, 28, 1000); // version
        write_u32_le(&mut shp, 32, 5); // Polygon
        write_u32_le(&mut shx, 28, 1000);
        write_u32_le(&mut shx, 32, 5);
        // Header bbox
        write_f64_le(&mut shp, 36, xmin);
        write_f64_le(&mut shp, 44, ymin);
        write_f64_le(&mut shp, 52, xmax);
        write_f64_le(&mut shp, 60, ymax);
        write_f64_le(&mut shx, 36, xmin);
        write_f64_le(&mut shx, 44, ymin);
        write_f64_le(&mut shx, 52, xmax);
        write_f64_le(&mut shx, 60, ymax);

        // shx index record
        write_u32_be(&mut shx, 100, 50); // .shp record offset in 16-bit words (100 bytes)
        write_u32_be(&mut shx, 104, record_content_words);

        // shp record header
        let rec_header = 100;
        write_u32_be(&mut shp, rec_header, 1); // record number
        write_u32_be(&mut shp, rec_header + 4, record_content_words);

        // shp record content
        let rec = rec_header + 8;
        write_u32_le(&mut shp, rec, 5); // Polygon
        write_f64_le(&mut shp, rec + 4, xmin); // xmin
        write_f64_le(&mut shp, rec + 12, ymin); // ymin
        write_f64_le(&mut shp, rec + 20, xmax); // xmax
        write_f64_le(&mut shp, rec + 28, ymax); // ymax
        write_u32_le(
            &mut shp,
            rec + 36,
            u32::try_from(num_parts).expect("part count fits u32"),
        ); // num_parts
        write_u32_le(
            &mut shp,
            rec + 40,
            u32::try_from(num_points).expect("point count fits u32"),
        ); // num_points
        let mut part_start = 0usize;
        for (i, ring) in parts.iter().enumerate() {
            write_u32_le(
                &mut shp,
                rec + 44 + i * 4,
                u32::try_from(part_start).expect("part start fits u32"),
            );
            part_start += ring.len();
        }

        let mut p = rec + 44 + num_parts * 4;
        for ring in parts {
            for &(x, y) in ring {
                write_f64_le(&mut shp, p, x);
                write_f64_le(&mut shp, p + 8, y);
                p += 16;
            }
        }

        fs::write(path, shp).unwrap();
        fs::write(path.with_extension("shx"), shx).unwrap();
    }

    pub(crate) fn write_test_polygon_shapefile(path: &Path) {
        // One world-sized square in EPSG:3857 meters.
        let half_c = 20_037_508.343;
        let outer = vec![
            (-half_c, -half_c),
            (half_c, -half_c),
            (half_c, half_c),
            (-half_c, half_c),
            (-half_c, -half_c),
        ];
        write_single_record_polygon_shapefile(path, &[outer]);
    }

    pub(crate) fn write_test_polygon_with_hole_shapefile(path: &Path) {
        let half_c = 20_037_508.343;
        // ESRI winding in EPSG:3857 (y up): the outer is clockwise and the
        // hole counterclockwise. Projection flips y, so the hole's signed
        // area is negative in Mercator [0,1] and the reader makes it a hole.
        let outer = vec![
            (-half_c, -half_c),
            (-half_c, half_c),
            (half_c, half_c),
            (half_c, -half_c),
            (-half_c, -half_c),
        ];
        let hole = vec![
            (-half_c * 0.5, -half_c * 0.5),
            (half_c * 0.5, -half_c * 0.5),
            (half_c * 0.5, half_c * 0.5),
            (-half_c * 0.5, half_c * 0.5),
            (-half_c * 0.5, -half_c * 0.5),
        ];
        write_single_record_polygon_shapefile(path, &[outer, hole]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::test_files::*;
    use super::*;
    use std::fs;

    type Polygons = Vec<(Vec<Point>, Vec<Vec<Point>>)>;

    /// Read every record, accepting all of them, and collect the polygons.
    fn read_all(path: &Path) -> Result<(usize, Polygons)> {
        let shapefile = Shapefile::open(path)?;
        let mut hits = 0;
        let mut polygons = Vec::new();
        for &record in shapefile.records() {
            let hit = shapefile.read_polygons(
                record,
                |_, _| true,
                |outer, inners| polygons.push((outer.to_vec(), inners.to_vec())),
            )?;
            hits += usize::from(hit);
        }
        Ok((hits, polygons))
    }

    #[test]
    fn reads_projected_rings_grouped_into_outer_and_holes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hole.shp");
        write_test_polygon_with_hole_shapefile(&path);
        let (hits, polygons) = read_all(&path).unwrap();
        assert_eq!(hits, 1);
        assert_eq!(polygons.len(), 1, "the counterclockwise ring is a hole");
        let (outer, inners) = &polygons[0];
        assert_eq!(outer.len(), 5);
        assert_eq!(inners.len(), 1);
        let half_c = 20_037_508.343;
        let expected = geometry::projection::from_epsg3857(-half_c, -half_c);
        assert_eq!((outer[0].x, outer[0].y), (expected.x, expected.y));
    }

    #[test]
    fn a_later_ring_wound_like_an_outer_starts_a_second_polygon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("two.shp");
        // Clockwise in EPSG:3857, the ESRI outer winding.
        let square = |x0: f64, y0: f64, x1: f64, y1: f64| {
            vec![(x0, y0), (x0, y1), (x1, y1), (x1, y0), (x0, y0)]
        };
        write_single_record_polygon_shapefile(
            &path,
            &[
                square(0.0, 0.0, 1000.0, 1000.0),
                square(2000.0, 0.0, 3000.0, 1000.0),
            ],
        );
        let (_, polygons) = read_all(&path).unwrap();
        assert_eq!(polygons.len(), 2);
        assert!(polygons.iter().all(|(_, inners)| inners.is_empty()));
    }

    #[test]
    fn a_rejected_record_is_not_read_past_its_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rejected.shp");
        write_test_polygon_shapefile(&path);
        let mut shp = fs::read(&path).unwrap();
        // Corrupt the part index: an accepted record fails on it.
        write_u32_le(&mut shp, 100 + 8 + 44, 6);
        fs::write(&path, shp).unwrap();
        let shapefile = Shapefile::open(&path).unwrap();
        let record = shapefile.records()[0];
        let hit = shapefile
            .read_polygons(record, |_, _| false, |_, _| panic!("rejected"))
            .unwrap();
        assert!(!hit);
        let err = shapefile
            .read_polygons(record, |_, _| true, |_, _| {})
            .expect_err("accepted record reads the bad part index");
        assert!(err.to_string().contains("part indices"));
    }

    #[test]
    fn a_bad_shape_type_fails_even_when_the_record_would_be_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("type.shp");
        write_test_polygon_shapefile(&path);
        let mut shp = fs::read(&path).unwrap();
        write_u32_le(&mut shp, 108, 3);
        fs::write(&path, shp).unwrap();
        let shapefile = Shapefile::open(&path).unwrap();
        let record = shapefile.records()[0];
        assert!(
            shapefile
                .read_polygons(record, |_, _| false, |_, _| {})
                .is_err()
        );
    }

    #[test]
    fn rejects_short_shx_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ocean_bad_shx.shp");
        write_test_polygon_shapefile(&path);
        fs::write(path.with_extension("shx"), [0u8; 64]).unwrap();
        let err = Shapefile::open(&path)
            .err()
            .expect("short .shx header should fail");
        assert!(matches!(err, ShapefileError::Malformed(_)), "{err}");
        assert!(err.to_string().contains("invalid .shx file"));
    }

    #[test]
    fn a_bad_index_is_reported_before_the_shp_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index_first.shp");
        write_test_polygon_shapefile(&path);
        fs::write(path.with_extension("shx"), [0u8; 64]).unwrap();
        fs::remove_file(&path).unwrap();
        let err = Shapefile::open(&path).err().expect("bad index");
        assert!(matches!(err, ShapefileError::Malformed(_)), "{err}");
    }

    #[test]
    fn errors_when_shx_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ocean_missing_shx.shp");
        write_test_polygon_shapefile(&path);
        fs::remove_file(path.with_extension("shx")).unwrap();
        let err = Shapefile::open(&path)
            .err()
            .expect("missing .shx should fail");
        assert!(
            matches!(&err, ShapefileError::Read(io) if io.kind() == std::io::ErrorKind::NotFound),
            "{err}"
        );
    }

    #[test]
    fn rejects_truncated_shp_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ocean_truncated_shp.shp");
        write_test_polygon_shapefile(&path);
        let mut shp_data = fs::read(&path).unwrap();
        shp_data.truncate(120);
        fs::write(&path, shp_data).unwrap();
        let err = read_all(&path).expect_err("truncated record should fail");
        assert!(matches!(err, ShapefileError::Malformed(_)), "{err}");
        assert!(err.to_string().contains("extends past the .shp file"));
    }

    #[test]
    fn rejects_negative_shx_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ocean_negative_offset.shp");
        write_test_polygon_shapefile(&path);
        let mut shx = fs::read(path.with_extension("shx")).unwrap();
        write_u32_be(&mut shx, 100, u32::MAX);
        fs::write(path.with_extension("shx"), shx).unwrap();
        let err = Shapefile::open(&path)
            .err()
            .expect("negative .shx offset should fail");
        assert!(matches!(err, ShapefileError::Malformed(_)), "{err}");
        assert!(err.to_string().contains("negative offset"));
    }

    #[test]
    fn rejects_malformed_polygon_geometry() {
        for defect in [
            "type",
            "bounds",
            "world_bounds",
            "coordinate",
            "world_point",
            "short_ring",
            "closure",
            "outside_bounds",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("malformed.shp");
            write_test_polygon_shapefile(&path);
            let mut shp = fs::read(&path).unwrap();
            let rec = 108;
            let points = rec + 48;
            match defect {
                "type" => write_u32_le(&mut shp, rec, 3),
                "bounds" => write_f64_le(&mut shp, rec + 4, f64::NAN),
                // Finite but far outside the world: once aborted the run at
                // quantization, or slipped past it and overflowed the engine.
                "world_bounds" => write_f64_le(&mut shp, rec + 4, -1.0e12),
                "coordinate" => write_f64_le(&mut shp, points + 16, f64::INFINITY),
                "world_point" => write_f64_le(&mut shp, points + 16, 1.0e12),
                "short_ring" => write_u32_le(&mut shp, rec + 40, 3),
                "closure" => write_f64_le(&mut shp, points + 4 * 16, 0.0),
                // A header understating its points: xmax shrunk to xmin.
                "outside_bounds" => {
                    let xmin = f64::from_le_bytes(shp[rec + 4..rec + 12].try_into().unwrap());
                    write_f64_le(&mut shp, rec + 20, xmin);
                }
                _ => unreachable!(),
            }
            fs::write(&path, shp).unwrap();
            let err = read_all(&path).expect_err(defect);
            assert!(
                matches!(err, ShapefileError::Malformed(_)),
                "{defect}: {err}"
            );
            if defect == "outside_bounds" {
                assert!(
                    err.to_string().contains("outside the record bounds"),
                    "{err}"
                );
            }
        }
    }

    #[test]
    fn hashes_the_buffers_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hash.shp");
        write_test_polygon_shapefile(&path);
        let hashes = Shapefile::open(&path).unwrap().hashes();
        assert_eq!(hashes.shp_xxh128, Xxh128::hash_file(&path).unwrap().0);
        assert_eq!(
            hashes.shx_xxh128,
            Xxh128::hash_file(&path.with_extension("shx")).unwrap().0
        );
    }
}
