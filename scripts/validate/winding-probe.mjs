// Print MapLibre's classifyRings signedArea for each ring of each polygon
// feature in one tile, per layer. Empirically settles winding conventions.
// Usage: node winding-probe.mjs <file.pmtiles> <z> <x> <y> [layer]
import { readFileSync } from "node:fs";
import { PMTiles } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import { gunzipSync } from "node:zlib";

const [, , path, zArg, xArg, yArg, onlyLayer] = process.argv;
const [z, x, y] = [Number(zArg), Number(xArg), Number(yArg)];

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
if (!resp) { console.log("no tile"); process.exit(1); }
let data;
try { data = gunzipSync(Buffer.from(resp.data)); } catch { data = Buffer.from(resp.data); }
const vt = new VectorTile(new PbfReader(data));

function signedArea(ring) {
  let sum = 0;
  for (let i = 0, len = ring.length, j = len - 1; i < len; j = i++) {
    sum += (ring[i].x - ring[j].x) * (ring[i].y + ring[j].y);
  }
  return sum;
}

for (const [name, layer] of Object.entries(vt.layers)) {
  if (onlyLayer && name !== onlyLayer) continue;
  let polyFeats = 0;
  for (let i = 0; i < layer.length; i++) {
    const feat = layer.feature(i);
    if (feat.type !== 3) continue;
    polyFeats++;
    if (polyFeats > 6) { console.log(`  ... (more features)`); break; }
    const rings = feat.loadGeometry();
    const verdicts = rings.map(r => {
      const a = signedArea(r);
      return `${r.length}v ${a > 0 ? "OUTER" : a < 0 ? "hole" : "ZERO"}(${a.toExponential(1)})`;
    });
    console.log(`${name} feat ${i}: ${verdicts.join("  ")}`);
  }
}
