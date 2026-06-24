//! Reusable mini-graph component — a vertical commit chain (no lanes,
//! no detail panel). Extracted from the full git-graph view for use in
//! contexts where a simple commit list is enough: the S-PSH push dialog
//! preview, future S-SOL-PSH cross-repo aggregation, etc.
//!
//! The widget is intentionally cheap and stateless — it renders whatever
//! list of [`MiniCommit`]s the caller hands it, fires a callback when a
//! row is clicked, and otherwise leaves selection state to the parent.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, IntoElement, ParentElement, SharedString, Styled,
    UniformListScrollHandle, uniform_list,
};
use time::{OffsetDateTime, UtcOffset};
use ui::prelude::*;
use ui::{Label, LabelCommon, LabelSize};

/// Single commit row in a [`MiniGraph`]. The committer date is stored as
/// raw unix seconds so callers don't need to format ahead of time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiniCommit {
    pub sha: String,
    pub subject: String,
    pub author_email: String,
    pub committer_date_unix: i64,
}

impl MiniCommit {
    /// First seven chars of `sha`, matching git's default short-sha length.
    pub fn short_sha(&self) -> String {
        self.sha.chars().take(7).collect()
    }
}

/// Vertical list of [`MiniCommit`]s with click-to-select. The widget owns
/// the list of commits; the *selected* index lives on the caller — pass it
/// in via [`MiniGraph::selected`].
pub struct MiniGraph {
    commits: Vec<MiniCommit>,
    selected: Option<usize>,
}

impl MiniGraph {
    pub fn new(commits: Vec<MiniCommit>) -> Self {
        Self {
            commits,
            selected: None,
        }
    }

    pub fn with_selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    pub fn len(&self) -> usize {
        self.commits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }

    pub fn commit(&self, ix: usize) -> Option<&MiniCommit> {
        self.commits.get(ix)
    }

    /// Render the graph as a uniform-list element. `on_select(ix, cx)` is
    /// called with the row index and an `&mut App` when a commit row is
    /// clicked, so the caller can dispatch entity updates.
    pub fn render<F>(self, on_select: F, _cx: &mut App) -> impl IntoElement
    where
        F: Fn(usize, &mut App) + 'static,
    {
        let on_select = Rc::new(on_select);
        let row_count = self.commits.len();
        let commits = self.commits;
        let selected = self.selected;
        let scroll_handle = UniformListScrollHandle::new();

        if row_count == 0 {
            return div()
                .id("mini-graph-empty")
                .py_4()
                .px_3()
                .child(
                    Label::new("No commits to push.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

        uniform_list(
            "mini-graph-list",
            row_count,
            move |range: std::ops::Range<usize>, _window, cx| {
                let mut elements: Vec<AnyElement> = Vec::with_capacity(range.len());
                for ix in range {
                    let Some(commit) = commits.get(ix) else {
                        continue;
                    };
                    let is_selected = selected == Some(ix);
                    let on_select = on_select.clone();
                    elements.push(render_row(ix, commit, is_selected, on_select, cx));
                }
                elements
            },
        )
        .track_scroll(&scroll_handle)
        .h_full()
        .w_full()
        .into_any_element()
    }
}

fn render_row(
    ix: usize,
    commit: &MiniCommit,
    is_selected: bool,
    on_select: Rc<dyn Fn(usize, &mut App)>,
    cx: &mut App,
) -> AnyElement {
    let subject: SharedString = commit.subject.clone().into();
    let short: SharedString = commit.short_sha().into();
    let date: SharedString = format_relative(commit.committer_date_unix).into();
    let bg = if is_selected {
        cx.theme().colors().element_selected
    } else {
        gpui::transparent_black()
    };

    div()
        .id(SharedString::from(format!("mini-graph-row-{ix}")))
        .px_2()
        .py_1()
        .gap_2()
        .bg(bg)
        .hover(|s| s.bg(cx.theme().colors().element_hover))
        .on_click(move |_event: &ClickEvent, _window, cx| {
            (on_select)(ix, cx);
        })
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    Label::new(subject)
                        .size(LabelSize::Small)
                        .color(Color::Default)
                        .truncate(),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(short)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(Label::new(date).size(LabelSize::XSmall).color(Color::Muted)),
                ),
        )
        .into_any_element()
}

/// Format a unix timestamp as a relative-from-now string ("3m ago"). Used
/// by the mini-graph row metadata. Falls back to the absolute date when
/// the system clock is somehow before the commit.
pub fn format_relative(committer_date_unix: i64) -> String {
    let Ok(then) = OffsetDateTime::from_unix_timestamp(committer_date_unix) else {
        return "unknown".into();
    };
    let now = OffsetDateTime::now_utc();
    let diff = now - then;
    let seconds = diff.whole_seconds();
    if seconds < 0 {
        let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let local = then.to_offset(local_offset);
        return local
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown".into());
    }
    if seconds < 60 {
        return "just now".into();
    }
    if seconds < 3_600 {
        return format!("{}m ago", seconds / 60);
    }
    if seconds < 86_400 {
        return format!("{}h ago", seconds / 3_600);
    }
    if seconds < 86_400 * 30 {
        return format!("{}d ago", seconds / 86_400);
    }
    if seconds < 86_400 * 365 {
        return format!("{}mo ago", seconds / (86_400 * 30));
    }
    format!("{}y ago", seconds / (86_400 * 365))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(sha: &str, subject: &str, ts: i64) -> MiniCommit {
        MiniCommit {
            sha: sha.into(),
            subject: subject.into(),
            author_email: "alice@example.com".into(),
            committer_date_unix: ts,
        }
    }

    #[test]
    fn short_sha_returns_seven_chars() {
        let commit = sample("0123456789abcdef0123456789abcdef01234567", "x", 0);
        assert_eq!(commit.short_sha(), "0123456");
    }

    #[test]
    fn short_sha_handles_truncated_input() {
        let commit = sample("abc", "x", 0);
        assert_eq!(commit.short_sha(), "abc");
    }

    #[test]
    fn empty_graph_is_empty() {
        let graph = MiniGraph::new(Vec::new());
        assert!(graph.is_empty());
        assert_eq!(graph.len(), 0);
    }

    #[test]
    fn single_commit_graph() {
        let graph = MiniGraph::new(vec![sample("aaa", "first", 0)]);
        assert_eq!(graph.len(), 1);
        assert!(!graph.is_empty());
        assert_eq!(graph.commit(0).map(|c| c.subject.as_str()), Some("first"));
    }

    #[test]
    fn many_commits_graph() {
        let mut commits = Vec::new();
        for ix in 0..50 {
            commits.push(sample(
                &format!("sha{ix:040}"),
                &format!("commit {ix}"),
                ix as i64,
            ));
        }
        let graph = MiniGraph::new(commits);
        assert_eq!(graph.len(), 50);
        assert_eq!(
            graph.commit(49).map(|c| c.subject.as_str()),
            Some("commit 49")
        );
    }

    #[test]
    fn relative_under_minute_is_just_now() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert_eq!(format_relative(now - 30), "just now");
    }

    #[test]
    fn relative_minutes() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert_eq!(format_relative(now - 120), "2m ago");
    }

    #[test]
    fn relative_hours() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert_eq!(format_relative(now - 7_200), "2h ago");
    }

    #[test]
    fn relative_days() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert_eq!(format_relative(now - 86_400 * 3), "3d ago");
    }
}
