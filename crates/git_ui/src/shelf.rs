//! S-SHL — Shelf pane Item.
//!
//! Top-level pane item that lists named [`git::operations::shelf::ShelfEntry`]s
//! for the active repository, with a detail pane showing the description +
//! file summary + stash patch. Mirrors the structure of [`crate::stashes`]
//! (S-STH) — stickable surface with filter, list, detail, per-row context
//! menu, and a "Shelve Current Changes…" form modal.

use std::any::TypeId;
use std::sync::Arc;

use editor::{Editor, EditorEvent, MultiBuffer};
use git::operations::shelf::{self, FilesSummary, ShelfEntry};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseDownEvent,
    ParentElement, Pixels, Point, PromptLevel, Render, SharedString, Styled, Subscription, Task,
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

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(ShelfView::deploy);
}

#[derive(Clone, Debug)]
struct ShelfRow {
    entry: ShelfEntry,
    is_orphaned: bool,
}

pub struct ShelfView {
    repo: Option<Entity<Repository>>,
    workspace: WeakEntity<Workspace>,
    #[allow(dead_code)]
    project: Entity<Project>,
    rows: Vec<ShelfRow>,
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

impl ShelfView {
    pub fn deploy(
        workspace: &mut Workspace,
        _: &git::Shelf,
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
            editor.set_placeholder_text("Filter shelf entries…", window, cx);
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
            rows: Vec::new(),
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

    fn refresh_entries(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.rows.clear();
            self.selected = None;
            cx.notify();
            return;
        };
        let work_dir = repo.read(cx).work_directory_abs_path.clone();
        let prior_sha = self
            .selected
            .and_then(|ix| self.rows.get(ix))
            .map(|row| row.entry.stash_sha.clone());
        cx.spawn(async move |this, cx| {
            let load_result: anyhow::Result<(Vec<ShelfEntry>, Vec<String>)> = cx
                .background_spawn(async move {
                    let store = shelf::ShelfStore::load(&work_dir)?;
                    let entries: Vec<ShelfEntry> = store.entries().to_vec();
                    let orphans = store.lookup_orphaned(&work_dir);
                    Ok((entries, orphans))
                })
                .await;
            let (entries, orphans) = match load_result {
                Ok(pair) => pair,
                Err(err) => {
                    log::warn!("shelf: failed to load store: {err}");
                    return;
                }
            };
            this.update(cx, |this, cx| {
                this.rows = entries
                    .into_iter()
                    .map(|entry| {
                        let is_orphaned = orphans.iter().any(|name| name == &entry.name);
                        ShelfRow { entry, is_orphaned }
                    })
                    .collect();
                if this.rows.is_empty() {
                    this.selected = None;
                    this.detail_for_sha = None;
                    this.detail_buffer.update(cx, |buffer, cx| {
                        buffer.set_capability(Capability::ReadWrite, cx);
                        let len = buffer.len();
                        buffer.edit([(0..len, "")], None, cx);
                        buffer.set_capability(Capability::ReadOnly, cx);
                    });
                } else {
                    this.selected = match prior_sha
                        .as_deref()
                        .and_then(|sha| this.rows.iter().position(|row| row.entry.stash_sha == sha))
                    {
                        Some(ix) => Some(ix),
                        None => Some(0),
                    };
                    if let Some(ix) = this.selected {
                        if let Some(row) = this.rows.get(ix).cloned() {
                            this.load_detail(row.entry, cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.rows.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(ix, row)| {
                let haystack = format!(
                    "{} {} {}",
                    row.entry.name,
                    row.entry.description.as_deref().unwrap_or(""),
                    row.entry.source_branch.as_deref().unwrap_or(""),
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

    fn select_entry(&mut self, ix: usize, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        if let Some(row) = self.rows.get(ix).cloned() {
            self.load_detail(row.entry, cx);
        }
        cx.notify();
    }

    fn load_detail(&mut self, entry: ShelfEntry, cx: &mut Context<Self>) {
        if self.detail_for_sha.as_deref() == Some(entry.stash_sha.as_str()) {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.detail_for_sha = Some(entry.stash_sha.clone());
        let stash_ref = entry.stash_sha.clone();
        let task = repo.update(cx, |repo, _| repo.stash_show_patch(stash_ref));
        let target_sha = entry.stash_sha;
        cx.spawn(async move |this, cx| {
            let patch = match task.await {
                Ok(Ok(text)) => text,
                Ok(Err(err)) => format!("(failed to load shelf diff: {err})"),
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

    fn selected_row(&self) -> Option<&ShelfRow> {
        self.selected.and_then(|ix| self.rows.get(ix))
    }

    fn dispatch_apply(
        &mut self,
        entry: ShelfEntry,
        remove: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let work_dir = repo.read(cx).work_directory_abs_path.clone();
        let name = entry.name;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_spawn(async move { shelf::apply(&work_dir, &name, remove) })
                .await?;
            this.update_in(cx, |this, window, cx| {
                this.refresh_entries(window, cx);
            })
            .ok();
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to apply shelf entry", window, cx, |e, _, _| {
            Some(e.to_string())
        });
    }

    fn dispatch_drop(&mut self, entry: ShelfEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let work_dir = repo.read(cx).work_directory_abs_path.clone();
        let name = entry.name;
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!(
                "Drop shelf entry {:?}? The underlying stash is also dropped.",
                name
            ),
            None,
            &["Drop", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return anyhow::Ok(());
            }
            cx.background_spawn(async move { shelf::drop(&work_dir, &name) })
                .await?;
            this.update_in(cx, |this, window, cx| {
                this.refresh_entries(window, cx);
            })
            .ok();
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to drop shelf entry", window, cx, |e, _, _| {
            Some(e.to_string())
        });
    }

    fn dispatch_forget(&mut self, entry: ShelfEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let work_dir = repo.read(cx).work_directory_abs_path.clone();
        let name = entry.name;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_spawn(async move {
                let mut store = shelf::ShelfStore::load(&work_dir)?;
                store.remove(&name)?;
                anyhow::Ok(())
            })
            .await?;
            this.update_in(cx, |this, window, cx| {
                this.refresh_entries(window, cx);
            })
            .ok();
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to forget shelf entry", window, cx, |e, _, _| {
            Some(e.to_string())
        });
    }

    fn dispatch_view_diff(
        &mut self,
        entry: ShelfEntry,
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
            entry.stash_sha,
            repo.downgrade(),
            workspace.downgrade(),
            None,
            None,
            window,
            cx,
        );
    }

    fn dispatch_rename(&mut self, entry: ShelfEntry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    ShelfRenameModal::new(repo, entry, window, cx)
                });
            });
        }
    }

    fn dispatch_edit_description(
        &mut self,
        entry: ShelfEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    ShelfEditDescriptionModal::new(repo, entry, window, cx)
                });
            });
        }
    }

    fn dispatch_copy_sha(&mut self, entry: ShelfEntry, _: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(entry.stash_sha));
    }

    fn open_shelve_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                ShelveCurrentChangesModal::new(repo, window, cx)
            });
        });
    }

    fn deploy_row_context_menu(
        &mut self,
        ix: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.get(ix).cloned() else {
            return;
        };
        let view = cx.weak_entity();
        let entry = row.entry.clone();
        let is_orphaned = row.is_orphaned;
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let mut menu = menu;
            if is_orphaned {
                menu = menu
                    .entry("Forget (stash already gone)", None, {
                        let view = view.clone();
                        let entry = entry.clone();
                        move |window, cx| {
                            let entry = entry.clone();
                            view.update(cx, |view, cx| view.dispatch_forget(entry, window, cx))
                                .ok();
                        }
                    })
                    .separator();
            } else {
                menu = menu
                    .entry("Apply", None, {
                        let view = view.clone();
                        let entry = entry.clone();
                        move |window, cx| {
                            let entry = entry.clone();
                            view.update(cx, |view, cx| {
                                view.dispatch_apply(entry, false, window, cx)
                            })
                            .ok();
                        }
                    })
                    .entry("Apply and Remove", None, {
                        let view = view.clone();
                        let entry = entry.clone();
                        move |window, cx| {
                            let entry = entry.clone();
                            view.update(cx, |view, cx| {
                                view.dispatch_apply(entry, true, window, cx)
                            })
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
                    .entry("Compare with Working Tree", None, {
                        let view = view.clone();
                        let entry = entry.clone();
                        move |window, cx| {
                            let entry = entry.clone();
                            view.update(cx, |view, cx| view.dispatch_view_diff(entry, window, cx))
                                .ok();
                        }
                    })
                    .entry("Rename…", None, {
                        let view = view.clone();
                        let entry = entry.clone();
                        move |window, cx| {
                            let entry = entry.clone();
                            view.update(cx, |view, cx| view.dispatch_rename(entry, window, cx))
                                .ok();
                        }
                    })
                    .entry("Edit Description…", None, {
                        let view = view.clone();
                        let entry = entry.clone();
                        move |window, cx| {
                            let entry = entry.clone();
                            view.update(cx, |view, cx| {
                                view.dispatch_edit_description(entry, window, cx)
                            })
                            .ok();
                        }
                    });
            }
            menu = menu.separator().entry("Copy Stash SHA", None, {
                move |window, cx| {
                    let entry = entry.clone();
                    view.update(cx, |view, cx| view.dispatch_copy_sha(entry, window, cx))
                        .ok();
                }
            });
            menu
        });
        let subscription =
            cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _window, cx| {
                this.context_menu.take();
                cx.notify();
            });
        self.context_menu = Some((menu, position, subscription));
        self.selected = Some(ix);
        cx.notify();
    }

    fn render_row(&self, ix: usize, row: &ShelfRow, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected == Some(ix);
        let timezone = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let timestamp = OffsetDateTime::from_unix_timestamp(row.entry.created_at_unix)
            .unwrap_or_else(|_| OffsetDateTime::now_utc());
        let relative = time_format::format_localized_timestamp(
            timestamp,
            OffsetDateTime::now_utc(),
            timezone,
            time_format::TimestampFormat::Relative,
        );
        let summary = files_count_label(&row.entry.files_summary);
        let description = row
            .entry
            .description
            .as_deref()
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from(""));

        ListItem::new(("shelf-row", ix))
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .start_slot(
                Icon::new(if row.is_orphaned {
                    IconName::Warning
                } else {
                    IconName::Hash
                })
                .size(IconSize::Small)
                .color(if row.is_orphaned {
                    Color::Warning
                } else {
                    Color::Muted
                }),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(Label::new(SharedString::from(row.entry.name.clone())).truncate())
                    .child(
                        h_flex()
                            .gap_1p5()
                            .when(!description.is_empty(), |this| {
                                this.child(
                                    Label::new(description.clone())
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
                                Label::new(summary)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .when(row.is_orphaned, |this| {
                                this.child(
                                    Label::new("orphaned")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                )
                            }),
                    ),
            )
            .on_click(cx.listener(move |this, _event, window, cx| {
                this.select_entry(ix, window, cx);
            }))
            .on_secondary_mouse_down(cx.listener(
                move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_row_context_menu(ix, event.position, window, cx);
                },
            ))
            .into_any_element()
    }
}

fn files_count_label(summary: &FilesSummary) -> SharedString {
    let total = summary.count_added + summary.count_modified + summary.count_deleted;
    if total == 0 {
        return SharedString::from("0 files");
    }
    SharedString::from(format!(
        "{} file{} (+{}/-{})",
        total,
        if total == 1 { "" } else { "s" },
        summary.total_lines_added,
        summary.total_lines_removed,
    ))
}

impl Focusable for ShelfView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for ShelfView {}

impl Render for ShelfView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible: Vec<usize> = self.filtered_indices();
        let row_count = visible.len();
        let total = self.rows.len();
        let count_label = if total == row_count {
            format!("{} entr{}", total, if total == 1 { "y" } else { "ies" })
        } else {
            format!("{} of {} entries", row_count, total)
        };
        let selected_row = self.selected_row().cloned();

        v_flex()
            .key_context("ShelfView")
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
                                let rows = self.rows.clone();
                                uniform_list(
                                    "shelf-list",
                                    row_count,
                                    cx.processor(
                                        move |this, range: std::ops::Range<usize>, _window, cx| {
                                            let mut items = Vec::with_capacity(range.len());
                                            for i in range {
                                                let Some(&ix) = visible.get(i) else {
                                                    continue;
                                                };
                                                let Some(row) = rows.get(ix) else {
                                                    continue;
                                                };
                                                items.push(this.render_row(ix, row, cx));
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
                            .child(self.render_detail_header(&selected_row, cx))
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

impl ShelfView {
    fn render_detail_header(&self, selected: &Option<ShelfRow>, _cx: &Context<Self>) -> AnyElement {
        match selected {
            Some(row) => h_flex()
                .w_full()
                .px_2()
                .py_1p5()
                .gap_2()
                .child(
                    Icon::new(IconName::Hash)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(SharedString::from(row.entry.name.clone())).size(LabelSize::Small),
                )
                .when_some(row.entry.source_branch.clone(), |this, branch| {
                    this.child(
                        Label::new("•")
                            .alpha(0.5)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(SharedString::from(branch))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .when_some(row.entry.description.clone(), |this, desc| {
                    this.child(
                        Label::new("•")
                            .alpha(0.5)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(SharedString::from(desc))
                            .truncate()
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .into_any_element(),
            None => h_flex()
                .w_full()
                .px_2()
                .py_1p5()
                .child(
                    Label::new("No shelf entry selected")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
        }
    }

    fn render_bottom_toolbar(&self, cx: &Context<Self>) -> AnyElement {
        let has_selection = self.selected_row().is_some();
        let is_orphaned = self.selected_row().map(|r| r.is_orphaned).unwrap_or(false);
        h_flex()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("shelf-current-changes", "Shelve Current Changes…")
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_shelve_form(window, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("shelf-apply", "Apply")
                    .disabled(!has_selection || is_orphaned)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(row) = this.selected_row().cloned() {
                            this.dispatch_apply(row.entry, false, window, cx);
                        }
                    })),
            )
            .child(
                Button::new("shelf-apply-remove", "Apply & Remove")
                    .disabled(!has_selection || is_orphaned)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(row) = this.selected_row().cloned() {
                            this.dispatch_apply(row.entry, true, window, cx);
                        }
                    })),
            )
            .child(
                IconButton::new("shelf-drop", IconName::Trash)
                    .shape(IconButtonShape::Square)
                    .disabled(!has_selection)
                    .tooltip(Tooltip::text("Drop selected entry"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(row) = this.selected_row().cloned() {
                            if row.is_orphaned {
                                this.dispatch_forget(row.entry, window, cx);
                            } else {
                                this.dispatch_drop(row.entry, window, cx);
                            }
                        }
                    })),
            )
            .into_any_element()
    }
}

impl Item for ShelfView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Hash).color(Color::Muted))
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
        "Shelf".into()
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some("Git Shelf".into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Shelf Pane Opened")
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

// =====================================================================
// Modals
// =====================================================================

pub struct ShelveCurrentChangesModal {
    repo: Entity<Repository>,
    name_editor: Entity<Editor>,
    description_editor: Entity<Editor>,
    remove_after: bool,
    focus_handle: FocusHandle,
}

impl ShelveCurrentChangesModal {
    fn new(repo: Entity<Repository>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Name (required)…", window, cx);
            editor
        });
        let description_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text("Description (optional, multiline)…", window, cx);
            editor
        });
        let focus_handle = name_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
        Self {
            repo,
            name_editor,
            description_editor,
            remove_after: true,
            focus_handle,
        }
    }

    fn commit_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        let description = {
            let raw = self.description_editor.read(cx).text(cx);
            let trimmed = raw.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        };
        let work_dir = self.repo.read(cx).work_directory_abs_path.clone();
        let remove_after = self.remove_after;
        let task: Task<anyhow::Result<()>> = cx.background_spawn(async move {
            shelf::shelve(&work_dir, &name, description, None, remove_after)?;
            Ok(())
        });
        cx.spawn(async move |_, _| task.await)
            .detach_and_prompt_err("Failed to shelve changes", window, cx, |e, _, _| {
                Some(e.to_string())
            });
        cx.emit(DismissEvent);
    }
}

impl Focusable for ShelveCurrentChangesModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ShelveCurrentChangesModal {}
impl ModalView for ShelveCurrentChangesModal {}

impl Render for ShelveCurrentChangesModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let remove_after = self.remove_after;
        v_flex()
            .key_context("ShelveCurrentChangesModal")
            .elevation_2(cx)
            .w(rems(40.))
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
                    .child(Icon::new(IconName::Hash).size(IconSize::XSmall))
                    .child(Headline::new("Shelve Current Changes").size(HeadlineSize::XSmall)),
            )
            .child(
                v_flex()
                    .px_3()
                    .pb_2()
                    .gap_2()
                    .child(self.name_editor.clone())
                    .child(self.description_editor.clone()),
            )
            .child(
                v_flex().px_3().pb_2().gap_1().child(
                    ui::Checkbox::new("shelf-remove-after", ui::ToggleState::from(remove_after))
                        .label("Remove from working tree after shelve")
                        .on_click(cx.listener(|this, state: &ui::ToggleState, _, cx| {
                            this.remove_after = matches!(state, ui::ToggleState::Selected);
                            cx.notify();
                        })),
                ),
            )
            .child(
                v_flex().px_3().pb_2().gap_1().child(
                    Label::new(
                        "Files: shelves the entire working-tree diff. \
                         Per-file selection comes in a future iteration.",
                    )
                    .color(Color::Muted)
                    .size(LabelSize::Small),
                ),
            )
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .pb_3()
                    .gap_2()
                    .justify_end()
                    .child(Button::new("shelf-cancel", "Cancel").on_click(cx.listener(
                        |_, _, _, cx| {
                            cx.emit(DismissEvent);
                        },
                    )))
                    .child(
                        Button::new("shelf-confirm", "Shelve")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.commit_form(window, cx);
                            })),
                    ),
            )
    }
}

pub struct ShelfRenameModal {
    repo: Entity<Repository>,
    entry: ShelfEntry,
    name_editor: Entity<Editor>,
    focus_handle: FocusHandle,
}

impl ShelfRenameModal {
    fn new(
        repo: Entity<Repository>,
        entry: ShelfEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial = entry.name.clone();
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

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let new_name = self.name_editor.read(cx).text(cx).trim().to_string();
        if new_name.is_empty() || new_name == self.entry.name {
            cx.emit(DismissEvent);
            return;
        }
        let work_dir = self.repo.read(cx).work_directory_abs_path.clone();
        let old = self.entry.name.clone();
        let task: Task<anyhow::Result<()>> = cx.background_spawn(async move {
            let mut store = shelf::ShelfStore::load(&work_dir)?;
            store.rename(&old, &new_name)?;
            Ok(())
        });
        cx.spawn(async move |_, _| task.await)
            .detach_and_prompt_err("Failed to rename shelf entry", window, cx, |e, _, _| {
                Some(e.to_string())
            });
        cx.emit(DismissEvent);
    }
}

impl Focusable for ShelfRenameModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ShelfRenameModal {}
impl ModalView for ShelfRenameModal {}

impl Render for ShelfRenameModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ShelfRenameModal")
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
                    .child(Headline::new("Rename Shelf Entry").size(HeadlineSize::XSmall)),
            )
            .child(div().px_3().pb_3().w_full().child(self.name_editor.clone()))
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .pb_3()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("shelf-rename-cancel", "Cancel").on_click(cx.listener(
                            |_, _, _, cx| {
                                cx.emit(DismissEvent);
                            },
                        )),
                    )
                    .child(
                        Button::new("shelf-rename-confirm", "Rename")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.commit_rename(window, cx);
                            })),
                    ),
            )
    }
}

pub struct ShelfEditDescriptionModal {
    repo: Entity<Repository>,
    entry: ShelfEntry,
    description_editor: Entity<Editor>,
    focus_handle: FocusHandle,
}

impl ShelfEditDescriptionModal {
    fn new(
        repo: Entity<Repository>,
        entry: ShelfEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial = entry.description.clone().unwrap_or_default();
        let description_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(initial, window, cx);
            editor
        });
        let focus_handle = description_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
        Self {
            repo,
            entry,
            description_editor,
            focus_handle,
        }
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.description_editor.read(cx).text(cx);
        let trimmed = raw.trim();
        let desc = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        let work_dir = self.repo.read(cx).work_directory_abs_path.clone();
        let name = self.entry.name.clone();
        let task: Task<anyhow::Result<()>> = cx.background_spawn(async move {
            let mut store = shelf::ShelfStore::load(&work_dir)?;
            store.update_description(&name, desc)?;
            Ok(())
        });
        cx.spawn(async move |_, _| task.await)
            .detach_and_prompt_err("Failed to update description", window, cx, |e, _, _| {
                Some(e.to_string())
            });
        cx.emit(DismissEvent);
    }
}

impl Focusable for ShelfEditDescriptionModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ShelfEditDescriptionModal {}
impl ModalView for ShelfEditDescriptionModal {}

impl Render for ShelfEditDescriptionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ShelfEditDescriptionModal")
            .elevation_2(cx)
            .w(rems(40.))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.commit(window, cx);
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
                        Headline::new(format!("Edit Description — {}", self.entry.name))
                            .size(HeadlineSize::XSmall),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pb_3()
                    .w_full()
                    .child(self.description_editor.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .pb_3()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("shelf-desc-cancel", "Cancel").on_click(cx.listener(
                            |_, _, _, cx| {
                                cx.emit(DismissEvent);
                            },
                        )),
                    )
                    .child(
                        Button::new("shelf-desc-save", "Save")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.commit(window, cx);
                            })),
                    ),
            )
    }
}
