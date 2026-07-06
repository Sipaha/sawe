//! Subagent tabs strip — the horizontal pill row painted just below
//! the status row when the current session has one or more claude
//! `Task` / `Agent` subagents in flight (inline Task subagents and/or
//! background Managed Agents). The strip lets the user switch the
//! visible conversation between "Main" (parent-only entries) and each
//! teammate stream's filtered slice, mirroring the Claude Code TUI
//! behaviour.
//!
//! Hidden entirely when there are no live teammate streams AND no
//! background-shell streams — a degenerate strip with only the "Main"
//! pill would just waste a row of vertical space. Teammate tabs iterate
//! `session.streams` in map order (phase 6c — insertion order matches
//! teammate first-appearance, so tab order stays stable across
//! renders). Background-SHELL pills also iterate `session.streams`
//! (phase 6d-A — shells are folded into the mirror as `StreamId::Shell`
//! tabs, only while `Running`).
//!
//! Async-Agent teammates now render as their `StreamId::Teammate` demux
//! stream pill (phase 6d-B — the separate `Background` pill is gone; an
//! async `Agent` already keeps a live teammate stream open, so it shows
//! as a normal `Task` pill). Every teammate pill's label reads
//! `Stream.label` (enriched from `teammate_labels` at `rebuild_streams`,
//! phase 6d-tail-2) — the single source of truth for the label.
//!
//! Inline Task pills deliberately omit a per-tab close button: per the
//! plan, tabs disappear naturally when the parent `Task` ToolCall
//! completes / fails / is cancelled.

use gpui::{AnyElement, Entity, IntoElement, ParentElement, SharedString, Styled};
use ui::prelude::*;
use ui::{Icon, IconName, IconSize, Label, LabelSize, Tooltip};

use super::SolutionSessionView;
use crate::background_shell::BackgroundShellId;
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
    // Teammate tabs iterate `session.streams` in map order (phase 6c). ALL live
    // teammate streams render, async Agents included. The friendly label now
    // rides `Stream.label` (enriched from `teammate_labels` in `rebuild_streams`,
    // phase 6d-tail-2), so the strip just reads `stream.label` — the single
    // source of truth for both desktop and the mobile wire.
    let tabs: Vec<(SharedString, SharedString)> = session_ref
        .streams
        .iter()
        .filter_map(|(sid, stream)| match sid {
            crate::stream::StreamId::Teammate(id) => Some((id.clone(), stream.label.clone())),
            _ => None,
        })
        .collect();
    // Background-shell pills (phase 6d-A): sourced from the derived
    // `StreamId::Shell` streams in `session.streams` (a shell stream exists only
    // while `Running`, so terminal shells have already auto-closed and drop out
    // here — no `ShellDisplayState`, no × affordance). Insertion order in
    // `streams` matches `background_shell_order`, so pill order stays stable.
    let shell_streams: Vec<(BackgroundShellId, SharedString)> = session_ref
        .streams
        .iter()
        .filter_map(|(id, stream)| match id {
            crate::stream::StreamId::Shell(bsid) => Some((bsid.clone(), stream.label.clone())),
            _ => None,
        })
        .collect();
    if tabs.is_empty() && shell_streams.is_empty() {
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
    for (id, label) in shell_streams {
        let is_active = matches!(&selected, SubagentView::Shell(s) if s == &id);
        let id_for_listener = id.clone();
        let pill_id = SharedString::from(format!("task-subagent-strip-shell-{}", id));
        row = row.child(shell_pill(
            pill_id,
            label,
            is_active,
            cx,
            move |this, _, _, cx| {
                let next = SubagentView::Shell(id_for_listener.clone());
                if this.selected_subagent != next {
                    this.selected_subagent = next;
                    cx.notify();
                }
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

/// One background-shell pill (phase 6d-A). A shell stream exists only while
/// `Running` (terminal shells auto-close and drop out of `session.streams`), so
/// there is no per-state colouring and no × close affordance — just a bordered,
/// terminal-icon-prefixed, accent pill.
fn shell_pill<F>(
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
    let (label_color, border_color) = if is_active {
        (Color::Default, colors.element_selected)
    } else {
        (Color::Accent, colors.border)
    };
    let tooltip_text = SharedString::from(format!("Show {}", label));
    h_flex()
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
        )
        .into_any_element()
}
