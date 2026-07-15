// Extract the vertices and edges of SVG path geometry inside a bbox ROI.
// Built for seam/spike triage: dump the same tile from several archives via
// `brokkr svg -o`, then compare what each build emits in the defect region
// without reading whole files. Prints, per path, every edge with at least
// one endpoint inside the ROI; long edges that only CROSS the ROI are
// caught by also printing edges whose segment intersects the bbox.
// Usage: node svg-roi.mjs <minx> <miny> <maxx> <maxy> <file.svg> [more.svg...]
import { readFileSync } from "node:fs";

const [, , x0a, y0a, x1a, y1a, ...files] = process.argv;
const [x0, y0, x1, y1] = [x0a, y0a, x1a, y1a].map(Number);
if (files.length === 0 || [x0, y0, x1, y1].some(Number.isNaN)) {
  console.error("usage: node svg-roi.mjs <minx> <miny> <maxx> <maxy> <file.svg>...");
  process.exit(2);
}

const inBox = (p) => p[0] >= x0 && p[0] <= x1 && p[1] >= y0 && p[1] <= y1;

// Conservative segment-vs-bbox test: true if either endpoint is inside, or
// the segment's bbox overlaps the ROI and the ROI's corners are not all on
// one side of the segment line.
function crossesBox(a, b) {
  if (inBox(a) || inBox(b)) return true;
  if (Math.max(a[0], b[0]) < x0 || Math.min(a[0], b[0]) > x1) return false;
  if (Math.max(a[1], b[1]) < y0 || Math.min(a[1], b[1]) > y1) return false;
  const side = (px, py) =>
    Math.sign((b[0] - a[0]) * (py - a[1]) - (b[1] - a[1]) * (px - a[0]));
  const s = [side(x0, y0), side(x0, y1), side(x1, y0), side(x1, y1)];
  return !(s.every((v) => v >= 0) || s.every((v) => v <= 0));
}

for (const file of files) {
  const svg = readFileSync(file, "utf8");
  console.log(`== ${file}`);
  for (const m of svg.matchAll(/<path id="([^"]+)" d="([^"]+)"/g)) {
    const [, id, d] = m;
    // Subpaths are M...Z runs; vertices are "x y" pairs after M/L.
    const lines = [];
    for (const sub of d.split("M").filter((s) => s.trim().length)) {
      const pts = [...sub.matchAll(/(-?[\d.]+) (-?[\d.]+)/g)].map((c) => [
        Number(c[1]),
        Number(c[2]),
      ]);
      if (pts.length < 2) continue;
      const closed = sub.includes("Z");
      const n = pts.length;
      for (let i = 0; i < (closed ? n : n - 1); i++) {
        const a = pts[i];
        const b = pts[(i + 1) % n];
        if (a[0] === b[0] && a[1] === b[1]) continue;
        if (crossesBox(a, b)) {
          const len = Math.hypot(b[0] - a[0], b[1] - a[1]);
          lines.push(
            `  ${id}[${i}] (${a[0]},${a[1]}) -> (${b[0]},${b[1]}) len=${len.toFixed(1)}`,
          );
        }
      }
    }
    if (lines.length) console.log(lines.join("\n"));
  }
}
