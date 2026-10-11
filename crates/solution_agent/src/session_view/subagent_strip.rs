//! Related Solution sessions use flat tabs below the status row, like Task streams.
//! Idle children are not live work; keep only a selected idle result reachable until Main is selected.

use chrono::{DateTime, Utc};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Window};
use solutions::SolutionId;
use ui::prelude::*;

use super::SolutionSessionView;
use crate::model::{SessionState, SolutionSession, SolutionSessionId};
use crate::store::SolutionAgentStore;

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

/// Compact "138.3k" / "1.2M" / "457" formatter for the token suffix.
/// Output is meant to be appended to a `· ` separator inside the row;
/// the unit ("tokens") is added by the caller so the same helper can
/// be reused for a tooltip if we ever want one.
#[cfg(test)]
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
            !hidden_ids.contains(&s.id) && !s.is_supervisor_ephemeral && !s.is_ephemeral
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
                solution_id: s.solution_id,
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
/// Every live descendant is reachable through horizontal scrolling; the band has a fixed height.
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
    let visited: Vec<_> = dfs_collect(root, &same_solution)
        .into_iter()
        .filter(|(snap, _)| snap.id == root || snap.id == current_id || snap.state_label != "idle")
        .collect();
    // Strip hides for top-level-with-no-children. `visited.len() ==
    // 1` is the same predicate either way: a top-level session with
    // children would yield ≥ 2, a sub-agent has at least its parent
    // in the chain.
    if visited.len() <= 1 {
        return None;
    }
    let rows = visited
        .iter()
        .map(|(snap, _)| StripRow {
            kind: StripRowKind::Session {
                session_id: snap.id,
                state_label: snap.state_label,
                state_color: snap.state_color,
                total_tokens: snap.total_tokens,
                is_current: snap.id == current_id,
            },
            label: if snap.id == root {
                "Main".into()
            } else {
                snap.title.clone()
            },
            indent_level: 0,
        })
        .collect();
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
        (s.id, s.solution_id)
    };
    let snapshots = collect_snapshots(store, &solution_id, cx);
    let rows = compute_strip_rows(current_id, &solution_id, &snapshots)?;

    let root = store.read(cx).related_dialog_root(current_id, cx)?;
    let mut strip = h_flex()
        .id("solution-session-subagent-strip")
        .debug_selector(|| "RELATED-SESSION-TABS".into())
        .flex_none()
        .w_full()
        .gap_1()
        .px_2()
        .py_1()
        .overflow_x_scroll()
        .border_t_1()
        .border_color(cx.theme().colors().border)
        .bg(cx.theme().colors().panel_background);
    for row in rows {
        let StripRowKind::Session {
            session_id,
            is_current,
            ..
        } = row.kind;
        let store = store.clone();
        strip = strip.child(super::task_subagent_strip::pill(
            format!("related-session-tab-{session_id}").into(),
            row.label,
            is_current,
            cx,
            move |_, _, _, cx| {
                store.update(cx, |store, cx| {
                    store.select_subagent_dialog(root, session_id, cx);
                });
            },
        ));
    }
    Some(strip.into_any_element())
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
            solution_id: SolutionId(1),
            title: SharedString::from(title.to_string()),
            state_label: "running",
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
        let rows = compute_strip_rows(parent.id, &SolutionId(1), &[parent]);
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
        let rows = compute_strip_rows(child_id, &SolutionId(1), &[parent, child])
            .expect("strip rows present");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].indent_level, 0);
        assert_eq!(rows[0].label.as_ref(), "Main");
        assert_eq!(rows[1].indent_level, 0);
        assert_eq!(rows[1].label.as_ref(), "beta");
        match &rows[0].kind {
            StripRowKind::Session { is_current, .. } => {
                assert!(!is_current, "parent is not current")
            }
        }
        match &rows[1].kind {
            StripRowKind::Session { is_current, .. } => assert!(*is_current, "child is current"),
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
        let rows =
            compute_strip_rows(parent_id, &SolutionId(1), &snaps).expect("strip rows present");
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_ref()).collect();
        assert_eq!(labels, vec!["Main", "beta", "delta", "gamma"]);
        let indents: Vec<u32> = rows.iter().map(|r| r.indent_level).collect();
        assert_eq!(indents, vec![0, 0, 0, 0]);
    }

    #[test]
    fn fanout_keeps_all_flat_tabs_reachable() {
        let parent = snap("a", None, "root", 1);
        let id = parent.id;
        let mut snaps = vec![parent];
        for i in 0..30 {
            snaps.push(snap(&format!("c{i:02}"), Some("a"), "child", i + 2));
        }
        let rows = compute_strip_rows(id, &SolutionId(1), &snaps).unwrap();
        assert_eq!(rows.len(), 31);
        assert!(rows.iter().all(|row| row.indent_level == 0));
    }

    #[test]
    fn idle_children_hide_without_closing_or_losing_selected_result() {
        let parent = snap("a", None, "root", 1);
        let mut child = snap("b", Some("a"), "result", 2);
        child.state_label = "idle";
        assert!(
            compute_strip_rows(parent.id, &SolutionId(1), &[parent.clone(), child.clone()])
                .is_none()
        );
        assert_eq!(
            compute_strip_rows(child.id, &SolutionId(1), &[parent, child])
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn cross_solution_session_is_filtered_out() {
        let parent = snap("a", None, "alpha", 1);
        let child = snap("b", Some("a"), "beta", 2);
        let mut other = snap("c", Some("a"), "outsider", 3);
        other.solution_id = SolutionId(8);
        let child_id = child.id;
        let rows = compute_strip_rows(child_id, &SolutionId(1), &[parent, child, other])
            .expect("strip rows present");
        assert_eq!(rows.len(), 2, "outsider should not appear in the strip");
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_ref()).collect();
        assert_eq!(labels, vec!["Main", "beta"]);
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
        let rows = compute_strip_rows(supervised.id, &SolutionId(1), &[supervised]);
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
        let rows = compute_strip_rows(supervised.id, &SolutionId(1), &[supervised, judge]);
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
    #[gpui::test]
    async fn selecting_child_keeps_root_tab_and_scopes_the_visible_dialog(
        cx: &mut gpui::TestAppContext,
    ) {
        let (solution, _tmp, project) = crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            crate::store::SolutionAgentStore::init_global(
                cx,
                std::sync::Arc::new(crate::adapter::AdapterRegistry::new()),
            )
        });
        let store = cx.update(|cx| crate::store::SolutionAgentStore::global(cx));
        store.update(cx, |store, cx| {
            let root = store
                .create_unstarted_session(solution, "codex-native".into(), project.clone(), cx)
                .unwrap();
            let child = store
                .create_unstarted_session(solution, "codex-native".into(), project.clone(), cx)
                .unwrap();
            store.session(child).unwrap().update(cx, |child, _| {
                child.parent_session_id = Some(root);
                child.tab_order = None;
            });
            store.set_active_dialog_session(solution, Some(root), cx);
            assert!(store.select_subagent_dialog(root, child, cx));
            assert_eq!(store.active_dialog_session(solution), Some(root));
            assert_eq!(store.visible_dialog_session(solution, cx), Some(child));
            assert!(store.session(child).unwrap().read(cx).tab_order.is_none());
            assert!(store.select_subagent_dialog(root, root, cx));
            assert_eq!(store.visible_dialog_session(solution, cx), Some(root));
            store
                .session(child)
                .unwrap()
                .update(cx, |child, _| child.tab_order = Some(1));
            store.set_active_dialog_session(solution, Some(child), cx);
            assert!(store.select_subagent_dialog(root, root, cx));
            assert_eq!(store.visible_dialog_session(solution, cx), Some(root));
            store
                .session(child)
                .unwrap()
                .update(cx, |child, _| child.is_ephemeral = true);
            assert!(!store.select_subagent_dialog(root, child, cx));
            store.session(child).unwrap().update(cx, |child, _| {
                child.is_ephemeral = false;
                child.solution_id = SolutionId(999);
            });
            assert!(!store.select_subagent_dialog(root, child, cx));
            store.session(child).unwrap().update(cx, |child, _| {
                child.solution_id = solution;
                child.parent_session_id = Some(child.id);
            });
            assert!(!store.select_subagent_dialog(root, child, cx));
        });
    }
}
