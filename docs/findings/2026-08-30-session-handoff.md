# Session handoff — 2026-08-30

Supersedes `findings/2026-08-27-session-handoff-2.md` for everything after
`8bed974075`. That file remains the record for the band's own height and
**phase 2b**; this one covers **phase 3 (the git panel), which is now
COMPLETE**, plus three items cleared out of its backlog.

Plans: `docs/plans/2026-08-30-git-panel-commit-tab.md` (done),
`docs/plans/2026-08-30-entries-persist-chain-teardown.md` (done).
Spec: `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md` §5.
**All three phases of the sandwich redesign are finished.**

---

## Where the work stands

**Phase 3 shipped whole.** The git panel's tabs are exactly **Changes |
Commit**. History is deleted. The git graph lost its inline right-hand
commit-details sidebar — whose `min_w(px(300.))` was the only min-width in the
view, and removing it is what makes the graph viable in the compact utility
half phase 2b moved it into. Selecting a commit in the graph opens a closable
Commit tab with the full message, a `short hash · author · date` row,
whole-commit +/− totals and a changed-files tree; double-clicking a file opens
that file's diff **for that commit** in the centre pane; a multi-row selection
renders "N commits selected".

**Seven backlog items also cleared:**

- **`ctrl-\`` now leaves the caret in the band's active terminal.** It used to
  focus the `ConsolePanel`'s root handle, so typing after the hotkey went
  nowhere until the user clicked in. Closing the focused tab now re-homes focus
  inside the console instead of ejecting to the centre-pane editor.
- **A soft close no longer discards in-flight transcript writes.** This was a
  silent, permanent data loss on every tab and window close.
- **An entry-row flush is now one batched write.** 200 rows went from 407
  executor turns to 7, and from 38 observable partial row sets to zero.
- **The git graph has a keybinding again.** `ctrl-alt-\`` → `git_graph::ToggleFocus`,
  driving the band's three-leg tri-state. Its dock toggle died with its `Panel`
  impl in phase 2b, leaving the unhideable status-bar button as the only path.
  Fixing it exposed a second bug: `GitGraphPanel::focus_handle` returned the
  *inner graph's* handle and the panel's own handle was never `track_focus`'d,
  so the tri-state could never reach its hide leg — and `set_active_repo`
  ejected focus to the centre pane on every member switch. 44 lines of keymap
  entries naming actions that exist nowhere went with it.
- **With vim on, typing into the git graph's search box drove the commit
  list.** `vim.json` binds bare `j` / `k` / `shift-g` / `g g` under
  `"GitGraph && !GitGraphSearchBar"` with no `vim_mode` gate, and **nothing
  emitted `GitGraphSearchBar`** — so `shift-g` jumped the list to the oldest
  commit instead of typing a `G`. The negated clause was a missing emission,
  not a dead leftover.
- **The Solution band and the project-toolbar row are now first-class nodes in
  the MCP visual dump.** Every band check this session had to go through
  screenshots because the structured dump did not mention them.
- **App quit now drains the entry-persist chain instead of cancelling it.**
  Quitting mid-turn silently truncated the tail of the conversation.

Everything is on `origin/main`. Working tree clean.

---

## The four things a future session must not re-derive

### 1. The crate dependency edge decides the whole phase-3 architecture

`git_graph` depends on `git_ui`; `git_ui` does **not** depend on `git_graph`.
So graph→panel is a **direct typed call** and panel→graph is a **GPUI event**.
Neither direction needs the string-named-action IoC trick, and the reverse
direction is impossible, not merely discouraged. Full write-up in FORK.md #100.

Two details that are load-bearing and non-obvious:

- The graph's push is deferred through `cx.defer_in`, because `select_entry` is
  reachable from `invalidate_state` and the deserialize path, where a
  synchronous `workspace.update` double-leases.
- `Event::CommitTabClosed` carries `Vec<Oid>`. The event reaches **every**
  `GitGraph` in the window, so one graph's close must not deselect another's.
  A payload-less version shipped first and was caught in review.

`show_commit_selection` takes a `CommitSelectionSource { UserGesture,
Background }` because a background re-anchor — a `git fetch` landing in a
terminal — was yanking the panel off Changes while the user typed a commit
message. A re-anchor that *fails* (a `commit --amend`) closes the tab instead
of stranding it on a commit that no longer exists.

### 2. Deleting History opened a command-palette hole, and one reader escaped the fix

Narrowing `dispatch_context` makes a tab inert to the **keymap**, but `.on_action`
registrations are independent. The fix unregisters 20 selection-scoped actions
while the Commit tab is active. **`git::FileHistory` escaped it**, because
`git_graph` registers that action on the *Workspace* element and resolves its
target through `GitPanel::selected_file_history_target` — the read is on the
caller's side of the seam, so a guard living on the panel's own registrations
cannot see it. That method now returns `None` off the Changes tab.

**The class to watch:** any cross-crate reader of a panel's selection bypasses
a guard that lives on that panel's registrations. One more of the same shape is
known and deliberately left: `GitPanel::select_entry_by_path`, called from
`project_diff.rs` on every local selection change, is a background sync and is
documented as deliberately un-guarded.

### 3. The persist chain: retention, not detachment

`entries_persist_chain` serialises per-session entry-row writes because GPUI
detached tasks have no FIFO guarantee. Each link moves the previous `Task` into
its own future, so **dropping the map entry cancels the entire chain**, not just
the last link.

Disposal is now stated by each caller (`ChainDisposition::{Drain, Abandon}`),
never inferred inside `evict_session_runtime_maps` — because `purge_session_hard`
evicts *before* it deletes, so a generic detach resurrects rows for a session
that no longer exists in `solution_sessions`, and nothing enumerates entry rows
without a session row, so those orphans are invisible and never reaped.

**`Drain` retains the chain under its key rather than detaching it.** Detaching
shipped first and was a **Critical**: freeing the map key lets a reopened
session build a second chain with nothing ordering it against the first, so the
old flush's trailing trim lands after the new chain's tail upsert and deletes
the reopened session's new message. Measured 4 rows before, 3 after. A drained
chain therefore **deliberately outlives its session**, and only a *spent* chain
may be reclaimed — each link flips an `Arc<AtomicBool>` as its last act, and the
sweep `retain`s on it. Full write-up in FORK.md #101.

`close_session` needed its liveness gate **in the same change**: the two bugs
point in opposite directions — one is loss of writes that should happen, the
other is execution of a rewrite that should not.

### 3b. GPUI's quit contract: only background work can finish

`App::shutdown` invokes each quit observer synchronously (entities and globals
alive, windows **not yet** cleared), collects their futures, clears windows,
runs `flush_effects()`, then **blocks the main thread** on those futures for a
200ms `SHUTDOWN_TIMEOUT`. The block passes the **foreground session id** down to
the scheduler, so that session is blocked for the whole quit window. On Linux it
is stronger still: the quit callback runs only after the event loop has already
returned, so the main-thread channel is not merely un-drained — there is no loop
left to drain it. **Awaiting a foreground `Task` from a quit observer can only
burn the 200ms and lose the write.**

That is why the persist chain moved from `cx.spawn` to `cx.background_spawn`.
The chain never needed the foreground: a link captures no entity, ordering comes
from `prev.await` rather than executor FIFO, and every `db.*` op was already a
background task.

The second half is subtler and cost an extra round: the quit hook must take the
chain map **on the future's first poll**, not at observer registration time —
otherwise the flush that `App::shutdown`'s own `windows.clear()` + `flush_effects()`
triggers (window release → `SolutionStoreEvent::Closed` → `cold_close_solution`)
lands in a map the hook already took. The trap in the obvious alternative:
`shutdown(&mut self)` holds the `App` `RefCell` mutably borrowed while it blocks,
so an `AsyncApp::update` inside the quit future panics. Full write-up in
FORK.md #103.

**Of 14 `on_app_quit` registrations in the tree, `MultiWorkspace` is the only
unconditionally foreground-bound one** (defended on the main route by an explicit
foreground pre-flush in `zed::quit`; the residual exposure is a platform-initiated
quit). `LspStore::shutdown_server` is a conditional second.

### 4. The legacy row layout is intended behaviour, not a hazard to avoid

This blocked the persist-chain item for two sessions on a false premise. There
is no schema difference, no migration, no version column: a "legacy" row set is
simply one written by a pre-2026-07-06 build, at global flat indices. Cold load
detects it by count and **deliberately arms the same realign**, with a green
test asserting the teammate rows are deleted. Repairing the flush does not
invent a truncation; it extends an accepted one. The gate we actually wanted is
"don't rewrite a session the user never touched", which is what the liveness
check already did.

---

## Commit chain since `8bed974075`

**Phase 3** — `6233753d7d` (plan), `509cf04ee8` (relocation), `85174e727e`
(the Commit tab), `11b0a6af1f` + `1a6699868f` (its review wave), `6e37650b31`
(graph↔panel wiring), `cff98212ff` (its review wave), `8d3b6acdaf` (sidebar
deleted), `8093f946e0` (History deleted), `44312c93d8` (palette guard),
`193690aea6` + `5856d346eb` (final review wave), `63ba1de90a` `15acd407ce`
`7c010e3cf9` (the three defects live verification found), `a4c455031e`
(visibility sweep), `21542b820c` (FORK.md #100 + INDEX), plus the docs commits
`96524f2bf5` `91cccb6592` `7f84d9617a` `3ec992a49f` `62a6a0425f` `b4d7f666ef`
`97032cfca0` `51c7889898` `561960506a`.

**Console focus** — `41fbf8f3c9` `e076d6fc7d` `2819c6b1bd` `bb29bcc7cc`.

**Persist chain** — `53d8acb420` (plan), `76be3e00fa` (disposition split),
`f851e02f97` (the Critical fix), `92c52062e9` `0c6c1e871e` `b521d71ce6`
(FORK.md #101 + follow-ups), `06028b311e` + `b43d1cbd3a` (batching),
`4373927a9c` + `2a2738234f` (its review follow-ups).

**Graph keybinding** — `114e2b3e79` `18eaec348f` `a90ade3341` `5ac3b50703`
`63c3182eba`, plus `3aed2b44d6` + `7a78c52751` (the vim search-box fix and
FORK.md #30's addendum).

**Band nodes in the visual dump** — `fde3747183` `ff8f3c639a` `78f2b3d136`
`71c219fe53` (FORK.md #102), then `9ce7855b6c` `4eb90999f9` `4e1e6c3152`
`f2accdbedb` (its review follow-ups, including the `run-mcp --runtime-dir`
fix below).

---

## Open pool, in priority order

1. **Backlog, none urgent.** `DebuggerSettings.dock` / `.button` are inert with
   their UI controls removed but the fields kept. `running.rs::handle_run_in_terminal`
   is a second, independent embedded-terminal mechanism inside the debug
   session's own sub-pane — if the band shows Terminal when an adapter fires
   `runInTerminal`, that tab lands invisibly in the unshown Debug pane
   (confirmed during the console-focus work: it builds its own `TerminalView`
   and never reaches `reveal_utility_section`). A remembered
   `last_dialog_session` id that fails validation lingers until the next real
   selection. `solution_agent.get_session` returns `session_not_found` after a
   window close until `list_sessions` re-hydrates.
2. **Deferred from the two plans**, with the reasoning already written down so
   nobody re-derives it — see each plan's deferred list:
   - FORK.md #55's three-changed-files-trees extraction trigger has now fired.
   - The git graph's `0.13` Date-column fraction: every row truncates to
     `15 Nov 2023 06…` while Author is mostly whitespace. No horizontal scroll,
     no table min-width, and a ≥72px graph gutter floor.
   - `SerializableItem::serialize` indexes `graph_data.commits` with the **view**
     index and no `view_to_data_idx` — an off-by-one whenever the synthetic
     "Local Changes" row is present.
   - Three keymap entries name `git_graph::{FocusNextTabStop,
     FocusPreviousTabStop, ScrollDown, ScrollUp}`, actions that exist nowhere.
   - No test drives `select_entry` under a live `Context<Workspace>` lease, so
     the `cx.defer_in` is protected only by reasoning.
   - `Copilot::shutdown_language_server` awaits an async block whose output *is*
     the `Option<impl Future>` from `LanguageServer::shutdown` and **discards
     it**, so the LSP shutdown exchange is never driven — and `io_tasks.lock().take()`
     runs eagerly, leaving the server to die by pipe closure. Found while
     auditing quit hooks; real, unfixed.
   - Abandoning a chain cancels it only best-effort: cancellation walks inward
     one runnable at a time, so an 8-link chain leaks 5 orphan rows. Unreachable
     today (a purged session id is never re-hydrated) but real.
   - `solution_agent`'s database opens via `Connection::open_file` under its own
     mutex rather than sqlez's `ThreadSafeConnection`, which is the only place
     `journal_mode=WAL` / `synchronous=NORMAL` get set — so it runs on sqlite
     defaults. The batching already cut its fsync traffic ~200×.

**Do not** clean up the ~18 legacy orphan sessions in the maintainer's database
— unchanged, still the maintainer's call, and the GC that would purge them is
deliberately gated on liveness with cold orphans logged instead.

---

## Active gotchas (additions to the previous handoff's list)

- **The harness's `<new-diagnostics>` blocks were wrong more than a dozen times
  this session**, including a full wall of `E0061` "takes 4 arguments but 3 were
  supplied" errors and a phantom `unused import: RepoPath`, both while
  `cargo check` returned 0. Always confirm with a real check.
- **Mutation-test in a `git worktree`, never in the shared checkout.** One
  reviewer mutation-tested by writing to `git_graph.rs` on `main` while several
  agents were live. It restored cleanly, but a concurrent commit would have
  baked the mutation into someone else's diff. Every later round used a
  worktree.
- **`script/run-mcp` only compiles a *missing* binary.** Three separate agents
  launched a stale one this session; one of them noticed only because the binary
  predated a commit by 19 minutes. `cargo build --bin sawe` first, every time.
- **Re-issuing `solutions.open` between headless probes resets focus.** Two
  agents chasing focus bugs read that as a failure of their own fix. Do the
  whole sequence in one connection.
- **`script/run-mcp --runtime-dir` did not isolate anything, and was actively
  dangerous — now fixed.** It set only `XDG_*`, which nothing in this fork's
  path chain reads (`paths::base_dir()` is `home_dir()/.spk/<channel>` and
  `home_dir()` honours only `SAWE_HOME`). Worse, that branch *skipped* the
  `export SAWE_HOME="$HOME"` its sibling does, so the script waited on a socket
  the editor never bound **while the editor it launched contended the
  maintainer's real lock**. Two agents collided on that lock this session. It
  now exports `SAWE_HOME` properly; `--skip-onboarding` was inert twice over
  (wrong DB path, and the branch it targeted no longer exists — Welcome is this
  fork's launcher for every cold launch) and now exits 2 with an explanation.
- A screenshot still renders the retained scene and does not run a draw.

---

## Process note

The controller + subagents loop held: a fresh implementer per task, a task
review naming explicit risks, controller-verified `cargo check --all-targets`
before believing any report, and a push only after review. **It caught two
Criticals that unit tests could not** — the persist chain's close→reopen
deletion (reproduced A/B, 4 rows vs 3) and, earlier, a background `git fetch`
yanking the git panel off Changes mid-commit-message.

**Every single round had an agent correctly disprove part of its own brief.**
The recon that unblocked the persist-chain item disproved a framing that had
survived two sessions; a task-1 review disproved plan fact 12 about
`get_remote`; an implementer measured that dropping a chain cancels it only
best-effort; another disproved the mechanism its brief prescribed and shipped a
better one; a live-verification agent disproved a defect report's "renders
blank" claim; and a reviewer proved a positive control was unfalsifiable and
rewrote it. Keep giving implementers explicit permission to return a documented
negative result — and keep asking for the evidence, because several of these
were found only by mutation-testing the fix's own test.
