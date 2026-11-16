//! The one MVT geometry command decoder.
//!
//! Every reader of an MVT command stream in the crate - the `tile_detail`
//! surface brokkr hashes, `verify`, `svg`, `diag`, the line merger, the MLT
//! encoder and the tests - decodes through [`decode`]. Two decisions live here
//! and nowhere else:
//!
//! - **Wire mechanics.** Command header split, unknown command ids, parameter
//!   truncation, zigzag, and the cursor. ClosePath does NOT move the cursor
//!   (MVT spec 4.3.3.3): the next MoveTo is relative to the last LineTo
//!   vertex. That rule was once spelled separately by each decoder, and a
//!   symmetric encoder/decoder violation of it stayed invisible to every
//!   internal round-trip for three months (reference/rendering-postmortem.md,
//!   R23). Owning it once removes the copies; it does not make an in-crate
//!   round-trip an independent check. The literal-word tests below and the
//!   external earcut oracle (scripts/validate/earcut-oracle.mjs) are that check.
//! - **The strict grammar** of spec 4.3.4 ([`Grammar`]), shared by the gate's
//!   strict `tile_detail` decode, `verify`, and [`decode_polygon_rings`].
//!
//! Consumers stay separate where their contracts differ: each builds its own
//! output from the [`CommandSink`] events (tolerant `tile_detail` keeps its
//! lenient quirks, MLT closes rings only when needed, svg draws unvalidated
//! sequences). The cursor is accumulated in i64 so verify's extent-scaled
//! limits keep their full domain; consumers that store i32 check it.

use std::fmt;

use protohoggr::Cursor;

/// The three MVT geometry commands. Any other id is a decode error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandKind {
    MoveTo,
    LineTo,
    ClosePath,
}

impl CommandKind {
    /// The wire command id.
    pub(crate) fn id(self) -> u32 {
        match self {
            Self::MoveTo => 1,
            Self::LineTo => 2,
            Self::ClosePath => 7,
        }
    }
}

/// One command header. MoveTo and LineTo are followed by `count`
/// [`Point`] events; ClosePath by none, whatever its count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Command {
    pub kind: CommandKind,
    pub count: u32,
}

/// One decoded parameter pair: the wire delta and the absolute cursor after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Point {
    pub dx: i32,
    pub dy: i32,
    pub x: i64,
    pub y: i64,
}

impl Point {
    /// The absolute position as i32, the MVT sint32 coordinate domain.
    pub(crate) fn xy_i32(self) -> Result<(i32, i32), GeometryError> {
        let x = i32::try_from(self.x).map_err(|_| GeometryError::CoordinateOverflow("x"))?;
        let y = i32::try_from(self.y).map_err(|_| GeometryError::CoordinateOverflow("y"))?;
        Ok((x, y))
    }
}

/// Why a command stream did not decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GeometryError {
    /// A packed varint did not read.
    Varint(protohoggr::WireError),
    /// A word exceeds u32.
    WordOutOfRange(u64),
    UnknownCommand(u32),
    /// A MoveTo or LineTo declared more parameters than the stream holds.
    Truncated,
    /// An absolute coordinate does not fit i32.
    CoordinateOverflow(&'static str),
    /// The stream breaks the spec 4.3.4 grammar.
    Grammar(&'static str),
    /// A consumer refused an event.
    Consumer(String),
}

impl fmt::Display for GeometryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Varint(error) => write!(f, "read geometry varint: {error}"),
            Self::WordOutOfRange(raw) => write!(f, "geometry word out of u32 range: {raw}"),
            Self::UnknownCommand(id) => write!(f, "unknown geometry command id {id}"),
            Self::Truncated => f.write_str("truncated geometry command parameters"),
            Self::CoordinateOverflow(axis) => write!(f, "geometry {axis} coordinate overflows i32"),
            Self::Grammar(message) => f.write_str(message),
            Self::Consumer(message) => f.write_str(message),
        }
    }
}

impl From<GeometryError> for String {
    fn from(error: GeometryError) -> Self {
        error.to_string()
    }
}

/// Where command words come from.
pub(crate) enum Words<'a> {
    /// Our own encoder's output.
    Slice(std::slice::Iter<'a, u32>),
    /// A packed protobuf `geometry` field.
    Packed(Cursor<'a>),
}

impl<'a> Words<'a> {
    pub(crate) fn slice(words: &'a [u32]) -> Self {
        Self::Slice(words.iter())
    }

    pub(crate) fn packed(bytes: &'a [u8]) -> Self {
        Self::Packed(Cursor::new(bytes))
    }

    fn next_word(&mut self) -> Result<Option<u32>, GeometryError> {
        match self {
            Self::Slice(iter) => Ok(iter.next().copied()),
            Self::Packed(cursor) => {
                if cursor.is_empty() {
                    return Ok(None);
                }
                let raw = cursor.read_varint().map_err(GeometryError::Varint)?;
                u32::try_from(raw)
                    .map(Some)
                    .map_err(|_| GeometryError::WordOutOfRange(raw))
            }
        }
    }
}

/// Receives a stream's events in wire order. A sink only receives events; it
/// cannot pull words, so it cannot leave parameters unconsumed.
pub(crate) trait CommandSink {
    fn command(&mut self, command: Command) -> Result<(), GeometryError>;
    fn point(&mut self, point: Point) -> Result<(), GeometryError>;
}

/// The spec 4.3.4 grammar to enforce, per geometry type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Grammar {
    Point,
    LineString,
    Polygon,
    /// verify's policy for a geometry type MVT does not name: no zero counts
    /// and no ClosePath, MoveTo and LineTo unsequenced.
    Unknown,
}

impl Grammar {
    /// The grammar for an MVT `type` field value.
    pub(crate) fn for_geom_type(geom_type: u8) -> Self {
        match geom_type {
            1 => Self::Point,
            2 => Self::LineString,
            3 => Self::Polygon,
            _ => Self::Unknown,
        }
    }
}

/// Header-level state of one grammar check.
struct GrammarState {
    grammar: Grammar,
    /// Point: a MoveTo was seen. LineString: a MoveTo awaits its LineTo.
    pending: bool,
    /// Polygon ring phase: 0 expects MoveTo, 1 LineTo, 2 ClosePath.
    phase: u8,
    any: bool,
}

impl GrammarState {
    fn new(grammar: Grammar) -> Self {
        Self {
            grammar,
            pending: false,
            phase: 0,
            any: false,
        }
    }

    fn check(&mut self, command: Command) -> Result<(), GeometryError> {
        use CommandKind::{ClosePath, LineTo, MoveTo};
        let err = |message| Err(GeometryError::Grammar(message));
        self.any = true;
        if command.count == 0 {
            return err("geometry command with zero repeat count");
        }
        match (self.grammar, command.kind) {
            (Grammar::Polygon, ClosePath) => {}
            (_, ClosePath) => return err("ClosePath in non-polygon geometry"),
            (Grammar::Point, MoveTo) => {
                if self.pending {
                    return err("point geometry must use a single MoveTo command (spec 4.3.4.2)");
                }
                self.pending = true;
            }
            (Grammar::Point, LineTo) => {
                return err("LineTo is invalid for point geometry (spec 4.3.4.2)");
            }
            (Grammar::LineString, MoveTo) => {
                if command.count != 1 {
                    return err("linestring MoveTo count must be 1 (spec 4.3.4.3)");
                }
                if self.pending {
                    return err("linestring MoveTo must follow a LineTo (spec 4.3.4.3)");
                }
                self.pending = true;
            }
            (Grammar::LineString, LineTo) => {
                if !self.pending {
                    return err("linestring LineTo must follow MoveTo (spec 4.3.4.3)");
                }
                self.pending = false;
            }
            (Grammar::Polygon, MoveTo) => {
                if command.count != 1 {
                    return err("polygon ring MoveTo count must be 1 (spec 4.3.4.4)");
                }
                match self.phase {
                    1 => return err("polygon ring missing LineTo"),
                    2 => return err("polygon ring missing ClosePath"),
                    _ => self.phase = 1,
                }
            }
            (Grammar::Polygon, LineTo) => {
                if self.phase != 1 {
                    return err("polygon LineTo must follow MoveTo (spec 4.3.4.4)");
                }
                if command.count <= 1 {
                    return err("polygon ring LineTo count must be greater than 1 (spec 4.3.4.4)");
                }
                self.phase = 2;
            }
            (Grammar::Unknown, MoveTo | LineTo) => {}
        }
        if self.grammar == Grammar::Polygon && command.kind == ClosePath {
            if command.count != 1 {
                return err("ClosePath command count is not 1 (spec 4.3.3.3)");
            }
            match self.phase {
                1 => return err("polygon ring has too few points (need >=3)"),
                0 => return err("polygon ClosePath must follow LineTo (spec 4.3.4.4)"),
                _ => self.phase = 0,
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), GeometryError> {
        let err = |message| Err(GeometryError::Grammar(message));
        if !self.any {
            return err("feature has empty geometry command stream");
        }
        match self.grammar {
            Grammar::LineString if self.pending => {
                err("linestring MoveTo missing LineTo (spec 4.3.4.3)")
            }
            Grammar::Polygon if self.phase == 1 => err("polygon ring missing LineTo"),
            Grammar::Polygon if self.phase == 2 => err("polygon ring missing ClosePath"),
            _ => Ok(()),
        }
    }
}

/// Decode a command stream into `sink`, enforcing `grammar` when given.
///
/// Each header is checked against the grammar before the sink sees it, and
/// the grammar's end-of-stream rules run only after every parameter decoded,
/// so success means the whole stream was read and accepted. `None` is the
/// unvalidated mode (tolerant detail, the svg viewer): wire errors still fail.
pub(crate) fn decode(
    mut words: Words<'_>,
    grammar: Option<Grammar>,
    sink: &mut impl CommandSink,
) -> Result<(), GeometryError> {
    let mut grammar = grammar.map(GrammarState::new);
    let (mut x, mut y) = (0i64, 0i64);
    while let Some(header) = words.next_word()? {
        let kind = match header & 0x7 {
            1 => CommandKind::MoveTo,
            2 => CommandKind::LineTo,
            7 => CommandKind::ClosePath,
            id => return Err(GeometryError::UnknownCommand(id)),
        };
        let command = Command {
            kind,
            count: header >> 3,
        };
        if let Some(grammar) = grammar.as_mut() {
            grammar.check(command)?;
        }
        sink.command(command)?;
        if kind == CommandKind::ClosePath {
            // The cursor stays at the last LineTo vertex (spec 4.3.3.3).
            continue;
        }
        for _ in 0..command.count {
            let dx = unzigzag(words.next_word()?.ok_or(GeometryError::Truncated)?);
            let dy = unzigzag(words.next_word()?.ok_or(GeometryError::Truncated)?);
            // Each step adds at most 2^31 and a stream holds fewer than 2^61
            // words, so i64 cannot overflow; checked anyway, it costs nothing.
            x = x
                .checked_add(i64::from(dx))
                .ok_or(GeometryError::CoordinateOverflow("x"))?;
            y = y
                .checked_add(i64::from(dy))
                .ok_or(GeometryError::CoordinateOverflow("y"))?;
            sink.point(Point { dx, dy, x, y })?;
        }
    }
    if let Some(grammar) = grammar {
        grammar.finish()?;
    }
    Ok(())
}

/// Polygon rings in wire order, each closed (first == last), under the strict
/// polygon grammar. Zero-area rings are kept: grouping and role assignment are
/// the caller's.
pub(crate) fn decode_polygon_rings(
    words: Words<'_>,
) -> Result<Vec<Vec<(i32, i32)>>, GeometryError> {
    struct Rings(Vec<Vec<(i32, i32)>>);
    impl CommandSink for Rings {
        fn command(&mut self, command: Command) -> Result<(), GeometryError> {
            match command.kind {
                CommandKind::MoveTo => self.0.push(Vec::new()),
                CommandKind::LineTo => {}
                CommandKind::ClosePath => {
                    if let Some(ring) = self.0.last_mut()
                        && let Some(&first) = ring.first()
                    {
                        ring.push(first);
                    }
                }
            }
            Ok(())
        }
        fn point(&mut self, point: Point) -> Result<(), GeometryError> {
            let xy = point.xy_i32()?;
            if let Some(ring) = self.0.last_mut() {
                ring.push(xy);
            }
            Ok(())
        }
    }
    let mut rings = Rings(Vec::new());
    decode(words, Some(Grammar::Polygon), &mut rings)?;
    Ok(rings.0)
}

/// Every MoveTo/LineTo position in wire order, unvalidated. Test helper.
impl std::error::Error for GeometryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Varint(error) => Some(error),
            _ => None,
        }
    }
}
impl From<GeometryError> for crate::DecodeError {
    fn from(error: GeometryError) -> Self {
        Self::caused("MVT geometry", error)
    }
}

#[cfg(test)]
pub(crate) fn decode_positions(words: &[u32]) -> Vec<(i32, i32)> {
    struct Positions(Vec<(i32, i32)>);
    impl CommandSink for Positions {
        fn command(&mut self, _: Command) -> Result<(), GeometryError> {
            Ok(())
        }
        fn point(&mut self, point: Point) -> Result<(), GeometryError> {
            self.0.push(point.xy_i32()?);
            Ok(())
        }
    }
    let mut positions = Positions(Vec::new());
    decode(Words::slice(words), None, &mut positions).expect("test geometry decodes");
    positions.0
}

/// Strict polygon rings of our own encoder output. Test helper.
#[cfg(test)]
pub(crate) fn polygon_rings(words: &[u32]) -> Vec<Vec<(i32, i32)>> {
    decode_polygon_rings(Words::slice(words)).expect("test polygon decodes")
}

#[inline]
fn unzigzag(n: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((n >> 1) as i32) ^ (-((n & 1) as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::cast_sign_loss)]
    fn zz(v: i32) -> u32 {
        ((v << 1) ^ (v >> 31)) as u32
    }

    /// Hand-written words, not encoder output: ring 2's MoveTo is relative to
    /// ring 1's LAST LineTo vertex (20,20), not to its first vertex.
    #[test]
    fn closepath_keeps_the_cursor_at_the_last_lineto_vertex() {
        let words = [
            9,
            zz(10),
            zz(10),
            18,
            zz(10),
            zz(0),
            zz(0),
            zz(10),
            15,
            9,
            zz(10),
            zz(10),
            18,
            zz(10),
            zz(0),
            zz(0),
            zz(10),
            15,
        ];
        let rings = decode_polygon_rings(Words::slice(&words)).expect("valid polygon");
        assert_eq!(rings[0], vec![(10, 10), (20, 10), (20, 20), (10, 10)]);
        assert_eq!(rings[1], vec![(30, 30), (40, 30), (40, 40), (30, 30)]);
    }

    #[test]
    fn packed_and_slice_sources_agree() {
        let words = [9u32, 300, 4, 18, 2, 2, 2, 0, 15];
        let mut packed = Vec::new();
        for &word in &words {
            let mut v = word;
            while v >= 0x80 {
                packed.push(u8::try_from(v & 0x7f).expect("7 bits") | 0x80);
                v >>= 7;
            }
            packed.push(u8::try_from(v).expect("7 bits"));
        }
        assert_eq!(
            decode_polygon_rings(Words::slice(&words)),
            decode_polygon_rings(Words::packed(&packed))
        );
    }

    #[test]
    fn wire_errors_fail_in_unvalidated_mode() {
        struct Null;
        impl CommandSink for Null {
            fn command(&mut self, _: Command) -> Result<(), GeometryError> {
                Ok(())
            }
            fn point(&mut self, _: Point) -> Result<(), GeometryError> {
                Ok(())
            }
        }
        assert_eq!(
            decode(Words::slice(&[4 | 8]), None, &mut Null),
            Err(GeometryError::UnknownCommand(4))
        );
        assert_eq!(
            decode(Words::slice(&[9, 0]), None, &mut Null),
            Err(GeometryError::Truncated)
        );
        assert!(decode(Words::packed(&[0x80]), None, &mut Null).is_err());
        // A zero count is a grammar error only.
        assert!(decode(Words::slice(&[1]), None, &mut Null).is_ok());
        assert!(decode(Words::slice(&[1]), Some(Grammar::LineString), &mut Null).is_err());
    }

    #[test]
    fn i32_coordinates_are_checked_where_stored() {
        let words = [9, u32::MAX - 1, 0, 18, zz(1), 0, zz(1), 0, 15];
        let error = decode_polygon_rings(Words::slice(&words)).expect_err("overflow");
        assert!(error.to_string().contains("overflows i32"));
    }

    #[test]
    fn grammar_rejects_what_the_spec_forbids() {
        let reject = |grammar, words: &[u32], needle: &str| {
            struct Null;
            impl CommandSink for Null {
                fn command(&mut self, _: Command) -> Result<(), GeometryError> {
                    Ok(())
                }
                fn point(&mut self, _: Point) -> Result<(), GeometryError> {
                    Ok(())
                }
            }
            let error = decode(Words::slice(words), Some(grammar), &mut Null)
                .expect_err("grammar violation");
            assert!(error.to_string().contains(needle), "{error} lacks {needle}");
        };
        reject(Grammar::Polygon, &[], "empty geometry");
        reject(Grammar::Point, &[9, 0, 0, 9, 2, 2], "single MoveTo");
        reject(Grammar::LineString, &[9, 0, 0], "missing LineTo");
        reject(
            Grammar::LineString,
            &[9, 0, 0, 10, 2, 2, 10, 2, 2],
            "LineTo must follow MoveTo",
        );
        reject(
            Grammar::LineString,
            &[9, 0, 0, 10, 2, 2, 15],
            "ClosePath in non-polygon",
        );
        reject(
            Grammar::Polygon,
            &[9, 0, 0, 18, 20, 0, 0, 20],
            "missing ClosePath",
        );
        reject(
            Grammar::Polygon,
            &[9, 0, 0, 10, 20, 0, 15],
            "greater than 1",
        );
        reject(Grammar::Polygon, &[9, 0, 0, 15], "too few points");
        reject(Grammar::Unknown, &[9, 0, 0, 15], "ClosePath in non-polygon");
    }
}
