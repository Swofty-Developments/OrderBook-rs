---
name: "source-command-implement-roadmap"
description: "Autonomously implement every roadmap issue end-to-end (PR → Copilot review → respond → green CI → merge → roadmap update) until done"
---

# source-command-implement-roadmap

Use this skill when the user asks to run the migrated source command `implement-roadmap`.

## Command Template

# Workflow: Implement the Roadmap — OrderBook-rs (unattended)

Drive every open issue in the roadmap to a **merged, green** state without
stopping for confirmation between steps. The single source of truth for *what to
do next* is [`.internalDoc/ROADMAP.md`](../../.internalDoc/ROADMAP.md).

## Unattended mode — read this first

- This command runs **autonomously**. The per-issue `STOP` / "confirm with the
  user" points in `/implement-issue`, `/create-pr`, and `/respond-review` are
  **converted into autonomous decision points** here — do **not** pause for
  approval at them. Plan, implement, test, and proceed on your own judgment,
  staying strictly within `rules/global_rules.md` and `AGENTS.md`.
- You stop **only** at an **Escalation condition** (see the dedicated section).
  When you escalate, leave the work in a safe state (PR open, CI status
  reported, roadmap untouched for that issue) and surface a concise summary.
- Never weaken a check to make progress: no `--no-verify`, no disabling a CI
  job, no `#[allow]` to silence a real warning, no skipped test, and never
  remove `#![deny(unsafe_code)]` to make code compile.
- One issue per PR. One merged PR per loop iteration.

## Context

- Repository: `joaquinbejar/OrderBook-rs` — high-performance, lock-free limit
  order book. The **core matching engine is synchronous** (crossbeam-skiplist +
  dashmap + atomics); `tokio` is **opt-in** (only `BookManagerTokio` and the
  NATS publishers). Never `.await` in the matching hot path. `#![deny(unsafe_code)]`
  is on `lib.rs` and must stay. Feature flags are gated (see policy).
- Binding rules: `rules/global_rules.md` and `AGENTS.md`. Read both before
  writing code. Follow the 10-step **Agent Workflow** in `AGENTS.md`
  (model+errors → core engine → snapshot/stats → eventing → sequencer/journal →
  NATS → manager → public surface → tests/examples/benches → docs).
- Module boundaries (must not be violated):
  - `error.rs` is a leaf (`std` + `thiserror` + upstream errors only).
  - `book.rs` is the core engine — consumes `matching`, `modifications`,
    `operations`, `stp`, `fees`, `cache`, `pool`, `iterators`, `snapshot`,
    `statistics`, `market_impact`, `trade`, `book_change_event`, `order_state`.
    It must **not** depend on `manager`, `nats*`, or `sequencer`.
  - `matching.rs` owns the algorithm — depends on `pricelevel`, `trade`, `stp`,
    `fees`, `pool`. **No tokio, no I/O.**
  - `sequencer/` depends on `book.rs` + `serialization`; **not** on `nats*` or
    `manager`. Journal-format changes require an `ORDERBOOK_SNAPSHOT_FORMAT_VERSION`
    bump + migration note.
  - `nats*.rs` and `manager.rs` depend on the core engine; the core never
    depends on them. Keep `BookManagerStd` / `BookManagerTokio` parity for any
    new book-level surface.
  - `prelude.rs` / `lib.rs` own the public re-exports; nothing inside `src/`
    imports back from `prelude.rs`.
- Specialized agents (delegate by territory):
  - `book-expert` — core engine: matching, operations, modifications, mass
    cancel, STP, fees, repricing, cache, pool, iterators, snapshots,
    statistics, market-impact, the multi-book manager, the IV solver.
  - `eventing-expert` — `sequencer/`, journal, replay, `nats.rs`,
    `nats_book_change.rs`, `serialization.rs`, `trade.rs`, `book_change_event.rs`,
    `order_state.rs`.
  - `devops` — Docker, `.github/workflows/`, `Makefile`, crates.io packaging,
    coverage tooling, Criterion benches, examples.
  - `architect` — module boundaries, coding standards, public surface, and
    keeping `doc/` + README in sync.
  - Review-only (pull in **before** opening the PR, by territory):
    `determinism-auditor` (after any change to `matching.rs`, `operations.rs`,
    `modifications.rs`, `book.rs`, `sequencer/`, or the serialization / NATS
    paths — and always before sequencer replay tests), `hotpath-reviewer`
    (perf-relevant hot-path changes), `microstructure-critic` (matching
    semantics, STP policy, fee asymmetry, README microstructure claims).
- GitHub account (must be active for every `gh` call): `joaquinbejar`. Verify
  with `gh auth status` before any `gh` operation. The `gh` wrapper selects the
  account by directory — always run `gh` from the repo root, never via a bare
  `bash -c` that bypasses the wrapper.

## Preconditions (run once at the start)

```bash
gh auth status                       # active account must be joaquinbejar
git switch main && git pull --ff-only
git status --porcelain               # must be clean before starting a new issue
```

If the tree is dirty or `main` is behind in a way that won't fast-forward,
**escalate**. Read `.internalDoc/ROADMAP.md` fully to load the order and the
gated/in-progress notes.

**Bootstrap (no roadmap yet).** `.internalDoc/ROADMAP.md` does not exist in this
repo yet (`.internalDoc/` is gitignored). If it is missing, build the work order
from live GitHub state instead and create the file so the loop can persist
progress:

```bash
gh issue list --repo joaquinbejar/OrderBook-rs --state open --limit 100 \
  --json number,title,labels,createdAt
```

Order by dependency notes in the issue bodies, then by issue number ascending.
Write a minimal `.internalDoc/ROADMAP.md` with a **Status ledger** (phase tables
with `[ ]` checkboxes + `depends on` notes), a **Remaining** list, a
**Changelog** table, and a final **recommended execution order** line. Keep it
local — never stage it.

---

## Outer loop — pick the next issue

Repeat until there is no eligible issue left:

1. Parse `.internalDoc/ROADMAP.md`. The **recommended execution order** line at
   the bottom of the Status ledger is authoritative; within it, respect the
   `depends on` notes in the phase tables (never start an issue whose dependency
   is not yet merged).
2. Cross-check against live state:
   ```bash
   gh issue list --repo joaquinbejar/OrderBook-rs --state open --limit 100
   gh pr list   --repo joaquinbejar/OrderBook-rs --state open
   ```
3. Select the **first** roadmap issue that is: open, not already merged, has all
   dependencies merged, and is **not gated** (see *Gated issues policy*). If the
   user passed a starting issue number or `continue`, honor it as the entry
   point but still respect dependency order.
4. If the only remaining issues are gated → **escalate** with the list and stop.
5. If no issues remain → finish: report the full changelog and stop.

Log the chosen issue: `▶ Implementing #<n> — <title> (phase <p>)`.

---

## Per-issue pipeline

### Step 1 — Implement and open the PR (`/implement-issue` + `/create-pr`)

Run the `/implement-issue` workflow for the selected issue, **autonomously**
(no STOPs): Understand → Plan (internally; do not enter plan-mode approval) →
Branch → Implement → Test → Pre-submission → Push → open PR via `/create-pr`.

- Branch from fresh `main`: `git switch main && git pull --ff-only && git switch -c issue-<n>-<slug>`.
- Follow the 10-step Agent Workflow in `AGENTS.md`. Delegate the body of the
  work to the owning agent per the issue's territory (`book-expert` for the core
  engine, `eventing-expert` for sequencer/journal/NATS/serialization, `devops`
  for CI/Makefile/benches/examples, `architect` for boundaries + public
  surface). Pull in the matching review-only agent **before** opening the PR:
  `determinism-auditor` after matching/sequencer/serialization/NATS changes,
  `hotpath-reviewer` for perf-relevant changes, `microstructure-critic` for
  matching-semantics or README-claim changes.
- Ship the full vertical slice: `///` docs on every new `pub` item, unit tests
  co-located, integration tests under `tests/unit/`, a runnable `examples/` demo
  and a Criterion `benches/` case when perf-relevant, round-trip tests for new
  event shapes, and `snapshots_match` verification after replay for sequencer
  changes. Bump the `What's New` sections in `lib.rs` + `README.md` and add a
  `CHANGELOG.md` entry.
- Gate before pushing: `make pre-push` must be clean (zero warnings). It runs
  `fix fmt lint-fix test readme doc` and may stage `README.md` — verify
  `git status` after it. For feature-touching work, also build the relevant
  flags, e.g. `cargo build --features special_orders,nats,bincode,journal`.
- The PR body must end with `Closes #<n>` and carry the same labels as the issue.

Capture the PR number: `PR=$(gh pr view --json number -q .number)`.

### Step 2 — Request Copilot review and wait for it

Request the GitHub Copilot code review:

```bash
# Primary
gh pr edit "$PR" --repo joaquinbejar/OrderBook-rs --add-reviewer Copilot
```

If that errors (Copilot not addable by name), use the GraphQL fallback — find
the Copilot reviewer actor, then request it:

```bash
# Find the bot id
gh api graphql -f query='
  query($owner:String!,$name:String!){
    repository(owner:$owner,name:$name){
      id
      suggestedActors(capabilities:[CAN_BE_ASSIGNED], first:50){
        nodes { login __typename ... on Bot { id } ... on User { id } }
      }
    }
  }' -f owner=joaquinbejar -f name=OrderBook-rs
# then requestReviews(pullRequestId, userIds:[<copilot id>], union:true)
```

If Copilot still cannot be requested programmatically → **escalate** (ask the
user to add the Copilot reviewer in the UI, then resume with `continue`).

**Wait for the review to land.** Copilot posts a review (state `COMMENTED` /
`CHANGES_REQUESTED` / `APPROVED`) and/or inline comments when finished. Poll
without a foreground `sleep` (use a background watcher or `gh pr checks --watch`
for CI in parallel):

```bash
# Poll until a Copilot review exists
gh api repos/joaquinbejar/OrderBook-rs/pulls/$PR/reviews \
  --jq '.[] | select(.user.login|test("[Cc]opilot")) | {state,id}'
```

Treat the review as finished once a Copilot review object appears (or Copilot
posts inline comments and no longer shows "is reviewing"). If nothing arrives
after a long wait, retry the request once, then **escalate**.

### Step 3 — Respond to the review (`/respond-review`) and fix

Run the `/respond-review` workflow for `$PR`, **autonomously**:

- Classify every Copilot comment (code change / question / suggestion / disagree
  / deferred refactor).
- For valid points: fix in the same PR, respecting `rules/global_rules.md` and
  the module boundaries. Run `make pre-push`, commit (`address review: …`), push
  **before** replying.
- For points that conflict with the rules or are wrong: reply with a one-sentence
  reason citing the rule/reference; do not change code.
- For out-of-scope suggestions: open a follow-up issue via `/create-issue`
  (add `> Created from PR #$PR review comment.`), reply with the link, and — if
  it's a genuinely new improvement — append it to the roadmap *Remaining* list.
- Reply to every comment and **resolve** each conversation.
- If a Copilot comment requires a **design decision, a new dependency, a new
  feature flag, or a breaking change** → **escalate** instead of guessing.

Re-trigger Copilot only if you made substantive changes and want a second pass
(optional); otherwise proceed.

### Step 4 — Make GitHub Actions green (hard gate)

CI must be green before merge. Always verify after the last push:

```bash
gh pr checks "$PR" --watch
```

If any check fails:

1. `gh run view <run-id> --log-failed` — read the actual failure.
2. Reproduce locally (`make pre-push`, or the specific failing target:
   `make lint` / `make test` / `make doc` / `make coverage` /
   `cargo build --release`, plus the relevant `--features` combo). Do **not**
   patch CI before you can reproduce.
3. Fix the **root cause** in the PR branch, `make pre-push`, commit, push.
4. Re-watch. Repeat up to **3** fix attempts. If still red after 3 → **escalate**
   with the failing log excerpt.

Note: `code_coverage_report` can show red on PRs that lack the `CODECOV_TOKEN`
secret (e.g. fork PRs) — that is an upload-auth failure, not a code failure.
Confirm from the log before treating it as a real break; never disable the job.

Do not merge while any check is red, pending, or skipped-because-failed.

### Step 5 — Merge the PR

Preconditions: CI green, all Copilot conversations resolved, branch up to date
with `main` (rebase if needed — `git fetch origin main && git rebase origin/main`,
resolve cleanly or escalate, `git push --force-with-lease`).

Merge with a merge commit (matches this repo's `Merge pull request #NN …`
history) and delete the branch:

```bash
gh pr merge "$PR" --repo joaquinbejar/OrderBook-rs --merge --delete-branch
```

Confirm the linked issue auto-closed (the `Closes #<n>` line). If it didn't,
close it manually with a comment linking the merged PR.

### Step 6 — Update local main

```bash
git switch main
git pull --ff-only
git remote prune origin
```

### Step 7 — Update the roadmap

Edit `.internalDoc/ROADMAP.md`:

1. Tick the issue's checkbox `[ ] → [x]` in its phase table.
2. Move it out of **Remaining** and add a row to the **Changelog** table:
   `| <date> | #<n> | <PR link> | <one-line summary> |` (use the real date from
   the environment context — never invent one).
3. If you discovered follow-up work, add it under **Remaining** with a short
   note and (if filed) its issue number.
4. Update the **recommended execution order** line so the next eligible issue is
   first.

`.internalDoc/` is **gitignored** — it is a local working document. Save the file
in place and do **not** `git add`/commit/push it (the path is excluded; a commit
would be a no-op or an error). The roadmap simply persists on disk between loop
iterations.

### Step 8 — Next issue

Return to the **Outer loop**. Emit a one-line progress summary:
`✓ #<n> merged (PR #$PR). Next: #<m>.`

---

## Gated issues policy

These are **never** implemented unattended — selecting one means **escalate and
skip** (mark it `⏸ gated` in your run log, leave it in the roadmap):

- **Design decision** required (label `question`). Cannot be resolved without a
  human call. Implement only after the decision is recorded in the issue thread.
- **New dependency** required. `rules/global_rules.md` / `AGENTS.md` forbid
  adding deps without explicit approval.
- **New feature flag** required. `AGENTS.md`: any new flag needs explicit
  approval and a matching CI / Makefile update — gate it.
- **Breaking public-API change** (label/marker ⛔). Reserved for the next
  scheduled major window.

Adding `unsafe` is **not** gated — it is outright forbidden (`#![deny(unsafe_code)]`
stays). Never reach for it.

When every remaining issue is gated, report them together and stop.

## Escalation conditions (stop and ask the user)

Stop the loop, summarize state, and wait for the user when any of these occur:

1. A gated issue is the only thing left, or is next and unresolved.
2. CI is still red after 3 root-cause fix attempts on the same PR.
3. `make pre-push` cannot be made clean without violating a rule.
4. A merge/rebase conflict cannot be resolved cleanly and safely.
5. Copilot requests a change needing a design decision, a new dependency, a new
   feature flag, or a breaking change.
6. Acceptance criteria are ambiguous or the issue contradicts the
   rules/architecture or a module boundary.
7. A change would require violating a module boundary, breaking
   `BookManagerStd` / `BookManagerTokio` parity, or bumping the journal format
   (`ORDERBOOK_SNAPSHOT_FORMAT_VERSION`) without a migration note.
8. `gh auth status` is not `joaquinbejar`, or a `gh`/git operation fails for an
   auth/permission reason.
9. The working tree is unexpectedly dirty at a loop boundary.

On escalation, never force a merge, never delete an issue, and never edit
`rules/` or `AGENTS.md` to make a check pass.

## Constraints

- Autonomous between issues; **escalate, don't guess** on the conditions above.
- One issue per PR; no scope creep — file follow-ups instead.
- `make pre-push` clean and CI green are hard gates for every PR. No `--no-verify`.
- Merge with `--merge --delete-branch`; rebase with `--force-with-lease` only.
- Keep `#![deny(unsafe_code)]` on `lib.rs`. Never add a dependency or feature
  flag without approval. Don't `.await` in the matching hot path.
- Never commit `.Codex/`, `.internalDoc/`, `target/`, `coverage/`, or secrets.
  `.internalDoc/` is gitignored and `.Codex/` is local working tooling — keep
  both local, never stage them.
- All code, comments, commits, PR/issue text, and review replies in English.
- Keep `.internalDoc/ROADMAP.md` accurate after **every** merge — it is how the
  loop knows where to resume.
