//! Sub-agents bubble strip — a compact row of tiles painted above the
//! status row when the current session has sub-agents (or is itself a
//! sub-agent with siblings under the same parent). The strip walks up
//! the `parent_session_id` chain to the top-most ancestor, then DFS
//! down to enumerate the whole sub-agent tree rooted at that ancestor,
//! collecting one [`StripRow`] per visited session. Clicking a row
//! switches the panel to that session via the navigator's existing
//! `open_session` path — the same intra-process mechanism the tab
//! strip and the History popover use, so no MCP round-trip is involved.
//!
//! The strip auto-hides when the current session is top-level AND has
//! no children (a degenerate single-session "tree" would just paint a
//! line under the chat with one bubble that says "you are here").
//! [`render_subagent_strip`] returns `None` in that case so the caller
//! can `when_some(...)` it into the layout without reserving space.
//!
//! Limit: at most [`MAX_STRIP_ROWS`] rows; further descendants are
//! collapsed into a single "… +N more" pseudo-row at the bottom so a
//! degenerate tree (long task lists, deep recursion) can't eat the
//! whole panel. Same-solution filter is applied even though the
//! server rejects cross-solution parents on `create_session` — kept
//! as defence-in-depth.

use chrono::{DateTime, Utc};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Window};
use solutions::SolutionId;
use ui::prelude::*;
use ui::{Color, Icon, IconName, IconSize, Label, LabelSize, Tooltip};

use super::SolutionSessionView;
use crate::model::{SessionState, SolutionSession, SolutionSessionId};
use crate::store::SolutionAgentStore;

/// Hard upper bound for visible rows. Past this, the strip truncates
/// with a "… +N more" overflow line — keeps the chat panel from
/// disappearing under a runaway sub-agent fanout.
pub(super) const MAX_STRIP_ROWS: usize = 12;

/// One pure-data row in the strip. Owned by [`render_subagent_strip`]
/// (and the test suite) — the renderer turns this into a clickable
/// element, while tests just assert over the vector shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StripRow {
    pub(super) kind: StripRowKind,
    /// Display text for this row. `Session` rows carry the session
    /// title; `Overflow` carries the pre-rendered "… +N more" label.
    pub(super) label: SharedString,
    /// 0 for the top-most ancestor. Non-overflow rows nest by +1 per
    /// generation; overflow rows always indent at 0 so the truncation
    /// marker is unmistakable.
    pub(super) indent_level: u32,
}

/// Two flavours of row: a real session or the truncation marker. Kept
/// as a sum type so the renderer can branch once (state pill, dot,
/// click handler are only drawn for `Session`) and the truncation
/// marker doesn't pollute every field with `Option<…>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StripRowKind {
    Session {
        session_id: SolutionSessionId,
        state_label: &'static str,
        state_color: StripStateColor,
        total_tokens: Option<u64>,
        is_current: bool,
    },
    Overflow,
}

/// Subset of `ui::Color` used by the strip. Decoupled from the GPUI
/// theme `Color` enum so the pure DFS (and its tests) doesn't need
/// `cx.theme()` to construct rows — the renderer translates this back
/// into a real `Color` at paint time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StripStateColor {
    Success,
    Warning,
    Error,
    Info,
}

impl StripStateColor {
    fn to_ui(self) -> Color {
        match self {
            Self::Success => Color::Success,
            Self::Warning => Color::Warning,
            Self::Error => Color::Error,
            Self::Info => Color::Info,
        }
    }
}

/// Compact "138.3k" / "1.2M" / "457" formatter for the token suffix.
/// Output is meant to be appended to a `· ` separator inside the row;
/// the unit ("tokens") is added by the caller so the same helper can
/// be reused for a tooltip if we ever want one.
pub(super) fn abbreviate_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Snapshot of one session pulled out of the store for the DFS. Decoupled
/// so the pure tree walk doesn't need a GPUI `cx` — only the renderer
/// reads back through entities to attach click handlers.
#[derive(Debug, Clone)]
pub(super) struct SessionSnapshot {
    pub(super) id: SolutionSessionId,
    pub(super) parent_session_id: Option<SolutionSessionId>,
    pub(super) solution_id: SolutionId,
    pub(super) title: SharedString,
    pub(super) state_label: &'static str,
    pub(super) state_color: StripStateColor,
    pub(super) total_tokens: Option<u64>,
    pub(super) created_at: DateTime<Utc>,
}

fn state_color(state: &SessionState) -> StripStateColor {
    match state {
        SessionState::Idle => StripStateColor::Success,
        SessionState::Running { .. } => StripStateColor::Info,
        SessionState::Stopping { .. } => StripStateColor::Info,
        SessionState::AwaitingInput => StripStateColor::Warning,
        SessionState::Errored(_) => StripStateColor::Error,
    }
}

/// "Awaiting input" instead of "Awaiting" (SessionState::short_label) —
/// the strip has the horizontal room and the longer form reads as a
/// clearer status pill at the size we render it.
fn state_label(state: &SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::Running { .. } => "running",
        SessionState::Stopping { .. } => "stopping",
        SessionState::AwaitingInput => "awaiting input",
        SessionState::Errored(_) => "errored",
    }
}

/// Read every session in the current solution into a flat snapshot
/// vector, excluding live ephemeral judge sessions. The DFS works
/// against snapshots so it stays a pure function and the unit tests
/// don't need GPUI render plumbing.
fn collect_snapshots(
    store: &Entity<SolutionAgentStore>,
    solution_id: &SolutionId,
    cx: &gpui::App,
) -> Vec<SessionSnapshot> {
    let store_ref = store.read(cx);
    let hidden_ids = store_ref.live_supervisor_session_ids();
    store_ref
        .sessions_for(solution_id)
        .into_iter()
        // Exclude ephemeral supervisor judges/auditors: by the live handle-map
        // union AND by the `is_supervisor_ephemeral` entity flag (the flag stays
        // correct even at the close-time window where the map entry is already
        // gone — see `close_session`).
        .filter(|entity| {
            let s = entity.read(cx);
            !hidden_ids.contains(&s.id) && !s.is_supervisor_ephemeral
        })
        .map(|entity| {
            let s = entity.read(cx);
            // Live thread overrides the cached value: a running session
            // streams `TokenUsageUpdated` events into the thread and
            // doesn't bother writing `cached_total_tokens` until the
            // next persist cycle — so reading the cache on a hot
            // session would show a stale number.
            let live_tokens = s
                .acp_thread()
                .and_then(|t| t.read(cx).token_usage().map(|u| u.used_tokens));
            SessionSnapshot {
                id: s.id,
                parent_session_id: s.parent_session_id,
                solution_id: s.solution_id.clone(),
                title: s.title.clone(),
                state_label: state_label(&s.state),
                state_color: state_color(&s.state),
                total_tokens: live_tokens.or(s.cached_total_tokens),
                created_at: s.created_at,
            }
        })
        .collect()
}

/// Walk parent pointers from `current` upward until we hit a session
/// whose `parent_session_id` is `None`. If a referenced parent is
/// missing from the snapshot vector (dangling pointer after a parent
/// delete — see F-server commit message) the walk stops at the
/// orphan, treating it as the head. Bounds the loop to
/// `snapshots.len()` iterations so a self-referential cycle (which
/// the server doesn't currently prevent) can't deadlock the renderer.
pub(super) fn find_root(
    current: SolutionSessionId,
    snapshots: &[SessionSnapshot],
) -> SolutionSessionId {
    let mut head = current;
    let snapshots_by_id: std::collections::HashMap<SolutionSessionId, &SessionSnapshot> =
        snapshots.iter().map(|s| (s.id, s)).collect();
    for _ in 0..snapshots.len() {
        match snapshots_by_id.get(&head) {
            Some(snap) => match snap.parent_session_id {
                Some(parent) if snapshots_by_id.contains_key(&parent) => {
                    head = parent;
                }
                _ => return head,
            },
            None => return head,
        }
    }
    head
}

/// DFS from `root` collecting `(snapshot, indent_level)` in pre-order.
/// Children of a node are sorted by `created_at` ASC so the order
/// matches the server-side `get_session_children` listing.
pub(super) fn dfs_collect(
    root: SolutionSessionId,
    snapshots: &[SessionSnapshot],
) -> Vec<(&SessionSnapshot, u32)> {
    let by_id: std::collections::HashMap<SolutionSessionId, &SessionSnapshot> =
        snapshots.iter().map(|s| (s.id, s)).collect();
    let mut children_of: std::collections::HashMap<SolutionSessionId, Vec<&SessionSnapshot>> =
        std::collections::HashMap::new();
    for snap in snapshots {
        if let Some(parent) = snap.parent_session_id
            && by_id.contains_key(&parent)
        {
            children_of.entry(parent).or_default().push(snap);
        }
    }
    for kids in children_of.values_mut() {
        kids.sort_by_key(|s| s.created_at);
    }
    let mut out = Vec::new();
    let mut stack: Vec<(SolutionSessionId, u32)> = Vec::new();
    stack.push((root, 0));
    // Visited set guards against the same self-cycle pathology
    // `find_root` defends against — DFS keeps moving down via a child
    // map, so a cycle only matters if `parent_session_id` somehow
    // pointed back at an ancestor; the visited set bounds the walk.
    let mut visited: std::collections::HashSet<SolutionSessionId> =
        std::collections::HashSet::new();
    while let Some((id, depth)) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Some(snap) = by_id.get(&id) else {
            continue;
        };
        out.push((*snap, depth));
        if let Some(kids) = children_of.get(&id) {
            // Push in reverse so the lowest `created_at` is popped
            // first → pre-order matches "oldest sibling first".
            for kid in kids.iter().rev() {
                stack.push((kid.id, depth.saturating_add(1)));
            }
        }
    }
    out
}

/// Build the row list for the strip given the pre-computed
/// snapshots. Returns `None` when the strip should hide entirely —
/// i.e. the tree only contains the current session and no children.
/// Truncates at [`MAX_STRIP_ROWS`] with a single overflow row at the
/// bottom, so a 50-child fanout still paints in a fixed band of
/// space.
pub(super) fn compute_strip_rows(
    current_id: SolutionSessionId,
    current_solution: &SolutionId,
    snapshots: &[SessionSnapshot],
) -> Option<Vec<StripRow>> {
    let same_solution: Vec<SessionSnapshot> = snapshots
        .iter()
        .filter(|s| &s.solution_id == current_solution)
        .cloned()
        .collect();
    if same_solution.is_empty() {
        return None;
    }
    let root = find_root(current_id, &same_solution);
    let visited = dfs_collect(root, &same_solution);
    // Strip hides for top-level-with-no-children. `visited.len() ==
    // 1` is the same predicate either way: a top-level session with
    // children would yield ≥ 2, a sub-agent has at least its parent
    // in the chain.
    if visited.len() <= 1 {
        return None;
    }
    let visible_cap = MAX_STRIP_ROWS;
    let (visible, hidden) = if visited.len() > visible_cap {
        // Reserve one slot for the overflow row so the truncation
        // marker is always rendered. `visible_cap - 1` is safe
        // because `MAX_STRIP_ROWS` is a const ≥ 2.
        let keep = visible_cap.saturating_sub(1);
        (&visited[..keep], visited.len() - keep)
    } else {
        (&visited[..], 0)
    };
    let mut rows: Vec<StripRow> = visible
        .iter()
        .map(|(snap, depth)| StripRow {
            kind: StripRowKind::Session {
                session_id: snap.id,
                state_label: snap.state_label,
                state_color: snap.state_color,
                total_tokens: snap.total_tokens,
                is_current: snap.id == current_id,
            },
            label: snap.title.clone(),
            indent_level: *depth,
        })
        .collect();
    if hidden > 0 {
        rows.push(StripRow {
            kind: StripRowKind::Overflow,
            label: SharedString::from(format!("… +{hidden} more")),
            indent_level: 0,
        });
    }
    Some(rows)
}

/// Render the strip above the status row. Returns `None` when the
/// strip should hide entirely (top-level session with no children),
/// so the caller can `.when_some(...)` it without reserving layout
/// space.
pub(super) fn render_subagent_strip(
    session: &Entity<SolutionSession>,
    store: &Entity<SolutionAgentStore>,
    _window: &mut Window,
    cx: &mut Context<SolutionSessionView>,
) -> Option<AnyElement> {
    let (current_id, solution_id) = {
        let s = session.read(cx);
        (s.id, s.solution_id.clone())
    };
    let snapshots = collect_snapshots(store, &solution_id, cx);
    let rows = compute_strip_rows(current_id, &solution_id, &snapshots)?;

    let container = v_flex()
        .id("solution-session-subagent-strip")
        .flex_none()
        .w_full()
        .border_t_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().surface_background)
        .py_0p5();

    let strip = rows.into_iter().fold(container, |this, row| {
        this.child(render_row(row, store.clone(), cx))
    });
    Some(strip.into_any_element())
}

fn render_row(
    row: StripRow,
    store: Entity<SolutionAgentStore>,
    cx: &mut Context<SolutionSessionView>,
) -> AnyElement {
    let indent_px = px(20.0 * row.indent_level as f32);
    match row.kind {
        StripRowKind::Overflow => h_flex()
            .id(SharedString::from("subagent-strip-overflow"))
            .w_full()
            .gap_2()
            .px_3()
            .py_1()
            .pl(indent_px + px(12.0))
            .child(
                Label::new(row.label)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element(),
        StripRowKind::Session {
            session_id,
            state_label,
            state_color,
            total_tokens,
            is_current,
        } => {
            // Filled dot for the current session, hollow Circle for
            // others. `IconName::Bullet` doesn't exist in the fork's
            // icon set — `Circle` (filled when tinted, muted-hollow
            // when not) is the closest match and matches the dot
            // language the tab strip already uses for status.
            let dot = if is_current {
                Icon::new(IconName::Circle)
                    .size(IconSize::XSmall)
                    .color(state_color.to_ui())
            } else {
                Icon::new(IconName::Circle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
            };
            let tokens_label: Option<AnyElement> = total_tokens.filter(|t| *t > 0).map(|t| {
                Label::new(SharedString::from(format!(
                    "· {} tokens",
                    abbreviate_tokens(t)
                )))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element()
            });
            let row_id = SharedString::from(format!("subagent-strip-row-{}", session_id.as_str()));
            let title_text = row.label.clone();
            let tooltip_text = SharedString::from(format!("Switch to {}", title_text));
            h_flex()
                .id(row_id)
                .w_full()
                .gap_2()
                .px_3()
                .py_1()
                .pl(indent_px + px(8.0))
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().colors().element_hover))
                .tooltip(Tooltip::text(tooltip_text))
                .child(dot)
                .child(Label::new(title_text).size(LabelSize::Small).truncate())
                .child(
                    Label::new(SharedString::from(state_label))
                        .size(LabelSize::XSmall)
                        .color(state_color.to_ui()),
                )
                .when_some(tokens_label, |this, el| this.child(el))
                .on_click(cx.listener(move |this, _, window, cx| {
                    switch_to_session(this, &store, session_id, window, cx);
                }))
                .into_any_element()
        }
    }
}

/// Click-target for a row: route the switch through the navigator's
/// `open_session` — same path the tab strip uses on mouse-down and
/// the History popover uses on "open historic session". If the
/// target session isn't currently in the tab strip, `open_session`
/// will append it and select it; if it is, it just toggles
/// `selected_index`. In-process only, no MCP round-trip.
fn switch_to_session(
    view: &mut SolutionSessionView,
    store: &Entity<SolutionAgentStore>,
    target: SolutionSessionId,
    window: &mut Window,
    cx: &mut Context<SolutionSessionView>,
) {
    if store.read(cx).session(target).is_none() {
        log::warn!("subagent strip click: session {target} no longer present in store");
        return;
    }
    // TODO(B10): once ConsolePanel owns the chat-tab strip, route the
    // subagent click into ConsolePanel's open-or-focus API so the user
    // jumps to the subagent's session as a sibling tab. Until then this
    // click is a no-op (logged); the user can still open the subagent
    // from the History popover.
    let _ = view;
    let _ = window;
    log::info!(
        "subagent-strip click → session {target}: tab routing parked until ConsolePanel owns it (B10)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, parent: Option<&str>, title: &str, created_secs: i64) -> SessionSnapshot {
        let id = SolutionSessionId::parse(&pad8(id)).expect("id");
        let parent = parent.map(|p| SolutionSessionId::parse(&pad8(p)).expect("parent id"));
        SessionSnapshot {
            id,
            parent_session_id: parent,
            solution_id: SolutionId("sol-a".into()),
            title: SharedString::from(title.to_string()),
            state_label: "idle",
            state_color: StripStateColor::Success,
            total_tokens: None,
            created_at: chrono::DateTime::<chrono::Utc>::from_timestamp(created_secs, 0)
                .expect("ts"),
        }
    }

    /// `SolutionSessionId::parse` requires exactly 8 lower-case base36
    /// chars. The test helpers want short readable handles ("a",
    /// "b1", "child2") — pad them to 8 here.
    fn pad8(s: &str) -> String {
        let mut out: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        while out.len() < 8 {
            out.push('0');
        }
        out.truncate(8);
        out
    }

    #[test]
    fn top_level_with_no_children_returns_none() {
        let parent = snap("a", None, "alpha", 1);
        let rows = compute_strip_rows(parent.id, &SolutionId("sol-a".into()), &[parent]);
        assert!(
            rows.is_none(),
            "strip should hide for a lone top-level session"
        );
    }

    #[test]
    fn parent_and_one_child_two_rows_correct_indent_and_current() {
        let parent = snap("a", None, "alpha", 1);
        let child = snap("b", Some("a"), "beta", 2);
        let child_id = child.id;
        let rows = compute_strip_rows(child_id, &SolutionId("sol-a".into()), &[parent, child])
            .expect("strip rows present");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].indent_level, 0);
        assert_eq!(rows[0].label.as_ref(), "alpha");
        assert_eq!(rows[1].indent_level, 1);
        assert_eq!(rows[1].label.as_ref(), "beta");
        match &rows[0].kind {
            StripRowKind::Session { is_current, .. } => {
                assert!(!is_current, "parent is not current")
            }
            _ => panic!("expected Session row"),
        }
        match &rows[1].kind {
            StripRowKind::Session { is_current, .. } => assert!(*is_current, "child is current"),
            _ => panic!("expected Session row"),
        }
    }

    #[test]
    fn dfs_order_is_oldest_first_with_grandchild_inline() {
        // parent
        //   ├── child1 (created at t=2)
        //   │     └── grandchild1 (created at t=4)
        //   └── child2 (created at t=3)
        let parent = snap("a", None, "alpha", 1);
        let child1 = snap("b", Some("a"), "beta", 2);
        let child2 = snap("c", Some("a"), "gamma", 3);
        let grandchild = snap("d", Some("b"), "delta", 4);
        let parent_id = parent.id;
        let snaps = vec![parent, child2, child1, grandchild];
        let rows = compute_strip_rows(parent_id, &SolutionId("sol-a".into()), &snaps)
            .expect("strip rows present");
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_ref()).collect();
        assert_eq!(labels, vec!["alpha", "beta", "delta", "gamma"]);
        let indents: Vec<u32> = rows.iter().map(|r| r.indent_level).collect();
        assert_eq!(indents, vec![0, 1, 2, 1]);
    }

    #[test]
    fn overflow_truncates_with_pseudo_row() {
        // 1 parent + 14 children == 15 sessions; cap is 12, so 11
        // session rows + 1 overflow row.
        let parent = snap("a", None, "root", 1);
        let parent_id = parent.id;
        let mut snaps = vec![parent];
        for i in 0..14 {
            let cid = format!("c{i:02}");
            snaps.push(snap(&cid, Some("a"), &format!("kid{i}"), 10 + i as i64));
        }
        let rows = compute_strip_rows(parent_id, &SolutionId("sol-a".into()), &snaps)
            .expect("strip rows present");
        assert_eq!(rows.len(), MAX_STRIP_ROWS);
        let last = rows.last().expect("last row");
        assert!(matches!(last.kind, StripRowKind::Overflow));
        assert!(
            last.label.as_ref().starts_with("… +"),
            "overflow label: {:?}",
            last.label
        );
        // 15 total sessions; we keep 11 sessions + 1 overflow = 12.
        // Hidden count therefore = 15 - 11 = 4.
        assert_eq!(last.label.as_ref(), "… +4 more");
    }

    #[test]
    fn cross_solution_session_is_filtered_out() {
        let parent = snap("a", None, "alpha", 1);
        let child = snap("b", Some("a"), "beta", 2);
        let mut other = snap("c", Some("a"), "outsider", 3);
        other.solution_id = SolutionId("sol-other".into());
        let child_id = child.id;
        let rows = compute_strip_rows(
            child_id,
            &SolutionId("sol-a".into()),
            &[parent, child, other],
        )
        .expect("strip rows present");
        assert_eq!(rows.len(), 2, "outsider should not appear in the strip");
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_ref()).collect();
        assert_eq!(labels, vec!["alpha", "beta"]);
    }

    #[test]
    fn abbreviate_tokens_formats_thresholds() {
        assert_eq!(abbreviate_tokens(0), "0");
        assert_eq!(abbreviate_tokens(999), "999");
        assert_eq!(abbreviate_tokens(1_000), "1.0k");
        assert_eq!(abbreviate_tokens(138_300), "138.3k");
        assert_eq!(abbreviate_tokens(1_200_000), "1.2M");
    }

    /// Verify that a live judge session excluded from snapshots (mirroring
    /// what `collect_snapshots` does after reading
    /// `store.live_supervisor_session_ids()`)
    /// causes the strip to hide when the supervised session is alone.
    /// This is the "Fix 2: judge in subagent strip" regression guard.
    #[test]
    fn judge_excluded_from_snapshots_strip_hides_for_lone_supervised_session() {
        let supervised = snap("aa", None, "Supervised", 1);
        // The judge is parent-linked to the supervised session but is
        // excluded by collect_snapshots before compute_strip_rows runs.
        // We simulate this by simply not including it in the snapshot slice.
        let rows = compute_strip_rows(supervised.id, &SolutionId("sol-a".into()), &[supervised]);
        assert!(
            rows.is_none(),
            "strip should hide when judge is excluded and only the supervised session remains"
        );
    }

    /// When a judge snapshot is NOT excluded (pre-fix behaviour), a child
    /// bubble appears. After the fix collect_snapshots removes it first, so
    /// compute_strip_rows only ever sees the filtered list. This test
    /// documents the pre-fix leak for clarity: a judge snapshot present in
    /// the list WOULD produce two rows (supervised + judge child).
    #[test]
    fn judge_snapshot_present_would_produce_two_rows_demonstrating_pre_fix_leak() {
        let supervised = snap("aa", None, "Supervised", 1);
        let judge = snap("bb", Some("aa"), "Judge", 2);
        let rows = compute_strip_rows(
            supervised.id,
            &SolutionId("sol-a".into()),
            &[supervised, judge],
        );
        assert!(
            rows.is_some(),
            "without filtering, a judge child produces strip rows"
        );
        assert_eq!(
            rows.unwrap().len(),
            2,
            "pre-fix: two rows (supervised root + judge child)"
        );
    }
}
