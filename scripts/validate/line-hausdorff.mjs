// Segment-interior Hausdorff distance between two archives' line layers, per
// zoom and layer, over the tiles whose bytes differ. Unlike regress's
// vertex-to-vertex displacement, this measures how far each side's vertices
// sit from the OTHER side's segments (union of every line in the layer), so a
// simplifier that removes an interior vertex scores its true perpendicular
// change rather than the distance to the nearest surviving vertex.
// ADVISORY: uncalibrated; written to bound the line-simplification change of
// 2026-11-15 (cascade replaced by per-zoom simplification from the source).
// Usage: node line-hausdorff.mjs <a.pmtiles> <b.pmtiles> <zmax> <minx,miny,maxx,maxy merc>
import { readFileSync } from "node:fs";
import { PMTiles } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import { gunzipSync } from "node:zlib";

const [pathA, pathB, zmaxArg, bboxArg] = process.argv.slice(2);
if (!bboxArg) {
  console.error("usage: node line-hausdorff.mjs <a.pmtiles> <b.pmtiles> <zmax> <minx,miny,maxx,maxy>");
  process.exit(2);
}
const zmax = Number(zmaxArg);
const [minx, miny, maxx, maxy] = bboxArg.split(",").map(Number);

class BufferSource {
  constructor(buf) { this.buf = buf; }
  getKey() { return "mem"; }
  async getBytes(offset, length) {
    return { data: this.buf.buffer.slice(this.buf.byteOffset + offset, this.buf.byteOffset + offset + length) };
  }
}
const open = path => new PMTiles(new BufferSource(readFileSync(path)));
const a = open(pathA);
const b = open(pathB);

function linesByLayer(bytes) {
  let data = new Uint8Array(bytes);
  try { data = gunzipSync(data); } catch { /* raw */ }
  const out = new Map();
  const tile = new VectorTile(new PbfReader(data));
  for (const [name, layer] of Object.entries(tile.layers)) {
    const segs = [];
    const verts = [];
    for (let i = 0; i < layer.length; i++) {
      const f = layer.feature(i);
      if (f.type !== 2) continue;
      for (const line of f.loadGeometry()) {
        for (let k = 0; k < line.length; k++) {
          verts.push(line[k]);
          if (k + 1 < line.length) segs.push([line[k], line[k + 1]]);
        }
      }
    }
    if (verts.length) out.set(name, { segs, verts });
  }
  return out;
}

function distToSeg(p, [s, e]) {
  const dx = e.x - s.x, dy = e.y - s.y;
  const len = dx * dx + dy * dy;
  let t = len === 0 ? 0 : ((p.x - s.x) * dx + (p.y - s.y) * dy) / len;
  t = Math.max(0, Math.min(1, t));
  const ex = p.x - (s.x + t * dx), ey = p.y - (s.y + t * dy);
  return Math.hypot(ex, ey);
}

// Only vertices inside the visible tile square count: clipping legitimately
// moves buffer-edge cut points along the boundary.
const inside = p => p.x >= 0 && p.x <= 4096 && p.y >= 0 && p.y <= 4096;
function directed(from, to) {
  let worst = 0, at = null;
  for (const p of from.verts) {
    if (!inside(p)) continue;
    let best = Infinity;
    for (const s of to.segs) {
      const d = distToSeg(p, s);
      if (d < best) best = d;
      if (best === 0) break;
    }
    if (to.segs.length === 0) best = Infinity;
    if (best > worst) { worst = best; at = p; }
  }
  return [worst, at];
}

const stats = new Map();
for (let z = 0; z <= zmax; z++) {
  const n = 2 ** z;
  const [x0, x1] = [Math.floor(minx * n), Math.floor(maxx * n)];
  const [y0, y1] = [Math.floor(miny * n), Math.floor(maxy * n)];
  for (let x = x0; x <= x1; x++) {
    for (let y = y0; y <= y1; y++) {
      const [ta, tb] = await Promise.all([a.getZxy(z, x, y), b.getZxy(z, x, y)]);
      if (!ta || !tb) continue;
      if (Buffer.compare(Buffer.from(ta.data), Buffer.from(tb.data)) === 0) continue;
      const la = linesByLayer(ta.data);
      const lb = linesByLayer(tb.data);
      for (const name of new Set([...la.keys(), ...lb.keys()])) {
        const empty = { segs: [], verts: [] };
        const [dab, pab] = directed(la.get(name) ?? empty, lb.get(name) ?? empty);
        const [dba, pba] = directed(lb.get(name) ?? empty, la.get(name) ?? empty);
        const d = Math.max(dab, dba);
        const key = `z${z} ${name}`;
        const s = stats.get(key) ?? { tiles: 0, ds: [], worst: 0, where: "" };
        s.tiles++;
        s.ds.push(d);
        if (d > s.worst) {
          s.worst = d;
          const p = dab >= dba ? pab : pba;
          s.where = `z${z}/${x}/${y} at ${p ? `${p.x},${p.y}` : "?"} (${dab >= dba ? "a->b" : "b->a"})`;
        }
        stats.set(key, s);
      }
    }
  }
}
for (const [key, s] of stats) {
  const ds = s.ds.sort((l, r) => l - r);
  const q = f => ds[Math.min(ds.length - 1, Math.floor(f * ds.length))].toFixed(1);
  console.log(`${key} tiles=${s.tiles} p50=${q(0.5)} p95=${q(0.95)} max=${s.worst.toFixed(1)} worst=${s.where}`);
}
