@AGENTS.md

## More rules

### More Bash rules
- Never use sed, find, awk, or complex bash commands
- Never chain commands with &&
- Never chain commands with ;
- Never pipe commands with |

### Listing files

`ls` is the default and works everywhere except one place.

`data/` is a symlink to the big NVMe mount (see `brokkr env`), and the
claude code harness refuses to traverse it, so `ls data/...` returns a
permission denial. That denial is because of the symlink.

To ls files in data/, use `print` instead.

### Communication rules

- Never use the `AskUserQuestion` tool - the harness runs in don't-ask mode and it will be denied. When you need a decision from the user, just ask in chat with the options laid out in prose.

### General rules

- Subagents must always be launched in the foreground, (never use `run_in_background: true`) so the user can approve tool requests.

### Memory rules

Do not use your Memory functionality. Do not read, write, or update memories. Do not suggest saving things to memory. Durable context belongs in CLAUDE.md or the relevant docs.

### Bash rules

- Never use `sed`, `find`, `awk`, `head`, `tail`, or complex bash commands.
- Never `find /`.
- Never run `git` with `-C <path>`
- One Bash() invocation === one command
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
- DISABLED UNTIL WE MAKE OUR FIRST RELEASE: Remember to update CHANGELOG.md for relevant commits (but not general small performance improvements.)
- Never offer to commit or tell the user "per your rules I've left things uncommitted". Don't mention git commits, ever. The user will instruct you when to commit.

### Never destroy state you did not create

- `.brokkr/results.db` and lockfiles (`Cargo.lock`) ride along with any commit
  per AGENTS.md and this file - a dirty results.db is the EXPECTED state, to be
  staged into the accompanying commit, never questioned as a mess and never
  discarded. There is no "clean tree" to protect from it; it belongs in the
  commit.

## Orchestration loop

If and when the users asks for the orchestration loop, read `reference/orchestrate.md` before proceeding.

Competitor reference sources remain available for research: `research/planetiler/` (Java), `research/tilemaker/` (C++), `research/tippecanoe/` (C++), `research/stedsplakat/` (TypeScript/JSTS Overpass→SVG poster renderer).

- `review` fans a prompt out to fresh AI sessions, one per archetype; config is `.review.toml`, and the prompt arrives on stdin (the one sanctioned pipe). `review --help` for the full surface, `--dry-run` to see the assembled prompt without sending.
- `echo '<prompt>' | review bare --profile deep` - codex gpt-5.6-sol at xhigh, no persona, no goal. Spec critique before code exists.
- `echo '<prompt>' | review goal --profile build` - codex gpt-5.6-terra at medium, /goal-driven, workspace-write.
- A role is an archetype plus a profile and needs both: the archetype is the persona, the profile is the tier (model, effort, sandbox). Both are named for what they are, not the job they do, since any archetype takes any profile. Without `--profile build` there is no workspace-write sandbox and the agent cannot edit a file. Profiles are per host, so check `.review.toml` has a block for this one.
- Never resume a run; relaunch fresh. `--session` exists and this workflow does not use it.

## Subagents

**Always get permission from the user before launching subagents - ASK FIRST,
EVERY TIME.** This is not satisfied by the user approving the underlying task.
"Yes, fix the bug" authorizes the work, NOT the fan-out: spawning Agent/Task
subagents (Explore, general-purpose, fork, anything) is a separate decision the
user makes explicitly. Before any `Agent`/`Task` launch, stop and ask in chat -
name what you want to spawn and why - then wait for a yes. Doing the
investigation yourself with Read/Grep/Bash needs no permission; only delegating
to subagents does. The sole exception is the orchestrate.md spec-loop, which the
user invokes by name and which carries its own standing authorization.

**Do NOT use git worktree isolation for parallel agents.** Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership - zero overlap.

Agent coordination rules:
- Each agent gets exclusive ownership of specific files. No two agents touch the same file.
- Agents must read their target file FIRST. Do not replace existing code with placeholders or stub it out.
- Agents must NOT run `brokkr check`, `brokkr test`, or `cargo`. The orchestrator validates between agents.

Audit protocol:
- Do not trust agent claims of completion. Verify existence + wiring + behavior.
- Use the 3-pass audit structure: domain-specific verification, then cross-cutting reconciliation (does the new instruction actually dispatch? is the new builtin actually installed?), then editorial normalization.
- Any discrepancies doc should contain only current gaps, not historical records. Remove resolved items entirely.

Subagent prompt rules:
- Scope the investigation, not the report. Caps like "under 1500 chars" or "max 15 findings" throw away signal you asked them to surface.
- Invite lateral findings up front. If they notice a bug, optimization, smell, or anything surprising while doing the scoped work, they should flag it, even when it's outside the immediate task.
- Name the question, not the method. Don't prescribe tools ("use `git diff`", "use `Read`"), don't prescribe steps ("read in full, not just hunks"), don't enumerate files when the scope already implies them ("piners-syntax crate only" + the agent's own `ls` / `git diff --name-only` is enough). Prescribing the method wastes tokens and signals distrust.
- Don't restate rules the agent already inherits. Subagents load the same CLAUDE.md / AGENTS.md as the main session, so the bash rules, no-cargo, no-worktrees, gremlins, etc. are already in scope. Re-listing them is noise.
- Do pass anything learned in *this* conversation that the agent can't see: the user's framing, prior decisions, what's already been ruled out, the specific claim being audited.
- For review tasks, ask for findings labeled *bug* / *gap* / *smell* / *nit* so the orchestrator can triage without re-reading the whole report.
