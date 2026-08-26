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

1. `RunController::run` / `rerun` are now associated functions taking the
   caller's `&mut Workspace` + `&mut Context<Workspace>`:
   `RunController::run(&controller, workspace, config_id, executor, window, cx)`.
   The controller-only half moved into `prepare_run` (validate, run
   before-launch steps, resolve the `RunRequest`) which returns a `PreparedRun`
   under its own lease; `run` then does the workspace-side half —
   `console_panel_for_workspace(workspace)`, `workspace.spawn_in_terminal`,
   `workspace.start_debug_session`, `workspace.show_error` — on the caller's
   borrow, and hands the result back to the controller via
   `track_console_panel_launch` / `track_fallback_launch` / `track_debug_launch`.
   Entry points: `toolbar_strip::run_config` and
   `toolbar_strip::run_selected_config` (used by `actions.rs` and
   `apply_run_command`). `with_controller` survives for `stop` / `select`,
   which touch no workspace state, with a doc comment saying so.

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
from a leased path.

## Regression coverage

`crates/run_config_ui/src/run_controller.rs` tests now drive `run` through a
real `workspace.update` lease via two helpers — `run_under_workspace_lease`
(direct `RunController::run`) and `dispatch_under_workspace_lease` (through the
`apply_run_command` seam that `dispatch_run_command` and the `run_config.run`
MCP tool use). Every `run` test uses one of them, so the whole group guards the
bug rather than one named test.

Proof they fail on the pre-fix shape: re-deriving the workspace in `run`
(`controller.read(cx).workspace.upgrade()…read(cx)`) makes
`run_then_stop_tracks_state`,
`stop_during_terminal_launch_window_records_pending_kill`,
`dropping_controller_clears_running_source` and
`run_under_workspace_lease_reaches_a_real_console_panel` all abort with
`cannot read workspace::Workspace while it is already being updated`
(`entity_map.rs:164`); the earlier draft of the same tests, written before the
fix, additionally hit `entity_map.rs:142` (`cannot update …`) on the Debug and
`show_error` paths.

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
  section (so the *real* `ConsolePanel` branch ran, not the fallback) and — for
  `run_config::Debug` on a run-only config — the `` `lease probe` does not
  support Debug `` error notification, i.e. the `show_error` early-return path
  renders instead of aborting.
- A `debug`-type config with a bogus adapter also survives
  (`workspace.start_debug_session` on the caller's borrow).

## How to apply

Before touching `RunController::run`'s branches again, or writing anything
similar: check whether the call is reachable through `dispatch_run_command` /
`workspace.register_action` (it always is) and take `&mut Workspace` from the
caller rather than upgrading a weak handle. Then check every callee for its own
`WeakEntity<Workspace>` — if it has one, it must run async, off the lease.
Regression-test through a real `workspace.update(cx, …)` wrapper; the
`window.update` shortcut cannot reproduce the bug.
