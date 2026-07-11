# Issue tracker

Issues live in **GitHub Issues on `Tnsr-Q/ratty`**. Agents use the `gh` CLI
where available, otherwise the GitHub MCP tools / REST API. Issues were
enabled 2026-07-11.

Work lands only via branch `claude/tender-thompson-gcq3c4` → PR into `main`
(see the repo's standing constraints); issue operations themselves need no
branch.

## Wayfinding operations

How this repo expresses the /wayfinder skill's concepts:

- **The map** is a single issue labelled `wayfinder:map` — currently
  [#10, "Wayfinder map — M3 organs design-locked"](https://github.com/Tnsr-Q/ratty/issues/10).
- **Tickets** are native **sub-issues** of the map. Add one with
  `POST /repos/Tnsr-Q/ratty/issues/{map}/sub_issues` (field `sub_issue_id` =
  the child issue's database id, not its number).
- **Types** are labels: `wayfinder:research`, `wayfinder:prototype`,
  `wayfinder:grilling`, `wayfinder:task` (one per ticket).
- **Blocking** uses GitHub's native issue dependencies:
  `POST /repos/Tnsr-Q/ratty/issues/{blocked}/dependencies/blocked_by`
  (field `issue_id` = the blocker's database id). GitHub renders these in the
  issue UI, so the frontier is visible without opening the map.
- **Claiming**: a session claims a ticket by assigning it to the driving
  account (`gh issue edit <n> --add-assignee @me`) **before any work**. An
  open, unassigned ticket is unclaimed.
- **The frontier** = open sub-issues of the map that are unassigned and have
  zero open blockers. Check a ticket with:
  `gh api /repos/Tnsr-Q/ratty/issues/<n> --jq '[.issue_dependencies_summary.blocked_by, .assignees]'`
- **Resolution**: post the answer as a comment on the ticket, close the
  ticket, and append a one-line pointer to the map's **Decisions so far**
  section. Assets produced while resolving are committed to the repo (or
  linked) and referenced from the issue — never pasted into the map.

One ticket per session. Expect concurrent sessions: skip anything assigned.
