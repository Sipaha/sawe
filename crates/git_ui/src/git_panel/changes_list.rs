//! The git panel's changes list: the row model behind the "Changes" tab and
//! everything that walks or paints it.
//!
//! Split out of `git_panel.rs` verbatim (bodies, comments and control flow are
//! unchanged; only the visibility of the entry points the parent module still
//! calls was widened to `pub(super)`). Three pieces live here:
//!
//!   - **The model.** `update_visible_entries` rebuilds `entries` (every row,
//!     collapsed or not), `visible_indices` (the subset `uniform_list` actually
//!     paints, in order) and `section_counts` from the repository's cached
//!     status, for both the flat and the tree view mode.
//!   - **The walkers.** `select_first` / `select_previous` / `select_next` /
//!     `select_last` step through `visible_indices` rather than `entries`, so a
//!     collapsed section or directory is skipped instead of being silently
//!     selectable; `visible_position` and `clamp_selection_to_visible` are the
//!     shared primitives that keep the selection on a painted row.
//!   - **The row renderers.** `render_list_header`, `render_status_entry` and
//!     `render_directory_entry`, plus the row primitives only they use
//!     (`row_background_colors`, the chevron pair, `entry_label`,
//!     `list_item_height`, `path_formatted`).
//!
//! `GitPanel` is a single large struct, so these stay inherent methods in a
//! second `impl GitPanel` block — the partial-class idiom this fork already
//! uses for `solution_agent::store` (see FORK.md decision #49 for why a trait
//! seam is a dead end here). The fields themselves (`entries`,
//! `visible_indices`, `section_counts`) remain declared on `GitPanel`.

use super::*;
use gpui::FontWeight;

/// Left padding of a section header row (`Changes` / `Untracked` /
/// `Conflicts`). Headers are the outermost level, so they get the bare row
/// padding.
fn header_row_padding() -> Pixels {
    px(ROW_LEFT_PADDING)
}

/// Left padding of a row *inside* a section — a directory or a file, at tree
/// `depth` (always 0 in flat mode). Section content is stepped in from its
/// header by `SECTION_CONTENT_INDENT` so that a depth-0 file doesn't line up
/// flush with the header it belongs to.
///
/// `INDENT_GUIDE_LEFT_OFFSET` is derived from the same constants and has to
/// keep matching this: the guides are positioned in the list's coordinate
/// space rather than inheriting a row's padding.
fn content_row_padding(depth: usize) -> Pixels {
    px(ROW_LEFT_PADDING + SECTION_CONTENT_INDENT + depth as f32 * TREE_INDENT)
}

/// Resting / hover / active backgrounds for a changed-file row, given the two
/// states it can be in at once: `selected` is the keyboard cursor, and
/// `open_in_pane` is "this is the diff the centre pane is showing". The open
/// row's wash is the stronger of the two — it is the one the eye is hunting
/// for — and a row that is both sums them.
///
/// A free function rather than a `GitPanel` method because the Commit tab's
/// file rows render outside the panel's own `impl` and must paint the same two
/// states the same way; the wash is the shared half of that vocabulary, the
/// bold file name is the other half.
pub(super) fn row_background_colors(
    selected: bool,
    open_in_pane: bool,
    cx: &App,
) -> (Hsla, Hsla, Hsla) {
    let info_color = cx.theme().status().info;
    let colors = cx.theme().colors();

    let base_bg = match (selected, open_in_pane) {
        (true, true) => info_color.alpha(SELECTED_BG_ALPHA + MARKED_BG_ALPHA),
        (true, false) => info_color.alpha(SELECTED_BG_ALPHA),
        (false, true) => info_color.alpha(MARKED_BG_ALPHA),
        (false, false) => colors.ghost_element_background,
    };

    if selected {
        (
            base_bg,
            info_color.alpha(SELECTED_BG_ALPHA + STATE_OPACITY_STEP),
            info_color.alpha(SELECTED_BG_ALPHA + STATE_OPACITY_STEP * 2.0),
        )
    } else {
        (
            base_bg,
            colors.ghost_element_hover,
            colors.ghost_element_active,
        )
    }
}

/// Height of one row of a changed-files list. A free function rather than a
/// `GitPanel` method because the Commit tab's tree pins its rows to the same
/// number: that tab is typographically slaved to this one, and its rows carry
/// the same `LabelSize::Default` text, which does not fit a `ButtonLike`'s own
/// 22px default.
pub(super) fn list_item_height() -> Rems {
    rems(1.75)
}

impl GitPanel {
    pub(super) fn update_visible_entries(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path_style = self.project.read(cx).path_style(cx);
        let bulk_staging = self.bulk_staging.take();
        let last_staged_path_prev_index = bulk_staging
            .as_ref()
            .and_then(|op| self.entry_by_path(&op.anchor));

        // Phase 3: inside a Solution the active member's repository wins over
        // the project-wide default (which follows whichever buffer was focused
        // last); outside one this still lands on the project default.
        self.refresh_active_repository_for_selector(window, cx);
        // S-PCH-HK — pick up the per-repo pre-commit config when the
        // active repository changes (cheap no-op when unchanged).
        self.ensure_pre_commit_config_loaded(cx);
        self.entries.clear();
        self.entries_indices.clear();
        self.single_staged_entry.take();
        self.single_tracked_entry.take();
        self.conflicted_count = 0;
        self.conflicted_staged_count = 0;
        self.changes_count = 0;
        self.diff_stat_total = DiffStat::default();
        self.new_count = 0;
        self.tracked_count = 0;
        self.new_staged_count = 0;
        self.tracked_staged_count = 0;
        self.entry_count = 0;
        self.max_width_item_index = None;
        self.git_access = GitAccess::Yes;

        let sort_by_path = GitPanelSettings::get_global(cx).sort_by_path;
        let is_tree_view = matches!(self.view_mode, GitPanelViewMode::Tree(_));
        let group_by_status = is_tree_view || !sort_by_path;

        if let Some(active_repo) = self.active_repository.as_ref() {
            let access = active_repo.update(cx, |active_repo, cx| active_repo.access(cx));

            cx.spawn_in(window, async move |git_panel, cx| {
                // When the user does not own the `.git` folder, the
                // `GitStore.spawn_local_git_worker` will fail to create the
                // receiver for Git jobs, so this access check will be
                // cancelled.
                //
                // We assume `GitAccess::No` on cancellation. I believe this is
                // imprecise, other failures could also cause cancellation, but
                // the consequence is just showing the "unsafe repo" UI, which
                // seems acceptable for this edge case.
                let access = match access.await {
                    Ok(access) => access,
                    Err(Canceled) => GitAccess::No,
                };

                git_panel.update(cx, |this, _cx| {
                    this.git_access = access;
                })
            })
            .detach_and_log_err(cx);
        }

        let mut changed_entries = Vec::new();
        let mut new_entries = Vec::new();
        let mut conflict_entries = Vec::new();
        let mut single_staged_entry = None;
        let mut staged_count = 0;
        let mut seen_directories = HashSet::default();
        let mut max_width_estimate = 0usize;
        let mut max_width_item_index = None;

        let Some(repo) = self.active_repository.as_ref() else {
            // Just clear entries if no repository is active. `visible_indices`
            // and `section_counts` are only assigned at the very end of this
            // function, so they must be cleared HERE too: leaving them pointing
            // at the `entries` emptied above makes `render_entries` size the
            // uniform_list with a row count whose every lookup then misses, so
            // the list claims N rows and paints none, and `selected_entry`
            // keeps an index that no longer resolves.
            self.visible_indices.clear();
            self.section_counts.clear();
            self.selected_entry = None;
            cx.notify();
            return;
        };

        let repo = repo.read(cx);

        self.stash_entries = repo.cached_stash();

        for entry in repo.cached_status() {
            self.changes_count += 1;
            let is_conflict = repo.had_conflict_on_last_merge_head_change(&entry.repo_path);
            let is_new = entry.status.is_created();
            let staging = entry.status.staging();

            if let Some(pending) = repo.pending_ops_for_path(&entry.repo_path)
                && pending
                    .ops
                    .iter()
                    .any(|op| op.git_status == pending_op::GitStatus::Reverted && op.finished())
            {
                continue;
            }

            let entry = GitStatusEntry {
                repo_path: entry.repo_path.clone(),
                status: entry.status,
                staging,
                diff_stat: entry.diff_stat,
            };

            if staging.has_staged() {
                staged_count += 1;
                single_staged_entry = Some(entry.clone());
            }

            if group_by_status && is_conflict {
                conflict_entries.push(entry);
            } else if group_by_status && is_new {
                new_entries.push(entry);
            } else {
                changed_entries.push(entry);
            }
        }

        if conflict_entries.is_empty() {
            if staged_count == 1
                && let Some(entry) = single_staged_entry.as_ref()
            {
                if let Some(ops) = repo.pending_ops_for_path(&entry.repo_path) {
                    if ops.staged() {
                        self.single_staged_entry = single_staged_entry;
                    }
                } else {
                    self.single_staged_entry = single_staged_entry;
                }
            } else if repo.pending_ops_summary().item_summary.staging_count == 1
                && let Some(ops) = repo.pending_ops().find(|ops| ops.staging())
            {
                self.single_staged_entry =
                    repo.status_for_path(&ops.repo_path)
                        .map(|status| GitStatusEntry {
                            repo_path: ops.repo_path.clone(),
                            status: status.status,
                            staging: StageStatus::Staged,
                            diff_stat: status.diff_stat,
                        });
            }
        }

        if conflict_entries.is_empty() && changed_entries.len() == 1 {
            self.single_tracked_entry = changed_entries.first().cloned();
        }

        let mut visible_indices = Vec::new();
        let mut section_counts: HashMap<Section, usize> = HashMap::default();
        let mut push_entry = |this: &mut Self, entry: GitListEntry, is_visible: bool| {
            if let Some(estimate) =
                this.width_estimate_for_list_entry(is_tree_view, &entry, path_style)
            {
                if estimate > max_width_estimate {
                    max_width_estimate = estimate;
                    max_width_item_index = Some(this.entries.len());
                }
            }

            if let Some(repo_path) = entry.status_entry().map(|status| status.repo_path.clone()) {
                this.entries_indices.insert(repo_path, this.entries.len());
            }

            if is_visible {
                visible_indices.push(this.entries.len());
            }

            this.entries.push(entry);
        };

        macro_rules! take_section_entries {
            () => {
                [
                    (Section::Conflict, std::mem::take(&mut conflict_entries)),
                    (Section::Tracked, std::mem::take(&mut changed_entries)),
                    (Section::New, std::mem::take(&mut new_entries)),
                ]
            };
        }

        let collapsed_sections = self.collapsed_sections.clone();

        match &mut self.view_mode {
            GitPanelViewMode::Tree(tree_state) => {
                tree_state.directory_descendants.clear();

                // This is just to get around the borrow checker
                // because push_entry mutably borrows self
                let mut tree_state = std::mem::take(tree_state);

                for (section, entries) in take_section_entries!() {
                    if entries.is_empty() {
                        continue;
                    }

                    section_counts.insert(section, entries.len());
                    let section_expanded = !collapsed_sections.contains(&section);

                    push_entry(
                        self,
                        GitListEntry::Header(GitHeaderEntry { header: section }),
                        true,
                    );

                    for (entry, is_visible) in
                        tree_state.build_tree_entries(section, entries, &mut seen_directories)
                    {
                        push_entry(self, entry, is_visible && section_expanded);
                    }
                }

                tree_state
                    .expanded_dirs
                    .retain(|key, _| seen_directories.contains(key));
                self.view_mode = GitPanelViewMode::Tree(tree_state);
            }
            GitPanelViewMode::Flat => {
                for (section, entries) in take_section_entries!() {
                    if entries.is_empty() {
                        continue;
                    }

                    section_counts.insert(section, entries.len());
                    let section_expanded = !collapsed_sections.contains(&section);

                    if section != Section::Tracked || !sort_by_path {
                        push_entry(
                            self,
                            GitListEntry::Header(GitHeaderEntry { header: section }),
                            true,
                        );
                    }

                    for entry in entries {
                        // A section whose header row was suppressed (flat +
                        // sort-by-path) can never be collapsed, so its entries
                        // must stay visible regardless of `collapsed_sections`.
                        let is_visible =
                            section_expanded || (section == Section::Tracked && sort_by_path);
                        push_entry(self, GitListEntry::Status(entry), is_visible);
                    }
                }
            }
        }

        debug_assert!(
            visible_indices.windows(2).all(|pair| pair[0] < pair[1]),
            "visible_indices must stay strictly ascending — visible_position \
             and clamp_selection_to_visible binary-search it"
        );
        self.visible_indices = visible_indices;
        self.section_counts = section_counts;
        self.max_width_item_index = max_width_item_index;

        self.update_counts(repo);

        let bulk_staging_anchor_new_index = bulk_staging
            .as_ref()
            .filter(|op| op.repo_id == repo.id)
            .and_then(|op| self.entry_by_path(&op.anchor));
        if bulk_staging_anchor_new_index == last_staged_path_prev_index
            && let Some(index) = bulk_staging_anchor_new_index
            && let Some(entry) = self.entries.get(index)
            && let Some(entry) = entry.status_entry()
            && GitPanel::stage_status_for_entry(entry, &repo)
                .as_bool()
                .unwrap_or(false)
        {
            self.bulk_staging = bulk_staging;
        }

        self.select_first_entry_if_none(window, cx);

        let suggested_commit_message = self.suggest_commit_message(cx);
        let placeholder_text = suggested_commit_message.unwrap_or("Enter commit message".into());

        self.commit_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text(&placeholder_text, window, cx)
        });

        cx.notify();
    }

    /// Position of an `entries` index inside `visible_indices`, or `None` when
    /// the entry is currently hidden (collapsed directory or section).
    /// `visible_indices` is built by walking `entries` in order, so it is
    /// strictly ascending — see the assertion where it is assigned. That makes
    /// this a binary search rather than the linear scan it looks like, which
    /// matters because every arrow keypress calls it (twice, via
    /// `scroll_to_selected_entry`) and a busy repository has thousands of rows.
    pub(super) fn visible_position(&self, entry_ix: usize) -> Option<usize> {
        self.visible_indices.binary_search(&entry_ix).ok()
    }

    /// Move the selection back onto a visible row after a collapse hid the row
    /// it pointed at. Every arrow-key handler early-returns when
    /// `visible_position` is `None`, so a selection stranded inside a collapsed
    /// section or directory freezes navigation entirely, with no panic and
    /// nothing on screen to explain it. The nearest preceding visible row is
    /// the header or directory that just swallowed the selection.
    pub(super) fn clamp_selection_to_visible(&mut self, cx: &mut Context<Self>) {
        let Some(selected_entry) = self.selected_entry else {
            return;
        };
        if self.visible_position(selected_entry).is_some() {
            return;
        }

        // Ascending, so the nearest preceding visible row is the element just
        // before the first one that is not less than the selection.
        let preceding = self
            .visible_indices
            .partition_point(|&ix| ix < selected_entry);
        self.selected_entry = preceding
            .checked_sub(1)
            .and_then(|ix| self.visible_indices.get(ix).copied())
            .or_else(|| self.visible_indices.first().copied());
        self.scroll_to_selected_entry(cx);
    }

    pub(super) fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Prefer the first selectable *content* row: the auto-selection that
        // runs when the panel first shows entries should open a diff rather
        // than park on a section header. Headers are still reachable with the
        // arrow keys.
        let first_entry = self
            .visible_indices
            .iter()
            .copied()
            .find(|&ix| !matches!(self.entries.get(ix), Some(GitListEntry::Header(..))))
            .or_else(|| self.visible_indices.first().copied());

        if let Some(first_entry) = first_entry {
            self.selected_entry = Some(first_entry);
            self.scroll_to_selected_entry(cx);
        }
    }

    pub(super) fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected_entry) = self.selected_entry else {
            return;
        };
        let Some(position) = self.visible_position(selected_entry) else {
            return;
        };
        let Some(new_index) = position
            .checked_sub(1)
            .and_then(|position| self.visible_indices.get(position).copied())
        else {
            return;
        };

        self.selected_entry = Some(new_index);
        self.scroll_to_selected_entry(cx);
    }

    pub(super) fn select_next(
        &mut self,
        _: &menu::SelectNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected_entry) = self.selected_entry else {
            return;
        };
        let Some(position) = self.visible_position(selected_entry) else {
            return;
        };
        let Some(new_index) = self.visible_indices.get(position + 1).copied() else {
            return;
        };

        self.selected_entry = Some(new_index);
        self.scroll_to_selected_entry(cx);
    }

    pub(super) fn select_last(
        &mut self,
        _: &menu::SelectLast,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&last_entry) = self.visible_indices.last() {
            self.selected_entry = Some(last_entry);
            self.scroll_to_selected_entry(cx);
        }
    }

    fn entry_label(&self, label: impl Into<SharedString>, color: Color) -> Label {
        Label::new(label.into()).color(color)
    }

    /// IDEA-style disclosure chevron for the collapsible rows (section headers
    /// and directories). Deliberately a plain `Icon` and not `ui::Disclosure`:
    /// the latter renders an `IconButton`, which is taller than a file row, and
    /// `uniform_list` sizes every row from the first one — so a taller header
    /// row would be clipped.
    fn render_row_chevron(expanded: bool) -> AnyElement {
        h_flex()
            .size(IconSize::Small.rems())
            .flex_none()
            .justify_center()
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::Small)
                .color(Color::Muted),
            )
            .into_any_element()
    }

    /// Invisible stand-in for the chevron so file rows line their checkbox,
    /// icon and label up with the directory / header rows above them.
    fn render_chevron_spacer() -> AnyElement {
        h_flex()
            .size(IconSize::Small.rems())
            .flex_none()
            .into_any_element()
    }

    pub(super) fn render_list_header(
        &self,
        ix: usize,
        header: &GitHeaderEntry,
        has_write_access: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id: ElementId = ElementId::Name(format!("header_{}", ix).into());
        let checkbox_id: ElementId = ElementId::Name(format!("header_{}_checkbox", ix).into());
        let checkbox_wrapper_id: ElementId =
            ElementId::Name(format!("header_{}_checkbox_wrapper", ix).into());
        let group_name: SharedString = format!("header_{}", ix).into();
        let toggle_state = self.header_state(header.header);
        let section = header.header;
        let expanded = !self.collapsed_sections.contains(&section);
        let count = self.section_counts.get(&section).copied().unwrap_or(0);
        let selected = self.selected_entry == Some(ix);
        let (base_bg, hover_bg, active_bg) = row_background_colors(selected, false, cx);

        h_flex()
            .id(id)
            .cursor_pointer()
            .group(group_name)
            .h(list_item_height())
            .w_full()
            .pl(header_row_padding())
            .pr_1()
            .gap_1p5()
            .border_1()
            .border_r_2()
            .when(selected && self.focus_handle.is_focused(window), |el| {
                el.border_color(cx.theme().colors().panel_focused_border)
            })
            .bg(base_bg)
            .hover(|s| s.bg(hover_bg))
            .active(|s| s.bg(active_bg))
            .child(Self::render_row_chevron(expanded))
            .child(
                div()
                    .id(checkbox_wrapper_id)
                    .flex_none()
                    .occlude()
                    .cursor_pointer()
                    .child(
                        Checkbox::new(checkbox_id, toggle_state)
                            .disabled(!has_write_access)
                            .fill()
                            .elevation(ElevationIndex::Surface)
                            .on_click({
                                let weak = cx.weak_entity();
                                move |_, window, cx| {
                                    weak.update(cx, |this, cx| {
                                        if !has_write_access {
                                            return;
                                        }
                                        this.toggle_staged_for_entry(
                                            &GitListEntry::Header(GitHeaderEntry {
                                                header: section,
                                            }),
                                            window,
                                            cx,
                                        );
                                        cx.stop_propagation();
                                    })
                                    .ok();
                                }
                            })
                            .tooltip(move |_window, cx| {
                                // The Conflicts section speaks resolution, not
                                // index state: the tick is what tells git the
                                // conflict is settled.
                                let phrase = match (section, toggle_state) {
                                    (Section::Conflict, ToggleState::Selected) => {
                                        "Mark section unresolved"
                                    }
                                    (Section::Conflict, _) => "Mark section resolved",
                                    (_, ToggleState::Selected) => "Unstage section",
                                    (_, _) => "Stage section",
                                };
                                Tooltip::simple(phrase, cx)
                            }),
                    ),
            )
            // Same size as the file names below it (`entry_label`): this is the
            // section's name, not an annotation. The "N files" count next to it
            // stays small and muted.
            .child(Label::new(header.title()))
            .child(
                Label::new(file_count_label(count))
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.selected_entry = Some(ix);
                this.toggle_section(section, window, cx);
            }))
            .into_any_element()
    }

    pub(super) fn render_status_entry(
        &self,
        ix: usize,
        entry: &GitStatusEntry,
        depth: usize,
        has_write_access: bool,
        repo: &Repository,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let settings = GitPanelSettings::get_global(cx);
        let tree_view = settings.tree_view;
        let path_style = self.project.read(cx).path_style(cx);
        let git_path_style = ProjectSettings::get_global(cx).git.path_style;
        let display_name = entry.display_name(path_style);

        let selected = self.selected_entry == Some(ix);
        // Not the same thing as `selected`: the cursor is where the keyboard
        // is, this is what the centre pane is showing. One row can be both.
        let open_in_pane = self.is_open_working_diff(repo.id, &entry.repo_path);
        let status_style = settings.status_style;
        let status = entry.status;
        let file_icon = if settings.file_icons {
            FileIcons::get_icon(entry.repo_path.as_std_path(), cx)
        } else {
            None
        };

        let has_conflict = status.is_conflicted();
        // Sticky for the whole merge, unlike `has_conflict`: a file that has
        // been marked resolved keeps its row under `Conflicts` with a tick, and
        // that tick's tooltip has to keep speaking resolution vocabulary.
        let had_conflict = repo.had_conflict_on_last_merge_head_change(&entry.repo_path);
        let is_modified = status.is_modified();
        let is_deleted = status.is_deleted();
        let is_created = status.is_created();

        let label_color = if status_style == StatusStyle::LabelColor {
            if has_conflict {
                Color::VersionControlConflict
            } else if is_created {
                Color::VersionControlUntracked
            } else if is_modified {
                Color::VersionControlModified
            } else if is_deleted {
                // We don't want a bunch of red labels in the list
                Color::Disabled
            } else {
                Color::VersionControlAdded
            }
        } else if is_created {
            // IDEA tints unversioned files in its Commit tool window; keep that
            // cue even when the panel is in "status icon" mode, where every
            // other row is plain. `is_created` is exactly the predicate behind
            // the "Untracked" section header, so it gets the dedicated
            // untracked tint; `version_control_added` would collide with the
            // green this panel already spends on "added to the index".
            Color::VersionControlUntracked
        } else {
            Color::Default
        };

        let path_color = if status.is_deleted() {
            Color::Disabled
        } else {
            Color::Muted
        };

        let id: ElementId = ElementId::Name(format!("entry_{}_{}", display_name, ix).into());
        let checkbox_wrapper_id: ElementId =
            ElementId::Name(format!("entry_{}_{}_checkbox_wrapper", display_name, ix).into());
        let checkbox_id: ElementId =
            ElementId::Name(format!("entry_{}_{}_checkbox", display_name, ix).into());

        let stage_status = GitPanel::stage_status_for_entry(entry, &repo);
        let mut is_staged: ToggleState = match stage_status {
            StageStatus::Staged => ToggleState::Selected,
            StageStatus::Unstaged => ToggleState::Unselected,
            StageStatus::PartiallyStaged => ToggleState::Indeterminate,
        };
        if self.show_placeholders && !self.has_staged_changes() && !entry.status.is_created() {
            is_staged = ToggleState::Selected;
        }

        let handle = cx.weak_entity();

        let (base_bg, hover_bg, active_bg) = row_background_colors(selected, open_in_pane, cx);

        let name_row = h_flex()
            .min_w_0()
            .flex_1()
            .gap_1()
            .when(settings.file_icons, |this| {
                this.child(
                    file_icon
                        .map(|file_icon| {
                            Icon::from_path(file_icon)
                                .size(IconSize::Small)
                                .color(Color::Muted)
                        })
                        .unwrap_or_else(|| {
                            Icon::new(IconName::File)
                                .size(IconSize::Small)
                                .color(Color::Muted)
                        }),
                )
            })
            .when(status_style != StatusStyle::LabelColor, |el| {
                el.child(git_status_icon(status))
            })
            .map(|this| {
                if tree_view {
                    this.child(
                        self.entry_label(display_name, label_color)
                            .when(open_in_pane, |label| label.weight(FontWeight::BOLD))
                            .when(status.is_deleted(), Label::strikethrough)
                            .truncate(),
                    )
                } else {
                    this.child(self.path_formatted(
                        entry.parent_dir(path_style),
                        path_color,
                        display_name,
                        label_color,
                        path_style,
                        git_path_style,
                        status.is_deleted(),
                        open_in_pane,
                    ))
                }
            });

        let id_for_diff_stat = id.clone();

        h_flex()
            .id(id)
            .h(list_item_height())
            .w_full()
            .pl(content_row_padding(depth))
            .pr_1()
            .gap_1p5()
            .border_1()
            .border_r_2()
            .when(selected && self.focus_handle.is_focused(window), |el| {
                el.border_color(cx.theme().colors().panel_focused_border)
            })
            .bg(base_bg)
            .hover(|s| s.bg(hover_bg))
            .active(|s| s.bg(active_bg))
            .child(Self::render_chevron_spacer())
            .child(
                div()
                    .id(checkbox_wrapper_id)
                    .flex_none()
                    .occlude()
                    .cursor_pointer()
                    .child(
                        Checkbox::new(checkbox_id, is_staged)
                            .disabled(!has_write_access)
                            .fill()
                            .elevation(ElevationIndex::Surface)
                            .on_click_ext({
                                let entry = entry.clone();
                                let this = cx.weak_entity();
                                move |_, click, window, cx| {
                                    this.update(cx, |this, cx| {
                                        if !has_write_access {
                                            return;
                                        }
                                        if click.modifiers().shift {
                                            this.stage_bulk(ix, cx);
                                        } else {
                                            let list_entry =
                                                if GitPanelSettings::get_global(cx).tree_view {
                                                    GitListEntry::TreeStatus(GitTreeStatusEntry {
                                                        entry: entry.clone(),
                                                        depth,
                                                    })
                                                } else {
                                                    GitListEntry::Status(entry.clone())
                                                };
                                            this.toggle_staged_for_entry(&list_entry, window, cx);
                                        }
                                        cx.stop_propagation();
                                    })
                                    .ok();
                                }
                            })
                            .tooltip(move |_window, cx| {
                                // Ticking a row under `Conflicts` is the same
                                // `ToggleStaged` action with the same binding,
                                // but the thing it means to the user is IDEA's
                                // "Mark as Resolved", not "add to the index".
                                let action = match (had_conflict, stage_status) {
                                    (true, StageStatus::Staged) => "Mark Unresolved",
                                    (true, _) => "Mark Resolved",
                                    (false, StageStatus::Staged) => "Unstage",
                                    (false, _) => "Stage",
                                };
                                let tooltip_name = action.to_string();

                                Tooltip::for_action(tooltip_name, &ToggleStaged, cx)
                            }),
                    ),
            )
            .child(name_row)
            .when(GitPanelSettings::get_global(cx).diff_stats, |el| {
                el.when_some(entry.diff_stat, move |this, stat| {
                    let id = format!("diff-stat-{}", id_for_diff_stat);
                    this.child(ui::DiffStat::new(
                        id,
                        stat.added as usize,
                        stat.deleted as usize,
                    ))
                })
            })
            .on_click({
                cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.selected_entry = Some(ix);
                    cx.notify();
                    // The gesture mapping itself lives on `GitPanel` so a test
                    // can drive it; this closure only reports what the mouse
                    // said.
                    this.handle_row_click(
                        event.modifiers().secondary(),
                        event.click_count(),
                        window,
                        cx,
                    );
                })
            })
            .on_mouse_down(
                MouseButton::Right,
                move |event: &MouseDownEvent, window, cx| {
                    // why isn't this happening automatically? we are passing MouseButton::Right to `on_mouse_down`?
                    if event.button != MouseButton::Right {
                        return;
                    }

                    let Some(this) = handle.upgrade() else {
                        return;
                    };
                    this.update(cx, |this, cx| {
                        this.deploy_entry_context_menu(event.position, ix, window, cx);
                    });
                    cx.stop_propagation();
                },
            )
            .into_any_element()
    }

    pub(super) fn render_directory_entry(
        &self,
        ix: usize,
        entry: &GitTreeDirEntry,
        has_write_access: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        // Directory rows never carry the open-diff mark: the pane shows a file.
        let selected = self.selected_entry == Some(ix);
        let label_color = Color::Muted;

        let id: ElementId = ElementId::Name(format!("dir_{}_{}", entry.name, ix).into());
        let checkbox_id: ElementId =
            ElementId::Name(format!("dir_checkbox_{}_{}", entry.name, ix).into());
        let checkbox_wrapper_id: ElementId =
            ElementId::Name(format!("dir_checkbox_wrapper_{}_{}", entry.name, ix).into());

        let (base_bg, hover_bg, active_bg) = row_background_colors(selected, false, cx);

        let settings = GitPanelSettings::get_global(cx);
        // Same lookup and the same (default) icon size the project panel uses
        // for directories, so the two trees show identical folder glyphs.
        let folder_icon = if settings.folder_icons {
            FileIcons::get_folder_icon(entry.expanded, entry.key.path.as_std_path(), cx)
        } else {
            None
        };

        let stage_status = if let Some(repo) = &self.active_repository {
            self.stage_status_for_directory(entry, repo.read(cx))
        } else {
            util::debug_panic!(
                "Won't have entries to render without an active repository in Git Panel"
            );
            StageStatus::PartiallyStaged
        };

        let toggle_state: ToggleState = match stage_status {
            StageStatus::Staged => ToggleState::Selected,
            StageStatus::Unstaged => ToggleState::Unselected,
            StageStatus::PartiallyStaged => ToggleState::Indeterminate,
        };

        let name_row = h_flex()
            .min_w_0()
            .flex_1()
            .gap_1()
            .when_some(folder_icon, |this, folder_icon| {
                this.child(Icon::from_path(folder_icon).color(Color::Muted))
            })
            .child(self.entry_label(entry.name.clone(), label_color).truncate());

        h_flex()
            .id(id)
            .h(list_item_height())
            .min_w_0()
            .w_full()
            .pl(content_row_padding(entry.depth))
            .pr_1()
            .gap_1p5()
            .border_1()
            .border_r_2()
            .when(selected && self.focus_handle.is_focused(window), |el| {
                el.border_color(cx.theme().colors().panel_focused_border)
            })
            .bg(base_bg)
            .hover(|s| s.bg(hover_bg))
            .active(|s| s.bg(active_bg))
            .child(Self::render_row_chevron(entry.expanded))
            .child(
                div()
                    .id(checkbox_wrapper_id)
                    .flex_none()
                    .occlude()
                    .cursor_pointer()
                    .child(
                        Checkbox::new(checkbox_id, toggle_state)
                            .disabled(!has_write_access)
                            .fill()
                            .elevation(ElevationIndex::Surface)
                            .on_click({
                                let entry = entry.clone();
                                let this = cx.weak_entity();
                                move |_, window, cx| {
                                    this.update(cx, |this, cx| {
                                        if !has_write_access {
                                            return;
                                        }
                                        this.toggle_staged_for_entry(
                                            &GitListEntry::Directory(entry.clone()),
                                            window,
                                            cx,
                                        );
                                        cx.stop_propagation();
                                    })
                                    .ok();
                                }
                            })
                            .tooltip(move |_window, cx| {
                                let action = match stage_status {
                                    StageStatus::Staged => "Unstage",
                                    StageStatus::Unstaged | StageStatus::PartiallyStaged => "Stage",
                                };
                                Tooltip::simple(format!("{action} folder"), cx)
                            }),
                    ),
            )
            .child(name_row)
            .on_click({
                let key = entry.key.clone();
                cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.selected_entry = Some(ix);
                    this.toggle_directory(&key, window, cx);
                })
            })
            .into_any_element()
    }

    fn path_formatted(
        &self,
        directory: Option<String>,
        path_color: Color,
        file_name: String,
        label_color: Color,
        path_style: PathStyle,
        git_path_style: GitPathStyle,
        strikethrough: bool,
        bold_file_name: bool,
    ) -> Div {
        let file_name_first = git_path_style == GitPathStyle::FileNameFirst;
        let file_path_first = git_path_style == GitPathStyle::FilePathFirst;

        let file_name = format!("{} ", file_name);

        h_flex()
            .min_w_0()
            .overflow_hidden()
            .when(file_path_first, |this| this.flex_row_reverse())
            .child(
                div().flex_none().child(
                    self.entry_label(file_name, label_color)
                        .when(bold_file_name, |label| label.weight(FontWeight::BOLD))
                        .when(strikethrough, Label::strikethrough),
                ),
            )
            .when_some(directory, |this, dir| {
                let path_name = if file_name_first {
                    dir
                } else {
                    format!("{dir}{}", path_style.primary_separator())
                };

                this.child(
                    self.entry_label(path_name, path_color)
                        .truncate_start()
                        .when(strikethrough, Label::strikethrough),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use git::{repository::repo_path, status::StatusCode};
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    use workspace::MultiWorkspace;

    use crate::git_panel::tests::init_test;

    use super::*;

    #[gpui::test]
    async fn test_tree_view_reveals_collapsed_parent_on_select_entry_by_path(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "src": {
                    "a": {
                        "foo.rs": "fn foo() {}",
                    },
                    "b": {
                        "bar.rs": "fn bar() {}",
                    },
                },
            }),
        )
        .await;

        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("src/a/foo.rs", StatusCode::Modified.worktree()),
                ("src/b/bar.rs", StatusCode::Modified.worktree()),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        cx.read(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .as_local()
                .unwrap()
                .scan_complete()
        })
        .await;

        cx.executor().run_until_parked();

        cx.update(|_window, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.git_panel.get_or_insert_default().tree_view = Some(true);
                })
            });
        });

        let panel = workspace.update_in(cx, GitPanel::new);

        let handle = cx.update_window_entity(&panel, |panel, _, _| {
            std::mem::replace(&mut panel.update_visible_entries_task, Task::ready(()))
        });
        cx.executor().advance_clock(2 * UPDATE_DEBOUNCE);
        handle.await;

        let src_key = panel.read_with(cx, |panel, _| {
            panel
                .entries
                .iter()
                .find_map(|entry| match entry {
                    GitListEntry::Directory(dir) if dir.key.path == repo_path("src") => {
                        Some(dir.key.clone())
                    }
                    _ => None,
                })
                .expect("src directory should exist in tree view")
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_directory(&src_key, window, cx);
        });

        panel.read_with(cx, |panel, _| {
            let state = panel
                .view_mode
                .tree_state()
                .expect("tree view state should exist");
            assert_eq!(state.expanded_dirs.get(&src_key).copied(), Some(false));
        });

        let worktree_id =
            cx.read(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());
        let project_path = ProjectPath {
            worktree_id,
            path: RelPath::unix("src/a/foo.rs").unwrap().into_arc(),
        };

        panel.update_in(cx, |panel, window, cx| {
            panel.select_entry_by_path(project_path, window, cx);
        });

        panel.read_with(cx, |panel, _| {
            let state = panel
                .view_mode
                .tree_state()
                .expect("tree view state should exist");
            assert_eq!(state.expanded_dirs.get(&src_key).copied(), Some(true));

            let selected_ix = panel.selected_entry.expect("selection should be set");
            assert!(panel.visible_indices.contains(&selected_ix));

            let selected_entry = panel
                .entries
                .get(selected_ix)
                .and_then(|entry| entry.status_entry())
                .expect("selected entry should be a status entry");
            assert_eq!(selected_entry.repo_path, repo_path("src/a/foo.rs"));
        });
    }

    #[gpui::test]
    async fn test_tree_view_select_next_at_last_visible_collapsed_directory(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "bar": {
                    "bar1.py": "print('bar1')",
                    "bar2.py": "print('bar2')",
                },
                "foo": {
                    "foo1.py": "print('foo1')",
                    "foo2.py": "print('foo2')",
                },
                "foobar.py": "print('foobar')",
            }),
        )
        .await;

        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("bar/bar1.py", StatusCode::Modified.worktree()),
                ("bar/bar2.py", StatusCode::Modified.worktree()),
                ("foo/foo1.py", StatusCode::Modified.worktree()),
                ("foo/foo2.py", StatusCode::Modified.worktree()),
                ("foobar.py", FileStatus::Untracked),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        cx.read(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .as_local()
                .unwrap()
                .scan_complete()
        })
        .await;

        cx.executor().run_until_parked();
        cx.update(|_window, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.git_panel.get_or_insert_default().tree_view = Some(true);
                })
            });
        });

        let panel = workspace.update_in(cx, GitPanel::new);
        let handle = cx.update_window_entity(&panel, |panel, _, _| {
            std::mem::replace(&mut panel.update_visible_entries_task, Task::ready(()))
        });

        cx.executor().advance_clock(2 * UPDATE_DEBOUNCE);
        handle.await;

        let foo_key = panel.read_with(cx, |panel, _| {
            panel
                .entries
                .iter()
                .find_map(|entry| match entry {
                    GitListEntry::Directory(dir) if dir.key.path == repo_path("foo") => {
                        Some(dir.key.clone())
                    }
                    _ => None,
                })
                .expect("foo directory should exist in tree view")
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_directory(&foo_key, window, cx);
        });

        let foo_idx = panel.read_with(cx, |panel, _| {
            let state = panel
                .view_mode
                .tree_state()
                .expect("tree view state should exist");
            assert_eq!(state.expanded_dirs.get(&foo_key).copied(), Some(false));

            let foo_idx = panel
                .entries
                .iter()
                .enumerate()
                .find_map(|(index, entry)| match entry {
                    GitListEntry::Directory(dir) if dir.key.path == repo_path("foo") => Some(index),
                    _ => None,
                })
                .expect("foo directory should exist in tree view");

            let foo_logical_idx = panel
                .visible_position(foo_idx)
                .expect("foo directory should be visible");
            let next_logical_idx = panel.visible_indices[foo_logical_idx + 1];
            assert!(matches!(
                panel.entries.get(next_logical_idx),
                Some(GitListEntry::Header(GitHeaderEntry {
                    header: Section::New
                }))
            ));

            foo_idx
        });

        // Section headers are selectable rows, so stepping off the last visible
        // entry of the tracked section lands on the `Untracked` header first.
        panel.update_in(cx, |panel, window, cx| {
            panel.selected_entry = Some(foo_idx);
            panel.select_next(&menu::SelectNext, window, cx);
        });

        panel.read_with(cx, |panel, _| {
            let selected_idx = panel.selected_entry.expect("selection should be set");
            assert!(matches!(
                panel.entries.get(selected_idx),
                Some(GitListEntry::Header(GitHeaderEntry {
                    header: Section::New
                }))
            ));
        });

        panel.update_in(cx, |panel, window, cx| {
            panel.select_next(&menu::SelectNext, window, cx);
        });

        panel.read_with(cx, |panel, _| {
            let selected_idx = panel.selected_entry.expect("selection should be set");
            let selected_entry = panel
                .entries
                .get(selected_idx)
                .and_then(|entry| entry.status_entry())
                .expect("selected entry should be a status entry");
            assert_eq!(selected_entry.repo_path, repo_path("foobar.py"));
        });
    }

    /// Renders the currently visible rows into a compact, readable shape:
    /// `[Header]`, `dir <name>` and file names, indented by tree depth.
    fn visible_rows(panel: &GitPanel) -> Vec<String> {
        panel
            .visible_indices
            .iter()
            .filter_map(|&ix| panel.entries.get(ix))
            .map(|entry| match entry {
                GitListEntry::Header(header) => format!("[{}]", header.title()),
                GitListEntry::Directory(dir) => {
                    format!("{}dir {}", "  ".repeat(dir.depth), dir.name)
                }
                GitListEntry::Status(status) => {
                    status.repo_path.display(PathStyle::Posix).to_string()
                }
                GitListEntry::TreeStatus(status) => format!(
                    "{}{}",
                    "  ".repeat(status.depth),
                    status.entry.display_name(PathStyle::Posix)
                ),
            })
            .collect()
    }

    /// Builds a panel over a repo with two tracked and two untracked files,
    /// each pair split across a directory and the repo root.
    async fn sectioned_panel(cx: &mut TestAppContext) -> (Entity<GitPanel>, VisualTestContext) {
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "src": {
                    "lib.rs": "pub fn hello() {}",
                    "main.rs": "fn main() {}",
                },
                "docs": {
                    "readme.md": "# hi",
                },
                "new.txt": "new",
            }),
        )
        .await;

        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("src/lib.rs", StatusCode::Modified.worktree()),
                ("src/main.rs", StatusCode::Modified.worktree()),
                ("docs/readme.md", FileStatus::Untracked),
                ("new.txt", FileStatus::Untracked),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut visual_cx = VisualTestContext::from_window(window_handle.into(), cx);

        visual_cx
            .read(|cx| {
                project
                    .read(cx)
                    .worktrees(cx)
                    .next()
                    .unwrap()
                    .read(cx)
                    .as_local()
                    .unwrap()
                    .scan_complete()
            })
            .await;
        visual_cx.executor().run_until_parked();

        let panel = workspace.update_in(&mut visual_cx, GitPanel::new);
        let handle = visual_cx.update_window_entity(&panel, |panel, _, _| {
            std::mem::replace(&mut panel.update_visible_entries_task, Task::ready(()))
        });
        visual_cx.executor().advance_clock(2 * UPDATE_DEBOUNCE);
        handle.await;

        (panel, visual_cx)
    }

    fn set_tree_view(cx: &mut VisualTestContext, tree_view: bool) {
        cx.update(|_window, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.git_panel.get_or_insert_default().tree_view = Some(tree_view);
                })
            });
        });
        cx.executor().run_until_parked();
    }

    #[gpui::test]
    async fn test_flat_and_tree_row_construction(cx: &mut TestAppContext) {
        init_test(cx);
        let (panel, mut cx) = sectioned_panel(cx).await;

        // Flat mode still groups into Changes / Untracked sections; every file
        // is one row, with no directory rows in between.
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                visible_rows(panel),
                vec![
                    "[Changes]",
                    "src/lib.rs",
                    "src/main.rs",
                    "[Untracked]",
                    "docs/readme.md",
                    "new.txt",
                ]
            );
            assert_eq!(panel.visible_indices.len(), panel.entries.len());
        });

        set_tree_view(&mut cx, true);

        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                visible_rows(panel),
                vec![
                    "[Changes]",
                    "dir src",
                    "  lib.rs",
                    "  main.rs",
                    "[Untracked]",
                    "dir docs",
                    "  readme.md",
                    "new.txt",
                ]
            );
        });
    }

    #[gpui::test]
    async fn test_section_header_counts(cx: &mut TestAppContext) {
        init_test(cx);
        let (panel, mut cx) = sectioned_panel(cx).await;

        // Counters count *files*, so they are the same in both view modes even
        // though the tree mode inserts extra directory rows.
        for tree_view in [false, true] {
            set_tree_view(&mut cx, tree_view);
            panel.read_with(&cx, |panel, _| {
                assert_eq!(panel.section_counts.get(&Section::Tracked), Some(&2));
                assert_eq!(panel.section_counts.get(&Section::New), Some(&2));
                assert_eq!(panel.section_counts.get(&Section::Conflict), None);
            });
        }
    }

    #[gpui::test]
    async fn test_section_headers_collapse_and_expand(cx: &mut TestAppContext) {
        init_test(cx);
        let (panel, mut cx) = sectioned_panel(cx).await;
        set_tree_view(&mut cx, true);

        let entry_count = panel.read_with(&cx, |panel, _| panel.entries.len());

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.toggle_section(Section::Tracked, window, cx);
        });

        // The collapsed section keeps its header (and its entries, so staging
        // the whole section still works) — only its rows stop being visible.
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                visible_rows(panel),
                vec![
                    "[Changes]",
                    "[Untracked]",
                    "dir docs",
                    "  readme.md",
                    "new.txt",
                ]
            );
            assert_eq!(panel.entries.len(), entry_count);
            assert_eq!(panel.section_counts.get(&Section::Tracked), Some(&2));
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.toggle_section(Section::New, window, cx);
        });

        panel.read_with(&cx, |panel, _| {
            assert_eq!(visible_rows(panel), vec!["[Changes]", "[Untracked]"]);
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.toggle_section(Section::Tracked, window, cx);
            panel.toggle_section(Section::New, window, cx);
        });

        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                visible_rows(panel),
                vec![
                    "[Changes]",
                    "dir src",
                    "  lib.rs",
                    "  main.rs",
                    "[Untracked]",
                    "dir docs",
                    "  readme.md",
                    "new.txt",
                ]
            );
            assert!(panel.collapsed_sections.is_empty());
        });
    }

    #[gpui::test]
    async fn test_collapsed_section_is_skipped_by_keyboard_navigation(cx: &mut TestAppContext) {
        init_test(cx);
        let (panel, mut cx) = sectioned_panel(cx).await;

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.toggle_section(Section::Tracked, window, cx);
            panel.select_first(&menu::SelectFirst, window, cx);
        });

        // With `Tracked` collapsed the first content row is in the next
        // section; arrow-up from there lands on the `Untracked` header rather
        // than on a hidden row.
        panel.read_with(&cx, |panel, _| {
            let selected = panel.selected_entry.expect("selection should be set");
            assert_eq!(
                panel
                    .entries
                    .get(selected)
                    .and_then(|entry| entry.status_entry())
                    .map(|entry| entry.repo_path.clone()),
                Some(repo_path("docs/readme.md"))
            );
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.select_previous(&menu::SelectPrevious, window, cx);
        });

        panel.read_with(&cx, |panel, _| {
            let selected = panel.selected_entry.expect("selection should be set");
            assert!(matches!(
                panel.entries.get(selected),
                Some(GitListEntry::Header(GitHeaderEntry {
                    header: Section::New
                }))
            ));
        });
    }

    /// The selection can sit on a row that a collapse is about to hide — the
    /// current call sites all happen to pre-point it at the toggled row, but
    /// that is a convention, not something the toggle enforces. A stranded
    /// selection makes `visible_position` return `None` forever, which silently
    /// freezes every arrow key instead of failing loudly.
    #[gpui::test]
    async fn test_collapsing_a_section_relocates_a_hidden_selection(cx: &mut TestAppContext) {
        init_test(cx);
        let (panel, mut cx) = sectioned_panel(cx).await;

        let main_rs = panel.read_with(&cx, |panel, _| {
            entry_index_for_path(panel, "src/main.rs").expect("src/main.rs should be a row")
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.selected_entry = Some(main_rs);
            panel.toggle_section(Section::Tracked, window, cx);
        });

        // The row is gone, so the selection lands on the header that swallowed
        // it rather than on a hidden index.
        panel.read_with(&cx, |panel, _| {
            let selected = panel.selected_entry.expect("selection should be set");
            assert!(matches!(
                panel.entries.get(selected),
                Some(GitListEntry::Header(GitHeaderEntry {
                    header: Section::Tracked
                }))
            ));
            assert!(panel.visible_position(selected).is_some());
        });

        // …and navigation still moves.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.select_next(&menu::SelectNext, window, cx);
        });

        panel.read_with(&cx, |panel, _| {
            let selected = panel.selected_entry.expect("selection should be set");
            assert!(matches!(
                panel.entries.get(selected),
                Some(GitListEntry::Header(GitHeaderEntry {
                    header: Section::New
                }))
            ));
        });
    }

    #[gpui::test]
    async fn test_collapsing_a_directory_relocates_a_hidden_selection(cx: &mut TestAppContext) {
        init_test(cx);
        let (panel, mut cx) = sectioned_panel(cx).await;
        set_tree_view(&mut cx, true);

        let (src_key, main_rs) = panel.read_with(&cx, |panel, _| {
            let src_key = panel
                .entries
                .iter()
                .find_map(|entry| match entry {
                    GitListEntry::Directory(dir) if dir.key.path == repo_path("src") => {
                        Some(dir.key.clone())
                    }
                    _ => None,
                })
                .expect("src directory should be a row");
            let main_rs =
                entry_index_for_path(panel, "src/main.rs").expect("src/main.rs should be a row");
            (src_key, main_rs)
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.selected_entry = Some(main_rs);
            panel.toggle_directory(&src_key, window, cx);
        });

        panel.read_with(&cx, |panel, _| {
            let selected = panel.selected_entry.expect("selection should be set");
            assert!(matches!(
                panel.entries.get(selected),
                Some(GitListEntry::Directory(dir)) if dir.key == src_key
            ));
            assert!(panel.visible_position(selected).is_some());
        });

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.select_next(&menu::SelectNext, window, cx);
        });

        panel.read_with(&cx, |panel, _| {
            let selected = panel.selected_entry.expect("selection should be set");
            assert!(matches!(
                panel.entries.get(selected),
                Some(GitListEntry::Header(GitHeaderEntry {
                    header: Section::New
                }))
            ));
        });
    }

    /// Each visible row paired with the left padding it is rendered with, in
    /// pixels. Calls the very functions the row renderers call, so the
    /// assertions below track the real layout instead of a copy of it.
    fn visible_row_paddings(panel: &GitPanel) -> Vec<(String, f32)> {
        let entries = panel
            .visible_indices
            .iter()
            .filter_map(|&ix| panel.entries.get(ix));
        visible_rows(panel)
            .into_iter()
            .zip(entries)
            .map(|(label, entry)| {
                let padding = match entry {
                    GitListEntry::Header(_) => header_row_padding(),
                    _ => content_row_padding(entry.depth()),
                };
                (label, f32::from(padding))
            })
            .collect()
    }

    #[gpui::test]
    async fn test_section_content_is_indented_under_its_header(cx: &mut TestAppContext) {
        init_test(cx);
        let (panel, mut cx) = sectioned_panel(cx).await;

        let header = ROW_LEFT_PADDING;
        let depth_0 = ROW_LEFT_PADDING + SECTION_CONTENT_INDENT;
        let depth_1 = depth_0 + TREE_INDENT;
        assert!(
            depth_0 > header,
            "section content must step in from its header"
        );

        // Flat mode has no directory rows, but its files are still section
        // content and still step in.
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                visible_row_paddings(panel),
                vec![
                    ("[Changes]".to_string(), header),
                    ("src/lib.rs".to_string(), depth_0),
                    ("src/main.rs".to_string(), depth_0),
                    ("[Untracked]".to_string(), header),
                    ("docs/readme.md".to_string(), depth_0),
                    ("new.txt".to_string(), depth_0),
                ]
            );
        });

        set_tree_view(&mut cx, true);

        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                visible_row_paddings(panel),
                vec![
                    ("[Changes]".to_string(), header),
                    ("dir src".to_string(), depth_0),
                    ("  lib.rs".to_string(), depth_1),
                    ("  main.rs".to_string(), depth_1),
                    ("[Untracked]".to_string(), header),
                    ("dir docs".to_string(), depth_0),
                    ("  readme.md".to_string(), depth_1),
                    ("new.txt".to_string(), depth_0),
                ]
            );
        });

        // The depth-0 indent guide is drawn in the list's coordinate space, so
        // it has to land on the chevron column of the depth-0 *content* rows
        // (7px into the 14px chevron), not on the header's.
        assert_eq!(INDENT_GUIDE_LEFT_OFFSET, depth_0 + 7.0);
    }

    /// Index into `entries` of the row for `path`, whether it is a flat or a
    /// tree status row.
    fn entry_index_for_path(panel: &GitPanel, path: &str) -> Option<usize> {
        let path = repo_path(path);
        panel.entries.iter().position(|entry| {
            entry
                .status_entry()
                .is_some_and(|status| status.repo_path == path)
        })
    }
}
