# Double-lease crash: `console_panel::NewChat` action with an active solution (FIXED)

**Discovered:** 2026-07-02, while verifying the new `console_panel::ShowSession`
action over MCP (dispatched `NewChat` as a routing sanity check → editor aborted).
**Fixed:** same day — `add_chat_tab` / `add_chat_tab_with_cwd` now take the
`project` as a parameter instead of re-reading it from the `Workspace` entity;
callers that hold `&mut Workspace` (the `NewChat` handler) pass
`workspace.project().clone()`, and the render-time "+" menu reads it while
nothing is leased. Verified live: dispatching `NewChat` with an active Solution
keeps the editor alive and creates a session (no panic).

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

## Fix (applied)

Deferring does **not** help here: a deferred closure that receives
`&mut Workspace` still leases it, so a nested `workspace_entity.read(cx)` would
panic again. The real fix is to stop re-reading the `Workspace` **entity**
inside `add_chat_tab_with_cwd` and instead receive the `project` from whoever
already holds `&mut Workspace`:

- `add_chat_tab(solution_id, project, …)` / `add_chat_tab_with_cwd(solution_id,
  project, cwd, …)` take `project: Entity<Project>`.
- The `NewChat` action handler reads `workspace.project().clone()` from the
  `&mut Workspace` it already holds (no entity re-lease) and passes it down.
- The "+" menu path reads the project once in `render_plus_popover`
  (render context, nothing leased) and captures it into the menu closure.

Verified live over MCP (`windows.dispatch_action console_panel::NewChat` with an
active Solution → editor alive, session created, zero panics). No unit test:
the chat-tab spawn path needs the full editor stack, which these panel tests
deliberately punt to the runtime MCP probe (see `bootstrap_panel` doc comment).
