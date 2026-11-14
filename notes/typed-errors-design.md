# Typed errors with a defined exit-code mapping - design and link plan

The structural finding on typed errors is a chain. Link 1 landed the error
type and the exit mapping; each later link types one subsystem at its
source. This file holds the settled rulings and the remaining links. Delete
it when the last link lands.

## Settled rulings

1. `PipelineError` is an opaque struct over a private inner enum. The public
   `kind()` is derived from the variant, so a kind and a reason can never
   disagree. Causes are preserved as boxed `dyn Error + Send + Sync`, never
   formatted away. Typed reasons are exposed by accessor where the operator
   action differs (`resume_refusal()`, later `artifact_refusal()`).
2. Evidence rule: a known operational failure is `Io`; a known domain
   rejection is a refusal; everything else is `Failed`. The absence of an
   errno is never evidence. `From<io::Error>` maps an error carrying
   `raw_os_error()` to `Io` and anything else to `Failed`. Nothing infers a
   domain refusal from an `io::ErrorKind`.
3. A wrapper constructed in the pipeline around an opaque subsystem failure
   (PBF open and read, way-index reads, MLT encoding, channel disconnects)
   is `Failed` unless the underlying typed error establishes more.
4. `Internal` is only for impossible in-memory program state. Persisted state
   that differs from expectation (sort chunks read back, layer ids read back,
   computed ocean found in builders populated from records) is `Failed`
   until the chunk store types its provenance.
5. Exit codes are broad failure classes, not recovery instructions. 0 ok;
   1 a verifier's negative report; 2 unclassified failure (may follow
   substantial work); 3 resume state absent or incompatible; 4 the named
   ocean artifact cannot serve the run; 64 invalid invocation or config
   (clap errors included, help and version stay 0); 65 input bytes read and
   rejected; 70 internal invariant; 74 operational I/O failure. Panics abort
   (release profile `panic = "abort"`) and are outside the table.
6. Sidecar `resume_refusal_*` counters are diagnostics. No recovery is
   promised on them; their absence means "reason unavailable".
7. `From<String>` is removed. Every string-returning validator is
   classified at its call site.
8. A typed producer and every caller and carrier of it land in the same
   link, or the generic conversion turns the new classification back into
   `Failed`.

## Link plan

Link 1 (landed): the type, the mapping in `main.rs` for every subcommand,
hand classification of every direct construction in `src/pipeline/**` and
`src/resources.rs`, typed `ResumeRefusal` in the checkpoint module, artifact
payload checks in assembly as `ArtifactRefused`, the exit table in
`reference/cli.md`.

Link 2 (medium): ocean artifact admission and reads typed at source
(`ArtifactRefusal`, `KeyMismatch` naming the differing key fields:
policy version versus shapefile hashes versus zooms versus level), the
artifact directory read during assembly, shapefile errors preserving errno
at the `io::Error::new(err.kind(), format!(..))` rewraps in
`ocean/shapefile.rs`, shapefile hashing failures kept out of the artifact
class.

Link 3 (medium): sort chunk store typed at source (`ChunkStoreError`:
compression mismatch, stale or missing chunk, count mismatch, corrupt),
covering both `SortReader::from_dir` and `SortWriter::resume`, so resume
paths reach exit 3 and full-run read-back failures stay `Failed`.

Link 4 (large): archive reader and directory codec, `tile_detail` and
`mvt::message` errors typed (`DecodeError` with a locus and kind), and the
inspect, svg and diag subcommands classified through it. diag coordinate
validation becomes config (64); diag keeps reporting feature decode
failures and continuing.

Link 5 (medium): verify splits operational read failures out of findings
(today a `read_tile` failure is recorded as "decompression failed" and exits
1), typed findings with `MAX_TILE_ERRORS` as presentation,
`VerifyError` variants carrying typed causes. Paint-order findings
(`RankError` is already typed internally).

Link 6 (small): encoding and merge violations (`MltEncodeError`,
both `check_merge` functions) as typed results, tests matching variants.

Link 7 (medium): panic and silent-drop sites. Node-store flat-index safety
cap as an error, relation re-read `expect`, ring-cap indivisible rect in
`geometry/pyramid.rs` and the discarded `enforce_ring_cap` bool,
multipolygon dropped unclosed chains, promoted orphan inners and degenerate
rings as counters, the sort-record decoder's silent returns (adjudicate
against its documented hot-path policy and the round-trip proposal in the
structural findings, refuse with a comment if it does not pay).

Link 8 (N+1, green agent): full audit and gate.
