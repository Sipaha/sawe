# Session handoff — 2026-08-27

**Phase 2a of the Solution band layout is COMPLETE and pushed through
`de19ba80af`.** This session picked up at Task 7 (the previous handoff
paused after Task 6), shipped Tasks 7, 7b and 8, then ran a whole-branch
review over the session's own range and landed its fix wave.

Supersedes `findings/2026-08-26-session-handoff.md` for everything after
`63d7f6032c`. The SDD ledger
(`.superpowers/sdd/2026-08-26-solution-band-layout/progress.md`) is
gitignored and deleted — everything durable from it is below.

Plan: `docs/plans/2026-08-26-solution-band-layout.md`.
Spec: `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md` §2–§4.

---

## Commit chain since `63d7f6032c`

**Task 7 — draggable divider + per-Solution persisted band geometry**
- `3d98ab815b` divider is draggable; `{divider_ratio, utility_visible,
  active_dialog_session}` persist per Solution in the agent DB
  (new `crates/solution_agent/src/db/band.rs`)
- `1e759b3a24` fix round: no-op `or_default()`, a double-lease guard test
  for the `ctrl-\`` path (incl. the plain-folder/non-Solution window),
  debounce tests, doc corrections

**Task 7b — cold-hydrated sessions reach `by_solution`** (inserted bug fix,
not in the original plan)
- `a5954bcf86` hydration now indexes cold sessions into `by_solution`, so
  the status-bar session tab strip survives a restart
- `6c29b8b63f` **gate the orphan-member purge on the session being live**
  (see the section below — this is the most important thing in the session)
- `5b23ee5e16` correct what that gate actually guarantees (docs only)

**Task 8 — status-bar cleanup, sweep, docs correction pass**
- `3861a2adfa` delete the `<Solution> · N projects` and `AI: N` status-bar
  indicator types outright (one consumer each)
- `e3eb066ce7` sweep fixes: 4 clippy findings from this plan's own commits,
  8 rustfmt-dirty files, and a defect the verification screenshots exposed
  (the band's utility half painted a transparent slab with no terminal in
  scope)
- `2ccff0f3af` FORK.md decisions #91–#93 + docs
- `8e6f1c4db2` fix round: FORK.md self-contradictions and misdirections,
  plus `/.superpowers/` into `.gitignore`

**Final whole-branch review fix wave** (8 commits, one per finding)
- `95a4d5523a` **Important**: a band mutation racing the DB open
  permanently discarded that Solution's persisted geometry
- `965a9d2936` say what `close_session`'s flush actually does
- `91c3a1f6ef` scope the orphan-purge gate's claim to its own loop
- `1cc929c9f3` stop `dump_visual_structure` inventing title-bar children
- `04dd2f4a07` only select dialogs the strip can tab
- `33baf3796a` document what the AI badge actually counts
- `564006b9a7` restore model + effort on the cold hydration path
- `a5d74aafbb` delete the hydration paths nothing calls
  (`restore_open_tabs`, `hydrate_open_tabs_lazy` + their tests)

**Tail**
- `1cbaf9940d` three docs the band work outdated (incl. one the fix wave
  itself introduced)
- `de19ba80af` rustfmt on `hydration.rs` — `cargo fmt --all --check` was RED
  after the fix wave while every per-crate check was green

**Final gates:** `cargo test -p solution_agent -p console_panel -p
solutions_ui -p solutions -p workspace` → 1175 passed / 0 failed.
`cargo fmt --all --check` clean. `./script/clippy` over the plan's crates →
exit 0 (see the clippy gotcha below for the unscoped run).

---

## READ THIS FIRST — the orphan-purge backlog is real and now visible

Fixing the empty-strip bug (`a5954bcf86`) had a side effect nobody planned:
**it woke `gc_orphan_members`, a destructive GC that had been a silent
no-op.** That function reads `by_solution`; hydration had never populated
it, so the `SolutionStoreEvent::Opened` handler's documented `hydrate → gc`
sequence had done nothing since the day it was written. Indexing cold
sessions made every one of them a purge candidate.

`gc_orphan_members` (`crates/solution_agent/src/store/teardown.rs:257`)
keeps a session only if its `cwd` is exactly the solution root or sits under
a **current** member path. Checked against read-only copies of the
maintainer's real databases (`~/.spk/sawe/data/solution_agent/
solution_agent.db`, `data/db/0-stable/db.sqlite`): Solution 14 (`Sawe1`) has
two members today (`sawe`, `spk-editor-mobile`) but its open sessions
include **13 with `cwd` under `…/Sawe1/spk-editor`, 2 under `spk-cockpit`,
2 under `spk-mail`, and 1 rooted in another Solution's directory** — ~18
orphans by that rule. Solution 6 has one more. Shipping `a5954bcf86` alone
would have hard-purged all of them on the next open of `Sawe1`: six DB
tables in one savepoint (`db.rs:584-605`) plus
`remove_dir_all(<root>/.agents/<sid>)` (`teardown.rs:44-53`), with no
confirmation, no undo and no log line. And it is not scoped to the member
that triggered it — `gc_orphan_members` purges *every* orphan it can see, so
one member removal in `Sawe1` would have taken all 18.

`6c29b8b63f` gates the purge on `acp_thread().is_some()`
(`teardown.rs:391`). That restores the pre-commit blast radius exactly
(before `a5954bcf86`, everything in `by_solution` had been put there by
`create_session_with_parent` or `resume_session`, i.e. was live by
construction) and kills the rename-during-in-flight-hydration false-orphan
race at the same time. Cold orphans now emit a `log::warn!` at
`target: "solution_agent::gc"` (`teardown.rs:317`) instead.

**The backlog still exists in the database and is now printed in the log.**
That is a decision waiting for the maintainer, not a solved problem. The
reversible way to retire one of these is to **close the chat** —
`close_session` is a SOFT close; `purge_session_hard` is the irreversible
one. Note the warn fires per cold orphan on **every** `Changed` **and**
every `Opened`, so a `Sawe1` open currently prints ~18 lines (a
once-per-process dedupe is in the deferred list).

Two things the gate deliberately does **not** cover, recorded in its own
doc comment so nobody reads it as "stale cwds are now safe":
`reset_context` warms a cold session (`store.rs:2945`) without touching
`s.cwd`, so `/clear` on a legacy orphan makes it purgeable where it was not
before (harmless — the user cleared it first); and `respawn_agent`
cold-izes a live session (`store.rs:2277`), so a `Changed` in that window
logs where it used to purge (safe).

---

## Outstanding task pool, in priority order

### 1. ~~`RunController::run` double-lease crash~~ — FIXED 2026-08-27 (`4038606620`)

**Do not re-implement this.** The Terminal branch of `RunController::run`
aborted the whole editor with a GPUI double-lease panic, reachable from the
ordinary UI Run button (`alt-shift-f10`, `run_config::Run`) **and** from
`run_config.run` over MCP. Pre-existing since the 2026-06-24 refork; task 6
neither introduced nor worsened it.

The fix needed **two** halves, and the prescription this document previously
carried ("`run`/`rerun` must take `&mut Workspace` from their callers") was
only the first — shipping it alone still aborts the editor:

1. `RunController::run` is an associated function taking the caller's
   `&mut Workspace` + `Context<Workspace>`; the controller-only work moved
   into `prepare_run`. `rerun` was deleted (zero callers).
2. `ConsolePanel::spawn_task` is called from inside the completion poller's
   async body, because it reads its **own** `WeakEntity<Workspace>`
   synchronously and so panics under the caller's lease even with half 1
   applied. Verified by re-introducing each half separately.

Full record, including the failure output for each half and the live
headless verification: `docs/findings/2026-08-26-run-controller-terminal-double-lease-crash.md`.

The task-8 verification item this blocked — "run-configuration output lands
in the band's terminal" — is now confirmed: a task's terminal tab paints in
the Solution band's utility section on a live headless instance.

### 2. Plan and execute phase 2b

Scope: GitGraph and Debug move into the band's **utility section** (joining
the terminal), the vertical dock button strips are deleted, and their
buttons are relocated "by geometry" per the spec. Carries these already-made
findings:

- **`DebugPanel` is the most coupled of the three** — budget for it first.
- The band's utility slot is a **type-erased** `Workspace` slot
  (`solution_band_utility_item`, FORK.md #91) precisely to break a
  `solution_agent`⇄`console_panel` crate cycle. New occupants go through
  the same slot.
- The slab-background fix from `e3eb066ce7` sits on `ConsolePanel`'s root,
  not on the band's `half()`, so GitGraph and Debug will each need the same
  line — painting the half once would cover all occupants, but that is a
  design change, not a doc fix.
- **The band has no height of its own** (see the deferred section) — this is
  a 2b task in its own right, and arguably the first one.
- Spec §3's "clicking the active utility button hides the section" and the
  `ctrl-shift-a` dialog-toggle hotkey were both deferred here (rulings
  below) because neither has a button to hang off until 2b.
- The utility slot is painted by `SolutionBand::render`
  (`solution_band.rs:123-131`), **not** by `Workspace::render`. FORK.md said
  the latter until `8e6f1c4db2`; a `grep Workspace::render` would find
  nothing.

### 3. Phase 3 — the git panel

The git panel becomes `Changes | Commit`; History is removed and the graph
loses its inline commit-details subpanel. Spec exists; no plan yet.

### 4. The deferred backlog below.

---

## Deferred / parked backlog

### `entries_persist_chain` is cancelled wholesale on every session teardown

`evict_session_runtime_maps` drops `entries_persist_chain[id]`
(`teardown.rs:463`) in the same synchronous block in which
`persist_all_rows` parks its DB body in `cx.spawn` (`store.rs:3606-3627`),
cancelling the task **before its first poll**. Persist is not debounced at
all (the 500ms/2s throttle in `store/acp_event.rs` governs only the MCP
emit), so a window close discards genuinely in-flight incremental persists,
not merely an un-debounced tail. `cold_close_solution`'s documented promise
to "capture any un-debounced tail so a reopen restores the full
conversation" has therefore **never** been met, and the same drop sits on
`close_session` and `purge_session_hard`.

This was proven, not inferred: an implementer probed a live session
(`main_len == 0`, 3 rows → 3 rows), then a re-reviewer established causation
by commenting out the single `remove` line, after which the probe returned 0
rows. A tripwire test is in place that will fail `left: 0, right: 2` the day
the cancellation is repaired.

**Do not apply the naive fix.** Letting the flush run is exactly what arms
`delete_entries_from(main_len)` against legacy row layouts — the destructive
change the Task 7b review existed to prevent. This needs its own task with
the legacy row-layout question settled first. Note also the asymmetry:
`cold_close_solution` gates its flush on `is_live` (`teardown.rs:388-393`),
**`close_session` does not** (`teardown.rs:636`) — the gate is the right
predicate for the day the cancellation is fixed, and only one site has it.

### The band has no height of its own

The band is content-driven. With an empty session it is roughly 128px tall,
of which the transcript region is about 30px — so **phase 2a's primary
surface currently ships as a status row and a compose box with no
conversation view**. With a long transcript it would grow against the
project zone unbounded. The spec never specifies a band height or a
*horizontal* drag handle between the project zone and the band; it specifies
only the *vertical* divider inside the band, which is what Task 7 built.
This is a 2b task and it is the one a user would notice first.

### Smaller deferrals

- **Band persistence residual**: a failing `load_band_states` still lets the
  flush write defaults. `95a4d5523a` fixed the racing case with a field-wise
  touched-mask plus write suppression until hydration lands, giving the
  statable guarantee *a persisted field is lost only if the user set that
  field this run* — the load-failure path is outside it.
- **`persist_band_state_now` runs unconditionally on an unchanged
  selection** (idempotent DB churn).
- **`band_state_writes` entries are never reaped** (bounded by the number of
  Solutions dragged in a process lifetime).
- **The band's flex-basis overshoots by the divider's 1px** — identical to
  the split-diff precedent it copies.
- **A divider drag in the last 400ms before process exit is lost** —
  accepted trade-off of the debounce ruling.
- **`BandStateChanged` reaches catch-all subscribers**: `status_item.rs:16`
  and `solutions_ui/src/solution_tab_strip.rs:66` both subscribe with a bare
  catch-all and fire on every one. Harmless (GPUI dedups `Effect::Notify`
  per `EntityId` per frame and the window repaints during a drag anyway),
  but the code no longer claims otherwise.
- **`solution_agent.get_session` over MCP now returns `session_not_found`
  after a window close** until the client calls `list_sessions` — cold
  sessions used to stay resident for the process lifetime. This is the one
  place `a5954bcf86`'s leak fix is observable on the wire.
- **`gc_orphan_members` warns per cold orphan on every `Changed` and every
  `Opened`** — a once-per-process dedupe would keep the log readable.
- **`solutions::mcp::visual_structure` emits no node for the Solution band
  or the project-toolbar row at all.** `1cc929c9f3` fixed the related lie
  (it was synthesising a `Branch` child under `TitleBar` although decision
  #27 moved the branch widget into the ProjectToolbar row, and a
  `SolutionsStatusItem` node for UI that no longer exists), but the two
  missing nodes are a code change, not a doc fix.
- **`SolutionBand::solution_id()` / `resolve_active_session()` re-walk the
  workspace's worktrees every render.** Fine at 2a's scope; revisit if 2b
  adds render-heavy work.
- **`cold_close_solution` evicts sessions without emitting `SessionClosed`**,
  so the band's view cache would not evict through that path. Benign only
  under one-window-per-Solution — if that invariant ever changes, this leaks
  editor-bearing views.
- **`ui::ContextMenu` nesting** — the session-tab overflow popover uses a
  hover+click Select while visible tabs are one click. Proven unfixable
  short of reworking shared `ui::ContextMenu` internals (see gotchas).
- **Narrow the un-park trigger further / observability on the `StopReason`
  wildcard.** `84efa8ba78` narrowed supervisor un-parking to stop reasons
  that prove the API responded, but `StopReason` is `#[non_exhaustive]`
  (agent-client-protocol-schema 0.13.6) so a literal exhaustive match is
  impossible from this crate and the wildcard defaults to "not proven". A
  `debug_assert!`/log on the `_` arm would make an unrecognised variant
  observable instead of manifesting months later as "parked supervisors wake
  later than expected". Recorded as a trap in FORK.md for whoever next bumps
  the ACP schema.

---

## Rulings a future session must not silently re-litigate

**Layout / band**

- **Phase 2 is split into 2a and 2b.** §2–§4 as one plan is ~14 tasks. 2a
  ships the working sandwich for the primary case (dialog + terminal); 2b
  generalises the utility section to three contents and does the button
  relocation. The intermediate state — band and bottom dock coexisting,
  vertical strips still rendering — is deliberate.
- **There is no separate `dialog_collapsed` flag: collapse IS
  `active_dialog_session == None`.** `session_tab_strip::toggle_selection`
  already returns `None` when the active tab is re-clicked, which is exactly
  spec §3's collapse rule. A second flag is a duplicate source of truth for
  one bit and invites desync. The persisted set is therefore
  `{divider_ratio, utility_visible, active_dialog_session}`.
- **Band state is per-Solution, stored on `SolutionAgentStore` and persisted
  in `SolutionAgentDb`, with a view-local fallback when the workspace
  resolves no Solution.** A plain-folder (non-Solution) workspace is a
  supported case in this fork and its terminal works today; keying the
  visibility bit solely on `SolutionId` would make `ctrl-\`` a no-op there.
  There is a regression test for exactly that window shape.
- **`SolutionBand` resolves its Solution off the `Entity<Project>`, not
  through the `Workspace` entity** (FORK.md #92). `set_utility_visible` /
  `toggle_utility_focus` run under a live `&mut Workspace` borrow; the
  obvious path compiles clean and double-lease-panics at runtime. Two tests
  guard it — both were confirmed to panic when the resolution is pointed
  back at `self.workspace.upgrade()?.read(cx)`.
- **The ratio's DB write is debounced (400ms, cancel-on-replace `Task`), not
  per-drag-move.** The split-diff precedent writes a process Global (free);
  ours writes SQLite.
- **Spec §3's "clicking the active utility button hides the section" is not
  implementable in 2a** — there is no utility/section button anywhere yet;
  they are 2b's "relocate by geometry" work. `ctrl-\`` provides
  hide-when-focused via `toggle_utility_focus`'s tri-state.
- **The `ctrl-shift-a` dialog-toggle hotkey is deferred to 2b.** It is the
  one collapse path that *would* need "remember the last session" state,
  i.e. the field the no-`dialog_collapsed` ruling removes; 2b answers the
  restore-target question once for both.

**Behaviour / correctness**

- **A new surface conforms to the existing one, not the reverse.**
  `status_row`'s state-dot colouring is shipped behaviour the maintainer
  sees daily. Sharing the colour *function* is not sharing the *decision* —
  the two call sites originally fed it different `is_running` inputs, so a
  `Stopping` session (up to ~40s after `cancel_turn`) read active in one
  place and idle in the other. `Stopping` was dropped from the tab strip's
  colouring input and **kept** in its separate close-busy check. Making
  `Stopping` Accent everywhere is now a deliberate one-line change at one
  shared site.
- **The orphan-purge fix is a liveness gate, not a mode flag.** Gating on
  `acp_thread().is_some()` restores the exact pre-commit blast radius while
  keeping the tab-strip fix; a mode flag would have added a second thing to
  get wrong.
- **The English close-confirmation prompt stays.** `console_panel` is the
  only crate in this fork carrying Russian UI strings; `git_ui` and
  `solutions_ui`, the closest analogues to the new surface, have none.
- **The overflow submenu stands.** Nesting a `right_click_menu` inside an
  open popover genuinely does not work — proven live, twice.

**Process**

- **The final whole-branch review was scoped to `63d7f60..HEAD` (tasks
  7/7b/8)**, not the plan's full base: tasks 1–6 each had their own review,
  and the full 2a range is a ~380KB diff no single reviewer reads carefully.
  Cross-task coherence on `solution_band.rs` (T4 → T6 → T7) was judged by
  reading the code at HEAD, and it reads as one design.
- **The workspace-wide `./script/clippy` is RED on pre-existing debt in
  crates this plan never touched, and clearing it is not this plan's job.**
  See gotchas.

---

## Active gotchas

- **`mcp__sawe__*` tools drive the maintainer's LIVE release editor**, not a
  test instance: real Solutions, the `release-fast` binary at
  `~/.spk/sawe/state/mcp.sock`. Using them to "verify" mutates the running
  workspace *and* photographs stale code. Launch your own
  `script/run-mcp --debug --headless` and drive the raw socket — after
  `cargo build --bin sawe`, because `run-mcp` only compiles when the binary
  is *missing*.
- **`script/run-mcp --runtime-dir` does not isolate the MCP socket.** It
  isolates via `XDG_*`, but `paths::base_dir()` only honours
  `--user-data-dir`, so the socket lands under `$HOME` and the script times
  out waiting for it. Use `--user-data-dir`, or invoke the binary directly.
- **The harness's `<new-diagnostics>` blocks are stale mid-edit snapshots.**
  Ten times across this plan — including one that claimed a type was missing
  from `store.rs` while `cargo check -p solution_agent --all-targets`
  returned 0. Always confirm with `cargo check --all-targets` before
  believing them or opening a fix round.
- **GPUI double-lease**: reading the `Workspace` entity under a `Workspace`
  lease panics at runtime, compiles clean, and unit tests on the
  `AnyWindowHandle::update` shape miss it entirely. Hit again in Task 7, and
  it is the root of the outstanding `RunController` crash.
- **`ui::ContextMenu` will not nest a `right_click_menu` inside an open
  popover.** `ContextMenu::build/new` unconditionally wires
  `cx.on_blur(focus_handle) -> cancel() -> DismissEvent`, and
  `right_click_menu`'s two-frames-deferred focus grab blurs the *outer*
  popover, tearing down its whole deferred child tree — inner menu included
  — before that inner menu renders. The one escape hatch
  (`ignore_blur_until`) is private with no public setter. Root cause is
  documented at the call site in `session_tab_strip.rs`.
- **`.superpowers/` is now in `.gitignore`** (`/.superpowers/`, line 67 —
  `/docs/superpowers/` at line 64 never covered it). A `git add -A` used to
  commit the whole SDD workspace.
- **`./script/clippy` unscoped is RED on pre-existing debt**:
  `crates/denoise/src/lib.rs:121` (`while_let_loop`, last touched by the
  2026-06-24 refork) and then seven findings in `git_ui` (`panel_buttons.rs`
  unused import + three never-used fns, `git_panel.rs::
  build_commit_message_prompt` never used, `Iterator::last` on a
  `DoubleEndedIterator`, an explicit `.into_iter()`). All predate this
  session; **the `git_ui` ones look like the maintainer's own work in
  progress**, so do not "clean them up" without asking. The scoped run over
  the plan's crates (`-p workspace -p solution_agent -p console_panel -p
  solutions_ui -p solutions -p git`) is exit 0.
- **`cargo fmt --all --check` catches slips the per-crate checks do not** —
  it was RED after the fix wave when every crate gate was green
  (`de19ba80af`). Run it before declaring a plan finished.
- **`on_drop` never fires under a drag handle** wrapped in `deferred()` +
  `block_mouse_except_scroll()` (FORK.md #84) — store position from
  `on_drag_move`, as both the split-diff divider and the band divider do.

---

## Process note worth keeping

Four times on this plan an agent came back having **disproved part of its
own brief**, and was right every time: `restore_open_tabs`'s missing caller
was a pre-existing gap and not a Task 5 regression; one of the controller's
four FORK.md corrections was already fixed and another was understated; a
`persist_all_rows` gate the controller demanded turned out to guard a flush
that never reaches disk at all; and two of the final review's eight findings
were wrong and were disputed with evidence. Keep giving agents explicit
permission to return a documented negative result instead of a fix.
