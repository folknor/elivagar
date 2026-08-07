#!/usr/bin/env node
// seam-probe.mjs <file.pmtiles> <z> <x> <y> [layer] [tolPx]
//
// Cross-feature shared-edge adjudicator for polygon layers, consumer-path
// decode. The question it answers: when two polygon features in one tile
// abut, do they hold the shared boundary as bit-identical vertex chains
// (the pinned-DP guarantee - no seam possible), or as nearly-coincident
// but diverging edges (the seam class: background slivers/overlaps open
// between the fills at render time)?
//
// Method: decode every polygon feature's rings, break them into segments,
// then for every pair of segments from DIFFERENT features classify:
//   - exact:   same two endpoints (either direction) - a shared chain edge,
//              seam-free by construction;
//   - near:    endpoints within tolPx of each other but not equal, and the
//              segments are roughly antiparallel - the seam signature.
// Reported per feature-pair with coordinates, worst first. Advisory
// diagnostic per the oracle discipline in AGENTS.md: it has NOT been
// calibrated on a known-bad artifact, so it is not a gate - a zero "near"
// count supports the no-seam reading, a nonzero count names concrete
// coordinates for feature-probe / zoom-overlay drill-down.
//
// tolPx is in tile pixels at extent scale (default 2). Segments shorter
// than tolPx are skipped (vertex dust cannot render as a seam).

import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { VectorTile } from "@mapbox/vector-tile";
import { PbfReader } from "pbf";
import { PMTiles } from "pmtiles";

class FileSource {
  constructor(path) {
    this.buf = readFileSync(path);
    this.path = path;
  }
  getKey() {
    return this.path;
  }
  async getBytes(offset, length) {
    return {
      data: this.buf.buffer.slice(
        this.buf.byteOffset + offset,
        this.buf.byteOffset + offset + length,
      ),
    };
  }
}

const [, , file, zs, xs, ys, layerArg, tolArg] = process.argv;
if (!file || zs === undefined || xs === undefined || ys === undefined) {
  console.error("usage: seam-probe.mjs <file.pmtiles> <z> <x> <y> [layer] [tolPx]");
  process.exit(2);
}
const z = Number(zs);
const x = Number(xs);
const y = Number(ys);
const onlyLayer = layerArg && layerArg !== "all" ? layerArg : null;
const tolPx = tolArg ? Number(tolArg) : 2;

const pm = new PMTiles(new FileSource(file));
const tile = await pm.getZxy(z, x, y);
if (!tile) {
  console.error(`no tile at z${z}/${x}/${y}`);
  process.exit(1);
}
let bytes = new Uint8Array(tile.data);
if (bytes[0] === 0x1f && bytes[1] === 0x8b) bytes = gunzipSync(bytes);
const vt = new VectorTile(new PbfReader(bytes));

// Segments tagged with their ring index: merge_same_attr_geometries folds
// adjacent same-attribute polygons into one multi-geometry feature at
// assemble time, so a cross-POLYGON seam usually lives between two rings of
// the same feature - the pair unit is the ring, not the feature.
function segmentsOfFeature(feat, ringBase) {
  const segs = [];
  const rings = feat.loadGeometry();
  for (let r = 0; r < rings.length; r++) {
    const ring = rings[r];
    for (let i = 0; i + 1 < ring.length; i++) {
      const a = ring[i];
      const b = ring[i + 1];
      if (a.x === b.x && a.y === b.y) continue;
      segs.push([a.x, a.y, b.x, b.y, ringBase + r]);
    }
  }
  return segs;
}

function d2(ax, ay, bx, by) {
  const dx = ax - bx;
  const dy = ay - by;
  return dx * dx + dy * dy;
}

for (const name of Object.keys(vt.layers)) {
  if (onlyLayer && name !== onlyLayer) continue;
  const layer = vt.layers[name];
  // Tolerance in integer tile units: extent px per "display pixel" at 256px.
  const tol = (layer.extent / 256) * tolPx;
  const tol2 = tol * tol;

  // Collect polygon features' segments, ring-tagged.
  const feats = [];
  let ringBase = 0;
  let ringCount = 0;
  for (let i = 0; i < layer.length; i++) {
    const f = layer.feature(i);
    if (f.type !== 3) continue;
    const segs = segmentsOfFeature(f, ringBase);
    let maxRing = ringBase;
    for (const s of segs) if (s[4] >= maxRing) maxRing = s[4] + 1;
    ringCount += maxRing - ringBase;
    ringBase = maxRing;
    feats.push({ idx: i, id: f.id, segs });
  }
  if (ringCount < 2) continue;

  // Bucket segments by cell for pair lookup.
  const cell = Math.max(1, Math.ceil(tol * 4));
  const grid = new Map();
  feats.forEach((f, fi) => {
    for (const s of f.segs) {
      const cx = Math.floor((s[0] + s[2]) / 2 / cell);
      const cy = Math.floor((s[1] + s[3]) / 2 / cell);
      const key = cx * 100003 + cy;
      let arr = grid.get(key);
      if (!arr) grid.set(key, (arr = []));
      arr.push([fi, s]);
    }
  });

  let exact = 0;
  const near = [];
  const seenPair = new Set();
  for (const arr of grid.values()) {
    for (let i = 0; i < arr.length; i++) {
      for (let j = i + 1; j < arr.length; j++) {
        const [fa, sa] = arr[i];
        const [fb, sb] = arr[j];
        if (sa[4] === sb[4]) continue; // same ring: consecutive-edge noise
        // Same segment, either direction?
        const fwd =
          sa[0] === sb[0] && sa[1] === sb[1] && sa[2] === sb[2] && sa[3] === sb[3];
        const rev =
          sa[0] === sb[2] && sa[1] === sb[3] && sa[2] === sb[0] && sa[3] === sb[1];
        if (fwd || rev) {
          exact++;
          continue;
        }
        // Near-coincident: both endpoint pairings within tol, not equal.
        const lenA = d2(sa[0], sa[1], sa[2], sa[3]);
        const lenB = d2(sb[0], sb[1], sb[2], sb[3]);
        if (lenA < tol2 || lenB < tol2) continue;
        const fwdNear =
          d2(sa[0], sa[1], sb[0], sb[1]) <= tol2 &&
          d2(sa[2], sa[3], sb[2], sb[3]) <= tol2;
        const revNear =
          d2(sa[0], sa[1], sb[2], sb[3]) <= tol2 &&
          d2(sa[2], sa[3], sb[0], sb[1]) <= tol2;
        if (fwdNear || revNear) {
          const pairKey = `${Math.min(sa[4], sb[4])}:${Math.max(sa[4], sb[4])}`;
          near.push({ pairKey, fa, fb, sa, sb });
          seenPair.add(pairKey);
        }
      }
    }
  }

  console.log(
    `${name}: ${feats.length} polygon features / ${ringCount} rings, ` +
      `${exact} exact shared segments, ` +
      `${near.length} near-miss segment pairs across ${seenPair.size} ring pairs ` +
      `(tol ${tolPx}px = ${tol} units)`,
  );
  for (const n of near.slice(0, 20)) {
    console.log(
      `  NEAR feat#${feats[n.fa].idx}(id ${feats[n.fa].id}) x feat#${feats[n.fb].idx}` +
        `(id ${feats[n.fb].id}): (${n.sa[0]},${n.sa[1]})-(${n.sa[2]},${n.sa[3]}) vs ` +
        `(${n.sb[0]},${n.sb[1]})-(${n.sb[2]},${n.sb[3]})`,
    );
  }
  if (near.length > 20) console.log(`  ... ${near.length - 20} more`);
}
