# Todo

Things the project might do later. Not defects: each would make the project
claim something it does not claim today.

## `elivagar verify` for MLT and brotli archives

`verify` accepts only MVT + gzip archives, while the writer can produce MLT
(uncompressed) and brotli-compressed MVT. Verifying those would need a
decompressor chosen from the header, and for MLT a separate structural
validator, since the MVT command grammar checks do not apply. Surfaced by the
2026-10-28 design hunt; set aside at triage on 2026-11-04 as a new capability
rather than a fix.

# Leftovers from moving the elivagar tooling out of brokkr

Small items left after the in-tree crates took over from brokkr's elivagar
commands.

## Stale brokkr spellings in in-tree code and config

Text that still names brokkr commands that are going away:
- crates/corpus/src/gate/regress/mod.rs: the module doc points to `brokkr
  pmtiles-inspect` for reading provenance; it is now `brokkr run elivagar
  -- inspect <path>` fed by `elivagar-devkit archive`.
- crates/corpus/src/gate/corpus/sort.rs: the spill-name comment says
  `brokkr clean` removes leftovers; `elivagar-devkit clean` does now.
- Comments in benches/run/tilegen.lua, brokkr.d/benches.toml and
  brokkr.d/hosts/*.toml (`brokkr tilegen`, `brokkr download-ocean`) that
  read as current rather than historical.

## serde_json feature split recompiles the library

crates/corpus enables `serde_json/float_roundtrip`, while crates/lib and
crates/cli use plain `serde_json`. Under resolver 3, a `-p elivagar-corpus`
build (`brokkr run elivagar-corpus`) resolves serde_json with the feature
and every product build resolves it without, so serde_json and crates/lib
build in two variants. Not confirmed as the cause of the reported
recompile-every-time. Fix: enable `float_roundtrip` on crates/lib's
serde_json too, so every graph agrees. The feature changes how the library
parses floats, so check what JSON crates/lib parses and run the corpus gate
before landing it.

## tilemaker needs Boost on the host

`elivagar-tools tilemaker` builds tilemaker from source and needs Boost
(system, filesystem, program_options, 1.66 or newer) installed. Planetiler
is verified on bygg; tilemaker has not yet been benched on a host with
Boost present.
