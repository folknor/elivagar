# Bugs

Defects and surprises the 2026-10-28 design hunt turned up on the way. The
hunt asked a structural question; these are what the hunters flagged
laterally. None has been verified - the fix pass verifies, and is expected to
find phantoms.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

---

# Run contract, provenance and resume

## BUG-065 - Sort records read back from chunk files are not checked against their partition

`sort/format.rs` `read_record` and `next_header_discard_payload` return the
raw key (`SortKey::from_raw`), and the merge reader never re-derives its
partition. Keys are checked where they are minted (`SortKey::new` refuses
tiles beyond z14) and in `SortKey::partition`; on read-back only the layer
byte is checked, when assembly groups the record into a tile. The tile part
of a corrupted or foreign chunk record reaches assembly unchecked. A key for a z15 tile then indexes slot 15 of the
`[u64; ZOOM_COUNT]` per-zoom counters in `assemble::report::WriteTally` and
panics; a valid z14 key from another partition is written out of order and
caught only by the writer's ordering check. The piece pre-scan checks the
bucket range but runs only for split partitions. Proposed: validate every
decoded key (normal reads and header-only scans) against its section's or
piece's tile range and return `InvalidData`.

Found by the coordinates-core landing's review.

## BUG-068 - build.rs reruns on less than its dirty flag observes

`build.rs` `dirty()` reads the whole `git status --porcelain` (minus `*.md`
and `.brokkr/results.db`), but `track()` declares only `src/`, `Cargo.toml`,
`Cargo.lock`, `build.rs`, `.git/HEAD` and the branch ref as rerun inputs. An
edit to any other tracked or untracked file (`tests/`, `scripts/`, `corpus/`,
...) marks the tree dirty by brokkr's rule, yet the script does not rerun, so
the archive can record `dirty: false` for a tree brokkr would call dirty (and
the reverse after the file is reverted). Either widen the declarations to
match (cost: reruns on unrelated edits) or narrow `dirty()` to the inputs that
reach the binary and document the divergence from brokkr's definition in
`reference/metadata.md`. Separately, `track()` builds `.git/...` paths
directly, which does not hold for a `.git` file (worktrees, submodules).

Found by the spar on the typed build-closure landing.

---

# Ocean

---

# Geometry

---

# Shortbread profile and layers

---

# Encoding and the sort record


---

# Found during the 2026-11-04 fix waves

## BUG-064 - Relation area ignores holes

`pipeline/relations.rs` computes a relation's area by summing its outer
rings without subtracting inner rings. That area drives the
`boundary_labels` area-based minimum zoom (the z2/z3/z4 thresholds now in
`shortbread/boundaries.rs`), so a boundary with large enclaves or lakes can
be promoted to a lower zoom than its true area warrants. Subtract the holes;
it can move `boundary_labels` min zooms.

Reported by the wave-10 relation fixer.

## BUG-066 - The line merger's re-encoder subtracts in unchecked i32

`encode_line_segments` in the line merger computes the MoveTo/LineTo deltas by
subtracting tile-local `i32` coordinates without an overflow check. The decode
side moved to a checked i64 cursor when every in-crate command-stream reader
was consolidated onto `mvt::decode`; this encoder did not follow. Whether any
reachable input can overflow (buffered tile-local coordinates are small) is
unverified - either check the arithmetic or document the bound that makes it
safe.

Reported by the MVT decoder consolidation implementer, 2026-11-14.

## BUG-067 - `relation_blocks_drop_rss_kb` measures dropping an empty buffer

Phase12 samples RSS around `drop(relations.blocks)` after the relation tail
and emits the difference as the `relation_blocks_drop_rss_kb` counter. But
the buffered blocks were already moved out (`std::mem::take`) into the
iterator handed to `process_relation_blocks`, which consumes and drops them
during the tail; on the spilled path the buffer was emptied when the cap
tripped. The drop being measured is always of an empty `Vec`, so the counter
reads noise, not the release of the relation stock. Either measure across
the tail's consumption or delete the counter. Behavior was carried over
unchanged by the phase12 module split.

Reported by the phase12 split implementer, 2026-11-14.

