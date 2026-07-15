# SVG regression corpus - hardened plan

All three specs this note planned are landed (digest gate, render core +
corpus, bless-machinery teardown); this note's job is done. Trimmed to the
content that stays durable - the ruling and its reasoning, and the viewer
geometry-interpretation findings that are not restated anywhere else. Full
design detail (survey, tier-by-tier writeup, calibration bookkeeping, review
notes) is in git history at the commits below, not repeated here.

## Goal

A git-committed, human-viewable, exhaustively-gated output baseline that
replaces the pmtiles bless machinery: an exhaustive digest gate (standing
gate), a human-viewable SVG corpus (rotation review), and an overlay
attribution renderer (on-demand, two-archive diff). Landed as
`elivagar corpus check|bless|render|render-manifest|rings` and
`elivagar regress --overlay`; current behavior and CLI surface are in
`reference/cli.md` and `AGENTS.md`.

## The decision: the pmtiles bless machinery is replaced

Ruling made 2026-07-15 (delegated by the user). The blessed archive was a
machine-local binary baseline outside version control, and that single
property produced four incidents in one week: the 07-09 rotation grind
(regress ran 15+ minutes against a stale baseline because the de-facto one
had been rotated away), the 07-14 mis-bless (an artifact-absent archive
blessed while every gate passed), the un-gateable window (nothing could
regress against blessed from the provenance landing until the 07-15
re-bless), and the stale-ocean-artifact incident (three days of pre-VW
spikes served world-wide while the 07-15 landings gated against each other -
a chain with no anchor). A baseline in git is versioned with the code,
survives archive rotation by construction, travels to any machine, and turns
every rotation into a reviewable diff.

What is genuinely lost and accepted: regress-vs-blessed gave instant
attribution against a curated baseline archive. The digest detects but
attributes only to a region; attribution needs a comparand archive - the
per-commit archives in `data/tilegen/` in practice, or a rebuild of the old
commit. Accepted because detection is the gate and attribution is a
debugging workflow.

Limitation stated plainly (the incident's lesson): NO baseline diff - old or
new - catches a defect already present in the baseline. Baseline-free
detectors (the earcut oracle, the boundary-line oracle, the H5 seam-gap
candidate) remain the only guard against inherited wrongness, and stay
authoritative.

## Preserved findings: viewer geometry interpretation

(Established pre-incident; the calibration knowledge the render core
implements against. Verified via the earcut oracle's verbatim classifyRings
and a clean-room OpenLayers review session. Not restated elsewhere, so kept
here in full.)

- Both viewers fill NONZERO. evenodd agrees only on well-formed alternating
  outers/holes and papers over exactly the ring-role bugs the corpus must
  catch.
- OpenLayers Canvas submits all of a feature's rings as subpaths of one
  path, nonzero rule; ring order irrelevant, only relative winding.
- MapLibre classifyRings self-calibrates: first nonzero-area ring defines
  outer winding; matching-wound rings start polygons, opposite-wound become
  holes; zero-area rings skipped; maxRings=500 keeps largest by area. ~20
  lines, derivable from the MVT spec.
- The one valid-input divergence: MapLibre's 500-ring clamp (OL keeps all
  rings). No single SVG matches both; flag, never silently pick.
- Practical rule: classifyRings grouping + skip zero-area + nonzero paths
  matches BOTH viewers for polygons within the clamp.
- Raster-only concerns (AA, joins, caps, label collision) live where no
  geometry bug does and are deliberately out of scope.

## Guardrails

- The earcut and boundary-line oracles remain authoritative and untouched;
  the corpus gates changed, they do not gate correctness-in-itself.
- Both corpus gates stay categorical byte-equality on deterministic output.
  No thresholding of continuous quantities - the ocean-coverage
  false-negative trap.
- Baseline-free defect detection (the H5 seam-gap candidate) is a separate
  workstream; the corpus does not subsume it and must not be argued to.

## Status

All three specs landed: digest gate (`ecef3b8` spec, `a8c4f84`/`340215a`
landing), render core + corpus (`8161ed7` spec, `41a953a`/`cb65c2f`
landing), bless-machinery teardown (this landing). See git history for each
design record.
