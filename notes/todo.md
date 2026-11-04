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
