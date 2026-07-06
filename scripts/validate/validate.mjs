import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const vtvalidate = require('@mapbox/vtvalidate');
import { PMTiles } from 'pmtiles';
import { readFileSync } from 'fs';
import { gunzipSync } from 'zlib';

const file = process.argv[2];
if (!file) {
  console.error('Usage: node validate.mjs <pmtiles-file>');
  process.exit(1);
}

// PMTiles source from file
const buf = readFileSync(file);
const source = {
  getKey() { return file; },
  async getBytes(offset, length) {
    const slice = buf.subarray(offset, offset + length);
    return { data: slice.buffer.slice(slice.byteOffset, slice.byteOffset + slice.byteLength) };
  }
};

const pm = new PMTiles(source);
const header = await pm.getHeader();

let tileCount = 0;
let errorCount = 0;
let errorTiles = [];

// Iterate all tiles
for (let z = header.minZoom; z <= header.maxZoom; z++) {
  const maxTile = (1 << z) - 1;
  for (let x = 0; x <= maxTile; x++) {
    for (let y = 0; y <= maxTile; y++) {
      const resp = await pm.getZxy(z, x, y);
      if (!resp || !resp.data || resp.data.byteLength === 0) continue;

      tileCount++;
      let raw;
      try {
        raw = gunzipSync(Buffer.from(resp.data));
      } catch {
        // Not gzipped, use as-is
        raw = Buffer.from(resp.data);
      }

      try {
        const result = await new Promise((resolve, reject) => {
          vtvalidate.isValid(raw, (err, result) => {
            if (err) reject(err);
            else resolve(result);
          });
        });
        if (result) {
          errorCount++;
          errorTiles.push({ z, x, y, error: result });
          if (errorTiles.length <= 50) {
            console.log(`INVALID z${z}/${x}/${y}: ${result}`);
          }
        }
      } catch (e) {
        errorCount++;
        errorTiles.push({ z, x, y, error: e.message });
        if (errorTiles.length <= 50) {
          console.log(`ERROR z${z}/${x}/${y}: ${e.message}`);
        }
      }
    }
  }
}

console.log(`\n--- Summary ---`);
console.log(`Tiles checked: ${tileCount}`);
console.log(`Invalid tiles: ${errorCount}`);
if (errorTiles.length > 50) {
  console.log(`(showing first 50 of ${errorTiles.length})`);
}
