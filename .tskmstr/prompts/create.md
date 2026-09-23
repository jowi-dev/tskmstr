# Create a ticket

Draft a new ticket for this repository. Keep the ticket body human-readable;
put the detailed plan in thatch memory, not the ticket.

## Recall thatch memory first

Before drafting, search thatch memory for context relevant to this repo and the
work you are about to describe — prior plans, conventions, and decisions — so
the ticket agrees with what the project already knows.

## Draft with the thatch skill

Use the `thatch-ticket-description` skill to draft the ticket: clear sections,
bold/italic emphasis for scanning, and no invented requirements.

## Write the plan back to thatch memory

Record the detailed plan and background you gathered in thatch memory so a later
session can recall it. Keep the ticket body itself concise and readable.

## Repo-specific context

This is **tskmstr** (`tm`), a Rust CLI + ratatui TUI that links Jira/GitHub
tickets to PRs and runs autonomous work lanes. Tickets for this repo are
**GitHub Issues** on `jowi-dev/tskmstr` (`[backend] provider = "github"`,
keys look like `GH-56`). Default branch is `main`; PR branches are named
`jowi-dev/gh-<n>-<slug>` and open via `tm pr create`, never `gh pr create`.

### Before drafting

- Recall thatch memory for the area (store `jowi-dev/tskmstr`); most features
  here already have an investigation or plan memory.
- Read the relevant ADR in `docs/decisions/` (0001-0007) and any plan in
  `docs/plans/`. A ticket must not contradict an accepted ADR; if it needs
  to, say so explicitly and propose a new ADR.
- Sweep open issues for duplicates and blockers:
  `gh issue list --repo jowi-dev/tskmstr --state open --search "<keywords>"`.
- Ask the operator what in-flight work blocks or is blocked by this ticket.

### Ticket body conventions (match existing issues, e.g. #54, #55, #56)

- Sections in this order: `## SYNOPSIS`, `## PROBLEM`, `## BACKGROUND`,
  `## PROPOSED APPROACH`, `## ACCEPTANCE CRITERIA`, `## NOTES`. Add
  `## DEPENDENCIES` only when there are real blockers. Skip RELEASE PLAN and
  VALIDATION unless the change has a migration or deploy step.
- Bold/italic the phrases that carry meaning; a reader skimming only the
  emphasis should get the story.
- Link code as GitHub blob URLs:
  `[src/tui/event.rs](https://github.com/jowi-dev/tskmstr/blob/main/src/tui/event.rs)`.
- Acceptance criteria are testable and name the test shape where useful
  (fake `TicketProvider`, `FakeGitOps`, `FakeTmuxOps`, `httpmock` for Jira).
- Refer to config as TOML keys (`[work.create].dir`, `status_on_pr`) and to
  commands as `tm <verb>`; the README (`README.md`) is the source of truth
  for what exists today. Never invent a flag or config key.
- Every behavior change implies TDD and doc updates (README section, ADR if
  architectural, `docs/plans/gh-<n>-<slug>.md` for multi-step work). Say so
  in NOTES when the implementer must touch docs.
- Quality gates the implementer will run, in this order (list them in
  ACCEPTANCE CRITERIA only when the ticket adds a new kind of check):

  ```
  nix develop -c cargo fmt --check
  nix develop -c cargo clippy --all-targets -- -D warnings
  nix develop -c cargo test
  nix build
  ```

  Plain `cargo` outside `nix develop` fails to link (`-liconv`); always go
  through the flake or `direnv allow`.

### Filing

1. Write the body to a file, then:

   ```
   tm ticket create --title "<title>" --status "To Do" --body "$(cat body.md)"
   ```

   `--status "To Do"` is required: the operator's global config sets
   `status_on_create = "In Progress"`, and a board-created ticket belongs in
   the backlog so `tm ready` and a lane run can pick it up. Do not pass
   `--no-transition`; that leaves the issue with no `tm:status/*` label.
2. Label and assign after filing (`tm ticket create` has no label flag):

   ```
   gh issue edit <n> --repo jowi-dev/tskmstr --add-label enhancement --add-assignee jowi-dev
   ```

   Use `bug` instead of `enhancement` for defects. Never touch the
   `tm:status/*` labels by hand; `tm ticket transition` owns them.
3. Record blockers with `tm ticket link GH-<n> --blocked-by GH-<m>` (or
   `--blocks`). Double-check direction; inverted links corrupt `tm ready`.
4. Verify with `tm ticket audit GH-<n>` and report the issue URL.

### Hazards

- There is no ticket delete: every `tm ticket create` is permanent. Get
  operator sign-off on the draft before filing; no dry run.
- Do not edit `.tskmstr.toml`, `hooks/`, or `flake.nix` from this session;
  file a ticket for those changes instead.
- Do not push, open PRs, or start lane runs from the create session. The
  deliverable is the issue plus its thatch memory entry.
