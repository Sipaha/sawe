# Desktop notifications: fire only when truly done, and click-to-focus the session

**Date:** 2026-07-04
**Crates:** `solution_agent` (`notifier.rs`, `store.rs`), `zed` (`notification_focus.rs`, `main.rs`)
**FORK.md:** decision #36

Two operator-requested refinements to the freedesktop desktop notifications.

## 1. "Agent finished" only when genuinely quiescent

The `Completed` toast fired on any `Running→Idle` past the 5-min gate, even when more work was coming without the user. `decide_notification` now suppresses `Completed` on three signals (was one):

- `has_pending_messages` — the queue will start another turn (unchanged).
- `has_live_background_work` — idle OVER a running `background_shell` / a messageable `background_agent`; it resumes on its own (e.g. a `Bash(run_in_background)` continuation). Same signal `tick_supervisor` uses.
- `supervisor_will_continue` — the Observer is enabled and auto-driving (`Watching`/`Judging`); it will nudge onward and fires its OWN `notify_supervisor_done` / `escalate_to_user` when work concludes, so a per-turn `Completed` is premature noise.

`AwaitingInput` / `Errored` still fire regardless (parked-needing-you / broken). Computed at the call site in `store.rs` (`handle_acp_event`).

## 2. Click a notification → focus its session

**Notifier side (`notifier::dispatch`):** each notification already carried the id `dev.sawe.session-{sid}`; now it also sets `default_action("open")` so the portal fires an `ActionInvoked` signal when the body is clicked.

**Listener + routing (`zed::notification_focus`, new file):** a single long-lived task spawned in `notification_focus::init` (from `main.rs`, after `console_panel::init`; Linux/FreeBSD only) opens one `ashpd::NotificationProxy` and awaits `receive_action_invoked()`. Since the `ActionInvoked` signal lives on the shared portal object, one proxy receives clicks for every notification we sent. On a click it parses the id → `SolutionSessionId` (ignoring foreign ids) and runs, on the main thread, the focus sequence:

1. session → `solution_id` (`SolutionSession.solution_id`).
2. Find the `MultiWorkspace` window holding that Solution — enumerate `cx.windows().filter_map(|w| w.downcast::<MultiWorkspace>())`, and within each the `Workspace` whose worktree maps via `SolutionStore::solution_for_path`.
3. `Window::activate_window()` — raise the OS window.
4. `MultiWorkspace::activate(target, None, window, cx)` — activate the Solution tab.
5. `Workspace::focus_panel::<ConsolePanel>(window, cx)` — reveal + focus the console dock.
6. `ConsolePanel::show_session(sid, window, cx)` — select the session's chat tab.

## Why the routing lives in `zed`

It spans `solution_agent` + `solutions` + `workspace` + `console_panel`; placing it in any of those would be a dependency cycle (`console_panel` already depends on `solution_agent`). The `zed` crate is the only one that may depend on all four. The window root is `MultiWorkspace` (confirmed: `workspace.rs` enumerates `cx.windows()...downcast::<MultiWorkspace>()`).

## Gotcha

`ConsolePanel::show_session` selects the tab but does NOT reveal the dock — the explicit `focus_panel::<ConsolePanel>` (step 5) is required or a collapsed panel stays collapsed. (The explorer flagged this; the same trap bit the `console_panel::ShowSession` action.)

## Tests / verification

- `notifier` unit tests: `completed_suppressed_over_live_background_work`, `completed_suppressed_while_supervisor_will_continue`, `errored_notifies_even_with_background_work_and_supervisor` (+ existing gates). 9 green.
- `notification_focus::tests::parses_our_notification_id_and_ignores_others` — id round-trip + foreign/malformed rejection.
- The actual portal **click → focus** is manual-verify only — there's no headless notification portal, so the routing (window raise / solution activate / panel focus / tab select) is compile-checked + traced against confirmed signatures, not automatically exercised.
