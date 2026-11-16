//! `mutate` - the calibration instrument. Rewrites an archive with one
//! controlled mutation while preserving its PMTiles header configuration and
//! metadata byte-for-byte after decompression. Not a production tile-editing
//! API; its sole consumer is the calibration suite (`brokkr.md`, Calibration).
//!
//! Brokkr-owned adjudication tooling, ported over elivagar's writer surface
//! (`eliv::PmtilesWriter`, incl. the one mutate-only `set_metadata_verbatim`).
//! The protobuf field surgery below is self-contained (no elivagar decoder).

use std::io::{self, Read, Write};
use std::path::Path;

use clap::ValueEnum;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use super::super::constants::mvt;
use super::super::eliv::{
    ArchiveView, Header, LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter, RawDirEntry, TileId,
    WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT, checked_tile_id, e7_to_degrees, encode_varint,
    read_varint,
};

/// A deliberately small set of direct archive mutations used to calibrate the
/// corpus gate. `drop-tile`/`nudge-geometry`/`layer-version` must FIRE with the
/// target named; `regzip` (byte-different, semantically equal) must CLEAR.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum MutationOp {
    DropTile,
    NudgeGeometry,
    LayerVersion,
    Regzip,
}

impl MutationOp {
    /// Canonical command-line spelling, used in generated calibrand names.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DropTile => "drop-tile",
            Self::NudgeGeometry => "nudge-geometry",
            Self::LayerVersion => "layer-version",
            Self::Regzip => "regzip",
        }
    }
}

/// Rewrite `input` into `output` applying one mutation, header + metadata
/// preserved verbatim.
pub fn mutate(
    input: &Path,
    output: &Path,
    target: Option<(u8, u32, u32)>,
    op: MutationOp,
) -> io::Result<()> {
    if op != MutationOp::Regzip && target.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--tile is required for this mutation",
        ));
    }
    let archive = ArchiveView::open(input)?;
    let target_id = target
        .map(|(z, x, y)| checked_tile_id(z, x, y))
        .transpose()?;
    let mut found = op == MutationOp::Regzip;
    let mut writer = PmtilesWriter::new(header_config(archive.header())?);
    writer.set_metadata_verbatim(archive.metadata()?);
    for run in archive.read_all_runs()? {
        let is_target = op != MutationOp::Regzip
            && target_id.is_some_and(|id| {
                id >= run.tile_id && id < run.tile_id + u64::from(run.run_length)
            });
        if is_target {
            found = true;
            let Some(id) = target_id else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "targeted mutation is missing its tile id",
                ));
            };
            copy_run(&mut writer, &archive, run, run.tile_id, id - run.tile_id)?;
            match op {
                MutationOp::DropTile => {}
                MutationOp::NudgeGeometry | MutationOp::LayerVersion => {
                    let raw = archive.raw_blob(run.blob())?;
                    let edited = mutate_payload(&raw, op)?;
                    writer.add_run(archive_tile_id(id)?, 1, &edited)?;
                }
                MutationOp::Regzip => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "regzip cannot target one tile",
                    ));
                }
            }
            let after = id + 1;
            copy_run(
                &mut writer,
                &archive,
                run,
                after,
                run.tile_id + u64::from(run.run_length) - after,
            )?;
        } else if op == MutationOp::Regzip {
            let raw = archive.raw_blob(run.blob())?;
            let recompressed = regzip(&raw)?;
            writer.add_run(archive_tile_id(run.tile_id)?, run.run_length, &recompressed)?;
        } else {
            copy_run(
                &mut writer,
                &archive,
                run,
                run.tile_id,
                u64::from(run.run_length),
            )?;
        }
    }
    if !found {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "target tile is not addressed by archive",
        ));
    }
    writer.write_to(output)
}

/// The writer configuration that reproduces `header`'s zoom range and
/// geography. E7 to degrees and back rounds to the same integers, so the
/// rewritten header carries the source's bounds and center exactly.
fn header_config(header: &Header) -> io::Result<PmtilesConfig> {
    PmtilesConfig::new(
        header.min_zoom,
        header.max_zoom,
        LonLatBbox {
            west: e7_to_degrees(header.bounds.west),
            south: e7_to_degrees(header.bounds.south),
            east: e7_to_degrees(header.bounds.east),
            north: e7_to_degrees(header.bounds.north),
        },
        MapCenter {
            lon: e7_to_degrees(header.center.lon),
            lat: e7_to_degrees(header.center.lat),
            zoom: header.center.zoom,
        },
    )
}

/// A run start read back from the source archive's validated directory.
fn archive_tile_id(raw: u64) -> io::Result<TileId> {
    TileId::new(raw).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("tile id {raw} is past the last z31 tile"),
        )
    })
}

fn copy_run(
    writer: &mut PmtilesWriter,
    archive: &ArchiveView,
    run: RawDirEntry,
    tile_id: u64,
    length: u64,
) -> io::Result<()> {
    if length != 0 {
        let length = u32::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "run too long"))?;
        writer.add_run(
            archive_tile_id(tile_id)?,
            length,
            &archive.raw_blob(run.blob())?,
        )?;
    }
    Ok(())
}

fn regzip(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::new();
    GzDecoder::new(raw).read_to_end(&mut decoded)?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(9));
    encoder.write_all(&decoded)?;
    encoder.finish()
}

#[derive(Clone)]
struct Field {
    number: u32,
    wire: u32,
    value: Vec<u8>,
}

fn fields(input: &[u8]) -> io::Result<Vec<Field>> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < input.len() {
        let tag = read_varint(input, &mut at)?;
        let number = u32::try_from(tag >> mvt::FIELD_NUMBER_SHIFT)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "field number too large"))?;
        let wire = u32::try_from(tag & mvt::FIELD_WIRE_MASK).unwrap_or(u32::MAX);
        let value = match wire {
            WIRE_VARINT => {
                let start = at;
                read_varint(input, &mut at)?;
                input[start..at].to_vec()
            }
            WIRE_64BIT => take(input, &mut at, 8)?,
            WIRE_LEN => {
                let n = usize::try_from(read_varint(input, &mut at)?)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "length too large"))?;
                take(input, &mut at, n)?
            }
            WIRE_32BIT => take(input, &mut at, 4)?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported protobuf wire type {wire}"),
                ));
            }
        };
        out.push(Field {
            number,
            wire,
            value,
        });
    }
    Ok(out)
}

fn take(input: &[u8], at: &mut usize, n: usize) -> io::Result<Vec<u8>> {
    let end = at
        .checked_add(n)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "protobuf length overflows"))?;
    let value = input
        .get(*at..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated protobuf field"))?
        .to_vec();
    *at = end;
    Ok(value)
}

fn encode_fields(fields: &[Field]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        encode_varint(
            &mut out,
            u64::from(field.number) << mvt::FIELD_NUMBER_SHIFT | u64::from(field.wire),
        );
        if field.wire == WIRE_LEN {
            encode_varint(&mut out, field.value.len() as u64);
        }
        out.extend_from_slice(&field.value);
    }
    out
}

fn mutate_payload(raw: &[u8], op: MutationOp) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::new();
    GzDecoder::new(raw).read_to_end(&mut decoded)?;
    let mut tile = fields(&decoded)?;
    let layer = tile
        .iter_mut()
        .find(|f| f.number == mvt::TILE_LAYER && f.wire == WIRE_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tile has no layer"))?;
    let mut layer_fields = fields(&layer.value)?;
    match op {
        MutationOp::LayerVersion => {
            if let Some(version) = layer_fields
                .iter_mut()
                .find(|f| f.number == mvt::layer::VERSION && f.wire == WIRE_VARINT)
            {
                version.value = vec![mvt::MUTATION_LAYER_VERSION];
            } else {
                layer_fields.push(Field {
                    number: mvt::layer::VERSION,
                    wire: WIRE_VARINT,
                    value: vec![mvt::MUTATION_LAYER_VERSION],
                });
            }
        }
        MutationOp::NudgeGeometry => {
            let feature_index = layer_fields
                .iter()
                .enumerate()
                .find_map(|(index, field)| {
                    (field.number == mvt::layer::FEATURE
                        && field.wire == WIRE_LEN
                        && fields(&field.value).ok()?.iter().any(|nested| {
                            nested.number == mvt::feature::GEOMETRY && nested.wire == WIRE_LEN
                        }))
                    .then_some(index)
                })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "layer has no feature geometry")
                })?;
            let feature = &mut layer_fields[feature_index];
            let mut feature_fields = fields(&feature.value)?;
            let geometry = feature_fields
                .iter_mut()
                .find(|f| f.number == mvt::feature::GEOMETRY && f.wire == WIRE_LEN)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "feature has no geometry")
                })?;
            geometry.value = nudge_geometry(&geometry.value)?;
            feature.value = encode_fields(&feature_fields);
        }
        MutationOp::DropTile | MutationOp::Regzip => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mutation does not edit tile geometry or layer version",
            ));
        }
    }
    layer.value = encode_fields(&layer_fields);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&encode_fields(&tile))?;
    encoder.finish()
}

fn nudge_geometry(input: &[u8]) -> io::Result<Vec<u8>> {
    let mut at = 0;
    while at < input.len() {
        let start = at;
        let command = read_varint(input, &mut at)?;
        let command_type =
            u32::try_from(command & u64::from(mvt::COMMAND_ID_MASK)).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "geometry command id too large")
            })?;
        if command_type == mvt::COMMAND_MOVE_TO && command >> mvt::COMMAND_COUNT_SHIFT != 0 {
            let parameter = read_varint(input, &mut at)?;
            let magnitude = i64::try_from(parameter >> 1).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "geometry delta too large")
            })?;
            let delta = if parameter & 1 == 0 {
                magnitude
            } else {
                -magnitude - 1
            };
            let changed_delta = delta.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "geometry delta overflow")
            })?;
            let changed = if changed_delta >= 0 {
                u64::try_from(changed_delta).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "geometry delta conversion failed",
                    )
                })? * 2
            } else {
                changed_delta.unsigned_abs() * 2 - 1
            };
            let mut out = input[..start].to_vec();
            encode_varint(&mut out, command);
            encode_varint(&mut out, changed);
            out.extend_from_slice(&input[at..]);
            return Ok(out);
        }
        // Skip the command's parameters. Arity is per command id, not uniform:
        // MoveTo/LineTo carry two varints per count, ClosePath carries none.
        // (Unreachable for valid MVT, where geometry opens with a MoveTo and
        // the loop above has already returned - robustness only.)
        let count = usize::try_from(command >> mvt::COMMAND_COUNT_SHIFT)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "geometry count too large"))?;
        let parameters = match command_type {
            mvt::COMMAND_MOVE_TO | mvt::COMMAND_LINE_TO => count.saturating_mul(2),
            mvt::COMMAND_CLOSE_PATH => 0,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown geometry command {other}"),
                ));
            }
        };
        for _ in 0..parameters {
            read_varint(input, &mut at)?;
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "geometry has no MoveTo",
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::let_underscore_must_use)]
mod calibration {
    //! The mutate calibrands (ported from elivagar's `corpus::mutation_tests`):
    //! `drop-tile`/`nudge-geometry`/`layer-version` must change the target
    //! tile's canonical hash and nothing else, and `regzip` (byte-different)
    //! must be semantically neutral. Built over the real `PmtilesWriter` +
    //! canonical hash on a synthetic archive - no external data - so they run in
    //! the normal test suite (`brokkr.md`, Calibration).

    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::super::super::constants::mvt;
    use super::super::super::eliv::{
        ArchiveView, LonLatBbox, MapCenter, PmtilesConfig, PmtilesWriter, TileId, WIRE_LEN,
        WIRE_VARINT, tile_id_to_zxy,
    };
    use super::super::compute;
    use super::super::digest::{DigestMode, LeafRun};
    use super::{Field, MutationOp, encode_fields, encode_varint, mutate};

    fn fixture_payload() -> Vec<u8> {
        // One point feature: geometry MoveTo(1) at (0,0); standard extent/version.
        let mut extent = Vec::new();
        encode_varint(&mut extent, u64::from(mvt::TILE_EXTENT));
        let mut geometry = Vec::new();
        encode_varint(&mut geometry, (1 << 3) | u64::from(mvt::COMMAND_MOVE_TO));
        encode_varint(&mut geometry, 0);
        encode_varint(&mut geometry, 0);
        let feature = encode_fields(&[
            Field {
                number: mvt::feature::GEOMETRY_TYPE,
                wire: WIRE_VARINT,
                value: vec![mvt::GEOMETRY_POINT],
            },
            Field {
                number: mvt::feature::GEOMETRY,
                wire: WIRE_LEN,
                value: geometry,
            },
        ]);
        let layer = encode_fields(&[
            Field {
                number: mvt::layer::NAME,
                wire: WIRE_LEN,
                value: b"test".to_vec(),
            },
            Field {
                number: mvt::layer::FEATURE,
                wire: WIRE_LEN,
                value: feature,
            },
            Field {
                number: mvt::layer::EXTENT,
                wire: WIRE_VARINT,
                value: extent,
            },
            Field {
                number: mvt::layer::VERSION,
                wire: WIRE_VARINT,
                value: vec![u8::try_from(mvt::FIXTURE_LAYER_VERSION).unwrap()],
            },
        ]);
        let tile = encode_fields(&[Field {
            number: mvt::TILE_LAYER,
            wire: WIRE_LEN,
            value: layer,
        }]);
        let mut gzip = GzEncoder::new(Vec::new(), Compression::new(6));
        gzip.write_all(&tile).unwrap();
        gzip.finish().unwrap()
    }

    // A z1 run of 4 tiles sharing one blob; the target is the second tile.
    fn source(path: &Path) -> u64 {
        let payload = fixture_payload();
        let config = PmtilesConfig::new(
            1,
            1,
            LonLatBbox {
                west: 0.0,
                south: 0.0,
                east: 1.0,
                north: 1.0,
            },
            MapCenter {
                lon: 0.0,
                lat: 0.0,
                zoom: 1,
            },
        )
        .unwrap();
        let mut writer = PmtilesWriter::new(config);
        let first = TileId::at(1, 0, 0);
        writer.add_run(first, 4, &payload).unwrap();
        writer.write_to(path).unwrap();
        first.raw() + 1
    }

    fn leaf_hashes(leaves: &[LeafRun]) -> BTreeMap<u64, u128> {
        leaves
            .iter()
            .flat_map(|leaf| {
                (leaf.tile_id..leaf.tile_id + u64::from(leaf.run_length))
                    .map(move |id| (id, leaf.hash))
            })
            .collect()
    }

    #[test]
    fn mutations_are_isolated_and_regzip_is_semantically_neutral() {
        let scratch = super::super::fixture::TestDir::new("corpus-mutation");
        let dir = &scratch.path;
        let sort_scratch = dir.join("sort-scratch");
        let input = dir.join("source.pmtiles");
        let target = source(&input);
        let before = ArchiveView::open(&input).unwrap();
        let (before_digest, before_leaves) =
            compute(&before, DigestMode::Leaves, &sort_scratch).unwrap();
        let before_meta = before.metadata().unwrap();

        for (op, name) in [
            (MutationOp::DropTile, "drop"),
            (MutationOp::NudgeGeometry, "nudge"),
            (MutationOp::LayerVersion, "version"),
        ] {
            let output = dir.join(format!("{name}.pmtiles"));
            mutate(&input, &output, Some(tile_id_to_zxy(target)), op).unwrap();
            let after = ArchiveView::open(&output).unwrap();
            assert_eq!(
                after.metadata().unwrap(),
                before_meta,
                "{name} metadata changed"
            );
            let (digest, leaves) = compute(&after, DigestMode::Leaves, &sort_scratch).unwrap();
            assert_ne!(
                digest.root, before_digest.root,
                "{name} did not FIRE (digest unchanged)"
            );
            let old = leaf_hashes(&before_leaves);
            let new = leaf_hashes(&leaves);
            for (&id, &hash) in &old {
                if id != target {
                    assert_eq!(
                        new.get(&id),
                        Some(&hash),
                        "{name} changed non-target tile {id}"
                    );
                }
            }
        }

        // regzip: byte-different, semantically identical (must CLEAR).
        let output = dir.join("regzip.pmtiles");
        mutate(&input, &output, None, MutationOp::Regzip).unwrap();
        let after = ArchiveView::open(&output).unwrap();
        let (digest, leaves) = compute(&after, DigestMode::Leaves, &sort_scratch).unwrap();
        assert_eq!(after.metadata().unwrap(), before_meta);
        assert_eq!(digest.root, before_digest.root, "regzip changed the digest");
        assert_eq!(leaves, before_leaves, "regzip changed the leaves");
        assert_ne!(
            fs::read(&input).unwrap(),
            fs::read(&output).unwrap(),
            "regzip left bytes unchanged"
        );
        // `scratch` removes the directory on drop.
    }
}
