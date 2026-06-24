//! Affected files component — list of changed paths in the commit, with
//! per-file status icon, +/- counts, and (for large commits) a lazy-load
//! window plus a fuzzy filter input.

use std::sync::Arc;

use editor::Editor;
use git::repository::{CommitFile, CommitFileStatus};
use gpui::{AnyElement, Entity, ParentElement, Styled, prelude::*};
use ui::prelude::*;

use crate::GitStatusIcon;
use git::status::{FileStatus, StatusCode, TrackedStatus};

/// Default first-window size when a commit exceeds the lazy threshold.
const FIRST_WINDOW: usize = 100;
/// Page size for the "Load more" button.
const LOAD_MORE_PAGE: usize = 100;

pub(crate) struct CommitAffectedFiles {
    pub(crate) filter_editor: Entity<Editor>,
    pub(crate) visible_count: usize,
    pub(crate) lazy_threshold: usize,
}

impl CommitAffectedFiles {
    pub(crate) fn new(
        lazy_threshold: usize,
        window: &mut Window,
        cx: &mut Context<crate::commit_view::CommitView>,
    ) -> Self {
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter files…", window, cx);
            editor
        });
        Self {
            filter_editor,
            visible_count: FIRST_WINDOW,
            lazy_threshold,
        }
    }

    /// Returns `(total, slice)` — the slice obeys filter + lazy window.
    pub(crate) fn visible_files<'a>(
        &self,
        files: &'a [CommitFile],
        cx: &App,
    ) -> (usize, Vec<&'a CommitFile>) {
        let total = files.len();
        let filter_text = self.filter_editor.read(cx).text(cx);
        let filter_text = filter_text.trim();
        let filtered: Vec<&CommitFile> = if filter_text.is_empty() {
            files.iter().collect()
        } else {
            let needle_lower = filter_text.to_lowercase();
            files
                .iter()
                .filter(|file| {
                    file.path
                        .as_unix_str()
                        .to_lowercase()
                        .contains(&needle_lower)
                })
                .collect()
        };

        if total > self.lazy_threshold {
            let cap = self.visible_count.min(filtered.len());
            (filtered.len(), filtered[..cap].to_vec())
        } else {
            (filtered.len(), filtered)
        }
    }
}

pub(crate) fn render_affected_files(
    files: &[CommitFile],
    state: &CommitAffectedFiles,
    cx: &mut Context<crate::commit_view::CommitView>,
) -> AnyElement {
    let total = files.len();
    let lazy = total > state.lazy_threshold;
    let (filtered_total, visible) = state.visible_files(files, cx);

    let mut list = v_flex().gap_0p5();
    for (ix, file) in visible.iter().enumerate() {
        list = list.child(render_row(ix, file));
    }

    let header_text = if lazy {
        format!(
            "{} of {} affected files (filtered: {})",
            visible.len(),
            total,
            filtered_total
        )
    } else {
        format!(
            "{} affected file{}",
            total,
            if total == 1 { "" } else { "s" }
        )
    };

    v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .justify_between()
                .child(
                    Label::new(header_text)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .when(lazy, |this| {
                    this.child(div().w_64().child(state.filter_editor.clone()))
                }),
        )
        .child(list)
        .when(lazy && state.visible_count < filtered_total, |this| {
            this.child(
                Button::new(
                    "affected-files-load-more",
                    format!(
                        "Load more ({} hidden)",
                        filtered_total.saturating_sub(state.visible_count)
                    ),
                )
                .style(ButtonStyle::OutlinedGhost)
                .label_size(LabelSize::Small)
                .full_width()
                .on_click(cx.listener(|view, _, _, cx| {
                    view.affected_files.visible_count = view
                        .affected_files
                        .visible_count
                        .saturating_add(LOAD_MORE_PAGE);
                    cx.notify();
                })),
            )
        })
        .into_any_element()
}

fn render_row(ix: usize, file: &CommitFile) -> AnyElement {
    let path = file.path.as_unix_str().to_string();
    let status = match file.status() {
        CommitFileStatus::Added => StatusCode::Added,
        CommitFileStatus::Modified => StatusCode::Modified,
        CommitFileStatus::Deleted => StatusCode::Deleted,
    };
    let file_status = FileStatus::Tracked(TrackedStatus {
        index_status: status,
        worktree_status: StatusCode::Unmodified,
    });

    h_flex()
        .id(SharedString::from(format!("affected-file-{ix}")))
        .gap_1p5()
        .py_0p5()
        .px_1()
        .rounded_sm()
        .child(GitStatusIcon::new(file_status))
        .child(Label::new(path).size(LabelSize::Small))
        .into_any_element()
}

// Suppress unused warning for the convenience constructor used by callers.
#[allow(dead_code)]
fn _force_arc_ref(_: Arc<()>) {}
