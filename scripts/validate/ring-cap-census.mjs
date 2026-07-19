// Rings-per-polygon census against MapLibre's EARCUT_MAX_RINGS=500 clamp.
//
// classifyRings silently drops all but the 500 largest rings of a polygon, so
// a merged multi-hole feature that crosses the cap loses holes with no error
// anywhere - the earcut oracle validates only the retained rings. This census
// groups every polygon feature's rings exactly as MapLibre does (verbatim
// classifyRings, NO clamp) and reports the per-zoom maximum ring count per
// polygon plus every polygon over the cap. Gate: 0 polygons over 500.
//
// Built for the low-zoom ocean union landing (OCEAN_POLICY_VERSION v3), which
// consolidates thousands of per-cell features into few many-holed shapes.
//
// Usage: node ring-cap-census.mjs <file.pmtiles> [layer] [--unique]
import { readFileSync } from "node:fs";
import { PMTiles, tileIdToZxy } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import { gunzipSync } from "node:zlib";

const args = process.argv.slice(2);
const unique = args.includes("--unique");
const positional = args.filter(arg => arg !== "--unique");
const [path, layerName = "ocean"] = positional;
if (!path) {
  console.error("usage: node ring-cap-census.mjs <file.pmtiles> [layer] [--unique]");
  process.exit(2);
}
const CAP = 500;

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

// VERBATIM maplibre-gl calculateSignedArea (p1 = current, p2 = PREVIOUS)
function calculateSignedArea(ring) {
  let sum = 0;
  for (let i = 0, len = ring.length, j = len - 1, p1, p2; i < len; j = i++) {
    p1 = ring[i];
    p2 = ring[j];
    sum += (p2.x - p1.x) * (p1.y + p2.y);
  }
  return sum;
}

// classifyRings grouping, verbatim except the clamp: we need true ring counts.
function groupRingCounts(rings) {
  if (rings.length <= 1) return [rings.length];
  const counts = [];
  let count = 0;
  let ccw;
  for (const ring of rings) {
    const area = calculateSignedArea(ring);
    if (area === 0) continue;
    if (ccw === undefined) ccw = area < 0;
    if (ccw === area < 0) {
      if (count > 0) counts.push(count);
      count = 1;
    } else {
      count++;
    }
  }
  if (count > 0) counts.push(count);
  return counts;
}

const perZoom = new Map();
const offenders = [];
let checked = 0;
const seenPayloads = new Set();

for await (const t of allTiles()) {
  const payloadKey = `${t.offset}:${t.length}`;
  if (unique && seenPayloads.has(payloadKey)) continue;
  if (unique) seenPayloads.add(payloadKey);
  const [z, x, y] = tileIdToZxy(t.tileId);
  const raw = buf.subarray(header.tileDataOffset + t.offset, header.tileDataOffset + t.offset + t.length);
  let data;
  try {
    data = raw[0] === 0x1f && raw[1] === 0x8b ? gunzipSync(raw) : raw;
  } catch {
    continue;
  }
  checked++;
  const vt = new VectorTile(new PbfReader(data));
  const layer = vt.layers[layerName];
  if (!layer) continue;
  let stat = perZoom.get(z);
  if (!stat) {
    stat = { polygons: 0, maxRings: 0, maxAt: "", over: 0 };
    perZoom.set(z, stat);
  }
  for (let i = 0; i < layer.length; i++) {
    const feature = layer.feature(i);
    if (feature.type !== 3) continue;
    for (const rings of groupRingCounts(feature.loadGeometry())) {
      stat.polygons++;
      if (rings > stat.maxRings) {
        stat.maxRings = rings;
        stat.maxAt = `z${z}/${x}/${y} feat ${i}`;
      }
      if (rings > CAP) {
        stat.over++;
        offenders.push(`z${z}/${x}/${y} feat ${i}: ${rings} rings`);
      }
    }
  }
}

console.log(`${path}  layer=${layerName}  cap=${CAP}  tiles=${checked}`);
console.log("zoom  polygons  max_rings  over_cap  max_at");
for (const z of [...perZoom.keys()].sort((a, b) => a - b)) {
  const s = perZoom.get(z);
  console.log(
    String(z).padStart(4),
    String(s.polygons).padStart(9),
    String(s.maxRings).padStart(10),
    String(s.over).padStart(9),
    ` ${s.maxAt}`,
  );
}
if (offenders.length) {
  console.log(`\n${offenders.length} polygons over the ${CAP}-ring MapLibre clamp:`);
  for (const line of offenders.slice(0, 50)) console.log(" ", line);
  process.exit(1);
}
console.log("\nNo polygon exceeds the MapLibre ring clamp.");
