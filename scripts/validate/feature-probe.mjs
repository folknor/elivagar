// Dump one feature's ring structure exactly as MapLibre sees it:
// per ring: vertex count, calculateSignedArea sign, bbox; then the
// classifyRings grouping and per-polygon earcut deviation.
// Usage: node feature-probe.mjs <file.pmtiles> <z> <x> <y> <layer> <featIdx>
import { readFileSync } from "node:fs";
import { PMTiles } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import earcut, { deviation, flatten } from "earcut";
import { gunzipSync } from "node:zlib";

const [, , path, zArg, xArg, yArg, layerName, featArg] = process.argv;
const [z, x, y, featIdx] = [Number(zArg), Number(xArg), Number(yArg), Number(featArg)];

class BufferSource {
  constructor(buf) { this.buf = buf; }
  getKey() { return "mem"; }
  async getBytes(offset, length) {
    return { data: this.buf.buffer.slice(this.buf.byteOffset + offset, this.buf.byteOffset + offset + length) };
  }
}
const buf = readFileSync(path);
const pm = new PMTiles(new BufferSource(buf));
const resp = await pm.getZxy(z, x, y);
let data;
try { data = gunzipSync(Buffer.from(resp.data)); } catch { data = Buffer.from(resp.data); }
const vt = new VectorTile(new PbfReader(data));
const layer = vt.layers[layerName];
const feat = layer.feature(featIdx);
const rings = feat.loadGeometry();

function calculateSignedArea(ring) {
  let sum = 0;
  for (let i = 0, len = ring.length, j = len - 1, p1, p2; i < len; j = i++) {
    p1 = ring[i]; p2 = ring[j];
    sum += (p2.x - p1.x) * (p1.y + p2.y);
  }
  return sum;
}
function bboxOf(ring) {
  let a = Infinity, b = Infinity, c = -Infinity, d = -Infinity;
  for (const p of ring) { a = Math.min(a, p.x); b = Math.min(b, p.y); c = Math.max(c, p.x); d = Math.max(d, p.y); }
  return [a, b, c, d];
}

console.log(`feature id=${feat.id} rings=${rings.length}`);
rings.forEach((r, i) => {
  const a = calculateSignedArea(r);
  console.log(`  ring ${i}: ${r.length}v area=${a.toExponential(2)} bbox=${bboxOf(r).join(",")} first=(${r[0].x},${r[0].y})`);
});

// faithful classifyRings
const polygons = [];
{
  let polygon, ccw;
  for (const ring of rings) {
    const area = calculateSignedArea(ring);
    if (area === 0) continue;
    if (ccw === undefined) ccw = area < 0;
    if (ccw === area < 0) { if (polygon) polygons.push(polygon); polygon = [ring]; }
    else if (polygon) polygon.push(ring);
  }
  if (polygon) polygons.push(polygon);
}
console.log(`classifyRings -> ${polygons.length} polygons:`);
polygons.forEach((poly, pi) => {
  const flat = flatten(poly.map(ring => ring.map(p => [p.x, p.y])));
  const tris = earcut(flat.vertices, flat.holes, flat.dimensions);
  const dev = deviation(flat.vertices, flat.holes, flat.dimensions, tris);
  console.log(`  poly ${pi}: ${poly.length} rings (outer ${poly[0].length}v bbox=${bboxOf(poly[0]).join(",")})  deviation=${dev.toExponential(3)}`);
});
