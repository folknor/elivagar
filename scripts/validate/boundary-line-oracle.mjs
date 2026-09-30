// Detect boundary-line retraces and duplicate sub-lines after MVT decoding.
// Usage: node boundary-line-oracle.mjs <file.pmtiles> [--only categories]
//          [--census] [--min-extent N] [--strict]
//
// Exit status is driven by intra-duplicate and cross-duplicate only. Those
// are categorical: 0 on correct output of every dataset measured. Palindrome
// and spur are ADVISORY: as output shapes they cannot tell a fabricated
// out-and-back from legitimate geometry that quantization made degenerate
// (a sub-unit-wide islet ring rounding both sides onto the same points, a
// narrow spit sharing one vertex at its neck), and denmark carries hundreds
// of those on correct output. The merger defects these categories were built
// for are gated in code instead, by the witness-based invariant checker in
// src/mvt/merge.rs. --strict restores exit 1 on every category.
//
// --census adds, per zoom, a histogram of retrace extent for palindromes and
// spurs: the largest distance (extent units) from the retrace pivot out to
// the apex. Extent says how large a retrace is, not what made it: a 1-unit
// jog can be rounding, and a long one can be a merger splice (the norway
// z13/4319/2421 case was 982 units).
// --min-extent N restricts the named offender list (not the counts) to
// retraces of at least N units, to drill into the tail.
import { readFileSync } from "node:fs";
import { PMTiles, tileIdToZxy } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import { gunzipSync } from "node:zlib";

const args = process.argv.slice(2);
const census = args.includes("--census");
const strict = args.includes("--strict");
const GATING = new Set(["intra-duplicate", "cross-duplicate"]);
const onlyAt = args.indexOf("--only");
const minExtentAt = args.indexOf("--min-extent");
const minExtent = minExtentAt < 0 ? 0 : Number(args[minExtentAt + 1]);
const valueAt = new Set([onlyAt, minExtentAt].filter(i => i >= 0).map(i => i + 1));
const path = args.find((arg, i) => !arg.startsWith("--") && !valueAt.has(i));
const categories = ["palindrome", "spur", "intra-duplicate", "cross-duplicate"];
const selected = new Set(
  onlyAt < 0 ? categories : (args[onlyAt + 1] ?? "").split(",").filter(Boolean),
);
if (!path || [...selected].some(category => !categories.includes(category))) {
  console.error("usage: node boundary-line-oracle.mjs <file.pmtiles> [--only palindrome,spur,intra-duplicate,cross-duplicate] [--census] [--min-extent N] [--strict]");
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
function dist(a, b) { return Math.hypot(a.x - b.x, a.y - b.y); }
// Retrace extent of a palindrome: its apex is the middle vertex.
function palindromeExtent(line) { return dist(line[0], line[Math.floor(line.length / 2)]); }
// Returns the largest retrace extent found, or -1 when there is no spur.
function spur(line) {
  // A closed ring encoded as a linestring shares its first and last vertex;
  // that closure pair matches the symmetric-apex test at the exact midpoint
  // of any odd-vertex-count loop without any retrace existing. Skip it.
  const last = line.length - 1;
  const closed = line[0].x === line[last].x && line[0].y === line[last].y;
  let extent = -1;
  for (let apex = 1; apex + 1 < line.length; apex++) {
    for (let delta = 1; apex - delta >= 0 && apex + delta < line.length; delta++) {
      if (closed && apex - delta === 0 && apex + delta === last) continue;
      const pivot = line[apex - delta];
      if (pivot.x === line[apex + delta].x && pivot.y === line[apex + delta].y) {
        extent = Math.max(extent, dist(pivot, line[apex]));
      }
    }
  }
  return extent;
}
const BUCKETS = [1, 2, 4, 8, 16, 64, 256, Infinity];
function bucket(extent) { return BUCKETS.find(limit => extent <= limit); }
const histogram = new Map();
function record(z, category, extent) {
  const k = `${z}:${category}:${bucket(extent)}`;
  histogram.set(k, (histogram.get(k) ?? 0) + 1);
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
          const found = (category, extent = Infinity) => {
            counts.set(`${z}:${category}`, (counts.get(`${z}:${category}`) ?? 0) + 1);
            if (selected.has(category) && extent >= minExtent && offenders.length < 15) {
              offenders.push(`${category} ${id}${Number.isFinite(extent) ? ` extent ${extent.toFixed(1)}` : ""}`);
            }
          };
          if (palindrome(line)) {
            const extent = palindromeExtent(line);
            found("palindrome", extent);
            record(z, "palindrome", extent);
          }
          const spurExtent = spur(line);
          if (spurExtent >= 0) {
            found("spur", spurExtent);
            record(z, "spur", spurExtent);
          }
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
if (census) {
  console.log("retrace extent census (extent units, upper bucket bounds):");
  const zooms = [...new Set([...histogram.keys()].map(value => Number(value.split(":")[0])))].sort((a, b) => a - b);
  for (const category of ["palindrome", "spur"]) {
    for (const z of zooms) {
      const row = BUCKETS.map(limit => `<=${limit}:${histogram.get(`${z}:${category}:${limit}`) ?? 0}`);
      if (row.some(cell => !cell.endsWith(":0"))) console.log(`  ${category} z${z} ${row.join(" ")}`);
    }
  }
}
for (const category of selected) {
  if (!strict && !GATING.has(category)) continue;
  failed ||= [...counts.entries()].some(([key, value]) => key.endsWith(`:${category}`) && value > 0);
}
if (!strict) console.log("(palindrome and spur are advisory; exit status reflects duplicates only - --strict gates all)");
process.exit(failed ? 1 : 0);
