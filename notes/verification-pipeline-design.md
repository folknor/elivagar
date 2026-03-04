# Verification Pipeline Design

## Status

Implementation-ready, lean plan.

## Why This Exists

We want a release-confidence check for generated PMTiles output.
This is not a new framework. It is a concrete verifier command that validates current output contracts.

## Scope

`elivagar verify <FILE.pmtiles>` will:

1. Validate PMTiles header and section bounds.
2. Parse metadata JSON and validate required schema fields.
3. Traverse all addressed tiles (no sampling initially).
4. Decompress tile payloads (current output: gzip).
5. Parse MVT payload structure.
6. Enforce core invariants:
- archive is readable end-to-end
- declared schema matches observed tiles
- expected layer set is present

## Design Decisions (Closed)

1. Reuse existing code, do not rebuild from scratch.
- Promote/adapt reader logic from `tests/pmtiles_roundtrip.rs`.
- Reuse metadata/header parsing patterns from `src/inspect.rs`.

2. No plugin architecture yet.
- No `CodecDecoder`/`PayloadVerifier` traits in v1.
- Single concrete implementation for current production path: PMTiles + gzip + MVT.

3. No profiles in v1.
- One strict mode of operation.
- Split modes only if real usage shows need.

4. No sampling in v1.
- Verify all tiles.
- If runtime becomes an issue, add sampling later based on measurements.

5. No JSON report schema in v1.
- Human-readable CLI output + exit code.
- Add `--json` only when there is a concrete consumer.

6. Keep verification separate from generation.
- Add `verify` subcommand only.
- Do not add `run --verify` now.

## Concrete Failure Modes This Targets

This verifier is meant to catch regressions in:

1. PMTiles container correctness
- invalid offsets/lengths
- broken directory encoding/decoding
- tile addressability mismatches

2. Metadata correctness
- invalid JSON
- missing/invalid `format`, `vector_layers`, schema info

3. Compression/payload readability
- tiles that fail gzip decompression
- tiles that fail MVT structural parsing

4. Schema drift in generated output
- missing expected layers
- mismatch between declared and observed layers

## File-by-File Implementation Plan

Target: 2-3 implementation files, minimal structural overhead.

### 1) `src/verify.rs` (new)

Single entry module for v1.

Responsibilities:
- `pub fn verify(path: &Path) -> Result<(), VerifyError>`
- PMTiles open + header checks
- metadata checks
- full tile traversal
- gzip decompress + MVT structural checks
- aggregate failures and print summary

Types in this file:
- `VerifyError`
- lightweight `VerifyStats`
- minimal helper structs for tile entries/context as needed

### 2) `src/main.rs` (edit)

Responsibilities:
- Add CLI subcommand:
  - `Verify { file: PathBuf }`
- Dispatch to `elivagar::verify::verify(&file)`
- non-zero exit on verification failure

### 3) `src/lib.rs` (edit)

Responsibilities:
- Export new module: `pub mod verify;`

### Optional 4) `src/pmtiles_reader.rs` (new, only if needed)

Create only if `src/verify.rs` gets too large.

Responsibilities:
- Shared PMTiles read helpers migrated from `tests/pmtiles_roundtrip.rs`.
- Keep API minimal and internal.

## Reuse Plan (Explicit)

### From `tests/pmtiles_roundtrip.rs`
- PMTiles directory decode and expansion
- tile read helpers
- gzip decompress helper
- minimal MVT layer decode primitives

Action:
- Move shared logic into `src/verify.rs` (or `src/pmtiles_reader.rs`), then update integration tests to use shared helpers where practical.

### From `src/inspect.rs`
- header field decoding helpers
- metadata handling patterns

Action:
- Reuse logic directly where possible; avoid duplicate parsing code paths.

## Verification Rules (v1)

Pass criteria:

1. PMTiles header is valid (`PMTiles`, version 3).
2. Directory and metadata sections are within file bounds.
3. Metadata JSON parses.
4. `format` indicates MVT/PBF payload expectation.
5. All addressed tiles can be read.
6. All tile payloads decompress as gzip.
7. All decompressed payloads parse as valid MVT structure.
8. Observed layers are consistent with metadata-declared vector layers.
9. Required Shortbread layer set is present in archive output.

Fail fast:
- Hard I/O or container corruption can terminate early.
- Otherwise accumulate errors up to a fixed cap and then stop with summary.

## Testing Plan

### Unit tests
- Add focused unit tests in `src/verify.rs` for:
  - malformed headers
  - invalid metadata JSON
  - gzip failures
  - invalid MVT payload bytes

### Integration tests
- Extend `tests/pmtiles_roundtrip.rs`:
  - run verifier on synthetic generated archives (pass case)
  - inject broken archive variants (fail cases)

### Full pipeline test
- Keep existing ignored full-pipeline integration test.
- Add optional verifier invocation on its output.

## CI Plan

Keep CI simple:

1. Existing test suite remains.
2. Add one integration test path that exercises `verify` on synthetic fixtures.
3. No real PBF full-pipeline run in default CI.

## Commit Plan

Three commits max:

1. Working `verify` command (readable end-to-end checks).
2. Tests for pass/fail cases.
3. CI/docs updates.
