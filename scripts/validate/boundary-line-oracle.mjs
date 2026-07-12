// Detect boundary-line retraces and duplicate sub-lines after MVT decoding.
// Usage: node boundary-line-oracle.mjs <file.pmtiles> [--only categories]
import { readFileSync } from "node:fs";
import { PMTiles, tileIdToZxy } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import { gunzipSync } from "node:zlib";

const args = process.argv.slice(2);
const path = args.find(arg => !arg.startsWith("--"));
const onlyAt = args.indexOf("--only");
const categories = ["palindrome", "spur", "intra-duplicate", "cross-duplicate"];
const selected = new Set(
  onlyAt < 0 ? categories : (args[onlyAt + 1] ?? "").split(",").filter(Boolean),
);
if (!path || [...selected].some(category => !categories.includes(category))) {
  console.error("usage: node boundary-line-oracle.mjs <file.pmtiles> [--only palindrome,spur,intra-duplicate,cross-duplicate]");
  process.exit(2);
}

class BufferSource {
  constructor(buf) { this.buf = buf; }
  getKey() { return "mem"; }
  async getBytes(offset, length) {
    return { data: this.buf.buffer.slice(this.buf.byteOffset + offset, this.buf.byteOffset + offset + length) };
  }
}

function key(line) { return line.map(point => `${point.x},${point.y}`).join(";"); }
function reverseKey(line) { return key([...line].reverse()); }
function canonical(line) { const fwd = key(line); const rev = reverseKey(line); return fwd < rev ? fwd : rev; }
function palindrome(line) { return key(line) === reverseKey(line); }
function spur(line) {
  // A closed ring encoded as a linestring shares its first and last vertex;
  // that closure pair matches the symmetric-apex test at the exact midpoint
  // of any odd-vertex-count loop without any retrace existing. Skip it.
  const last = line.length - 1;
  const closed = line[0].x === line[last].x && line[0].y === line[last].y;
  for (let apex = 1; apex + 1 < line.length; apex++) {
    for (let delta = 1; apex - delta >= 0 && apex + delta < line.length; delta++) {
      if (closed && apex - delta === 0 && apex + delta === last) continue;
      if (line[apex - delta].x === line[apex + delta].x && line[apex - delta].y === line[apex + delta].y) return true;
    }
  }
  return false;
}

const buf = readFileSync(path);
const pm = new PMTiles(new BufferSource(buf));
const header = await pm.getHeader();
const root = await pm.cache.getDirectory(pm.source, header.rootDirectoryOffset, header.rootDirectoryLength, header);
const dirs = [root];
const counts = new Map();
const offenders = [];
while (dirs.length) {
  const dir = dirs.pop();
  for (const entry of dir) {
    if (entry.runLength === 0) {
      dirs.push(await pm.cache.getDirectory(pm.source, header.leafDirectoryOffset + entry.offset, entry.length, header));
      continue;
    }
    for (let run = 0; run < entry.runLength; run++) {
      const [z, x, y] = tileIdToZxy(entry.tileId + run);
      const raw = buf.subarray(header.tileDataOffset + entry.offset, header.tileDataOffset + entry.offset + entry.length);
      let data; try { data = gunzipSync(raw); } catch { data = raw; }
      let layer; try { layer = new VectorTile(new PbfReader(data)).layers.boundaries; } catch { continue; }
      if (!layer) continue;
      const tileLines = new Map();
      for (let feat = 0; feat < layer.length; feat++) {
        const feature = layer.feature(feat);
        if (feature.type !== 2) continue;
        const seen = new Set();
        for (const [sub, line] of feature.loadGeometry().entries()) {
          const id = `z${z}/${x}/${y} feat ${feat} sub ${sub}`;
          const found = category => { counts.set(`${z}:${category}`, (counts.get(`${z}:${category}`) ?? 0) + 1); if (selected.has(category) && offenders.length < 15) offenders.push(`${category} ${id}`); };
          if (palindrome(line)) found("palindrome");
          if (spur(line)) found("spur");
          const lineKey = canonical(line);
          if (seen.has(lineKey)) found("intra-duplicate");
          seen.add(lineKey);
          const previous = tileLines.get(lineKey);
          if (previous !== undefined && previous !== feat) found("cross-duplicate");
          tileLines.set(lineKey, feat);
        }
      }
    }
  }
}

let failed = false;
console.log(`${path} boundaries line oracle`);
for (const z of [...new Set([...counts.keys()].map(value => Number(value.split(":")[0])))].sort((a, b) => a - b)) {
  console.log(`z${z} ${categories.map(category => `${category}=${counts.get(`${z}:${category}`) ?? 0}`).join(" ")}`);
}
for (const offender of offenders) console.log(`  ${offender}`);
for (const category of selected) failed ||= [...counts.entries()].some(([key, value]) => key.endsWith(`:${category}`) && value > 0);
process.exit(failed ? 1 : 0);
