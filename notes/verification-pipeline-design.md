# Verification Pipeline Design

## Status

Draft proposal for a standalone output verification subsystem for elivagar.

Goals:
- Keep verification separate from generation code paths.
- Verify produced archives as an external consumer would.
- Support future tile payload formats (MVT, MLT, others).
- Support future compression codecs (gzip, brotli, none, others).
- Provide stable CLI and CI integration for pass/fail gating.

## Problem Statement

Elivagar has strong unit and integration coverage in core modules, but no dedicated verifier command that enforces output contracts across container, metadata, compression, and payload semantics.

We need:
- A reusable verifier engine.
- Pluggable payload and codec verification.
- Structured reporting for local debugging and CI.

## Non-Goals

- Replacing existing unit tests for internal logic.
- Byte-for-byte output reproducibility checks as a default gate.
- Tight coupling to any single payload format or codec.

## High-Level Architecture

Verification runs in layers:

1. PMTiles/container verification
- Header magic/version.
- Section offsets/lengths bounds checks.
- Directory decode and entry invariants.
- Dedup invariants (`num_unique <= num_addressed`, etc.).

2. Metadata verification
- Metadata JSON parseability.
- Required keys and value type checks.
- Vector layer declarations and schema sanity.

3. Tile payload pipeline
- Select codec decoder (`auto` or explicit).
- Decode compressed tile bytes into payload bytes.
- Select payload verifier (`auto` or explicit).
- Run per-tile semantic checks.

4. Final aggregation/reporting
- Error/warning aggregation.
- Coverage stats (tiles scanned, sampled, skipped).
- Format/codec summaries.
- Exit code and optional JSON report.

## Module Layout

Proposed new module tree:

- `src/verify/mod.rs`
- `src/verify/engine.rs`
- `src/verify/error.rs`
- `src/verify/report.rs`
- `src/verify/config.rs`
- `src/verify/container.rs`
- `src/verify/metadata.rs`
- `src/verify/codec/mod.rs`
- `src/verify/codec/gzip.rs`
- `src/verify/codec/none.rs`
- `src/verify/codec/brotli.rs` (stub until enabled)
- `src/verify/format/mod.rs`
- `src/verify/format/mvt.rs`
- `src/verify/format/mlt.rs` (stub initially)

## Core Interfaces

Two plugin axes: codec and payload format.

```rust
pub trait CodecDecoder {
    fn id(&self) -> &'static str;
    fn decode(&self, input: &[u8]) -> Result<Vec<u8>, VerifyError>;
}

pub trait PayloadVerifier {
    fn id(&self) -> &'static str;
    fn verify_tile(&mut self, tile_payload: &[u8], ctx: &TileContext) -> Result<(), VerifyError>;
    fn finish(&self) -> FormatSummary;
}
```

Notes:
- `PayloadVerifier` receives decompressed payload bytes only.
- The engine is responsible for codec decode + dispatch.
- New combinations (for example `MLT + brotli`) require no engine redesign.

## Detection and Overrides

CLI and engine support both auto-detect and explicit selection.

Inputs:
- PMTiles header tile type.
- Metadata `format` token.
- PMTiles internal compression fields where available.

Rules:
- Default: `--format auto --codec auto`.
- If explicit override is passed, use override and report mismatch as warning/error based on strictness profile.

## Verification Profiles

Profiles tune scope and strictness:

1. `basic` (default local)
- Container + metadata checks.
- Small deterministic sample of tiles.
- Fast feedback.

2. `ci`
- Container + metadata checks.
- Deterministic larger sample.
- Strict schema and layer invariants.

3. `release`
- Full scan or very large sample.
- Strict mode enabled.
- JSON report artifact required.

## CLI Design

Add subcommand:

`elivagar verify <FILE>`

Flags:
- `--format auto|mvt|mlt`
- `--codec auto|gzip|brotli|none`
- `--profile basic|ci|release`
- `--sample N`
- `--strict`
- `--max-errors N`
- `--json` (machine-readable output)
- `--json-out <PATH>` (optional report artifact file)

Optional run integration:
- `elivagar run ... --verify`
- Uses same verifier engine post-write.
- Must not duplicate verification logic in pipeline code.

## Report Model

`VerificationReport` should include:
- File path.
- Selected profile, format, codec.
- Tile population stats (`addressed`, `unique`, scanned/sample size).
- Error list (bounded by `max-errors`).
- Warning list.
- Derived metrics (layer counts, decode failures, metadata issues).
- Pass/fail summary.

Exit codes:
- `0`: pass.
- non-zero: failed checks, decode failures, or fatal I/O.

## Initial Rule Set (Phase 1)

Container:
- PMTiles magic/version valid.
- Offsets/lengths in file bounds.
- Directory decodes without truncation/overflow.
- Tile entries decode to valid z/x/y ranges.

Metadata:
- Valid JSON.
- Required keys present (`name`, `format`, `vector_layers` where applicable).
- Layer declarations parse and are internally consistent.

Codec:
- gzip and none supported initially.
- brotli returns clear "unsupported in this build" until implemented.

Payload format:
- MVT verifier decodes protobuf layer/feature structure.
- MLT verifier stub with explicit unsupported status.

## Testing Strategy

1. Unit tests
- Container invariant checks.
- Metadata parser/validator edge cases.
- Codec decoder error handling.
- Format verifier edge cases.

2. Integration tests (synthetic)
- Build tiny PMTiles fixtures and run `verify`.
- Assert pass/fail and key diagnostics.

3. Optional heavy integration
- Existing ignored full-pipeline real-PBF validation remains explicit/manual.
- Verifier can be applied to those outputs without special-case logic.

## CI Integration

Current CI already runs clippy/tests.
Add:
- `elivagar verify` on one or more synthetic fixture archives as gating signal.
- Optional scheduled/manual job for larger verification profile.

Do not run full real-dataset pipeline in default CI path.

## Rollout Plan

Phase 1:
- Add `verify` module skeleton.
- Implement container + metadata checks.
- Implement codec: gzip, none.
- Implement payload: MVT.
- Wire CLI `verify`.

Phase 2:
- Add `run --verify`.
- Improve report output and JSON schema.
- Expand integration fixtures.

Phase 3:
- Add brotli codec decoder.
- Add MLT payload verifier.
- Add profile hardening for release gates.

## Open Questions

1. Where should strict schema expectations live?
- Hardcoded in verifier.
- Loaded from schema descriptor.
- Hybrid.

2. Sampling policy default:
- Fixed count.
- Percentage with cap.
- Zoom-stratified sample.

3. Failure policy for declared-vs-observed layer mismatch:
- Warning in `basic`.
- Error in `ci`/`release`.

4. JSON report stability:
- Internal only.
- Versioned public contract for tooling.

## Acceptance Criteria

Design is complete when:
- `elivagar verify` validates current MVT+gzip archives end-to-end.
- Verifier implementation does not depend on mutable generation state.
- Format and codec extension points are exercised by at least one implementation each.
- CI can fail on verification regressions from synthetic fixtures.

## Concrete File-by-File Plan

Implementation should be done in this order to keep compile state stable and enable small reviewable commits.

### Step 0: CLI and module scaffolding

1. `src/lib.rs`
- Add `pub mod verify;`
- Keep verifier API externally callable from integration tests and future tooling.

2. `src/main.rs`
- Extend `Command` enum with `Verify(VerifyArgs)`.
- Add `VerifyArgs` clap struct:
  - `file: PathBuf`
  - `format`, `codec`, `profile`
  - `sample`, `strict`, `max_errors`, `json`, `json_out`
- Route `Command::Verify` to `elivagar::verify::run_verify(...)`.

### Step 1: Core verify module

3. `src/verify/mod.rs` (new)
- Public module entrypoint.
- Re-export primary config/report/error types.
- Add `pub fn run_verify(cfg: VerifyConfig) -> Result<VerificationReport, VerifyError>`.

4. `src/verify/error.rs` (new)
- Define `VerifyError` enum with variants:
  - `Io`
  - `Container`
  - `Metadata`
  - `Codec`
  - `Format`
  - `Config`
- Implement `Display` and `From<io::Error>`.

5. `src/verify/config.rs` (new)
- Define:
  - `VerifyConfig`
  - `VerifyProfile` (`Basic`, `Ci`, `Release`)
  - `FormatChoice` (`Auto`, `Mvt`, `Mlt`)
  - `CodecChoice` (`Auto`, `Gzip`, `Brotli`, `None`)
- Add profile defaults:
  - sample size
  - strict flag defaults
  - max errors default

6. `src/verify/report.rs` (new)
- Define:
  - `VerificationReport`
  - `VerificationIssue` (severity + code + message + tile context optional)
  - `VerificationStats`
  - `FormatSummary`
- Add helpers:
  - `is_pass()`
  - bounded issue append with `max_errors`
  - optional JSON serialization shape

### Step 2: Container and metadata validators

7. `src/verify/container.rs` (new)
- PMTiles header parse helpers for verify path.
- Validate:
  - magic/version
  - offsets/lengths bounds
  - directory decodability
  - z/x/y decode validity
  - dedup header invariants
- Return typed container model for engine use (entries + metadata offsets + header fields).

8. `src/verify/metadata.rs` (new)
- Load metadata section as string.
- Parse JSON and validate required keys.
- Parse `vector_layers` for schema checks.
- Return `MetadataModel` used by engine and format verifiers.

### Step 3: Codec plugin axis

9. `src/verify/codec/mod.rs` (new)
- Define `CodecDecoder` trait.
- Expose registry/selector for `auto` and explicit choices.
- Define `decode_tile(...)` helper with issue mapping.

10. `src/verify/codec/gzip.rs` (new)
- Implement gzip decode using current project dependency stack.
- Return `VerifyError::Codec` on decode failure with tile context.

11. `src/verify/codec/none.rs` (new)
- Pass-through decoder.

12. `src/verify/codec/brotli.rs` (new stub)
- Implement trait but return unsupported error.
- Keep wiring in selector so CLI contract is already stable.

### Step 4: Payload format plugin axis

13. `src/verify/format/mod.rs` (new)
- Define `PayloadVerifier` trait.
- Define `TileContext` (z/x/y + tile_id + offsets).
- Selector for format verifier (`auto` and explicit).

14. `src/verify/format/mvt.rs` (new)
- Decode MVT payload using existing protobuf logic patterns.
- Validate layer and feature envelope:
  - layer name non-empty
  - feature geometry type valid
  - counts and table structure coherent
- Accumulate format metrics for report summary.

15. `src/verify/format/mlt.rs` (new stub)
- Trait implementation returning unsupported status for now.
- Keep compile-time extension point for later MLT integration.

### Step 5: Engine orchestration

16. `src/verify/engine.rs` (new)
- Orchestrate full flow:
  - parse config/profile
  - run container checks
  - run metadata checks
  - select codec + format verifier
  - tile iteration (full or sampled)
  - per-tile decode + payload verify
  - final summary and pass/fail
- Ensure deterministic sampling:
  - stable order over directory entries
  - fixed seed derived from file hash or header constants

### Step 6: Run integration

17. `src/pipeline.rs`
- No logic changes required for verifier core.
- Phase 2 only: if `run --verify` is added, call verifier after successful write using output path.

### Step 7: Tests

18. `tests/verify_cli.rs` (new)
- End-to-end CLI tests for:
  - passing synthetic PMTiles
  - metadata failure
  - codec mismatch override
  - unsupported format/codec behavior

19. `tests/verify_engine.rs` (new)
- Direct engine tests against synthetic fixtures:
  - profile behavior
  - sample size behavior
  - max error truncation
  - strict vs non-strict mismatch policy

20. `tests/pmtiles_roundtrip.rs` (existing)
- Add verifier invocation tests on generated fixtures.
- Keep ignored full-pipeline test unchanged, but assert verifier pass when enabled.

21. `src/verify/*` unit tests (inline per module)
- Header decode edge cases, metadata edge cases, decode failures, stub behavior.

### Step 8: CI wiring

22. `.github/workflows/ci.yml`
- Add verify job step after tests:
  - build synthetic fixture in-test or from test helper
  - run `elivagar verify ... --profile ci`
- Keep real PBF pipeline out of default CI.

### Step 9: Documentation

23. `README.md`
- Add `verify` command usage and examples.
- Document profiles and exit codes.

24. `CLAUDE.md`
- Add short verifier usage discipline:
  - use `verify` for output contract checks
  - keep heavy full-pipeline runs explicitly user-triggered

## Commit Plan

Suggested commit slicing:

1. CLI + verify module scaffolding (`lib.rs`, `main.rs`, `verify/{mod,error,config,report}`).
2. Container + metadata validation.
3. Codec and format trait axes with MVT + gzip/none and stubs.
4. Engine orchestration and deterministic sampling.
5. Tests (unit + integration + CLI).
6. CI + docs updates.

Each commit should pass `brokkr check`.
