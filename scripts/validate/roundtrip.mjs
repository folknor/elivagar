import { createRequire } from 'module';
const require = createRequire(import.meta.url);
import { PMTiles } from 'pmtiles';
import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'fs';
import { gunzipSync, gzipSync } from 'zlib';
import { join } from 'path';

const { VectorTile } = require('@mapbox/vector-tile');
const PbfModule = require('pbf');
const Protobuf = PbfModule.default || PbfModule;
const vtpbf = require('vt-pbf');

const input = process.argv[2];
const outDir = process.argv[3];
if (!input || !outDir) {
  console.error('Usage: node roundtrip.mjs <input.pmtiles> <output-dir>');
  process.exit(1);
}

const buf = readFileSync(input);
const source = {
  getKey() { return input; },
  async getBytes(offset, length) {
    const slice = buf.subarray(offset, offset + length);
    return { data: slice.buffer.slice(slice.byteOffset, slice.byteOffset + slice.byteLength) };
  }
};

const pm = new PMTiles(source);
const header = await pm.getHeader();

console.log(`Input: ${input}`);
console.log(`Zoom: ${header.minZoom}..${header.maxZoom}`);
console.log(`Output: ${outDir}/`);

if (!existsSync(outDir)) mkdirSync(outDir, { recursive: true });

let tileCount = 0;
let errorCount = 0;

// Iterate only tiles that exist by scanning the bbox at each zoom.
// PMTiles getZxy returns null for missing tiles, so we still check,
// but we limit the range to the data bounds.
const minLon = header.minLon || 7.0;
const maxLon = header.maxLon || 16.0;
const minLat = header.minLat || 54.0;
const maxLat = header.maxLat || 58.5;

function lonToTileX(lon, z) {
  return Math.floor((lon + 180) / 360 * (1 << z));
}
function latToTileY(lat, z) {
  const latRad = lat * Math.PI / 180;
  return Math.floor((1 - Math.log(Math.tan(latRad) + 1 / Math.cos(latRad)) / Math.PI) / 2 * (1 << z));
}

for (let z = header.minZoom; z <= header.maxZoom; z++) {
  const xMin = Math.max(0, lonToTileX(minLon, z) - 1);
  const xMax = Math.min((1 << z) - 1, lonToTileX(maxLon, z) + 1);
  const yMin = Math.max(0, latToTileY(maxLat, z) - 1);
  const yMax = Math.min((1 << z) - 1, latToTileY(minLat, z) + 1);

  for (let x = xMin; x <= xMax; x++) {
    for (let y = yMin; y <= yMax; y++) {
      const resp = await pm.getZxy(z, x, y);
      if (!resp || !resp.data || resp.data.byteLength === 0) continue;

      tileCount++;
      try {
        let raw;
        try { raw = gunzipSync(Buffer.from(resp.data)); }
        catch { raw = Buffer.from(resp.data); }

        const tile = new VectorTile(new Protobuf(raw));
        const reencoded = vtpbf(tile);
        const compressed = gzipSync(Buffer.from(reencoded));

        const dir = join(outDir, String(z), String(x));
        if (!existsSync(dir)) mkdirSync(dir, { recursive: true });
        writeFileSync(join(dir, `${y}.mvt`), compressed);
      } catch (e) {
        errorCount++;
        if (errorCount <= 20) console.log(`Error z${z}/${x}/${y}: ${e.message}`);
      }

      if (tileCount % 5000 === 0) console.log(`  ${tileCount} tiles...`);
    }
  }
}

console.log(`\nDone: ${tileCount} tiles, ${errorCount} errors`);
console.log(`\nTo serve: npx serve ${outDir} --cors -l 3034`);
