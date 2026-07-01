# Bug sweep — solution_agent post-Phase-6 (2026-06-30)

Live tracker for issues the maintainer reported while stress-testing the
freshly-shipped `solution_agent` system-message + supervisor work (commits
`6737e00db6` system notes, `638106bd2c` usage-limit/Observer wiring,
`5f1659c766` flush-on-Stopped). Root causes below are confirmed by code
reading (and #3 by an empirical repro).

SCOREBOARD (2026-06-30):
- #1 judge↔reply race — ✅ SHIPPED `18e65fbce7`
- #2 Observer plaque render — ✅ SHIPPED `eb458205b8`
- #3 session-not-found after restart — ✅ SHIPPED `c987efa5d2`
- #4 mobile/desktop session-list scope — ✅ SHIPPED `e625529dad` (maintainer: must be 1:1; mobile now filters to the pinned strip set)
- #5 status latched on Error while streaming — ✅ SHIPPED `b5107a7d41`
- #6 reconnect → Done, doesn't continue — ✅ SHIPPED `e2bafe0506` (maintainer: send the agent a "process restarted, continue" prompt)
- #7 usage-limit resume timer vs new user messages — ✅ SHIPPED `5628f43c76` (clear gate on a successful turn; survives a re-hit Error)
- collateral: `865baee27a` acp_servers test-match SystemNote arm.

Resume: this is the durable task pool. Each row says STATUS + the exact
fix site. Pick the first unshipped/undecided and continue.

---

## #1 — Observer sends a nudge AFTER the user already replied (judge↔reply race)
**STATUS: SHIPPED.** Marker-based suppression + judge teardown on reply.

Shipped design (refined from the plan below): a transient
`SupervisorState.judge_superseded` bool is the single suppression signal.
`supersede_judge_on_user_reply` (called from the `from_user` send funnel in
queue.rs, next to `reset_supervisor_continue_counter`) tears the in-flight
judge down, sets `judge_superseded = true`, and returns status `Judging →
Watching`. `apply_verdict` consumes the marker (`mem::take`) at entry and, if
set, records the verdict for audit then returns WITHOUT acting (no nudge /
Observer note / counter bump / escalation). `tick_supervisor` clears the marker
when spawning a fresh judge so it never pre-suppresses the next cycle. Chosen
over a judge-handle / `status == Judging` guard because both wrongly suppressed
faithful existing `apply_verdict` tests (Done/Compact/Ask issued from
`Watching`/`WaitingUser`); the marker suppresses ONLY a genuinely superseded
verdict. Tests: `user_reply_supersedes_in_flight_judge` +
`verdict_applies_while_judge_in_flight` (control). All 480 lib tests green.

Original plan (kept for context):

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
**SUPERSEDED 2026-07-01 (commit `74a8f8b025`, FORK.md decision #29):** the
breadcrumb layout described below (icon + tag column + a PLAIN `Label` body)
was replaced entirely. System entries now render as a readable message bubble —
plaque badge (level icon + tag) over a `render_span` markdown body (same path as
user/assistant messages), tinted background + left border per level. The
`Label`-not-markdown body was the real unreadability cause; the wrapping-row fix
below was only a partial mitigation. Kept here for history.

**STATUS (historical): fix done, UNCOMMITTED; AFTER-screenshot verification in progress.**
Confirmed via BEFORE png (`docs/findings/2026-06-30-observer-note-BEFORE.png`):
the breadcrumb rendered on a single non-wrapping `h_flex` row — icon + inline
"Observer" tag + long body running off the right edge. Fix (conversation_render.rs):
icon pinned left, then `v_flex().flex_1().min_w_0()` column holding the tag on its
own line above a WRAPPING `Label` body (LabelSize::Small). Added MCP affordance
`solution_agent.push_system_note` (mcp.rs + editor_mcp/lifecycle.rs GLOBAL_TOOLS;
catalog count 85→86 in .rules) so an agent can inject a note to exercise the render
path; 2 tests. Repro: `/tmp/full.py` over `/tmp/mcp.py` Client against a
`--debug --headless` instance. Commit once AFTER-screenshot confirms clean wrap.

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
**STATUS: SHIPPED `c987efa5d2`.** Fix B (durable). Refinement: `apply_tab_orders`
is UPDATE-only and matches zero rows before the metadata row exists, so COALESCE
in the INSERT isn't enough alone — the metadata write itself now CARRIES the real
`tab_order` (new field on `SolutionSessionMetadata`, plumbed through db INSERT/SELECT
+ all constructions), and `create_session_with_parent` re-persists the row AFTER
`open_session_in_strip` stamps the in-memory tab_order. `ON CONFLICT … SET tab_order
= COALESCE(excluded.tab_order, solution_sessions.tab_order)` keeps a None write from
clobbering. Tests: `tab_order_survives_update_before_insert`,
`save_metadata_does_not_wipe_existing_tab_order`, `create_session_persists_tab_order_for_restart`.

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
**STATUS: SHIPPED `e625529dad`.** Maintainer's call: this IS a bug — must be 1:1.
Fix: `ListSessionsTool` top-level enumeration now filters to
`session.tab_order.is_some()` (the desktop strip's pinned set, matching
`workspace.snapshot`). Sub-agent (`parent_session_id`) drill-downs are exempt
(children are never pinned). `create_session` pins via `open_session_in_strip`
(+ #3 made it durable), so nothing legitimate is hidden — a session created
anywhere is pinned and shows on both surfaces. Test:
`list_sessions_excludes_untabbed_sessions`. Original by-design analysis kept
below for context.

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
**STATUS: SHIPPED** (commit b5107a7d41). `SessionState::resume_on_activity`
(NewEntry) + `clear_error_on_activity` (EntryUpdated, Errored-only so a late
streaming-reveal can't resurrect a finished turn). 480 lib tests green.

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

## #6 — after watchdog reconnect the session sits at "Done" instead of continuing
**STATUS: SHIPPED `e2bafe0506`.** Maintainer's call: write to the agent that its
process hung, was restarted, and it can continue. Fix: `reconnect_agent`
captures `was_running` before flipping to `Errored("reconnecting…")`; after
`resume_session` succeeds it calls `maybe_send_reconnect_continuation(resumed,
was_running, cx)` which (only if was_running) sends a fresh continuation prompt
(`RECONNECT_CONTINUATION_PROMPT`, `from_user:false`) — NOT a replay of the
interrupted turn. A reconnect of an already-idle session (manual MCP reconnect)
sends nothing. Tests: `reconnect_continues_a_wedged_running_session`,
`reconnect_idle_session_sends_no_continuation` (drive the extracted method
directly — the mock backend can't load/resume). Original analysis below.

`reconnect_agent` (store.rs:4786, fired by the stuck-session watchdog at
store.rs:2134) is deliberately NON-DESTRUCTIVE: drops the wedged pooled
connection, sets `Errored("reconnecting…")`, cold-izes the thread (keeps
`entries`), `resume_session` respawns the subprocess and regrafts the ACP
session with history+context, pushes an Info note ("Агент не отвечал —
переподключил…"), and the session lands `Idle` ("Done"). It does NOT re-issue
the interrupted turn (replaying a mid-turn prompt could re-run tool calls that
already had side effects) and does NOT touch `supervisor_states`.

So continuation is expected to come from the supervisor: if supervision is
`Watching`, the watchdog fires a judge `IDLE_THRESHOLD_SECS = 60` s after the
reconnect (the Info note bumps `last_activity`, restarting the idle timer) →
continue-nudge. A supervised session DOES self-resume, just up to ~60 s later
(the maintainer likely screenshotted before that). An UNsupervised session
stays at Done by design until the user sends anything.

Gap / proposed fix (await decision):
- (a) After a successful reconnect, if supervision is enabled, immediately
  re-arm `Watching` and force a continue-nudge (skip the 60 s idle wait) so a
  supervised session resumes promptly.
- (b) For an unsupervised session, make the Info breadcrumb actionable
  ("история сохранена — напиши сообщение, чтобы продолжить") so it's clear it
  won't move on its own.
- (c) (rejected) re-issue the last turn — unsafe re: tool-call side effects.

---

## #7 — usage-limit resume timer vs. incoming user messages
**STATUS: SHIPPED `5628f43c76`.** Investigation outcome — the resume gate IS the
supervisor mechanism (`on_judge_failed` Quota → `next_eligible_ms` + a
`backoff_timers` wake task; only when the observer is enabled). Findings vs the
maintainer's spec: Q1(a) a user message while gated IS sent to the agent (not
blocked) — already correct; Q1(b) a re-hit keeps the gate (re-hit surfaces as
`Error`, not `Stopped`) — already correct; **Q2 was the gap** — nothing cleared
the gate when the agent responded successfully, so the session stayed gated
until the stale reset time and the timer fired a redundant judge. Fix:
`clear_resume_gate_on_agent_response` (clears `next_eligible_ms` +
`backoff_attempt` + removes the timer, mirroring `apply_verdict`'s success
clear), called from the `Stopped` handler — a `Stopped` is proof the wall is
gone; a re-hit `Error` leaves the gate armed. Tests:
`successful_turn_clears_pending_usage_limit_resume_gate`,
`rehit_error_keeps_pending_usage_limit_resume_gate`.

Maintainer's spec for correct behavior:
when a usage-limit auto-resume timer is pending and a user sends a new message,
(1) attempt the send to the agent; if it hits the limit AGAIN, the timer STAYS
(keep waiting); (2) if the agent actually responds (limit lifted), CANCEL the
resume timer. Need to verify the current code does both and flag any gap
(e.g. timer not cancelled on a successful response → redundant auto-resume;
or a user message dropped while the timer is pending). Source: commit
`27f9af13f4` usage-limit detect/auto-resume.

---

## Cross-cutting
All of #1/#3/#5 touch `store.rs` (distinct regions) + db.rs/queue.rs — shipping
SEQUENTIALLY on `main` to avoid worktree merge conflicts. Pushing is
pre-authorized. #2 is `conversation_render.rs` (independent). #4 is FORK.md +
mcp.rs DTO (independent) and gated on the maintainer's product call.
