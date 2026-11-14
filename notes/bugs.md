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

