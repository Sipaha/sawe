# Latent double-lease crash: `console_panel::NewChat` action with an active solution

**Discovered:** 2026-07-02, while verifying the new `console_panel::ShowSession`
action over MCP (dispatched `NewChat` as a routing sanity check → editor aborted).

## Symptom

Dispatching the `console_panel::NewChat` **action** (e.g. via
`windows.dispatch_action`) while a Solution is active crashes the editor:

```
thread 'main' panicked at crates/gpui/src/app/entity_map.rs:164:
cannot read workspace::Workspace while it is already being updated
  … console_panel::panel::ConsolePanel::add_chat_tab_with_cwd (panel.rs:1318)
  … console_panel::panel::ConsolePanel::add_chat_tab (panel.rs:1305)
  … console_panel::init::{closure} (console_panel.rs, NewChat handler)
```

## Root cause

The `NewChat` workspace-action handler (`console_panel::init`) runs inside a
`Workspace` update (the handler receives `&mut Workspace` + `Context<Workspace>`).
It then calls `panel.update(… add_chat_tab …)`, and `add_chat_tab_with_cwd`
does `let project = workspace.read(cx)…` (panel.rs:1318) — reading the
`Workspace` entity that is still mutably leased → `double_lease_panic`. Same
class as decision-#… /  `gpui-panel-new-workspace-double-lease` (reading
`Workspace` inside a `Workspace` update).

## Why it's latent (not hit in normal use)

- The "+" button does **not** use the action: its context-menu "New AI Chat"
  entry (panel.rs:~831) calls `add_chat_tab_with_cwd` directly from a menu
  closure, where the `Workspace` is not being updated → safe.
- `NewChat` has **no keybinding** in `assets/keymaps/`.
- The only action-dispatch site is the menu's "New AI Chat (no active solution)"
  fallback (panel.rs:~847), shown only when there is *no* active solution — and
  the `NewChat` handler early-returns when `active_solution_id_for_workspace`
  is `None`, so it never reaches `add_chat_tab` there.

So today the crash is reachable only by dispatching `NewChat` as an action
(MCP / a future keybinding) while a Solution is open.

## Fix (when someone picks this up — out of scope for the ShowSession change)

Defer the `Workspace` read out of the action-handler's update, or read the
project inside the panel's own context. Mirror the `cx.defer_in` remedy used
for the git-graph panel double-lease. A regression test would dispatch
`NewChat` with an active solution and assert no panic.
