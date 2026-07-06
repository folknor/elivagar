@AGENTS.md

## More Bash rules
- Never use sed, find, awk, or complex bash commands
- Never chain commands with &&
- Never chain commands with ;
- Never pipe commands with |

## Orchestration loop

If and when the users asks for the orchestration loop, read `reference/orchestrate.md` before proceeding.

Competitor reference sources remain available for research: `research/planetiler/` (Java), `research/tilemaker/` (C++), `research/tippecanoe/` (C++), `research/stedsplakat/` (TypeScript/JSTS Overpass→SVG poster renderer).

- `scripts/codex-review.py '<prompt>'` - codex gpt-5.5 at xhigh reasoning, no goal. Spec critique before code exists.
- `scripts/codex-implement.py [--effort LEVEL] '<prompt>'` - codex gpt-5.5, /goal-driven, medium default. Implements from a spec.
- `scripts/codex_common.py` - shared launcher: runs `codex exec`, captures NDJSON internally, prints a clean digest (final message, usage, transcript path). Never resume a run; relaunch fresh.
