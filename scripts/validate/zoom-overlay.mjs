// Low-zoom generalization adjudicator: renders one tile's polygons as fill
// and superimposes the SAME archive's higher-zoom geometry for the same
// region as a stroked outline, rescaled into the target tile's pixel space.
//
// A correct generalization tracks the fine outline within a few target-zoom
// pixels everywhere, deviating evenly; the low-zoom ocean defect classes
// (seam wedges, dropped cell fragments, half-peninsula spikes) show as fill
// diverging from the outline in one spot. This answers "is what I'm seeing
// at z2 how it SHOULD look" without an external ground truth: the detail
// zoom is the ground truth, at N times the resolution.
//
// Usage: node zoom-overlay.mjs <file.pmtiles> <z> <x> <y> <detailZ> [layer] [-o out.svg]
import { readFileSync, writeFileSync } from "node:fs";
import { PMTiles } from "pmtiles";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import { gunzipSync } from "node:zlib";

const args = process.argv.slice(2);
let outPath = null;
const oIdx = args.indexOf("-o");
if (oIdx !== -1) {
  outPath = args[oIdx + 1];
  args.splice(oIdx, 2);
}
const [path, zs, xs, ys, dzs, layerName = "ocean"] = args;
if (!path || !zs || !xs || !ys || !dzs) {
  console.error("usage: node zoom-overlay.mjs <file.pmtiles> <z> <x> <y> <detailZ> [layer] [-o out.svg]");
  process.exit(2);
}
const [z, x, y, detailZ] = [zs, xs, ys, dzs].map(Number);
if (detailZ <= z) {
  console.error("detailZ must be greater than z");
  process.exit(2);
}

class BufferSource {
  constructor(buf) { this.buf = buf; }
  getKey() { return "mem"; }
  async getBytes(offset, length) {
    return { data: this.buf.buffer.slice(this.buf.byteOffset + offset, this.buf.byteOffset + offset + length) };
  }
}

const buf = readFileSync(path);
const pm = new PMTiles(new BufferSource(buf));

async function tileLayer(tz, tx, ty) {
  const t = await pm.getZxy(tz, tx, ty);
  if (!t || !t.data) return null;
  const raw = Buffer.from(t.data);
  let data;
  try {
    data = raw[0] === 0x1f && raw[1] === 0x8b ? gunzipSync(raw) : raw;
  } catch {
    return null;
  }
  return new VectorTile(new PbfReader(data)).layers[layerName] ?? null;
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

const parts = [];
parts.push(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4096 4096" width="2048" height="2048">`);
parts.push(`  <title>${z}/${x}/${y} fill vs z${detailZ} outline (${layerName})</title>`);
parts.push(`  <rect width="4096" height="4096" fill="#f2efe9"/>`);

// Target tile polygons, filled.
const target = await tileLayer(z, x, y);
if (!target) {
  console.error(`no ${layerName} layer in ${z}/${x}/${y}`);
  process.exit(1);
}
const targetScale = 4096 / target.extent;
parts.push(`  <g id="target-z${z}" fill="#aad3df" fill-rule="nonzero">`);
for (let i = 0; i < target.length; i++) {
  const feature = target.feature(i);
  if (feature.type !== 3) continue;
  let d = "";
  for (const ring of feature.loadGeometry()) {
    if (ring.length < 3) continue;
    d += `M${ring[0].x * targetScale} ${ring[0].y * targetScale}`;
    for (let k = 1; k < ring.length; k++) d += ` L${ring[k].x * targetScale} ${ring[k].y * targetScale}`;
    d += " Z";
  }
  if (d) parts.push(`    <path d="${d}"/>`);
}
parts.push(`  </g>`);

// Detail zoom outline, rescaled into target tile space. Ring vertices in the
// 128-unit tile buffer are drawn too (slight double-stroke on tile seams).
const span = 1 << (detailZ - z);
// Thin stroke on purpose: the composite is made to be inspected zoomed-in,
// and a wide stroke (in viewBox units it scales with the zoom) buries the
// deviation band it exists to reveal.
parts.push(`  <g id="detail-z${detailZ}" fill="none" stroke="#d63333" stroke-width="0.6" stroke-linejoin="round">`);
let detailTiles = 0;
for (let dy = 0; dy < span; dy++) {
  for (let dx = 0; dx < span; dx++) {
    const layer = await tileLayer(detailZ, x * span + dx, y * span + dy);
    if (!layer) continue;
    detailTiles++;
    const scale = 4096 / layer.extent / span;
    const ox = dx * (4096 / span);
    const oy = dy * (4096 / span);
    for (let i = 0; i < layer.length; i++) {
      const feature = layer.feature(i);
      if (feature.type !== 3) continue;
      let d = "";
      for (const ring of feature.loadGeometry()) {
        if (ring.length < 3 || calculateSignedArea(ring) === 0) continue;
        // Precision matters: at detailZ = z+6 one detail pixel is ~0.016
        // viewBox units, so rounding must stay well below that or the
        // outline degrades to a coarser zoom's detail.
        const px = (p) => `${(ox + p.x * scale).toFixed(3)} ${(oy + p.y * scale).toFixed(3)}`;
        d += `M${px(ring[0])}`;
        for (let k = 1; k < ring.length; k++) d += ` L${px(ring[k])}`;
        d += " Z";
      }
      if (d) parts.push(`    <path d="${d}"/>`);
    }
  }
}
parts.push(`  </g>`);
parts.push(`</svg>`);

const svg = parts.join("\n") + "\n";
if (outPath) {
  writeFileSync(outPath, svg);
  console.error(`wrote ${outPath} (${detailTiles} z${detailZ} detail tiles under z${z}/${x}/${y})`);
} else {
  process.stdout.write(svg);
}
