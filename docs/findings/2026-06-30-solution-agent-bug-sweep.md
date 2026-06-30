# Bug sweep — solution_agent post-Phase-6 (2026-06-30)

Live tracker for 5 issues the maintainer reported while stress-testing the
freshly-shipped `solution_agent` system-message + supervisor work (commits
`6737e00db6` system notes, `638106bd2c` usage-limit/Observer wiring,
`5f1659c766` flush-on-Stopped). Root causes below are confirmed by code
reading (and #3 by an empirical repro). Fix order: #5 → #1 → #3 → #2 → #4.

Resume: this is the durable task pool. Each row says STATUS + the exact
fix site. Pick the first unshipped and continue.

---

## #1 — Observer sends a nudge AFTER the user already replied (judge↔reply race)
**STATUS: root-caused, fix designed, NOT shipped.**

When a supervised session goes idle, `tick_supervisor` (store.rs:2185) sets
status `Judging` and `spawn_judge` launches an ephemeral judge session. While
the judge thinks, the user can reply. The user-send funnel
(`queue.rs:471` → `reset_supervisor_continue_counter`) resets the counter but
**does not tear down the in-flight judge**. The judge later returns a verdict →
`apply_verdict` (store.rs:1599) → `send_supervisor_nudge` (store.rs:1555)
*unconditionally* pushes the Observer SystemNote ("Наблюдатель направил
агента: …") and queues the nudge to the agent — duplicating the direction the
user already gave.

The symmetric case for a manual *stop* already has a guard:
`hold_supervisor` (store.rs:1447) calls `finish_judge` to tear the judge down.
The *reply* case has no equivalent.

**Fix (defense-in-depth, two layers):**
1. New `supersede_judge_on_user_reply(id, cx)` called from the `from_user`
   branch in `send_message_blocks_targeted` (queue.rs:471, next to the existing
   `reset_supervisor_continue_counter`): if a judge handle exists for `id`,
   `finish_judge(id)` and set supervisor status `Judging→Watching`.
   (Cannot fold into `reset_supervisor_continue_counter` — its early-return
   when `consecutive_continues==0` skips the first-ever judge.)
2. Staleness guard at the TOP of `apply_verdict`: capture
   `let judge_present = self.judge_sessions.contains_key(&id);` BEFORE the
   `finish_judge` at line 1627. If `!judge_present`, the verdict is stale
   (user superseded it, or the judge-stuck watchdog already finished it) —
   append the VerdictRecord for audit, then return WITHOUT nudging / pushing
   the Observer note / incrementing `consecutive_continues` / escalating.
   In the normal path the handle is always present at verdict time (inserted
   in `spawn_ephemeral_supervisor_session` before the judge is briefed).

**Test:** store/tests.rs gpui test — insert cold supervised session, set
`supervisor_states[id]` enabled+`Judging`, insert a `judge_sessions[id]`
handle; call `supersede_judge_on_user_reply` → assert handle gone + status
`Watching`; then call `apply_verdict(Continue)` → assert `consecutive_continues
== 0` (suppressed) and no Observer note appended.

---

## #2 — Observer "plaque" renders strangely
**STATUS: needs a live screenshot to diagnose precisely. NOT shipped.**

One supervisor action produces TWO conversation elements: (a) the short
Observer breadcrumb SystemNote (`conversation_render.rs:431` — Eye icon +
"Observer" tag + `text_md` rendered as a PLAIN `Label` (no markdown), inside an
`h_flex().items_start()` with a left accent border), and (b) the actual nudge,
which is sent to the agent as a user message and renders as the normal blue
accent-tinted user bubble (`render_user_message`, bubble_bg =
`text_accent.opacity(0.12)`). The maintainer's first screenshot shows (b) — the
full instruction in a blue bubble — and calls the "this is an Observer message"
marking strange.

Suspected strangeness in (a): long `text_md` in an `h_flex` with `items_start`
and no width/wrap constraint → the plain `Label` doesn't wrap like the markdown
body does, so a long gist overflows / misaligns next to the icon+tag. Possibly
also: the breadcrumb + the indistinguishable user-bubble nudge read as
redundant/confusing.

**Next action:** build `--debug --headless`, get an Observer SystemNote into a
session, `workspace.screenshot` to SEE it. If there's no MCP affordance to push
a system note, ADD one (per CLAUDE.md "extend the MCP surface"). Then decide the
render fix (likely: wrap the text in a `v_flex`/markdown body, mark the nudge
bubble itself as Observer-sourced).

---

## #3 — "session not found" after restarting the editor on an empty (never-messaged) chat
**STATUS: root-caused + EMPIRICALLY PROVEN, fix chosen, NOT shipped.**

Lost-update race between two detached background DB writes issued back-to-back
with no happens-before in `create_session_with_parent`:
- metadata INSERT — store.rs:2393 `persist_session_row` → `db.save_metadata`
  (`detach_and_log_err`, runs on the work-stealing `BackgroundExecutor`).
- tab_order UPDATE — store.rs:2428 `open_session_in_strip` → `persist_tab_order`
  → `db.update_tab_orders` (also `background_spawn`).

They contend on one `Arc<Mutex<Connection>>` with no FIFO ordering. When the
UPDATE wins and runs first, `apply_tab_orders` (db.rs:1381) matches ZERO rows
(metadata row doesn't exist yet) and silently no-ops. Then the metadata INSERT
(db.rs:888-923) runs — its column list AND its `ON CONFLICT(id) DO UPDATE SET`
**both omit `tab_order`** → row created with `tab_order = NULL`. Nothing ever
re-persists it for an idle, never-touched session. After restart
`restore_open_tabs` queries `tab_order IS NOT NULL AND closed_at IS NULL`
(db.rs:1406 `select_open_tabs`) → session never enters `self.sessions` →
`self.session(id)` None → `anyhow!("unknown session {id}")` (queue.rs:453).
(The desktop tab can still show because lazy hydration keys off
`closed_at IS NULL` only — db.rs:1436 — so the placeholder renders, but the
send path resolves against the missing `tab_order` open-set.)

Refuted suspects: restore does NOT skip empty sessions; not flush-on-first-
message. Proven via temp test: `update_tab_orders(id)` then `save_metadata(id)`
→ `list_open_tabs == []`.

**Fix (Option B — order-independent, durable):** make the metadata INSERT
preserve any pre-existing `tab_order` — thread the session's current tab_order
into `SolutionSessionMetadata`/`persist_session_row` and write
`ON CONFLICT(id) DO UPDATE SET tab_order = COALESCE(excluded.tab_order, solution_sessions.tab_order)`
(and include tab_order in the INSERT column list). Then a metadata INSERT
landing after a tab_order UPDATE no longer clobbers it. (Option A — await the
metadata write before `open_session_in_strip` — is the localized alternative;
B fixes the whole class of detached-write races.)

**Test:** db.rs unit — `tab_order_survives_update_before_insert`
(`update_tab_orders` then `save_metadata`, assert `list_open_tabs == [id]`);
mirror `tab_order_roundtrips_per_solution` (db.rs:1703). Plus store-level
create→adversarial-order→restore→send integration mirroring
`restore_open_tabs_hydrates_cold_sessions` (tests.rs:1905).

---

## #4 — spk-mail session shows on MOBILE but not DESKTOP
**STATUS: WORKING AS DESIGNED. Needs a product decision + a FORK.md note. No code bug.**

Desktop strip = `list_open_tabs` = `tab_order IS NOT NULL AND closed_at IS NULL`
(only sessions pinned to the ConsolePanel strip). Mobile `list_sessions`
(mcp.rs:311) force-hydrates via `hydrate_all_for_solution` →
`list_open_session_ids` = `closed_at IS NULL` (ANY tab_order, db.rs:1436). The
divergence is deliberate (comments at mcp.rs:331-337, db.rs:1430-1435,
store.rs:3983): `closed_at` is the real "dismissed" signal; `tab_order` is a
desktop-strip-only notion; mobile has no strip so it shows all non-dismissed
sessions. spk-mail has `closed_at NULL` + `tab_order NULL` (un-pinned) → mobile
shows it, desktop doesn't.

Real gap: the mobile `SessionSummary` DTO (mcp.rs:437) does NOT expose
`tab_order`/`is_tabbed`, so the phone can't label pinned-vs-other.

**Recommendation (await maintainer):** keep divergent (don't regress the
phone's "watch all my agents" value), but (a) add a FORK.md "Key architectural
decision" entry documenting the split, and (b) optionally expose `is_tabbed`
in `SessionSummary` so mobile can group "Pinned on desktop" vs "Other
sessions". Lowest-risk "make them identical" path (NOT recommended) = mobile
uses `list_open_tabs` semantics.

---

## #5 — status row stuck on "Error: agent error" while the agent is actively streaming
**STATUS: root-caused, fix designed, NOT shipped.**

`SessionState::Errored(msg)` (model.rs:128) is LATCHED. Set by
`AcpThreadEvent::Error | LoadError` → `Errored("agent error")` (store.rs:7290),
also by transient `restart_agent`/`reconnect_agent` ("restarting…"/
"reconnecting…", store.rs:4693/4792) and by a failed turn future (queue.rs:662).
Nothing clears it when the SAME subprocess keeps streaming: `EntryUpdated`
(store.rs:7374 — the streaming-chunk event) doesn't touch `state` at all;
`NewEntry` (store.rs:6884) flips to `Running` only from `Idle | AwaitingInput`,
NOT `Errored`; `Stopped`→`Idle` (store.rs:7046) but the error paths deliberately
DON'T emit `Stopped`. claude_native keeps the pump alive after an error
(connection.rs:1268, orphan-error 1450), so recovered streaming arrives as
`NewEntry`/`EntryUpdated` and the row stays red. status_row.rs:191 renders
`Errored(msg)` purely from `state`, no live-activity cross-check.

**Fix (store-side, layer 1 only — smallest, covers all emitters):** widen the
reset so genuine NON-SystemNote agent activity clears `Errored→Running`:
- `NewEntry` arm (store.rs:6884): add `| SessionState::Errored(_)` to the
  reset guard (keep the existing `is_system_note` skip).
- `EntryUpdated` arm (store.rs:7374): add an `Errored→Running` reset at the top
  for a non-SystemNote updated entry.
A genuinely terminal error still surfaces (no further entries arrive; later
`Stopped`→Idle). Optional layer 2: reset `reconnect_agent` resumed session to
Idle after `resume_session` succeeds.

**Test:** store/tests.rs — `Error` sets `Errored`, then non-system `NewEntry` /
`EntryUpdated` → `Running`; a SystemNote `NewEntry` must NOT clear `Errored`;
`Error` then `Stopped` → `Idle`. Plus a live screenshot of the status row
showing "Thinking…" (not red) while streaming (status-row change ⇒ screenshot
required).

---

## Cross-cutting
All of #1/#3/#5 touch `store.rs` (distinct regions) + db.rs/queue.rs — shipping
SEQUENTIALLY on `main` to avoid worktree merge conflicts. Pushing is
pre-authorized. #2 is `conversation_render.rs` (independent). #4 is FORK.md +
mcp.rs DTO (independent) and gated on the maintainer's product call.
