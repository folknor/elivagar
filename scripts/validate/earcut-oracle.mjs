// Headless MapLibre-tessellation oracle - FAITHFUL to maplibre-gl's fill
// bucket pipeline (verified against maplibre-gl dist source):
//   1. feature.loadGeometry() -> rings in tile coords
//   2. classifyRings(rings, EARCUT_MAX_RINGS=500) - SELF-CALIBRATING winding:
//      the first nonzero-area ring defines "outer" for the feature; rings
//      matching its winding start new polygons, opposite-wound rings become
//      holes of the current polygon. Single-ring features bypass entirely.
//      All but the 500 largest rings per polygon are dropped.
//   3. earcut(flatten(polygon)) per polygon.
// We measure earcut.deviation per polygon (triangulated vs true area) and
// count structural anomalies MapLibre would render wrongly:
//   - lead_holes: rings before the first same-winding ring after ring 0
//     exist only when winding flips mid-feature in unexpected ways (the
//     self-calibrating algorithm never drops rings, but a hole that ARRIVES
//     before its outer attaches to the WRONG polygon; we count holes whose
//     bbox is not contained in their assigned outer's bbox as misattached).
//
// Usage: node earcut-oracle.mjs <file.pmtiles> [layer|all] [deviation-threshold] [--unique]
//
// `all` runs the same per-polygon tessellation over every layer that carries
// polygon features and prints one table per layer, each block formatted
// exactly as the single-layer run's - so the ocean block of an `all` scan is
// line-identical to `... ocean`. Only layer selection widens; the math is
// untouched.
import { readFileSync } from "node:fs";
import { PMTiles, tileIdToZxy } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import earcut, { deviation, flatten } from "earcut";
import { gunzipSync } from "node:zlib";

const args = process.argv.slice(2);
const unique = args.includes("--unique");
const positional = args.filter(arg => arg !== "--unique");
const [path, layerName = "ocean", thresholdArg = "0.01"] = positional;
if (!path) {
  console.error("usage: node earcut-oracle.mjs <file.pmtiles> [layer|all] [threshold]");
  process.exit(2);
}
const allLayers = layerName === "all";
const THRESHOLD = Number(thresholdArg);
const EARCUT_MAX_RINGS = 500;

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

// VERBATIM maplibre-gl calculateSignedArea (note: p1 = current, p2 = PREVIOUS)
function calculateSignedArea(ring) {
  let sum = 0;
  for (let i = 0, len = ring.length, j = len - 1, p1, p2; i < len; j = i++) {
    p1 = ring[i];
    p2 = ring[j];
    sum += (p2.x - p1.x) * (p1.y + p2.y);
  }
  return sum;
}

// VERBATIM maplibre-gl classifyRings (self-calibrating winding + maxRings)
function classifyRings(rings, maxRings) {
  const len = rings.length;
  if (len <= 1) return [rings];
  const polygons = [];
  let polygon;
  let ccw;
  for (const ring of rings) {
    const area = calculateSignedArea(ring);
    if (area === 0) continue;
    ring.area = Math.abs(area);
    if (ccw === undefined) ccw = area < 0;
    if (ccw === area < 0) {
      if (polygon) polygons.push(polygon);
      polygon = [ring];
    } else {
      polygon.push(ring);
    }
  }
  if (polygon) polygons.push(polygon);
  if (maxRings > 1) {
    for (let j = 0; j < polygons.length; j++) {
      if (polygons[j].length <= maxRings) continue;
      polygons[j].sort((a, b) => b.area - a.area);
      polygons[j] = polygons[j].slice(0, maxRings);
    }
  }
  return polygons;
}

function bboxOf(ring) {
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const p of ring) {
    if (p.x < minX) minX = p.x;
    if (p.y < minY) minY = p.y;
    if (p.x > maxX) maxX = p.x;
    if (p.y > maxY) maxY = p.y;
  }
  return { minX, minY, maxX, maxY };
}

// layer name -> zoom -> stat. A single-layer run fills exactly one entry.
const perLayer = new Map();
const offenders = [];
let checked = 0;
const seenPayloads = new Set();

function zoomStat(name, z) {
  let perZoom = perLayer.get(name);
  if (!perZoom) {
    perZoom = new Map();
    perLayer.set(name, perZoom);
  }
  let zs = perZoom.get(z);
  if (!zs) {
    zs = { features: 0, polys: 0, over: 0, worst: 0, worstAt: "", misattached: 0 };
    perZoom.set(z, zs);
  }
  return zs;
}

for await (const t of allTiles()) {
  const payloadKey = `${t.offset}:${t.length}`;
  if (unique && seenPayloads.has(payloadKey)) continue;
  if (unique) seenPayloads.add(payloadKey);
  const [z, x, y] = tileIdToZxy(t.tileId);
  const raw = buf.subarray(header.tileDataOffset + t.offset, header.tileDataOffset + t.offset + t.length);
  let data;
  try { data = gunzipSync(raw); } catch { data = raw; }
  let vt;
  try { vt = new VectorTile(new PbfReader(data)); } catch { continue; }
  const names = allLayers ? Object.keys(vt.layers) : [layerName];
  let sawLayer = false;

  for (const name of names) {
    const layer = vt.layers[name];
    if (!layer) continue;
    sawLayer = true;

    for (let i = 0; i < layer.length; i++) {
      const feat = layer.feature(i);
      if (feat.type !== 3) continue;
      const zs = zoomStat(name, z);
      zs.features++;
      const rings = feat.loadGeometry();
      const polys = classifyRings(rings, EARCUT_MAX_RINGS);
      for (const poly of polys) {
        zs.polys++;
        // Structural check: every hole bbox should sit inside its outer's bbox.
        if (poly.length > 1) {
          const ob = bboxOf(poly[0]);
          for (let h = 1; h < poly.length; h++) {
            const hb = bboxOf(poly[h]);
            if (hb.minX < ob.minX || hb.minY < ob.minY || hb.maxX > ob.maxX || hb.maxY > ob.maxY) {
              zs.misattached++;
            }
          }
        }
        const flat = flatten(poly.map(ring => ring.map(p => [p.x, p.y])));
        const tris = earcut(flat.vertices, flat.holes, flat.dimensions);
        const dev = deviation(flat.vertices, flat.holes, flat.dimensions, tris);
        if (dev > zs.worst) { zs.worst = dev; zs.worstAt = `z${z}/${x}/${y} feat ${i}`; }
        if (dev > THRESHOLD) {
          zs.over++;
          offenders.push({ layer: name, z, x, y, i, dev });
        }
      }
    }
  }
  if (sawLayer) checked++;
}

const blocks = perLayer.size ? [...perLayer.keys()].sort() : [layerName];
let firstBlock = true;
for (const name of blocks) {
  if (!firstBlock) console.log("");
  firstBlock = false;
  console.log(`${path}  layer=${name}  threshold=${THRESHOLD}  (faithful maplibre classifyRings, maxRings=${EARCUT_MAX_RINGS})`);
  console.log(`${unique ? "unique payloads" : "tiles"} scanned: ${checked}`);
  console.log("zoom  features     polys  over_thresh  misattached  worst_deviation  worst_at");
  const perZoom = perLayer.get(name) ?? new Map();
  for (const z of [...perZoom.keys()].sort((a, b) => a - b)) {
    const s = perZoom.get(z);
    console.log(
      String(z).padStart(4), String(s.features).padStart(9), String(s.polys).padStart(9),
      String(s.over).padStart(11), String(s.misattached).padStart(11),
      s.worst.toExponential(3).padStart(16), " ", s.worstAt,
    );
  }
}
offenders.sort((a, b) => b.dev - a.dev);
if (offenders.length) {
  console.log(`\ntop offenders (${Math.min(15, offenders.length)} of ${offenders.length}):`);
  for (const o of offenders.slice(0, 15)) {
    const where = allLayers ? `${o.layer} ` : "";
    console.log(`  ${where}z${o.z}/${o.x}/${o.y} feat ${o.i}  deviation=${o.dev.toExponential(3)}`);
  }
} else {
  console.log("\nNo polygon exceeded the deviation threshold.");
}
