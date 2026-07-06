// Headless MapLibre-tessellation oracle.
//
// Runs the earcut algorithm (the exact tessellator MapLibre GL uses for fill
// layers) over every polygon feature in a PMTiles archive and measures
// earcut.deviation - the relative error between the triangulated area and the
// polygon's true area. Correct tessellation => deviation ~0. The "garbage
// triangles" failure mode => large deviation.
//
// Usage: node earcut-oracle.mjs <file.pmtiles> [layer] [deviation-threshold]
//   layer defaults to "ocean"; threshold defaults to 0.01 (1%).
//
// Output: per-zoom table (features, triangulated, worst deviation, count over
// threshold) plus the top offenders with z/x/y so they can be eyeballed.
import { readFileSync } from "node:fs";
import { PMTiles, FetchSource } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import Pbf from "pbf";
import earcut, { deviation, flatten } from "earcut";
import { gunzipSync } from "node:zlib";

const [, , path, layerName = "ocean", thresholdArg = "0.01"] = process.argv;
if (!path) {
  console.error("usage: node earcut-oracle.mjs <file.pmtiles> [layer] [threshold]");
  process.exit(2);
}
const THRESHOLD = Number(thresholdArg);

// Minimal in-memory source (avoid fetch; file is local).
class BufferSource {
  constructor(buf) { this.buf = buf; }
  getKey() { return "mem"; }
  async getBytes(offset, length) {
    return { data: this.buf.buffer.slice(this.buf.byteOffset + offset, this.buf.byteOffset + offset + length) };
  }
}

const buf = readFileSync(path);
const pm = new PMTiles(new BufferSource(buf));
const header = await pm.getHeader();

// Walk all entries via the directory structure.
async function* allTiles() {
  const root = await pm.cache.getDirectory(pm.source, header.rootDirectoryOffset, header.rootDirectoryLength, header);
  const stack = [root];
  while (stack.length) {
    const dir = stack.pop();
    for (const entry of dir) {
      if (entry.runLength === 0) {
        const leaf = await pm.cache.getDirectory(pm.source, header.leafDirectoryOffset + entry.offset, entry.length, header);
        stack.push(leaf);
      } else {
        for (let i = 0; i < entry.runLength; i++) {
          yield { tileId: entry.tileId + i, offset: entry.offset, length: entry.length };
        }
      }
    }
  }
}

// tileId -> z/x/y (Hilbert, matches pmtiles spec)
import { tileIdToZxy } from "pmtiles";

const perZoom = new Map(); // z -> {features, polys, over, worst, worstAt}
const offenders = [];

let checked = 0;
for await (const t of allTiles()) {
  const [z, x, y] = tileIdToZxy(t.tileId);
  const raw = buf.subarray(header.tileDataOffset + t.offset, header.tileDataOffset + t.offset + t.length);
  let data;
  try { data = gunzipSync(raw); } catch { data = raw; }
  let vt;
  try { vt = new VectorTile(new Pbf(data)); } catch { continue; }
  const layer = vt.layers[layerName];
  if (!layer) continue;

  let zs = perZoom.get(z);
  if (!zs) { zs = { features: 0, polys: 0, over: 0, worst: 0, worstAt: "" }; perZoom.set(z, zs); }

  for (let i = 0; i < layer.length; i++) {
    const feat = layer.feature(i);
    if (feat.type !== 3) continue;
    zs.features++;
    // classifyRings inside loadGeometry consumers: use feat.loadGeometry() and
    // split into polygons by winding, exactly like MapLibre's fill bucket.
    const rings = feat.loadGeometry();
    const polys = classifyRings(rings);
    for (const poly of polys) {
      zs.polys++;
      const flat = flatten(poly.map(ring => ring.map(p => [p.x, p.y])));
      const tris = earcut(flat.vertices, flat.holes, flat.dimensions);
      const dev = deviation(flat.vertices, flat.holes, flat.dimensions, tris);
      if (dev > zs.worst) { zs.worst = dev; zs.worstAt = `z${z}/${x}/${y} feat ${i}`; }
      if (dev > THRESHOLD) {
        zs.over++;
        offenders.push({ z, x, y, i, dev });
      }
    }
  }
  checked++;
}

// MapLibre's ring classification (winding-based, from @mapbox/vector-tile docs)
function classifyRings(rings) {
  if (rings.length <= 1) return [rings];
  const polygons = [];
  let polygon = null;
  for (const ring of rings) {
    const a = signedArea(ring);
    if (a === 0) continue;
    if (a > 0) {
      if (polygon) polygons.push(polygon);
      polygon = [ring];
    } else if (polygon) {
      polygon.push(ring);
    }
  }
  if (polygon) polygons.push(polygon);
  return polygons;
}
function signedArea(ring) {
  let sum = 0;
  for (let i = 0, len = ring.length, j = len - 1; i < len; j = i++) {
    sum += (ring[i].x - ring[j].x) * (ring[i].y + ring[j].y);
  }
  return sum;
}

console.log(`${path}  layer=${layerName}  threshold=${THRESHOLD}`);
console.log(`tiles-with-layer scanned: ${checked}`);
console.log("zoom  features     polys  over_thresh  worst_deviation  worst_at");
for (const z of [...perZoom.keys()].sort((a, b) => a - b)) {
  const s = perZoom.get(z);
  console.log(
    String(z).padStart(4), String(s.features).padStart(9), String(s.polys).padStart(9),
    String(s.over).padStart(11), s.worst.toExponential(3).padStart(16), " ", s.worstAt,
  );
}
offenders.sort((a, b) => b.dev - a.dev);
if (offenders.length) {
  console.log(`\ntop offenders (${Math.min(15, offenders.length)} of ${offenders.length}):`);
  for (const o of offenders.slice(0, 15)) {
    console.log(`  z${o.z}/${o.x}/${o.y} feat ${o.i}  deviation=${o.dev.toExponential(3)}`);
  }
} else {
  console.log("\nNo polygon exceeded the deviation threshold: earcut tessellates this archive cleanly.");
}
