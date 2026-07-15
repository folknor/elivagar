# Spec C: teardown of the pmtiles bless machinery

Written against `reference/technical-implementation-spec.md` (the contract
this document must satisfy). Spawned from the Spec C item in
`notes/svg-corpus-plan.md` ("The decision: the pmtiles bless machinery is
replaced", ruling of 2026-07-15, and the "Spec split" section's item 3,
including the R2 cross-repo ordering and inventory findings folded there).
The measurement record cited throughout is `reference/performance.md` plus
`.brokkr/results.db`.

## What this spec does

Removes the machine-local blessed-archive baseline system - `brokkr bless`,
`brokkr regress` (the blessed-resolving wrapper), `datasets.<D>.blessed` in
brokkr.toml, and every doc paragraph carrying the bless-rotation
discipline - and rotates the standing output gate to the committed corpus
(`elivagar corpus check` against `corpus/denmark/`). Reframes
`elivagar regress` as what it now is: the explicit two-archive semantic
diff and attribution instrument (tier 3), never a baseline resolver.
Renames the `blessed`-named comparand identifiers inside the surviving
regress engine, the R2 decision this spec is required to make rather than
leave as a silent leftover.

Two landings in this repo, ordered so a standing gate exists at every
commit boundary, plus one named brokkr-repo task that is excluded here as
genuinely separate work.

## Entry conditions - verified 2026-07-15

The plan gates Spec C on both prior specs calibrating. Both records exist
in `reference/performance.md`:

- **Spec A (digest gate)**: "Corpus digest gate calibration (2026-07-15,
  commit a8c4f84, denmark locations)". All six readings both directions:
  self-check pass (1,296,999 tiles, 2,156 ms), regzip byte-different
  control pass, drop-tile/nudge-geometry/layer-version mutants each FAIL
  naming the tile, stale-artifact archive REFUSED at the contract
  (`contract mismatch: config.ocean.artifact_key.policy_version`).
- **Spec B (render core + corpus)**: "SVG corpus render core calibration
  (2026-07-15, commit 41a953a, denmark locations)". Differential
  ring-grouping oracle byte-equal over the full archive (59,464,966 bytes
  each side), determinism gate (three renders, byte-identical), tier-2
  FIRES (nudge-geometry mutant moves the SVG), tier-2 CLEARS (regzip
  renders byte-identical, full check passes in 2,376 ms), and the human
  calibration pair against the preserved `bc71cf1` stale-artifact archive.

The plan's own text ("readings not recorded yet", "pending post-commit
step") predates the 41a953a landing; the performance.md record supersedes
it. The committed baseline exists: `corpus/denmark/` holds contract.json,
digest, leaves, manifest.toml, and tiles/, blessed from
`data/tilegen/denmark-a8c4f84.pmtiles`.

**Brick 0 (pre-landing reading, run before any edit):** confirm the gate
is green at the current HEAD so the rotation hands over a working gate:

```
elivagar corpus check data/tilegen/denmark-420534e.pmtiles --corpus corpus/denmark
```

(The `elivagar` binary is the tilegen-built one in `target/release/`; the
calibration record ran these exact subcommands. `420534e` is HEAD at
survey time - substitute the current per-commit archive, or produce one
with `brokkr tilegen --dataset denmark --variant locations` first.)
Expected: exit 0, ~2.2 s. A failure here means an unadjudicated output
change slipped in since `a8c4f84`; that adjudication is prerequisite work
outside this spec, and the teardown does not start until it is resolved.

## The decision inherited

Recorded in full in `notes/svg-corpus-plan.md`; carried here because this
spec executes it. The blessed archive was a machine-local binary baseline
outside version control, and that single property produced four incidents
in one week (the 07-09 rotation grind, the 07-14 mis-bless, the
un-gateable window after the provenance landing, the 07-15
stale-ocean-artifact incident). The corpus baseline is in git: versioned
with the code, survives archive rotation by construction, travels to any
machine, and turns every rotation into a reviewable diff.

Duty mapping (what the bless machinery provided, where each duty lives
after this landing):

| duty | before | after |
|---|---|---|
| exhaustive tile coverage | `brokkr regress`, 1.3M tiles | `corpus check` digest, same coverage, ~2.2 s |
| attr bit-exactness | regress canonical tier | digest (hashes id, geom type, sorted bit-exact attrs, geometry) |
| absorb legitimate reorder | regress canonical multiset hash | same machinery, reused inside the digest |
| refactor bit-identity gate | regress tol 0 / `cmp -s` | digest equality (semantic AND exhaustive); `cmp -s` remains for archive bytes |
| comparability enforcement | prose rule + brokkr resolution | `contract.json` compared before content; mismatch = refusal exit 2 |
| diff classification/attribution | regress detail tier vs blessed | `elivagar regress <a> --against <b>` + `--overlay`, two explicit paths |
| human adjudication of rotations | viewer session, unrecorded | `corpus bless --rotate` commit diff (leaves + manifest SVGs) |

Accepted loss, stated plainly: regress-vs-blessed gave instant attribution
against a curated archive. The digest detects and names tiles (leaves
mode); attribution needs a comparand archive - in practice the per-commit
archives in `data/tilegen/`, or a rebuild of the old commit. Detection is
the gate; attribution is a debugging workflow.

## Survey of the ground

Full inventory, re-run over source and docs 2026-07-15 (the R2 finding
that the old "8 files" grep was incomplete is addressed by this list).

**Survey-freshness note (re-checked 2026-07-15, post-review):** HEAD has
advanced to `cb65c2f` and the working tree is clean -
`notes/planet-30gb-roadmap.md` is no longer dirty. So the "420534e is HEAD
at survey time" and "planet-30gb-roadmap.md dirty in the working tree"
phrasings below are stale as to the tree state, though the file/line
inventory itself still holds. The Brick 0 archive must be resolved from
the current HEAD (`data/tilegen/denmark-cb65c2f.pmtiles`, or a fresh
`brokkr tilegen --dataset denmark --variant locations` build), and the
roadmap R-bricks no longer have uncommitted user changes to preserve -
they are ordinary edits. Line numbers throughout this survey key to the
pre-`cb65c2f` tree and will have drifted; the quoted before/after text in
each brick is the real anchor, so match on the text, not the line number.

### Config and data

- `brokkr.toml:63-66` - `[plantasjen.datasets.denmark.blessed]`
  (file/commit/xxhash). The only blessed entry; dm6 has none. DELETED.
- `brokkr.toml:29` - historical comment naming the 07-14 mis-bless inside
  the tilegen-contract rationale. SURVIVES (incident history).
- `data/blessed/` - 10 archives on disk (denmark x8, germany-64bdee1,
  norway-9e8dce2). Machine-local, not in git. See D4.
- `data/tilegen/` - per-commit archives (denmark at 420534e, a8c4f84,
  bc71cf1, bdf87fc, d8b5147 at survey time). The tier-3 comparand source;
  untouched.

### Documents

- `AGENTS.md:53-60` - the `brokkr regress`/`brokkr bless` command-list
  entries and "THE standing gate" designation. REWRITTEN (Landing 1,
  edit A1).
- `AGENTS.md:343-345` - pipeline ocean phase: "the blessed regress
  baseline is artifact-active ... rotating the artifact forces a bless
  rotation". REWRITTEN (A2).
- `AGENTS.md:361-366` - CLI-rule history ("a denmark archive was blessed
  as the regress baseline while the artifact was silently absent").
  SURVIVES (incident history, past tense).
- `AGENTS.md:403-404` - "Reporting is not enforcement: `brokkr regress`
  still does not read the block". REWRITTEN (A3).
- `AGENTS.md:419-444` - the "regress - invoke as `brokkr regress`, never
  the raw binary" section. REWRITTEN (A4), and a standing-gate section
  added (A5).
- `reference/cli.md:8` - wrapper list names `brokkr regress`,
  `brokkr bless`. REWRITTEN (C1).
- `reference/cli.md:31-34` - incident history. SURVIVES.
- `reference/cli.md:180-182` - ocean-build: "The blessed regress baseline
  must be rebuilt and re-blessed". REWRITTEN (C2).
- `reference/cli.md:233-235` - "Reporting is not enforcement." REWRITTEN
  (C3).
- `reference/cli.md:291-319` - the regress section, titled
  `elivagar regress <CURRENT> --against <BLESSED>`, with the "Invoke this
  as `brokkr regress`" paragraph. REWRITTEN (C4; metavar rename lands in
  Landing 2).
- `reference/cli.md:368-371` - corpus section: "advisory ... not a
  replacement for `brokkr regress` yet". REWRITTEN (C5).
- `reference/technical-implementation-spec.md:33-37` - the gate list's
  `brokkr regress` bullet. REWRITTEN (T1).
- `reference/technical-implementation-spec.md:53-57` - "the blessed
  regress references and scoreboard rows are locations-variant runs".
  REWRITTEN (T2).
- `reference/performance.md` - every `blessed`/`regress` mention is a
  dated historical record (the 07-14 regression narrative, the
  ec5bd11/re-bless note, gate readings at old landings). SURVIVE
  untouched; a new dated rotation entry is APPENDED (P1).
- `reference/metadata.md:58` - present tense: "`brokkr bless` records an
  xxhash over the whole archive". REWRITTEN (M1).
- `reference/metadata.md:269-274` - Known gaps: "No consumer ENFORCES
  this yet ... `brokkr regress` still does not read it". REWRITTEN (M2).
- `notes/planet-30gb-roadmap.md` (dirty in the working tree - edits must
  be surgical, preserving the user's uncommitted changes): lines 24-27
  standing-gate list; 29-35 companion note's "blessed baseline" phrasing;
  115-120 go/no-go item 3 (the re-bless item); 147-148 gate policy;
  150-153 "Bless immediately"; 282-287 artifact standing caveats; 388
  "re-bless territory"; 457-463 sequencing. REWRITTEN (R1-R7).
- `notes/virtual-planet-serving.md:18,130` - "blessed baseline",
  "`brokkr regress` against a blessed [archive]". REWRITTEN (V1).
- `notes/svg-corpus-plan.md` - originating note. Gets a one-line status
  update at the Spec C item (landed, date, commit). (N1)

### Source

- `src/main.rs:50` - clap doc comment on the `Corpus` subcommand: "Create
  or check an advisory semantic corpus digest." Live, user-visible
  `--help` text calling the digest "advisory" - the exact word C5 rewrites
  in cli.md and the exact framing this spec inverts (advisory -> standing
  gate). REWRITTEN (Landing 1, S5). Missed by the original survey; caught
  by review.
- `src/main.rs:272` - doc comment on `RegressArgs::against`: "Blessed
  PMTiles archive to compare against." (user-visible `--help` text).
  Landing 2.
- `src/inspect.rs:202` - live rustdoc on `print_provenance`: "Reporting is
  not enforcement: `brokkr regress` does not read the block, so these
  lines let a human refuse a comparison, they do not refuse it." Present
  tense, references the dead `brokkr regress`, user-visible rustdoc - the
  same claim A3/C3/M2 rewrite. REWRITTEN (Landing 1, S4). Missed by the
  original survey (which listed inspect.rs only at 1008); caught by review
  and would have tripped Landing 1 gate 5's `brokkr regress` grep as a
  non-survivor hit with no brick to explain it.
- `src/regress.rs` - the comparand is named `blessed` throughout the
  engine: `RegressTotals::only_in_blessed`, `counters.addressed_blessed`,
  the summary token `only_blessed=`, JSON keys `only_in_blessed` /
  `addressed_blessed`, the sidecar counter `regress_addressed_blessed`,
  `regress(current, blessed, cfg)`, `PairSpan::blessed`,
  `BlobPair::blessed`, `merge_runs`, `blessed_runs`,
  `blessed_fingerprints`, the `DetailDiffSink` method parameters, the
  pairing/classify functions (`pair_detail_features`,
  `classify_detail_geometry`, `component_structure_matches`, ...),
  `unpaired_blessed`, `remaining_blessed`, `residual_blessed`,
  `prev_blessed`. Landing 2.
- `src/regress/tests.rs` - fixtures and assertions using `blessed`
  naming. Landing 2.
- `src/corpus/overlay.rs` - tier 3 emitter fields/params named `blessed`
  (`OverlayEntry::blessed`, `matched(... blessed ...)`, ...). Landing 2.
- `src/pipeline/mod.rs:455-458` - comment: "the blessed baseline consumes
  the artifact ... same data/ocean-tiles.pmtiles the blessed archive was
  built with". Landing 1 (S1).
- `src/provenance.rs:22,44` - "blessing records a hash over the whole
  archive"; "bless reproducibility checks". Landing 1 (S2).
- `src/geometry/overlay/mod.rs:234` - "the class brokkr regress on
  denmark would only catch if". Landing 1 (S3).
- `src/inspect.rs:1008` - "The case that motivated this: a blessed
  baseline built before the ...". SURVIVES (historical motivation for a
  live guard; past tense, describes no live machinery).
- `src/corpus.rs` - `bless()`, `--rotate`, the "blessed N tiles" output
  line. SURVIVES ENTIRELY: this is the corpus's own bless verb, the new
  machinery. The teardown removes the *pmtiles archive* bless machinery,
  not the word.

### What already does not exist

Confirmed absent, so no brick removes them: elivagar has no code path
reading `datasets.<D>.blessed` (resolution lived in brokkr);
`dump_svg_examples` was deleted at the Spec B landing (replaced by
`--overlay`); no elivagar test or script reads `data/blessed/`.

## Decisions resolved inline

### D1. The gates after the teardown

Three modes, replacing every duty `brokkr regress` had. These exact
commands go into the doc rewrites below and are the standing definitions:

**Output-neutral landing (the standing gate).** Fresh denmark locations
build, then the digest check:

```
brokkr tilegen --dataset denmark --variant locations
elivagar corpus check data/tilegen/denmark-<commit>.pmtiles --corpus corpus/denmark
```

Substitute the landing commit's short hash; `brokkr tilegen` names its
output that way. Pass = exit 0 (digest byte-equal over every addressed
tile, attrs bit-exact, legitimate reorder absorbed). Exit 1 names the
changed zoom(s) and, in leaves mode, the exact changed tile runs. Exit 2
is a refusal (contract mismatch) and means the comparison itself is
invalid - never read it as pass or fail.

**Intended output change (rotation).** The check fails by design. The
landing adjudicates every changed tile - the `corpus/denmark/leaves` git
diff names each changed run; attribution when needed comes from
`elivagar regress <new> --against data/tilegen/denmark-<prev>.pmtiles
--overlay <dir>` against the previous commit's archive - and then rotates
the baseline in the same commit as the code:

```
elivagar corpus bless data/tilegen/denmark-<commit>.pmtiles --corpus corpus/denmark --rotate
```

The commit IS the bless; review of that diff (contract.json, digest,
leaves, manifest SVGs) is the human gate. `bless` refuses to overwrite
without `--rotate` and refuses non-locations archives.

**Explicit-tolerance geometry landing.** Where a spec states a tolerance
gate rather than zero-diff:

```
elivagar regress data/tilegen/denmark-<new>.pmtiles --against data/tilegen/denmark-<prev>.pmtiles --tol <N> --max-moved <M>
```

with the tolerance and the displacement-percentile verdict criteria
stated in that spec. This is the surviving two-archive engine; the caller
owns comparability and establishes it from the provenance blocks
(`brokkr pmtiles-inspect`) before reading the verdict.

`--tol <N>` alone is NOT enough and would defeat the gate: `--tol`
reclassifies a within-tolerance move from `structural_moved` to
`tolerance_moved`, but `RegressReport::passed` (`src/regress.rs`) still
requires `tolerance_moved <= cfg.max_moved`, and `max_moved` defaults to
0. So any tolerated movement fails the verdict unless the command also
carries an explicit `--max-moved <M>` budget. A tolerance gate therefore
states BOTH the geometry tolerance `<N>` and the count budget `<M>` (plus
the displacement-percentile criterion). (Caught by review: the original
template supplied only `--tol N`.)

The corpus gate is strictly stronger on comparability than the raw
two-archive engine it fronts for the standing role. Note the accurate
attribution: it is the *raw* `elivagar regress` binary that reads no
provenance block - it diffs whatever two archives it is handed, which is
the surface that produced the 07-14 locations-vs-raw false alarm (ran to
completion, reported 363,620 phantom structural moves). The *`brokkr
regress` wrapper* is not that surface: current brokkr
(`src/elivagar/regress.rs`, `gate_contract`) already reads both
provenance blocks and refuses on a contract mismatch before launching
elivagar - it is exactly what stops the 363,620-move class today. What
`corpus check` adds over the wrapper is that the comparand contract is
committed (`contract.json`), not a live sibling archive, so the gate
travels and survives rotation. `corpus check` compares that committed
`contract.json` - input identity, variant, full config contract, ocean
artifact key, style hash - before any content and refuses with the field
named.

### D2. Cross-repo sequencing - the R2 four-step order, revised

R2 pinned: (1) add compatible `brokkr corpus` wrappers alongside
`brokkr bless`/`brokkr regress`, (2) land and calibrate, (3) switch the
gate, (4) remove the old brokkr commands. Steps 2 is done. Step 1 is
revised away, with the reasoning recorded:

The wrapper-first ordering existed to guarantee "never a window with no
gate", on the assumption that a gate needs a brokkr wrapper to be
invocable with its enforcement intact. That assumption predates the
landed contract guard. The wrapper's two enforcement duties - a baseline
is always locations-generated, and comparisons never cross contracts -
are now enforced by the corpus machinery itself (`bless` refuses
non-locations archives; `check` refuses contract mismatches), and the
raw invocation takes an explicit archive path that `brokkr tilegen`
already produces. The gate is therefore fully invocable and fully
enforced with no wrapper. Landing 1 makes the switch atomic: the same
commit that deletes the blessed entry (killing `brokkr regress`
resolution) installs the corpus check as the documented standing gate.
Before that commit the old gate works; from that commit the new gate
works; no window.

The brokkr-repo task (named below, excluded from this spec) then removes
the dangling commands and adds the wrapper as ergonomics, not as
enforcement. Two hazards until it lands, both verified against installed
brokkr and neither closable from this repo:

- `brokkr bless` still exists and does NOT fail against the entry-less
  config - `src/elivagar/bless.rs` calls `write_blessed_entry`
  unconditionally, so it would recreate the deleted `blessed` table on the
  next invocation. This is why removing `brokkr bless` is the brokkr
  task's first item.
- `brokkr regress` fails loudly ONLY in its bare form (no `--against`),
  where it calls `resolve_blessed_path` and errors on the missing entry.
  `brokkr regress --against <path>` takes the explicit archive
  (`src/elivagar/cmd.rs`) and still runs - so "regress is disabled" is
  true only for the resolver path, not the whole command.

The mitigation this repo CAN apply: the AGENTS.md rewrite documents both
commands as removed-pending-brokkr so a session does not reach for them,
and Landing 1 gate 4 records the bare-`brokkr regress` loud failure as the
proof of the resolver window. It cannot make `brokkr bless` or `brokkr
regress --against` refuse; only the brokkr task can.

### D3. The `blessed` identifier rename: renamed to `baseline`

The R2-mandated decision. Inside the surviving two-archive engine,
`blessed` is a lie after this landing - there is no blessed archive,
only the archive the current one is compared against. Decision: rename
every identifier containing the token `blessed` in `src/regress.rs`,
`src/regress/tests.rs`, and `src/corpus/overlay.rs` to use `baseline`,
including the externally visible surfaces (JSON keys, summary tokens,
sidecar counter, `--help` text, cli.md metavar). Pre-1.0, breaking these
is legal. Rationale correction (review): the original text claimed the
rename must land after Landing 1 because `brokkr regress` parses the
JSON/summary output and that parsing "stops mattering" once the wrapper is
dead. That premise is false - installed brokkr
(`src/elivagar/regress.rs`) runs elivagar regress with
`run_passthrough_timed` and consumes only the process exit code, never the
JSON keys or summary tokens. So no machine consumer parses the renamed
fields, and the rename is output-safe regardless of ordering. The rename
still lands in Landing 2, but for the honest reason: to keep Landing 1 a
tight, reviewable, docs-and-config atomic gate-switch commit rather than
mixing a ~150-identifier mechanical rename into it. Ordering is a
hygiene choice here, not a correctness constraint.

Why `baseline` and not `comparand`: both are truthful; `baseline` names
the role the `--against` archive plays in the comparison (the thing
`current` is judged against), pairs naturally with `current` in ~150
identifiers and report fields, and carries no registry implication once
the registry is gone. `comparand` is reserved for prose where symmetry
matters.

Scope guard, stated twice deliberately: `corpus bless`, `--rotate`, the
`bless()` function in `src/corpus.rs`, and every "bless" in corpus
vocabulary are the NEW machinery and are not touched by this rename.
Short positional locals (`bl`, `bi`, `bl_used`, `bpos`) stay - they
abbreviate position in the pair, and churning them adds diff noise
without truth.

Mapping (exhaustive for externally visible names; internal identifiers
follow the same token substitution):

| before | after |
|---|---|
| `RegressTotals::only_in_blessed` + JSON key `only_in_blessed` | `only_in_baseline` |
| summary token `only_blessed=` | `only_baseline=` |
| `counters.addressed_blessed` + JSON key `addressed_blessed` | `addressed_baseline` |
| sidecar counter `regress_addressed_blessed` | `regress_addressed_baseline` |
| `regress(current, blessed, cfg)` | `regress(current, baseline, cfg)` |
| main.rs `--against` help "Blessed PMTiles archive to compare against." | "Baseline PMTiles archive to compare against." |
| cli.md `elivagar regress <CURRENT> --against <BLESSED>` | `--against <BASELINE>` |
| `DetailDiffSink` `blessed` parameters, `OverlayEntry::blessed`, all `*_blessed` / `blessed_*` identifiers | `baseline` token throughout |

### D4. `data/blessed/` physical files

The teardown removes every *reference*; it does not delete the archives.
They are state this work did not create, they are large, and two of them
retain diagnostic value (`denmark-d8b5147` was the last blessed baseline;
old blessed archives can serve as tier-3 comparands exactly like
`data/tilegen/` archives). Post-landing, `data/blessed/` is an
unreferenced directory of ordinary archives. Its deletion is a disk-space
decision for the user, recorded here as the one manual cleanup this spec
leaves behind: `data/blessed/` (10 files, denmark x8 plus
germany-64bdee1 and norway-9e8dce2) may be removed at will once the
landing is in.

### D5. What survives as history

Incident narratives stay in past tense wherever they explain a live rule:
`brokkr.toml:29`, `AGENTS.md:361-366`, `reference/cli.md:31-34`,
`reference/metadata.md:16-20`, all of `reference/performance.md`'s dated
entries, `src/inspect.rs:1008`. The verification greps below carry this
survivor list so "no stale references" has an exact meaning: every
remaining `bless`/`blessed` hit is either corpus vocabulary or one of
these historical records.

### D6. Gate cost and dataset policy carry over unchanged

Denmark-only for routine landings (user call of 2026-07-09) transfers to
the corpus gate verbatim; the check costs ~2.2 s on the 1.3M-tile
denmark archive (recorded), cheaper than the regress canonical tier it
replaces. Germany/norway corpus baselines remain optional-later exactly
as the plan states; nothing in this teardown creates them.

## Landing 1: gate rotation and teardown

One commit. Everything below lands together because the deletion of the
brokkr.toml entry and the doc rewires that install the corpus gate are
one atomic statement; splitting them opens the no-gate window this
ordering exists to prevent.

### Config

**B1 - brokkr.toml.** Delete lines 63-66, the
`[plantasjen.datasets.denmark.blessed]` table. Nothing else in the file
changes (the line-29 comment survives per D5).

### Source comments (the non-rename source surface)

**S1 - `src/pipeline/mod.rs:455-458`.** Rewrite the comment's final
clause. Current meaning: artifact output differs benignly, so the blessed
baseline is artifact-active and artifact rotation forces a bless
rotation. New meaning, same facts, corpus terms:

```
// equivalent, so extracts DO consume the artifact and the committed
// corpus baseline (corpus/<dataset>/) is artifact-active: its contract
// records the artifact key, so a gate run must carry the same
// data/ocean-tiles.pmtiles the baseline was captured from, and rotating
// the artifact forces a corpus rotation (corpus check refuses on the
// key until then).
```

(The replacement deliberately avoids the token "blessed": the artifact is
not blessed, the corpus is, and leaving "blessed" here would make
`src/pipeline/mod.rs` a `rg -n "blessed" src/` hit that Landing 2 gate 4
does not sanction - it lists only `src/inspect.rs` and `src/corpus.rs` as
survivors. "captured from" carries the same meaning without the token.)

**S2 - `src/provenance.rs:22,44`.** Line 22: "Same-commit builds are
byte-identical and blessing records a hash over the whole archive"
becomes "Same-commit builds are byte-identical and the corpus digest is
recomputed from archive content". Line 44: "for diagnosis and bless
reproducibility checks, never comparison gating" becomes "for diagnosis
and corpus-rotation review, never comparison gating".

**S3 - `src/geometry/overlay/mod.rs:234`.** "the class brokkr regress on
denmark would only catch if" becomes "the class the denmark corpus
digest would only catch if" (the sentence's claim - a state leak only
caught under a specific input - is unchanged).

**S4 - `src/inspect.rs:202`.** The live rustdoc on `print_provenance`,
same substance as A3/C3/M2. "Reporting is not enforcement: `brokkr
regress` does not read the block, so these lines let a human refuse a
comparison, they do not refuse it. See the consumer-contract gap in
reference/metadata.md." becomes: "Reporting is not enforcement for the
ad-hoc path: `elivagar regress` diffs whatever two archives it is handed
and reads nothing here, so for an ad-hoc comparison these lines let a
human refuse; nothing refuses for you. The standing gate is different -
`elivagar corpus check` compares the committed contract against this
block before reading content and refuses a mismatch. See
reference/metadata.md." (Do NOT reintroduce "blessed" in the replacement,
per the Landing 2 grep.)

**S5 - `src/main.rs:50`.** The clap doc comment "Create or check an
advisory semantic corpus digest." becomes "Create or check a semantic
corpus digest (the standing output gate)." This is user-visible `--help`
text; leaving "advisory" contradicts the whole spec (C5, A5, P1) which
promotes the digest from advisory to THE standing gate and rewrites the
identical word in cli.md. (Landing 1, not Landing 2: it is a plain wording
fix, not part of the `blessed` rename.)

### AGENTS.md

**A1 - command list (lines 51-60).** The Verification block loses the
`brokkr regress` and `brokkr bless` entries and gains the gate
definition:

```
# Verification
brokkr verify pmtiles [--dataset D] [--tiles VARIANT] [--geometry-stats]
# THE standing output gate: fresh denmark locations build + corpus
# digest check against the committed corpus/denmark/ baseline:
#   brokkr tilegen --dataset denmark --variant locations
#   elivagar corpus check data/tilegen/denmark-<commit>.pmtiles \
#       --corpus corpus/denmark
# Raw-binary spelling until the brokkr corpus wrapper lands; brokkr
# bless / brokkr regress are removed (see the corpus sections below).
```

**A2 - pipeline ocean paragraph (lines 343-345).** "- so the blessed
regress baseline is artifact-active, the gate machine must pass the same
artifact, and rotating the artifact forces a bless rotation." becomes
"- so the committed corpus baseline is artifact-active (its contract
records the artifact key), the gate machine must pass the same artifact,
and rotating the artifact forces a corpus rotation: `corpus check`
refuses on the key until the baseline is re-blessed with `--rotate`."

**A3 - inspect section (lines 403-404).** "Reporting is not enforcement:
`brokkr regress` still does not read the block, so these lines let you
refuse a comparison, they do not refuse it." becomes "For the standing
gate, reporting became enforcement: `corpus check` compares the
committed contract against this block before reading content and refuses
a mismatch with the field named. `elivagar regress` reads nothing here
by design - it diffs whatever two archives it is handed - so for ad-hoc
comparisons these lines let you refuse; nothing refuses for you."

**A4 - the regress section (lines 419-444).** Retitled
"### `elivagar regress <CURRENT> --against <BASELINE>` - the two-archive
semantic diff". Content changes:

- Drop the "invoke as `brokkr regress`, never the raw binary" framing
  and the brokkr.toml resolution paragraph entirely.
- Open with: it takes two explicit archive paths, no registry, and it is
  the attribution instrument (`--overlay` renders per-tile diff SVGs),
  not the standing gate - `corpus check` is (section above). The natural
  comparand source is `data/tilegen/<dataset>-<commit>.pmtiles`.
- Keep verbatim the canonical-form paragraph (what is decoded, the total
  within-run order, what the canonicalization absorbs) and the
  classification list - that machinery is unchanged.
- Keep the variant lesson, restated for the new shape: comparing a
  locations archive against a raw build reports a six-figure structural
  diff for two correct builds (2026-07-09 and 2026-07-14 both burned on
  this). The corpus gate refuses that comparison mechanically via its
  contract; regress by design does not, so establish comparability from
  the provenance blocks (`brokkr pmtiles-inspect`) before reading a
  verdict.

(The `<BASELINE>` metavar itself flips in Landing 2 with the code; A4's
prose is written against the new name from the start - one commit apart
in the same session, and docs describing an argument by role rather than
flag spelling are correct across both.)

**A5 - new standing-gate section.** Inserted before the regress section,
titled "### The standing gate: `elivagar corpus check`". Contents: the
three D1 modes with their exact commands; exit semantics (0 pass, 1
content diff naming tiles, 2 contract refusal - never read 2 as a
verdict); the rotation discipline (bless + commit = the review, refusal
without `--rotate`); denmark-only routine policy; the pointer to
`reference/cli.md` for the full corpus surface and to
`reference/performance.md` for the calibration record. Also state the
brokkr wrapper status: `brokkr bless`/`brokkr regress` are removed from
the workflow as of this landing and error against current brokkr.toml;
their brokkr-side removal and a `brokkr corpus` wrapper are a named
brokkr-repo task.

### reference/cli.md

**C1 - line 8.** "`brokkr` wraps the measured paths (`brokkr tilegen`,
`brokkr regress`, `brokkr bless`, `brokkr pmtiles-inspect`)" becomes
"`brokkr` wraps the measured paths (`brokkr tilegen`,
`brokkr pmtiles-inspect`)".

**C2 - ocean-build rotation paragraph (lines 180-182).** Becomes:
"Rotating the artifact is an output-changing event. The corpus contract
records the artifact key, so the next `corpus check` refuses with the
key mismatch named until the corpus is re-blessed (`corpus bless
--rotate`) from a build carrying the new artifact - and that rotation
commit is the review."

**C3 - inspect provenance paragraph (lines 233-235).** Same substance as
A3: `corpus check` enforces the contract for the standing gate;
`elivagar regress` does not read the block; the consumer-contract gap in
`reference/metadata.md` is updated accordingly (M2).

**C4 - the regress section (lines 291-319).** Retitle to
`elivagar regress <CURRENT> --against <BASELINE>` (metavar text lands
with Landing 2; the section rewrite lands here). Replace the "Invoke
this as `brokkr regress`" paragraph with the D1 framing: explicit
two-archive diff and tier-3 attribution engine; comparability is the
caller's, established from provenance; comparand source
`data/tilegen/<dataset>-<commit>.pmtiles`; the variant false-alarm
history retained as the motivating case. Flag table unchanged apart from
the metavar and the `--against` description ("baseline archive to
compare against (required)").

**C5 - the corpus section opener (lines 368-371).** "The advisory corpus
digest ... It is not a replacement for `brokkr regress` yet." becomes
"**The standing output gate.** The corpus digest checks the semantic MVT
content of every addressed tile in an explicit archive against the
committed baseline. Calibrated both directions 2026-07-15
(`reference/performance.md`); it replaced the pmtiles bless machinery
(`brokkr bless`/`brokkr regress` and the blessed-archive registry) when
the standing gate rotated." Follow with the exit-code semantics already
documented, unchanged.

### reference/technical-implementation-spec.md

**T1 - the gate-list bullet (lines 33-37).** Replace the `brokkr
regress` clause with:

"`elivagar corpus check data/tilegen/denmark-<commit>.pmtiles --corpus
corpus/denmark` on a fresh `brokkr tilegen --dataset denmark --variant
locations` build for changes intended to be output-neutral (pass = exit
0, digest byte-equal over every tile; exit 2 is a contract refusal, not
a verdict); intended output changes instead adjudicate the named tiles
(the `corpus/denmark/leaves` git diff, plus `elivagar regress --against
<previous archive> --overlay <dir>` for attribution) and land with the
corpus rotation (`corpus bless ... --rotate`) in the same commit;
explicit-tolerance geometry landings gate on `elivagar regress <current>
--against <prior> --tol N --max-moved M` with the tolerance, the
tolerated-move count budget, and the displacement-percentile criteria
stated in the spec (the `--max-moved M` budget is mandatory, not
optional: `passed()` requires `tolerance_moved <= max_moved` and
`max_moved` defaults to 0, so `--tol N` alone fails on any tolerated
move - see D1)".

**T2 - the variant passage (lines 53-57).** Replace the blessed-specific
sentences: the corpus baseline is a locations-variant build (`corpus
bless` refuses anything else) and its contract records the input
identity, so a cross-variant `corpus check` refuses loudly instead of
producing the 2026-07-09 false alarm; an `elivagar regress` read across
variants remains that false alarm, because regress reads no contract -
which is why its gate commands must still pin `--variant` explicitly.

### reference/performance.md

**P1 - append a dated entry** (history above it untouched):

"## Standing gate rotation (2026-07-15, spec C teardown)" recording:
`brokkr bless`/`brokkr regress` and `datasets.denmark.blessed` removed;
`elivagar corpus check` promoted from advisory to THE standing output
gate; entry conditions were the two calibration sections above (digest
gate at `a8c4f84`, render core at `41a953a`); gate cost ~2.2 s on
denmark. Note that the 07-15 "re-bless pending user decision" thread
closes here: the committed corpus baseline supersedes re-blessing.

Commit-hash mechanics (review): a commit cannot contain its own final
hash, so P1 does NOT record "the landing commit hash" inside the Landing 1
commit. Write the entry with a symbolic reference ("this landing", "the
spec-C teardown commit") and, if a literal hash is wanted, add it in a
tiny follow-up `git commit --amend`-free note commit after Landing 1
lands - never as a self-referential field. The same chicken-and-egg
applies to the rotation workflow in D1: `corpus bless --rotate` writes
digest/contract/manifest files, so those files are part of the commit and
change its hash; the rotation commit therefore also cannot embed its own
hash, and any "archive built at commit X" provenance references the
PARENT/code commit or is recorded post-hoc, not the rotation commit's own
hash.

### reference/metadata.md

**M1 - line 58.** "Same-commit builds are byte-identical (AGENTS.md) and
`brokkr bless` records an xxhash over the whole archive; a per-run value
here would break both." becomes "Same-commit builds are byte-identical
(AGENTS.md) and the corpus digest and contract are recomputed from
archive content; a per-run value here would break both."

**M2 - Known gaps, first bullet.** Becomes: "**Enforcement landed for
the standing gate only.** `elivagar corpus check` compares the committed
`contract.json` against this block before reading content and refuses
with the differing field named - step 1 of the consumer contract,
enforced and calibrated (the stale-artifact archive refuses with
`contract mismatch: config.ocean.artifact_key.policy_version`).
`elivagar regress` still reads nothing here: an ad-hoc two-archive diff
can be run across incomparable archives, so establish comparability with
`elivagar inspect` first."

### Roadmap and companion notes

**R1 - `notes/planet-30gb-roadmap.md:24-27`.** Standing gates list:
"`elivagar verify`, the earcut oracle
(`scripts/validate/earcut-oracle.mjs`), and `elivagar corpus check`
against the committed `corpus/denmark/` baseline."

**R2 - lines 29-35.** "the blessed baseline the serve path verifies
against" becomes "the committed corpus baseline the serve path verifies
against".

**R3 - go/no-go item 3 (lines 115-120).** Becomes: "3. RESOLVED by the
corpus gate (spec C): the committed `corpus/denmark/` baseline is in git
and gateable at any commit - the un-gateable-against-blessed window is
structurally gone. A planet run wants a green `corpus check` at the
attempt commit before it starts. Planet-scale corpus blessing uses
bucket mode (`--mode buckets`); that bless is a decision at planet-bless
time, and bucket-mode rotations hand per-tile attribution to the tier-3
overlay emitter (the reviewability hole the corpus plan records as open
for bucket mode)."

**R4 - gate policy (lines 147-148).** "regress runs on DENMARK ONLY for
routine landings; germany/NA regress only at blessing rotations" becomes
"the corpus check runs on DENMARK ONLY for routine landings;
germany/NA archives get checked only at corpus rotations (their corpora
are optional and currently absent)."

**R5 - "Bless immediately" (lines 150-153).** Becomes "**Rotate the
corpus in the landing commit** for any accepted output-changing landing:
`corpus bless --rotate` rewrites digest + contract + manifest and the
commit diff is the review. The baseline lives in git, so it survives
archive rotation by construction - the 07-09 grind against a
rotated-away de-facto baseline is no longer expressible."

**R6 - artifact caveats (lines 282-287) and line 388.** Same rewrite as
A2/S1 (contract records the key; rotation forces corpus rotation);
"re-bless territory" becomes "corpus-rotation territory".

**R7 - sequencing (lines 457-463).** The third gating item ("the
re-bless that makes `brokkr regress` gateable again") becomes "a green
`corpus check` at the attempt commit (the re-bless item closed with the
spec C teardown - the baseline is committed)".

**V1 - `notes/virtual-planet-serving.md:18,130`.** Line 18: "the
committed corpus baseline that the serve path is verified against".
Line 130: the on-demand-vs-batch equality check names its instruments
explicitly: "`elivagar corpus check` for exhaustive semantic equality
against the committed baseline, or `elivagar regress --against <batch
archive>` for attribution".

**N1 - `notes/svg-corpus-plan.md`.** At the Spec C item: append
"**Landed** (spec: `notes/spec-c-bless-teardown.md`; see the landing
commit for the record)."

### Landing 1 verification

Exact commands, in order. Binary/placeholder resolution (review): the
`elivagar` binary is not on `PATH` in this checkout - it is
`./target/release/elivagar` (the tilegen-built one, as Brick 0 states);
spell that path when running these by hand. `<commit>` is the current
HEAD short hash that `brokkr tilegen` stamps into the output name (at
survey re-check that is `cb65c2f`), so `data/tilegen/denmark-<commit>.pmtiles`
resolves to the archive step 2 just produced.

1. `brokkr check` - clippy + full test suite green (comments and config
   only, but the boundary rule holds regardless).
2. `brokkr tilegen --dataset denmark --variant locations` - fresh build
   at the landing commit.
3. `./target/release/elivagar corpus check
   data/tilegen/denmark-<commit>.pmtiles --corpus corpus/denmark` - exit 0.
   This is the new standing gate exercised at its first boundary; it
   doubles as the output-neutrality proof for the whole landing.
4. `brokkr regress` (bare, no `--against`) - must now FAIL LOUDLY at
   resolution (nonzero exit, message naming the missing
   `datasets.denmark.blessed` entry), never silently pass or fall back.
   This is the resolver path only: `brokkr regress --against <path>` still
   runs (explicit archive, no entry needed) and `brokkr bless` still
   recreates the entry - neither is closable here (D2). This reading is
   recorded in the landing notes: it is the proof there is no silent-pass
   window on the RESOLVER between this landing and the brokkr-repo removal,
   not a proof the whole command is disabled.
5. Inventory grep, against the D5 survivor list:
   `rg -in "brokkr regress|brokkr bless|datasets\..*\.blessed|data/blessed"`
   over `src/`, `reference/`, `AGENTS.md`, `CLAUDE.md`, `brokkr.toml`,
   `notes/planet-30gb-roadmap.md`, `notes/virtual-planet-serving.md`.
   Every hit must be a D5 survivor (dated incident history) or this
   spec/the plan describing the teardown itself.

## Landing 2: the `baseline` rename

One commit, immediately after Landing 1 (ordering rationale in D3: not
because any machine consumer parses the renamed fields - none does, brokkr
propagates only the exit code - but to keep Landing 1 a tight
docs-and-config gate-switch commit and land the ~150-identifier mechanical
rename on its own).

**B2 - the mechanical rename** per the D3 mapping: every identifier
containing the token `blessed` in `src/regress.rs`,
`src/regress/tests.rs`, `src/corpus/overlay.rs` renamed with
`blessed -> baseline`; the JSON keys, summary token, and sidecar counter
renamed with them; `src/main.rs:272` help text updated;
`reference/cli.md` regress-section metavar and `--against` row updated
(`<BLESSED>` -> `<BASELINE>`). `src/corpus.rs` and all corpus-bless
vocabulary untouched. Short positional locals (`bl`, `bi`, `bl_used`,
`bpos`) untouched.

### Landing 2 verification

1. `brokkr check` - the full suite compiles and passes under the new
   names (regress/tests.rs exercises the renamed engine end to end).
2. `brokkr tilegen --dataset denmark --variant locations` then
   `elivagar corpus check data/tilegen/denmark-<commit>.pmtiles --corpus
   corpus/denmark` - exit 0: the rename is output-neutral, proven by the
   standing gate itself.
3. `elivagar regress data/tilegen/denmark-<commit>.pmtiles --against
   data/tilegen/denmark-a8c4f84.pmtiles --json` - runs to completion and
   the JSON carries `only_in_baseline` / `addressed_baseline`; a smoke
   read that the renamed reporting surface is coherent (the diff verdict
   itself is whatever the archives say; this reading checks names, not
   content).
4. Closing grep: `rg -n "blessed" src/` returns exactly the sanctioned
   survivors - `src/inspect.rs` (historical comment) and `src/corpus.rs`
   corpus-bless vocabulary - and nothing in `src/regress.rs`,
   `src/regress/tests.rs`, or `src/corpus/overlay.rs`.

## The brokkr-repo task (named and excluded)

Genuinely separate work in the brokkr repository, not deferral: this
repo's landings are complete and gated without it. Its required
contents, so the task is buildable when picked up:

1. Remove `brokkr bless` - first, because it is the one command that can
   silently re-create a `blessed` entry in brokkr.toml (the D2 hazard).
2. Remove `brokkr regress` (the blessed-resolving wrapper).
3. Audit `brokkr suite elivagar` and any other composite command for
   embedded regress/bless steps; remove or rewire them.
4. Add `brokkr corpus check [--dataset D]` as ergonomics: resolve the
   current per-commit archive and `corpus/<dataset>/`, invoke
   `elivagar corpus check`, record the invocation in history.db like
   every brokkr command. Enforcement stays in elivagar (contract guard);
   the wrapper adds resolution and provenance recording only.
5. Remove the `blessed` field from brokkr's brokkr.toml schema. Note the
   subtlety (review): `Dataset` in `src/config_parts/schema.rs` carries
   `#[serde(deny_unknown_fields)]`, so simply deleting the `blessed:
   Option<BlessedEntry>` field does NOT make a stray `[*.datasets.*.blessed]`
   table "ignored" - it makes it a hard parse error that breaks every
   brokkr invocation against a brokkr.toml that still has the table. So
   this item must EITHER keep a deprecated, ignored field (e.g. rename to
   `_blessed` / `#[serde(skip)]` or a catch-all) so old tables parse
   harmlessly, OR sequence the field removal strictly after the brokkr.toml
   `blessed` table is deleted (Landing 1 B1 deletes denmark's, but other
   hosts/datasets may carry their own). "At worst ignored" is only
   achievable with the deprecated-field route under `deny_unknown_fields`.

Until it lands, the installed brokkr's bare `brokkr regress` fails loudly
against the entry-less brokkr.toml (verified as Landing 1 gate 4), but
`brokkr regress --against <path>` and `brokkr bless` do NOT fail (D2) -
`bless` will recreate the entry. AGENTS.md documents both commands as
removed-from-workflow so a session does not reach for them; that is the
only closure this repo can apply until the brokkr task lands.

## Stopping rule and out of scope

- **brokkr-repo changes**: named above, excluded (separate repo).
- **germany/norway corpus baselines**: optional-later per the plan; no
  brick here creates one.
- **The bucket-mode rotation reviewability hole** (planet scale): open
  finding recorded in `notes/svg-corpus-plan.md`; R3 restates it where
  the roadmap points at bucket mode. Closing it is future corpus work,
  not teardown work.
- **`scripts/ocean-coverage.sh` and the `ocean-coverage` subcommand**:
  their broken/triage status is a standing separate decision; untouched.
- **`src/regress.rs` engine behavior**: no semantic change anywhere in
  this spec - Landing 2 renames, it does not modify pairing,
  classification, or thresholds.
- **Physical deletion of `data/blessed/`**: user decision (D4).
- **`elivagar regress` CLI shape**: `--against` stays required and
  positional-plus-flag stays as is; only its metavar and help text
  change. No new resolution or registry of any kind is added - that
  would rebuild what this spec removes.

## Performance statement

This spec is off every measured path: it touches documentation,
brokkr.toml, source comments, and identifier names. No benchmark is
owed. Neutrality is confirmed by the gates named per landing: `brokkr
check` green and the denmark corpus check exit 0 at both boundaries
(digest byte-equality is a stronger neutrality statement than a bench
delta). The one measured quantity this spec changes is the standing
gate's own cost, already recorded: ~2.2 s per denmark check
(`reference/performance.md`, digest gate calibration), replacing the
regress canonical tier at comparable cost with strictly wider
enforcement.

## Review findings folded (2026-07-15)

Two reviews (`notes/spec-c-bless-teardown-R1.md`, opus;
`notes/spec-c-bless-teardown-R2.md`, codex gpt-5.6-sol xhigh) were each
validated against the source before folding. Every valid finding is now
in-place above; this section is the audit trail.

### Folded (validated true)

- **Missed source: `src/inspect.rs:202`** (R1 gap #1 = R2 #6). Live
  present-tense rustdoc referencing the dead `brokkr regress`; verified at
  the cited line. Added to the source inventory and given brick S4;
  would otherwise have tripped Landing 1 gate 5's grep with no brick.
- **Missed source: `src/main.rs:50`** (R1 gap #2). Clap `--help` text
  calls the digest "advisory", contradicting the spec's advisory ->
  standing-gate thesis; verified. Added to inventory and given brick S5.
- **S1 reintroduces "blessed"** (R1 smell #3). Verified against Landing 2
  gate 4's `rg -n "blessed" src/` survivor list (inspect.rs + corpus.rs
  only). S1 reworded to "captured from"; note added.
- **brokkr survey factually wrong** (R2 #2). Verified: installed brokkr
  `gate_contract` (`src/elivagar/regress.rs`) DOES read both provenance
  blocks and refuse on mismatch, and it consumes only the regress exit
  code via `run_passthrough_timed`, never JSON/summary fields. Fixed the
  D1 comparability paragraph (attributed no-provenance-read to the raw
  `elivagar regress`, not the `brokkr regress` wrapper) and the D3
  ordering rationale (rename is safe regardless of ordering; Landing 2 is
  a hygiene choice).
- **Landing 1 does not fully remove the bless machinery** (R2 #1, R1 smell
  #4). Verified: `brokkr bless` (`bless.rs`) calls `write_blessed_entry`
  unconditionally and recreates the deleted table; `brokkr regress
  --against <path>` (`cmd.rs`) bypasses the resolver and still runs. Only
  bare `brokkr regress` fails loudly. Corrected D2, Landing 1 gate 4, and
  the brokkr-task closing paragraph so the "fail loudly" claim is scoped
  to the resolver path.
- **Tolerance gate needs `--max-moved`** (R2 #4). Verified:
  `RegressReport::passed` requires `tolerance_moved <= max_moved` and
  `max_moved` defaults to 0, so `--tol N` alone fails any tolerated move.
  Added `--max-moved M` to the D1 and T1 tolerance commands with the
  reasoning.
- **P1 self-hash impossibility + rotation hash hole** (R2 #5). A commit
  cannot embed its own hash; the rotation commit includes corpus files so
  it has the same problem. Added the commit-hash-mechanics note to P1.
- **Gate commands not copy-pasteable re binary path** (R2 #3, partial -
  see rejected). Verified `elivagar` is not on PATH here. Added the
  binary-path/placeholder resolution note to the Landing 1 verification
  block and spelled `./target/release/elivagar` in the gate command.
- **brokkr task item 5 not buildable** (R2 #7). Verified: `Dataset` has
  `#[serde(deny_unknown_fields)]`, so dropping the `blessed` field makes a
  stray table a parse error, not "ignored". Rewrote item 5 with the
  deprecated-field / ordering options.
- **Stale ground survey** (R2 #8). Verified: HEAD is `cb65c2f` and the
  tree is clean (roadmap no longer dirty). Added the survey-freshness note
  and updated Brick 0's archive resolution.
- **Line-number reliance** (R1 nit #5). Valid nit; the survey-freshness
  note now instructs matching on quoted before/after text rather than line
  numbers, which is the real anchor. No per-brick line-number rewrite -
  the drift is inherent to a two-landing plan and the text anchors carry
  the load.

### Rejected or scoped down

- **R2 #3, the placeholder half.** The complaint that `<commit>`,
  `<new>`, `<prev>` violate the "exact command" rule is rejected: those
  are structurally necessary variables (the commit is not known until the
  build runs; the two comparand archives are arbitrary), consistent with
  the repo-wide convention of `<uuid>` / `<commit>` in AGENTS.md and
  cli.md, and the tech-spec rule targets vagueness ("run the relevant
  tests"), not parameterized paths. Only the binary-PATH half was a real
  defect and is folded. The `elivagar` spelling inside the DOC rewrites
  (AGENTS.md / cli.md prose) is also left as-is: those docs refer to the
  binary conceptually throughout (`elivagar run ...`), and that is the
  house style, not a copy-paste target.

Nothing else was rejected; the two reports did not conflict with each
other on any point (R1 #1 and R2 #6 are the same finding, folded once;
R1 smell #4 and R2 #1 overlap and are folded together).
