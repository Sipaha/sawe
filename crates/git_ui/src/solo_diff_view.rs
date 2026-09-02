use crate::{git_panel::GitStatusEntry, git_status_icon, soft_wrap_button};
use anyhow::Result;
use editor::{
    Direction, Editor, EditorEvent, EditorSettings, SplittableEditor, ToggleSplitDiff,
    actions::{GoToHunk, GoToPreviousHunk, ToggleSoftWrap},
};
use fs::Fs;
use git::repository::RepoPath;
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Render, Subscription, Task, WeakEntity, Window,
};
use language::{Buffer, HighlightedText};
use multi_buffer::{MultiBuffer, MultiBufferSnapshot};
use project::{
    Project,
    git_store::{Repository, RepositoryId},
};
use settings::{DiffViewStyle, Settings, SettingsStore, update_settings_file};
use std::{
    any::{Any, TypeId},
    cell::Cell,
    sync::Arc,
};
use ui::{
    Color, DiffStat, Icon, IconButton, IconName, Label, LabelCommon as _, SharedString, Tooltip,
    prelude::*, vertical_divider,
};
use util::paths::{PathExt as _, PathStyle};
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
    item::{ItemEvent, PreviewTabsSettings, SaveOptions, TabContentParams},
    searchable::SearchableItemHandle,
};

/// Memoized `MultiBufferSnapshot::diff_hunks().count()`.
///
/// Counting hunks walks every excerpt and every hunk of the multibuffer with a
/// couple of O(log n) anchor resolutions per hunk. That is nothing for a
/// single-file diff, but a project diff over several hundred files repaints its
/// toolbar often enough that redoing the walk each time is worth avoiding.
///
/// Every component of the key is an O(1) sum-tree summary read: `edit_count`
/// moves on any buffer edit, `non_text_state_update_count` on any buffer
/// non-text change, and the changed-row totals on any change to the diff itself
/// (including the async arrival of the initial diff). Staging does not move the
/// key, and correctly so — it only rewrites secondary hunk status, never the
/// hunk count.
#[derive(Default)]
pub(crate) struct HunkCountCache(Cell<Option<(HunkCountKey, usize)>>);

#[derive(Clone, Copy, PartialEq, Eq)]
struct HunkCountKey {
    edit_count: usize,
    non_text_state_update_count: usize,
    added_rows: u32,
    removed_rows: u32,
}

impl HunkCountCache {
    pub(crate) fn count(&self, snapshot: &MultiBufferSnapshot) -> usize {
        let (added_rows, removed_rows) = snapshot.total_changed_lines();
        let key = HunkCountKey {
            edit_count: snapshot.edit_count(),
            non_text_state_update_count: snapshot.non_text_state_update_count(),
            added_rows,
            removed_rows,
        };
        if let Some((cached_key, count)) = self.0.get()
            && cached_key == key
        {
            return count;
        }
        let count = snapshot.diff_hunks().count();
        self.0.set(Some((key, count)));
        count
    }
}

/// How a [`SoloDiffView`] should be placed in the pane.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SoloDiffOpen {
    /// Replaceable preview tab (single click / selection-follow). Rendered
    /// italic and swapped out when the next preview opens. Keeps keyboard focus
    /// wherever it was (e.g. the git panel), so arrow-nav can keep driving it.
    Preview,
    /// Pinned permanent tab (double-click / Enter). Moves focus into the diff.
    Permanent,
}

pub struct SoloDiffView {
    repository: Entity<Repository>,
    repository_id: RepositoryId,
    repo_path: RepoPath,
    buffer: Entity<Buffer>,
    editor: Entity<SplittableEditor>,
    workspace: WeakEntity<Workspace>,
    hunk_count_cache: HunkCountCache,
    _settings_subscription: Subscription,
}

impl SoloDiffView {
    pub fn open_or_focus(
        entry: GitStatusEntry,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        mode: SoloDiffOpen,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let Some(workspace_entity) = workspace.upgrade() else {
            return Task::ready(Err(anyhow::anyhow!("workspace was dropped")));
        };

        let existing = workspace_entity
            .read(cx)
            .items_of_type::<SoloDiffView>(cx)
            .find(|item| item.read(cx).matches(&repository, &entry.repo_path, cx));
        if let Some(existing) = existing {
            let focus_item = mode == SoloDiffOpen::Permanent;
            workspace_entity.update(cx, |workspace, cx| {
                workspace.activate_item(&existing, true, focus_item, window, cx);
                // A deliberate "open" pins an existing preview; a preview gesture
                // never demotes an already-pinned tab.
                if mode == SoloDiffOpen::Permanent {
                    workspace.active_pane().update(cx, |pane, _cx| {
                        pane.unpreview_item_if_preview(existing.item_id());
                    });
                }
            });
            if focus_item {
                existing.focus_handle(cx).focus(window, cx);
            }
            return Task::ready(Ok(existing));
        }

        let Some(project_path) = repository
            .read(cx)
            .repo_path_to_project_path(&entry.repo_path, cx)
        else {
            return Task::ready(Err(anyhow::anyhow!(
                "could not resolve repository path {:?}",
                entry.repo_path
            )));
        };

        let project = workspace_entity.read(cx).project().clone();
        let repo_path = entry.repo_path;
        window.spawn(cx, async move |cx| {
            let buffer = project
                .update(cx, |project, cx| {
                    project.open_buffer(project_path.clone(), cx)
                })
                .await?;
            let diff = project
                .update(cx, |project, cx| {
                    project.open_uncommitted_diff(buffer.clone(), cx)
                })
                .await?;

            workspace_entity.update_in(cx, |workspace, window, cx| {
                let workspace_handle = cx.entity();
                let view = cx.new(|cx| {
                    Self::new(
                        project,
                        repository,
                        repo_path,
                        buffer,
                        diff,
                        workspace_handle,
                        window,
                        cx,
                    )
                });

                let item: Box<dyn ItemHandle> = Box::new(view.clone());
                let focus_item = mode == SoloDiffOpen::Permanent;
                workspace.active_pane().update(cx, |pane, cx| {
                    // A preview opens into (and replaces) the pane's single
                    // preview slot; a permanent open appends its own tab.
                    let destination_index = if mode == SoloDiffOpen::Preview
                        && PreviewTabsSettings::get_global(cx).enabled
                    {
                        pane.replace_preview_item_id(item.item_id(), window, cx)
                    } else {
                        None
                    };
                    pane.add_item(item, true, focus_item, destination_index, window, cx);
                });
                view
            })
        })
    }

    fn new(
        project: Entity<Project>,
        repository: Entity<Repository>,
        repo_path: RepoPath,
        buffer: Entity<Buffer>,
        diff: Entity<buffer_diff::BufferDiff>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository_id = repository.read(cx).id;
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::singleton(buffer.clone(), cx);
            multibuffer.add_diff(diff, cx);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let mut editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project.clone(),
                workspace.clone(),
                window,
                cx,
            );
            // This view shows uncommitted changes, so the left pane's text is
            // the file's content at HEAD.
            editor.set_lhs_blame_base(Some("HEAD".into()), cx);
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_should_serialize(false, cx);
                let snapshot = editor.snapshot(window, cx);
                editor.go_to_hunk_before_or_after_position(
                    &snapshot,
                    language::Point::new(0, 0),
                    Direction::Next,
                    true,
                    window,
                    cx,
                );
            });
            editor
        });

        let mut previous_diff_view_style = EditorSettings::get_global(cx).diff_view_style;
        let settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, move |this, window, cx| {
                let diff_view_style = EditorSettings::get_global(cx).diff_view_style;
                if diff_view_style != previous_diff_view_style {
                    this.editor.update(cx, |editor, cx| {
                        if editor.diff_view_style() != diff_view_style {
                            editor.toggle_split(&ToggleSplitDiff, window, cx);
                        }
                    });
                    previous_diff_view_style = diff_view_style;
                    cx.notify();
                }
            });

        Self {
            repository,
            repository_id,
            repo_path,
            buffer,
            editor,
            workspace: workspace.downgrade(),
            hunk_count_cache: HunkCountCache::default(),
            _settings_subscription: settings_subscription,
        }
    }

    fn matches(&self, repository: &Entity<Repository>, repo_path: &RepoPath, cx: &App) -> bool {
        self.repository_id == repository.read(cx).id && &self.repo_path == repo_path
    }

    /// The repository this diff belongs to. Read by the git panel to decide
    /// whether one of *its* rows is the diff the pane is showing; the same
    /// relative path can exist in a second repository in the window.
    pub fn repository_id(&self) -> RepositoryId {
        self.repository_id
    }

    /// The working-copy file this diff is showing.
    pub fn repo_path(&self) -> &RepoPath {
        &self.repo_path
    }

    /// Number of diff hunks in this file's diff. Also gates the prev/next-hunk
    /// buttons: with a single hunk there is nowhere to navigate to.
    fn hunk_count(&self, cx: &App) -> usize {
        let editor = self.editor.read(cx).rhs_editor().read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        self.hunk_count_cache.count(&snapshot)
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut App) {
        self.focus_handle(cx).focus(window, cx);
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        });
    }
}

impl EventEmitter<EditorEvent> for SoloDiffView {}

impl Focusable for SoloDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for SoloDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
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

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.buffer
            .read(cx)
            .file()
            .and_then(|file| {
                Some(
                    file.full_path(cx)
                        .file_name()?
                        .to_string_lossy()
                        .to_string(),
                )
            })
            .unwrap_or_else(|| {
                self.repo_path
                    .as_ref()
                    .display(PathStyle::local())
                    .into_owned()
            })
            .into()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(
            self.buffer
                .read(cx)
                .file()
                .map(|file| file.full_path(cx).compact().to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    self.repo_path
                        .as_ref()
                        .display(PathStyle::local())
                        .into_owned()
                })
                .into(),
        )
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Solo Diff View Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.deactivated(window, cx);
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            })
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor
                .rhs_editor()
                .update(cx, |editor, cx| editor.navigate(data, window, cx))
        })
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        // Defer to the embedded editor: respects `toolbar.breadcrumbs`
        // (hidden by default in this fork — the tab already names the file).
        self.editor.breadcrumb_location(cx)
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<gpui::Font>)> {
        self.editor.breadcrumbs(cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.added_to_workspace(workspace, window, cx)
            })
        });
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).rhs_editor().read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.save(options, project, window, cx)
    }
}

impl Render for SoloDiffView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}

pub struct SoloDiffStyleToolbar {
    solo_diff: Option<WeakEntity<SoloDiffView>>,
}

pub struct SoloDiffGitToolbar {
    solo_diff: Option<WeakEntity<SoloDiffView>>,
}

impl SoloDiffStyleToolbar {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self { solo_diff: None }
    }

    fn solo_diff(&self) -> Option<Entity<SoloDiffView>> {
        self.solo_diff.as_ref()?.upgrade()
    }

    fn set_diff_view_style(
        &mut self,
        diff_view_style: DiffViewStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(solo_diff) = self.solo_diff() else {
            return;
        };
        let workspace = solo_diff.read(cx).workspace.clone();

        update_settings_file(<dyn Fs>::global(cx), cx, move |settings, _| {
            settings.editor.diff_view_style = Some(diff_view_style);
        });

        if let Some(workspace) = workspace.upgrade() {
            let splittable_editors = {
                workspace
                    .read(cx)
                    .items(cx)
                    .filter_map(|item| item.act_as_type(TypeId::of::<SplittableEditor>(), cx))
                    .filter_map(|item| item.downcast::<SplittableEditor>().ok())
                    .collect::<Vec<_>>()
            };

            for editor in splittable_editors {
                editor.update(cx, |editor, cx| {
                    if editor.diff_view_style() != diff_view_style {
                        editor.toggle_split(&ToggleSplitDiff, window, cx);
                    }
                });
            }
        }

        cx.notify();
    }
}

impl EventEmitter<ToolbarItemEvent> for SoloDiffStyleToolbar {}

impl ToolbarItemView for SoloDiffStyleToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.solo_diff = active_pane_item
            .and_then(|item| item.act_as::<SoloDiffView>(cx))
            .map(|entity| entity.downgrade());
        if self.solo_diff.is_some() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl SoloDiffStyleToolbar {
    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(solo_diff) = self.solo_diff() {
            solo_diff.update(cx, |solo_diff, cx| {
                solo_diff.dispatch_action(action, window, cx);
            });
        }
    }
}

impl Render for SoloDiffStyleToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(solo_diff) = self.solo_diff() else {
            return div();
        };
        let focus_handle = solo_diff.focus_handle(cx);
        let prev_next = solo_diff.read(cx).hunk_count(cx) > 1;
        let editor_entity = solo_diff.read(cx).editor.clone();
        let editor = editor_entity.read(cx);
        let diff_view_style = editor.diff_view_style();
        let is_soft_wrap_enabled = editor.is_soft_wrap_enabled(cx);
        let is_split_set = diff_view_style == DiffViewStyle::Split;
        let split_icon = if is_split_set && !editor.is_split() {
            IconName::DiffSplitAuto
        } else {
            IconName::DiffSplit
        };

        h_flex()
            .h_8()
            .items_center()
            .gap_1()
            // IDEA puts change navigation first in the diff toolbar.
            .child(
                IconButton::new("solo-diff-prev", IconName::ArrowUp)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title_in(
                        "Go to previous hunk",
                        &GoToPreviousHunk,
                        &focus_handle,
                    ))
                    .disabled(!prev_next)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&GoToPreviousHunk, window, cx)
                    })),
            )
            .child(
                IconButton::new("solo-diff-next", IconName::ArrowDown)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title_in(
                        "Go to next hunk",
                        &GoToHunk,
                        &focus_handle,
                    ))
                    .disabled(!prev_next)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&GoToHunk, window, cx)
                    })),
            )
            .child(vertical_divider())
            .child(
                IconButton::new("solo-diff-unified", IconName::DiffUnified)
                    .icon_size(IconSize::Small)
                    .toggle_state(diff_view_style == DiffViewStyle::Unified)
                    .tooltip(Tooltip::text("Unified"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_diff_view_style(DiffViewStyle::Unified, window, cx);
                    })),
            )
            .child(
                IconButton::new("solo-diff-split", split_icon)
                    .icon_size(IconSize::Small)
                    .toggle_state(diff_view_style == DiffViewStyle::Split)
                    .tooltip(Tooltip::text("Split"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_diff_view_style(DiffViewStyle::Split, window, cx);
                    })),
            )
            .child(vertical_divider())
            .child(
                soft_wrap_button("solo-diff-soft-wrap", is_soft_wrap_enabled, &focus_handle)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&ToggleSoftWrap, window, cx)
                    })),
            )
            .child(vertical_divider())
            .child(div().w_1())
    }
}

impl SoloDiffGitToolbar {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self { solo_diff: None }
    }

    fn solo_diff(&self) -> Option<Entity<SoloDiffView>> {
        self.solo_diff.as_ref()?.upgrade()
    }
}

impl EventEmitter<ToolbarItemEvent> for SoloDiffGitToolbar {}

impl ToolbarItemView for SoloDiffGitToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.solo_diff = active_pane_item
            .and_then(|item| item.act_as::<SoloDiffView>(cx))
            .map(|entity| entity.downgrade());
        if self.solo_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for SoloDiffGitToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(solo_diff) = self.solo_diff() else {
            return div();
        };
        let solo_diff = solo_diff.read(cx);
        let hunk_count = solo_diff.hunk_count(cx);
        let status_entry = solo_diff
            .repository
            .read(cx)
            .status_for_path(&solo_diff.repo_path);
        let status = status_entry.as_ref().map(|entry| entry.status);
        let diff_stat = status_entry.and_then(|entry| entry.diff_stat);

        h_group_xl()
            .my_neg_1()
            .py_1()
            .items_center()
            .flex_wrap()
            .justify_between()
            .children(status.map(|status| git_status_icon(status).into_any_element()))
            .children(diff_stat.map(|stat| {
                DiffStat::new("solo-diff-stat", stat.added as usize, stat.deleted as usize)
                    .into_any_element()
            }))
            .child(
                Label::new(difference_count_label(hunk_count))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().w_1())
    }
}

/// IntelliJ IDEA's diff toolbar ends with a bare count of differences, worded
/// exactly like this. Zero is rendered rather than hidden so the toolbar slot
/// keeps a stable width and an empty diff reads as "nothing left to review"
/// instead of "the count is missing".
pub(crate) fn difference_count_label(count: usize) -> String {
    if count == 1 {
        "1 difference".to_string()
    } else {
        format!("{count} differences")
    }
}

#[cfg(test)]
mod tests {
    use super::difference_count_label;

    #[test]
    fn difference_count_label_is_singular_only_at_one() {
        assert_eq!(difference_count_label(0), "0 differences");
        assert_eq!(difference_count_label(1), "1 difference");
        assert_eq!(difference_count_label(2), "2 differences");
        assert_eq!(difference_count_label(17), "17 differences");
    }
}
