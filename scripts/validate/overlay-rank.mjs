#!/usr/bin/env node
// Ranks `regress --overlay` SVGs by how much fill area changed.
//
// Usage: node overlay-rank.mjs <overlay-dir> [top]
//
// For each structural_moved pair (current pink, comparand blue, as in
// overlay-diff.mjs) this computes the nonzero-signed area of each side and
// the absolute difference, then prints the `top` (default 30) pairs by that
// difference, with the change as a fraction of the comparand's area. A
// one-unit vertex snap moves area by a few square units; a dropped
// fragment, wedge or spike moves it by far more, so the head of this list
// is where to look. ADVISORY: area can stay equal under a visible change
// (a moved notch), so it ranks, it does not clear. No dependencies.

import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

const CURRENT = '#e91e63';
const COMPARAND = '#2196f3';

function parseRings(d) {
  const rings = [];
  let ring = null;
  for (const token of d.match(/[MLZ]|-?\d+(?:\.\d+)?/g) ?? []) {
    if (token === 'M') {
      ring = [];
      rings.push(ring);
    } else if (token === 'Z' || token === 'L') {
      continue;
    } else {
      const last = ring[ring.length - 1];
      if (last && last.length === 1) last.push(Number(token));
      else ring.push([Number(token)]);
    }
  }
  return rings;
}

function area(rings) {
  let sum = 0;
  for (const ring of rings) {
    for (let i = 0; i < ring.length; i++) {
      const [x0, y0] = ring[i];
      const [x1, y1] = ring[(i + 1) % ring.length];
      sum += x0 * y1 - x1 * y0;
    }
  }
  return Math.abs(sum / 2);
}

function ringArea(ring) {
  return area([ring]);
}

function ringBox(ring) {
  const xs = ring.map((p) => p[0]);
  const ys = ring.map((p) => p[1]);
  return `x ${Math.min(...xs)}..${Math.max(...xs)} y ${Math.min(...ys)}..${Math.max(...ys)}`;
}

/// Rings on one side with no counterpart on the other: a counterpart has a
/// bounding box within 2 units on every side and an area within 2% (or 8
/// square units). Prints each unmatched ring's area, vertex count and box.
function ringDelta(file, pair) {
  const svg = readFileSync(file, 'utf8');
  const paths = [
    ...svg.matchAll(/<path data-class="structural_moved" d="([^"]*)" fill="([^"]*)"/g),
  ].map((m) => ({ d: m[1], colour: m[2] }));
  const side = (c) => parseRings(paths.filter((p) => p.colour === c)[pair]?.d ?? '');
  const box = (r) => [
    Math.min(...r.map((p) => p[0])),
    Math.min(...r.map((p) => p[1])),
    Math.max(...r.map((p) => p[0])),
    Math.max(...r.map((p) => p[1])),
  ];
  const matches = (r, s) => {
    const a = box(r);
    const b = box(s);
    if (a.some((v, i) => Math.abs(v - b[i]) > 2)) return false;
    const x = ringArea(r);
    const y = ringArea(s);
    return Math.abs(x - y) <= Math.max(8, 0.02 * Math.max(x, y));
  };
  const cur = side(CURRENT);
  const cmp = side(COMPARAND);
  for (const [label, a, b] of [
    ['current', cur, cmp],
    ['comparand', cmp, cur],
  ]) {
    const unmatched = a.filter((r) => !b.some((s) => matches(r, s)));
    console.log(`${label}: ${a.length} rings, ${unmatched.length} unmatched`);
    for (const r of unmatched) {
      console.log(`  area ${ringArea(r).toFixed(1)}  ${r.length}v  ${ringBox(r)}`);
    }
  }
}

if (process.argv[2] === '--rings') {
  ringDelta(process.argv[3], Number(process.argv[4] ?? 0));
  process.exit(0);
}

const [dir, topArg] = process.argv.slice(2);
const top = Number(topArg ?? 30);
const rows = [];
for (const name of readdirSync(dir).filter((n) => n.endsWith('.svg')).sort()) {
  const svg = readFileSync(join(dir, name), 'utf8');
  const paths = [
    ...svg.matchAll(/<path data-class="structural_moved" d="([^"]*)" fill="([^"]*)"(?: stroke="([^"]*)")?/g),
  ].map((m) => ({ d: m[1], colour: m[2] === 'none' ? m[3] : m[2], line: m[2] === 'none' }));
  const cur = paths.filter((p) => p.colour === CURRENT);
  const cmp = paths.filter((p) => p.colour === COMPARAND);
  for (let i = 0; i < Math.max(cur.length, cmp.length); i++) {
    if (cur[i]?.line || cmp[i]?.line) continue;
    const a = cur[i] ? area(parseRings(cur[i].d)) : 0;
    const b = cmp[i] ? area(parseRings(cmp[i].d)) : 0;
    rows.push({ name, pair: i, a, b, diff: Math.abs(a - b) });
  }
}
rows.sort((x, y) => y.diff - x.diff);
console.log(`pairs ranked: ${rows.length}`);
console.log('diff_area  rel       current_area  comparand_area  file pair');
for (const r of rows.slice(0, top)) {
  const rel = r.b > 0 ? (r.diff / r.b).toExponential(2) : 'inf';
  console.log(
    `${r.diff.toFixed(1).padStart(9)}  ${rel.padEnd(8)}  ${r.a.toFixed(1).padStart(12)}  ${r.b.toFixed(1).padStart(14)}  ${r.name} ${r.pair}`,
  );
}
const small = rows.filter((r) => r.diff <= 16).length;
console.log(`pairs with |area change| <= 16 square units: ${small} of ${rows.length}`);
