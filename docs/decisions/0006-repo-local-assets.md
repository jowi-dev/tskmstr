# ADR-0006: `.tskmstr/` Is the Repo-Local Home for Generated Assets

**Status:** Accepted
**Date:** 2026-09-11

## Problem

`tm init` scaffolds assets a lane run reads back later — today the lane
prompt file, tomorrow possibly more (per-repo skills, templates). GitHub
issue #30 left their location an open decision with three candidates:

1. A committed `.tskmstr/` directory at the repo root.
2. The historical bare `prompts/<lane>-lane.md` default.
3. Personal, gitignored files (e.g. under `~/.claude`).

The choice matters because issue #30's goal is *a lane that works on run
one, including on a fresh clone*: option 3 fails that outright (a clone gets
nothing), and option 2 scatters tm-generated files across the repo root
with nothing marking them as tm's.

## Decision

**Generated repo-local assets default into a committed `.tskmstr/`
directory at the repo root** — `tm init` scaffolds lane prompts at
`.tskmstr/prompts/<lane>-lane.md`. The directory plays the `.vscode/` role:
one machine-portable home for tm's per-repo assets, consistent with the
`.tskmstr.toml` config file beside it. Repos that treat lane prompts as
personal can add `.tskmstr/` to `.gitignore` and keep the same layout.

Two deliberate boundaries:

- **This is a default, not a constraint.** `prompt_file` remains an
  arbitrary path resolved by the existing rules (relative paths against the
  repo root, `~` against home — see `resolve_prompt_path` in
  `src/work/run.rs`); existing configs keep whatever they name. No loader
  learned anything new for this decision.
- **Runner-owned discovery directories stay runner-owned.** Session skills
  are only found where the agent CLI looks for them (`.claude/skills/` for
  the claude runner, via `AgentRunner::skills_dir`), so skills authored by
  the agent-assisted setup land there, not in `.tskmstr/`. `.tskmstr/`
  holds only assets whose paths tm itself resolves.

## Consequences

- A fresh clone of an onboarded repo has a working lane: the config names
  `.tskmstr/prompts/<lane>-lane.md` and the file travels with the repo.
- `tm`'s footprint in a repo is discoverable at a glance: `.tskmstr.toml`
  plus `.tskmstr/`.
- Repos onboarded before this decision keep their `prompts/<lane>-lane.md`
  files untouched; the new default only applies when the wizard proposes a
  path for a lane that doesn't have one.
