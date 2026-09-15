# Log-noise triage: one leak, one standing condition, two expected failures

Date: 2026-09-15
Status: fixed

Measured on the maintainer's live log and on probe editors. Four sources, in
descending volume. Only the first was a real bug; the rest were expected
conditions reported as if they were events.

| source | volume | verdict |
|---|---|---|
| `gpui/window.rs … window not found` (headless) | **12 786 lines/minute** | a leak — closed windows were never untracked |
| `… is orphaned (no current member covers it)` | 1583 lines = 44% of the live log | a standing condition re-reported on every sweep |
| `notify::inotify: unable to remove watch descriptor` | 328 lines | third-party INFO for a documented benign race |
| two context-free startup ERRORs | 3 per launch | expected `ENOENT` / `EISDIR` routed through `log_err()` |

## 1. The headless platform never untracked a closed window

`HeadlessClient::open_window` pushed `window.clone()` into its `windows` vec and
**nothing ever removed it**. The 60 Hz refresh timer therefore kept calling
`refresh()` on windows gpui had already closed; each call fired the dead
`request_frame` callback, whose three `handle.update(…)` calls all failed
against a `WindowId` no longer in `cx.windows`:

```
187 lines/sec = 62 fps × 3 call sites (window.rs:1459 / :1525 / :1537)
```

Reproduced exactly: a bare headless editor logs zero of these; `solutions.open`
— which opens the solution window and closes the startup one — starts the flood
and it never stops. It rotated the 1 MB log away in under a minute, which is how
it cost three probe runs in the session that found it: the INFO lines being
measured were gone by the time they were read.

Two non-cosmetic consequences beyond the noise: the closed window's offscreen
wgpu renderer stayed alive for the process's lifetime, and `active_window()` —
`windows.last()`, which `dispatch_action` routes through — kept answering with
a dead handle.

**Why it had no close hook to fix.** X11 removes its entry from
`X11ClientStatePtr::drop_window`, driven by `X11Window`'s own teardown. The
headless platform has no close event at all: gpui closes a window by dropping
the `Box<dyn PlatformWindow>` it owns. Holding a clone made that drop a no-op.
So ownership IS the signal — `TrackedWindow` now holds a
`WeakHeadlessWindow`, the box gpui holds is the only strong reference, and
`HeadlessClient::live_windows()` prunes whatever no longer upgrades. The two
handle accessors prune too, so they cannot answer with a window that is gone.

Verified: same repro, 0 lines in 30s (was 3747 in 20s); 63-line log for a full
create → open → session → screenshot cycle. Not a regression in disguise — the
timer still ticks live windows: an agent reply painted into the transcript with
no input event of any kind, and `workspace.screenshot` still returns a live
frame.

## 2. A standing condition is not an event

`gc_orphan_members` runs on **every** `SolutionStoreEvent::Changed`, and for
each cold session whose member directory is gone it logs a WARN. The session
stays stranded, so every sweep re-logged the same line: 69 sweeps × ~23 sessions
= 1583 identical WARNs, 44% of the log, at the level people actually scan.

The store now remembers which session was reported against which situation
(`cwd` + member list) and re-reports only when that changes. Re-adding the
member clears the record, so a later re-orphaning is reported again rather than
staying silent forever.

## 3 & 4. Expected failures routed through `log_err()`

- `notify` logs one INFO per watch descriptor the kernel had already dropped.
  Its own source says why: *"Log level is info, because it is not a 'real'
  error"* — an expected race. Added to `zlog`'s `DEFAULT_FILTERS` at `Warn`,
  alongside the existing `zbus` / `naga` / `usvg` entries; anything notify
  reports at WARN or above still gets through.
- `editor_mcp` unlinked the well-known socket path with
  `remove_file(&sock).log_err()`. On a fresh runtime dir the path is *supposed*
  to be missing, so every launch printed `ERROR … No such file or directory
  (os error 2)` twice with no path in it. Now `NotFound` is the silent normal
  case and anything else is a WARN that names the path.
- The themes-directory watcher called `load_bytes` on the watched directory
  itself — `ERROR … Is a directory (os error 21)`, once per launch. It now
  skips entries whose metadata says `is_dir`.

Probe launch went from 4 ERROR lines to 1, and that one is real information
(an icon theme named in settings that is not installed in the probe profile).

## What is deliberately still there

After these fixes the maintainer's log is dominated by the
`solution_agent::store` hook-pull ledger (~1400 lines) — one INFO per hook
firing. That is not noise: it is the "where did my message go?" audit trail, and
it is what made the 8m33s parked-delivery timeline reconstructable
(`2026-09-15-parked-parent-cannot-be-reached-by-the-queue.md`). Anyone who wants
it quieter should turn it down per-scope in settings
(`"log": { "solution_agent::store": "warn" }`) rather than delete the call
sites.
