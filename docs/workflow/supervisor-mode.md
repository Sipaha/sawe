# Supervisor mode — multi-agent workflow for sawe

> Read by the **top-level supervisor** (main Claude Code session). Sub-agents
> have a narrow task scope from their dispatch prompt — they do NOT read this
> file.
>
> Adapted 2026-05-15 from voxelcraft's `docs/workflow/supervisor-mode.md`,
> tailored to sawe's "fork-of-Zed, mostly bugfixes + targeted feature
> work" reality. The playbook itself is **in scope to improve** — when a step
> trips you twice, fix the doc, don't soldier through it.

## 0. Bootstrap — resume from a paused session

When the user types **"ты автономный супервизор"**, **"продолжай работу"**,
**"continue work"**, **"resume"**, or any close paraphrase:

1. `Glob "docs/findings/*-session-handoff.md"` → `Read` the latest one
   (filenames are dated `YYYY-MM-DD`, lexicographic order = chronological).
   That file is the previous session's pause snapshot.
2. `git log --oneline -15` — confirm the commit chain matches what the
   handoff claims (drift detection).
3. Read this file's § 7 "NEXT" — the priority heuristic.
4. Pick from the pool. Start in the **same turn**. No clarifying
   questions, no priority polls (see `§ Anti-patterns`).

No handoff finding exists? → read `docs/INDEX.md` first; the plans table
shows what's open. Pick the highest-priority "ready to dispatch" row.

When the user pauses a session ("сделай паузу", "пауза", "stop",
"context reset"), the supervisor **must** write a fresh
`docs/findings/YYYY-MM-DD-session-handoff.md` BEFORE the final response.
Structure (see prior examples):

- One-line status.
- Cumulative table of phases shipped this session (with commit SHAs).
- Findings + ADRs created.
- Workflow rules established / updated.
- **Outstanding pool** with priority recommendation.
- Open architectural decisions (un-written ADRs).
- Active gotchas the next session should know.
- Resume recipe (one paragraph: read this → INDEX → git log → pick).

Then update `docs/INDEX.md` § findings with the new row at the top
(`status: handoff`). The handoff finding is the load-bearing artefact
that makes the next session resume bootstrap-free.

---

## Two tracks

Every user request flows down one of two tracks. The supervisor picks the track
**at READ time** and sticks with it (escalate light → heavy if scope creeps
mid-flight, never the reverse).

```
LIGHT  (bugfix / small UI tweak / single-crate refactor)
  1. READ      → CLAUDE.md (in context), FORK.md if upstream-zone, 1–2 grep findings
  2. DISPATCH  → Agent (general-purpose), single sub-agent, worktree OPTIONAL
  3. VERIFY    → cargo build --bin sawe + clippy/test on touched crate
                 + MCP smoke-test if UI-visible
  4. COMMIT    → one commit, descriptive subject (NO Co-Authored-By, NO amend)

HEAVY  (new feature / architectural change / multi-crate / new public API)
  1. READ      → docs/INDEX.md, current spec(s) in docs/superpowers/, FORK.md,
                 1–2 related ADRs (when we have them)
  2. PLAN-DOC  → docs/plans/YYYY-MM-DD-<slug>.md (≥ 6 sections)
                 → commit "plan: <slug>"
  3. ADR       → docs/architecture/decisions/NNNN-<slug>.md if the decision
                 has long-term consequences (data format, public API contract,
                 multi-crate invariant). Skip for tactical choices.
  4. DISPATCH  → 1+ Agents. Parallel = `isolation: "worktree"` MANDATORY.
  5. VERIFY    → full check matrix (see § 4) + visual smoke-test if UI
  6. FINALIZE  → tick spec acceptance, update INDEX.md, commit
                 "finalize: <slug>", screenshot if visual
  7. PUSH      → ONLY when user asks (this fork has no scheduled push cadence)
```

**Cadence:** LIGHT ≈ 5–30 min. HEAVY ≈ 30 min – 2 h per phase (most fall into
30–60 min). Don't pad LIGHT work with phase-doc ceremony — that's friction, not
discipline.

> **The supervisor's job includes optimising the workflow itself.** If a step
> trips you twice (an instruction sub-agents misread, a verification you can't
> do cleanly, a tool gap that forces release builds) — fix the playbook /
> tooling / sub-agent prompt template, don't soldier through it again.

---

## 1. READ — gather context

### Always (both tracks)
- `CLAUDE.md` (already in context — but skim "What's disabled" + "Build conventions").
- `FORK.md` § "Touched upstream files" — if you're about to edit a file in
  `crates/{editor,language,lsp,multi_buffer,project,terminal,dap,vim,theme,gpui*,…}`,
  check whether it's already listed. If yes — it's fork-touched, refactor more
  freely. If no — it's an **untouched core crate**, apply the "stay
  upstream-shaped" rule (additive patches, no style renames).

### HEAVY track adds
- `docs/INDEX.md` — the doc bookshelf (currently absent; see § Bootstrap).
- The latest 1–3 plan docs in `docs/superpowers/plans/` and their corresponding
  spec docs in `docs/superpowers/specs/` for context on what's recently shipped.
- `git log --oneline -10` to know what's fresh on master.
- The ADRs for the area (when we have them).
- For UI work: 1 screenshot from the user's report (`Read` the image) before
  proposing layout changes — guessing at pixel coordinates without seeing the
  current state is wasted.

### Signal you've gathered enough
You can answer in two sentences: **what am I about to change, and why this way**.

---

## 2. Track decision

Pick LIGHT iff **all** of:
- Single crate touched.
- ≤ ~200 LOC net change (rough — don't measure).
- No new public API on a fork-owned crate.
- No refactor of an untouched-upstream file (only an additive patch).
- No locked-rebrand-identifier change (see CLAUDE.md § "Locked rebrand
  identifiers" — these need explicit user approval regardless).
- Not toggling a disabled subsystem (see CLAUDE.md § "What's disabled").

Otherwise: HEAVY. When in doubt, HEAVY — the cost of an unnecessary plan doc
(~10 min to write) is cheaper than the cost of an unscoped sub-agent dispatch
(half-implemented mess, hours to unwind).

---

## 3. PLAN-DOC structure (HEAVY only)

`docs/plans/YYYY-MM-DD-<slug>.md`:

```markdown
# <Title>

**Status:** ready to dispatch
**Estimated:** <range>   (1 sub-agent session, or N parallel sessions)
**Goal:** <one sentence — what the user gets out of this>

## Context
<what triggered this; previous spec(s) it builds on; the gap it closes>

## Scope
### A. <component> — `crates/<crate>` / `<file>`
- <specific implementable item>
- <test that proves it>
### B. ... (3–6 sections)

## Out of scope
- <thing the reviewer might expect but we're deferring; link to follow-up if filed>

## Architectural decisions
<2–5 numbered decisions. If any is genuinely architectural — also file an ADR.>

## Risks
- <risk>: <mitigation>

## Verification
- cargo build --bin sawe
- cargo clippy -p <crate> -- -D warnings
- cargo test -p <crate>
- MCP smoke-test: <what the agent will drive via the socket; what the assertion is>
- Visual: <screenshot of the relevant region, before/after if UI>

## When done
<concrete checklist for "phase closed">
```

Commit the plan doc as a separate commit BEFORE dispatching — sub-agents read it,
and if the dispatch goes sideways the plan is still on the tree.

```bash
git add docs/plans/YYYY-MM-DD-<slug>.md
git commit -m "plan: <slug>

<3–5 line description: scope, why this design over alternatives>"
```

> **CRITICAL — worktree freshness trap.** The Claude Agent SDK's
> `isolation: "worktree"` branches from **session-start HEAD**, not from
> current `main` at dispatch time. Plans + rule updates made earlier in
> the same session are **invisible** to dispatched sub-agents.
>
> Two workarounds, used together:
>
> 1. **Inline the plan-doc** in the sub-agent dispatch prompt under a
>    `## PLAN DOC` header — they read it from the prompt, not from disk.
>    Same for recent rule changes in `.rules` / `CLAUDE.md` / `FORK.md`.
> 2. **Tell the sub-agent to `git rebase origin/main` at the start** if
>    earlier-in-session commits would otherwise be missing from their
>    base — e.g. when this phase depends on a prior phase's crates /
>    files that landed on main in the same session. (Sub-agent for
>    R-1.5 rediscovered this unprompted 2026-05-15; bake it in.) The
>    rebase is a clean fast-forward if no in-flight work in the worktree
>    has conflicting changes — sub-agents that hit a conflict should
>    surface in REPORT rather than resolve blindly.
>
> See [`docs/findings/2026-05-agent-worktree-staleness.md`](../findings/2026-05-agent-worktree-staleness.md).

---

## 4. DISPATCH — sub-agent prompt template

```
Agent({
  description: "<10–40 char summary>",
  subagent_type: "general-purpose",      // or "Plan" for read-only investigation
  prompt: `<see template below>`,
  isolation: "worktree",                  // MANDATORY for parallel dispatch
  run_in_background: true                 // if you have other work to do meanwhile
})
```

### Mandatory prompt sections

```
## MANDATORY BEFORE WORK

1. CLAUDE.md — the hard rules. Skim "What's disabled" and "Build conventions".
2. FORK.md — § "Touched upstream files" + § "Fork-owned crates".
3. docs/plans/YYYY-MM-DD-<slug>.md — THE FULL SPEC for this task. If
   this path doesn't exist in your worktree, the spec is inlined below
   under `## PLAN DOC` — the worktree may be pinned to session-start
   state.
4. 1–3 related ADRs (give the paths if any).
5. The existing files you'll edit (give the paths).

## SCOPE — sections A, B, C…

For each section:
- The specific API (struct / fn signatures).
- The files created / edited.
- The tests that should be written.

## HARD RULES

> These "NO X" rules forbid X **in your code changes** — they do NOT forbid
> running tooling. `cargo build`/`test`/`fmt`/`clippy`, `script/run-mcp`,
> and reading MCP responses are *required* during verification, not "modifications".

- NO Co-Authored-By in commits. NO `git commit --amend` / no rewriting commits.
  Just create fresh commits.
- NO `unwrap()` / `expect()` in production code. In `#[cfg(test)]` / `tests/` — fine.
- NO `let _ = fallible_call()?` — handle errors (`?` to propagate, `.log_err()`
  to swallow visibly, `match` for custom logic).
- NO release builds. Always `cargo build --bin sawe` (debug), `cargo test`
  (debug). `cargo build --release` / `script/bundle-*` is for the maintainer.
- NO changes to locked rebrand identifiers (CLAUDE.md § "Locked rebrand
  identifiers") without explicit user approval.
- NO re-enabling disabled subsystems (auto_update, telemetry, collab, Zeta,
  native cloud LLM, Sentry, upstream AgentPanel) — see CLAUDE.md "What's disabled".
- In **untouched upstream crates** (`crates/{editor, language, lsp,
  multi_buffer, project, terminal, dap, vim, theme, gpui*, …}` not already
  listed in FORK.md § "Notable upstream file modifications"): bug fixes
  in-tree are fine; prefer **additive** patches; do NOT refactor / rename
  for style. If you'd genuinely benefit from a non-additive change there
  (split file, rename type, restructure), surface it in your REPORT and
  let the supervisor decide — don't take that call yourself. See ADR-0001.
- If you first-touch an upstream file: ADD A ROW to FORK.md § "Notable
  upstream file modifications" in the same commit.
- New files: prefer `src/<module>.rs`, NOT `src/<module>/mod.rs`. New crates:
  set `[lib] path = "<name>.rs"` in Cargo.toml.
- Build verification: `cargo build --bin sawe` MUST pass before commit.
  If you ran `cargo build … | tail` and saw "succeeded" — re-check with
  `set -o pipefail` or read the captured output; the pipe masks cargo exit
  codes (CLAUDE.md trap).

## CHECKS

cd /home/spk/.spk/sawe/solutions/spk-solutions/sawe
cargo build --bin sawe 2>&1 | tee /tmp/build.txt
grep -E "^error|could not compile" /tmp/build.txt    # must be empty
cargo clippy -p <crate> --all-targets -- -D warnings 2>&1 | tee /tmp/clippy.txt
cargo test -p <crate> --no-fail-fast 2>&1 | tee /tmp/test.txt
grep "test result:" /tmp/test.txt | awk '{ tot+=$4; failed+=$6 } END { print "TOTAL:", tot, "failed:", failed }'

The supervisor does the **end-to-end MCP smoke-test** post-merge (visible UI
changes only). Do NOT inline that in your prompt — it spins the editor 30–60 sec
and the supervisor's screenshot is the source of truth anyway.

## DOCUMENTATION

- Tick the relevant acceptance items [x] in the plan doc.
- If you discovered something non-obvious about the codebase — drop a finding
  in `docs/findings/YYYY-MM-<slug>.md` (one short paragraph, no fluff).
- If you established a new architectural invariant — propose an ADR in your REPORT
  (the supervisor files it, not you).
- Do NOT touch `docs/INDEX.md` — the supervisor finalizes it.

## COMMIT

git add <specific files — NOT `-A`, NOT `.`>
git commit -m "<crate>: <imperative summary in lower case>

<bullet 1>
<bullet 2>

<optional: Tests: <num delta>>"

NO Co-Authored-By. NO --amend. NO --no-verify.

## REPORT (≤ 400 words)

- Files created / changed (paths).
- The commit SHA.
- cargo build/clippy/test results — pass counts + any failure.
- Acceptance items verified end-to-end (which ones; how).
- What's left / gotchas / deferred to next phase.
- **Anything that surprised you in the codebase** — architectural bottlenecks,
  fragile abstractions, leaky couplings, "this only became clear from reading
  the source" gotchas. Surface it even if out of scope — the supervisor
  decides whether to file an ADR / finding / follow-up.
- **If you worked around something instead of root-fixing it** — say so
  explicitly with what the proper fix would be and what it'd touch.
- **If you saw flaky tests** — name them (`crate::module::test_fn`), the
  symptom, what you tried. "Saw some flaky tests" without names is
  unactionable.
```

### When to parallelize

If 2+ scopes are independent (different crates, ≤ ~20 functions of expected
overlap) — dispatch in **one message** with multiple `Agent` calls. Sub-agent
throughput is the supervisor-velocity bottleneck; be aggressive.

**MANDATORY:** every parallel write-phase dispatch uses `isolation: "worktree"`.
No exceptions. Sub-agents on the same working tree trip each other's `cargo
build` lock, write half-states the other sees mid-investigation, and a
`TaskStop` of one leaves the survivor in a dirty state. Single-agent dispatch
on master is fine; **parallel on master is not**.

### Parallel merge ritual

```bash
git log --oneline master..worktree-agent-<id_A>
git log --oneline master..worktree-agent-<id_B>

# Merge sequentially; pick the smaller / less risky first.
git merge worktree-agent-<id_A> --no-edit
# If conflict: resolve, `git add <files>`, `git commit`.

git merge worktree-agent-<id_B> --no-edit

# Re-run checks on the merged state.
cargo build --bin sawe 2>&1 | tee /tmp/post_merge_build.txt
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tee /tmp/post_merge_clippy.txt
cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/post_merge_test.txt
# If anything fails on merged state that didn't fail in either branch in isolation
# — it's an interaction bug. Fix in a small supervisor-side commit before finalize.

# Cleanup (the Agent tool's auto-cleanup does NOT trigger — target/ is populated):
git worktree remove -f -f .claude/worktrees/agent-<id_A>
git branch -D worktree-agent-<id_A>
# (… same for B.)
```

End-of-session sweep if you deferred cleanup:

```bash
for d in .claude/worktrees/agent-*; do git worktree remove -f -f "$d"; done
git worktree prune
git branch --list 'worktree-agent-*' | xargs -r git branch -D
```

`target/` per worktree is ~5–15 GB. Five stale worktrees = 50+ GB. Don't carry
them across sessions.

### Read-only investigation (`subagent_type: "Plan"` or `"Explore"`)

For root-causing a bug, dispatch an investigation agent and tell it explicitly:
**"Read-only — don't edit source files, don't `git commit`. But DO run
`cargo build`/`cargo test`/`cargo run` and read MCP responses to confirm
hypotheses; those write only to `target/` (build artefacts, not source) and are
required to reason about the code."** Over-literal sub-agents will refuse to
compile anything otherwise and reason blind.

Read-only agents can run in parallel with a write-phase sub-agent without
worktree isolation (no working-dir conflict).

---

## 5. VERIFY — after notification

When the sub-agent finishes:

```bash
git log --oneline -3                                   # did it actually commit?
git status                                             # clean? (if not — uncommitted leftovers)
cargo build --bin sawe 2>&1 | tee /tmp/build.txt
grep -E "^error|could not compile" /tmp/build.txt      # must be empty
cargo clippy -p <touched-crate> --all-targets -- -D warnings 2>&1 | tee /tmp/clippy.txt
cargo test -p <touched-crate> --no-fail-fast 2>&1 | tee /tmp/test.txt
grep "test result:" /tmp/test.txt | awk '{ tot+=$4; failed+=$6 } END { print tot, failed }'
```

Re-run is justified ONLY when source changed since the previous run. Never
re-run to "see more output" — pipe to file once, grep the file (see
voxelcraft anti-pattern; same applies here).

### MCP smoke-test (UI / workspace / project / buffer change)

**Default to `--headless` for all agent-driven work.** Since ADR-0002,
`workspace.screenshot` returns real rendered pixels in headless mode
(native offscreen wgpu pipeline — no X server / Xvfb / window on the
user's desktop). State ops, screenshots, action dispatch, keystroke
delivery all work uniformly.

| Mode | Window | When to use |
|---|---|---|
| `--debug --headless` | None | **Default.** Every agent-driven verification — state ops, screenshots, action dispatch. No window on the user's desktop. |
| `--debug --display` | Visible on user's desktop | Only when a human developer wants to watch the editor render as the agent drives it. |

```bash
# 1. Make sure no stale editor process is holding the socket.
pgrep -af "target/debug/sawe" | grep -v bash || echo "no leftover"
# If there's one — kill by PID, NOT by pattern (a `pkill -f sawe`
# can match the current bash and exit-144 yourself).

# 2. Launch.
script/run-mcp --debug --headless &   # state-only verification (preferred)
# OR:
script/run-mcp --debug --display &    # when a screenshot is required
# Both auto-build if missing, set SAWE_HOME, and strip stale
# socket/lock state up front (so a failing precheck doesn't leave a
# half-ready state behind).
until [ -S "$HOME/.spk/sawe-dev/config/mcp.sock" ]; do sleep 0.5; done

# 3. Drive via a small Python (or socat) client over the JSON-RPC newline-delimited socket.
#    Always start with `editor.capabilities`. Then exercise the scenario from
#    the plan doc. Use `windows.dispatch_action` over `windows.click_at` when
#    a named action exists (geometry-independent).

# 4. Visual assert (`--display` mode only): `workspace.screenshot` returns
#    a PNG of the offscreen-rendered window content (immune to occlusion by
#    other windows on the same display). Read the PNG with the Read tool,
#    eyeball the visual.

# 5. Teardown.
pkill -f target/debug/sawe      # OK by name here — exit 144 noise is harmless
```

**The screenshot is the source of truth.** A sub-agent report saying "the
feature works" + a screenshot showing it doesn't → trust the screenshot,
dispatch a hotfix.

---

## 6. FINALIZE (HEAVY)

### `docs/plans/YYYY-MM-DD-<slug>.md`
- Status → `complete`.
- All acceptance items ticked `[x]`.
- Final commit SHA appended at the bottom.

### `docs/INDEX.md`
- Add a row in the relevant table (plans, ADRs, findings — see § Bootstrap for
  table layout).

### Screenshot (if UI / visual change)
- Copy from `/tmp/<slug>.png` to `docs/superpowers/plans/YYYY-MM-DD-<slug>-screenshot.png`.

### Commit

```bash
git add <plan doc, INDEX.md, screenshot if any>
git commit -m "finalize: <slug>

- INDEX.md row added
- plan doc ticked + final SHA appended
- screenshot: <one-line of what's visible>"
```

### Push
Only when the user explicitly asks. This fork has no scheduled push cadence.

---

## 7. NEXT — picking the next phase

After FINALIZE, look at the pool of outstanding tasks the user has named
across the session (UI fixes, queued asks, follow-ups surfaced in REPORTs,
spec items waiting for a phase).

**Self-pick, don't poll.** If the user named multiple tasks and did NOT say
"do A before B", **the supervisor picks an order on its own judgement and
starts the next phase in the same turn**. Heuristic:

1. Items that unblock others first (e.g. an infrastructure phase before the
   UI phases that depend on it).
2. Items where the supervisor has the freshest context (cheap to keep
   running) before context-switching to a colder area.
3. User-facing pain (a reported bug) before purely architectural cleanup,
   when scope is similar.

Surface the choice in 1–2 sentences ("taking X next because it unblocks Y;
deferring Z to after") so the user can redirect if they want — but **start
the work immediately**, don't wait for permission.

**Ask is still appropriate** for:
- A major direction shift outside the named pool ("now pivot to W").
- A design call with multiple viable answers + serious downstream
  consequences (where getting it wrong wastes a full phase).
- The user said "what would you do here?" explicitly.

Saying "I'm picking X next" while starting the work is **not** the same as
"asking which to pick". The former gives the user a redirect handle without
forcing them into the loop on every transition.

---

## Anti-patterns

❌ **Stopping with "which next?" between phases when the task pool is
known.** If the user has already named multiple outstanding tasks and
hasn't said "this before that", the supervisor picks an order on its own
judgement and starts the next phase in the same turn (§ 7 "NEXT"). Polling
the user for priority between every phase turns the workflow into
turn-by-turn approval — that's not autonomous, it's just slower
non-autonomous. State the pick + start the work; the user can redirect if
they want.

❌ **Trusting sub-agent claims without verifying.** Sub-agent says "tests pass"
→ run `cargo test` anyway. "The feature works" → screenshot it.

❌ **Counting tests instead of reading them.** A green "+N tests" line means
nothing if N are tautologies (`assert!(true)`, `assert_eq!(x, x)`, asserting on
something the test itself just set). Spot-check 2–3 of the sub-agent's new
tests in the diff: do they make a non-trivial behavioural assertion on the
real code path?

❌ **Refactoring an untouched upstream crate "while I'm here".** Even
diff-minimal-looking renames raise the cost of future cherry-picks from
upstream. Default: bug fixes in-tree fine, additive patches fine, style
renames no. **Escape valve:** if a non-additive change genuinely pays
(splits a file that was already cumbersome, removes a class of bugs), the
supervisor can authorise it — FORK.md § "Working principles for upstream
modifications" decision 2. Once a file is listed in FORK.md "Notable
upstream file modifications", this rule relaxes (we already paid the
cherry-pick cost on that file). See ADR-0001.

❌ **Release builds for verification.** `cargo build --release` is 3–9 min, the
maintainer does release builds at finalize / bundle time. Agent verification is
debug-only. If a check genuinely requires release behaviour (optimizer-only
bug), pause and ask the user.

❌ **Piping a long cargo command to `tail` / `grep` and trusting the exit
code.** `cargo build … | tail` reports `tail`'s exit (always 0) — a failed
build looks "succeeded" and leaves stale binary. Use `set -o pipefail`, or pipe
to file and grep the file.

❌ **`pkill -f sawe` from the shell that's about to depend on the kill
result.** The pattern matches the current bash → exit 144. The kill happens
but the next command in the same chain sees noise. Prefer `kill <pid>` by PID,
or split the kill into its own command.

❌ **`pgrep -f "<pattern>"` in a watch-loop, where `<pattern>` literally appears
in the loop body.** `pgrep -f` matches on the full command line of every
process, *including the bash that's running the loop* — the bash inherits the
loop body as its `argv[2]`. So the loop matches itself, the exit-condition is
never satisfied, and the watcher polls forever after the real target has long
since exited. We hit this once with `until ! pgrep -f "cargo test -p remote_control_ui" > /dev/null; do sleep 2; done`
self-matching for **8h41m** after the cargo test had finished in 1m53s.
Two safe shapes:

  ```bash
  # 1. Wait on a marker line in the output file the target writes to —
  #    no pgrep at all, no self-match risk:
  until grep -q '^EXIT: ' /tmp/.../task.output; do sleep 2; done

  # 2. If you really need pgrep, exclude self by pid:
  until [ -z "$(pgrep -f 'cargo test -p remote_control_ui' | grep -v $$)" ]; do
      sleep 2
  done
  ```

❌ **Re-enabling a disabled subsystem to "fix" a missing feature.** auto_update
/ telemetry / collab / Zeta / native cloud LLM / upstream AgentPanel are
disabled deliberately (CLAUDE.md § "What's disabled"). They are not bugs.

❌ **Changing a locked rebrand identifier without user approval.** Display name,
CLI binary name, bundle IDs, URL scheme, config dir, GitHub repo,
attribution — see CLAUDE.md § "Locked rebrand identifiers".

❌ **Dispatching parallel write-phase sub-agents WITHOUT worktree isolation.**
Hard rule — if you call `Agent` ≥ 2 times in a single message, every one of
them MUST have `isolation: "worktree"`.

❌ **Categorical instructions an over-literal agent can mis-read.** A bare
"don't modify any files" → agent refuses `cargo test`. A bare "don't touch
INDEX" → agent refuses to tick plan-doc acceptance items. When you write a
prohibition, say what it **doesn't** preclude: "don't edit source/docs, don't
`git commit` — but DO run `cargo build/test/run`".

❌ **Skipping the finalize step.** Plan doc left at `in progress`, INDEX.md
out of date, screenshot missing → the next session starts from a stale state
and the confusion compounds.

❌ **Cycling bug fixes without root-cause investigation.** If you see "fix-1,
fix-2, fix-3" on the same area in `git log` — STOP. Dispatch a Plan agent to
read the code + find the root cause; don't dispatch yet another impl agent.

❌ **Carrying `target/` worktrees across sessions.** Auto-cleanup doesn't
trigger (target/ is populated). 5 stale worktrees = 50+ GB. Cleanup is
explicit, end-of-session sweep covers it.

❌ **Marking flaky tests `#[ignore]` to silence them.** Flaky tests almost
always reveal a shared-resource race (TCP port, file path, global static, env
var). Either fix root-cause in scope (`tempfile`, test-name-derived port,
`serial_test`'s `#[serial]`) or surface in REPORT with name + symptom. Silent
silencing erodes the suite.

❌ **Inline visual smoke-test in a sub-agent prompt.** Triggers a 30–60 sec
debug build + a 14-sec MCP boot per sub-agent dispatch, proves nothing the
supervisor's post-merge smoke-test wouldn't. Supervisor does the smoke-test
once after VERIFY.

---

## Cancel / pivot mid-flight

```bash
TaskStop({ task_id: "<agent_id>" })
# Sub-agent committed in its worktree → just don't merge.
git worktree remove -f -f .claude/worktrees/agent-<id>
git branch -D worktree-agent-<id>
# If the plan doc is already committed and we're cancelling the direction:
git revert <plan-commit-sha>
# Or: keep the plan doc but mark it "Status: cancelled — <reason>".
```

---

## Quick-glance checklist (before each dispatch)

- [ ] LIGHT vs HEAVY decided.
- [ ] HEAVY: plan doc written + committed (`plan: <slug>`).
- [ ] FORK.md "Touched upstream files" updated in plan if first-touch is expected.
- [ ] Sub-agent prompt has all 8 sections (MANDATORY / SCOPE / HARD RULES /
  CHECKS / DOCUMENTATION / COMMIT / REPORT).
- [ ] Parallel dispatch? → all calls have `isolation: "worktree"`.

After the sub-agent finishes:
- [ ] `git status` clean (sub-agent committed).
- [ ] cargo build/clippy/test all green on touched crates.
- [ ] UI change → MCP smoke-test + screenshot eyeballed.
- [ ] HEAVY → plan doc finalized, INDEX.md row added, finalize commit.
- [ ] Stale worktrees cleaned up (or queued for end-of-session sweep).

---

## Doc layout

The supervisor's bookshelf — start every session at `docs/INDEX.md`:

| Path | What lives here |
|---|---|
| `docs/INDEX.md` | Bookshelf: tables of plans, ADRs, findings, module-docs status. Always start here. |
| `docs/workflow/supervisor-mode.md` | This file — the supervisor's playbook. |
| `docs/workflow/doc-discipline.md` | Decision tree for "where do I write this?". |
| `docs/workflow/adr-template.md` | ADR template. |
| `docs/architecture/decisions/NNNN-<slug>.md` | ADRs (architectural decisions with long-term consequences). |
| `docs/findings/YYYY-MM-<slug>.md` | Short, dated discovery notes (one fact each, no fluff). |
| `docs/plans/YYYY-MM-DD-<slug>.md` | Plan docs for HEAVY-track work (committed to the repo so sub-agents in worktree can read them). |
| `docs/superpowers/{plans,specs}/` | **Personal / local drafts** — gitignored. Use for in-progress ideas before they're polished into a `docs/plans/` doc. |
| `CLAUDE.md` | Hard rules + fork philosophy + locked rebrand identifiers. Always in context. |
| `FORK.md` | Touched-upstream-files registry + fork-local crates list + numbered architectural decisions. Edit when first-touching an upstream file. |

`docs/findings/` and `docs/architecture/decisions/` start empty — we don't
pre-populate templates (premature scaffolding rots). New entries appear as
the workflow is exercised.
