# Auto-compaction replaces the observer's `compact` verdict; the phone answers approvals

Date: 2026-09-29. Status: shipped. Decision: FORK.md #220. It retires #166 and changes the asker in #183 and #219.

## What the maintainer asked for

- Remove the observer's compaction trigger.
- Add an on/off auto-compaction toggle next to the eraser, on by default.
- Add threshold settings on right-click.
- Do the same on the phone.
- On the phone, make "button questions" answerable instead of "open the editor".

## What changed

### Auto-compaction (desktop and phone)

- There is a per-session setting (`model::AutoCompactSetting`), persisted when it differs from the
  default (`solution_session_auto_compact`).
- When the context crosses the threshold while the agent's turn runs,
  `store::auto_compact::tick_auto_compaction` starts the handoff ladder as `HandoffAsker::Auto`:
  ask, ask again after 15 min, then send the prompt.
  - It fires once per context. A new threshold may fire again.
  - It never fires on an idle session, so nothing is compacted on startup.
- The ask is marked as the editor's (`spk_editor_recovery` meta). It renders as an
  "Editor · to the agent" plaque on the desktop and an "editor" plaque on the phone.
- Desktop control: a bolt next to the eraser. Click toggles; right-click opens the threshold menu:
  the default for the window, the presets 30–80%, and a custom value set over MCP.
- Phone control: the same in the status strip. Tap toggles; long-press opens the menu.
- Wire: `solution_agent.set_auto_compact`, plus `auto_compact` on `get_session` and
  `get_session_changes`. Feature token: `auto_compact`.

### Observer

- The `compact` verdict is refused by the MCP tool and removed from the judge prompt.
- Reviews of running work (the context-threshold and hourly triggers, `ActiveReviewSnapshot`,
  `ObserverSchedule`) are gone. `compact` was the only verdict they could act on.
- Idle reviews are unchanged.

### Phone approvals

- `pending_approvals` on `get_session` and `get_session_changes` (feature token
  `pending_approvals`) lists every tool call waiting on the user, across all streams, with its
  reason and options.
- The phone shows them above the status strip with working buttons. The "open SPK Editor" banner
  remains only against an older desktop, which sends no such field.

## Found on the way: the first turn of a claude process did not know its window

claude states `modelUsage.contextWindow` only in a `result`, so during a process's first turn the
thread's `max_tokens` was 0 and auto-compaction could not fire. A long autonomous turn after a
restart would never have been asked. The `init` message names the configured model with its suffix
(`claude-opus-5-5[1m]`), while stream messages carry only the bare id. `claude_native` now seeds the
sticky window from it: `[1m]` gives 1M, otherwise the 200k default. A `result`'s real figure still
overrides it.

## Verification

- Desktop: 965+ `solution_agent` tests pass, including:
  - new ladder and auto-compaction tests (`store/tests/user_handoff.rs`);
  - MCP tests for `pending_approvals` and `set_auto_compact`;
  - the judge gate test.
- `claude_native`: the mock init now carries `claude-mock[1m]`, and the prompt test asserts
  `max_tokens == 1M` without any `modelUsage`.
- `./script/clippy` is clean on all touched crates.
- Phone: 933 unit tests, including a DTO decode test that separates absent from empty
  `pending_approvals`.
- Live, headless debug editor with real claude, and the `saweEmu` emulator paired over remote
  control:
  - Auto-compaction at a 10% threshold fired mid-turn ("Your context is 16% full"). The agent
    finished its step, called `start_compact` itself, and the context rotated in place. On the
    second run this happened in the first turn after an editor restart, which confirms the
    window-seeding fix.
  - Desktop screenshots showed the bolt, the threshold menu, and the "Editor · to the agent" plaque.
  - Phone: a safety-hook approval (`rm -f "$D"/*.tmp` in a loop) appeared as "Waiting for your
    answer" with Allow/Reject. Tapping Allow resolved it and the agent carried on.
  - The bolt toggled the desktop setting. The long-press menu showed "Default (40%)" and
    "✓ 10% (custom)", and picking 50% round-tripped.
  - The editor ask rendered as the "editor" plaque on the phone.

## Known, not addressed

- The phone's context meter in the status strip can lag the real figure: it read 3% while the
  desktop showed 16%. It reads the session-list cache. This existed before this change.
