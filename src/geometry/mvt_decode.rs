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
                // MoveTo — start a new ring. Only 1 MoveTo per ring in polygons.
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
                // LineTo — append vertices to the current ring.
                let Some(ring) = rings.last_mut() else {
                    // LineTo without a preceding MoveTo — skip.
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
                // ClosePath — close the current ring and reset cursor to ring start.
                if let Some(ring) = rings.last_mut()
                    && let Some(&first) = ring.first()
                {
                    ring.push(first);
                    cx = first.0;
                    cy = first.1;
                }
            }
            _ => {
                // Unknown command — skip.
            }
        }
    }
    rings
}

/// Zigzag-decode a u32 back to i32.
#[inline]
fn unzigzag(n: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    { ((n >> 1) as i32) ^ (-((n & 1) as i32)) }
}
