#!/usr/bin/env node
// Summarizes what changed in a `brokkr regress --overlay` SVG.
//
// Usage: node overlay-diff.mjs [--rings] <overlay.svg | overlay-dir>...
//
// Each overlay carries every structural_moved feature twice, current
// (pink, #e91e63) then comparand (blue, #2196f3). For each pair this
// prints ring counts and totals, the vertices present on only one side
// with their bounding box, and the edges present on only one side - with
// how many of those touch the visible tile square [0, EXTENT]^2. A change
// whose every differing edge misses that square cannot alter what a
// clipping fill renderer draws inside the tile; a single touching edge
// means it can, and the tile needs a visual look. --rings adds per-ring
// vertex counts and areas. No dependencies.

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

const CURRENT = '#e91e63';
const COMPARAND = '#2196f3';
const EXTENT = 4096;

/// Sub-paths of an SVG path. Each is an array of [x, y] points carrying a
/// `closed` flag: polygon rings end in Z, line sub-paths do not, and only a
/// closed ring has an edge from its last point back to its first.
function parseRings(d) {
  const rings = [];
  let ring = null;
  for (const token of d.match(/[MLZ]|-?\d+(?:\.\d+)?/g) ?? []) {
    if (token === 'M') {
      ring = [];
      ring.closed = false;
      rings.push(ring);
    } else if (token === 'Z') {
      if (ring) ring.closed = true;
    } else if (token === 'L') {
      continue;
    } else {
      const last = ring[ring.length - 1];
      if (last && last.length === 1) last.push(Number(token));
      else ring.push([Number(token)]);
    }
  }
  return rings;
}

function area2(ring) {
  let sum = 0;
  for (let i = 0; i < ring.length; i++) {
    const [x0, y0] = ring[i];
    const [x1, y1] = ring[(i + 1) % ring.length];
    sum += x0 * y1 - x1 * y0;
  }
  return sum;
}

function bbox(points) {
  if (points.length === 0) return '-';
  const xs = points.map((p) => p[0]);
  const ys = points.map((p) => p[1]);
  return `x ${Math.min(...xs)}..${Math.max(...xs)}  y ${Math.min(...ys)}..${Math.max(...ys)}`;
}

function describe(label, rings, perRing) {
  const total = rings.reduce((n, r) => n + r.length, 0);
  const summary = perRing
    ? `  [${rings.map((r) => `${r.length}v/${(area2(r) / 2).toFixed(1)}`).join(' ')}]`
    : '';
  console.log(`  ${label}: ${rings.length} rings, ${total} vertices${summary}`);
}

function onlyIn(a, b) {
  const keys = new Set(b.flat().map((p) => `${p[0]},${p[1]}`));
  return a.flat().filter((p) => !keys.has(`${p[0]},${p[1]}`));
}

/// Undirected edges as a multiset, keyed by their sorted endpoints.
function edgeCounts(rings) {
  const counts = new Map();
  for (const ring of rings) {
    const edges = ring.closed ? ring.length : ring.length - 1;
    for (let i = 0; i < edges; i++) {
      const a = ring[i];
      const b = ring[(i + 1) % ring.length];
      const [p, q] = a[0] < b[0] || (a[0] === b[0] && a[1] <= b[1]) ? [a, b] : [b, a];
      const key = `${p[0]},${p[1]},${q[0]},${q[1]}`;
      counts.set(key, (counts.get(key) ?? 0) + 1);
    }
  }
  return counts;
}

function edgesOnlyIn(a, b) {
  const out = [];
  for (const [key, count] of a) {
    const extra = count - (b.get(key) ?? 0);
    for (let i = 0; i < extra; i++) out.push(key.split(',').map(Number));
  }
  return out;
}

/// Liang-Barsky: does the closed segment touch the closed square [0, EXTENT]^2?
function touchesVisible([x0, y0, x1, y1]) {
  const dx = x1 - x0;
  const dy = y1 - y0;
  let t0 = 0;
  let t1 = 1;
  for (const [p, q] of [
    [-dx, x0],
    [dx, EXTENT - x0],
    [-dy, y0],
    [dy, EXTENT - y0],
  ]) {
    if (p === 0) {
      if (q < 0) return false;
    } else {
      const t = q / p;
      if (p < 0) t0 = Math.max(t0, t);
      else t1 = Math.min(t1, t);
      if (t0 > t1) return false;
    }
  }
  return true;
}

let differingEdges = 0;
function reportEdges(label, edges) {
  differingEdges += edges.length;
  const visible = edges.filter(touchesVisible).length;
  const points = edges.flatMap(([x0, y0, x1, y1]) => [
    [x0, y0],
    [x1, y1],
  ]);
  console.log(
    `  edges only in ${label}: ${edges.length}, touching visible square: ${visible}, ${bbox(points)}`,
  );
  return visible;
}

const args = process.argv.slice(2);
const perRing = args.includes('--rings');
// A directory argument stands for every overlay SVG in it.
const files = args
  .filter((a) => a !== '--rings')
  .flatMap((a) =>
    statSync(a).isDirectory()
      ? readdirSync(a)
          .filter((name) => name.endsWith('.svg'))
          .sort()
          .map((name) => join(a, name))
      : [a],
  );
let touching = 0;
for (const file of files) {
  const svg = readFileSync(file, 'utf8');
  // Polygons carry the side colour in fill; lines are fill="none" and carry
  // it in stroke.
  const paths = [
    ...svg.matchAll(/<path data-class="structural_moved" d="([^"]*)" fill="([^"]*)"(?: stroke="([^"]*)")?/g),
  ].map((m) => ({ d: m[1], colour: m[2] === 'none' ? m[3] : m[2] }));
  const current = paths.filter((p) => p.colour === CURRENT).map((p) => parseRings(p.d));
  const comparand = paths.filter((p) => p.colour === COMPARAND).map((p) => parseRings(p.d));
  console.log(`${file}: ${current.length} current, ${comparand.length} comparand`);
  const pairs = Math.max(current.length, comparand.length);
  for (let i = 0; i < pairs; i++) {
    const cur = current[i] ?? [];
    const cmp = comparand[i] ?? [];
    console.log(` pair ${i}`);
    describe('current  ', cur, perRing);
    describe('comparand', cmp, perRing);
    const gained = onlyIn(cur, cmp);
    const lost = onlyIn(cmp, cur);
    console.log(`  vertices only in current:   ${gained.length}, ${bbox(gained)}`);
    console.log(`  vertices only in comparand: ${lost.length}, ${bbox(lost)}`);
    const curEdges = edgeCounts(cur);
    const cmpEdges = edgeCounts(cmp);
    touching += reportEdges('current', edgesOnlyIn(curEdges, cmpEdges));
    touching += reportEdges('comparand', edgesOnlyIn(cmpEdges, curEdges));
  }
}
console.log(`TOTAL edges present on one side only: ${differingEdges}`);
console.log(`TOTAL changed edges touching the visible square: ${touching}`);
