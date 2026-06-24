//! S-BRP — IDEA-style collapsible-sections branches popup.
//!
//! Lives alongside the legacy `BranchList` (in the parent `branch_picker`
//! module) so `git_picker.rs` and `commit_modal.rs` keep working unchanged.

use std::path::Path;
use std::sync::Arc;

use editor::Editor;
use git::repository::Branch;
use gpui::{
    AnyElement, App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, MouseDownEvent, ParentElement, Render,
    SharedString, Styled, Subscription, WeakEntity, Window, actions, rems,
};
use menu::{Cancel, Confirm};
use project::git_store::{Repository, RepositoryEvent};
use time::OffsetDateTime;
use ui::{Headline, HeadlineSize, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt;
use workspace::notifications::DetachAndPromptErr;
use workspace::{ModalView, Workspace};

use super::{context_menu, favorites, tree};
use crate::git_panel::show_error_toast;

actions!(
    git,
    [
        /// Opens the collapsible-sections branches popup (S-BRP).
        BranchesPopupOpen,
        /// Toggle favorite-status for the currently-selected branch row.
        BranchesPopupToggleFavorite,
    ]
);

#[derive(Debug, Clone)]
struct BranchStatusEntry {
    name: SharedString,
    is_remote: bool,
    is_head: bool,
    upstream_track: Option<SharedString>,
    subject: Option<SharedString>,
    committer_date_relative: Option<SharedString>,
}

impl BranchStatusEntry {
    fn from_branch(b: &Branch) -> Self {
        let track = b
            .upstream
            .as_ref()
            .and_then(|u| u.tracking.status())
            .map(|s| {
                let mut buf = String::new();
                if s.ahead > 0 {
                    use std::fmt::Write as _;
                    let _ = write!(buf, "↑{}", s.ahead);
                }
                if s.behind > 0 {
                    use std::fmt::Write as _;
                    if !buf.is_empty() {
                        buf.push(' ');
                    }
                    let _ = write!(buf, "↓{}", s.behind);
                }
                SharedString::from(buf)
            })
            .filter(|s| !s.is_empty());
        let (subject, committer_date_relative) = b
            .most_recent_commit
            .as_ref()
            .map(|c| {
                let local_offset =
                    time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
                let commit_time = OffsetDateTime::from_unix_timestamp(c.commit_timestamp)
                    .unwrap_or_else(|_| OffsetDateTime::now_utc());
                let relative = time_format::format_localized_timestamp(
                    commit_time,
                    OffsetDateTime::now_utc(),
                    local_offset,
                    time_format::TimestampFormat::Relative,
                );
                (Some(c.subject.clone()), Some(SharedString::from(relative)))
            })
            .unwrap_or((None, None));
        Self {
            name: SharedString::from(b.name().to_string()),
            is_remote: b.is_remote(),
            is_head: b.is_head,
            upstream_track: track,
            subject,
            committer_date_relative,
        }
    }
}

/// Section order for the IDEA-style single-list layout.
/// Each entry is (stable key, display label).
const SECTION_ORDER: [(&str, &str); 6] = [
    ("recent", "Recent"),
    ("favorites", "Favorites"),
    ("local", "Local"),
    ("remote", "Remote"),
    ("tags", "Tags"),
    ("backups", "Backups"),
];

#[derive(Debug, Clone)]
enum PopupRow {
    Branch {
        entry: BranchStatusEntry,
        depth: usize,
    },
    Group {
        path: SharedString,
        depth: usize,
        expanded: bool,
    },
    Tag {
        name: SharedString,
    },
    Backup {
        branch: SharedString,
        op: SharedString,
        before_sha: SharedString,
    },
    Empty {
        message: SharedString,
    },
    Section {
        key: &'static str,
        label: SharedString,
        collapsed: bool,
        /// Number of items in the section (counted from source, so branches
        /// hidden inside collapsed groups still count) — shown as a badge so
        /// `0` makes an empty section obvious without expanding it.
        count: usize,
    },
}

pub struct BranchesPopup {
    workspace: WeakEntity<Workspace>,
    repository: Option<Entity<Repository>>,
    work_dir: Option<Arc<Path>>,
    /// Collapsed top-level section nodes by stable key. Absent ⇒ expanded.
    collapsed_sections: std::collections::HashSet<&'static str>,
    query: Entity<Editor>,
    rows: Vec<PopupRow>,
    selected_index: usize,
    branches: Vec<BranchStatusEntry>,
    tags: Vec<SharedString>,
    favorites_snapshot: favorites::RepoFavoritesSnapshot,
    expanded_groups: std::collections::HashSet<String>,
    backups: Vec<crate::backup_mcp::BackupEntry>,
    default_branch: Option<SharedString>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl BranchesPopup {
    /// Public constructor for hosting inside a `PopoverMenu` (title-bar widget)
    /// or as a fallback modal (keyboard action).
    pub fn new(
        workspace: WeakEntity<Workspace>,
        repository: Option<Entity<Repository>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search branches…", window, cx);
            editor
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &query,
            window,
            |this, _editor, event: &editor::EditorEvent, _window, cx| {
                if matches!(
                    event,
                    editor::EditorEvent::BufferEdited | editor::EditorEvent::Edited { .. }
                ) {
                    this.rebuild_rows(cx);
                }
            },
        ));

        let work_dir = repository
            .as_ref()
            .map(|r| r.read(cx).work_directory_abs_path.clone());

        if let Some(repo) = &repository {
            subscriptions.push(cx.subscribe(repo, |this, _repo, event, cx| {
                if matches!(event, RepositoryEvent::BranchListChanged) {
                    this.refresh_branches_from_repo(cx);
                }
            }));
        }

        let branches = repository
            .as_ref()
            .map(|r| {
                r.read(cx)
                    .branch_list
                    .iter()
                    .map(BranchStatusEntry::from_branch)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let favorites_snapshot = work_dir
            .as_ref()
            .and_then(|wd| favorites::load_for_repo(wd).log_err())
            .unwrap_or_default();

        let mut this = Self {
            workspace,
            repository: repository.clone(),
            work_dir,
            // Every section starts collapsed — the popup opens as a compact
            // list of section headers (each with a count badge); the user
            // expands what they need. A search query force-expands (see
            // `rebuild_rows`).
            collapsed_sections: SECTION_ORDER.iter().map(|(key, _)| *key).collect(),
            query,
            rows: Vec::new(),
            selected_index: 0,
            branches,
            tags: Vec::new(),
            favorites_snapshot,
            expanded_groups: std::collections::HashSet::new(),
            backups: Vec::new(),
            default_branch: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };

        // Async: load default branch + tags + initial backups list.
        if let Some(repo) = repository {
            let default_request = repo.update(cx, |repo, _| repo.default_branch(false));
            let tags_request = repo.update(cx, |repo, _| repo.tags());
            cx.spawn(async move |this, cx| {
                let default = default_request.await.ok().and_then(Result::ok).flatten();
                this.update(cx, |this, cx| {
                    this.default_branch = default;
                    cx.notify();
                })
                .ok();
                if let Ok(Ok(tags)) = tags_request.await {
                    this.update(cx, |this, cx| {
                        this.tags = tags;
                        this.rebuild_rows(cx);
                    })
                    .ok();
                }
            })
            .detach();
        }

        this.rebuild_rows(cx);
        this.refresh_backups(cx);
        this.query.focus_handle(cx).focus(window, cx);
        this
    }

    fn refresh_branches_from_repo(&mut self, cx: &mut Context<Self>) {
        if let Some(repo) = &self.repository {
            self.branches = repo
                .read(cx)
                .branch_list
                .iter()
                .map(BranchStatusEntry::from_branch)
                .collect();
            self.rebuild_rows(cx);
        }
    }

    fn toggle_section(&mut self, key: &'static str, cx: &mut Context<Self>) {
        if !self.collapsed_sections.remove(key) {
            self.collapsed_sections.insert(key);
        }
        self.rebuild_rows(cx);
        cx.notify();
    }

    fn refresh_backups(&mut self, cx: &mut Context<Self>) {
        let Some(work_dir) = self.work_dir.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { git::backup::list(&work_dir, None, None) })
                .await;
            if let Ok(list) = result {
                let backups: Vec<crate::backup_mcp::BackupEntry> = list
                    .into_iter()
                    .map(|b| crate::backup_mcp::BackupEntry {
                        branch: b.branch,
                        op: b.op,
                        timestamp_unix: b.timestamp_unix,
                        before_sha: b.before_sha,
                    })
                    .collect();
                this.update(cx, |this, cx| {
                    this.backups = backups;
                    this.rebuild_rows(cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Returns true for rows the user can act on (checkout, restore, etc.).
    /// Section headers and empty placeholders are non-actionable.
    fn is_actionable(row: &PopupRow) -> bool {
        matches!(
            row,
            PopupRow::Branch { .. } | PopupRow::Tag { .. } | PopupRow::Backup { .. }
        )
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text(cx);
        let lower_query = query.to_lowercase();
        let favorites: collections::HashSet<String> =
            self.favorites_snapshot.favorites.iter().cloned().collect();
        let recent_order: std::collections::HashMap<String, usize> = self
            .favorites_snapshot
            .recent
            .iter()
            .enumerate()
            .map(|(i, e)| (e.branch.clone(), i))
            .collect();

        self.rows.clear();

        let q = lower_query.as_str();
        for (key, label) in SECTION_ORDER {
            // Item count for the badge — counted directly from source so the
            // total is right even when branches sit inside collapsed groups.
            // These filters MUST mirror the matching `*_rows` builders below.
            let count = match key {
                "recent" => self
                    .branches
                    .iter()
                    .filter(|b| {
                        !b.is_remote
                            && recent_order.contains_key(b.name.as_ref())
                            && (q.is_empty() || b.name.to_lowercase().contains(q))
                    })
                    .count(),
                "favorites" => self
                    .branches
                    .iter()
                    .filter(|b| {
                        favorites.contains(b.name.as_ref())
                            && (q.is_empty() || b.name.to_lowercase().contains(q))
                    })
                    .count(),
                "local" => self
                    .branches
                    .iter()
                    .filter(|b| !b.is_remote && (q.is_empty() || b.name.to_lowercase().contains(q)))
                    .count(),
                "remote" => self
                    .branches
                    .iter()
                    .filter(|b| b.is_remote && (q.is_empty() || b.name.to_lowercase().contains(q)))
                    .count(),
                "tags" => self
                    .tags
                    .iter()
                    .filter(|t| q.is_empty() || t.to_lowercase().contains(q))
                    .count(),
                "backups" => self
                    .backups
                    .iter()
                    .filter(|b| {
                        q.is_empty()
                            || b.branch.to_lowercase().contains(q)
                            || b.op.to_lowercase().contains(q)
                    })
                    .count(),
                _ => 0,
            };

            // Collapsed by default; an active search query force-expands so
            // matches stay visible without manual toggling.
            let collapsed = q.is_empty() && self.collapsed_sections.contains(key);
            self.rows.push(PopupRow::Section {
                key,
                label: SharedString::from(label),
                collapsed,
                count,
            });
            if !collapsed {
                let body = match key {
                    "recent" => self.recent_rows(&lower_query, &recent_order),
                    "favorites" => self.favorites_rows(&lower_query, &favorites),
                    "local" => self.local_remote_rows(&lower_query, false),
                    "remote" => self.local_remote_rows(&lower_query, true),
                    "tags" => self.tag_rows(&lower_query),
                    "backups" => self.backup_rows(&lower_query),
                    _ => Vec::new(),
                };
                self.rows.extend(body);
            }
        }

        // Default selection to the first actionable row so Enter always acts
        // on a branch/tag/backup rather than toggling a section header.
        self.selected_index = self.rows.iter().position(Self::is_actionable).unwrap_or(0);
        cx.notify();
    }

    fn recent_rows(
        &self,
        lower_query: &str,
        recent_order: &std::collections::HashMap<String, usize>,
    ) -> Vec<PopupRow> {
        let query_empty = lower_query.is_empty();
        let mut entries: Vec<&BranchStatusEntry> = self
            .branches
            .iter()
            .filter(|b| !b.is_remote)
            .filter(|b| recent_order.contains_key(b.name.as_ref()))
            .filter(|b| query_empty || b.name.to_lowercase().contains(lower_query))
            .collect();
        entries.sort_by_key(|b| *recent_order.get(b.name.as_ref()).unwrap_or(&usize::MAX));
        if entries.is_empty() {
            vec![PopupRow::Empty {
                message: SharedString::from(
                    "No recently checked-out branches yet — checkout one to populate.",
                ),
            }]
        } else {
            entries
                .into_iter()
                .map(|entry| PopupRow::Branch {
                    entry: entry.clone(),
                    depth: 0,
                })
                .collect()
        }
    }

    fn favorites_rows(
        &self,
        lower_query: &str,
        favorites: &collections::HashSet<String>,
    ) -> Vec<PopupRow> {
        let query_empty = lower_query.is_empty();
        let mut entries: Vec<&BranchStatusEntry> = self
            .branches
            .iter()
            .filter(|b| favorites.contains(b.name.as_ref()))
            .filter(|b| query_empty || b.name.to_lowercase().contains(lower_query))
            .collect();
        entries.sort_by(|a, b| a.name.as_ref().cmp(b.name.as_ref()));
        if entries.is_empty() {
            vec![PopupRow::Empty {
                message: SharedString::from("No favorites yet — star a branch to keep it here."),
            }]
        } else {
            entries
                .into_iter()
                .map(|entry| PopupRow::Branch {
                    entry: entry.clone(),
                    depth: 0,
                })
                .collect()
        }
    }

    fn local_remote_rows(&self, lower_query: &str, want_remote: bool) -> Vec<PopupRow> {
        let query_empty = lower_query.is_empty();
        let mut entries: Vec<&BranchStatusEntry> = self
            .branches
            .iter()
            .filter(|b| b.is_remote == want_remote)
            .filter(|b| query_empty || b.name.to_lowercase().contains(lower_query))
            .collect();
        entries.sort_by(|a, b| a.name.as_ref().cmp(b.name.as_ref()));
        let names: Vec<String> = entries.iter().map(|e| e.name.to_string()).collect();
        let tree = tree::BranchTree::build(&names, self.expanded_groups.clone());
        let by_name: std::collections::HashMap<&str, &BranchStatusEntry> =
            entries.iter().map(|e| (e.name.as_ref(), *e)).collect();
        if names.is_empty() {
            vec![PopupRow::Empty {
                message: SharedString::from(if want_remote {
                    "No remote branches"
                } else {
                    "No local branches"
                }),
            }]
        } else {
            let mut rows = Vec::new();
            for row in tree.rows {
                match row {
                    tree::TreeRow::Group {
                        path,
                        depth,
                        expanded,
                    } => {
                        rows.push(PopupRow::Group {
                            path: SharedString::from(path),
                            depth,
                            expanded,
                        });
                    }
                    tree::TreeRow::Leaf {
                        full_name, depth, ..
                    } => {
                        if let Some(entry) = by_name.get(full_name.as_str()) {
                            rows.push(PopupRow::Branch {
                                entry: (*entry).clone(),
                                depth,
                            });
                        }
                    }
                }
            }
            rows
        }
    }

    fn tag_rows(&self, lower_query: &str) -> Vec<PopupRow> {
        let query_empty = lower_query.is_empty();
        let mut tags: Vec<SharedString> = self
            .tags
            .iter()
            .filter(|t| query_empty || t.to_lowercase().contains(lower_query))
            .cloned()
            .collect();
        tags.sort();
        if tags.is_empty() {
            vec![PopupRow::Empty {
                message: SharedString::from("No tags"),
            }]
        } else {
            tags.into_iter()
                .map(|name| PopupRow::Tag { name })
                .collect()
        }
    }

    fn backup_rows(&self, lower_query: &str) -> Vec<PopupRow> {
        if self.backups.is_empty() {
            return vec![PopupRow::Empty {
                message: SharedString::from("No backup refs."),
            }];
        }
        let mut backups = self.backups.clone();
        if !lower_query.is_empty() {
            backups.retain(|b| {
                b.branch.to_lowercase().contains(lower_query)
                    || b.op.to_lowercase().contains(lower_query)
            });
        }
        if backups.is_empty() {
            vec![PopupRow::Empty {
                message: SharedString::from("No backup refs."),
            }]
        } else {
            backups
                .into_iter()
                .map(|backup| PopupRow::Backup {
                    branch: SharedString::from(backup.branch),
                    op: SharedString::from(backup.op),
                    before_sha: SharedString::from(backup.before_sha),
                })
                .collect()
        }
    }

    fn is_favorite(&self, branch_name: &str) -> bool {
        self.favorites_snapshot
            .favorites
            .iter()
            .any(|b| b == branch_name)
    }

    fn current_head(&self) -> Option<&str> {
        self.branches
            .iter()
            .find(|b| b.is_head)
            .map(|b| b.name.as_ref())
    }

    fn dispatch_default(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(idx).cloned() else {
            return;
        };
        match row {
            PopupRow::Branch { entry, .. } => {
                self.checkout_branch(entry.name, window, cx);
                cx.emit(DismissEvent);
            }
            PopupRow::Group { path, .. } => {
                if self.expanded_groups.contains(path.as_ref()) {
                    self.expanded_groups.remove(path.as_ref());
                } else {
                    self.expanded_groups.insert(path.to_string());
                }
                self.rebuild_rows(cx);
            }
            PopupRow::Tag { name } => {
                self.checkout_revision(name, window, cx);
                cx.emit(DismissEvent);
            }
            PopupRow::Backup {
                branch, before_sha, ..
            } => {
                self.restore_backup(branch, before_sha, window, cx);
            }
            PopupRow::Section { key, .. } => {
                self.toggle_section(key, cx);
            }
            PopupRow::Empty { .. } => {}
        }
    }

    fn checkout_branch(
        &mut self,
        branch: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let work_dir = self.work_dir.clone();
        let workspace = self.workspace.clone();
        let branch_for_recent = branch.clone();
        cx.spawn_in(window, async move |_, cx| {
            let recv = repo.update(cx, |repo, _| repo.change_branch(branch.to_string()));
            match recv.await {
                Ok(Ok(())) => {
                    if let Some(work_dir) = work_dir {
                        favorites::record_checkout(&work_dir, branch_for_recent.as_ref()).log_err();
                    }
                    anyhow::Ok(())
                }
                Ok(Err(e)) => {
                    if let Some(workspace) = workspace.upgrade() {
                        cx.update(|_window, cx| {
                            show_error_toast(
                                workspace,
                                format!("git switch {}", branch_for_recent),
                                e,
                                cx,
                            );
                        })?;
                    }
                    Ok(())
                }
                Err(_) => Err(anyhow::anyhow!("change_branch was canceled")),
            }
        })
        .detach_and_log_err(cx);
    }

    fn checkout_revision(
        &mut self,
        revision: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        cx.spawn_in(window, async move |_, cx| {
            let recv = repo.update(cx, |repo, _| repo.checkout_revision(revision.to_string()));
            recv.await??;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn restore_backup(
        &mut self,
        branch: SharedString,
        before_sha: SharedString,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(work_dir) = self.work_dir.clone() else {
            return;
        };
        cx.background_spawn(async move {
            crate::backup_mcp::create_restore_ref(&work_dir, branch.as_ref(), before_sha.as_ref())
                .log_err();
        })
        .detach();
    }

    fn handle_toggle_favorite(
        &mut self,
        _: &BranchesPopupToggleFavorite,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(work_dir) = self.work_dir.clone() else {
            return;
        };
        let Some(row) = self.rows.get(self.selected_index).cloned() else {
            return;
        };
        let branch_name = match row {
            PopupRow::Branch { entry, .. } => entry.name,
            _ => return,
        };
        let work_dir_clone = work_dir.clone();
        let branch_string = branch_name.to_string();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    favorites::toggle_favorite(&work_dir_clone, &branch_string)
                })
                .await;
            if result.is_ok() {
                let snapshot = cx
                    .background_spawn(async move { favorites::load_for_repo(&work_dir) })
                    .await
                    .ok();
                this.update(cx, |this, cx| {
                    if let Some(snapshot) = snapshot {
                        this.favorites_snapshot = snapshot;
                    }
                    this.rebuild_rows(cx);
                })
                .ok();
            }
        })
        .detach();
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.selected_index;
        self.dispatch_default(idx, window, cx);
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn select_prev(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut index = self.selected_index;
        loop {
            if index == 0 {
                break;
            }
            index -= 1;
            if Self::is_actionable(&self.rows[index]) {
                self.selected_index = index;
                cx.notify();
                break;
            }
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let mut index = self.selected_index;
        loop {
            if index + 1 >= self.rows.len() {
                break;
            }
            index += 1;
            if Self::is_actionable(&self.rows[index]) {
                self.selected_index = index;
                cx.notify();
                break;
            }
        }
    }


    /// Render the IDEA-style action header rows that appear between the search
    /// field and the first section node. Rows: Update Project /
    /// Update All Projects (solution-wide, ≥2 members only) / Commit / Push /
    /// separator / New Branch / Checkout Tag or Revision… / separator.
    fn render_action_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Pre-compute theme colors so the closures below don't need to borrow cx.
        let hover_bg = cx.theme().colors().ghost_element_hover;
        let border_color = cx.theme().colors().border_variant;

        let make_row = |id: &'static str| {
            h_flex()
                .id(id)
                .w_full()
                .px_3()
                .py_1()
                .gap_2()
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
        };
        let icon_slot = |name: IconName| Icon::new(name).size(IconSize::Small).color(Color::Muted);

        let sep = || div().h_px().mx_3().bg(border_color);

        v_flex()
            // Update / Push / Commit moved off this popup: Update + Push are now
            // dedicated buttons on the project toolbar (beside the branch
            // widget), and Commit goes through the git panel. The solution-wide
            // "Update All Projects" entry was dropped entirely — a fetch+pull
            // across every member can leave repos conflicted with no good way to
            // resolve it from here. The popup is branch-only now.
            // New Branch — opens BranchList picker where typing a new name creates it
            .child(
                make_row("popup-action-new-branch")
                    .child(icon_slot(IconName::GitBranchPlus))
                    .child(Label::new("New Branch").size(LabelSize::Small))
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(zed_actions::git::Branch), cx);
                        cx.emit(DismissEvent);
                    })),
            )
            // Checkout Tag or Revision… — opens the BranchList picker (same action as New
            // Branch); the user can pick a branch, tag, or type a revision there. There is
            // no dedicated tag-focused open path yet.
            .child(
                make_row("popup-action-checkout-revision")
                    .child(icon_slot(IconName::GitBranch))
                    .child(Label::new("Checkout Tag or Revision…").size(LabelSize::Small))
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(zed_actions::git::Branch), cx);
                        cx.emit(DismissEvent);
                    })),
            )
            // Separator before branch section nodes
            .child(sep().mt_1())
    }

    fn render_row(&self, ix: usize, row: &PopupRow, cx: &mut Context<Self>) -> AnyElement {
        let selected = ix == self.selected_index;
        match row {
            PopupRow::Empty { message } => h_flex()
                .pl(rems(1.75))
                .py_0p5()
                .child(
                    Label::new(message.clone())
                        .color(Color::Muted)
                        .size(LabelSize::Small)
                        .italic(),
                )
                .into_any_element(),
            PopupRow::Group {
                path,
                depth,
                expanded,
            } => {
                let path = path.clone();
                let chevron = if *expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                };
                ListItem::new(("branches-popup-group", ix))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(selected)
                    .start_slot(Icon::new(chevron).size(IconSize::Small).color(Color::Muted))
                    .child(
                        h_flex()
                            // Indent one level deeper than the raw tree depth so a
                            // group nests visibly under its section header.
                            .pl(rems((*depth as f32 + 1.0) * 1.0))
                            // Show only this group's own segment ("feature"), not
                            // the full prefix path ("origin/feature").
                            .child(
                                Label::new(SharedString::from(
                                    path.rsplit('/').next().unwrap_or(path.as_ref()),
                                ))
                                .color(Color::Muted),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        if this.expanded_groups.contains(path.as_ref()) {
                            this.expanded_groups.remove(path.as_ref());
                        } else {
                            this.expanded_groups.insert(path.to_string());
                        }
                        this.rebuild_rows(cx);
                    }))
                    .into_any_element()
            }
            PopupRow::Section {
                key,
                label,
                collapsed,
                count,
            } => {
                let key = *key;
                let chevron = if *collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                };
                ListItem::new(("branches-popup-section", ix))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(selected)
                    .start_slot(Icon::new(chevron).size(IconSize::Small).color(Color::Muted))
                    // Section headers read as structure (clearer than the muted
                    // body rows); the count badge sits on the right.
                    .child(Label::new(label.clone()).color(Color::Default))
                    .end_slot(
                        h_flex()
                            .px_1()
                            .rounded_sm()
                            .bg(cx.theme().colors().element_background)
                            .child(
                                Label::new(count.to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_section(key, cx);
                    }))
                    .into_any_element()
            }
            PopupRow::Branch { entry, depth, .. } => self
                .render_branch_row(ix, entry, *depth, selected, cx)
                .into_any_element(),
            PopupRow::Tag { name } => {
                let tag_name = name.clone();
                ListItem::new(("branches-popup-tag", ix))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(selected)
                    .start_slot(Icon::new(IconName::Hash).size(IconSize::Small))
                    .child(Label::new(tag_name.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.checkout_revision(tag_name.clone(), window, cx);
                        cx.emit(DismissEvent);
                    }))
                    .on_secondary_mouse_down(cx.listener({
                        let tag = name.clone();
                        move |this, _: &MouseDownEvent, window, cx| {
                            let workspace = this.workspace.clone();
                            let Some(repository) = this.repository.clone() else {
                                return;
                            };
                            let menu = context_menu::build_tag_menu(
                                context_menu::TagContext {
                                    workspace,
                                    repository,
                                    tag_name: tag.clone(),
                                },
                                window,
                                cx,
                            );
                            window.defer(cx, move |window, cx| {
                                menu.update(cx, |menu, cx| {
                                    menu.focus_handle(cx).focus(window, cx);
                                });
                            });
                        }
                    }))
                    .into_any_element()
            }
            PopupRow::Backup {
                branch,
                op,
                before_sha,
            } => {
                let short_sha: String = before_sha.chars().take(7).collect();
                let label = format!("{} ({}) — {}", branch, op, short_sha);
                let branch_clone = branch.clone();
                let sha_clone = before_sha.clone();
                ListItem::new(("branches-popup-backup", ix))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(selected)
                    .start_slot(Icon::new(IconName::CountdownTimer).size(IconSize::Small))
                    .child(Label::new(label).color(Color::Muted))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.restore_backup(branch_clone.clone(), sha_clone.clone(), window, cx);
                    }))
                    .into_any_element()
            }
        }
    }

    fn render_branch_row(
        &self,
        ix: usize,
        entry: &BranchStatusEntry,
        depth: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_default = self
            .default_branch
            .as_ref()
            .is_some_and(|d| d.as_ref() == entry.name.as_ref());
        let is_favorite = self.is_favorite(entry.name.as_ref());

        // S-SOL-PRT — surface a lock indicator next to protected
        // branches. We key off `delete_branch` because it's the op
        // most users associate with "is this branch protected?" and
        // the policy maps protected branches to `Forbidden` for
        // delete. Cheap glob-match — the snapshot is cached.
        let is_protected = self
            .work_dir
            .as_ref()
            .map(|wd| {
                matches!(
                    solutions::branch_protection::check(wd, entry.name.as_ref(), "delete_branch"),
                    solutions::branch_protection::Decision::Forbidden { .. }
                )
            })
            .unwrap_or(false);
        let star_icon = if is_favorite {
            IconName::StarFilled
        } else {
            IconName::Star
        };
        let star_color = if is_favorite {
            Color::Accent
        } else {
            Color::Muted
        };

        let entry_for_click = entry.clone();
        let entry_for_menu = entry.clone();
        let is_head = entry.is_head;
        let entry_label = entry.name.clone();
        let track = entry.upstream_track.clone();
        let subject = entry.subject.clone();
        let date = entry.committer_date_relative.clone();

        let star_branch = entry.name.to_string();
        let star_button = IconButton::new(("branches-popup-star", ix), star_icon)
            .icon_size(IconSize::Small)
            .icon_color(star_color)
            .tooltip(Tooltip::text(if is_favorite {
                "Unfavorite Branch"
            } else {
                "Favorite Branch"
            }))
            .on_click(cx.listener(move |this, _, _window, cx| {
                let Some(work_dir) = this.work_dir.clone() else {
                    return;
                };
                let branch = star_branch.clone();
                cx.spawn(async move |this, cx| {
                    let _ = cx
                        .background_spawn(
                            async move { favorites::toggle_favorite(&work_dir, &branch) },
                        )
                        .await;
                    let work_dir = this
                        .read_with(cx, |this, _| this.work_dir.clone())
                        .ok()
                        .flatten();
                    if let Some(work_dir) = work_dir {
                        let snap = cx
                            .background_spawn(async move { favorites::load_for_repo(&work_dir) })
                            .await
                            .ok();
                        this.update(cx, |this, cx| {
                            if let Some(snap) = snap {
                                this.favorites_snapshot = snap;
                            }
                            this.rebuild_rows(cx);
                        })
                        .ok();
                    }
                })
                .detach();
            }));

        let icon_name = if is_head {
            IconName::Check
        } else if entry.is_remote {
            IconName::Screen
        } else {
            IconName::GitBranch
        };
        let icon_color = if is_head { Color::Accent } else { Color::Muted };

        ListItem::new(("branches-popup-branch", ix))
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .start_slot(Icon::new(icon_name).color(icon_color).size(IconSize::Small))
            .child(
                h_flex()
                    .w_full()
                    // Indent one level deeper than the raw tree depth so a branch
                    // nests under its section/group header (matches the group rows).
                    .pl(rems((depth as f32 + 1.0) * 1.0))
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .when(is_protected, |this| {
                                        this.child(
                                            Icon::new(IconName::LockOutlined)
                                                .color(Color::Muted)
                                                .size(IconSize::XSmall),
                                        )
                                    })
                                    .child(Label::new(entry_label))
                                    .when(is_default, |this| {
                                        this.child(
                                            Label::new("default")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                    })
                                    .when_some(track, |this, t| {
                                        this.child(
                                            Label::new(t)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                    }),
                            )
                            .when(subject.is_some() || date.is_some(), |this| {
                                this.child(
                                    h_flex()
                                        .gap_1()
                                        .when_some(date, |this, d| {
                                            this.child(
                                                Label::new(d)
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Muted),
                                            )
                                        })
                                        .when_some(subject, |this, s| {
                                            this.child(
                                                Label::new(s.to_string())
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Muted)
                                                    .truncate(),
                                            )
                                        }),
                                )
                            }),
                    )
                    .child(star_button),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.standard_click() {
                    this.checkout_branch(entry_for_click.name.clone(), window, cx);
                    cx.emit(DismissEvent);
                }
            }))
            .on_secondary_mouse_down(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                let workspace = this.workspace.clone();
                let Some(repository) = this.repository.clone() else {
                    return;
                };
                let is_favorite = this.is_favorite(entry_for_menu.name.as_ref());
                let menu = context_menu::build_branch_menu(
                    context_menu::BranchContext {
                        workspace,
                        repository,
                        branch_name: entry_for_menu.name.clone(),
                        is_remote: entry_for_menu.is_remote,
                        is_head,
                        is_favorite,
                    },
                    window,
                    cx,
                );
                window.defer(cx, move |window, cx| {
                    menu.update(cx, |menu, cx| {
                        menu.focus_handle(cx).focus(window, cx);
                    });
                });
            }))
            .into_any_element()
    }
}

impl ModalView for BranchesPopup {}
impl EventEmitter<DismissEvent> for BranchesPopup {}

impl Focusable for BranchesPopup {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BranchesPopup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let head = self.current_head().map(|s| s.to_string());
        let popup = v_flex()
            .key_context("BranchesPopup")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::handle_toggle_favorite))
            .elevation_2(cx)
            .w(rems(32.))
            // Definite height (not just `max_h`): as an anchored popover the
            // container is content-sized, so the `flex_1` scroll list below
            // would collapse to zero height without a height basis here.
            .h(rems(36.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(Headline::new("Branches").size(HeadlineSize::XSmall))
                    .when_some(head, |this, h| {
                        this.child(
                            Label::new(format!("on {}", h))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(div().px_3().pb_1().child(self.query.clone()))
            .child(div().h_px().bg(cx.theme().colors().border_variant))
            .child(self.render_action_header(cx))
            .child(
                // Rows have heterogeneous heights (1-line section headers /
                // empty / tags vs 2-line branch rows with a commit subtitle),
                // so a `uniform_list` (single measured row height) overflows the
                // taller branch rows onto the next row. Render a plain
                // variable-height scroll list instead — the popup has few rows
                // and no scroll-to-selected, so virtualization isn't needed.
                v_flex()
                    .id("branches-popup-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children({
                        let rows = self.rows.clone();
                        rows.iter()
                            .enumerate()
                            .map(|(ix, row)| self.render_row(ix, row, cx))
                            .collect::<Vec<_>>()
                    }),
            );

        popup
    }
}

// ---- modals invoked from the per-branch context menu ----

pub struct SetUpstreamModal {
    repo: Entity<Repository>,
    branch: SharedString,
    editor: Entity<Editor>,
}

impl SetUpstreamModal {
    pub fn new(
        repo: Entity<Repository>,
        branch: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("origin/main", window, cx);
            editor
        });
        Self {
            repo,
            branch,
            editor,
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let upstream = self.editor.read(cx).text(cx);
        let upstream = upstream.trim().to_string();
        if upstream.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        let repo = self.repo.clone();
        let branch = self.branch.to_string();
        cx.spawn(async move |_, cx| {
            let recv = repo.update(cx, |repo, _| repo.set_upstream(branch, upstream));
            recv.await??;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to set upstream", window, cx, |_, _, _| None);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for SetUpstreamModal {}
impl ModalView for SetUpstreamModal {}
impl Focusable for SetUpstreamModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for SetUpstreamModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SetUpstreamModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!("Set Upstream for {}", self.branch))
                            .size(HeadlineSize::XSmall),
                    ),
            )
            .child(div().px_3().pb_3().w_full().child(self.editor.clone()))
    }
}

pub struct RenameBranchPopupModal {
    branch: SharedString,
    work_dir: Arc<Path>,
    editor: Entity<Editor>,
}

impl RenameBranchPopupModal {
    pub fn new(
        _repo: Entity<Repository>,
        branch: SharedString,
        work_dir: Arc<Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(branch.to_string(), window, cx);
            editor
        });
        Self {
            branch,
            work_dir,
            editor,
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let new_name = self.editor.read(cx).text(cx).trim().to_string();
        if new_name.is_empty() || new_name == self.branch.as_ref() {
            cx.emit(DismissEvent);
            return;
        }
        let old = self.branch.to_string();
        let work_dir = self.work_dir.to_path_buf();
        cx.spawn(async move |_, cx| {
            cx.background_spawn(async move {
                git::operations::OpRunner::run(
                    git::operations::RenameBranchOp { old, new: new_name },
                    &work_dir,
                )
            })
            .await
        })
        .detach_and_prompt_err("Failed to rename branch", window, cx, |_, _, _| None);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RenameBranchPopupModal {}
impl ModalView for RenameBranchPopupModal {}
impl Focusable for RenameBranchPopupModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for RenameBranchPopupModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RenameBranchPopupModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!("Rename Branch ({})", self.branch))
                            .size(HeadlineSize::XSmall),
                    ),
            )
            .child(div().px_3().pb_3().w_full().child(self.editor.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// Returns the section keys (in order) that appear as `PopupRow::Section` entries.
    fn section_headers(rows: &[PopupRow]) -> Vec<&'static str> {
        rows.iter()
            .filter_map(|row| {
                if let PopupRow::Section { key, .. } = row {
                    Some(*key)
                } else {
                    None
                }
            })
            .collect()
    }

    #[gpui::test]
    async fn test_branches_popup_section_order(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let popup = window_handle
            .update(cx, |_mw, window, cx| {
                cx.new(|cx| {
                    BranchesPopup::new(WeakEntity::<Workspace>::new_invalid(), None, window, cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        popup.update(cx, |popup, _cx| {
            let headers = section_headers(&popup.rows);
            assert_eq!(
                headers,
                vec!["recent", "favorites", "local", "remote", "tags", "backups"],
            );
        });
    }

    #[gpui::test]
    async fn test_branches_popup_toggle_section_collapses(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let popup = window_handle
            .update(cx, |_mw, window, cx| {
                cx.new(|cx| {
                    BranchesPopup::new(WeakEntity::<Workspace>::new_invalid(), None, window, cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        // All sections present initially.
        popup.update(cx, |popup, _cx| {
            let headers = section_headers(&popup.rows);
            assert_eq!(headers.len(), 6);
        });

        // Sections start collapsed by default, so toggling "recent" expands it.
        window_handle
            .update(cx, |_mw, _window, cx| {
                popup.update(cx, |popup, cx| {
                    popup.toggle_section("recent", cx);
                });
            })
            .unwrap();
        cx.run_until_parked();

        popup.update(cx, |popup, _cx| {
            let collapsed = popup
                .rows
                .iter()
                .find_map(|r| {
                    if let PopupRow::Section {
                        key: "recent",
                        collapsed,
                        ..
                    } = r
                    {
                        Some(*collapsed)
                    } else {
                        None
                    }
                })
                .expect("recent section header must exist");
            assert!(
                !collapsed,
                "recent section should expand on first toggle (collapsed by default)"
            );
            // All 6 section headers still present regardless of collapsed state.
            assert_eq!(section_headers(&popup.rows).len(), 6);
        });

        // Toggle "recent" again — it should collapse back.
        window_handle
            .update(cx, |_mw, _window, cx| {
                popup.update(cx, |popup, cx| {
                    popup.toggle_section("recent", cx);
                });
            })
            .unwrap();
        cx.run_until_parked();

        popup.update(cx, |popup, _cx| {
            let collapsed = popup
                .rows
                .iter()
                .find_map(|r| {
                    if let PopupRow::Section {
                        key: "recent",
                        collapsed,
                        ..
                    } = r
                    {
                        Some(*collapsed)
                    } else {
                        None
                    }
                })
                .expect("recent section header must exist");
            assert!(
                collapsed,
                "recent section should collapse again after second toggle"
            );
        });
    }

    #[gpui::test]
    async fn test_branches_popup_nav_skips_non_actionable_rows(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let popup = window_handle
            .update(cx, |_mw, window, cx| {
                cx.new(|cx| {
                    BranchesPopup::new(WeakEntity::<Workspace>::new_invalid(), None, window, cx)
                })
            })
            .unwrap();
        cx.run_until_parked();

        // Build a hand-crafted rows vec:
        //   index 0 — Section (non-actionable)
        //   index 1 — Empty   (non-actionable)
        //   index 2 — Tag     (actionable)
        //   index 3 — Section (non-actionable)
        //   index 4 — Tag     (actionable)
        window_handle
            .update(cx, |_mw, _window, cx| {
                popup.update(cx, |popup, cx| {
                    popup.rows = vec![
                        PopupRow::Section {
                            key: "recent",
                            label: "Recent".into(),
                            collapsed: false,
                            count: 0,
                        },
                        PopupRow::Empty {
                            message: "none".into(),
                        },
                        PopupRow::Tag { name: "v1".into() },
                        PopupRow::Section {
                            key: "tags",
                            label: "Tags".into(),
                            collapsed: false,
                            count: 1,
                        },
                        PopupRow::Tag { name: "v2".into() },
                    ];
                    popup.selected_index = 0;
                    cx.notify();
                });
            })
            .unwrap();

        // select_next from index 0 (Section) → should land on index 2 (Tag "v1"),
        // skipping index 1 (Empty).
        window_handle
            .update(cx, |_mw, window, cx| {
                popup.update(cx, |popup, cx| {
                    popup.select_next(&menu::SelectNext, window, cx);
                });
            })
            .unwrap();

        popup.update(cx, |popup, _cx| {
            assert_eq!(
                popup.selected_index, 2,
                "select_next from Section(0) should skip Empty(1) and land on Tag(2)"
            );
        });

        // select_next from index 2 (Tag "v1") → should land on index 4 (Tag "v2"),
        // skipping index 3 (Section).
        window_handle
            .update(cx, |_mw, window, cx| {
                popup.update(cx, |popup, cx| {
                    popup.select_next(&menu::SelectNext, window, cx);
                });
            })
            .unwrap();

        popup.update(cx, |popup, _cx| {
            assert_eq!(
                popup.selected_index, 4,
                "select_next from Tag(2) should skip Section(3) and land on Tag(4)"
            );
        });

        // select_next from index 4 (last row) → no-op, stays at 4.
        window_handle
            .update(cx, |_mw, window, cx| {
                popup.update(cx, |popup, cx| {
                    popup.select_next(&menu::SelectNext, window, cx);
                });
            })
            .unwrap();

        popup.update(cx, |popup, _cx| {
            assert_eq!(
                popup.selected_index, 4,
                "select_next at last actionable row should be a no-op"
            );
        });

        // select_prev from index 4 (Tag "v2") → should land on index 2 (Tag "v1"),
        // skipping index 3 (Section).
        window_handle
            .update(cx, |_mw, window, cx| {
                popup.update(cx, |popup, cx| {
                    popup.selected_index = 4;
                    popup.select_prev(&menu::SelectPrevious, window, cx);
                });
            })
            .unwrap();

        popup.update(cx, |popup, _cx| {
            assert_eq!(
                popup.selected_index, 2,
                "select_prev from Tag(4) should skip Section(3) and land on Tag(2)"
            );
        });

        // select_prev from index 2 (Tag "v1") → no actionable row before it
        // (index 0 = Section, index 1 = Empty) → no-op, stays at 2.
        window_handle
            .update(cx, |_mw, window, cx| {
                popup.update(cx, |popup, cx| {
                    popup.select_prev(&menu::SelectPrevious, window, cx);
                });
            })
            .unwrap();

        popup.update(cx, |popup, _cx| {
            assert_eq!(
                popup.selected_index, 2,
                "select_prev from Tag(2) with only non-actionable rows before it should be a no-op"
            );
        });
    }
}
