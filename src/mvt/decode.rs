//! MVT polygon geometry decoder: command streams back to rings.
//!
//! The canonical-tile decoder that used to live here belonged to the regress
//! engine's legacy differential oracle and was deleted with it;
//! decode_mvt_polygon remains the lenient helper for tests, while archive
//! verification uses its checked counterpart.

/// Decode MVT polygon geometry commands back to closed rings in tile coordinates.
///
/// Inverse of `mvt::encode_polygon`. Walks MoveTo/LineTo/ClosePath commands,
/// undoes delta encoding, and returns one `Vec<(i32, i32)>` per ring (closed:
/// first == last vertex).
///
/// Designed for trusted in-pipeline data (output of our own `encode_polygon`).
/// On truncated input, may return partially decoded rings. Unknown command IDs
/// are silently skipped without consuming parameters (safe for well-formed
/// streams; may desync on malformed data).
#[cfg(test)]
pub fn decode_mvt_polygon(commands: &[u32]) -> Vec<Vec<(i32, i32)>> {
    let mut rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    let mut i = 0;

    while i < commands.len() {
        let cmd = commands[i];
        let cmd_id = cmd & 0x7;
        let cmd_count = cmd >> 3;
        i += 1;

        match cmd_id {
            1 => {
                // MoveTo - start a new ring. Only 1 MoveTo per ring in polygons.
                for _ in 0..cmd_count {
                    if i + 1 >= commands.len() {
                        return rings;
                    }
                    let dx = unzigzag(commands[i]);
                    let dy = unzigzag(commands[i + 1]);
                    i += 2;
                    cx += dx;
                    cy += dy;
                    rings.push(vec![(cx, cy)]);
                }
            }
            2 => {
                // LineTo - append vertices to the current ring.
                let Some(ring) = rings.last_mut() else {
                    // LineTo without a preceding MoveTo - skip.
                    i += (cmd_count as usize) * 2;
                    continue;
                };
                for _ in 0..cmd_count {
                    if i + 1 >= commands.len() {
                        return rings;
                    }
                    let dx = unzigzag(commands[i]);
                    let dy = unzigzag(commands[i + 1]);
                    i += 2;
                    cx += dx;
                    cy += dy;
                    ring.push((cx, cy));
                }
            }
            7 => {
                // ClosePath - close the current ring. Per MVT spec 4.3.3.3
                // the cursor is NOT changed (stays at the last LineTo vertex).
                if let Some(ring) = rings.last_mut()
                    && let Some(&first) = ring.first()
                {
                    ring.push(first);
                }
            }
            _ => {
                // Unknown command - skip.
            }
        }
    }
    rings
}

/// Decode and validate a polygon command stream without panicking or wrapping
/// coordinates. Each ring must follow MoveTo(1), LineTo(n > 1), ClosePath(1).
// tile_detail also decodes byte streams under a strict/tolerant policy, while
// verify applies additional coordinate limits. This helper checks the polygon
// grammar and i32 arithmetic without those callers' extra policies.
pub(crate) fn try_decode_mvt_polygon(commands: &[u32]) -> Result<Vec<Vec<(i32, i32)>>, String> {
    let mut rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut current_ring: Option<Vec<(i32, i32)>> = None;
    let mut cx = 0i32;
    let mut cy = 0i32;
    let mut i = 0;
    let mut ring_phase = 0u8; // 0 = MoveTo, 1 = LineTo, 2 = ClosePath

    if commands.is_empty() {
        return Err("empty polygon geometry command stream".to_string());
    }

    while i < commands.len() {
        let cmd = commands[i];
        let cmd_id = cmd & 0x7;
        let cmd_count = cmd >> 3;
        i += 1;

        if cmd_count == 0 {
            return Err("polygon command with zero repeat count".to_string());
        }

        match cmd_id {
            1 => {
                if cmd_count != 1 {
                    return Err("polygon ring MoveTo count must be 1".to_string());
                }
                if current_ring.is_some() {
                    return Err("polygon ring missing ClosePath before MoveTo".to_string());
                }
                ensure_parameters(commands, i, cmd_count)?;
                let (x, y) = decode_point(commands, &mut i, &mut cx, &mut cy)?;
                current_ring = Some(vec![(x, y)]);
                ring_phase = 1;
            }
            2 => {
                if cmd_count <= 1 {
                    return Err("polygon ring LineTo count must be greater than 1".to_string());
                }
                if ring_phase != 1 {
                    return Err("polygon LineTo must follow MoveTo".to_string());
                }
                let Some(ring) = current_ring.as_mut() else {
                    return Err("polygon LineTo without a preceding MoveTo".to_string());
                };
                ensure_parameters(commands, i, cmd_count)?;
                for _ in 0..cmd_count {
                    ring.push(decode_point(commands, &mut i, &mut cx, &mut cy)?);
                }
                ring_phase = 2;
            }
            7 => {
                // ClosePath - close the current ring. Per MVT spec 4.3.3.3
                // the cursor is NOT changed (stays at the last LineTo vertex).
                if cmd_count != 1 {
                    return Err("polygon ClosePath count must be 1".to_string());
                }
                if ring_phase != 2 {
                    return Err("polygon ClosePath must follow LineTo".to_string());
                }
                let Some(mut ring) = current_ring.take() else {
                    return Err("polygon ClosePath without an open ring".to_string());
                };
                if ring.len() < 3 {
                    return Err("polygon ring has fewer than 3 points".to_string());
                }
                let first = ring[0];
                ring.push(first);
                rings.push(ring);
                ring_phase = 0;
            }
            _ => return Err(format!("unknown polygon geometry command id {cmd_id}")),
        }
    }

    match ring_phase {
        1 => return Err("polygon ring missing LineTo".to_string()),
        2 => return Err("polygon ring missing ClosePath".to_string()),
        _ => {}
    }
    Ok(rings)
}

fn ensure_parameters(commands: &[u32], start: usize, count: u32) -> Result<(), String> {
    let count =
        usize::try_from(count).map_err(|_| "polygon command count exceeds usize".to_string())?;
    let parameter_count = count
        .checked_mul(2)
        .ok_or_else(|| "polygon parameter count overflows usize".to_string())?;
    let end = start
        .checked_add(parameter_count)
        .ok_or_else(|| "polygon command end overflows usize".to_string())?;
    if end > commands.len() {
        return Err("truncated polygon command parameters".to_string());
    }
    Ok(())
}

fn decode_point(
    commands: &[u32],
    i: &mut usize,
    cx: &mut i32,
    cy: &mut i32,
) -> Result<(i32, i32), String> {
    let dx = unzigzag(commands[*i]);
    let dy = unzigzag(commands[*i + 1]);
    *i += 2;
    *cx = (*cx)
        .checked_add(dx)
        .ok_or_else(|| "MVT polygon x coordinate overflows i32".to_string())?;
    *cy = (*cy)
        .checked_add(dy)
        .ok_or_else(|| "MVT polygon y coordinate overflows i32".to_string())?;
    Ok((*cx, *cy))
}

/// Zigzag-decode a u32 back to i32.
#[inline]
fn unzigzag(n: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((n >> 1) as i32) ^ (-((n & 1) as i32))
    }
}

#[cfg(test)]
mod tests {
    /// Round-trip: encode a single ring polygon and decode it back.
    #[test]
    fn decode_mvt_polygon_single_ring_round_trip() {
        let ring = vec![(100, 200), (300, 200), (300, 400), (100, 400), (100, 200)];
        let mut buf = Vec::new();
        crate::mvt::encode_polygon(&mut buf, &[&ring]);
        let decoded = super::decode_mvt_polygon(&buf);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], ring);
    }

    /// Round-trip: multi-ring polygon (outer + inner hole).
    #[test]
    fn decode_mvt_polygon_multi_ring_round_trip() {
        let outer = vec![(0, 0), (4096, 0), (4096, 4096), (0, 4096), (0, 0)];
        let inner = vec![
            (1000, 1000),
            (1000, 3000),
            (3000, 3000),
            (3000, 1000),
            (1000, 1000),
        ];
        let mut buf = Vec::new();
        crate::mvt::encode_polygon(&mut buf, &[&outer, &inner]);
        let decoded = super::decode_mvt_polygon(&buf);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], outer);
        assert_eq!(decoded[1], inner);
    }

    /// Empty command buffer produces no rings.
    #[test]
    fn decode_mvt_polygon_empty() {
        let decoded = super::decode_mvt_polygon(&[]);
        assert!(decoded.is_empty());
    }

    /// Round-trip with negative coordinates (buffer region outside tile).
    #[test]
    fn decode_mvt_polygon_negative_coords() {
        let ring = vec![
            (-128, -128),
            (4224, -128),
            (4224, 4224),
            (-128, 4224),
            (-128, -128),
        ];
        let mut buf = Vec::new();
        crate::mvt::encode_polygon(&mut buf, &[&ring]);
        let decoded = super::decode_mvt_polygon(&buf);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], ring);
    }

    /// Round-trip: two separate polygons encoded sequentially (as multipolygon).
    #[test]
    fn decode_mvt_polygon_two_outer_rings() {
        let ring_a = vec![(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)];
        let ring_b = vec![(200, 200), (300, 200), (300, 300), (200, 300), (200, 200)];
        let mut buf = Vec::new();
        crate::mvt::encode_polygon(&mut buf, &[&ring_a, &ring_b]);
        let decoded = super::decode_mvt_polygon(&buf);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], ring_a);
        assert_eq!(decoded[1], ring_b);
    }
}
