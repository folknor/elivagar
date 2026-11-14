//! The PMTiles v3 header: 127 bytes, one codec.
//!
//! Decoding is lossless. Every byte the header can hold has a representation
//! here - enum bytes the spec does not define decode to an `Other` arm,
//! coordinates decode to their raw E7 integers whatever they say - so
//! `Header::decode(b).encode() == b` for every 127-byte input with the v3
//! magic. Only the magic and the version are refused, because without them the
//! bytes are not a PMTiles v3 header at all.
//!
//! Lossless decoding is deliberate: `inspect` is a diagnostic and must still
//! print the sections, counts and bounds of an archive whose tile type or
//! compression it cannot read. Whether an archive is READABLE for a workflow
//! is a separate question, answered by [`TileDecoder::for_contract`] and by
//! the reader's layout and section checks.

use std::io;

/// The header occupies the first 127 bytes of every PMTiles v3 archive.
pub const HEADER_LEN: usize = 127;

const MAGIC: &[u8; 7] = b"PMTiles";
const VERSION: u8 = 3;

/// A byte range of the archive, as the header declares it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Section {
    pub offset: u64,
    pub length: u64,
}

impl Section {
    /// The exclusive end, or `None` when `offset + length` overflows.
    pub fn end(self) -> Option<u64> {
        self.offset.checked_add(self.length)
    }
}

/// Header compression byte, shared by the internal (directories, metadata)
/// and the tile compression fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compression {
    /// The spec's 0: the producer did not say.
    Unknown,
    None,
    Gzip,
    Brotli,
    Zstd,
    /// A byte the v3 spec does not define.
    Other(u8),
}

impl Compression {
    fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Unknown,
            1 => Self::None,
            2 => Self::Gzip,
            3 => Self::Brotli,
            4 => Self::Zstd,
            other => Self::Other(other),
        }
    }

    fn byte(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::None => 1,
            Self::Gzip => 2,
            Self::Brotli => 3,
            Self::Zstd => 4,
            Self::Other(byte) => byte,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::None => "none",
            Self::Gzip => "gzip",
            Self::Brotli => "brotli",
            Self::Zstd => "zstd",
            Self::Other(_) => "unrecognized",
        }
    }
}

/// The name, with the raw byte for a value the spec does not define.
impl std::fmt::Display for Compression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(byte) => write!(f, "unrecognized ({byte})"),
            known => f.write_str(known.name()),
        }
    }
}

/// The name, with the raw byte for a value the spec does not define.
impl std::fmt::Display for TileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(byte) => write!(f, "unrecognized ({byte})"),
            known => f.write_str(known.name()),
        }
    }
}

/// Header tile type byte.
///
/// The writer records MLT as `Unknown` (with an explicit metadata marker);
/// newer spec revisions assign MLT a byte of its own, which decodes here as
/// `Other`, preserved rather than named, because adopting it would change the
/// writer's output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileType {
    Unknown,
    Mvt,
    Png,
    Jpeg,
    Webp,
    Avif,
    /// A byte the spec revision this codec follows does not define.
    Other(u8),
}

impl TileType {
    fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Unknown,
            1 => Self::Mvt,
            2 => Self::Png,
            3 => Self::Jpeg,
            4 => Self::Webp,
            5 => Self::Avif,
            other => Self::Other(other),
        }
    }

    fn byte(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Mvt => 1,
            Self::Png => 2,
            Self::Jpeg => 3,
            Self::Webp => 4,
            Self::Avif => 5,
            Self::Other(byte) => byte,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Mvt => "MVT",
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Webp => "WebP",
            Self::Avif => "AVIF",
            Self::Other(_) => "unrecognized",
        }
    }
}

/// Header clustered byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Clustering {
    Unclustered,
    Clustered,
    /// A byte other than the spec's 0 or 1.
    Other(u8),
}

impl Clustering {
    fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Unclustered,
            1 => Self::Clustered,
            other => Self::Other(other),
        }
    }

    fn byte(self) -> u8 {
        match self {
            Self::Unclustered => 0,
            Self::Clustered => 1,
            Self::Other(byte) => byte,
        }
    }
}

/// Header bounds as stored: signed E7 integers (degrees times 10^7), named by
/// side so longitude and latitude cannot be swapped by position. These are
/// raw wire values - a decoded header may carry any integers here; only the
/// writer's [`LonLatBbox::to_e7`] guarantees a valid box.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct E7Bbox {
    pub west: i32,
    pub south: i32,
    pub east: i32,
    pub north: i32,
}

/// Header center as stored: E7 longitude and latitude plus a zoom. Raw wire
/// values, like [`E7Bbox`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct E7Center {
    pub lon: i32,
    pub lat: i32,
    pub zoom: u8,
}

/// Convert an E7 integer back to degrees.
pub fn e7_to_degrees(value: i32) -> f64 {
    f64::from(value) / 1e7
}

/// Round degrees to E7. The caller has checked the value is finite and within
/// +-180, so the product fits an `i32`.
#[allow(clippy::cast_possible_truncation)]
fn degrees_to_e7(value: f64) -> i32 {
    (value * 1e7).round() as i32
}

fn check_degrees(what: &str, value: f64, limit: f64) -> io::Result<()> {
    if value.is_finite() && value.abs() <= limit {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("PMTiles {what} {value} is not a finite coordinate within +-{limit}"),
        ))
    }
}

/// Geographic bounds in WGS84 degrees, named by side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LonLatBbox {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

impl LonLatBbox {
    /// Validate and round to the header's E7 form. Every value must be finite
    /// and in range; an inverted box is refused, because the header has no
    /// antimeridian-crossing representation and swapping sides would invent
    /// a different box.
    pub fn to_e7(self) -> io::Result<E7Bbox> {
        check_degrees("west bound", self.west, 180.0)?;
        check_degrees("east bound", self.east, 180.0)?;
        check_degrees("south bound", self.south, 90.0)?;
        check_degrees("north bound", self.north, 90.0)?;
        if self.west > self.east || self.south > self.north {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("PMTiles bounds are inverted: {self:?}"),
            ));
        }
        Ok(E7Bbox {
            west: degrees_to_e7(self.west),
            south: degrees_to_e7(self.south),
            east: degrees_to_e7(self.east),
            north: degrees_to_e7(self.north),
        })
    }
}

/// Default map center in WGS84 degrees plus a zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapCenter {
    pub lon: f64,
    pub lat: f64,
    pub zoom: u8,
}

impl MapCenter {
    /// Validate and round to the header's E7 form. Containment in the bounds
    /// is not checked: that is a policy, not coordinate validity.
    pub fn to_e7(self) -> io::Result<E7Center> {
        check_degrees("center longitude", self.lon, 180.0)?;
        check_degrees("center latitude", self.lat, 90.0)?;
        Ok(E7Center {
            lon: degrees_to_e7(self.lon),
            lat: degrees_to_e7(self.lat),
            zoom: self.zoom,
        })
    }
}

/// A decoded PMTiles v3 header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub root_dir: Section,
    pub metadata: Section,
    pub leaf_dirs: Section,
    pub tile_data: Section,
    pub addressed_tiles: u64,
    pub tile_entries: u64,
    pub tile_contents: u64,
    pub clustering: Clustering,
    pub internal_compression: Compression,
    pub tile_compression: Compression,
    pub tile_type: TileType,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds: E7Bbox,
    pub center: E7Center,
}

fn u64_at(bytes: &[u8; HEADER_LEN], offset: usize) -> u64 {
    let mut word = [0_u8; 8];
    word.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(word)
}

fn i32_at(bytes: &[u8; HEADER_LEN], offset: usize) -> i32 {
    let mut word = [0_u8; 4];
    word.copy_from_slice(&bytes[offset..offset + 4]);
    i32::from_le_bytes(word)
}

fn put_u64(bytes: &mut [u8; HEADER_LEN], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_i32(bytes: &mut [u8; HEADER_LEN], offset: usize, value: i32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

impl Header {
    /// Decode the first 127 bytes of an archive. Refuses only a missing magic
    /// or a version other than 3.
    pub fn decode(bytes: &[u8; HEADER_LEN]) -> io::Result<Self> {
        if &bytes[..7] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a PMTiles archive (magic mismatch)",
            ));
        }
        if bytes[7] != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("not PMTiles v3 (version byte {})", bytes[7]),
            ));
        }
        let section = |offset| Section {
            offset: u64_at(bytes, offset),
            length: u64_at(bytes, offset + 8),
        };
        Ok(Self {
            root_dir: section(8),
            metadata: section(24),
            leaf_dirs: section(40),
            tile_data: section(56),
            addressed_tiles: u64_at(bytes, 72),
            tile_entries: u64_at(bytes, 80),
            tile_contents: u64_at(bytes, 88),
            clustering: Clustering::from_byte(bytes[96]),
            internal_compression: Compression::from_byte(bytes[97]),
            tile_compression: Compression::from_byte(bytes[98]),
            tile_type: TileType::from_byte(bytes[99]),
            min_zoom: bytes[100],
            max_zoom: bytes[101],
            bounds: E7Bbox {
                west: i32_at(bytes, 102),
                south: i32_at(bytes, 106),
                east: i32_at(bytes, 110),
                north: i32_at(bytes, 114),
            },
            center: E7Center {
                zoom: bytes[118],
                lon: i32_at(bytes, 119),
                lat: i32_at(bytes, 123),
            },
        })
    }

    /// Encode to the 127-byte wire form.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0_u8; HEADER_LEN];
        bytes[..7].copy_from_slice(MAGIC);
        bytes[7] = VERSION;
        for (offset, section) in [
            (8, self.root_dir),
            (24, self.metadata),
            (40, self.leaf_dirs),
            (56, self.tile_data),
        ] {
            put_u64(&mut bytes, offset, section.offset);
            put_u64(&mut bytes, offset + 8, section.length);
        }
        put_u64(&mut bytes, 72, self.addressed_tiles);
        put_u64(&mut bytes, 80, self.tile_entries);
        put_u64(&mut bytes, 88, self.tile_contents);
        bytes[96] = self.clustering.byte();
        bytes[97] = self.internal_compression.byte();
        bytes[98] = self.tile_compression.byte();
        bytes[99] = self.tile_type.byte();
        bytes[100] = self.min_zoom;
        bytes[101] = self.max_zoom;
        put_i32(&mut bytes, 102, self.bounds.west);
        put_i32(&mut bytes, 106, self.bounds.south);
        put_i32(&mut bytes, 110, self.bounds.east);
        put_i32(&mut bytes, 114, self.bounds.north);
        bytes[118] = self.center.zoom;
        put_i32(&mut bytes, 119, self.center.lon);
        put_i32(&mut bytes, 123, self.center.lat);
        bytes
    }

    /// The four sections by name, in header field order (not file order).
    pub fn sections(&self) -> [(&'static str, Section); 4] {
        [
            ("root_dir", self.root_dir),
            ("metadata", self.metadata),
            ("leaf_dirs", self.leaf_dirs),
            ("tile_data", self.tile_data),
        ]
    }
}

/// Tile decoder selected for archive workflows which parse MVT payloads.
///
/// The MVT validation, rendering and archive-splice paths support MVT with
/// gzip only. This is the one readability decision; it takes header types,
/// not an archive, so a run can ask it of its own output contract before any
/// archive exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileDecoder {
    MvtGzip,
}

impl TileDecoder {
    pub fn for_contract(tile_type: TileType, tile_compression: Compression) -> io::Result<Self> {
        if tile_type != TileType::Mvt {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("expected tile type MVT, got {tile_type}"),
            ));
        }
        if tile_compression != Compression::Gzip {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("expected tile compression gzip, got {tile_compression}"),
            ));
        }
        Ok(Self::MvtGzip)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::MvtGzip => "MVT + gzip",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_then_encode_is_lossless() {
        // A deterministic pseudo-random walk over header bodies, including
        // enum bytes outside the spec and arbitrary coordinates.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        for _ in 0..2000 {
            let mut bytes = [0_u8; HEADER_LEN];
            bytes[..7].copy_from_slice(MAGIC);
            bytes[7] = VERSION;
            for byte in &mut bytes[8..] {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state.to_le_bytes()[0];
            }
            let header = Header::decode(&bytes).expect("v3 magic decodes");
            assert_eq!(header.encode(), bytes);
        }
    }

    #[test]
    fn decode_refuses_only_magic_and_version() {
        let mut bytes = Header::decode(&{
            let mut b = [0_u8; HEADER_LEN];
            b[..7].copy_from_slice(MAGIC);
            b[7] = VERSION;
            b
        })
        .expect("zero body decodes")
        .encode();
        bytes[7] = 2;
        assert!(Header::decode(&bytes).is_err());
        bytes[7] = VERSION;
        bytes[0] = b'X';
        assert!(Header::decode(&bytes).is_err());
    }

    #[test]
    fn unrecognized_bytes_survive_decode() {
        let mut bytes = [0_u8; HEADER_LEN];
        bytes[..7].copy_from_slice(MAGIC);
        bytes[7] = VERSION;
        bytes[96] = 2;
        bytes[98] = 7;
        bytes[99] = 9;
        let header = Header::decode(&bytes).expect("decodes");
        assert_eq!(header.clustering, Clustering::Other(2));
        assert_eq!(header.tile_compression, Compression::Other(7));
        assert_eq!(header.tile_type, TileType::Other(9));
        assert!(TileDecoder::for_contract(header.tile_type, header.tile_compression).is_err());
    }

    #[test]
    fn degree_conversion_rounds_and_validates() {
        let bbox = LonLatBbox {
            west: 8.000_000_06,
            south: 54.5,
            east: 13.0,
            north: 58.0,
        }
        .to_e7()
        .expect("valid box");
        assert_eq!(bbox.west, 80_000_001);
        assert_eq!(bbox.north, 580_000_000);
        for bad in [
            LonLatBbox {
                west: f64::NAN,
                south: 0.0,
                east: 1.0,
                north: 1.0,
            },
            LonLatBbox {
                west: -181.0,
                south: 0.0,
                east: 1.0,
                north: 1.0,
            },
            LonLatBbox {
                west: 0.0,
                south: 0.0,
                east: 1.0,
                north: 91.0,
            },
            LonLatBbox {
                west: 2.0,
                south: 0.0,
                east: 1.0,
                north: 1.0,
            },
            LonLatBbox {
                west: 0.0,
                south: 2.0,
                east: 1.0,
                north: 1.0,
            },
        ] {
            assert!(bad.to_e7().is_err(), "{bad:?} must be refused");
        }
        assert!(
            MapCenter {
                lon: 0.0,
                lat: f64::INFINITY,
                zoom: 0
            }
            .to_e7()
            .is_err()
        );
    }
}
