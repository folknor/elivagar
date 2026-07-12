// Dump one boundaries sub-line exactly as MapLibre decodes it, with spur apexes.
// Usage: node line-probe.mjs <file.pmtiles> <z> <x> <y> <featIdx> <subIdx>
import { readFileSync } from "node:fs";
import { PMTiles } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import { gunzipSync } from "node:zlib";

const [path, zArg, xArg, yArg, featArg, subArg] = process.argv.slice(2);
if (!path || subArg === undefined) {
  console.error("usage: node line-probe.mjs <file.pmtiles> <z> <x> <y> <featIdx> <subIdx>");
  process.exit(2);
}
const [z, x, y, featIdx, subIdx] = [zArg, xArg, yArg, featArg, subArg].map(Number);

class BufferSource {
  constructor(buf) { this.buf = buf; }
  getKey() { return "mem"; }
  async getBytes(offset, length) {
    return { data: this.buf.buffer.slice(this.buf.byteOffset + offset, this.buf.byteOffset + offset + length) };
  }
}

const buf = readFileSync(path);
const pm = new PMTiles(new BufferSource(buf));
const tile = await pm.getZxy(z, x, y);
if (!tile) { console.error("tile not found"); process.exit(1); }
let data = new Uint8Array(tile.data);
try { data = gunzipSync(data); } catch { /* raw */ }
const layer = new VectorTile(new PbfReader(data)).layers.boundaries;
if (!layer) { console.error("no boundaries layer"); process.exit(1); }
const feature = layer.feature(featIdx);
console.log(`z${z}/${x}/${y} feat ${featIdx} type=${feature.type} extent=${layer.extent} props=${JSON.stringify(feature.properties)}`);
const lines = feature.loadGeometry();
console.log(`sub-lines: ${lines.length} (vertex counts: ${lines.map(line => line.length).join(", ")})`);
const line = lines[subIdx];
if (!line) { console.error("sub-line not found"); process.exit(1); }
for (const [index, point] of line.entries()) console.log(`  v${index}: ${point.x},${point.y}`);
for (let apex = 1; apex + 1 < line.length; apex++) {
  for (let delta = 1; apex - delta >= 0 && apex + delta < line.length; delta++) {
    if (line[apex - delta].x === line[apex + delta].x && line[apex - delta].y === line[apex + delta].y) {
      console.log(`spur apex v${apex} (${line[apex].x},${line[apex].y}) matches v${apex - delta}==v${apex + delta} at delta ${delta}: (${line[apex - delta].x},${line[apex - delta].y})`);
    }
  }
}
