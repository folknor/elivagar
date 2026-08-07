# Wikidata Multilingual Name Enrichment

Investigated 2026-03-06. Status 2026-08-07: unimplemented, still a
plausible future feature; the Planetiler-mechanics research below remains
sound. If picked up, the fetch phase belongs in pbfhogg enrichment
territory (the injected-prepass pattern), not necessarily in elivagar -
decide at spec time.

## Problem

Elivagar emits exactly 3 name attributes: `name`, `name_en`, `name_de` (hardcoded in
`wire_format.rs`). Many OSM features only have the local-language `name` tag. Features
with `wikidata=Q*` tags link to Wikidata entities that carry names in dozens of
languages, but this data is unused.

~4.17M OSM features carry `wikidata=Q*` tags (taginfo). Planetiler's planet run
found 1.39M unique QIDs after deduplication.

## How Planetiler does it

Source: `planetiler-core/.../util/Wikidata.java`, `Translations.java`

### Two-phase architecture

**Phase 1: Fetch (`--fetch-wikidata`).** Scans PBF for all `wikidata=Q*` tags,
deduplicates QIDs, batches in groups of 5,000, sends SPARQL queries to
`https://query.wikidata.org/bigdata/namespace/wdq/sparql`:

```sparql
SELECT ?id ?label where {
  VALUES ?id { wd:Q1 wd:Q2 ... } ?id (owl:sameAs* / rdfs:label) ?label
}
```

Retrieves labels in ALL languages (no filtering at fetch time). Results written to
`wikidata_names.json` as newline-delimited JSON:
```json
["1",{"en":"English Name","de":"Deutscher Name"},1709654321000]
```
Format: `[QID_string, {lang: name, ...}, update_timestamp_ms]`

**Phase 2: Use (`--use-wikidata`, default true).** Loads `wikidata_names.json` into
an in-memory `LongObjectMap<Map<String, String>>` (QID -> language -> name). Registered
as a `TranslationProvider` fallback. During tile generation, features with a `wikidata`
tag get matching `name:lang` attributes added.

### Planet-scale numbers (v0.1.0)

- 1,390,005 unique QIDs fetched (~278 batches of 5,000)
- `wikidata_names.json`: ~291 MB
- Fetch phase: 553 seconds (9.2 minutes), 3,825s CPU time
- 4.6% of total 12,064s planet run

### Language filtering

Happens at output time, not fetch time. Default set: `en, ru, ar, zh, ja, ko, fr, de,
fi, pl, es, be, br, he` (14 languages). Configurable via `--languages` (supports `*`
for all, `-lang` for exclusions).

### Known issues

- **#1290** (2025-07): Wikidata fetch worker crash. The SPARQL endpoint is the single
  point of failure -- rate-limits, timeouts, intermittent errors. Retry logic exists
  (added in #113/#115) but endpoint remains unreliable at planet scale.
- **#679** (2023-10, open): Wikidata "labels" (rdfs:label) are UI display names that
  sometimes include disambiguators (e.g. "Washington, D.C." instead of "Washington").
  The Wikidata name property P2561 would be more appropriate for cartographic labels.
  Not yet implemented.

### Configuration flags

- `--fetch-wikidata` / `--only-fetch-wikidata` -- trigger download
- `--use-wikidata` (default true) -- use cached translations
- `--wikidata-cache` -- path to cache file
- `--wikidata-max-age` -- re-fetch entries older than this
- `--wikidata-update-limit` -- limit stale entry refresh per run
- `--languages` -- filter output languages

## Tilemaker and Tippecanoe

Neither has Wikidata name enrichment. Tilemaker's only `wikidata` reference is a
hardcoded Q192770 check to skip the Caspian Sea. Tippecanoe is a tile encoder with
no OSM awareness.

## Wikidata data scale

- ~120M+ Q-items total
- Full JSON dump: ~100 GB compressed (bz2), ~500+ GB decompressed
- Each entity has: `type`, `id`, `labels`, `descriptions`, `aliases`, `claims`, `sitelinks`
- Labels structure: `"labels": {"en": {"language": "en", "value": "Berlin"}, ...}`
- No official labels-only dump exists
- Estimated names-only extract for OSM: 1.4M QIDs x ~30 languages x ~20 bytes/name
  = ~840 MB uncompressed, ~100-200 MB compressed (Planetiler's 291 MB confirms this)

## Design options

### Option A: Offline pre-join in pbfhogg

Enrich PBF with Wikidata names before tile generation.

- Pros: elivagar unchanged; names are regular OSM tags; reusable
- Cons: inflates PBF significantly (adding ~30 name tags to 4M features); every
  PBF rebuild requires enrichment step; adds a pipeline stage
- Memory: ~200-300 MB for lookup table

### Option B: Runtime join in elivagar

Load names lookup table at startup, join during PBF processing.

- Pros: no PBF modification; clean separation; only pays for emitted languages;
  language filtering at tile-gen time
- Cons: adds memory; needs shared read-only map for parallel processing;
  lookup file must be distributed alongside PBF
- Memory: ~200-300 MB (fits easily in 30 GB)
- Implementation: `HashMap<u64, Vec<(LangId, String)>>` loaded at startup.
  During `name_attrs()`, check for `wikidata` tag, look up QID, add `name:*`
  for configured languages where OSM doesn't already have them

### Option C: External preprocessor

Standalone tool producing a compact binary lookup file.

- **C1: SPARQL-based** (like Planetiler) -- scan PBF for QIDs, batch-query SPARQL.
  Same reliability issues as Planetiler.
- **C2: Dump-based** -- download `latest-all.json.bz2`, stream it, extract labels
  for QIDs present in OSM. Reliable but requires ~100 GB download.

## Recommended approach

**C2 (dump-based preprocessor) + B (runtime join in elivagar).**

Rationale:
1. SPARQL endpoint is a reliability liability (Planetiler #1290)
2. Wikidata dump processing is a one-time cost per weekly release
3. Runtime join is architecturally clean (lookup file is just another input, like ocean)
4. QID set can be pre-filtered by scanning PBF first

## Implementation sketch

### Preprocessor (new tool, possibly in pbfhogg)

1. Scan PBF, collect all unique `wikidata=Q*` values -> QID set
2. Stream `latest-all.json.bz2`, for each entity:
   - If QID in set, extract labels for configured languages
   - Write to compact binary lookup file (e.g. `wikidata_names.bin`)
3. Format: sorted by QID, binary searchable or hash-map loadable

### Elivagar integration

1. New CLI flag: `--wikidata-names path` (optional, like `--ocean`)
2. Load lookup file at startup into shared `HashMap<u64, SmallVec<[(u8, String)]>>`
3. During PBF processing, when emitting name attributes:
   - If feature has `wikidata` tag and QID exists in lookup
   - For each configured language: if OSM doesn't have `name:lang`, use Wikidata label
   - OSM tags always take precedence (consistent with Planetiler's `putIfAbsent`)
4. Wire format: need to extend `KEY_NAMES` beyond the current 3 keys, or switch to
   dynamic key encoding for name attributes

### Language strategy

Start with Planetiler's 14-language default. Make configurable via `--languages`.
The Shortbread spec doesn't mandate specific languages.

### Open questions

1. Lookup file format: JSON-lines (human-readable, ~291 MB) vs binary (fast loading)
2. Whether to use rdfs:label or P2561 name statements (Planetiler #679 still open)
3. Wire format key encoding for variable language sets
4. Whether to make the preprocessor a pbfhogg subcommand or standalone tool

## Status

Research complete. Not a current priority - requires wire format changes and a new
external data dependency. Worth doing before planet-scale release for label coverage.
