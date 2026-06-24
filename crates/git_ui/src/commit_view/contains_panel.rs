//! "Contains" panel — branches / tags that contain the commit.
//!
//! Loads asynchronously from `Repository::branches_containing` /
//! `tags_containing`; collapses when the combined list exceeds five entries.

use gpui::{AnyElement, Entity, ParentElement, Styled, Task, prelude::*};
use project::git_store::Repository;
use ui::prelude::*;
use util::ResultExt as _;

const COLLAPSE_THRESHOLD: usize = 5;

/// State for the "Contains" panel. The widget loads lazily — we don't
/// want to spawn `git branch --contains` for every commit shown in
/// `git_graph` if the user never opens the detail surface.
pub(crate) struct CommitContainsPanel {
    branches: Vec<SharedString>,
    tags: Vec<SharedString>,
    expanded: bool,
    loading: bool,
    _load_task: Option<Task<()>>,
}

impl CommitContainsPanel {
    pub(crate) fn new() -> Self {
        Self {
            branches: Vec::new(),
            tags: Vec::new(),
            expanded: false,
            loading: false,
            _load_task: None,
        }
    }

    pub(crate) fn load(
        &mut self,
        sha: String,
        repository: Entity<Repository>,
        cx: &mut Context<crate::commit_view::CommitView>,
    ) {
        if self.loading {
            return;
        }
        self.loading = true;
        let branches_rx = repository.update(cx, |repo, _| repo.branches_containing(sha.clone()));
        let tags_rx = repository.update(cx, |repo, _| repo.tags_containing(sha));
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let (branches_res, tags_res) = futures::join!(branches_rx, tags_rx);
            let branches = branches_res
                .ok()
                .and_then(|inner| inner.log_err())
                .unwrap_or_default();
            let tags = tags_res
                .ok()
                .and_then(|inner| inner.log_err())
                .unwrap_or_default();
            this.update(cx, |view, cx| {
                view.contains_panel.branches = branches;
                view.contains_panel.tags = tags;
                view.contains_panel.loading = false;
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn render(
        &self,
        cx: &mut Context<crate::commit_view::CommitView>,
    ) -> Option<AnyElement> {
        if self.branches.is_empty() && self.tags.is_empty() {
            return None;
        }
        let total = self.branches.len() + self.tags.len();
        let collapsed = total > COLLAPSE_THRESHOLD && !self.expanded;
        let visible_branches: &[SharedString] = if collapsed {
            // When collapsed show at most 3 branches inline plus the toggle.
            let n = self.branches.len().min(3);
            &self.branches[..n]
        } else {
            &self.branches[..]
        };
        let visible_tags: &[SharedString] = if collapsed {
            let n = self.tags.len().min(2);
            &self.tags[..n]
        } else {
            &self.tags[..]
        };

        let mut row = h_flex().flex_wrap().gap_1().child(
            Label::new("Contains")
                .size(LabelSize::Small)
                .color(Color::Muted),
        );

        for (ix, branch) in visible_branches.iter().enumerate() {
            row = row.child(chip(
                ix,
                "br",
                IconName::GitBranch,
                branch.clone(),
                Color::Muted,
            ));
        }
        for (ix, tag) in visible_tags.iter().enumerate() {
            row = row.child(chip(ix, "tg", IconName::Bookmark, tag.clone(), Color::Info));
        }

        if total > COLLAPSE_THRESHOLD {
            let label = if collapsed {
                format!(
                    "+{} more",
                    total - visible_branches.len() - visible_tags.len()
                )
            } else {
                "Show less".to_string()
            };
            row = row.child(
                Button::new("contains-toggle", label)
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .color(Color::Accent)
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.contains_panel.expanded = !view.contains_panel.expanded;
                        cx.notify();
                    })),
            );
        }
        Some(row.into_any_element())
    }
}

fn chip(
    ix: usize,
    kind: &'static str,
    icon: IconName,
    label: SharedString,
    color: Color,
) -> AnyElement {
    h_flex()
        .id(SharedString::from(format!("contains-{kind}-{ix}")))
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .border_1()
        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
        .child(Label::new(label).size(LabelSize::Small).color(color))
        .into_any_element()
}
