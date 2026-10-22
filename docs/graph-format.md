# `tm graph` output format

`tm graph` shows which epics unlock which: the epics in a project, the
tickets they are made of, and the links that say finishing one ticket makes
another possible. `tm graph --json` prints this as one versioned JSON
document, meant for scripts and for a future tskmstr GUI that reads the
export instead of calling Jira or GitHub itself.

```
tm graph [EPIC] [--json | --mermaid]
```

With no flag, `tm graph` prints one line per epic, `KEY  STATUS  TITLE
(done/total)`, followed by indented `unlocks:` and `unlocked by:` lines when
the epic has any. `--json` and `--mermaid` pick the other two views and
cannot be combined.

## Recording an unlocks link

An unlocks link is a `Blocks` link. To record that A unlocks B, run:

```
tm ticket link A --blocks B
```

This works on both backends: a Jira "Blocks" issue link, or a GitHub
native "blocked by" relationship.

## Links and their direction

The structure is a graph, not a tree: an epic can be unlocked by several
others, and can unlock several others. Every link has a `kind`:

- `unlocks` goes from the blocker to the blocked ticket. "A unlocks B"
  means A blocks B, so finishing A makes B possible: `from` is A, `to` is
  B.
- `parent` goes from the epic to its child: `from` is the epic, `to` is a
  ticket that is part of it. A ticket has at most one parent.

Only links that touch an epic directly appear. If a ticket in epic A
blocks a ticket in epic B, the export does not infer that A unlocks B.

## Which nodes appear

- **Every open epic** in the project.
- **Done epics, only when** they have an unlocks link to or from an open
  epic. A Done epic with no such link is left out.
- **Plain tickets at the far end of an epic's unlocks link**, with
  `is_epic: false`. They are leaves: their own links are not followed.
- **The children of every included epic**, each with a `parent` link from
  its epic.

Each key appears once. A ticket that is both a child and an unlocks
endpoint is one node with both links.

## Scoped export: `tm graph EPIC`

`tm graph EPIC` (the key is case-insensitive) builds the whole graph, then
keeps only the part connected to `EPIC`: the epics reachable from it
through unlocks links in either direction, the plain tickets next to those
epics, and those epics' children. The walk passes through epics only, so a
plain ticket shared by two epics does not connect them.

`EPIC` must be an epic in the graph. A key that is not an epic (or names no
ticket) is an error, and so is a Done epic the graph leaves out because it
has no unlocks link to an open epic.

## Cycles

A loop of unlocks links (A unlocks B, B unlocks A, or an epic that unlocks
itself) is a planning mistake, but it does not stop the export. Each loop
is printed to stderr as `warning: unlocks cycle among A, B` in every view,
and listed in the JSON document's `warnings`. The exit code stays `0`.

## The JSON document

### Top level

| Field | Type | Meaning |
|---|---|---|
| `schema_version` | integer | The format version. See "Version rules" below. |
| `generated_at` | string | When the graph was built, in UTC, as `YYYY-MM-DDTHH:MM:SSZ`. |
| `source` | object | The backend and project the graph was read from. |
| `nodes` | array of node | Every ticket in the graph: epics in listing order first, then other tickets in the order they were first seen. |
| `links` | array of link | Every link, in the order it was first seen. No `(from, to, kind)` appears twice. |
| `warnings` | array of warning | Problems in the graph's shape, such as unlocks cycles. Empty when there are none. |

### `source`

| Field | Type | Meaning |
|---|---|---|
| `provider` | string | The backend: `jira` or `github`. |
| `project` | string | The Jira project key, or the GitHub `owner/repo`. |

### Node

| Field | Type | Meaning |
|---|---|---|
| `key` | string | Ticket key, such as `PROJ-12`, `GH-12`, or `owner/repo#12` for a GitHub child in another repo. Unique within the document. |
| `backend` | string | The backend the key belongs to: `jira:<base url>:<project key>` or `github:<owner/repo>`. Lets the format hold keys from several backends later. |
| `title` | string | One-line summary. |
| `status` | string | Workflow status name, such as `In Progress`. |
| `status_category` | string | `new` (to do), `indeterminate` (in progress), or `done`. |
| `url` | string | Browsable URL for the ticket. |
| `is_epic` | boolean | Whether the backend says this ticket is an epic. |
| `children` | object or null | Child progress for each epic in the graph's epic set; `null` for every other node. A child the backend reports as an epic, but that is not itself in the epic set, has `is_epic: true` and `children: null`. |

### `children`

| Field | Type | Meaning |
|---|---|---|
| `done` | integer | Children whose `status_category` is `done`. |
| `total` | integer | All children. |

### Link

| Field | Type | Meaning |
|---|---|---|
| `from` | string | For `parent`, the epic. For `unlocks`, the blocker. A node `key`. |
| `to` | string | For `parent`, the child. For `unlocks`, the ticket it unlocks. A node `key`. |
| `kind` | string | `parent` or `unlocks`. See "Links and their direction" above. |

### Warning

| Field | Type | Meaning |
|---|---|---|
| `kind` | string | Machine-readable kind. Today always `cycle`. |
| `keys` | array of string | The node keys involved, sorted. |
| `message` | string | A one-line explanation for people, the same text printed to stderr. |

### Example

PROJ-1 is Done and unlocked PROJ-2. PROJ-2 has two children and also
unlocks the plain ticket PROJ-30.

```json
{
  "schema_version": 1,
  "generated_at": "2026-10-22T15:04:05Z",
  "source": {
    "provider": "jira",
    "project": "PROJ"
  },
  "nodes": [
    {
      "key": "PROJ-1",
      "backend": "jira:https://example.atlassian.net:PROJ",
      "title": "Accounts service",
      "status": "Done",
      "status_category": "done",
      "url": "https://example.atlassian.net/browse/PROJ-1",
      "is_epic": true,
      "children": { "done": 1, "total": 1 }
    },
    {
      "key": "PROJ-2",
      "backend": "jira:https://example.atlassian.net:PROJ",
      "title": "Billing",
      "status": "In Progress",
      "status_category": "indeterminate",
      "url": "https://example.atlassian.net/browse/PROJ-2",
      "is_epic": true,
      "children": { "done": 1, "total": 2 }
    },
    {
      "key": "PROJ-30",
      "backend": "jira:https://example.atlassian.net:PROJ",
      "title": "Invoice email template",
      "status": "To Do",
      "status_category": "new",
      "url": "https://example.atlassian.net/browse/PROJ-30",
      "is_epic": false,
      "children": null
    },
    {
      "key": "PROJ-10",
      "backend": "jira:https://example.atlassian.net:PROJ",
      "title": "Account records",
      "status": "Done",
      "status_category": "done",
      "url": "https://example.atlassian.net/browse/PROJ-10",
      "is_epic": false,
      "children": null
    },
    {
      "key": "PROJ-20",
      "backend": "jira:https://example.atlassian.net:PROJ",
      "title": "Charge cards",
      "status": "Done",
      "status_category": "done",
      "url": "https://example.atlassian.net/browse/PROJ-20",
      "is_epic": false,
      "children": null
    },
    {
      "key": "PROJ-21",
      "backend": "jira:https://example.atlassian.net:PROJ",
      "title": "Refunds",
      "status": "To Do",
      "status_category": "new",
      "url": "https://example.atlassian.net/browse/PROJ-21",
      "is_epic": false,
      "children": null
    }
  ],
  "links": [
    { "from": "PROJ-1", "to": "PROJ-2", "kind": "unlocks" },
    { "from": "PROJ-2", "to": "PROJ-30", "kind": "unlocks" },
    { "from": "PROJ-1", "to": "PROJ-10", "kind": "parent" },
    { "from": "PROJ-2", "to": "PROJ-20", "kind": "parent" },
    { "from": "PROJ-2", "to": "PROJ-21", "kind": "parent" }
  ],
  "warnings": []
}
```

## Version rules

`schema_version` is the version of this format, starting at `1`. It is its
own number, unrelated to the repo config `schema_version` that `tm check`
reads.

- Adding an optional field does not change `schema_version`. Readers should
  ignore fields they do not know.
- Removing a field, renaming one, or changing what a field or value means
  bumps `schema_version`.

## Mermaid output

`tm graph --mermaid` prints a `flowchart LR` that GitHub and most Markdown
viewers render. It draws epics and unlocks links only:

- Each epic is a rectangle labeled `KEY: TITLE (done/total)`; its children
  appear only as that count.
- A plain ticket at the end of an unlocks link is a rounded stadium shape,
  labeled `KEY: TITLE`.
- Each node is styled by status category with the `todo`, `inprogress`,
  and `done` classes.
- Node ids are the key with every character other than an ASCII letter or
  digit replaced by `_` (`owner/repo#12` becomes `owner_repo_12`). Labels
  are double-quoted, with `#` written as `#35;` and `"` as `#quot;`, so
  titles with quotes, brackets, or pipes render.
