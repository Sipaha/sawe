//! S-STH — Stashes pane Item.
//!
//! Top-level pane item that pairs a list of git stashes with a detail
//! panel showing the patch text. Coexists with the modal
//! [`crate::stash_picker::StashList`] which is still wired into the Stash
//! tab of [`crate::git_picker`]; this view is the dedicated, stickable
//! surface.
//!
//! Per-stash mutations (apply / pop / drop / branch / rename) all go
//! through `project::git_store::Repository` so they pick up the local
//! snapshot refresh + `RepositoryEvent::StashEntriesChanged` plumbing.
//! Drop is gated through [`gpui::Window::prompt`] for confirmation;
//! Rename is implemented as `drop + stash push` — the user is warned the
//! stash sha changes (the original is recoverable from the auto-backup
//! created by [`stash_drop`]).

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use editor::{Editor, EditorEvent, MultiBuffer};
use git::stash::StashEntry as GitStashEntry;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseDownEvent,
    ParentElement, Pixels, Point, PromptLevel, Render, SharedString, Styled, Subscription,
    WeakEntity, Window, anchored, deferred, uniform_list,
};
use language::{Buffer, Capability};
use project::Project;
use project::git_store::{Repository, RepositoryEvent};
use time::{OffsetDateTime, UtcOffset};
use ui::{ContextMenu, Divider, IconButtonShape, ListItem, ListItemSpacing, Tooltip, prelude::*};
use workspace::item::{ItemEvent, TabContentParams};
use workspace::notifications::DetachAndPromptErr;
use workspace::{Item, ItemNavHistory, ModalView, Workspace};

/// Lightweight projection of a `git::stash::StashEntry` enriched with the
/// per-stash badge data used by the row renderer (file count + untracked
/// flag). The `stash_sha` is stable across reorders and matches the OID
/// we get from `cached_stash`.
#[derive(Clone, Debug)]
pub struct StashEntry {
    pub index: usize,
    pub stash_sha: String,
    pub message: SharedString,
    pub branch: Option<SharedString>,
    pub created_at_unix: i64,
    pub file_count: usize,
    pub has_untracked: bool,
}

impl StashEntry {
    fn stash_ref(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(StashesView::deploy);
}

pub struct StashesView {
    repo: Option<Entity<Repository>>,
    workspace: WeakEntity<Workspace>,
    #[allow(dead_code)]
    project: Entity<Project>,
    entries: Vec<StashEntry>,
    selected: Option<usize>,
    filter: SharedString,
    filter_editor: Entity<Editor>,
    detail_editor: Entity<Editor>,
    detail_buffer: Entity<Buffer>,
    detail_for_sha: Option<String>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl StashesView {
    /// Action handler for `crate::fork_actions::Stashes` — finds (or creates) the
    /// Stashes pane item in the active pane and activates it.
    pub fn deploy(
        workspace: &mut Workspace,
        _: &crate::fork_actions::Stashes,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace.items_of_type::<Self>(cx).next();
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let project = workspace.project().clone();
        let repo = project.read(cx).active_repository(cx);
        let workspace_handle = workspace.weak_handle();
        let view = cx.new(|cx| Self::new(repo, project, workspace_handle, window, cx));
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    fn new(
        repo: Option<Entity<Repository>>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter stashes…", window, cx);
            editor
        });

        let detail_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(String::new(), cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(detail_buffer.clone(), cx));
        let detail_editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx);
            editor.set_read_only(true);
            editor.set_show_gutter(false, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor
        });

        let mut subscriptions = Vec::new();
        if let Some(repo_entity) = repo.as_ref() {
            subscriptions.push(cx.subscribe_in(
                repo_entity,
                window,
                |this, _repo, event, window, cx| {
                    if matches!(event, RepositoryEvent::StashEntriesChanged) {
                        this.refresh_entries(window, cx);
                    }
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &filter_editor,
            window,
            |this, editor, event: &EditorEvent, _window, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    this.filter = text.into();
                    cx.notify();
                }
            },
        ));

        let mut this = Self {
            repo,
            workspace,
            project,
            entries: Vec::new(),
            selected: None,
            filter: SharedString::default(),
            filter_editor,
            detail_editor,
            detail_buffer,
            detail_for_sha: None,
            context_menu: None,
            focus_handle,
            _subscriptions: subscriptions,
        };
        this.refresh_entries(window, cx);
        this
    }

    fn refresh_entries(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.entries.clear();
            self.selected = None;
            cx.notify();
            return;
        };
        let cached = repo.read(cx).cached_stash().entries.to_vec();
        let stat_tasks: Vec<_> = cached
            .iter()
            .map(|entry| {
                let stash_ref = format!("stash@{{{}}}", entry.index);
                repo.update(cx, |repo, _| repo.stash_stat(stash_ref))
            })
            .collect();
        let prior = std::mem::take(&mut self.entries);
        cx.spawn_in(window, async move |this, cx| {
            let mut stats: HashMap<usize, git::stash::StashStat> = HashMap::new();
            for (entry, task) in cached.iter().zip(stat_tasks.into_iter()) {
                if let Ok(Ok(stat)) = task.await {
                    stats.insert(entry.index, stat);
                }
            }
            this.update_in(cx, |this, _window, cx| {
                this.entries = cached
                    .iter()
                    .map(|entry| Self::entry_from_git(entry, stats.get(&entry.index).cloned()))
                    .collect();
                if this.entries.is_empty() {
                    this.selected = None;
                    this.detail_for_sha = None;
                    this.detail_buffer.update(cx, |buffer, cx| {
                        buffer.set_text("", cx);
                    });
                } else {
                    let prior_sha = this
                        .selected
                        .and_then(|ix| prior.get(ix))
                        .map(|entry| entry.stash_sha.clone());
                    this.selected = match prior_sha
                        .as_deref()
                        .and_then(|sha| this.entries.iter().position(|e| e.stash_sha == sha))
                    {
                        Some(ix) => Some(ix),
                        None => Some(0),
                    };
                }
                cx.notify();
                let needs_detail = this.selected.and_then(|ix| this.entries.get(ix).cloned());
                if let Some(entry) = needs_detail {
                    this.load_detail(entry, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn entry_from_git(entry: &GitStashEntry, stat: Option<git::stash::StashStat>) -> StashEntry {
        let (file_count, has_untracked) = stat
            .map(|s| (s.file_count, s.has_untracked))
            .unwrap_or((0, false));
        StashEntry {
            index: entry.index,
            stash_sha: entry.oid.to_string(),
            message: entry.message.clone().into(),
            branch: entry.branch.clone().map(SharedString::from),
            created_at_unix: entry.timestamp,
            file_count,
            has_untracked,
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(ix, entry)| {
                let haystack = format!(
                    "{} {} {}",
                    entry.message,
                    entry.branch.as_deref().unwrap_or(""),
                    entry.stash_ref()
                )
                .to_lowercase();
                if haystack.contains(&needle) {
                    Some(ix)
                } else {
                    None
                }
            })
            .collect()
    }

    fn select_entry(&mut self, entry_ix: usize, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(entry_ix);
        if let Some(entry) = self.entries.get(entry_ix).cloned() {
            self.load_detail(entry, cx);
        }
        cx.notify();
    }

    fn load_detail(&mut self, entry: StashEntry, cx: &mut Context<Self>) {
        if self.detail_for_sha.as_deref() == Some(entry.stash_sha.as_str()) {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.detail_for_sha = Some(entry.stash_sha.clone());
        let stash_ref = entry.stash_ref();
        let task = repo.update(cx, |repo, _| repo.stash_show_patch(stash_ref));
        let target_sha = entry.stash_sha;
        cx.spawn(async move |this, cx| {
            let patch = match task.await {
                Ok(Ok(text)) => text,
                Ok(Err(err)) => format!("(failed to load stash diff: {err})"),
                Err(_) => return,
            };
            this.update(cx, |this, cx| {
                if this.detail_for_sha.as_deref() != Some(target_sha.as_str()) {
                    return;
                }
                this.detail_buffer.update(cx, |buffer, cx| {
                    buffer.set_capability(Capability::ReadWrite, cx);
                    let len = buffer.len();
                    buffer.edit([(0..len, patch.as_str())], None, cx);
                    buffer.set_capability(Capability::ReadOnly, cx);
                });
            })
            .ok();
        })
        .detach();
    }

    fn apply_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.dispatch_apply(entry, window, cx);
    }

    fn pop_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.dispatch_pop(entry, window, cx);
    }

    fn drop_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.dispatch_drop(entry, window, cx);
    }

    fn selected_entry(&self) -> Option<&StashEntry> {
        self.selected.and_then(|ix| self.entries.get(ix))
    }

    fn dispatch_apply(&mut self, entry: StashEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let index = entry.index;
        cx.spawn(async move |_, cx| {
            repo.update(cx, |repo, cx| repo.stash_apply(Some(index), cx))
                .await?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to apply stash", window, cx, |e, _, _| {
            Some(e.to_string())
        });
    }

    fn dispatch_pop(&mut self, entry: StashEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let index = entry.index;
        cx.spawn(async move |_, cx| {
            repo.update(cx, |repo, cx| repo.stash_pop(Some(index), cx))
                .await?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to pop stash", window, cx, |e, _, _| {
            Some(e.to_string())
        });
    }

    fn dispatch_drop(&mut self, entry: StashEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!(
                "Drop {}? The auto-backup ref keeps the prior stash recoverable.",
                entry.stash_ref()
            ),
            None,
            &["Drop", "Cancel"],
            cx,
        );
        let index = entry.index;
        cx.spawn(async move |_, cx| {
            if answer.await != Ok(0) {
                return anyhow::Ok(());
            }
            repo.update(cx, |repo, cx| repo.stash_drop(Some(index), cx))
                .await??;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to drop stash", window, cx, |e, _, _| {
            Some(e.to_string())
        });
    }

    fn dispatch_branch_from(
        &mut self,
        entry: StashEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let stash_ref = entry.stash_ref();
        let workspace = self.workspace.clone();
        if let Some(workspace) = workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    StashBranchModal::new(repo, stash_ref, window, cx)
                });
            });
        }
    }

    fn dispatch_view_diff(
        &mut self,
        entry: StashEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        crate::commit_view::CommitView::open(
            entry.stash_sha.clone(),
            repo.downgrade(),
            workspace.downgrade(),
            Some(entry.index),
            None,
            window,
            cx,
        );
    }

    fn dispatch_rename(&mut self, entry: StashEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let workspace = self.workspace.clone();
        if let Some(workspace) = workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    StashRenameModal::new(repo, entry, window, cx)
                });
            });
        }
    }

    fn dispatch_copy_ref(
        &mut self,
        entry: StashEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let payload = format!("{} {}", entry.stash_ref(), entry.stash_sha);
        cx.write_to_clipboard(ClipboardItem::new_string(payload));
    }

    fn open_stash_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                StashCurrentChangesModal::new(repo, window, cx)
            });
        });
    }

    fn deploy_row_context_menu(
        &mut self,
        entry_ix: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get(entry_ix).cloned() else {
            return;
        };
        let stash_ref = entry.stash_ref();
        let view = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.entry("Apply", None, {
                let view = view.clone();
                let entry = entry.clone();
                move |window, cx| {
                    let entry = entry.clone();
                    view.update(cx, |view, cx| view.dispatch_apply(entry, window, cx))
                        .ok();
                }
            })
            .entry("Pop", None, {
                let view = view.clone();
                let entry = entry.clone();
                move |window, cx| {
                    let entry = entry.clone();
                    view.update(cx, |view, cx| view.dispatch_pop(entry, window, cx))
                        .ok();
                }
            })
            .entry("Drop", None, {
                let view = view.clone();
                let entry = entry.clone();
                move |window, cx| {
                    let entry = entry.clone();
                    view.update(cx, |view, cx| view.dispatch_drop(entry, window, cx))
                        .ok();
                }
            })
            .separator()
            .entry("Branch from Stash…", None, {
                let view = view.clone();
                let entry = entry.clone();
                move |window, cx| {
                    let entry = entry.clone();
                    view.update(cx, |view, cx| view.dispatch_branch_from(entry, window, cx))
                        .ok();
                }
            })
            .entry("View Diff in Editor", None, {
                let view = view.clone();
                let entry = entry.clone();
                move |window, cx| {
                    let entry = entry.clone();
                    view.update(cx, |view, cx| view.dispatch_view_diff(entry, window, cx))
                        .ok();
                }
            })
            .separator()
            .entry("Rename… (drop + re-stash; sha changes)", None, {
                let view = view.clone();
                let entry = entry.clone();
                move |window, cx| {
                    let entry = entry.clone();
                    view.update(cx, |view, cx| view.dispatch_rename(entry, window, cx))
                        .ok();
                }
            })
            .entry(format!("Copy Stash Reference ({})", stash_ref), None, {
                move |window, cx| {
                    let entry = entry.clone();
                    view.update(cx, |view, cx| view.dispatch_copy_ref(entry, window, cx))
                        .ok();
                }
            })
        });
        let subscription =
            cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _window, cx| {
                this.context_menu.take();
                cx.notify();
            });
        self.context_menu = Some((menu, position, subscription));
        self.selected = Some(entry_ix);
        cx.notify();
    }

    fn render_row(
        &self,
        entry_ix: usize,
        entry: &StashEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected == Some(entry_ix);
        let timezone = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let timestamp = OffsetDateTime::from_unix_timestamp(entry.created_at_unix)
            .unwrap_or_else(|_| OffsetDateTime::now_utc());
        let relative = time_format::format_localized_timestamp(
            timestamp,
            OffsetDateTime::now_utc(),
            timezone,
            time_format::TimestampFormat::Relative,
        );
        let label = format!("stash@{{{}}}: {}", entry.index, entry.message);
        let entry_for_click = entry.clone();
        ListItem::new(("stashes-row", entry_ix))
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .start_slot(
                Icon::new(IconName::BoxOpen)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(Label::new(label).truncate())
                    .child(
                        h_flex()
                            .gap_1p5()
                            .when_some(entry.branch.clone(), |this, branch| {
                                this.child(
                                    Label::new(branch)
                                        .truncate()
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new("•")
                                        .alpha(0.5)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            })
                            .child(
                                Label::new(relative)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new("•")
                                    .alpha(0.5)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(format!("{} files", entry.file_count))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .when(entry.has_untracked, |this| {
                                this.child(
                                    Label::new("untracked")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                )
                            }),
                    ),
            )
            .on_click(cx.listener(move |this, _event, window, cx| {
                this.select_entry(entry_ix, window, cx);
            }))
            .on_secondary_mouse_down(cx.listener(
                move |this, event: &MouseDownEvent, window, cx| {
                    let _ = entry_for_click;
                    this.deploy_row_context_menu(entry_ix, event.position, window, cx);
                },
            ))
            .into_any_element()
    }
}

impl Focusable for StashesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for StashesView {}

impl Render for StashesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible: Vec<usize> = self.filtered_indices();
        let row_count = visible.len();
        let total = self.entries.len();
        let count_label = if total == row_count {
            format!("{} stash{}", total, if total == 1 { "" } else { "es" })
        } else {
            format!("{} of {} stashes", row_count, total)
        };
        let selected_entry = self.selected_entry().cloned();

        v_flex()
            .key_context("StashesView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(div().flex_1().child(self.filter_editor.clone()))
                    .child(
                        Label::new(count_label)
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w(rems(28.))
                            .min_w(rems(20.))
                            .h_full()
                            .border_r_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child({
                                let entries = self.entries.clone();
                                uniform_list(
                                    "stashes-list",
                                    row_count,
                                    cx.processor(
                                        move |this, range: std::ops::Range<usize>, _window, cx| {
                                            let mut items = Vec::with_capacity(range.len());
                                            for i in range {
                                                let Some(&entry_ix) = visible.get(i) else {
                                                    continue;
                                                };
                                                let Some(entry) = entries.get(entry_ix) else {
                                                    continue;
                                                };
                                                items.push(this.render_row(entry_ix, entry, cx));
                                            }
                                            items
                                        },
                                    ),
                                )
                                .h_full()
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_detail_header(&selected_entry, cx))
                            .child(Divider::horizontal())
                            .child(div().flex_1().min_h_0().child(self.detail_editor.clone())),
                    ),
            )
            .child(self.render_bottom_toolbar(cx))
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl StashesView {
    fn render_detail_header(
        &self,
        selected: &Option<StashEntry>,
        _cx: &Context<Self>,
    ) -> AnyElement {
        match selected {
            Some(entry) => {
                let stash_ref = entry.stash_ref();
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .child(
                        Icon::new(IconName::BoxOpen)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(stash_ref).size(LabelSize::Small))
                    .child(
                        Label::new(entry.message.clone())
                            .truncate()
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
            None => h_flex()
                .w_full()
                .px_2()
                .py_1p5()
                .child(
                    Label::new("No stash selected")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
        }
    }

    fn render_bottom_toolbar(&self, cx: &Context<Self>) -> AnyElement {
        let has_selection = self.selected_entry().is_some();
        let _ = cx;
        h_flex()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("stash-current-changes", "Stash Current Changes…")
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_stash_form(window, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("stash-apply", "Apply")
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.apply_selected(window, cx);
                    })),
            )
            .child(
                Button::new("stash-pop", "Pop")
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.pop_selected(window, cx);
                    })),
            )
            .child(
                IconButton::new("stash-drop", IconName::Trash)
                    .shape(IconButtonShape::Square)
                    .disabled(!has_selection)
                    .tooltip(Tooltip::text("Drop selected stash"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.drop_selected(window, cx);
                    })),
            )
            .into_any_element()
    }
}

impl Item for StashesView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::BoxOpen).color(Color::Muted))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, _: &App) -> SharedString {
        "Stashes".into()
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some("Git Stashes".into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Stashes Pane Opened")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            None
        }
    }

    fn navigate(
        &mut self,
        _data: Arc<dyn std::any::Any + Send>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> bool {
        false
    }

    fn set_nav_history(
        &mut self,
        _nav_history: ItemNavHistory,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

// ===== modals =====

pub struct StashBranchModal {
    repo: Entity<Repository>,
    stash_ref: String,
    name_editor: Entity<Editor>,
    focus_handle: FocusHandle,
}

impl StashBranchModal {
    fn new(
        repo: Entity<Repository>,
        stash_ref: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("New branch name…", window, cx);
            editor
        });
        let focus_handle = name_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
        Self {
            repo,
            stash_ref,
            name_editor,
            focus_handle,
        }
    }
}

impl Focusable for StashBranchModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for StashBranchModal {}
impl ModalView for StashBranchModal {}

impl Render for StashBranchModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let stash_ref = self.stash_ref.clone();
        v_flex()
            .key_context("StashBranchModal")
            .elevation_2(cx)
            .w(rems(34.))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                let name = this.name_editor.read(cx).text(cx).trim().to_string();
                if name.is_empty() {
                    cx.emit(DismissEvent);
                    return;
                }
                let repo = this.repo.clone();
                let stash_ref = this.stash_ref.clone();
                cx.spawn(async move |_, cx| {
                    repo.update(cx, |repo, cx| repo.stash_branch(name, stash_ref, cx))
                        .await??;
                    anyhow::Ok(())
                })
                .detach_and_prompt_err(
                    "Failed to branch from stash",
                    window,
                    cx,
                    |e, _, _| Some(e.to_string()),
                );
                cx.emit(DismissEvent);
            }))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!("Branch from {}", stash_ref))
                            .size(HeadlineSize::XSmall),
                    ),
            )
            .child(div().px_3().pb_3().w_full().child(self.name_editor.clone()))
    }
}

pub struct StashCurrentChangesModal {
    repo: Entity<Repository>,
    message_editor: Entity<Editor>,
    include_untracked: bool,
    keep_index: bool,
    focus_handle: FocusHandle,
}

impl StashCurrentChangesModal {
    fn new(repo: Entity<Repository>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let message_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Stash message (optional)…", window, cx);
            editor
        });
        let focus_handle = message_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
        Self {
            repo,
            message_editor,
            include_untracked: true,
            keep_index: false,
            focus_handle,
        }
    }
}

impl Focusable for StashCurrentChangesModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for StashCurrentChangesModal {}
impl ModalView for StashCurrentChangesModal {}

impl Render for StashCurrentChangesModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let include_untracked = self.include_untracked;
        let keep_index = self.keep_index;
        v_flex()
            .key_context("StashCurrentChangesModal")
            .elevation_2(cx)
            .w(rems(34.))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.commit_form(window, cx);
            }))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::BoxOpen).size(IconSize::XSmall))
                    .child(Headline::new("Stash Current Changes").size(HeadlineSize::XSmall)),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .w_full()
                    .child(self.message_editor.clone()),
            )
            .child(
                v_flex()
                    .px_3()
                    .pb_2()
                    .gap_1()
                    .child(
                        ui::Checkbox::new(
                            "stash-include-untracked",
                            ui::ToggleState::from(include_untracked),
                        )
                        .label("Include untracked files")
                        .on_click(cx.listener(
                            |this, state: &ui::ToggleState, _, cx| {
                                this.include_untracked = matches!(state, ui::ToggleState::Selected);
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        ui::Checkbox::new("stash-keep-index", ui::ToggleState::from(keep_index))
                            .label("Keep staged changes (--keep-index)")
                            .on_click(cx.listener(|this, state: &ui::ToggleState, _, cx| {
                                this.keep_index = matches!(state, ui::ToggleState::Selected);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .pb_3()
                    .gap_2()
                    .justify_end()
                    .child(Button::new("stash-cancel", "Cancel").on_click(cx.listener(
                        |_, _, _, cx| {
                            cx.emit(DismissEvent);
                        },
                    )))
                    .child(
                        Button::new("stash-confirm", "Stash")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.commit_form(window, cx);
                            })),
                    ),
            )
    }
}

impl StashCurrentChangesModal {
    fn commit_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.message_editor.read(cx).text(cx);
        let trimmed = raw.trim();
        let message = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        let include_untracked = self.include_untracked;
        let keep_index = self.keep_index;
        let repo = self.repo.clone();
        cx.spawn(async move |_, cx| {
            repo.update(cx, |repo, cx| {
                repo.stash_push(message, include_untracked, keep_index, cx)
            })
            .await??;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to stash changes", window, cx, |e, _, _| {
            Some(e.to_string())
        });
        cx.emit(DismissEvent);
    }
}

pub struct StashRenameModal {
    repo: Entity<Repository>,
    entry: StashEntry,
    name_editor: Entity<Editor>,
    focus_handle: FocusHandle,
}

impl StashRenameModal {
    fn new(
        repo: Entity<Repository>,
        entry: StashEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial = entry.message.to_string();
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial, window, cx);
            editor
        });
        let focus_handle = name_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
        Self {
            repo,
            entry,
            name_editor,
            focus_handle,
        }
    }
}

impl Focusable for StashRenameModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for StashRenameModal {}
impl ModalView for StashRenameModal {}

impl Render for StashRenameModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let stash_ref = self.entry.stash_ref();
        v_flex()
            .key_context("StashRenameModal")
            .elevation_2(cx)
            .w(rems(34.))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.commit_rename(window, cx);
            }))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::Pencil).size(IconSize::XSmall))
                    .child(
                        Headline::new(format!("Rename {}", stash_ref)).size(HeadlineSize::XSmall),
                    ),
            )
            .child(
                v_flex()
                    .px_3()
                    .pb_1()
                    .gap_1()
                    .child(
                        Label::new(
                            "Rename drops the stash and re-creates it from the working tree state \
                             — the resulting stash sha will differ. The auto-backup ref keeps the \
                             prior stash recoverable for 30 days.",
                        )
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                    )
                    .child(self.name_editor.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .pb_3()
                    .pt_1()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("stash-rename-cancel", "Cancel").on_click(cx.listener(
                            |_, _, _, cx| {
                                cx.emit(DismissEvent);
                            },
                        )),
                    )
                    .child(
                        Button::new("stash-rename-confirm", "Rename")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.commit_rename(window, cx);
                            })),
                    ),
            )
    }
}

impl StashRenameModal {
    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let new_message = self.name_editor.read(cx).text(cx).trim().to_string();
        if new_message.is_empty() || new_message == self.entry.message.as_ref() {
            cx.emit(DismissEvent);
            return;
        }
        let repo = self.repo.clone();
        let index = self.entry.index;
        cx.spawn(async move |_, cx| {
            repo.update(cx, |repo, cx| repo.stash_drop(Some(index), cx))
                .await??;
            repo.update(cx, |repo, cx| {
                repo.stash_push(Some(new_message), true, false, cx)
            })
            .await??;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to rename stash", window, cx, |e, _, _| {
            Some(e.to_string())
        });
        cx.emit(DismissEvent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::Oid;
    use std::str::FromStr;

    fn fake_entry(index: usize, message: &str) -> GitStashEntry {
        let oid = Oid::from_str(&format!("{:0>40x}", index)).unwrap();
        GitStashEntry {
            index,
            oid,
            message: message.to_string(),
            branch: Some(format!("branch-{index}")),
            timestamp: 1_700_000_000 + index as i64,
        }
    }

    #[test]
    fn entry_from_git_picks_up_stat_when_present() {
        let raw = fake_entry(0, "WIP");
        let stat = git::stash::StashStat {
            file_count: 3,
            has_untracked: true,
        };
        let entry = StashesView::entry_from_git(&raw, Some(stat));
        assert_eq!(entry.index, 0);
        assert_eq!(entry.file_count, 3);
        assert!(entry.has_untracked);
        assert_eq!(entry.branch.as_deref(), Some("branch-0"));
    }

    #[test]
    fn entry_from_git_defaults_to_zero_when_stat_missing() {
        let raw = fake_entry(2, "msg");
        let entry = StashesView::entry_from_git(&raw, None);
        assert_eq!(entry.file_count, 0);
        assert!(!entry.has_untracked);
        assert_eq!(entry.message.as_ref(), "msg");
    }
}
