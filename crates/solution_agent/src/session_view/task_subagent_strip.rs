//! Subagent tabs strip — the horizontal pill row painted just below
//! the status row when the current session has one or more claude
//! `Task` / `Agent` subagents in flight (inline Task subagents and/or
//! background Managed Agents). The strip lets the user switch the
//! visible conversation between "Main" (parent-only entries), each
//! inline Task subagent's filtered slice, and each background agent's
//! standalone JSONL transcript, mirroring the Claude Code TUI
//! behaviour.
//!
//! Hidden entirely when there are no active Task subagents AND no
//! tracked background agents — a degenerate strip with only the
//! "Main" pill would just waste a row of vertical space. Iteration
//! over `active_subagent_order` / `background_agent_order` (NOT the
//! HashMaps directly) so tab order matches spawn order and stays
//! stable across renders; the HashMaps on their own have random
//! hash-seed iteration order.
//!
//! Inline Task pills deliberately omit a per-tab close button: per the
//! plan, tabs disappear naturally when the parent `Task` ToolCall
//! completes / fails / is cancelled. Background-agent pills carry a ×
//! close button ONLY when they are in the `Dead` rendering state
//! (their JSONL hasn't been written to for `agent.managed_agent_stale_timeout_secs`
//! and no terminal `stop_reason` was ever observed); a Dead pill can
//! be dismissed manually because the healthcheck tick only prunes it
//! after a much longer linger window. `Done` pills (terminal
//! stop_reason observed) auto-disappear and never render at all.

use chrono::{DateTime, Utc};
use gpui::{AnyElement, Entity, IntoElement, ParentElement, SharedString, Styled};
use std::time::{Duration, SystemTime};
use ui::prelude::*;
use ui::{Icon, IconName, IconSize, Label, LabelSize, Tooltip};

use super::SolutionSessionView;
use crate::background_agent::{BackgroundAgentId, BackgroundAgentSnapshot};
use crate::background_shell::{BackgroundShellId, BackgroundShellSnapshot, ShellRuntimeState};
use crate::model::SolutionSession;
use crate::store::SubagentView;

/// Build the subagent-tabs strip. Returns `None` when the session
/// has no in-flight subagents so the caller can `when_some(...)` the
/// element into the layout without reserving an empty row.
pub(super) fn render_task_subagent_strip(
    view: &SolutionSessionView,
    session: &Entity<SolutionSession>,
    cx: &mut Context<SolutionSessionView>,
) -> Option<AnyElement> {
    let session_ref = session.read(cx);
    // Snapshot label / id pairs so the click listeners (which need
    // `'static` data) don't have to borrow back through the session
    // entity inside their closures.
    let tabs: Vec<(SharedString, SharedString)> = session_ref
        .active_subagent_order
        .iter()
        .filter_map(|id| {
            session_ref
                .active_subagents
                .get(id)
                .map(|tab| (id.clone(), tab.label.clone()))
        })
        .collect();
    // Background-agent pill snapshot. Computed up front (vs in the
    // render loop) for the same reason as `tabs`: the classifier needs
    // a borrow of `session_ref`, but each closure later wants to take
    // owned data into a `'static` listener. `Done`-state pills drop
    // out here — they auto-hide on terminal `stop_reason`, no UI
    // surface required.
    let now = SystemTime::now();
    // Fork-local managed-agent stale timeout (upstream v1.7.2 dropped this
    // field from the resolved `AgentSettings`; mirror `store`'s pinned default).
    let stale = Duration::from_secs(120);
    let bg_agents: Vec<(SharedString, SharedString, BackgroundAgentDisplayState)> = session_ref
        .background_agent_order
        .iter()
        .filter_map(|id| {
            session_ref.background_agents.get(id).map(|ba| {
                let snap = ba.latest.as_ref();
                let label_body = snap
                    .map(|s| s.activity_label.clone())
                    .unwrap_or_else(|| SharedString::new_static("Starting…"));
                let display_label = SharedString::from(format!("{}·{}", id.short(), label_body));
                let display_state = classify_background_agent_display(snap, now, stale);
                (
                    SharedString::from(id.as_str().to_string()),
                    display_label,
                    display_state,
                )
            })
        })
        .filter(|(_, _, state)| *state != BackgroundAgentDisplayState::Done)
        .collect();
    // Background-shell pill snapshot. Same up-front-collection rationale as
    // `tabs` / `bg_agents`: the classifier borrows `session_ref`, but the
    // listeners want owned `'static` data. Unlike agents, shells have no
    // auto-hidden "Done" state — terminal shells stay in the strip (with a ×)
    // until manually dismissed, so nothing is filtered out here.
    let bg_shells: Vec<(BackgroundShellId, SharedString, ShellDisplayState)> = session_ref
        .background_shell_order
        .iter()
        .filter_map(|id| {
            session_ref.background_shells.get(id).map(|shell| {
                let display_state = classify_background_shell_display(
                    &shell.state,
                    shell.latest.as_ref(),
                    shell.registered_at,
                    now,
                    stale,
                );
                let label = shell_pill_label(id, &shell.command);
                (id.clone(), label, display_state)
            })
        })
        .collect();
    if tabs.is_empty() && bg_agents.is_empty() && bg_shells.is_empty() {
        return None;
    }
    let selected = view.selected_subagent.clone();

    let main_active = matches!(selected, SubagentView::Main);
    let main_pill = pill(
        SharedString::from("task-subagent-strip-main"),
        SharedString::from("Main"),
        main_active,
        cx,
        move |this, _, _, cx| {
            if !matches!(this.selected_subagent, SubagentView::Main) {
                this.selected_subagent = SubagentView::Main;
                cx.notify();
            }
        },
    );

    let mut row = h_flex()
        .id("task-subagent-strip")
        .w_full()
        .flex_none()
        .gap_1()
        .px_2()
        .py_1()
        .overflow_x_scroll()
        .border_t_1()
        .border_color(cx.theme().colors().border)
        .bg(cx.theme().colors().panel_background)
        .child(main_pill);

    for (id, label) in tabs {
        let is_active = matches!(&selected, SubagentView::Task(sel) if sel == &id);
        let id_for_listener = id.clone();
        let pill_id = SharedString::from(format!("task-subagent-strip-{}", id));
        row = row.child(pill(
            pill_id,
            label,
            is_active,
            cx,
            move |this, _, _, cx| {
                let next = SubagentView::Task(id_for_listener.clone());
                if this.selected_subagent != next {
                    this.selected_subagent = next;
                    cx.notify();
                }
            },
        ));
    }
    for (id_str, label, state) in bg_agents {
        let is_active = matches!(
            &selected,
            SubagentView::Background(b) if b.as_str() == id_str.as_ref()
        );
        let id_for_listener = id_str.clone();
        let id_for_close = id_str.clone();
        let pill_id = SharedString::from(format!("task-subagent-strip-bg-{}", id_str));
        row = row.child(background_pill(
            pill_id,
            label,
            is_active,
            state,
            cx,
            move |this, _, _, cx| {
                let next =
                    SubagentView::Background(BackgroundAgentId::new(id_for_listener.clone()));
                if this.selected_subagent != next {
                    this.selected_subagent = next;
                    cx.notify();
                }
            },
            move |this, _, _, cx| {
                let id = BackgroundAgentId::new(id_for_close.clone());
                let session_id = this.session_id();
                let store = crate::store::SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| {
                    store.remove_background_agent(session_id, id, cx);
                });
            },
        ));
    }
    for (id, label, state) in bg_shells {
        let is_active = matches!(&selected, SubagentView::Shell(s) if s == &id);
        let id_for_listener = id.clone();
        let id_for_close = id.clone();
        let pill_id = SharedString::from(format!("task-subagent-strip-shell-{}", id));
        row = row.child(background_shell_pill(
            pill_id,
            label,
            is_active,
            state,
            cx,
            move |this, _, _, cx| {
                let next = SubagentView::Shell(id_for_listener.clone());
                if this.selected_subagent != next {
                    this.selected_subagent = next;
                    cx.notify();
                }
            },
            move |this, _, _, cx| {
                let id = id_for_close.clone();
                let session_id = this.session_id();
                let store = crate::store::SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| {
                    store.remove_background_shell(session_id, id, cx);
                });
            },
        ));
    }
    Some(row.into_any_element())
}

/// One pill button. Accent background + bolder label for the active
/// tab; muted hover for the rest. The click handler is provided by
/// the caller because the "Main" pill and each subagent pill need
/// different captures, and trying to share one closure across them
/// would force a runtime branch on the id inside every listener.
fn pill<F>(
    id: SharedString,
    label: SharedString,
    is_active: bool,
    cx: &mut Context<SolutionSessionView>,
    on_click: F,
) -> AnyElement
where
    F: Fn(
            &mut SolutionSessionView,
            &gpui::ClickEvent,
            &mut Window,
            &mut Context<SolutionSessionView>,
        ) + 'static,
{
    let colors = cx.theme().colors();
    let (bg, label_color) = if is_active {
        (colors.element_selected, Color::Default)
    } else {
        (colors.element_background, Color::Muted)
    };
    let tooltip_text = SharedString::from(format!("Show {}", label));
    let label_size = if is_active {
        LabelSize::Default
    } else {
        LabelSize::Small
    };
    h_flex()
        .id(id)
        .flex_none()
        .h(px(24.0))
        .px_2()
        .gap_1()
        .items_center()
        .rounded_md()
        .bg(bg)
        .cursor_pointer()
        .hover(|s| s.bg(colors.element_hover))
        .tooltip(Tooltip::text(tooltip_text))
        .child(
            Label::new(label)
                .size(label_size)
                .color(label_color)
                .truncate(),
        )
        .on_click(cx.listener(on_click))
        .into_any_element()
}

/// Three-state visual classification for a background-agent pill.
///
///   - `Running`: JSONL was touched within `MANAGED_AGENT_STALE_TIMEOUT`
///     and no terminal `stop_reason` was observed. Normal pill colours.
///   - `Dead`: no terminal stop_reason, but the JSONL mtime is older
///     than the stale timeout — the agent process has likely crashed
///     or wedged. Error-tinted label + manual × dismiss affordance.
///   - `Done`: a terminal `stop_reason` was observed on the last
///     snapshot. Filtered out of the render path entirely (we don't
///     surface "done" agents in the strip — the user is expected to
///     have read whatever they wanted before the agent finished).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundAgentDisplayState {
    Running,
    Dead,
    Done,
}

/// Pure classifier extracted from the render path so it's unit-testable
/// without booting a GPUI context. `snap == None` is treated as
/// "Starting…" → `Running`: a registered agent with no JSONL line yet
/// is the normal initial state, not a dead pill.
pub(crate) fn classify_background_agent_display(
    snap: Option<&BackgroundAgentSnapshot>,
    now: SystemTime,
    stale: Duration,
) -> BackgroundAgentDisplayState {
    let Some(snap) = snap else {
        return BackgroundAgentDisplayState::Running;
    };
    if snap.stop_reason.is_some() {
        return BackgroundAgentDisplayState::Done;
    }
    let elapsed = now.duration_since(snap.mtime).unwrap_or_default();
    if elapsed > stale {
        BackgroundAgentDisplayState::Dead
    } else {
        BackgroundAgentDisplayState::Running
    }
}

/// One background-agent pill. Visually distinct from the inline-Task
/// `pill` builder: bordered (not solid bg) to mark "this is a different
/// log source", plus a × close affordance in the `Dead` state. `Done`
/// state is not handled here — the caller filters it out before
/// invocation, and the match arm is `unreachable!` rather than a
/// silent default so a future refactor that lets Done through can't
/// regress to "Done pills render as Running".
#[allow(clippy::too_many_arguments)]
fn background_pill<F, G>(
    id: SharedString,
    label: SharedString,
    is_active: bool,
    state: BackgroundAgentDisplayState,
    cx: &mut Context<SolutionSessionView>,
    on_click: F,
    on_close: G,
) -> AnyElement
where
    F: Fn(
            &mut SolutionSessionView,
            &gpui::ClickEvent,
            &mut Window,
            &mut Context<SolutionSessionView>,
        ) + 'static,
    G: Fn(
            &mut SolutionSessionView,
            &gpui::ClickEvent,
            &mut Window,
            &mut Context<SolutionSessionView>,
        ) + 'static,
{
    let colors = cx.theme().colors();
    let (label_color, border_color) = match (state, is_active) {
        (BackgroundAgentDisplayState::Running, true) => (Color::Default, colors.element_selected),
        (BackgroundAgentDisplayState::Running, false) => (Color::Muted, colors.border),
        (BackgroundAgentDisplayState::Dead, _) => (Color::Error, colors.border),
        (BackgroundAgentDisplayState::Done, _) => unreachable!("done pills are filtered out"),
    };
    let tooltip_text = SharedString::from(format!("Show {}", label));
    // Per-pill unique id for the × button: a constant id would collide
    // across multiple Dead pills in the same render tree (duplicate ElementId).
    let close_id = SharedString::from(format!("{id}-close"));
    let mut pill_row = h_flex()
        .id(id)
        .flex_none()
        .h(px(24.0))
        .px_2()
        .gap_1()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(border_color)
        .bg(colors.element_background)
        .cursor_pointer()
        .hover(|s| s.bg(colors.element_hover))
        .tooltip(Tooltip::text(tooltip_text))
        .on_click(cx.listener(on_click))
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(label_color)
                .truncate(),
        );
    if matches!(state, BackgroundAgentDisplayState::Dead) {
        pill_row = pill_row.child(
            h_flex()
                .id(close_id)
                .flex_none()
                .px_1()
                .child(Label::new("×").size(LabelSize::Small).color(Color::Muted))
                .on_click(cx.listener(on_close)),
        );
    }
    pill_row.into_any_element()
}

/// Visual classification for a background-shell pill. Unlike the
/// background-agent classifier there is no auto-hidden `Done` state: a
/// terminal shell (`Exited`/`Killed`) keeps an explicit, colour-coded pill
/// (with a × to dismiss) so the user can still drill into its output.
///
///   - `Running`: process still alive and its output was touched within the
///     stale window. Accent pill.
///   - `Exited(code)` / `Killed`: terminal states, surfaced verbatim with a ×.
///   - `Stale`: still nominally `Running`, but no output activity for longer
///     than the stale timeout — likely wedged. Warning-tinted + dismissible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellDisplayState {
    Running,
    Exited(Option<i32>),
    Killed,
    Stale,
}

/// Pure classifier extracted from the render path so it's unit-testable
/// without booting a GPUI context. Terminal `state` values pass through; a
/// `Running` shell is `Stale` when its last-activity age (the snapshot mtime
/// if present, else `registered_at`) exceeds `stale`, otherwise `Running`.
pub(crate) fn classify_background_shell_display(
    state: &ShellRuntimeState,
    latest: Option<&BackgroundShellSnapshot>,
    registered_at: DateTime<Utc>,
    now: SystemTime,
    stale: Duration,
) -> ShellDisplayState {
    match state {
        ShellRuntimeState::Exited(code) => return ShellDisplayState::Exited(*code),
        ShellRuntimeState::Killed => return ShellDisplayState::Killed,
        ShellRuntimeState::Running => {}
    }
    // Last-activity instant: the snapshot mtime when we've tailed output,
    // otherwise the registration time (a shell that never wrote output).
    let last_activity = match latest {
        Some(snap) => snap.mtime,
        None => {
            let secs = registered_at.timestamp().max(0) as u64;
            SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
        }
    };
    let elapsed = now.duration_since(last_activity).unwrap_or_default();
    if elapsed > stale {
        ShellDisplayState::Stale
    } else {
        ShellDisplayState::Running
    }
}

/// Build the pill label: `<short-id>·<command>`, with the command truncated to
/// ~24 chars (the strip is narrow). The `Label` element also truncates on
/// layout, but pre-trimming keeps the tooltip / element string short.
fn shell_pill_label(id: &BackgroundShellId, command: &str) -> SharedString {
    const CMD_CAP: usize = 24;
    let cmd: String = if command.chars().count() > CMD_CAP {
        let truncated: String = command.chars().take(CMD_CAP).collect();
        format!("{truncated}…")
    } else {
        command.to_string()
    };
    SharedString::from(format!("{}·{}", id.short(), cmd))
}

/// One background-shell pill. Mirrors [`background_pill`] but prefixed with a
/// terminal icon and coloured by [`ShellDisplayState`]. A × close affordance is
/// shown for terminal/stale shells (`Exited`/`Killed`/`Stale`); a live
/// `Running` shell has no × (it disappears via the terminal-state transition,
/// not manual dismissal).
#[allow(clippy::too_many_arguments)]
fn background_shell_pill<F, G>(
    id: SharedString,
    label: SharedString,
    is_active: bool,
    state: ShellDisplayState,
    cx: &mut Context<SolutionSessionView>,
    on_click: F,
    on_close: G,
) -> AnyElement
where
    F: Fn(
            &mut SolutionSessionView,
            &gpui::ClickEvent,
            &mut Window,
            &mut Context<SolutionSessionView>,
        ) + 'static,
    G: Fn(
            &mut SolutionSessionView,
            &gpui::ClickEvent,
            &mut Window,
            &mut Context<SolutionSessionView>,
        ) + 'static,
{
    let colors = cx.theme().colors();
    let label_color = match state {
        ShellDisplayState::Running => {
            if is_active {
                Color::Default
            } else {
                Color::Accent
            }
        }
        ShellDisplayState::Exited(Some(0)) => Color::Success,
        ShellDisplayState::Exited(_) => Color::Error,
        ShellDisplayState::Killed => Color::Conflict,
        ShellDisplayState::Stale => Color::Warning,
    };
    let border_color = if is_active {
        colors.element_selected
    } else {
        match state {
            ShellDisplayState::Running => colors.border,
            ShellDisplayState::Exited(Some(0)) => cx.theme().status().success,
            ShellDisplayState::Exited(_) => cx.theme().status().error,
            ShellDisplayState::Killed => cx.theme().status().conflict,
            ShellDisplayState::Stale => cx.theme().status().warning,
        }
    };
    let show_close = matches!(
        state,
        ShellDisplayState::Exited(_) | ShellDisplayState::Killed | ShellDisplayState::Stale
    );
    let tooltip_text = SharedString::from(format!("Show {}", label));
    // Per-pill unique id for the × button: a constant id would collide
    // across multiple terminal/stale shell pills (duplicate ElementId).
    let close_id = SharedString::from(format!("{id}-close"));
    let mut pill_row = h_flex()
        .id(id)
        .flex_none()
        .h(px(24.0))
        .px_2()
        .gap_1()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(border_color)
        .bg(colors.element_background)
        .cursor_pointer()
        .hover(|s| s.bg(colors.element_hover))
        .tooltip(Tooltip::text(tooltip_text))
        .on_click(cx.listener(on_click))
        .child(
            Icon::new(IconName::Terminal)
                .size(IconSize::XSmall)
                .color(label_color),
        )
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(label_color)
                .truncate(),
        );
    if show_close {
        pill_row = pill_row.child(
            h_flex()
                .id(close_id)
                .flex_none()
                .px_1()
                .child(Label::new("×").size(LabelSize::Small).color(Color::Muted))
                .on_click(cx.listener(on_close)),
        );
    }
    pill_row.into_any_element()
}

#[cfg(test)]
mod classifier_tests {
    use super::*;
    use gpui::SharedString;

    #[test]
    fn classifier_returns_running_when_snap_is_none() {
        assert_eq!(
            classify_background_agent_display(None, SystemTime::now(), Duration::from_secs(120)),
            BackgroundAgentDisplayState::Running,
        );
    }

    #[test]
    fn classifier_returns_done_for_terminal_stop_reason() {
        let snap = BackgroundAgentSnapshot {
            mtime: SystemTime::now(),
            activity_label: SharedString::from("Done."),
            stop_reason: Some(SharedString::from("end_turn")),
        };
        assert_eq!(
            classify_background_agent_display(
                Some(&snap),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            BackgroundAgentDisplayState::Done,
        );
    }

    #[test]
    fn classifier_returns_dead_for_stale_mtime() {
        let snap = BackgroundAgentSnapshot {
            mtime: SystemTime::now() - Duration::from_secs(200),
            activity_label: SharedString::from("Bash: x"),
            stop_reason: None,
        };
        assert_eq!(
            classify_background_agent_display(
                Some(&snap),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            BackgroundAgentDisplayState::Dead,
        );
    }

    #[test]
    fn classifier_returns_running_for_fresh_mtime() {
        let snap = BackgroundAgentSnapshot {
            mtime: SystemTime::now(),
            activity_label: SharedString::from("Bash: x"),
            stop_reason: None,
        };
        assert_eq!(
            classify_background_agent_display(
                Some(&snap),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            BackgroundAgentDisplayState::Running,
        );
    }

    #[test]
    fn classifier_prefers_done_over_dead_when_stop_reason_present_on_stale_snap() {
        // A stop_reason promotes the pill to Done even if mtime is
        // ancient — the agent ended cleanly long ago and we want it to
        // auto-hide, not turn into a Dead pill the user has to dismiss.
        let snap = BackgroundAgentSnapshot {
            mtime: SystemTime::now() - Duration::from_secs(9999),
            activity_label: SharedString::from("Done."),
            stop_reason: Some(SharedString::from("end_turn")),
        };
        assert_eq!(
            classify_background_agent_display(
                Some(&snap),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            BackgroundAgentDisplayState::Done,
        );
    }

    // -----------------------------------------------------------------------
    // Task 12 — classify_background_shell_display
    // -----------------------------------------------------------------------

    fn shell_snap(mtime: SystemTime) -> BackgroundShellSnapshot {
        BackgroundShellSnapshot {
            mtime,
            output_tail: SharedString::from("…"),
        }
    }

    #[test]
    fn shell_classifier_exited_zero() {
        assert_eq!(
            classify_background_shell_display(
                &ShellRuntimeState::Exited(Some(0)),
                None,
                Utc::now(),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            ShellDisplayState::Exited(Some(0)),
        );
    }

    #[test]
    fn shell_classifier_exited_nonzero() {
        assert_eq!(
            classify_background_shell_display(
                &ShellRuntimeState::Exited(Some(2)),
                Some(&shell_snap(SystemTime::now())),
                Utc::now(),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            ShellDisplayState::Exited(Some(2)),
        );
    }

    #[test]
    fn shell_classifier_killed() {
        assert_eq!(
            classify_background_shell_display(
                &ShellRuntimeState::Killed,
                None,
                Utc::now(),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            ShellDisplayState::Killed,
        );
    }

    #[test]
    fn shell_classifier_running_fresh() {
        assert_eq!(
            classify_background_shell_display(
                &ShellRuntimeState::Running,
                Some(&shell_snap(SystemTime::now())),
                Utc::now(),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            ShellDisplayState::Running,
        );
    }

    #[test]
    fn shell_classifier_running_old_activity_is_stale() {
        assert_eq!(
            classify_background_shell_display(
                &ShellRuntimeState::Running,
                Some(&shell_snap(SystemTime::now() - Duration::from_secs(300))),
                Utc::now(),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            ShellDisplayState::Stale,
        );
    }

    #[test]
    fn shell_classifier_running_no_snapshot_old_registration_is_stale() {
        // No snapshot → fall back to registered_at; an old registration with
        // no output is a wedged shell that never wrote anything.
        assert_eq!(
            classify_background_shell_display(
                &ShellRuntimeState::Running,
                None,
                Utc::now() - chrono::Duration::seconds(300),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            ShellDisplayState::Stale,
        );
    }

    #[test]
    fn shell_classifier_running_no_snapshot_fresh_registration_is_running() {
        assert_eq!(
            classify_background_shell_display(
                &ShellRuntimeState::Running,
                None,
                Utc::now(),
                SystemTime::now(),
                Duration::from_secs(120),
            ),
            ShellDisplayState::Running,
        );
    }

    #[test]
    fn shell_pill_label_truncates_long_command() {
        let id = BackgroundShellId::new("bvb4ful1z");
        let label = shell_pill_label(&id, "cargo build --bin sawe --profile release-fast");
        assert!(label.starts_with("bvb4ful1z·"));
        assert!(label.ends_with('…'));
    }

    #[test]
    fn shell_pill_label_keeps_short_command() {
        let id = BackgroundShellId::new("abc");
        let label = shell_pill_label(&id, "ls -la");
        assert_eq!(label.as_ref(), "abc·ls -la");
    }
}
