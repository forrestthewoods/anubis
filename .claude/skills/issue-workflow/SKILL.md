---
name: issue-workflow
description: How Anubis GitHub issues are triaged, approved, worked and reviewed, including the labels that drive the Issue Tracker board. Use when filing, triaging or picking up an issue, or when asked about the board.
---

# Anubis issue workflow

Claude does most of the work; Forrest makes the calls that are his and approves
every PR. Nothing merges to master without his explicit OK in the conversation.

## Labels are the source of truth

Claude can't read or write GitHub Projects from cloud sessions (GraphQL and the
project endpoints are blocked), so every triage value is a label.
`.github/workflows/board-sync.yml` copies them onto the Anubis Issue Tracker
board (user project 8) on every issue event and every 30 minutes. The board is a
view: a card dragged by hand is put back on the next sync. To change anything,
change the labels. A failed sync run (unknown or conflicting labels, a missing
board option, an expired token) shows up in the Actions logs; check them after
changing labels.

| Field | Labels | Meaning |
|---|---|---|
| Kind | `bug`, `perf`, `feature`, `design`, `chore` | bug: wrong output, failed build, hang or security hole. perf: correct but too slow or too big; needs numbers before and after. feature: a new capability. design: an open question whose output is a decision. chore: no behaviour change (refactor, cleanup, tests, CI, docs). |
| Severity | `sev: critical/high/medium/low/nit` | critical: wrong output or a security hole that common use hits. high: a supported setup can't build, or wrong output in a rare but realistic case. medium: an edge case with a workaround, or a real speed or usability cost. low: minor. nit: no user-visible effect. For features and design: how much it matters for current goals. |
| Plan | `plan: yes/no` | yes = written on the issue concretely enough that a fresh Claude could implement it from the issue alone. |
| Complexity | `cx: low/medium/high` | The complexity of the plan. The best plan is clean, simple, surgical and maintainable; the less those hold, the more complex it is. Without a plan, a guess at the best plan likely to be found. |
| Confidence | `conf: high/medium/low` | That the plan works and is the right fix. An unreproduced AI-review finding is at most medium. |
| Worth it | `worth: yes/no` | Claude's call: does fixing it matter enough to pay for the change and the review? The bar is higher for `sev: low` and higher still for `sev: nit`, because small fixes add up to a lot of aggregate complexity. A low or nit fix is worth it when it removes code or stays a few lines; a new code path for a case nobody hits is not. Forrest can overrule. |
| Status | `status: needs-triage/needs-info/triaged/ready/in-progress/in-review/parked` | Closed issues show as Done. An open issue without a status label shows as Needs triage. |

Other labels (`python` for the python branch, `documentation`, ...) are ignored
by the sync.

## Columns

| Status | Means | Who moves it on |
|---|---|---|
| Needs triage | New or reopened, fields not set | Claude |
| Needs info | Can't be triaged or planned until someone answers a question or supplies something (a repro, a log, a measurement only they can take). The question is in a comment on the issue | Whoever owes the answer; Claude re-triages once it arrives |
| Triaged | All fields set; waiting on Forrest's call | Forrest: go (Ready), park (Parked), no (closed as not planned) |
| Ready | Approved, or meets the auto-ready rule | Claude, when it starts work |
| In progress | A branch or draft PR exists | Claude, when the PR is ready |
| In review | PR ready, CI green on Windows and Linux | Forrest approves; Claude merges on his OK |
| Done | Closed | |
| Parked | Worth it, but not now | Forrest |

**Auto-ready rule.** Claude may move an issue from Triaged to Ready without
asking when all hold: kind is bug, perf or chore (perf needs measurements before
and after in the PR); plan yes; complexity low; confidence high; worth it yes;
and it changes no Papyrus syntax, CLI flag, rule field, default or
`natron.toml` format, and forces no toolchain reinstall.

**Review limit.** At most 3 of Claude's PRs waiting on Forrest at once.

**Closing.** Claude never closes an issue on its own judgment. It recommends
"close as not planned" and closes on Forrest's word. A merged PR with
`Fixes #N` closes its issue.

## Triage (Claude)

1. Read the issue and check it against current master. Reproduce bugs when
   practical; AI-review findings keep their caveat banner.
2. Set all seven label groups in one edit (setting labels replaces the whole
   list, so include the issue's other labels).
3. Post one comment headed `## Triage`: a table of the values, then one line
   each for severity and worth it, then the plan (approach, files, tests) or
   what is missing for one.
4. If a question blocks triage or the plan, ask it in the issue (naming who it's for), set `status: needs-info`, and list it in the next digest. Information Claude can gather itself (reading code, measuring, reproducing) is not needs-info: gather it.
5. An issue Claude files gets triaged in the same session.

## Forrest's issue comments

Sessions get no notification for issue comments (only for PRs they subscribe
to). An hourly scheduled task in the coordinator session picks up Forrest's new
issue comments, reacts with 👀, and routes each to the thread working that
issue. Claude's own comments also post under Forrest's account, so the watcher
tells them apart by the "Generated by Claude Code" footer: every Claude comment
must end with it. For anything urgent, Forrest uses the project chat.

## Digest (Claude to Forrest)

When several issues wait in Triaged, post one table in the project chat sorted
by recommendation: issue link, one-line summary, the fields, the
recommendation. Forrest answers in shorthand ("go 225 233, park 212, close
203"); move the labels to match.

## Working an issue (Claude)

One thread per issue, or per group of closely related issues fixed together.
Never one thread (or one PR) for several unrelated issues; ask for a new thread
instead.

1. Set `status: in-progress`. If the plan was missing, write it on the issue
   first; if it comes out above low complexity or below high confidence, put
   the issue back to `status: triaged` for Forrest.
2. Fix it on the session's assigned branch, with tests. Follow `CLAUDE.md`:
   works on Windows and Linux, doesn't design out macOS, never reads source or
   header files, no uncacheable actions.
3. `cargo test`; for medium complexity or more, an adversarial self-review
   before asking Forrest for anything.
4. Open the PR with `Fixes #N`, subscribe to its activity, set
   `status: in-review` once CI is green, and tell Forrest what to look at.
5. Answer review comments in the PR thread; merge only on Forrest's explicit OK.

Every GitHub issue or PR reference shown to Forrest is a clickable link.
