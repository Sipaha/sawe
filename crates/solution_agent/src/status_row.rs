//! Status footer rendered between the conversation list and the compose box: state badge, token meter, model / mode / cwd labels, compact + clear popover.

use chrono::TimeZone as _;
use gpui::{Animation, AnimationExt, ElementId, WeakEntity, pulsating_between};
use gpui::{
    Context, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, px,
};
use ui::prelude::*;
use ui::{
    CommonAnimationExt, ContextMenu, IconName, Label, LabelSize, PopoverMenu, right_click_menu,
};
use util::ResultExt as _;
use workspace::Workspace;

use crate::compact::{
    COMPACT_BUTTON_MIN_PCT, COMPACT_BUTTON_WARN_PCT, COMPACT_HEADROOM_MIN_TOKENS,
};
use crate::model::SessionState;
use crate::session_view::SolutionSessionView;
use crate::store::SolutionAgentStore;

impl SolutionSessionView {
    /// Resolve the agent's currently-selected model name asynchronously
    /// and store it in `status_cached_model`. The status row reads this
    /// cache on subsequent renders; `status_pending_model_fetch` dedupes
    /// the spawn so the row doesn't fire a fresh request every frame.
    fn ensure_status_model_loaded(&mut self, cx: &mut Context<Self>) {
        if self.status_cached_model.is_some() || self.status_pending_model_fetch {
            return;
        }
        let session_id = self.session_id();
        let store = SolutionAgentStore::global(cx);
        let Some(thread) = store
            .read(cx)
            .session(session_id)
            .and_then(|s| s.read(cx).acp_thread().cloned())
        else {
            return;
        };
        let acp_session_id = thread.read(cx).session_id().clone();
        let connection = thread.read(cx).connection().clone();
        let Some(selector) = connection.model_selector(&acp_session_id) else {
            return;
        };
        let task = selector.selected_model(cx);
        self.status_pending_model_fetch = true;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.status_pending_model_fetch = false;
                if let Ok(info) = result {
                    this.status_cached_model = Some(info.name);
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
    }

    /// Spawn a 1 Hz tick that re-renders this view for as long as the
    /// session sits in `Running`. Drives the "Thinking… Ns" counter in
    /// the status row even when no AcpThreadEvents fire (long pauses
    /// between tool calls). Idempotent.
    fn ensure_status_thinking_tick(&mut self, cx: &mut Context<Self>) {
        if self.status_thinking_tick.is_some() {
            return;
        }
        self.status_thinking_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let still_running = this
                    .update(cx, |this, cx| {
                        let session = this.session_entity().read(cx);
                        let running = matches!(session.state, SessionState::Running { .. });
                        if running {
                            cx.notify();
                        }
                        running
                    })
                    .ok()
                    .unwrap_or(false);
                if !still_running {
                    break;
                }
            }
            let _ = this.update(cx, |this, _| {
                this.status_thinking_tick = None;
            });
        }));
    }

    /// Spawn a coarse ~15s tick so the status row's "last activity"
    /// relative label stays current without AcpThreadEvents. Runs for
    /// as long as this view is alive; the timer task is held in
    /// `status_activity_tick` and gets dropped (cancelled) when the
    /// view is dropped.
    fn ensure_status_activity_tick(&mut self, cx: &mut Context<Self>) {
        if self.status_activity_tick.is_some() {
            return;
        }
        self.status_activity_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(15))
                    .await;
                if this
                    .update(cx, |_, cx| {
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
            let _ = this.update(cx, |this, _| {
                this.status_activity_tick = None;
            });
        }));
    }
}

/// Free-function entry point used by `SolutionSessionView::render`. Lives in
/// `status_row` rather than as a method on the view so the file boundary
/// matches the visual one — everything below the conversation list +
/// above the compose row is owned by this module. `is_resuming` is
/// precomputed by the caller because reading it from the view inside
/// this function would re-borrow while GPUI's renderer already holds the
/// entity's lease.
pub(crate) fn render_status_row(
    view: &mut SolutionSessionView,
    is_resuming: bool,
    cx: &mut Context<SolutionSessionView>,
) -> Option<gpui::AnyElement> {
    let session_id = view.session_id();
    let session = SolutionAgentStore::global(cx).read_with(cx, |s, _| s.session(session_id))?;
    let weak_view = cx.entity().downgrade();
    // Snapshot phase: read every value we need from the live session
    // entity, drop the borrow, THEN mutate view caches and spawn timers.
    // Without this split the mutating section would clash with the
    // immutable borrow of `s` through `cx`.
    let s = session.read(cx);
    let agent_id = s.agent_id.clone();
    // Working-directory label: the project name the user picked when
    // creating this session ("Solution root" → "ROOT", member project
    // → catalog name). Lookup needs the live `Solution` so we can
    // compare against `solution.root` and resolve member catalog ids.
    let cwd_label: SharedString = solutions::SolutionStore::try_global(cx)
        .and_then(|store| {
            store.read_with(cx, |store, _| {
                store
                    .solutions()
                    .iter()
                    .find(|sol| sol.id == s.solution_id)
                    .cloned()
            })
        })
        .and_then(|solution| crate::store::project_name_for_cwd(&solution, &s.cwd, cx))
        .unwrap_or_else(|| SharedString::from("ROOT"));
    // For most states the short label ("Idle", "Running", …) is
    // the right thing to show. For `Errored(msg)` we surface the
    // full message inline so the user actually learns *what* went
    // wrong (e.g. "You've hit your limit · resets 2:10pm") instead
    // of just seeing "Error" with no follow-up. The tooltip carries
    // the same text so very long errors that get truncated by
    // flexbox can still be read in full on hover.
    // Cold (no live `AcpThread`) and resuming (cold-tab Send
    // handshake in flight) are session-level conditions that
    // don't fit into `SessionState`'s "agent activity" axis —
    // surface them as override states for the badge so the user
    // doesn't see a misleading bare "Idle" while the subprocess
    // is dead-asleep or mid-handshake.
    let is_cold = s.is_cold();
    // `is_resuming` is precomputed by the caller (`SolutionSessionView`)
    // and passed in. Reading it from `active_view` here would double-lease
    // the view: this method runs inside `nav.update(...)` invoked from
    // *within* `SolutionSessionView::render`, so the view entity is
    // already leased by GPUI's renderer.
    let (state_text, error_text): (SharedString, Option<SharedString>) = if is_resuming {
        (SharedString::from("Resuming…"), None)
    } else if is_cold {
        // The session was restored from disk and the subprocess
        // hasn't been spawned yet. Tooltip-less label is fine —
        // the meaning is glanceable (Sleeping = inactive, send
        // a message to wake it up).
        (SharedString::from("Sleeping"), None)
    } else {
        match &s.state {
            SessionState::Errored(msg) => (
                SharedString::from(format!("Error: {msg}")),
                Some(msg.clone()),
            ),
            SessionState::Running { started_at, .. } => {
                let elapsed = started_at.elapsed().as_secs();
                let label = if elapsed >= 1 {
                    format!("Thinking… {}", format_elapsed(elapsed))
                } else {
                    "Thinking…".to_string()
                };
                (SharedString::from(label), None)
            }
            // "Done in Xs" replaces a bare "Idle" right after a turn
            // completes so a foreground user gets an explicit "the
            // agent finished" cue (the desktop notification path is
            // gated to unfocused panels and ≥5min turns, so without
            // this an in-foreground watcher only sees "Thinking…"
            // disappear). Cleared on the next Running transition.
            SessionState::Idle if s.last_turn_duration.is_some() => {
                let secs = s.last_turn_duration.map(|d| d.as_secs()).unwrap_or(0);
                let label = if secs >= 1 {
                    format!("Done in {}", format_elapsed(secs))
                } else {
                    "Done".to_string()
                };
                (SharedString::from(label), None)
            }
            other => (SharedString::from(other.short_label()), None),
        }
    };
    let is_idle = matches!(s.state, SessionState::Idle) && !is_cold && !is_resuming;
    let is_running = matches!(s.state, SessionState::Running { .. }) && !is_resuming;
    let is_errored = matches!(s.state, SessionState::Errored(_));
    // Live thread → live `TokenUsage`; cold / sleeping → fall
    // back to the `cached_total_tokens` mirrored from metadata
    // at restore time + refreshed on every live
    // `TokenUsageUpdated`. Without this fallback the meter showed
    // "0 / 1.0M · 0.0%" for any restored conversation, which
    // looked like the editor lost the agent's context.
    // On a Task subagent tab the meter shows the SUBAGENT's own context, not
    // the parent's — a Task is a separate agent with its own window. The id is
    // the parent `tool_use_id` the subagent usage is keyed by. `None` here =>
    // Main tab (parent meter, as before). Background/Shell tabs hide the meter
    // entirely (handled via `is_subagent_tab` below), so they never reach here.
    let task_subagent_id: Option<SharedString> = match &view.selected_subagent {
        crate::store::SubagentView::Task(id) => Some(id.clone()),
        _ => None,
    };
    let usage = if let Some(task_id) = task_subagent_id.as_ref() {
        s.acp_thread()
            .and_then(|thread| thread.read(cx).subagent_token_usage(task_id).cloned())
    } else {
        s.acp_thread()
            .and_then(|thread| thread.read(cx).token_usage().cloned())
            .or_else(|| {
                s.cached_total_tokens.map(|used| acp_thread::TokenUsage {
                    used_tokens: used,
                    ..Default::default()
                })
            })
    };
    // Synchronous read of the agent's current session mode
    // ("default", "plan", …). Claude exposes this via ACP — when
    // the connection doesn't implement modes (e.g. mock test
    // adapter) we just hide the segment.
    let mode_text: Option<SharedString> = s.acp_thread().and_then(|thread| {
        let thread = thread.read(cx);
        let modes = thread.connection().session_modes(thread.session_id(), cx)?;
        let current = modes.current_mode();
        modes
            .all_modes()
            .into_iter()
            .find(|m| m.id == current)
            .map(|m| SharedString::from(m.name))
            .or_else(|| Some(SharedString::from(current.0.to_string())))
    });
    // Newest entry's server-stamped time, if it has a real one. The newest
    // entry is the last element of `entries`; `> 0` filters out the
    // NO_TIMESTAMP_MS sentinel and 0/missing. An all-historical or empty
    // session has no real newest time → we render no relative label at all.
    let last_activity_ms = s.entries.last().map(|e| e.created_ms).filter(|&ms| ms > 0);
    let _ = s;
    // While the active session is in `Running`, drive a 1 Hz tick
    // so the elapsed counter ("Thinking… Ns") in `state_text`
    // advances even when no AcpThreadEvents fire (long pauses
    // between tool calls, etc.). Idempotent — the spawn happens
    // only on the first render that observes Running, and the
    // task self-cancels by checking `still_running` each tick.
    if is_running {
        view.ensure_status_thinking_tick(cx);
    } else if view.status_thinking_tick.is_some() {
        view.status_thinking_tick = None;
    }
    // Keep the "last activity" relative label fresh with a coarse
    // ~15s tick for as long as this session tab is open.
    view.ensure_status_activity_tick(cx);
    // Prefer the agent's live `active_model` (latched from every
    // `message_start`, so reflects whichever model claude actually
    // routed the last turn through — including hot swaps via
    // `claude --model` after restart). Falls back to the editor-side
    // selector cache when the agent hasn't seen its first turn yet
    // or when the connection doesn't expose a live value.
    view.ensure_status_model_loaded(cx);
    let model_text = SolutionAgentStore::global(cx)
        .read(cx)
        .session(view.session_id())
        .and_then(|s| s.read(cx).acp_thread().cloned())
        .and_then(|thread| {
            let thread = thread.read(cx);
            thread.connection().active_model(thread.session_id())
        })
        .or_else(|| view.status_cached_model.clone());

    let store = SolutionAgentStore::global(cx);
    let model_options = store.read(cx).session_models(view.session_id(), cx);
    let selected_value = store.read(cx).selected_model(view.session_id(), cx);
    // Display name for the trigger / read-only label. Prefer the user's
    // explicit choice (so the panel reflects a selection IMMEDIATELY — before
    // the agent's next `message_start` refreshes the observed `active_model`),
    // then the live active model, then the raw selected value.
    let model_label: Option<SharedString> = selected_value
        .as_ref()
        .and_then(|v| {
            model_options
                .iter()
                .find(|m| &m.value == v)
                .map(|m| SharedString::from(m.display_name.clone()))
        })
        .or_else(|| model_text.clone())
        .or_else(|| selected_value.clone().map(SharedString::from));

    let raw_used = usage.as_ref().map(|u| u.used_tokens).unwrap_or(0);
    let used = if task_subagent_id.is_some() {
        // A subagent's meter isn't ratcheted through the parent view's peak
        // state (`status_peak_used_tokens` belongs to the Main session); show
        // its raw latest context so switching tabs can't pollute either peak.
        raw_used
    } else {
        let peak = view.status_peak_used_tokens;
        let used = smooth_used_tokens(raw_used, peak);
        if used != peak {
            view.status_peak_used_tokens = used;
        }
        used
    };
    // claude-acp doesn't always populate `max_tokens` (it's gated by an
    // upstream beta flag). Once a real limit has been seen for this session
    // we keep it (cached below) so a later 0/missing update never downgrades
    // the meter to the global fallback (the 200k/1M flicker). The Claude
    // Opus 4 window is the fallback only until a real value first arrives.
    let advertised_max = usage.as_ref().map(|u| u.max_tokens);
    let (max, new_cached_max) = resolve_max_tokens(advertised_max, view.status_cached_max_tokens);
    // A Task subagent inherits the parent's model, so its meter borrows the
    // parent's resolved window as the denominator — but it must NOT write back
    // into the parent's cache (the subagent may advertise 0/a different value).
    if task_subagent_id.is_none()
        && let Some(cached) = new_cached_max
    {
        view.status_cached_max_tokens = Some(cached);
    }
    // Display clamp: tokens-in-context can never legitimately exceed the
    // window. A previously-poisoned reading (the pre-fix
    // `claude_native::apply_usage` ratcheted peak to the SDK's
    // sub-call-aggregated `result.usage`, which for a multi-step turn
    // can be 2-3× the window — observed "1.8M / 1.0M · 100.0%") would
    // otherwise stay visible across restarts because `cached_total_tokens`
    // and `peak_used_tokens` carry over. The source fix prevents NEW
    // overflows; this clamp neutralises the old ones in the UI.
    let used = used.min(max);
    let pct = if max == 0 {
        0.0
    } else {
        (used as f64 / max as f64).clamp(0.0, 1.0)
    };
    let meter_text = SharedString::from(format!(
        "{} / {} · {:.1}%",
        format_tokens_compact(used),
        format_tokens_compact(max),
        pct * 100.0
    ));
    let bar_color = if pct >= 0.8 {
        cx.theme().status().error
    } else if pct >= 0.5 {
        cx.theme().status().warning
    } else {
        cx.theme().colors().text_accent
    };

    // The compact prompt + the agent's dump need real headroom (~3k
    // for the prompt, ~10–20k for state.md / decisions.md / next.md
    // / continue.md combined). A percentage gate misbehaves across
    // model sizes — 10 % of a 200 k window is only 20 k tokens
    // (tight) while 10 % of a 1 M window is 100 k (more than
    // enough). Tie the disable threshold to absolute remaining
    // tokens instead so the button stays usable on long-context
    // models even past 90 %.
    let remaining = max.saturating_sub(used);
    let too_full = remaining < COMPACT_HEADROOM_MIN_TOKENS;
    let pct_ok = pct >= COMPACT_BUTTON_MIN_PCT;
    let compact_enabled = (is_idle || is_cold) && !is_errored && pct_ok && !too_full;
    let clear_enabled = !is_running && !is_resuming;
    let trigger_enabled = compact_enabled || clear_enabled;

    let compact_warning = pct >= COMPACT_BUTTON_WARN_PCT && !too_full;
    let compact_tooltip: SharedString = if is_running || is_resuming {
        "Wait for the current turn to finish before compacting".into()
    } else if is_errored {
        "Compact unavailable while the session is in error".into()
    } else if too_full {
        format!(
            "Only {} of headroom left — start a fresh session manually",
            format_tokens(remaining)
        )
        .into()
    } else if pct < COMPACT_BUTTON_MIN_PCT {
        "Conversation is short — compact later".into()
    } else if compact_warning {
        "Context is filling up — compact recommended".into()
    } else if is_cold {
        "Compact context: wake the session, dump a summary, then continue in a fresh context".into()
    } else {
        "Compact context: agent dumps a summary, then a fresh session continues".into()
    };

    let clear_tooltip: SharedString = if !clear_enabled {
        "Wait for the current turn to finish before clearing".into()
    } else {
        "Clear context: wipe the conversation, keep the tab".into()
    };

    let trigger_tooltip: SharedString = if !trigger_enabled {
        "Wait for the current turn to finish before cleaning up context".into()
    } else {
        "Compact or clear the session's context".into()
    };

    let trigger_color = if compact_warning {
        Color::Warning
    } else {
        Color::Muted
    };

    let cleanup_button: gpui::AnyElement = if !trigger_enabled {
        ui::IconButton::new("solution-status-cleanup", IconName::Eraser)
            .icon_size(IconSize::Small)
            .icon_color(trigger_color)
            .disabled(true)
            .tooltip(ui::Tooltip::text(trigger_tooltip))
            .into_any_element()
    } else {
        let trigger = ui::IconButton::new("solution-status-cleanup", IconName::Eraser)
            .icon_size(IconSize::Small)
            .icon_color(trigger_color)
            .tooltip(ui::Tooltip::text(trigger_tooltip));
        let weak_view = Some(weak_view);
        PopoverMenu::new("solution-status-cleanup-menu")
            .trigger(trigger)
            .menu(move |window, cx| {
                let weak_view = weak_view.clone();
                let compact_tooltip = compact_tooltip.clone();
                let clear_tooltip = clear_tooltip.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let compact_label: SharedString = if compact_enabled {
                        "Compact context".into()
                    } else {
                        format!("Compact context — {compact_tooltip}").into()
                    };
                    let compact_entry = ui::ContextMenuEntry::new(compact_label)
                        .icon(IconName::Archive)
                        .icon_color(Color::Muted)
                        .disabled(!compact_enabled)
                        .handler({
                            let weak_view = weak_view.clone();
                            move |window, cx| {
                                let Some(view) = weak_view.as_ref().and_then(|w| w.upgrade())
                                else {
                                    return;
                                };
                                view.update(cx, |view, cx| {
                                    if is_cold {
                                        view.start_compact_from_cold(window, cx);
                                    } else {
                                        view.start_compact(cx);
                                    }
                                });
                            }
                        });
                    menu = menu.item(compact_entry);

                    let clear_label: SharedString = if clear_enabled {
                        "Clear context".into()
                    } else {
                        format!("Clear context — {clear_tooltip}").into()
                    };
                    let clear_entry = ui::ContextMenuEntry::new(clear_label)
                        .icon(IconName::Eraser)
                        .icon_color(Color::Muted)
                        .disabled(!clear_enabled)
                        .handler({
                            move |window, cx| {
                                let prompt = window.prompt(
                                    gpui::PromptLevel::Warning,
                                    "Clear conversation?",
                                    Some(
                                        "The current conversation will be removed from the \
                                             session. The agent's history is preserved on disk \
                                             but cannot be restored through History.",
                                    ),
                                    &["Cancel", "Clear"],
                                    cx,
                                );
                                window
                                    .spawn(cx, async move |cx| {
                                        // Button index 1 = Clear; 0 / Esc cancels.
                                        if prompt.await.ok() != Some(1) {
                                            return;
                                        }
                                        cx.update(|_, cx| {
                                            SolutionAgentStore::global(cx).update(
                                                cx,
                                                |store, cx| {
                                                    store
                                                        .reset_context(session_id, cx)
                                                        .detach_and_log_err(cx);
                                                },
                                            );
                                        })
                                        .log_err();
                                    })
                                    .detach();
                            }
                        });
                    menu = menu.item(clear_entry);

                    menu
                }))
            })
            .anchor(gpui::Anchor::TopRight)
            .into_any_element()
    };

    // The status row reflects the SELECTED tab, not always Main. A
    // `Background`/`Shell` subagent has its own liveness/activity, so the
    // badge shows THAT; `Main`/`Task` fold into the parent session state (a
    // Task runs inside the parent turn, so the parent's Running/Idle is its
    // status). On a subagent tab the token meter + compact/clear are
    // session/Main-only concepts (we don't track a background agent's own
    // tokens), so they're hidden from the row below.
    let subagent_status: Option<(SharedString, bool)> = {
        use crate::store::SubagentView;
        let session_entity = SolutionAgentStore::global(cx)
            .read(cx)
            .session(view.session_id());
        let session = session_entity.as_ref().map(|s| s.read(cx));
        match (&view.selected_subagent, session) {
            (SubagentView::Background(id), Some(session)) => {
                session.background_agents.get(id).map(|agent| {
                    let running = agent
                        .latest
                        .as_ref()
                        .map_or(true, |snap| snap.stop_reason.is_none());
                    let label = if running {
                        agent
                            .latest
                            .as_ref()
                            .map(|snap| snap.activity_label.clone())
                            .unwrap_or_else(|| SharedString::new_static("Starting…"))
                    } else {
                        SharedString::new_static("Done")
                    };
                    (label, running)
                })
            }
            (SubagentView::Shell(id), Some(session)) => {
                session.background_shells.get(id).map(|shell| {
                    use crate::background_shell::ShellRuntimeState;
                    match &shell.state {
                        ShellRuntimeState::Running => (SharedString::new_static("Running"), true),
                        ShellRuntimeState::Exited(Some(code)) => {
                            (SharedString::from(format!("Exited ({code})")), false)
                        }
                        ShellRuntimeState::Exited(None) => {
                            (SharedString::new_static("Exited"), false)
                        }
                        ShellRuntimeState::Killed => (SharedString::new_static("Killed"), false),
                    }
                })
            }
            // Main / Task fold into the parent session state; a gone session
            // (None) also falls through to the Main-derived badge.
            _ => None,
        }
    };
    let is_subagent_tab = subagent_status.is_some();
    // A Task tab shows the subagent's meter but no parent-session controls
    // (compact/clear/model/effort all act on the Main session, not the Task).
    let is_task_tab = task_subagent_id.is_some();
    // Block model switching while the agent is mid-turn (or resuming) — a
    // `set_model` then only lands on the *next* turn and reads as a no-op.
    let model_select_enabled = !is_running && !is_resuming;
    let show_model_dropdown =
        !is_subagent_tab && !is_task_tab && !model_options.is_empty() && model_select_enabled;

    let effort_value = store.read(cx).selected_effort(view.session_id(), cx);
    // Effort always has a fixed option list, so the dropdown shows whenever the
    // model dropdown would (main tab, not running/resuming). It does NOT depend
    // on a captured list.
    let show_effort_dropdown = !is_subagent_tab && !is_task_tab && model_select_enabled;
    // Claude doesn't stream the current effort level (unlike the model, which
    // it reports every turn), so when the user hasn't set an explicit override
    // we show "auto" — i.e. Claude Code's own default effort, no override —
    // rather than a bare "effort" placeholder. Picking a level overrides it.
    let effort_label: SharedString = effort_value
        .clone()
        .map(SharedString::from)
        .unwrap_or_else(|| "auto".into());

    // State badge ("Thinking… 3m05s" / "Done in 12s" / "Error: …")
    // anchors the LEFT of the row — that's where the user's eye
    // lands first, and the row sits directly above the compose
    // box, so the active-state cue is glanceable while typing the
    // next message. The meter / agent / model / cwd / mode group
    // on the right is reference info for the current session, not
    // status — putting them after the badge gives a clean
    // "what's happening" → "where it's running" reading order.
    let state_badge: gpui::AnyElement = {
        // On a subagent tab the badge reflects the subagent (computed
        // above); shadow the Main-derived inputs so the rest of this block
        // is unchanged. `cold`/`resuming`/`error` don't apply to a subagent.
        let (state_text, is_running, is_resuming, is_cold, error_text) = match subagent_status {
            Some((label, running)) => (label, running, false, false, None),
            None => (state_text, is_running, is_resuming, is_cold, error_text),
        };
        let mut label = Label::new(state_text).size(LabelSize::Small);
        if error_text.is_some() {
            // Truncate long server errors (e.g. the multi-sentence 529
            // Overloaded blurb) with an ellipsis so they can't blow the status
            // row out to full panel width — the full text is in the tooltip.
            label = label.color(Color::Error).truncate();
        } else if is_running || is_resuming {
            label = label.color(Color::Accent);
        } else if is_cold {
            label = label.color(Color::Muted);
        }
        let inner: gpui::AnyElement = if is_running {
            let icon = div()
                .flex_none()
                .child(
                    ui::Icon::new(IconName::Sparkle)
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .with_animation(
                    ElementId::Name("solution-status-thinking-pulse".into()),
                    Animation::new(std::time::Duration::from_secs(1))
                        .repeat()
                        .with_easing(pulsating_between(0.4, 1.0)),
                    |element: gpui::Div, delta| element.opacity(delta),
                );
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(icon)
                .child(label)
                .into_any_element()
        } else if is_resuming {
            // Rotating ⟳ next to "Resuming…" — same visual
            // vocabulary the in-flight session-creation tabs use,
            // so the user reads it instantly as "agent is
            // attaching, hold on".
            let icon = ui::Icon::new(IconName::ArrowCircle)
                .size(IconSize::Small)
                .color(Color::Accent)
                .with_rotate_animation(2);
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(icon)
                .child(label)
                .into_any_element()
        } else if is_cold {
            // Cold session — clicking the badge wakes the agent
            // without forcing the user to type-then-send a message
            // first. Mirrors what `start_resume` does on the next
            // send path: builds the metadata snapshot and dispatches
            // the ACP handshake, with the badge flipping to
            // `Resuming…` on the same tick via `cx.notify`.
            div()
                .id("solution-status-sleep-wake")
                .flex()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .tooltip(ui::Tooltip::text("Click to wake the session"))
                .child(label)
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.resuming {
                        return;
                    }
                    this.start_resume(window, cx);
                }))
                .into_any_element()
        } else {
            label.into_any_element()
        };
        match error_text {
            Some(full) => div()
                .id("solution-status-error-text")
                .max_w(px(400.))
                .overflow_hidden()
                .tooltip(ui::Tooltip::text(full))
                .child(inner)
                .into_any_element(),
            None => inner,
        }
    };

    // "Last activity" relative label, sitting right after the state
    // badge so a stalled agent (Running but last entry long ago) is
    // obvious at a glance: `Thinking… 8m05s · 8m ago`. Hover reveals
    // the absolute date-time of the newest entry. Rendered only when
    // the newest entry carries a real server timestamp — an all-
    // historical or empty session shows nothing here, leaving the row
    // visually identical to before.
    let activity_badge: Option<gpui::AnyElement> = last_activity_ms
        .and_then(|ms| chrono::Utc.timestamp_millis_opt(ms).single())
        .map(|dt| {
            let now = chrono::Utc::now();
            let relative = relative_time_short(dt, now);
            let absolute = format_activity_tooltip(dt, now);
            div()
                .id("solution-status-last-activity")
                .flex_none()
                .tooltip(ui::Tooltip::text(absolute))
                .child(
                    Label::new(relative)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        });

    // `border_t_1()` (top border): the row now sits between the
    // conversation list and the compose box, so the separator we
    // want is at the TOP — without it the row blends into the
    // conversation. The previous `border_b_1()` made sense when
    // the row lived directly under the tab strip, but in the new
    // position a bottom border lands inside the editor_background
    // strip of the compose wrapper and is invisible.
    Some(
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h_7()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(div().flex_none().child(state_badge))
            // Stop sits right of the state badge while a turn is running —
            // relocated here from the compose row (which now carries no
            // action buttons). Cancels the in-flight turn and clears any
            // queued follow-ups. While stopping, the badge itself reads
            // "Stopping", so no separate button is shown.
            .when(is_running, |this| {
                // A bare clickable icon, not an IconButton: the button's box +
                // padding made the gap to the following "·" wider than the
                // other `gap_2` separators and pushed the glyph off the row's
                // vertical center. A plain `Icon` is exactly icon-sized, so the
                // surrounding `gap_2` stays symmetric and `items_center` lines
                // it up with the dots and labels.
                this.child(
                    div()
                        .id("solution-status-stop")
                        .flex_none()
                        .cursor_pointer()
                        .child(
                            ui::Icon::new(IconName::Stop)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                        .tooltip(ui::Tooltip::text(
                            "Stop response (Esc) — clears queued follow-ups",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_turn(cx))),
                )
            })
            .when_some(activity_badge, |this, badge| {
                this.child(Label::new("·").color(Color::Muted).size(LabelSize::Small))
                    .child(badge)
            })
            .child(Label::new("·").color(Color::Muted).size(LabelSize::Small))
            // Token meter + compact/clear are session/Main concepts — hidden
            // on a subagent tab (we don't track a background agent's own
            // tokens, and you don't compact a subagent). The `·` above stays
            // as the separator before the agent/cwd labels either way.
            // Meter (text + bar) shows on Main AND Task tabs — on a Task tab
            // its data is the subagent's own context (sourced above). Hidden
            // only on Background/Shell tabs (`is_subagent_tab`), where we don't
            // track the agent's own tokens.
            .when(!is_subagent_tab, |this| {
                this.child(
                    div().flex_none().child(
                        Label::new(meter_text)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(72.0))
                        .h(px(4.0))
                        .rounded_full()
                        .bg(cx.theme().colors().border)
                        .child(
                            div()
                                .h_full()
                                .w(relative((pct as f32).clamp(0.0, 1.0)))
                                .rounded_full()
                                .bg(bar_color),
                        ),
                )
            })
            // Compact/clear act on the Main session only — not on a Task
            // subagent (you don't compact a subagent's borrowed context).
            .when(!is_subagent_tab && !is_task_tab, |this| {
                this.child(div().flex_none().child(cleanup_button))
            })
            .child(
                Label::new(agent_id)
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(Label::new("·").color(Color::Muted).size(LabelSize::Small))
            .child(
                Label::new(cwd_label)
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .when(show_model_dropdown, |this| {
                let session_id = view.session_id();
                let options = model_options.clone();
                let selected = selected_value.clone();
                let label: SharedString =
                    model_label.clone().unwrap_or_else(|| "model".into());
                let trigger = ui::Button::new("solution-status-model-trigger", label)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    );
                this.child(Label::new("·").color(Color::Muted).size(LabelSize::Small))
                    .child(
                        PopoverMenu::new("solution-status-model-menu")
                            .trigger(trigger)
                            .menu(move |window, cx| {
                                let options = options.clone();
                                let selected = selected.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for model in &options {
                                        let is_current =
                                            selected.as_deref() == Some(model.value.as_str());
                                        let value = model.value.clone();
                                        let entry =
                                            ui::ContextMenuEntry::new(model.display_name.clone())
                                                .when(is_current, |entry| {
                                                    entry
                                                        .icon(IconName::Check)
                                                        .icon_color(Color::Accent)
                                                })
                                                .handler(move |_window, cx| {
                                                    let value = value.clone();
                                                    SolutionAgentStore::global(cx).update(
                                                        cx,
                                                        |store, cx| {
                                                            store.select_model(
                                                                session_id, value, cx,
                                                            );
                                                        },
                                                    );
                                                });
                                        menu = menu.item(entry);
                                    }
                                    menu = menu.separator();
                                    menu = menu.item(
                                        ui::ContextMenuEntry::new("Refresh models")
                                            .icon(IconName::RotateCw)
                                            .icon_color(Color::Muted)
                                            .handler(move |_window, cx| {
                                                SolutionAgentStore::global(cx).update(
                                                    cx,
                                                    |store, cx| {
                                                        store.refresh_models(session_id, cx);
                                                    },
                                                );
                                            }),
                                    );
                                    menu
                                }))
                            })
                            .anchor(gpui::Anchor::TopRight),
                    )
            })
            .when(!show_model_dropdown, |this| {
                this.when_some(model_label.clone(), |this, model| {
                    this.child(Label::new("·").color(Color::Muted).size(LabelSize::Small))
                        .child(Label::new(model).color(Color::Muted).size(LabelSize::Small))
                })
            })
            .when(show_effort_dropdown, |this| {
                let session_id = view.session_id();
                let effort_value = effort_value.clone();
                let trigger =
                    ui::Button::new("solution-status-effort-trigger", effort_label.clone())
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .end_icon(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        );
                this.child(Label::new("·").color(Color::Muted).size(LabelSize::Small))
                    .child(
                        PopoverMenu::new("solution-status-effort-menu")
                            .trigger(trigger)
                            .menu(move |window, cx| {
                                let effort_value = effort_value.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for level in crate::store::EFFORT_LEVELS {
                                        let is_current = effort_value.as_deref() == Some(*level);
                                        let value = level.to_string();
                                        let entry =
                                            ui::ContextMenuEntry::new(SharedString::from(*level))
                                                .when(is_current, |e| {
                                                    e.icon(IconName::Check)
                                                        .icon_color(Color::Accent)
                                                })
                                                .handler(move |_window, cx| {
                                                    let value = value.clone();
                                                    SolutionAgentStore::global(cx).update(
                                                        cx,
                                                        |store, cx| {
                                                            store.select_effort(
                                                                session_id, value, cx,
                                                            );
                                                        },
                                                    );
                                                });
                                        menu = menu.item(entry);
                                    }
                                    menu
                                }))
                            })
                            .anchor(gpui::Anchor::TopRight),
                    )
            })
            .when_some(mode_text, |this, mode| {
                this.child(Label::new("·").color(Color::Muted).size(LabelSize::Small))
                    .child(Label::new(mode).color(Color::Muted).size(LabelSize::Small))
            })
            .when(!is_subagent_tab, |this| {
                let sup = SolutionAgentStore::global(cx)
                    .read(cx)
                    .supervisor_state(session_id);
                let enabled = sup.as_ref().map(|s| s.enabled).unwrap_or(false);
                let held = sup
                    .as_ref()
                    .is_some_and(|s| matches!(s.status, crate::supervisor::SupervisorStatus::Held));
                // The observer is actively reviewing the conversation (an
                // ephemeral judge turn is running) — surface it with a pulsing
                // icon so the user can see the supervisor is working right now.
                let judging = sup.as_ref().is_some_and(|s| {
                    matches!(s.status, crate::supervisor::SupervisorStatus::Judging)
                });
                let trigger_count = sup.as_ref().map(|s| s.trigger_count).unwrap_or(0);
                let (icon, icon_color, tooltip_text): (IconName, Color, SharedString) = if !enabled {
                    (
                        IconName::Eye,
                        Color::Muted,
                        "Click to enable supervisor · right-click for settings".into(),
                    )
                } else if held {
                    // Standing by after a manual stop / a "done" verdict: won't
                    // re-engage until the user sends the next message.
                    (
                        IconName::Clock,
                        Color::Warning,
                        "Supervisor on hold — your next message resumes it · click to disable · right-click for settings".into(),
                    )
                } else if judging {
                    (
                        IconName::Eye,
                        Color::Accent,
                        "Supervisor reviewing… · click to disable · right-click for settings".into(),
                    )
                } else {
                    (
                        IconName::Eye,
                        Color::Accent,
                        "Supervisor on · click to disable · right-click for settings".into(),
                    )
                };
                let workspace = view.workspace_handle().clone();
                // Left-click toggles supervision; right-click opens the settings
                // popover. Wrapping the IconButton in `right_click_menu` keeps the
                // button's own `on_click` (left) working while the secondary
                // button deploys the menu.
                let icon_button = ui::IconButton::new("solution-status-supervisor", icon)
                    .icon_size(IconSize::Small)
                    .icon_color(icon_color)
                    .tooltip(ui::Tooltip::text(tooltip_text))
                    .on_click(move |_, _window, cx| {
                        SolutionAgentStore::global(cx).update(cx, |store, cx| {
                            store.set_supervision_enabled(session_id, !enabled, cx);
                        });
                    });
                // While the observer is reviewing, wrap the icon in a repeating
                // opacity pulse so it reads as "working now" at a glance. The
                // animation self-drives repaints; a `SessionStateChanged` swaps
                // back to the static icon when `Judging` ends.
                let icon_element = if judging {
                    div()
                        .child(icon_button)
                        .with_animation(
                            "solution-status-supervisor-judging",
                            Animation::new(std::time::Duration::from_secs(1))
                                .repeat()
                                .with_easing(pulsating_between(0.35, 1.0)),
                            |el, delta| el.opacity(delta),
                        )
                        .into_any_element()
                } else {
                    icon_button.into_any_element()
                };
                let trigger_cell = std::cell::RefCell::new(Some(icon_element));
                this.child(Label::new("·").color(Color::Muted).size(LabelSize::Small))
                    .child(
                        right_click_menu("solution-status-supervisor-menu")
                            .trigger(move |_, _, _| {
                                trigger_cell
                                    .borrow_mut()
                                    .take()
                                    .unwrap_or_else(|| div().into_any_element())
                            })
                            .menu(move |window, cx| {
                                supervisor_popover_menu(session_id, workspace.clone(), window, cx)
                            })
                            .into_any_element(),
                    )
                    // When enabled, show the firing counter just right of the icon.
                    .when(enabled, |this| {
                        this.child(
                            Label::new(trigger_count.to_string())
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                    })
            })
            .into_any_element(),
    )
}

fn supervisor_popover_menu(
    session_id: crate::model::SolutionSessionId,
    workspace: WeakEntity<Workspace>,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) -> gpui::Entity<ContextMenu> {
    let store = SolutionAgentStore::global(cx);
    let state = store.read(cx).supervisor_state(session_id);
    let enabled = state.as_ref().map(|s| s.enabled).unwrap_or(false);
    let status_label: SharedString = state
        .as_ref()
        .map(|s| s.status.human_label())
        .unwrap_or("Off")
        .into();
    let consecutive_continues = state.as_ref().map(|s| s.consecutive_continues).unwrap_or(0);
    let custom_prompt = state.as_ref().and_then(|s| s.custom_prompt.clone());
    // Read verdicts once; derive both the summary stats and the log rows
    // from the same vec to avoid two filesystem reads.
    let verdicts = store
        .read(cx)
        .solution_root_for_app(session_id, cx)
        .map(|root| {
            let dir = crate::supervisor::supervisor_dir(&root, session_id);
            crate::supervisor::read_verdicts(&dir)
        })
        .unwrap_or_default();
    let stats = crate::supervisor::verdict_stats(&verdicts);
    ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        let header_text: SharedString = if enabled {
            format!("Supervisor · {status_label}").into()
        } else {
            "Supervisor disabled".into()
        };
        menu = menu.header(header_text);
        let toggle_label: SharedString = if enabled {
            "Disable supervisor".into()
        } else {
            "Enable supervisor".into()
        };
        menu = menu.item(
            ui::ContextMenuEntry::new(toggle_label)
                .icon(if enabled {
                    IconName::EyeOff
                } else {
                    IconName::Eye
                })
                .icon_color(Color::Muted)
                .handler(move |_window, cx| {
                    SolutionAgentStore::global(cx).update(cx, |store, cx| {
                        store.set_supervision_enabled(session_id, !enabled, cx);
                    });
                }),
        );

        let prompt_label: SharedString = match &custom_prompt {
            Some(p) => format!("Instruction: “{}”", p.chars().take(30).collect::<String>()).into(),
            None => "Set supervisor instruction…".into(),
        };
        let current_prompt = custom_prompt.clone();
        menu = menu.item(
            ui::ContextMenuEntry::new(prompt_label)
                .icon(IconName::Pencil)
                .icon_color(Color::Muted)
                .handler(move |window, cx| {
                    let Some(workspace) = workspace.upgrade() else {
                        return;
                    };
                    let current = current_prompt.clone();
                    workspace.update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, move |window, cx| {
                            crate::supervisor_instruction_modal::SupervisorInstructionModal::new(
                                session_id, current, window, cx,
                            )
                        });
                    });
                }),
        );

        menu = menu.separator();
        menu = menu.header(SharedString::from(format!(
            "Consecutive continues: {consecutive_continues}/{}",
            crate::supervisor::MAX_CONSECUTIVE_CONTINUES
        )));
        let audits_suffix = if stats.audits > 0 {
            format!(" · 🔍{}", stats.audits)
        } else {
            String::new()
        };
        menu = menu.header(SharedString::from(format!(
            "Verdicts: {} · ↻{} ⚙{} ✓{} 💬{} ❗{}{} · ~{} tokens",
            stats.total,
            stats.by_action[crate::supervisor::VerdictAction::Continue as usize],
            stats.by_action[crate::supervisor::VerdictAction::Compact as usize],
            stats.by_action[crate::supervisor::VerdictAction::Done as usize],
            stats.by_action[crate::supervisor::VerdictAction::AskAgent as usize],
            stats.by_action[crate::supervisor::VerdictAction::Ask as usize],
            audits_suffix,
            stats.total_tokens,
        )));

        // Show up to 8 most-recent verdicts, newest first.
        menu = menu.separator();
        if verdicts.is_empty() {
            menu = menu.header("No verdicts");
        } else {
            menu = menu.header("Recent verdicts");
            for record in verdicts.iter().rev().take(8) {
                let action_icon = match record.action {
                    Some(crate::supervisor::VerdictAction::Continue) => "↻",
                    Some(crate::supervisor::VerdictAction::Compact) => "⚙",
                    Some(crate::supervisor::VerdictAction::Done) => "✓",
                    Some(crate::supervisor::VerdictAction::AskAgent) => "💬",
                    Some(crate::supervisor::VerdictAction::Ask) => "❗",
                    Some(crate::supervisor::VerdictAction::Wait) => "⏳",
                    None => {
                        if matches!(record.kind, crate::supervisor::VerdictKind::Audit) {
                            "🔍"
                        } else {
                            "?"
                        }
                    }
                };
                let reasoning = if record.reasoning.chars().count() > 60 {
                    let truncated: String = record.reasoning.chars().take(57).collect();
                    format!("{action_icon} {truncated}…")
                } else {
                    format!("{action_icon} {}", record.reasoning)
                };
                menu = menu.header(SharedString::from(reasoning));
            }
        }

        menu
    })
}

/// Hardcoded fallback when claude-acp doesn't advertise the model's
/// context-window size (the field is gated by an upstream beta flag).
/// 1M matches Claude Opus 4 with the long-context flag enabled, which
/// is the default for this fork.
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;

/// Resolve the context-window limit for the meter, hardened against the
/// 200k/1M flicker: claude-acp does not advertise `max_tokens` on every usage
/// update (it is gated by an upstream beta flag), so a later update can arrive
/// with `max_tokens == 0`. Once a real (non-zero) limit has been seen for a
/// session we keep it — a subsequent 0/missing value must NOT downgrade the
/// meter to the global [`DEFAULT_CONTEXT_WINDOW`] fallback (which made the meter
/// jump 1M → cached → 1M as updates raced). Returns the limit to display and the
/// value to persist as the session's cached max for the next render.
pub(crate) fn resolve_max_tokens(
    advertised: Option<u64>,
    cached: Option<u64>,
) -> (u64, Option<u64>) {
    match advertised.filter(|max| *max > 0) {
        Some(real) => (real, Some(real)),
        None => match cached {
            Some(cached) => (cached, Some(cached)),
            None => (DEFAULT_CONTEXT_WINDOW, None),
        },
    }
}

/// Smooth the per-session `used_tokens` against per-API-call flicker. The
/// real source fix lives in `claude_native::translate::assistant_usage_update`
/// (drive the meter off per-assistant-message usage, not the terminal
/// `result` event whose SDK-side aggregation can collapse to the last
/// sub-call's tiny numbers on cache-warm follow-up turns). This helper is
/// the second line of defence: ratchet `peak` UP freely (real context never
/// shrinks on its own), and only ratchet DOWN when `raw_used` collapses
/// to ≤ 10 % of the peak — the signature of an explicit context reset
/// (`/clear`, or a `/compact` that summarised the prior context down to a
/// fraction). Anything between 10 % and 100 % of peak is treated as
/// per-call wobble and the peak is held. Returns the value to display.
pub(crate) fn smooth_used_tokens(raw_used: u64, peak: u64) -> u64 {
    if raw_used >= peak {
        raw_used
    } else if raw_used.saturating_mul(10) <= peak {
        raw_used
    } else {
        peak
    }
}

/// "Thinking… 7s" / "Thinking… 1m32s" / "Thinking… 1h05m" — granularity
/// shifts up as the turn drags on so a 40-minute thought doesn't render
/// as "Thinking… 2412s" (mentally divide-by-60 every render). Hours +
/// minutes only past the hour mark; seconds drop off there because
/// minute-precision is enough at that scale and the extra digits just
/// added jitter without information.
pub(crate) fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Compact token count, "12.3k tok" / "456 tok", for the History popover.
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M tok", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k tok", tokens as f64 / 1_000.0)
    } else {
        format!("{} tok", tokens)
    }
}

/// Short token count, "12.3k" / "456", with no unit suffix. Used in the
/// status row where the magnitudes of the two operands ("used / max")
/// already make their meaning unambiguous.
pub(crate) fn format_tokens_compact(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Compact "X ago" formatter mirroring `solutions_ui::welcome::relative_time_label`
/// but kept local to avoid a fork-internal cross-crate dep cycle.
pub(crate) fn relative_time_short(
    ts: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let secs = now.signed_duration_since(ts).num_seconds();
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 7 * 86_400 {
        format!("{}d ago", secs / 86_400)
    } else if secs < 30 * 86_400 {
        format!("{}w ago", secs / (7 * 86_400))
    } else if secs < 365 * 86_400 {
        format!("{}mo ago", secs / (30 * 86_400))
    } else {
        format!("{}y ago", secs / (365 * 86_400))
    }
}

/// `HH:MM` in the machine's local timezone, 24-hour, zero-padded. Input is a
/// UTC instant (created_ms reconstructs to UTC via `Utc.timestamp_millis_opt`).
pub(crate) fn format_hm(when: chrono::DateTime<chrono::Utc>) -> String {
    when.with_timezone(&chrono::Local)
        .format("%H:%M")
        .to_string()
}

/// Date-separator label for `when` relative to `now` (both compared on their
/// local-tz calendar dates). "Today" / "Yesterday" for the two most recent
/// days, else ISO `YYYY-MM-DD` (locale-independent — we deliberately don't pull
/// in `icu` for localized month names on desktop).
pub(crate) fn local_date_label<Tz: chrono::TimeZone>(
    when: chrono::DateTime<Tz>,
    now: chrono::DateTime<Tz>,
) -> String {
    use chrono::Datelike;
    let when_d = when.date_naive();
    let now_d = now.date_naive();
    let days = (now_d - when_d).num_days();
    match days {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        _ => format!(
            "{:04}-{:02}-{:02}",
            when_d.year(),
            when_d.month(),
            when_d.day()
        ),
    }
}

/// Absolute date-time label for the status-row "last activity" tooltip:
/// `"<date-label> <HH:MM>"` ("Today 14:05" / "2026-05-19 09:12"). Reuses the
/// shared date / time formatters so the tooltip stays consistent with the
/// History popover and bubble hover times.
fn format_activity_tooltip(
    when: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    // `local_date_label` reads `date_naive()`, so feed it LOCAL-zone datetimes —
    // otherwise the date label reads the UTC calendar date while `format_hm`
    // renders local time, and the two disagree near midnight in non-UTC zones
    // (e.g. 01:00 local = prior-day UTC would render "Yesterday 01:00").
    let when_local = when.with_timezone(&chrono::Local);
    let now_local = now.with_timezone(&chrono::Local);
    format!(
        "{} {}",
        local_date_label(when_local, now_local),
        format_hm(when)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_max_tokens_keeps_real_value_once_known() {
        // No real value yet, nothing cached → global fallback, nothing to cache.
        assert_eq!(
            resolve_max_tokens(None, None),
            (DEFAULT_CONTEXT_WINDOW, None)
        );
        assert_eq!(
            resolve_max_tokens(Some(0), None),
            (DEFAULT_CONTEXT_WINDOW, None)
        );

        // A real advertised value is used and becomes the cache.
        assert_eq!(
            resolve_max_tokens(Some(200_000), None),
            (200_000, Some(200_000))
        );

        // A later 0/missing update after a real value was known must NOT
        // downgrade to the global fallback — the cached max is kept.
        assert_eq!(
            resolve_max_tokens(None, Some(200_000)),
            (200_000, Some(200_000))
        );
        assert_eq!(
            resolve_max_tokens(Some(0), Some(200_000)),
            (200_000, Some(200_000))
        );

        // A new real value supersedes the cache.
        assert_eq!(
            resolve_max_tokens(Some(1_000_000), Some(200_000)),
            (1_000_000, Some(1_000_000))
        );
    }

    #[test]
    fn smooth_used_tokens_ratchets_up_holds_through_flicker_resets_on_compact() {
        // First observation — no prior peak, displayed value = raw.
        assert_eq!(smooth_used_tokens(50_000, 0), 50_000);

        // Ratchets up freely as the conversation grows.
        assert_eq!(smooth_used_tokens(200_000, 50_000), 200_000);

        // Per-API-call flicker (the 212k → 37k bug): SDK reports the last
        // sub-call's usage on a cache-warm follow-up turn, dropping raw to
        // ~18 % of peak. Above the 10 % floor, so display HOLDS the peak.
        assert_eq!(smooth_used_tokens(37_000, 200_000), 200_000);

        // An even nastier flicker case — 25 k out of 200 k peak (12.5 %)
        // is still above the floor and held.
        assert_eq!(smooth_used_tokens(25_000, 200_000), 200_000);

        // A real context reset shows up as a near-total collapse:
        //   /clear → raw ~= 0 (only the system prompt is left)
        //   /compact → summary + system prompt, often << 10 % of peak
        // Both signatures cross the floor and the display follows down.
        assert_eq!(smooth_used_tokens(20_000, 200_000), 20_000);
        assert_eq!(smooth_used_tokens(0, 200_000), 0);
    }

    #[test]
    fn format_hm_pads_to_24h() {
        use chrono::TimeZone;
        let dt = chrono::Utc.timestamp_millis_opt(0).unwrap(); // 1970-01-01 00:00 UTC
        let s = format_hm(dt);
        assert_eq!(s.len(), 5); // "HH:MM"
        assert_eq!(s.as_bytes()[2], b':');
    }

    #[test]
    fn format_activity_tooltip_combines_date_and_time() {
        let now = chrono::Utc::now();
        // Same instant → "Today HH:MM".
        let tip = format_activity_tooltip(now, now);
        assert!(tip.starts_with("Today "));
        assert_eq!(tip.len(), "Today ".len() + 5); // "Today " + "HH:MM"
        // A 10-day-old instant → "YYYY-MM-DD HH:MM".
        let older = now - chrono::Duration::days(10);
        let tip = format_activity_tooltip(older, now);
        assert_eq!(tip.len(), 10 + 1 + 5); // date + space + time
        assert_eq!(tip.as_bytes()[10], b' ');
    }

    #[test]
    fn local_date_label_relative_today_yesterday() {
        use chrono::TimeZone;
        // Pinned base with no DST edge so the relative-day math is
        // deterministic (Local::now() can flake near a spring-forward).
        let now = chrono::Local
            .with_ymd_and_hms(2026, 6, 15, 12, 0, 0)
            .single()
            .unwrap();
        assert_eq!(local_date_label(now, now), "Today");
        let yest = now - chrono::Duration::days(1);
        assert_eq!(local_date_label(yest, now), "Yesterday");
        let older = now - chrono::Duration::days(10);
        let label = local_date_label(older, now);
        assert_eq!(label.len(), 10); // YYYY-MM-DD
        assert_eq!(label.as_bytes()[4], b'-');
    }
}
