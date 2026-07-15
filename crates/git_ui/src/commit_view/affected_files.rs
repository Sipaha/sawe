//! Affected files component — the changed paths in a commit, rendered as an
//! IntelliJ-IDEA-style collapsible directory tree: folders (with single-child
//! chains compacted and a per-folder file count) that expand/collapse, and file
//! leaves carrying a status icon. For large commits a fuzzy filter + lazy-load
//! window bound the underlying file set before the tree is built.

use std::collections::{BTreeMap, HashSet};

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
/// Horizontal indent per tree depth level (matches `git_panel::TREE_INDENT`).
const INDENT: f32 = 16.0;

pub(crate) struct CommitAffectedFiles {
    pub(crate) filter_editor: Entity<Editor>,
    pub(crate) visible_count: usize,
    pub(crate) lazy_threshold: usize,
    /// Directories the user has collapsed, keyed by full unix dir path. Default
    /// (absent) = expanded, matching IDEA's fully-expanded initial tree.
    pub(crate) collapsed_dirs: HashSet<String>,
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
            collapsed_dirs: HashSet::new(),
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

    let rows = build_tree(&visible, &state.collapsed_dirs);
    let hover_bg = cx.theme().colors().element_hover;

    let mut list = v_flex().gap_0p5();
    for row in rows {
        list = list.child(render_tree_row(row, hover_bg, cx));
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

/// One flattened tree row: a directory header or a file leaf.
enum TreeRow<'a> {
    Dir {
        /// Full unix path of the directory (the collapse-state key).
        key: String,
        /// Display name — possibly a compacted chain like `a/b/c`.
        name: String,
        depth: usize,
        file_count: usize,
        collapsed: bool,
    },
    File {
        file: &'a CommitFile,
        depth: usize,
    },
}

#[derive(Default)]
struct Node<'a> {
    dirs: BTreeMap<String, Node<'a>>,
    files: Vec<&'a CommitFile>,
}

fn count_files(node: &Node) -> usize {
    node.files.len() + node.dirs.values().map(count_files).sum::<usize>()
}

/// Build the flattened, depth-tagged tree for the given files, honoring the
/// user's collapsed directories. Directories sort before their files, both
/// alphabetically; single-child directory chains are compacted onto one row.
fn build_tree<'a>(files: &[&'a CommitFile], collapsed: &HashSet<String>) -> Vec<TreeRow<'a>> {
    let mut root = Node::default();
    for file in files {
        let components: Vec<&str> = file.path.components().collect();
        let Some((_, dirs)) = components.split_last() else {
            continue;
        };
        let mut node = &mut root;
        for dir in dirs {
            node = node.dirs.entry((*dir).to_string()).or_default();
        }
        node.files.push(file);
    }

    let mut out = Vec::new();
    flatten(&root, "", 0, collapsed, &mut out);
    out
}

fn flatten<'a>(
    node: &Node<'a>,
    prefix: &str,
    depth: usize,
    collapsed: &HashSet<String>,
    out: &mut Vec<TreeRow<'a>>,
) {
    for (name, child) in &node.dirs {
        // Compact single-child chains: `a` -> `a/b/c` while the descendant is a
        // lone directory with no files of its own.
        let mut display_name = name.clone();
        let mut tail = name.clone();
        let mut cur = child;
        while cur.files.is_empty() && cur.dirs.len() == 1 {
            let (child_name, child_node) = cur
                .dirs
                .iter()
                .next()
                .expect("len checked to be 1");
            display_name = format!("{display_name}/{child_name}");
            tail = format!("{tail}/{child_name}");
            cur = child_node;
        }
        let key = if prefix.is_empty() {
            tail
        } else {
            format!("{prefix}/{tail}")
        };
        let is_collapsed = collapsed.contains(&key);
        out.push(TreeRow::Dir {
            file_count: count_files(cur),
            name: display_name,
            depth,
            collapsed: is_collapsed,
            key: key.clone(),
        });
        if !is_collapsed {
            flatten(cur, &key, depth + 1, collapsed, out);
        }
    }

    let mut files = node.files.clone();
    files.sort_by(|a, b| a.path.as_unix_str().cmp(b.path.as_unix_str()));
    for file in files {
        out.push(TreeRow::File { file, depth });
    }
}

fn render_tree_row(
    row: TreeRow,
    hover_bg: gpui::Hsla,
    cx: &mut Context<crate::commit_view::CommitView>,
) -> AnyElement {
    match row {
        TreeRow::Dir {
            key,
            name,
            depth,
            file_count,
            collapsed,
        } => {
            let folder_icon = if collapsed {
                IconName::Folder
            } else {
                IconName::FolderOpen
            };
            let toggle_key = key.clone();
            div()
                .id(SharedString::from(format!("affected-dir-{key}")))
                .w_full()
                .rounded_sm()
                .cursor_pointer()
                .hover(move |this| this.bg(hover_bg))
                .on_click(cx.listener(move |view, _, _, cx| {
                    if !view.affected_files.collapsed_dirs.remove(&toggle_key) {
                        view.affected_files
                            .collapsed_dirs
                            .insert(toggle_key.clone());
                    }
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .gap_1p5()
                        .py_0p5()
                        .px_1()
                        .pl(px(depth as f32 * INDENT + 4.0))
                        .child(
                            Icon::new(folder_icon)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(name).size(LabelSize::Small))
                        .child(
                            Label::new(format!(
                                "{} file{}",
                                file_count,
                                if file_count == 1 { "" } else { "s" }
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
                .into_any_element()
        }
        TreeRow::File { file, depth } => {
            let name = file
                .path
                .components()
                .last()
                .map(|component| component.to_string())
                .unwrap_or_else(|| file.path.as_unix_str().to_string());
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
                .id(SharedString::from(format!(
                    "affected-file-{}",
                    file.path.as_unix_str()
                )))
                .gap_1p5()
                .py_0p5()
                .px_1()
                .pl(px(depth as f32 * INDENT + 4.0))
                .rounded_sm()
                .child(GitStatusIcon::new(file_status))
                .child(Label::new(name).size(LabelSize::Small))
                .into_any_element()
        }
    }
}
