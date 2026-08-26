# `RunController::run`'s Terminal branch double-lease-panicked the whole app

**Date:** 2026-08-26 (found), 2026-08-27 (fixed)
**Status:** FIXED — see "The fix" below. Kept as the worked example of the
nested-`Workspace`-lease trap; FORK.md #93 points here.
**Crates:** `run_config_ui`, and the call shape it shares with `console_panel`

## What happened

Clicking Run (or dispatching `run_config.run` over MCP) on any run
configuration whose provider resolves to `RunRequest::Terminal` crashed the
whole editor with:

```
thread 'main' panicked at crates/gpui/src/app/entity_map.rs:164:32:
cannot read workspace::Workspace while it is already being updated
```

Backtrace root: `RunController::run`'s console-panel lookup. The crash was
**not specific to that line** — the no-panel fallback
(`workspace.update(cx, |workspace, cx| workspace.spawn_in_terminal(..))`), the
Debug arm (`workspace.start_debug_session`) and every early-return error path
(`notify_error` → `workspace.show_error`) had the same shape. `git log -L`
traces the line to the re-fork commit (`3249e3f33e`, 2026-06-24).

## Why

`RunController::run` was always invoked from a call chain that already held
`&mut Workspace`:

- Keybinding / Run button: `actions.rs` registers `Run` via
  `workspace.register_action(|workspace: &mut Workspace, _, window, cx| …)`.
- MCP: `run_config::mcp::RunRunConfigTool` → `RunConfigStore::dispatch_command`
  → `toolbar_strip::dispatch_run_command`, which opens
  `workspace.update(cx, …)` explicitly around `apply_run_command`.

Inside `run`, the Terminal branch re-derived its OWN `Entity<Workspace>` from
`self.workspace: WeakEntity<Workspace>` and called `.read(cx)` / `.update(cx, …)`
on it — a second lease on the *same* entity. GPUI's `EntityMap` tracks leases
globally per entity id, not per `Context<T>` flavour, so it panics no matter
which typed `Context` you reach it through.

## Why no existing test caught it

Every test that called `controller.run(..)` did it through
`window.update(cx, |_, window, cx| controller.update(cx, |c, cx| c.run(..)))`.
`AnyWindowHandle::update` hands back a bare `&mut App` with **no lease at all**,
so the nested lease was invisible. A test written that way passes against the
broken code.

## The fix (2026-08-27)

Two changes, both required — **threading `&mut Workspace` in is not sufficient
on its own**:

1. `RunController::run` is now an associated function taking the caller's
   `&mut Workspace` + `&mut Context<Workspace>`:
   `RunController::run(&controller, workspace, config_id, executor, window, cx)`.
   The controller-only half moved into `prepare_run` (validate, run
   before-launch steps, resolve the `RunRequest`) which returns a `PreparedRun`
   under its own lease; `run` then does the workspace-side half —
   `console_panel_for_workspace(workspace)`, `workspace.spawn_in_terminal`,
   `workspace.start_debug_session`, `workspace.show_error` — on the caller's
   borrow, and hands the result back to the controller via
   `track_console_panel_launch` / `track_fallback_launch` / `track_debug_launch`.
   Entry points: `toolbar_strip::run_by_id` and
   `toolbar_strip::run_selected_config` (used by `apply_run_command` and
   `actions.rs` respectively). `with_controller` survives for `stop` /
   `select`, which touch no workspace state, with a doc comment saying so;
   it and the new helpers are all `pub(crate)`. `rerun` was deleted rather
   than converted — it had zero callers anywhere, and leaving a `pub` entry
   point to the new contract that nothing exercises invites a future caller
   to wire it up from inside a `with_controller` closure, which would
   double-lease the *controller*.

2. `ConsolePanel::spawn_task` is now called from **inside the completion
   poller's async body**, not synchronously. It holds its own
   `WeakEntity<Workspace>` and does `workspace.read(cx)` on line 1 — so calling
   it while `run`'s caller still borrows the workspace panics inside
   `console_panel`, even with fix (1) applied. This was verified by
   experiment, not reasoned about: with fix (1) in place and `spawn_task`
   restored to a synchronous `console_panel.update(cx, …)`,
   `run_under_workspace_lease_reaches_a_real_console_panel` panics at
   `entity_map.rs:164`. Deferring into the poller matches what upstream already
   does — `terminal_view::terminal_panel::TerminalProvider::spawn` calls
   `TerminalPanel::spawn_task` from inside `window.spawn(cx, …)` for the same
   reason.

`notify_error` still opens a `Workspace` lease and is now only reachable from
the two async pollers; it carries a doc comment saying it must not be called
from a leased path. `ConsolePanel::spawn_task` and `spawn_in_new_terminal`
carry the matching warning on the callee side — the constraint has to live
where the `WeakEntity<Workspace>` is, not only in the caller that currently
respects it.

### A launch window the fix opened, and closed

Deferring `spawn_task` into the poller means the run is marked active
synchronously while nothing has spawned yet. A `stop` landing in that gap
(back-to-back `run_config.run` / `run_config.stop` over MCP reaches it in one
turn) would otherwise start a process purely to kill it. The poller now drains
`terminal_launches_pending_kill` **before** calling `spawn_task` and returns if
the token was set. Covered by
`stop_before_the_poller_ticks_never_spawns_a_terminal`, which asserts
`ConsolePanel::tab_count() == 0`; without the guard it fails with `left: 1`.

## Regression coverage

`crates/run_config_ui/src/run_controller.rs` tests now drive `run` through a
real `workspace.update` lease via two helpers — `run_under_workspace_lease`
(direct `RunController::run`) and `dispatch_under_workspace_lease`, which enters
through the `apply_run_command` seam that `dispatch_run_command` and the
`run_config.run` MCP tool use and holds **both** of that seam's leases: the test
window's root view is now a real `MultiWorkspace` (`MultiWorkspace::test_new`),
so the helper reproduces
`window_handle.update(|multi, window, cx| workspace.update(…))` exactly. A
future `run` branch that reaches for `MultiWorkspace` therefore cannot slip
through the way this bug slipped through the old bare-`&mut App` tests. Every
`run` test uses one of the two helpers, so the whole group guards the bug rather
than one named test.

Proof they fail on the pre-fix shape: re-deriving the workspace in `run`
(`controller.read(cx).workspace.upgrade()…read(cx)`) makes
`run_then_stop_tracks_state`,
`stop_during_terminal_launch_window_records_pending_kill`,
`dropping_controller_clears_running_source` and
`run_under_workspace_lease_reaches_a_real_console_panel` and
`stop_before_the_poller_ticks_never_spawns_a_terminal` all abort with
`cannot read workspace::Workspace while it is already being updated`
(`entity_map.rs:164`). The pre-fix drafts of these tests, written against the
old API before the signature changed, additionally hit `entity_map.rs:142`
(`cannot update …`) on the Debug and `show_error` paths.

`run_reaches_a_real_console_panel_via_solution_band_utility_item` (added by
phase 2a task 6, in the old non-leasing shape) was replaced by
`run_under_workspace_lease_reaches_a_real_console_panel`, which asserts the same
thing — a tab really lands in a real `ConsolePanel` installed into
`Workspace::solution_band_utility_item` — in the shape that reproduces the bug.

## Live verification

`script/run-mcp --debug --headless`, solution `BugDemo` opened, a `shell` config
running `sleep 120`:

- `run_config.run` → `{ok: true}`, `run_config.list` reports `running: true`,
  editor process alive, `sleep 120` present as a child of the editor pid.
- `run_config::Stop` then `run_config::Run` dispatched as window actions (the
  `workspace.register_action` path) → run stops and restarts with a new child
  pid, editor alive.
- Screenshot shows the task's terminal tab in the Solution band's utility
  section, so the *real* `ConsolePanel` branch ran rather than the headless
  fallback — that is the branch that panics without half 2.
- `run_config::Debug` on a run-only config renders the
  `` `lease probe` does not support Debug `` notification, i.e. `run`'s
  `show_error` early return, which used to abort. **The `run_config.run`
  *MCP tool* cannot reach that path**: `RunRunConfigTool` validates the
  executor itself and returns `unsupported_executor: … does not support debug`
  (`isError: true`) before dispatching. The two are told apart by the message —
  `{executor:?}` gives a capitalised `Debug` and no prefix, the MCP tool's
  `executor_str` gives lowercase `debug` behind an `unsupported_executor:`
  prefix. The screenshot shows the former.
- A `debug`-type config with a bogus adapter also survives
  (`workspace.start_debug_session` on the caller's borrow).
- Back-to-back `run_config.run` / `run_config.stop` sent as two frames with no
  wait between them: the run reports `running: false` afterwards and the editor
  has **no `sleep` child** — nothing was spawned only to be killed. This is a
  best-effort observation (the poller may tick between two separate JSON-RPC
  requests); `stop_before_the_poller_ticks_never_spawns_a_terminal` is the
  deterministic proof.

Screenshot: `.superpowers/sdd/run-controller-crash/band-utility-terminal-tab-and-show-error.png`.

Probe state cleaned up afterwards (config deleted, solution closed, dev editor
stopped).

## How to apply

Before touching `RunController::run`'s branches again, or writing anything
similar: check whether the call is reachable through `dispatch_run_command` /
`workspace.register_action` (it always is) and take `&mut Workspace` from the
caller rather than upgrading a weak handle. Then check every callee for its own
`WeakEntity<Workspace>` — if it has one, it must run async, off the lease, and
its doc comment should say so where the weak handle lives, not only in the
caller. Moving work off the lease also moves it off the caller's tick, so
re-check any state the caller marked synchronously for a cancellation window
(see "A launch window the fix opened, and closed").

Regression-test through a real `workspace.update(cx, …)` wrapper — and, for the
`dispatch_run_command` seam, through the outer `MultiWorkspace` lease too. The
`window.update` shortcut cannot reproduce the bug.
