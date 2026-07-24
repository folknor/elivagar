#!/usr/bin/env node
// Independent MapLibre ring-grouping dump.  It deliberately does not invoke
// the gate's decoder: PMTiles traversal, MVT decoding, and classifyRings all
// happen in Node so cmp against `brokkr pmtiles-corpus rings` is a real
// differential check.
import {readFileSync, writeFileSync} from 'node:fs';
import {gunzipSync} from 'node:zlib';
import {PMTiles, tileIdToZxy} from 'pmtiles';
import {VectorTile} from '@mapbox/vector-tile';
import {PbfReader} from 'pbf';

const args = process.argv.slice(2);
if (args.length !== 3 || args[1] !== '-o') {
  console.error('usage: ring-grouping-oracle.mjs <file.pmtiles> -o <out.txt>');
  process.exit(2);
}
class BufferSource { constructor(buf) { this.buf = buf; } getKey() { return 'rings'; }
  async getBytes(offset, length) { return {data: this.buf.buffer.slice(this.buf.byteOffset + offset, this.buf.byteOffset + offset + length)}; } }
const buf = readFileSync(args[0]);
const pm = new PMTiles(new BufferSource(buf));
const header = await pm.getHeader();
async function* tiles() { const root = await pm.cache.getDirectory(pm.source, header.rootDirectoryOffset, header.rootDirectoryLength, header); const stack=[root]; while(stack.length) { for (const entry of stack.pop()) { if (entry.runLength === 0) stack.push(await pm.cache.getDirectory(pm.source, header.leafDirectoryOffset + entry.offset, entry.length, header)); else for(let i=0;i<entry.runLength;i++) yield {tileId:entry.tileId+i,offset:entry.offset,length:entry.length}; } } }
function signed(ring) { let sum=0; for(let i=0,j=ring.length-1;i<ring.length;j=i++) { const a=ring[i], b=ring[j]; sum+=(b.x-a.x)*(b.y+a.y); } return sum; }
function classify(rings) { if(rings.length<=1) return rings.length ? [rings] : []; const polygons=[]; let polygon, ccw; for(const ring of rings) { const area=signed(ring); if(area===0) continue; ring.area=Math.abs(area); if(ccw===undefined) ccw=area<0; if(ccw === (area<0)) { if(polygon) polygons.push(polygon); polygon=[ring]; } else polygon.push(ring); } if(polygon) polygons.push(polygon); for(let i=0;i<polygons.length;i++) if(polygons[i].length>500) polygons[i]=polygons[i].sort((a,b)=>b.area-a.area).slice(0,500); return polygons; }
const lines=[];
for await (const entry of tiles()) { const [z,x,y]=tileIdToZxy(entry.tileId); const raw=buf.subarray(header.tileDataOffset+entry.offset,header.tileDataOffset+entry.offset+entry.length); let data; try { data=gunzipSync(raw); } catch { data=raw; } const tile=new VectorTile(new PbfReader(data)); for(const [name, layer] of Object.entries(tile.layers)) for(let i=0;i<layer.length;i++) { const feature=layer.feature(i); if(feature.type!==3) continue; const groups=classify(feature.loadGeometry()); const text=groups.length ? groups.map(p=>p.map(r=>r.length).join('+')).join('|') : '-'; lines.push(`${z}/${x}/${y} ${name} ${i} ${text}`); } }
lines.sort();
writeFileSync(args[2], `${lines.join('\n')}${lines.length?'\n':''}`);
