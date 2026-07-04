# Mobile: final turn message lost when the turn ends via an orphan result

**Date:** 2026-07-04
**Crates:** `claude_native` (`connection.rs`), `acp_thread` (`acp_thread.rs`), `solution_agent` (`store.rs`)
**FORK.md:** decision #35

## Symptom (reported live, recurring)

On the mobile client, when an agent turn ends **while the dialog is closed**, the final assistant message doesn't arrive. Reopening the dialog shows the last intermediate step seen before closing, and state `Idle`. Pull-to-refresh / a full `get_session` does NOT recover it — the tail is genuinely absent server-side.

## Why the mobile catch-up couldn't save it

The mobile client (`spk-editor-mobile`) syncs via `get_session` / `get_session_changes(since_seq)` and has robust catch-up on reopen (delta → full-load fallback, liveness probe + reconnect, tail-resync, 60s safety-net poll). All of it reads the server's `SolutionSession.entries` (`mcp.rs:1469`, `get_session_changes` filters `mod_seq > since_seq` at `mcp.rs:1791`). If the tail was never written into `session.entries` with a bumped `mod_seq`, **no** client mechanism can recover it — a full reopen shows stale too. So the bug is server-side.

## Root cause — orphan-result turn-end bypasses the end-of-turn tail flush

The final buffered assistant text of a turn is flushed into the entry's markdown by the run-turn completion path in `AcpThread` (`acp_thread.rs`, Ok/Err arms): `flush_streaming_text` then `cx.emit(EntryUpdated(last))`. That explicit `EntryUpdated` is what makes the store re-convert the entry and **bump its `mod_seq`** (`store.rs` EntryUpdated arm) — the reveal task's per-tick updates stop before the final tail.

But `claude_native` synthesizes a terminal event **out of band** for an *orphan result*: claude emits a terminating `result` with no `prompt()` in flight (documented shape: a `Bash(run_in_background=true)` continuation — the background command finishes, claude resumes on its own, streams `BashOutput` + a follow-up assistant message, and emits a second terminating result). That path (`connection.rs:1436-1462`) emitted `Stopped`/`Error` **directly on the thread without the tail flush**. So the follow-up message's final tail was never turned into an `EntryUpdated`, never funneled into `session.entries`, never got a `mod_seq` bump — permanently missing from `get_session`. The `Idle` state still propagated (the synthesized `Stopped` flips state → `SessionStateChanged` → dirty), which is exactly why the client shows `Idle` + a stale tail.

## Fix

- **`AcpThread::flush_end_of_turn_tail(cx)`** (new `pub` method, `acp_thread.rs`): `flush_streaming_text` + `EntryUpdated(entries.len()-1)`. The two mainline completion arms now call it (behaviour-preserving refactor).
- **`claude_native` orphan path** (`connection.rs`): call `thread.flush_end_of_turn_tail(cx)` in the same `thread.update` step, before the synthesized `Stopped`/`Error` emit. Now the orphan turn's tail reaches `session.entries` with a bumped `mod_seq`, so the mobile catch-up delivers it on reopen.
- **Secondary (`store.rs`)**: the `Error`/`LoadError` arm now flushes pending entry-append throttles synchronously (`flush_pending_entry_appends`, extracted and shared with the `Stopped` arm) — symmetric turn-end delivery, so a turn that errors while already `Errored` doesn't strand the last append on the 500 ms debounce timer.

## Tests

- `acp_thread::tests::flush_end_of_turn_tail_signals_last_entry` — no-entry no-op; with an entry, emits `EntryUpdated(last)`.
- `solution_agent::store::tests::errored_flushes_pending_entry_update_debounce_immediately` — mirror of the `Stopped` flush test for the `Error` arm.
- Existing `stopped_flushes_pending_entry_update_debounce_immediately` still green. Suites: acp_thread + solution_agent (511) green.

## Not covered

The orphan-result path itself has no integration harness (would need a mock claude emitting a second, unmatched terminating `result`). The fix is unit-covered at the method + store level; the orphan call site is a one-line reuse of the tested method. A future `claude_native/tests` orphan fixture would close this.
