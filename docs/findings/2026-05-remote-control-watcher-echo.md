# settings::watch_config_file echoes our own atomic_write back through the watcher

Discovered while wiring `RemoteControlStore::set_enabled(true)` in R-2: the
standard `settings::watch_config_file` pattern (used here and in
`solution_agent`, `solutions`, etc.) emits one channel payload per
fs event — including the initial-state read AND one per our own
`Fs::atomic_write` calls from `save_to_disk`. Each event re-reads the
*current* file content via `fs.load(&path).await`, so multiple watcher
events can carry the same text. Without a squelch the watcher's
"adopt parsed settings" path overwrites in-memory mutations that
haven't yet been flushed to disk, in a sequence like:

```
set_address("…", cx)   → save_to_disk (write 1, payload P1)
set_port(0, cx)        → save_to_disk (write 2, payload P2)
add_client("Test", cx) → save_to_disk (write 3, payload P3)
set_enabled(true, cx)  → save_to_disk (write 4, payload P4 with enabled:true)
                       → start_listener_async (spawns bootstrap)
[run_until_parked]
watcher receives initial-load → P3 (file already has P3 on disk when watcher started)
watcher event 1                → P4 (read latest)
watcher event 2                → P4
…
bootstrap completes; this.update sees this.settings.enabled = false because some
intermediate watcher event applied P1/P2/P3 (all enabled:false) over the in-memory
P4 (enabled:true)
```

The fix in `RemoteControlStore` is a `HashSet<String>` of recently-rendered
payloads (`self_write_echoes`): every `save_to_disk` inserts the to-be-written
text BEFORE the async write fires; the watcher checks set membership on each
event and skips matches. The set is bounded at `MAX_ECHO_HISTORY = 32` to
defend against pathological mutation floods; arbitrary eviction is harmless
because a stale echo not in the set just compares `parsed == settings` and is a no-op.

Watch out: this pattern is duplicated across every store that uses
`watch_config_file` for round-trip persistence. The other in-tree call sites
(`solution_agent`, `solutions`) don't appear to hit this in practice because
their writes go through batched-state operations that don't fire the watcher
multiple times back-to-back. For a brand-new store, expect it.

Secondary discovery: GPUI's deterministic test scheduler (the default for
`#[gpui::test]`) panics on cross-thread wakes. Wakeups from a tokio worker
thread back into the test foreground require `cx.executor().allow_parking()`
at the top of the test. Precedent: `crates/git_graph/src/git_graph.rs`,
`crates/db/src/db.rs`, `crates/acp_thread/src/acp_thread.rs` all call it.
