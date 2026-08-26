# `RunController::run`'s Terminal branch double-lease-panics the whole app — pre-existing, both branches

**Date:** 2026-08-26
**Status:** confirmed by live repro on a headless build (backtrace captured); NOT fixed — out of scope for the task that found it (phase 2a task 6)
**Crates:** `run_config_ui`

## What happens

Clicking Run (or dispatching `run_config.run` over MCP) on any run
configuration whose provider resolves to `RunRequest::Terminal` crashes the
whole editor with:

```
thread 'main' panicked at crates/gpui/src/app/entity_map.rs:164:32:
cannot read workspace::Workspace while it is already being updated
```

Backtrace root: `run_config_ui::run_controller::RunController::run` at
`crates/run_config_ui/src/run_controller.rs:419` (the `console_panel`
lookup) — and the crash is **not specific to that line**: the very next
branch down, the no-panel fallback at line ~501
(`workspace.update(cx, |workspace, cx| workspace.spawn_in_terminal(..))`),
has the exact same shape and would panic identically. Confirmed via
`git log -L` that this line traces to the original re-fork commit
(`3249e3f33e`, 2026-06-24) — it predates phase 2a entirely.

## Why

`RunController::run(&mut self, .., window, cx: &mut Context<Self>)` is
always invoked from a call chain that already holds `&mut Workspace`:

- The keybinding path: `crates/run_config_ui/src/actions.rs` registers `Run`
  via `workspace.register_action(|workspace: &mut Workspace, _, window, cx| { ... controller.run(..) })`.
- The MCP path: `run_config::mcp::RunRunConfigTool::run` calls
  `RunConfigStore::dispatch_command`, whose sink is
  `toolbar_strip::dispatch_run_command`, which does
  `window_handle.update(cx, |multi, window, cx| { workspace.update(cx, |workspace, cx| { apply_run_command(workspace, ..) }) })`
  — an explicit, active `Workspace::update` lease around the whole call.

Inside `RunController::run`, the Terminal branch re-derives its OWN
`Entity<Workspace>` from `self.workspace: WeakEntity<Workspace>` and calls
`.read(cx)` / `.update(cx, ..)` on it — a second, nested lease attempt on
the *same* entity, which GPUI's `EntityMap` tracks globally per entity id
(not per `Context<T>` flavor), so it panics regardless of which typed
`Context` you reach it through.

## Why no existing test caught this

Every test in `run_config_ui/src/run_controller.rs` that calls
`controller.run(..)` does it via `window.update(cx, |_, window, cx| { controller.update(cx, |c, cx| c.run(..)) })`
— `AnyWindowHandle::update` hands back a bare `&mut App` (no lease at all),
not `&mut Workspace`. That sidesteps the real production call shape
entirely, so the double-lease was invisible to the suite. None of the
existing tests populate `Workspace::solution_band_utility_item` either
(they all use `workspace.set_terminal_provider(PendingTerminalProvider)`),
so the "real ConsolePanel found" branch specifically had zero coverage —
see `run_reaches_a_real_console_panel_via_solution_band_utility_item`
(added by task 6) for a test that exercises that branch correctly, via the
same non-double-leasing `window.update` shape the sibling tests use.

## Why task 6 didn't fix it

Task 6 was "move `ConsolePanel` out of the dock"; this bug is orthogonal —
it panics identically whether or not a `ConsolePanel` is reachable at all
(the panic is on `.read(cx)` itself, before the lookup's result is used).
The task's diff on this line only swapped the RHS lookup mechanism
(`workspace.panel::<ConsolePanel>(cx)` → `console_panel_for_workspace(workspace.read(cx))`),
leaving `workspace.read(cx)` byte-identical — confirmed via `git diff`.

A real fix needs `RunController::run` (and its `rerun` wrapper) to stop
re-deriving `Entity<Workspace>` from the weak handle and instead take
`&mut Workspace` (or equivalent already-borrowed state) from its callers,
who all already have it. That touches `actions.rs`, `toolbar_strip.rs`
(`apply_run_command`/`with_controller`), and the test module's calling
convention in the same file — a bigger, separate change than task 6's
scope.

## How to apply

Before touching `RunController::run`'s Terminal or Debug branches again,
check whether the call is reachable through `dispatch_run_command` /
`workspace.register_action` (it always is) and thread `&Workspace` in
rather than calling `self.workspace.upgrade().read(cx)` /
`.update(cx, ..)` from inside. Regression-test any fix by calling
`controller.run(..)` through a **real** `workspace.update(cx, |workspace, cx| ..)`
wrapper (not the `window.update` shortcut this file's tests use today) —
that's the only shape that reproduces the bug.
