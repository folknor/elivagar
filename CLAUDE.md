@AGENTS.md

## More rules

### Listing files

`data/` may be a plain directory or a symlink to a separate drive,
depending on the host. The claude code harness refuses to traverse
symlinks, so on a host where `data/` is a symlink, `ls data/...` returns
a permission denial - use `print` as the workaround there.

### Communication rules

- Never use the `AskUserQuestion` tool - the harness runs in don't-ask mode and it will be denied. When you need a decision from the user, just ask in chat with the options laid out in prose.

### Memory rules

Do not use your Memory functionality. Durable context belongs in CLAUDE.md or the relevant docs.

### Bash rules

- Never use `sed`, `find`, `awk`, `head`, `tail`, or complex bash commands.
- Never `find /`.
- One Bash() invocation === one command.
- Never chain commands with `&&`.
- Never chain commands with `;`.
- Never chain/pipe commands with `|`. Exception: piping into `review` is allowed.
- Never capture stdout into env vars (`UUID=$(...)`).
- Keep `git commit -m` messages free of zsh metacharacters - braces `{}`, brackets `[]`, parens `()`, angle brackets `<>`, `#`. They trip the permission matcher and block the commit. Spell lists out (`syntax, vm, data and runner`, not `{syntax,vm,data,runner}`), write `5.1 per bar` not `5.1/bar`, name attributes in prose not `#[attr]`.

### git commit rules
- Always run `brokkr fmt` before a commit.
- Never commit markdown changes alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Hard-wrap the message body at ~72 columns, matching the existing history; the
  subject stays one concise line. The wall-of-text we keep producing comes from
  `git commit -m "<whole paragraph>"`: a single `-m` is recorded as ONE unwrapped
  line. Embed real line breaks so every body line wraps at ~72 (one `-m` per
  paragraph is fine only when each paragraph already carries its own newlines).
  Newlines are not metacharacters, so this composes with the no-metacharacters-in
  `-m` rule (CLAUDE.md Bash rules) - wrap with literal newlines while still
  avoiding braces, brackets, parens, angle brackets and the hash sign.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless the user explicitly asks. Stop after the commit.
- Remember to update CHANGELOG.md for relevant commits (but not general small performance improvements.)
- Never offer to commit or tell the user "per your rules I've left things uncommitted". Don't mention git commits, ever. The user will instruct you when to commit.

### Never destroy state you did not create

- `.brokkr/results.db` and lockfiles (`Cargo.lock`) ride along with any commit
  per AGENTS.md and this file - a dirty results.db is the EXPECTED state, to be
  staged into the accompanying commit, never questioned as a mess and never
  discarded. There is no "clean tree" to protect from it; it belongs in the
  commit.

### Subagents

- Always get permission from the user before launching subagents.
- Do NOT use git worktree isolation for parallel agents. Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership - zero overlap.

Agent coordination rules:
- Each agent gets exclusive ownership of specific files. No two agents touch the same file.
- Agents must read their target file FIRST. Do not replace existing code with placeholders or stub it out.
- Agents must NOT run `brokkr` or `cargo`. The orchestrator validates between agents.

Audit protocol:
- Do not trust agent claims of completion. Verify existence + wiring + behavior.
- Use the 3-pass audit structure: domain-specific verification, then cross-cutting reconciliation (is the new code actually wired into its callers?), then editorial normalization.
- Any discrepancies doc should contain only current gaps, not historical records. Remove resolved items entirely.

Subagent prompt rules:
- Scope the investigation, not the report. Caps like "under 1500 chars" or "max 15 findings" throw away signal you asked them to surface.
- Invite lateral findings up front. If they notice a bug, optimization, smell, or anything surprising while doing the scoped work, they should flag it, even when it's outside the immediate task.
- Name the question, not the method. Don't prescribe tools ("use `git diff`", "use `Read`"), don't prescribe steps ("read in full, not just hunks"), don't enumerate files when the scope already implies them. Prescribing the method wastes tokens and signals distrust.
- Don't restate rules the agent already inherits. Subagents load the same CLAUDE.md / AGENTS.md as the main session, so the bash rules, no-cargo, no-worktrees, gremlins, etc. are already in scope. Re-listing them is noise.
- Do pass anything learned in *this* conversation that the agent can't see: the user's framing, prior decisions, what's already been ruled out, the specific claim being audited.

### Codex agents

Never tell a codex agent to read CLAUDE.md (it is Claude-specific and contradicts their job), and never tell them to read AGENTS.md (codex loads it automatically). Put any rule they need directly in the prompt.

## Docs site

VitePress, in `docs/`, deployed to GitHub Pages by
`.github/workflows/deploy.yml` on every push to main. Sourced from the
`gh-template` remote, configured as a local-path git remote.

```
pnpm install --frozen-lockfile
pnpm dev        # localhost:5173
pnpm build      # writes docs/.vitepress/dist
pnpm preview
```

**Template updates are pulled per path, never merged.** `git checkout
template/master -- <path>` picks up a theme or layout fix. Do NOT `git merge
template/master`, which is what the template's own README tells you to do: it
carries a `raw/` directory holding a fully vendored copy of the mise repo plus
four projects' logos and generated images, and merging unrelated histories
writes all of it into this repo permanently even if the next commit deletes
it. pbfhogg took that hit; we did not.

**Two pnpm projects, deliberately.** The root workspace owns the docs site and
declares `packages: ['.']` only. `scripts/validate/` keeps its own lockfile
because the oracles are calibrated against specific versions of maplibre-gl
and `@mapbox/vector-tile` - a shared lockfile would let a VitePress bump
re-resolve the exact packages the earcut gate measures against, silently
invalidating the calibration the oracle discipline in AGENTS.md requires.
Install the oracles from their own directory.

**The reference docs are included, not copied.** `docs/reference/*.md` are
four-line stubs carrying `<!--@include: ../../reference/<name>.md-->`, so
`reference/` stays the single source of truth and the site cannot drift from
it. Add a reference document by adding a stub plus a sidebar entry in
`docs/.vitepress/config.ts`; edit content in `reference/` only.

Guide pages under `docs/guide/` ARE hand-written for outside readers. They may
compress or omit internal detail, but must never contradict `reference/` or
AGENTS.md.

Run `pnpm build` after touching anything under `docs/`, and
`cargo package --list` after touching what ships - only `docs/public/*.svg`
may appear there.
