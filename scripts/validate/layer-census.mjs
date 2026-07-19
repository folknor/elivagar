#!/usr/bin/env node
// layer-census.mjs <file.pmtiles> <z> <x> <y> [layer]
//
// Lists every MVT layer in one tile with its feature count and geometry-type
// breakdown, decoded the way MapLibre does (@mapbox/vector-tile). Answers
// "which layers does this tile actually contain" - e.g. when adjudicating
// whether a corpus SVG is missing a layer or the tile genuinely lacks it.
// With a layer argument, additionally prints that layer's per-attribute
// value histogram (e.g. which land kinds are present and need styling).
import fs from 'node:fs';
import zlib from 'node:zlib';
import { PMTiles } from 'pmtiles';
import { VectorTile } from '@mapbox/vector-tile';
import { PbfReader } from 'pbf';

const [file, zs, xs, ys, only] = process.argv.slice(2);
if (!file || !zs || !xs || !ys) {
  console.error('usage: layer-census.mjs <file.pmtiles> <z> <x> <y> [layer]');
  process.exit(2);
}
const [z, x, y] = [zs, xs, ys].map(Number);

class FileSource {
  constructor(p) { this.fd = fs.openSync(p, 'r'); }
  getKey() { return 'file'; }
  async getBytes(offset, length) {
    const buf = Buffer.alloc(length);
    fs.readSync(this.fd, buf, 0, length, offset);
    return { data: buf.buffer.slice(buf.byteOffset, buf.byteOffset + length) };
  }
}

const pm = new PMTiles(new FileSource(file));
const tile = await pm.getZxy(z, x, y);
if (!tile || !tile.data) {
  console.error(`no tile at ${z}/${x}/${y}`);
  process.exit(1);
}
const raw = Buffer.from(tile.data);
const data = raw[0] === 0x1f && raw[1] === 0x8b ? zlib.gunzipSync(raw) : raw;
const vt = new VectorTile(new PbfReader(data));

const TYPE = { 1: 'point', 2: 'line', 3: 'polygon' };
for (const [name, layer] of Object.entries(vt.layers)) {
  const types = {};
  for (let i = 0; i < layer.length; i++) {
    const t = TYPE[layer.feature(i).type] ?? 'unknown';
    types[t] = (types[t] ?? 0) + 1;
  }
  const parts = Object.entries(types).map(([t, n]) => `${n} ${t}`).join(', ');
  console.log(`${name}: ${layer.length} features (${parts})`);
  if (only === name) {
    const values = {};
    for (let i = 0; i < layer.length; i++) {
      for (const [k, v] of Object.entries(layer.feature(i).properties)) {
        const key = `${k}=${v}`;
        values[key] = (values[key] ?? 0) + 1;
      }
    }
    for (const [kv, n] of Object.entries(values).sort((a, b) => b[1] - a[1])) {
      console.log(`  ${kv}: ${n}`);
    }
  }
}
