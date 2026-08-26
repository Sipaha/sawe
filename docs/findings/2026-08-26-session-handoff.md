# Session handoff — 2026-08-26

Paused at the maintainer's request after Task 6 of phase 2a. This session
designed a three-phase UI redesign, shipped phase 1 whole, shipped six
supervisor bug-fixes off a user bug report, and got phase 2a to 6/8 tasks.

## Commit chain since the previous handoff

Starting point: `8053136e76`.

**Design + phase-1 plan**
- `7f49c7e76f` — the approved spec (`docs/plans/2026-08-26-solution-band-ai-dialogs-design.md`) + the phase-1 plan.

**Phase 1 — AI sessions become Solution-scoped (COMPLETE)**
- `8f4350f267` chat tabs no longer filtered by the active member
- `471dfdd0f7` create paths stop stamping `member_id`/member-cwd; `cwd: None` = solution root
- `facb6c600a` the `member_id` field and every mirror deleted (incl. `project_label`, the status-row project label)
- `1350384b01` the startup migration CLEARS `member_id` instead of backfilling it
- `e35f5ec746` + `f7cb3c0637` member-less-solution guards removed
- `916984c628` rustfmt
- `a6be8884bc` + `aeccd51c48` clippy fix + FORK.md #89 + docs
- fix wave: `eb98afe107`, `0ebb5ea701`, `c4a6d6851d`, `7fb7483e96`, `761bac3c49`

**Out-of-plan — user-reported supervisor bugs (COMPLETE)**
- `be570d79e9` branch protection no longer protects by name out of the box (FORK.md #90)
- `2cc8d97155` parse the month-day usage-limit reset form (`resets Aug 29, 9pm`)
- `fd7eac9f4e` clear a terminal quota stop on the next successful turn
- `d4e668375b` **Critical**: gate `apply_usage_limit_stop` so it never rewrites a `Disabled`/`Held` supervisor
- `c79383327f` propagate the wall-clear to sibling sessions store-wide
- `93fb8233bb` ETA carries the date when the resume is not today
- `84efa8ba78` narrow the un-park trigger to wall-lifting `StopReason`s

**Phase 2a — the solution band (6/8 tasks)**
- `055cc800a8` the phase-2a plan
- `4bf7f3ab6b` T1: `Workspace::solution_band_item` slot (full-width, above the status bar)
- `cdf5b247e8` T2: `active_dialog_session` in the agent store
- `304488a18c` + `4bd54bdec6` T3: session tab strip in the status bar
- `f00afe7b93` T4: `SolutionBand` renders the active dialog
- `4f26fa0378` T5: chat tabs leave `ConsolePanel` (−1257 lines)
- `54895eb2d7` + `31305a49d7` T5b: rename / restart / drag-reorder restored
- `a15d753bb7` T6: terminal moves into the band; `ConsolePanel` undocked — **committed, review in flight, NOT pushed**

## Where to resume

`docs/plans/2026-08-26-solution-band-layout.md`, ledger at
`.superpowers/sdd/2026-08-26-solution-band-layout/progress.md` (read it —
it carries every ruling and deferred minor).

1. **Finish T6**: its review was in flight at pause. If clean, push. Named
   risks it was checking: whether run-configuration output was verified
   end-to-end or only compiled; whether deleting the `console_panel`
   settings key is a migration hazard; whether the `zed` test failure it
   called pre-existing really is.
2. **T7** — divider + collapse rules + band persistence. Use
   `SplitEditorState`'s `on_drag_move`, **not** `on_drop`: under
   `deferred()` + `block_mouse_except_scroll()` the drop never fires
   (FORK.md #84). Also fold in the two deferred T2 minors.
3. **T8** — drop the "`<Solution>` · N projects" and "AI: N" status-bar
   indicators, full sweep, one `./script/clippy` run, and a FORK.md pass
   that **corrects rather than appends**: #36 is now actively wrong about
   notification click-through, the `console_panel` crate-table row (~line
   49) still claims the panel hosts AI-chat tabs and owns `ChatProvider`,
   lines 207/394 are stale, and the `:117` claim that the bottom dock
   spans the full window is false on `main` (the commits that did that
   live on unreachable refs).

Then **phase 2b** (GitGraph + Debug into the utility section, delete the
vertical dock strips, relocate their buttons per "by geometry") and
**phase 3** (git panel `Changes | Commit`, History removed, graph loses
its inline commit-details subpanel) still need plans.

## Open items the maintainer should know about

- **A pre-existing crash, found but deliberately not fixed**:
  `RunController::run`'s Terminal branch double-lease-panics, reachable
  from the normal UI Run button, git-blamed to 2026-06-24. Full repro in
  `docs/findings/2026-08-26-run-controller-terminal-double-lease-crash.md`.
  Needs scheduling.
- **Upstream `terminal_view::TerminalPanel`**: the maintainer asked
  whether to revert terminals to it. Answer given and accepted — it has
  zero Solution/member awareness while `ConsolePanel`'s remaining code is
  almost entirely that. **Closed; they need nothing from the old panel.**

## Active gotchas (all cost real time this session)

- **`mcp__sawe__*` drives the maintainer's LIVE editor**, not a test
  instance. Never verify with it. Launch your own
  `script/run-mcp --debug --headless` — after `cargo build --bin sawe`,
  because `run-mcp` only compiles when the binary is *missing* and will
  otherwise photograph stale code.
- **The harness's `<new-diagnostics>` blocks are frequently stale
  mid-edit snapshots** — eight times this session. Always confirm with
  `cargo check --all-targets` before believing them or opening a fix round.
- **GPUI double-lease**: reading the `Workspace` entity under a
  `Workspace` lease panics at runtime, compiles clean, and unit tests miss
  it. Bitten four times here, most recently by a refactor that unified two
  call sites and made *both* evaluate the same forbidden argument.
- **`ui::ContextMenu` will not nest a `right_click_menu` inside an open
  popover** — proven live in T5b. Its unconditional `on_blur` dismisses
  the outer menu the moment the inner grabs focus, tearing down the
  deferred child tree. Root cause is documented at the call site in
  `session_tab_strip.rs`.
- **`script/run-mcp --runtime-dir` does not actually isolate the MCP
  socket** (this fork's path resolution ignores the XDG vars);
  `--user-data-dir` works.

## Process notes worth keeping

Two agents produced their best work by *refusing* the task as framed: one
disproved an invariant the brief asserted and stopped rather than build on
it (which is how the `Disabled`-supervisor Critical was caught before it
shipped), and one tried the reviewer's suggested nesting, found it
genuinely impossible, and returned a documented negative result. Both were
correct calls. Keep giving agents explicit permission to come back
empty-handed with a reason.
