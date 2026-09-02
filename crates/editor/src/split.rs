use std::{
    ops::{Range, RangeInclusive},
    sync::Arc,
};

use buffer_diff::{BufferDiff, BufferDiffSnapshot};
use collections::{HashMap, HashSet};

use gpui::{
    Action, AppContext as _, Entity, EventEmitter, Focusable, Font, Pixels, SharedString,
    Subscription, WeakEntity, canvas,
};
use itertools::Itertools;
use language::{Buffer, Capability, HighlightedText};
use multi_buffer::{
    Anchor, AnchorRangeExt as _, BufferOffset, ExcerptRange, ExpandExcerptDirection, MultiBuffer,
    MultiBufferDiffHunk, MultiBufferPoint, MultiBufferSnapshot, PathKey,
};
use project::Project;
use rope::Point;
use settings::{DiffViewStyle, SeedQuerySetting, Settings, SettingsStore};
use text::{Bias, BufferId, OffsetRangeExt as _, Patch, ToPoint as _};
use ui::{
    App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _, Render,
    Styled as _, Window, div,
};

use crate::{
    display_map::CompanionExcerptPatch,
    element::SplitSide,
    split_editor_view::{SplitEditorState, SplitEditorView},
};
use workspace::{
    ActivatePaneLeft, ActivatePaneRight, Item, ToolbarItemLocation, Workspace,
    item::{ItemBufferKind, ItemEvent, SaveOptions, TabContentParams},
    searchable::{SearchEvent, SearchToken, SearchableItem, SearchableItemHandle},
};

use crate::{
    Autoscroll, Editor, EditorEvent, EditorSettings, RenderDiffHunkControlsFn, ToggleSoftWrap,
    actions::{DisableBreakpoint, EditLogBreakpoint, EnableBreakpoint, ToggleBreakpoint},
    display_map::Companion,
    git::blame::BlameBaseSource,
};
use zed_actions::assistant::InlineAssist;

pub(crate) fn patches_for_lhs_range(
    rhs_snapshot: &MultiBufferSnapshot,
    lhs_snapshot: &MultiBufferSnapshot,
    lhs_bounds: Range<MultiBufferPoint>,
) -> Vec<CompanionExcerptPatch> {
    patches_for_range(
        lhs_snapshot,
        rhs_snapshot,
        lhs_bounds,
        |diff, range, buffer| diff.patch_for_base_text_range(range, buffer),
    )
}

pub(crate) fn patches_for_rhs_range(
    lhs_snapshot: &MultiBufferSnapshot,
    rhs_snapshot: &MultiBufferSnapshot,
    rhs_bounds: Range<MultiBufferPoint>,
) -> Vec<CompanionExcerptPatch> {
    patches_for_range(
        rhs_snapshot,
        lhs_snapshot,
        rhs_bounds,
        |diff, range, buffer| diff.patch_for_buffer_range(range, buffer),
    )
}

fn buffer_range_to_base_text_range(
    rhs_range: &Range<Point>,
    diff_snapshot: &BufferDiffSnapshot,
    rhs_buffer_snapshot: &text::BufferSnapshot,
) -> Range<Point> {
    let start = diff_snapshot
        .buffer_point_to_base_text_range(Point::new(rhs_range.start.row, 0), rhs_buffer_snapshot)
        .start;
    let end = diff_snapshot
        .buffer_point_to_base_text_range(Point::new(rhs_range.end.row, 0), rhs_buffer_snapshot)
        .end;
    let end_column = diff_snapshot.base_text().line_len(end.row);
    Point::new(start.row, 0)..Point::new(end.row, end_column)
}

fn translate_lhs_selections_to_rhs(
    selections_by_buffer: &HashMap<BufferId, (Vec<Range<BufferOffset>>, Option<u32>)>,
    splittable: &SplittableEditor,
    cx: &App,
) -> HashMap<Entity<Buffer>, (Vec<Range<BufferOffset>>, Option<u32>)> {
    let Some(lhs) = &splittable.lhs else {
        return HashMap::default();
    };
    let lhs_snapshot = lhs.multibuffer.read(cx).snapshot(cx);

    let mut translated: HashMap<Entity<Buffer>, (Vec<Range<BufferOffset>>, Option<u32>)> =
        HashMap::default();

    for (lhs_buffer_id, (ranges, scroll_offset)) in selections_by_buffer {
        let Some(diff) = lhs_snapshot.diff_for_buffer_id(*lhs_buffer_id) else {
            continue;
        };
        let rhs_buffer_id = diff.buffer_id();

        let Some(rhs_buffer) = splittable
            .rhs_editor
            .read(cx)
            .buffer()
            .read(cx)
            .buffer(rhs_buffer_id)
        else {
            continue;
        };

        let Some(diff) = splittable
            .rhs_editor
            .read(cx)
            .buffer()
            .read(cx)
            .diff_for(rhs_buffer_id)
        else {
            continue;
        };

        let diff_snapshot = diff.read(cx).snapshot(cx);
        let rhs_buffer_snapshot = rhs_buffer.read(cx).snapshot();
        let base_text_buffer = diff.read(cx).base_text_buffer();
        let base_text_snapshot = base_text_buffer.read(cx).snapshot();

        let translated_ranges: Vec<Range<BufferOffset>> = ranges
            .iter()
            .map(|range| {
                let start_point = base_text_snapshot.offset_to_point(range.start.0);
                let end_point = base_text_snapshot.offset_to_point(range.end.0);

                let rhs_start = diff_snapshot
                    .base_text_point_to_buffer_point(start_point, &rhs_buffer_snapshot);
                let rhs_end =
                    diff_snapshot.base_text_point_to_buffer_point(end_point, &rhs_buffer_snapshot);

                BufferOffset(rhs_buffer_snapshot.point_to_offset(rhs_start))
                    ..BufferOffset(rhs_buffer_snapshot.point_to_offset(rhs_end))
            })
            .collect();

        translated.insert(rhs_buffer, (translated_ranges, *scroll_offset));
    }

    translated
}

fn translate_lhs_hunks_to_rhs(
    lhs_hunks: &[MultiBufferDiffHunk],
    splittable: &SplittableEditor,
    cx: &App,
) -> Vec<MultiBufferDiffHunk> {
    let Some(lhs) = &splittable.lhs else {
        return vec![];
    };
    let lhs_snapshot = lhs.multibuffer.read(cx).snapshot(cx);
    let rhs_snapshot = splittable.rhs_multibuffer.read(cx).snapshot(cx);
    let rhs_hunks: Vec<MultiBufferDiffHunk> = rhs_snapshot.diff_hunks().collect();

    let mut translated = Vec::new();
    for lhs_hunk in lhs_hunks {
        let Some(diff) = lhs_snapshot.diff_for_buffer_id(lhs_hunk.buffer_id) else {
            continue;
        };
        let rhs_buffer_id = diff.buffer_id();
        if let Some(rhs_hunk) = rhs_hunks.iter().find(|rhs_hunk| {
            rhs_hunk.buffer_id == rhs_buffer_id
                && rhs_hunk.diff_base_byte_range == lhs_hunk.diff_base_byte_range
        }) {
            translated.push(rhs_hunk.clone());
        }
    }
    translated
}

fn patches_for_range<F>(
    source_snapshot: &MultiBufferSnapshot,
    target_snapshot: &MultiBufferSnapshot,
    source_bounds: Range<MultiBufferPoint>,
    translate_fn: F,
) -> Vec<CompanionExcerptPatch>
where
    F: Fn(&BufferDiffSnapshot, RangeInclusive<Point>, &text::BufferSnapshot) -> Patch<Point>,
{
    struct PendingExcerpt {
        source_buffer_snapshot: language::BufferSnapshot,
        source_excerpt_range: ExcerptRange<text::Anchor>,
        buffer_point_range: Range<Point>,
    }

    let mut result = Vec::new();
    let mut current_buffer_id: Option<BufferId> = None;
    let mut pending_excerpts: Vec<PendingExcerpt> = Vec::new();
    let mut union_context_start: Option<Point> = None;
    let mut union_context_end: Option<Point> = None;

    let flush_buffer = |pending: &mut Vec<PendingExcerpt>,
                        union_start: Point,
                        union_end: Point,
                        result: &mut Vec<CompanionExcerptPatch>| {
        let Some(first) = pending.first() else {
            return;
        };

        let source_buffer_id = first.source_buffer_snapshot.remote_id();
        let Some(diff) = source_snapshot.diff_for_buffer_id(source_buffer_id) else {
            pending.clear();
            return;
        };
        let source_is_lhs = source_buffer_id == diff.base_text().remote_id();
        let target_buffer_id = if source_is_lhs {
            diff.buffer_id()
        } else {
            diff.base_text().remote_id()
        };
        let Some(target_buffer) = target_snapshot.buffer_for_id(target_buffer_id) else {
            pending.clear();
            return;
        };
        let rhs_buffer = if source_is_lhs {
            target_buffer
        } else {
            &first.source_buffer_snapshot
        };

        let patch = translate_fn(diff, union_start..=union_end, rhs_buffer);

        let mut source_excerpts = source_snapshot
            .excerpts_for_buffer(source_buffer_id)
            .peekable();
        let mut target_excerpts = target_snapshot
            .excerpts_for_buffer(target_buffer_id)
            .peekable();

        for excerpt in pending.drain(..) {
            while let Some(source_excerpt_range) = source_excerpts.peek()
                && source_excerpt_range != &excerpt.source_excerpt_range
            {
                source_excerpts.next();
                target_excerpts.next();
            }
            if let Some(source_excerpt_range) = source_excerpts.peek()
                && let Some(target_excerpt_range) = target_excerpts.peek()
            {
                result.push(patch_for_excerpt(
                    source_snapshot,
                    target_snapshot,
                    &excerpt.source_buffer_snapshot,
                    target_buffer,
                    source_excerpt_range.clone(),
                    target_excerpt_range.clone(),
                    &patch,
                    excerpt.buffer_point_range,
                ));
            }
        }
    };

    for (buffer_snapshot, source_range, source_excerpt_range) in
        source_snapshot.range_to_buffer_ranges(source_bounds)
    {
        let buffer_id = buffer_snapshot.remote_id();

        if current_buffer_id != Some(buffer_id) {
            if let (Some(start), Some(end)) = (union_context_start.take(), union_context_end.take())
            {
                flush_buffer(&mut pending_excerpts, start, end, &mut result);
            }
            current_buffer_id = Some(buffer_id);
        }

        let buffer_point_range = source_range.to_point(&buffer_snapshot);
        let source_context_range = source_excerpt_range.context.to_point(&buffer_snapshot);

        union_context_start = Some(union_context_start.map_or(source_context_range.start, |s| {
            s.min(source_context_range.start)
        }));
        union_context_end = Some(union_context_end.map_or(source_context_range.end, |e| {
            e.max(source_context_range.end)
        }));

        pending_excerpts.push(PendingExcerpt {
            source_buffer_snapshot: buffer_snapshot,
            source_excerpt_range,
            buffer_point_range,
        });
    }

    if let (Some(start), Some(end)) = (union_context_start, union_context_end) {
        flush_buffer(&mut pending_excerpts, start, end, &mut result);
    }

    result
}

fn patch_for_excerpt(
    source_snapshot: &MultiBufferSnapshot,
    target_snapshot: &MultiBufferSnapshot,
    source_buffer_snapshot: &language::BufferSnapshot,
    target_buffer_snapshot: &language::BufferSnapshot,
    source_excerpt_range: ExcerptRange<text::Anchor>,
    target_excerpt_range: ExcerptRange<text::Anchor>,
    patch: &Patch<Point>,
    source_edited_range: Range<Point>,
) -> CompanionExcerptPatch {
    let source_buffer_range = source_excerpt_range
        .context
        .to_point(source_buffer_snapshot);
    let source_multibuffer_range = (source_snapshot
        .anchor_in_buffer(source_excerpt_range.context.start)
        .expect("buffer should exist in multibuffer")
        ..source_snapshot
            .anchor_in_buffer(source_excerpt_range.context.end)
            .expect("buffer should exist in multibuffer"))
        .to_point(source_snapshot);
    let target_buffer_range = target_excerpt_range
        .context
        .to_point(target_buffer_snapshot);
    let target_multibuffer_range = (target_snapshot
        .anchor_in_buffer(target_excerpt_range.context.start)
        .expect("buffer should exist in multibuffer")
        ..target_snapshot
            .anchor_in_buffer(target_excerpt_range.context.end)
            .expect("buffer should exist in multibuffer"))
        .to_point(target_snapshot);

    let edits = patch
        .edits()
        .iter()
        .skip_while(|edit| edit.old.end < source_buffer_range.start)
        .take_while(|edit| edit.old.start <= source_buffer_range.end)
        .map(|edit| {
            let clamped_source_start = edit.old.start.max(source_buffer_range.start);
            let clamped_source_end = edit.old.end.min(source_buffer_range.end);
            let source_multibuffer_start =
                source_multibuffer_range.start + (clamped_source_start - source_buffer_range.start);
            let source_multibuffer_end =
                source_multibuffer_range.start + (clamped_source_end - source_buffer_range.start);
            let clamped_target_start = edit
                .new
                .start
                .max(target_buffer_range.start)
                .min(target_buffer_range.end);
            let clamped_target_end = edit
                .new
                .end
                .max(target_buffer_range.start)
                .min(target_buffer_range.end);
            let target_multibuffer_start =
                target_multibuffer_range.start + (clamped_target_start - target_buffer_range.start);
            let target_multibuffer_end =
                target_multibuffer_range.start + (clamped_target_end - target_buffer_range.start);
            text::Edit {
                old: source_multibuffer_start..source_multibuffer_end,
                new: target_multibuffer_start..target_multibuffer_end,
            }
        });

    let edits = [text::Edit {
        old: source_multibuffer_range.start..source_multibuffer_range.start,
        new: target_multibuffer_range.start..target_multibuffer_range.start,
    }]
    .into_iter()
    .chain(edits);

    let mut merged_edits: Vec<text::Edit<Point>> = Vec::new();
    for edit in edits {
        if let Some(last) = merged_edits.last_mut() {
            if edit.new.start <= last.new.end || edit.old.start <= last.old.end {
                last.old.end = last.old.end.max(edit.old.end);
                last.new.end = last.new.end.max(edit.new.end);
                continue;
            }
        }
        merged_edits.push(edit);
    }

    let edited_range = source_multibuffer_range.start
        + (source_edited_range.start - source_buffer_range.start)
        ..source_multibuffer_range.start + (source_edited_range.end - source_buffer_range.start);

    let source_excerpt_end =
        source_multibuffer_range.start + (source_buffer_range.end - source_buffer_range.start);
    let target_excerpt_end =
        target_multibuffer_range.start + (target_buffer_range.end - target_buffer_range.start);

    CompanionExcerptPatch {
        patch: Patch::new(merged_edits),
        edited_range,
        source_excerpt_range: source_multibuffer_range.start..source_excerpt_end,
        target_excerpt_range: target_multibuffer_range.start..target_excerpt_end,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Action, Default)]
#[action(namespace = editor)]
pub struct ToggleSplitDiff;

pub struct SplittableEditor {
    rhs_multibuffer: Entity<MultiBuffer>,
    rhs_editor: Entity<Editor>,
    lhs: Option<LhsEditor>,
    workspace: WeakEntity<Workspace>,
    split_state: Entity<SplitEditorState>,
    searched_side: Option<SplitSide>,
    /// The preferred diff style.
    diff_view_style: DiffViewStyle,
    /// True when the current width is below the minimum threshold for split
    /// mode, regardless of the current diff view style setting.
    too_narrow_for_split: bool,
    last_width: Option<Pixels>,
    lhs_blame_base: Option<LhsBlameBase>,
    /// Set by [`Self::disable_diff_hunk_controls`]. The renderer it installs
    /// is an opaque `Arc<dyn Fn>` that a caller cannot ask anything about, so
    /// the decision is recorded here for anyone who needs to know whether this
    /// diff offers stage / restore buttons at all.
    diff_hunk_controls_disabled: bool,
    style_controls_painted_by_consumer: bool,
    _subscriptions: Vec<Subscription>,
}

/// The git revision the left-hand pane's text was taken from, so `git::Blame`
/// can annotate it correctly. Anything `git blame` accepts, e.g. `HEAD`.
type LhsBlameBase = SharedString;

struct LhsEditor {
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    was_last_focused: bool,
    _subscriptions: Vec<Subscription>,
}

impl SplittableEditor {
    pub fn rhs_editor(&self) -> &Entity<Editor> {
        &self.rhs_editor
    }

    pub fn lhs_editor(&self) -> Option<&Entity<Editor>> {
        self.lhs.as_ref().map(|s| &s.editor)
    }

    pub fn update_editors(
        &self,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Editor, &mut Context<Editor>),
    ) {
        if let Some(lhs) = &self.lhs {
            lhs.editor.update(cx, &f);
        }
        self.rhs_editor.update(cx, &f);
    }

    pub fn diff_view_style(&self) -> DiffViewStyle {
        self.diff_view_style
    }

    pub fn is_split(&self) -> bool {
        self.lhs.is_some()
    }

    /// Both sides are kept in sync by [`Self::toggle_soft_wrap`], and the rhs
    /// editor is the side a freshly split lhs copies its state from.
    pub fn is_soft_wrap_enabled(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).is_soft_wrap_enabled(cx)
    }

    pub fn set_render_diff_hunk_controls(
        &mut self,
        render_diff_hunk_controls: RenderDiffHunkControlsFn,
        cx: &mut Context<Self>,
    ) {
        self.update_editors(cx, |editor, cx| {
            editor.set_render_diff_hunk_controls(render_diff_hunk_controls.clone(), cx);
        });
        // Installing a real renderer undoes `disable_diff_hunk_controls`, so
        // the flag has to come back down with it: a view that disabled the
        // controls at construction and installed its own later would otherwise
        // keep reporting them as gone while painting them.
        self.diff_hunk_controls_disabled = false;
    }

    pub fn disable_diff_hunk_controls(&mut self, cx: &mut Context<Self>) {
        let empty_controls = Arc::new(|_, _: &_, _, _, _, _: &_, _: &mut _, _: &mut _| {
            gpui::Empty.into_any_element()
        });
        self.update_editors(cx, |editor, cx| {
            editor.set_render_diff_hunk_controls(empty_controls.clone(), cx);
        });
        self.diff_hunk_controls_disabled = true;
    }

    /// Tells this editor that whoever owns it paints the diff style controls
    /// — hunk navigation and Unified/Split — in a toolbar of its own.
    ///
    /// `BufferSearchBar` paints that quartet for every `SplittableEditor`
    /// consumer, because most of them (`ProjectDiff`, `CommitView`,
    /// `text_diff_view`, `agent_diff`) have no toolbar of their own to put it
    /// in. A consumer that *does* have one must set this, or the two surfaces
    /// both land in `PrimaryLeft` and the user sees two identical pairs of
    /// hunk arrows and two Unified/Split toggles side by side. Which surface
    /// paints them is a fact about the consumer, not about the multibuffer's
    /// shape, so it has to be said here rather than guessed at in `search`
    /// (which cannot depend on the crates that own these views anyway).
    pub fn set_style_controls_painted_by_consumer(&mut self, painted_by_consumer: bool) {
        self.style_controls_painted_by_consumer = painted_by_consumer;
    }

    /// Whether some other surface already paints this diff's hunk navigation
    /// and Unified/Split toggle. See
    /// [`Self::set_style_controls_painted_by_consumer`].
    pub fn style_controls_painted_by_consumer(&self) -> bool {
        self.style_controls_painted_by_consumer
    }

    /// Whether [`Self::disable_diff_hunk_controls`] is the last thing to have
    /// set this diff's hunk-control renderer — i.e. whether its hunks offer
    /// stage / restore buttons at all right now. Installing a renderer through
    /// [`Self::set_render_diff_hunk_controls`] puts it back to `false`.
    pub fn diff_hunk_controls_disabled(&self) -> bool {
        self.diff_hunk_controls_disabled
    }

    pub fn set_render_diff_hunks_as_unstaged(&self, cx: &mut Context<Self>) {
        self.update_editors(cx, |editor, cx| {
            editor.set_render_diff_hunks_as_unstaged(true, cx);
        });
    }

    fn focused_side(&self) -> SplitSide {
        if let Some(lhs) = &self.lhs
            && lhs.was_last_focused
        {
            SplitSide::Left
        } else {
            SplitSide::Right
        }
    }

    pub fn focused_editor(&self) -> &Entity<Editor> {
        if let Some(lhs) = &self.lhs
            && lhs.was_last_focused
        {
            &lhs.editor
        } else {
            &self.rhs_editor
        }
    }

    pub fn new(
        style: DiffViewStyle,
        rhs_multibuffer: Entity<MultiBuffer>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rhs_editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(rhs_multibuffer.clone(), Some(project.clone()), window, cx);
            editor.set_expand_all_diff_hunks(cx);
            editor.disable_runnables();
            editor.disable_code_lens(cx);
            editor.disable_inline_diagnostics();
            editor.disable_mouse_wheel_zoom();
            editor.set_minimap_visibility(crate::MinimapVisibility::Disabled, window, cx);
            editor.start_temporary_diff_override();
            // Diff views are for reviewing changes — breakpoint/bookmark
            // gutter affordances (and the hover "add breakpoint" dot) are
            // out of place in every SplittableEditor consumer.
            editor.set_show_breakpoints(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor
        });
        // TODO(split-diff) we might want to tag editor events with whether they came from rhs/lhs
        let subscriptions = vec![
            cx.subscribe(
                &rhs_editor,
                |this, _, event: &EditorEvent, cx| match event {
                    EditorEvent::ExpandExcerptsRequested {
                        excerpt_anchors,
                        lines,
                        direction,
                    } => {
                        this.expand_excerpts(
                            excerpt_anchors.iter().copied(),
                            *lines,
                            *direction,
                            cx,
                        );
                    }
                    _ => cx.emit(event.clone()),
                },
            ),
            cx.subscribe(&rhs_editor, |this, _, event: &SearchEvent, cx| {
                if this.searched_side.is_none() || this.searched_side == Some(SplitSide::Right) {
                    cx.emit(event.clone());
                }
            }),
            cx.observe_global_in::<SettingsStore>(window, move |this, window, cx| {
                let diff_view_style = EditorSettings::get_global(cx).diff_view_style;
                if this.diff_view_style() != diff_view_style {
                    this.toggle_split(&ToggleSplitDiff, window, cx);
                    cx.notify();
                }
            }),
        ];

        let this = cx.weak_entity();
        window.defer(cx, {
            let workspace = workspace.downgrade();
            let rhs_editor = rhs_editor.downgrade();
            move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        rhs_editor
                            .update(cx, |editor, cx| {
                                editor.added_to_workspace(workspace, window, cx);
                            })
                            .ok();
                    })
                    .ok();
                if style == DiffViewStyle::Split {
                    this.update(cx, |this, cx| {
                        this.split(window, cx);
                    })
                    .ok();
                }
            }
        });
        let split_state = cx.new(|cx| SplitEditorState::new(cx));
        Self {
            diff_view_style: style,
            rhs_editor,
            rhs_multibuffer,
            lhs: None,
            workspace: workspace.downgrade(),
            split_state,
            searched_side: None,
            too_narrow_for_split: false,
            last_width: None,
            lhs_blame_base: None,
            diff_hunk_controls_disabled: false,
            style_controls_painted_by_consumer: false,
            _subscriptions: subscriptions,
        }
    }

    /// Opts this diff into git blame on the left-hand pane, whose text is the
    /// content of each file at `revision`.
    ///
    /// Consumers must opt in: `SplittableEditor` is also used to compare two
    /// unrelated files (`file_diff_view`, `text_diff_view`) and agent-produced
    /// snapshots (`agent_diff`), where the left pane's text is not any
    /// revision of the right pane's path and blaming it would be misleading.
    /// Passing `None` turns left-pane blame back off, e.g. when a view
    /// switches to a diff base that has no single commit-ish.
    pub fn set_lhs_blame_base(&mut self, revision: Option<SharedString>, cx: &mut Context<Self>) {
        self.lhs_blame_base = revision;
        let paths = self.diff_paths(cx);
        self.sync_lhs_blame_sources(&paths, cx);
    }

    /// The revision the left-hand pane is being blamed against, if any. A
    /// consumer's blame base is derived from what it is showing, so this is
    /// how a test pins that derivation without reaching into blame itself.
    pub fn lhs_blame_base(&self) -> Option<&SharedString> {
        self.lhs_blame_base.as_ref()
    }

    /// Tells the left-hand editor how to blame each detached base-text buffer.
    /// The repository and repo-relative path come from the right-hand buffer,
    /// which for an uncommitted diff is a real project file.
    fn sync_lhs_blame_sources(
        &self,
        paths: &[(PathKey, Entity<BufferDiff>)],
        cx: &mut Context<Self>,
    ) {
        let Some(lhs) = &self.lhs else { return };
        let (Some(revision), Some(project)) = (
            self.lhs_blame_base.clone(),
            self.rhs_editor.read(cx).project().cloned(),
        ) else {
            if !lhs.editor.read(cx).blame_base_sources().is_empty() {
                lhs.editor.update(cx, |editor, cx| {
                    editor.set_blame_base_sources(HashMap::default(), cx);
                });
            }
            return;
        };

        let mut sources = lhs.editor.read(cx).blame_base_sources().clone();
        for (_, diff) in paths {
            let diff = diff.read(cx);
            let base_buffer_id = diff.base_text_buffer().read(cx).remote_id();
            let rhs_buffer_id = diff.buffer_id;
            let Some((repository, repo_path)) = project
                .read(cx)
                .git_store()
                .read(cx)
                .repository_and_path_for_buffer_id(rhs_buffer_id, cx)
            else {
                sources.remove(&base_buffer_id);
                continue;
            };
            sources.insert(
                base_buffer_id,
                BlameBaseSource {
                    repository,
                    repo_path,
                    revision: revision.clone(),
                },
            );
        }

        let live_buffer_ids = lhs
            .multibuffer
            .read(cx)
            .snapshot(cx)
            .all_buffer_ids()
            .collect::<HashSet<_>>();
        sources.retain(|buffer_id, _| live_buffer_ids.contains(buffer_id));

        lhs.editor.update(cx, |editor, cx| {
            editor.set_blame_base_sources(sources, cx);
        });
    }

    pub fn split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lhs.is_some() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let all_paths = self.diff_paths(cx);
        if all_paths.is_empty() && !self.rhs_multibuffer.read(cx).is_empty() {
            return;
        }

        // The two panes only line up if they reserve the same block rows, and a
        // buffer header is `FILE_HEADER_HEIGHT` (two) rows tall. Whether the
        // right pane draws one is a property of the multibuffer the consumer
        // handed us — `MultiBufferSnapshot::show_headers` — and *not* of it
        // being a singleton. `MultiBuffer::without_headers` is used for
        // non-singleton multibuffers too (a single-file commit diff, an agent's
        // edit review), and keying off `is_singleton` gave every one of those a
        // header-bearing left pane against a header-less right one: the whole
        // left pane, gutter and text together, painted two rows low, and the
        // connector ribbons ramped across the difference because they believed
        // it. Mirroring `show_headers` subsumes the singleton case, which is
        // header-less by construction.
        let rhs_shows_headers = self.rhs_multibuffer.read(cx).snapshot(cx).show_headers();
        let lhs_multibuffer = cx.new(|cx| {
            let mut multibuffer = if rhs_shows_headers {
                MultiBuffer::new(Capability::ReadOnly)
            } else {
                MultiBuffer::without_headers(Capability::ReadOnly)
            };
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });

        let render_diff_hunk_controls = self.rhs_editor.read(cx).render_diff_hunk_controls.clone();
        let render_diff_hunks_as_unstaged = self.rhs_editor.read(cx).render_diff_hunks_as_unstaged;
        let lhs_editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(lhs_multibuffer.clone(), Some(project.clone()), window, cx);
            editor.set_render_diff_hunks_as_unstaged(render_diff_hunks_as_unstaged, cx);
            editor.set_number_deleted_lines(true, cx);
            editor.set_delegate_expand_excerpts(true);
            editor.set_delegate_stage_and_restore(true);
            editor.set_delegate_open_excerpts(true);
            editor.set_show_vertical_scrollbar(false, cx);
            editor.disable_lsp_data();
            editor.disable_runnables();
            editor.disable_diagnostics(cx);
            editor.disable_mouse_wheel_zoom();
            editor.set_minimap_visibility(crate::MinimapVisibility::Disabled, window, cx);
            // Same reasoning as the right pane in `Self::new`: breakpoints and
            // bookmarks are meaningless in a review surface, and left on they
            // fill the gutter context menu with dead entries.
            editor.set_show_breakpoints(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor.set_split_side(Some(SplitSide::Left), cx);
            editor
        });

        lhs_editor.update(cx, |editor, cx| {
            editor.set_render_diff_hunk_controls(render_diff_hunk_controls, cx);
        });

        let mut subscriptions = vec![cx.subscribe_in(
            &lhs_editor,
            window,
            |this, _, event: &EditorEvent, window, cx| match event {
                EditorEvent::ExpandExcerptsRequested {
                    excerpt_anchors,
                    lines,
                    direction,
                } => {
                    if let Some(lhs) = &this.lhs {
                        let rhs_snapshot = this.rhs_multibuffer.read(cx).snapshot(cx);
                        let lhs_snapshot = lhs.multibuffer.read(cx).snapshot(cx);
                        let rhs_anchors = excerpt_anchors
                            .iter()
                            .filter_map(|anchor| {
                                let (anchor, lhs_buffer) =
                                    lhs_snapshot.anchor_to_buffer_anchor(*anchor)?;
                                let diff = lhs_snapshot.diff_for_buffer_id(anchor.buffer_id)?;
                                let rhs_buffer_id = diff.buffer_id();
                                let rhs_buffer = rhs_snapshot.buffer_for_id(rhs_buffer_id)?;
                                let rhs_point = diff.base_text_point_to_buffer_point(
                                    anchor.to_point(&lhs_buffer),
                                    &rhs_buffer,
                                );
                                rhs_snapshot.anchor_in_excerpt(rhs_buffer.anchor_before(rhs_point))
                            })
                            .collect::<Vec<_>>();
                        this.expand_excerpts(rhs_anchors.into_iter(), *lines, *direction, cx);
                    }
                }
                EditorEvent::StageOrUnstageRequested { stage, hunks } => {
                    if this.lhs.is_some() {
                        let translated = translate_lhs_hunks_to_rhs(hunks, this, cx);
                        if !translated.is_empty() {
                            let stage = *stage;
                            this.rhs_editor.update(cx, |editor, cx| {
                                let chunk_by = translated.into_iter().chunk_by(|h| h.buffer_id);
                                for (buffer_id, hunks) in &chunk_by {
                                    editor.do_stage_or_unstage(stage, buffer_id, hunks, cx);
                                }
                            });
                        }
                    }
                }
                EditorEvent::RestoreRequested { hunks } => {
                    if this.lhs.is_some() {
                        let translated = translate_lhs_hunks_to_rhs(hunks, this, cx);
                        if !translated.is_empty() {
                            this.rhs_editor.update(cx, |editor, cx| {
                                editor.restore_diff_hunks(translated, cx);
                            });
                        }
                    }
                }
                EditorEvent::OpenExcerptsRequested {
                    selections_by_buffer,
                    split,
                } => {
                    if this.lhs.is_some() {
                        let translated =
                            translate_lhs_selections_to_rhs(selections_by_buffer, this, cx);
                        if !translated.is_empty() {
                            let workspace = this.workspace.clone();
                            let split = *split;
                            Editor::open_buffers_in_workspace(
                                workspace, translated, split, window, cx,
                            );
                        }
                    }
                }
                _ => cx.emit(event.clone()),
            },
        )];

        subscriptions.push(
            cx.subscribe(&lhs_editor, |this, _, event: &SearchEvent, cx| {
                if this.searched_side == Some(SplitSide::Left) {
                    cx.emit(event.clone());
                }
            }),
        );

        let lhs_focus_handle = lhs_editor.read(cx).focus_handle(cx);
        subscriptions.push(
            cx.on_focus_in(&lhs_focus_handle, window, |this, _window, cx| {
                if let Some(lhs) = &mut this.lhs {
                    if !lhs.was_last_focused {
                        lhs.was_last_focused = true;
                        cx.notify();
                    }
                }
            }),
        );

        let rhs_focus_handle = self.rhs_editor.read(cx).focus_handle(cx);
        subscriptions.push(
            cx.on_focus_in(&rhs_focus_handle, window, |this, _window, cx| {
                if let Some(lhs) = &mut this.lhs {
                    if lhs.was_last_focused {
                        lhs.was_last_focused = false;
                        cx.notify();
                    }
                }
            }),
        );

        let rhs_display_map = self.rhs_editor.read(cx).display_map.clone();
        let lhs_display_map = lhs_editor.read(cx).display_map.clone();
        let rhs_display_map_id = rhs_display_map.entity_id();
        let companion = cx.new(|_| Companion::new(rhs_display_map_id));
        let lhs = LhsEditor {
            editor: lhs_editor,
            multibuffer: lhs_multibuffer,
            was_last_focused: false,
            _subscriptions: subscriptions,
        };

        self.rhs_editor.update(cx, |editor, cx| {
            editor.set_delegate_expand_excerpts(true);
            editor.set_split_side(Some(SplitSide::Right), cx);
            editor.buffer().update(cx, |rhs_multibuffer, cx| {
                rhs_multibuffer.set_show_deleted_hunks(false, cx);
                rhs_multibuffer.set_use_extended_diff_range(true, cx);
            })
        });

        self.lhs = Some(lhs);

        self.sync_lhs_for_paths(all_paths, cx);

        rhs_display_map.update(cx, |dm, cx| {
            dm.set_companion(Some((lhs_display_map, companion.clone())), cx);
        });

        let lhs = self.lhs.as_ref().unwrap();

        let shared_scroll_anchor = self
            .rhs_editor
            .read(cx)
            .scroll_manager
            .scroll_anchor_entity();
        lhs.editor.update(cx, |editor, _cx| {
            editor
                .scroll_manager
                .set_shared_scroll_anchor(shared_scroll_anchor);
        });

        let this = cx.entity().downgrade();
        self.rhs_editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.set_on_local_selections_changed(Some(Box::new(
                move |cursor_position, window, cx| {
                    let this = this.clone();
                    window.defer(cx, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.sync_cursor_to_other_side(true, cursor_position, window, cx);
                        })
                        .ok();
                    })
                },
            )));
        });
        lhs.editor.update(cx, |editor, _cx| {
            let this = this.clone();
            editor.set_on_local_selections_changed(Some(Box::new(
                move |cursor_position, window, cx| {
                    let this = this.clone();
                    window.defer(cx, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.sync_cursor_to_other_side(false, cursor_position, window, cx);
                        })
                        .ok();
                    })
                },
            )));
        });

        // Copy soft wrap state from rhs (source of truth) to lhs
        let rhs_soft_wrap_override = self.rhs_editor.read(cx).soft_wrap_mode_override;
        lhs.editor.update(cx, |editor, cx| {
            editor.soft_wrap_mode_override = rhs_soft_wrap_override;
            cx.notify();
        });

        cx.notify();
    }

    fn diff_paths(&self, cx: &App) -> Vec<(PathKey, Entity<BufferDiff>)> {
        let rhs_multibuffer = self.rhs_multibuffer.read(cx);
        let rhs_multibuffer_snapshot = rhs_multibuffer.snapshot(cx);
        rhs_multibuffer_snapshot
            .buffers_with_paths()
            .filter_map(|(buffer, path)| {
                let diff = rhs_multibuffer.diff_for(buffer.remote_id())?;
                Some((path.clone(), diff))
            })
            .collect()
    }

    fn activate_pane_left(
        &mut self,
        _: &ActivatePaneLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(lhs) = &self.lhs {
            if !lhs.was_last_focused {
                lhs.editor.read(cx).focus_handle(cx).focus(window, cx);
                lhs.editor.update(cx, |editor, cx| {
                    editor.request_autoscroll(Autoscroll::fit(), cx);
                });
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn activate_pane_right(
        &mut self,
        _: &ActivatePaneRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                self.rhs_editor.read(cx).focus_handle(cx).focus(window, cx);
                self.rhs_editor.update(cx, |editor, cx| {
                    editor.request_autoscroll(Autoscroll::fit(), cx);
                });
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn sync_cursor_to_other_side(
        &mut self,
        from_rhs: bool,
        source_point: Point,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(lhs) = &self.lhs else {
            return;
        };

        let (source_editor, target_editor) = if from_rhs {
            (&self.rhs_editor, &lhs.editor)
        } else {
            (&lhs.editor, &self.rhs_editor)
        };

        let source_snapshot = source_editor.update(cx, |editor, cx| editor.snapshot(window, cx));
        let target_snapshot = target_editor.update(cx, |editor, cx| editor.snapshot(window, cx));

        let display_point = source_snapshot
            .display_snapshot
            .point_to_display_point(source_point, Bias::Right);
        let display_point = target_snapshot.clip_point(display_point, Bias::Right);
        let target_point = target_snapshot.display_point_to_point(display_point, Bias::Right);

        target_editor.update(cx, |editor, cx| {
            editor.set_suppress_selection_callback(true);
            editor.change_selections(crate::SelectionEffects::no_scroll(), window, cx, |s| {
                s.select_ranges([target_point..target_point]);
            });
            editor.set_suppress_selection_callback(false);
        });
    }

    pub fn toggle_split(
        &mut self,
        _: &ToggleSplitDiff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.diff_view_style {
            DiffViewStyle::Unified => {
                self.diff_view_style = DiffViewStyle::Split;
                if !self.too_narrow_for_split {
                    self.split(window, cx);
                }
            }
            DiffViewStyle::Split => {
                self.diff_view_style = DiffViewStyle::Unified;
                if self.is_split() {
                    self.unsplit(window, cx);
                }
            }
        }
    }

    fn intercept_toggle_breakpoint(
        &mut self,
        _: &ToggleBreakpoint,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only block breakpoint actions when the left (lhs) editor has focus
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                cx.stop_propagation();
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn intercept_enable_breakpoint(
        &mut self,
        _: &EnableBreakpoint,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only block breakpoint actions when the left (lhs) editor has focus
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                cx.stop_propagation();
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn intercept_disable_breakpoint(
        &mut self,
        _: &DisableBreakpoint,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only block breakpoint actions when the left (lhs) editor has focus
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                cx.stop_propagation();
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn intercept_edit_log_breakpoint(
        &mut self,
        _: &EditLogBreakpoint,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only block breakpoint actions when the left (lhs) editor has focus
        if let Some(lhs) = &self.lhs {
            if lhs.was_last_focused {
                cx.stop_propagation();
            } else {
                cx.propagate();
            }
        } else {
            cx.propagate();
        }
    }

    fn intercept_inline_assist(
        &mut self,
        _: &InlineAssist,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.lhs.is_some() {
            cx.stop_propagation();
        } else {
            cx.propagate();
        }
    }

    fn toggle_soft_wrap(
        &mut self,
        _: &ToggleSoftWrap,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(lhs) = &self.lhs {
            cx.stop_propagation();

            let is_lhs_focused = lhs.was_last_focused;
            let (focused_editor, other_editor) = if is_lhs_focused {
                (&lhs.editor, &self.rhs_editor)
            } else {
                (&self.rhs_editor, &lhs.editor)
            };

            // Toggle the focused editor
            focused_editor.update(cx, |editor, cx| {
                editor.toggle_soft_wrap(&ToggleSoftWrap, window, cx);
            });

            // Copy the soft wrap state from the focused editor to the other editor
            let soft_wrap_override = focused_editor.read(cx).soft_wrap_mode_override;
            other_editor.update(cx, |editor, cx| {
                editor.soft_wrap_mode_override = soft_wrap_override;
                cx.notify();
            });
        } else {
            cx.propagate();
        }
    }

    fn unsplit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(lhs) = self.lhs.take() else {
            return;
        };

        // Detach the stale lhs editor from the shared scroll anchor while the split companion still exists,
        // so its anchor can be converted to lhs native before rhs tears down split specific state.
        lhs.editor.update(cx, |editor, cx| {
            let lhs_snapshot = editor.display_map.update(cx, |dm, cx| dm.snapshot(cx));
            editor
                .scroll_manager
                .unshare_scroll_anchor(&lhs_snapshot, cx);
            editor.set_on_local_selections_changed(None);
        });

        self.rhs_editor.update(cx, |rhs, cx| {
            let rhs_snapshot = rhs.display_map.update(cx, |dm, cx| dm.snapshot(cx));
            let native_anchor = rhs.scroll_manager.native_anchor(&rhs_snapshot, cx);
            let rhs_display_map_id = rhs_snapshot.display_map_id;
            rhs.scroll_manager
                .scroll_anchor_entity()
                .update(cx, |shared, _| {
                    shared.scroll_anchor = native_anchor;
                    shared.display_map_id = Some(rhs_display_map_id);
                });

            rhs.set_on_local_selections_changed(None);
            rhs.set_delegate_expand_excerpts(false);
            rhs.set_split_side(None, cx);
            rhs.buffer().update(cx, |buffer, cx| {
                buffer.set_show_deleted_hunks(true, cx);
                buffer.set_use_extended_diff_range(false, cx);
            });
            rhs.display_map.update(cx, |dm, cx| {
                dm.set_companion(None, cx);
            });
        });
        cx.notify();
    }

    pub fn update_excerpts_for_path(
        &mut self,
        path: PathKey,
        buffer: Entity<Buffer>,
        ranges: impl IntoIterator<Item = Range<Point>> + Clone,
        context_line_count: u32,
        diff: Entity<BufferDiff>,
        cx: &mut Context<Self>,
    ) -> bool {
        let has_ranges = ranges.clone().into_iter().next().is_some();
        if self.lhs.is_none() {
            return self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
                let added_a_new_excerpt = rhs_multibuffer.update_excerpts_for_path(
                    path,
                    buffer.clone(),
                    ranges,
                    context_line_count,
                    cx,
                );
                if has_ranges
                    && rhs_multibuffer
                        .diff_for(buffer.read(cx).remote_id())
                        .is_none_or(|old_diff| old_diff.entity_id() != diff.entity_id())
                {
                    rhs_multibuffer.add_diff(diff, cx);
                }
                added_a_new_excerpt
            });
        }

        let result = self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
            let added_a_new_excerpt = rhs_multibuffer.update_excerpts_for_path(
                path.clone(),
                buffer.clone(),
                ranges,
                context_line_count,
                cx,
            );
            if has_ranges
                && rhs_multibuffer
                    .diff_for(buffer.read(cx).remote_id())
                    .is_none_or(|old_diff| old_diff.entity_id() != diff.entity_id())
            {
                rhs_multibuffer.add_diff(diff.clone(), cx);
            }
            added_a_new_excerpt
        });

        self.sync_lhs_for_paths(vec![(path, diff)], cx);
        result
    }

    fn expand_excerpts(
        &mut self,
        excerpt_anchors: impl Iterator<Item = Anchor> + Clone,
        lines: u32,
        direction: ExpandExcerptDirection,
        cx: &mut Context<Self>,
    ) {
        if self.lhs.is_none() {
            self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
                rhs_multibuffer.expand_excerpts(excerpt_anchors, lines, direction, cx);
            });
            return;
        }

        let paths: Vec<_> = self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
            let snapshot = rhs_multibuffer.snapshot(cx);
            let paths = excerpt_anchors
                .clone()
                .filter_map(|anchor| {
                    let (anchor, _) = snapshot.anchor_to_buffer_anchor(anchor)?;
                    let path = snapshot.path_for_buffer(anchor.buffer_id)?;
                    let diff = rhs_multibuffer.diff_for(anchor.buffer_id)?;
                    Some((path.clone(), diff))
                })
                .collect::<HashMap<_, _>>()
                .into_iter()
                .collect();
            rhs_multibuffer.expand_excerpts(excerpt_anchors, lines, direction, cx);
            paths
        });

        self.sync_lhs_for_paths(paths, cx);
    }

    pub fn remove_excerpts_for_path(&mut self, path: PathKey, cx: &mut Context<Self>) {
        self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
            rhs_multibuffer.remove_excerpts(path.clone(), cx);
        });

        if let Some(lhs) = &self.lhs {
            lhs.multibuffer.update(cx, |lhs_multibuffer, cx| {
                lhs_multibuffer.remove_excerpts(path, cx);
            });
        }
    }

    fn search_token(&self) -> SearchToken {
        SearchToken::new(self.focused_side() as u64)
    }

    fn editor_for_token(&self, token: SearchToken) -> Option<&Entity<Editor>> {
        if token.value() == SplitSide::Left as u64 {
            return self.lhs.as_ref().map(|lhs| &lhs.editor);
        }
        Some(&self.rhs_editor)
    }

    fn sync_lhs_for_paths(
        &self,
        paths: Vec<(PathKey, Entity<BufferDiff>)>,
        cx: &mut Context<Self>,
    ) {
        let Some(lhs) = &self.lhs else { return };

        self.rhs_multibuffer.update(cx, |rhs_multibuffer, cx| {
            for (path, diff) in &paths {
                let (path, diff) = (path.clone(), diff.clone());
                let main_buffer_id = diff.read(cx).buffer_id;
                let Some(main_buffer) = rhs_multibuffer.buffer(diff.read(cx).buffer_id) else {
                    lhs.multibuffer.update(cx, |lhs_multibuffer, lhs_cx| {
                        lhs_multibuffer.remove_excerpts(path, lhs_cx);
                    });
                    continue;
                };
                let main_buffer_snapshot = main_buffer.read(cx).snapshot();

                let base_text_buffer = diff.read(cx).base_text_buffer().clone();
                let diff_snapshot = diff.read(cx).snapshot(cx);
                let base_text_buffer_snapshot = base_text_buffer.read(cx).snapshot();

                let mut paired_ranges: Vec<(Range<Point>, ExcerptRange<text::Anchor>)> = Vec::new();

                let mut have_excerpt = false;
                let mut did_merge = false;
                let rhs_multibuffer_snapshot = rhs_multibuffer.snapshot(cx);
                for info in rhs_multibuffer_snapshot.excerpts_for_buffer(main_buffer_id) {
                    have_excerpt = true;
                    let rhs_context = info.context.to_point(&main_buffer_snapshot);
                    let lhs_context = buffer_range_to_base_text_range(
                        &rhs_context,
                        &diff_snapshot,
                        &main_buffer_snapshot,
                    );

                    if let Some((prev_lhs_context, prev_rhs_range)) = paired_ranges.last_mut()
                        && prev_lhs_context.end >= lhs_context.start
                    {
                        did_merge = true;
                        prev_lhs_context.end = lhs_context.end;
                        prev_rhs_range.context.end = info.context.end;
                        continue;
                    }

                    paired_ranges.push((lhs_context, info));
                }

                let (lhs_ranges, rhs_ranges): (Vec<_>, Vec<_>) = paired_ranges.into_iter().unzip();
                let lhs_ranges = lhs_ranges
                    .into_iter()
                    .map(|range| {
                        ExcerptRange::new(base_text_buffer_snapshot.anchor_range_outside(range))
                    })
                    .collect::<Vec<_>>();

                lhs.multibuffer.update(cx, |lhs_multibuffer, lhs_cx| {
                    lhs_multibuffer.update_path_excerpts(
                        path.clone(),
                        base_text_buffer,
                        &base_text_buffer_snapshot,
                        &lhs_ranges,
                        lhs_cx,
                    );
                    if have_excerpt
                        && lhs_multibuffer
                            .diff_for(base_text_buffer_snapshot.remote_id())
                            .is_none_or(|old_diff| old_diff.entity_id() != diff.entity_id())
                    {
                        lhs_multibuffer.add_inverted_diff(
                            diff.clone(),
                            main_buffer.clone(),
                            lhs_cx,
                        );
                    }
                });

                if did_merge {
                    rhs_multibuffer.update_path_excerpts(
                        path,
                        main_buffer,
                        &main_buffer_snapshot,
                        &rhs_ranges,
                        cx,
                    );
                }
            }
        });

        // After the excerpts land: the blame sources are pruned against the
        // left multibuffer's live buffers, which only now include these paths.
        self.sync_lhs_blame_sources(&paths, cx);
    }

    fn width_changed(&mut self, width: Pixels, window: &mut Window, cx: &mut Context<Self>) {
        self.last_width = Some(width);

        let min_ems = EditorSettings::get_global(cx).minimum_split_diff_width;

        let style = self.rhs_editor.read(cx).create_style(cx);
        let font_id = window.text_system().resolve_font(&style.text.font());
        let font_size = style.text.font_size.to_pixels(window.rem_size());
        let em_advance = window
            .text_system()
            .em_advance(font_id, font_size)
            .unwrap_or(font_size);
        let min_width = em_advance * min_ems;
        let is_split = self.lhs.is_some();

        self.too_narrow_for_split = min_ems > 0.0 && width < min_width;

        match self.diff_view_style {
            DiffViewStyle::Unified => {}
            DiffViewStyle::Split => {
                if self.too_narrow_for_split && is_split {
                    self.unsplit(window, cx);
                } else if !self.too_narrow_for_split && !is_split {
                    self.split(window, cx);
                }
            }
        }
    }

    pub fn remove_excerpts_for_buffer(
        &mut self,
        buffer_id: BufferId,
        cx: &mut Context<'_, SplittableEditor>,
    ) {
        let snapshot = self.rhs_multibuffer.read(cx).snapshot(cx);
        let Some(path) = snapshot.path_for_buffer(buffer_id) else {
            return;
        };
        self.remove_excerpts_for_path(path.clone(), cx);
    }
}

#[cfg(test)]
impl SplittableEditor {
    fn check_invariants(&self, quiesced: bool, cx: &mut App) {
        use text::Bias;

        use crate::display_map::Block;
        use crate::display_map::DisplayRow;

        let rhs_snapshot = self
            .rhs_editor
            .update(cx, |editor, cx| editor.display_snapshot(cx));

        let Some(lhs) = self.lhs.as_ref() else {
            assert!(
                rhs_snapshot.companion_snapshot().is_none(),
                "rhs display snapshot should not have a companion when unsplit"
            );

            let shared_scroll_anchor = self
                .rhs_editor
                .read(cx)
                .scroll_manager
                .shared_scroll_anchor(cx);
            if let Some(display_map_id) = shared_scroll_anchor.display_map_id {
                assert_eq!(
                    display_map_id, rhs_snapshot.display_map_id,
                    "unsplit editor should not retain a scroll anchor native to a torn-down split companion"
                );
            }

            let _ = self
                .rhs_editor
                .read(cx)
                .scroll_manager
                .native_anchor(&rhs_snapshot, cx);
            return;
        };

        self.debug_print(cx);
        self.check_excerpt_invariants(quiesced, cx);

        let lhs_snapshot = lhs
            .editor
            .update(cx, |editor, cx| editor.display_snapshot(cx));

        let lhs_companion = lhs_snapshot
            .companion_snapshot()
            .expect("lhs display snapshot should have rhs companion while split");
        assert_eq!(
            lhs_companion.display_map_id, rhs_snapshot.display_map_id,
            "lhs display snapshot companion should point to rhs display map"
        );
        assert!(
            lhs_companion.companion_snapshot().is_none(),
            "embedded companion snapshot should not recursively contain another companion"
        );

        let rhs_companion = rhs_snapshot
            .companion_snapshot()
            .expect("rhs display snapshot should have lhs companion while split");
        assert_eq!(
            rhs_companion.display_map_id, lhs_snapshot.display_map_id,
            "rhs display snapshot companion should point to lhs display map"
        );
        assert!(
            rhs_companion.companion_snapshot().is_none(),
            "embedded companion snapshot should not recursively contain another companion"
        );

        let lhs_scroll_anchor_entity_id = lhs
            .editor
            .read(cx)
            .scroll_manager
            .scroll_anchor_entity()
            .entity_id();
        let rhs_scroll_anchor_entity_id = self
            .rhs_editor
            .read(cx)
            .scroll_manager
            .scroll_anchor_entity()
            .entity_id();
        assert_eq!(
            lhs_scroll_anchor_entity_id, rhs_scroll_anchor_entity_id,
            "split editors should share a scroll anchor entity"
        );

        let shared_scroll_anchor = self
            .rhs_editor
            .read(cx)
            .scroll_manager
            .shared_scroll_anchor(cx);
        if let Some(display_map_id) = shared_scroll_anchor.display_map_id {
            assert!(
                display_map_id == lhs_snapshot.display_map_id
                    || display_map_id == rhs_snapshot.display_map_id,
                "shared scroll anchor should be native to one side of the split"
            );
        }
        let _ = lhs
            .editor
            .read(cx)
            .scroll_manager
            .native_anchor(&lhs_snapshot, cx);
        let _ = self
            .rhs_editor
            .read(cx)
            .scroll_manager
            .native_anchor(&rhs_snapshot, cx);

        if quiesced {
            let lhs_max_row = lhs_snapshot.max_point().row();
            let rhs_max_row = rhs_snapshot.max_point().row();
            assert_eq!(lhs_max_row, rhs_max_row, "mismatch in display row count");

            let lhs_excerpt_block_rows = lhs_snapshot
                .blocks_in_range(DisplayRow(0)..lhs_max_row + 1)
                .filter(|(_, block)| {
                    matches!(
                        block,
                        Block::BufferHeader { .. } | Block::ExcerptBoundary { .. }
                    )
                })
                .map(|(row, _)| row)
                .collect::<Vec<_>>();
            let rhs_excerpt_block_rows = rhs_snapshot
                .blocks_in_range(DisplayRow(0)..rhs_max_row + 1)
                .filter(|(_, block)| {
                    matches!(
                        block,
                        Block::BufferHeader { .. } | Block::ExcerptBoundary { .. }
                    )
                })
                .map(|(row, _)| row)
                .collect::<Vec<_>>();
            assert_eq!(lhs_excerpt_block_rows, rhs_excerpt_block_rows);

            for (lhs_hunk, rhs_hunk) in lhs_snapshot.diff_hunks().zip(rhs_snapshot.diff_hunks()) {
                assert_eq!(
                    lhs_hunk.diff_base_byte_range, rhs_hunk.diff_base_byte_range,
                    "mismatch in hunks"
                );
                assert_eq!(
                    lhs_hunk.status, rhs_hunk.status,
                    "mismatch in hunk statuses"
                );

                let (lhs_point, rhs_point) =
                    if lhs_hunk.row_range.is_empty() || rhs_hunk.row_range.is_empty() {
                        use multi_buffer::ToPoint as _;

                        let lhs_end = Point::new(lhs_hunk.row_range.end.0, 0);
                        let rhs_end = Point::new(rhs_hunk.row_range.end.0, 0);

                        let lhs_excerpt_end = lhs_snapshot
                            .anchor_in_excerpt(lhs_hunk.excerpt_range.context.end)
                            .unwrap()
                            .to_point(&lhs_snapshot);
                        let lhs_exceeds = lhs_end >= lhs_excerpt_end;
                        let rhs_excerpt_end = rhs_snapshot
                            .anchor_in_excerpt(rhs_hunk.excerpt_range.context.end)
                            .unwrap()
                            .to_point(&rhs_snapshot);
                        let rhs_exceeds = rhs_end >= rhs_excerpt_end;
                        if lhs_exceeds != rhs_exceeds {
                            continue;
                        }

                        (lhs_end, rhs_end)
                    } else {
                        (
                            Point::new(lhs_hunk.row_range.start.0, 0),
                            Point::new(rhs_hunk.row_range.start.0, 0),
                        )
                    };
                let lhs_point = lhs_snapshot.point_to_display_point(lhs_point, Bias::Left);
                let rhs_point = rhs_snapshot.point_to_display_point(rhs_point, Bias::Left);
                assert_eq!(
                    lhs_point.row(),
                    rhs_point.row(),
                    "mismatch in hunk position"
                );
            }
        }
    }

    fn debug_print(&self, cx: &mut App) {
        use crate::DisplayRow;
        use crate::display_map::Block;
        use buffer_diff::DiffHunkStatusKind;

        assert!(
            self.lhs.is_some(),
            "debug_print is only useful when lhs editor exists"
        );

        let lhs = self.lhs.as_ref().unwrap();

        // Get terminal width, default to 80 if unavailable
        let terminal_width = std::env::var("COLUMNS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(80);

        // Each side gets half the terminal width minus the separator
        let separator = " │ ";
        let side_width = (terminal_width - separator.len()) / 2;

        // Get display snapshots for both editors
        let lhs_snapshot = lhs.editor.update(cx, |editor, cx| {
            editor.display_map.update(cx, |map, cx| map.snapshot(cx))
        });
        let rhs_snapshot = self.rhs_editor.update(cx, |editor, cx| {
            editor.display_map.update(cx, |map, cx| map.snapshot(cx))
        });

        let lhs_max_row = lhs_snapshot.max_point().row().0;
        let rhs_max_row = rhs_snapshot.max_point().row().0;
        let max_row = lhs_max_row.max(rhs_max_row);

        // Build a map from display row -> block type string
        // Each row of a multi-row block gets an entry with the same block type
        // For spacers, the ID is included in brackets
        fn build_block_map(
            snapshot: &crate::DisplaySnapshot,
            max_row: u32,
        ) -> std::collections::HashMap<u32, String> {
            let mut block_map = std::collections::HashMap::new();
            for (start_row, block) in
                snapshot.blocks_in_range(DisplayRow(0)..DisplayRow(max_row + 1))
            {
                let (block_type, height) = match block {
                    Block::Spacer {
                        id,
                        height,
                        is_below: _,
                    } => (format!("SPACER[{}]", id.0), *height),
                    Block::ExcerptBoundary { height, .. } => {
                        ("EXCERPT_BOUNDARY".to_string(), *height)
                    }
                    Block::BufferHeader { height, .. } => ("BUFFER_HEADER".to_string(), *height),
                    Block::FoldedBuffer { height, .. } => ("FOLDED_BUFFER".to_string(), *height),
                    Block::Custom(custom) => {
                        ("CUSTOM_BLOCK".to_string(), custom.height.unwrap_or(1))
                    }
                };
                for offset in 0..height {
                    block_map.insert(start_row.0 + offset, block_type.clone());
                }
            }
            block_map
        }

        let lhs_blocks = build_block_map(&lhs_snapshot, lhs_max_row);
        let rhs_blocks = build_block_map(&rhs_snapshot, rhs_max_row);

        fn display_width(s: &str) -> usize {
            unicode_width::UnicodeWidthStr::width(s)
        }

        fn truncate_line(line: &str, max_width: usize) -> String {
            let line_width = display_width(line);
            if line_width <= max_width {
                return line.to_string();
            }
            if max_width < 9 {
                let mut result = String::new();
                let mut width = 0;
                for c in line.chars() {
                    let c_width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if width + c_width > max_width {
                        break;
                    }
                    result.push(c);
                    width += c_width;
                }
                return result;
            }
            let ellipsis = "...";
            let target_prefix_width = 3;
            let target_suffix_width = 3;

            let mut prefix = String::new();
            let mut prefix_width = 0;
            for c in line.chars() {
                let c_width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if prefix_width + c_width > target_prefix_width {
                    break;
                }
                prefix.push(c);
                prefix_width += c_width;
            }

            let mut suffix_chars: Vec<char> = Vec::new();
            let mut suffix_width = 0;
            for c in line.chars().rev() {
                let c_width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if suffix_width + c_width > target_suffix_width {
                    break;
                }
                suffix_chars.push(c);
                suffix_width += c_width;
            }
            suffix_chars.reverse();
            let suffix: String = suffix_chars.into_iter().collect();

            format!("{}{}{}", prefix, ellipsis, suffix)
        }

        fn pad_to_width(s: &str, target_width: usize) -> String {
            let current_width = display_width(s);
            if current_width >= target_width {
                s.to_string()
            } else {
                format!("{}{}", s, " ".repeat(target_width - current_width))
            }
        }

        // Helper to format a single row for one side
        // Format: "ln# diff bytes(cumul) text" or block info
        // Line numbers come from buffer_row in RowInfo (1-indexed for display)
        fn format_row(
            row: u32,
            max_row: u32,
            snapshot: &crate::DisplaySnapshot,
            blocks: &std::collections::HashMap<u32, String>,
            row_infos: &[multi_buffer::RowInfo],
            cumulative_bytes: &[usize],
            side_width: usize,
        ) -> String {
            // Get row info if available
            let row_info = row_infos.get(row as usize);

            // Line number prefix (3 chars + space)
            // Use buffer_row from RowInfo, which is None for block rows
            let line_prefix = if row > max_row {
                "    ".to_string()
            } else if let Some(buffer_row) = row_info.and_then(|info| info.buffer_row) {
                format!("{:>3} ", buffer_row + 1) // 1-indexed for display
            } else {
                "    ".to_string() // block rows have no line number
            };
            let content_width = side_width.saturating_sub(line_prefix.len());

            if row > max_row {
                return format!("{}{}", line_prefix, " ".repeat(content_width));
            }

            // Check if this row is a block row
            if let Some(block_type) = blocks.get(&row) {
                let block_str = format!("~~~[{}]~~~", block_type);
                let formatted = format!("{:^width$}", block_str, width = content_width);
                return format!(
                    "{}{}",
                    line_prefix,
                    truncate_line(&formatted, content_width)
                );
            }

            // Get line text
            let line_text = snapshot.line(DisplayRow(row));
            let line_bytes = line_text.len();

            // Diff status marker
            let diff_marker = match row_info.and_then(|info| info.diff_status.as_ref()) {
                Some(status) => match status.kind {
                    DiffHunkStatusKind::Added => "+",
                    DiffHunkStatusKind::Deleted => "-",
                    DiffHunkStatusKind::Modified => "~",
                },
                None => " ",
            };

            // Cumulative bytes
            let cumulative = cumulative_bytes.get(row as usize).copied().unwrap_or(0);

            // Format: "diff bytes(cumul) text" - use 3 digits for bytes, 4 for cumulative
            let info_prefix = format!("{}{:>3}({:>4}) ", diff_marker, line_bytes, cumulative);
            let text_width = content_width.saturating_sub(info_prefix.len());
            let truncated_text = truncate_line(&line_text, text_width);

            let text_part = pad_to_width(&truncated_text, text_width);
            format!("{}{}{}", line_prefix, info_prefix, text_part)
        }

        // Collect row infos for both sides
        let lhs_row_infos: Vec<_> = lhs_snapshot
            .row_infos(DisplayRow(0))
            .take((lhs_max_row + 1) as usize)
            .collect();
        let rhs_row_infos: Vec<_> = rhs_snapshot
            .row_infos(DisplayRow(0))
            .take((rhs_max_row + 1) as usize)
            .collect();

        // Calculate cumulative bytes for each side (only counting non-block rows)
        let mut lhs_cumulative = Vec::with_capacity((lhs_max_row + 1) as usize);
        let mut cumulative = 0usize;
        for row in 0..=lhs_max_row {
            if !lhs_blocks.contains_key(&row) {
                cumulative += lhs_snapshot.line(DisplayRow(row)).len() + 1; // +1 for newline
            }
            lhs_cumulative.push(cumulative);
        }

        let mut rhs_cumulative = Vec::with_capacity((rhs_max_row + 1) as usize);
        cumulative = 0;
        for row in 0..=rhs_max_row {
            if !rhs_blocks.contains_key(&row) {
                cumulative += rhs_snapshot.line(DisplayRow(row)).len() + 1;
            }
            rhs_cumulative.push(cumulative);
        }

        // Print header
        eprintln!();
        eprintln!("{}", "═".repeat(terminal_width));
        let header_left = format!("{:^width$}", "(LHS)", width = side_width);
        let header_right = format!("{:^width$}", "(RHS)", width = side_width);
        eprintln!("{}{}{}", header_left, separator, header_right);
        eprintln!(
            "{:^width$}{}{:^width$}",
            "ln# diff len(cum) text",
            separator,
            "ln# diff len(cum) text",
            width = side_width
        );
        eprintln!("{}", "─".repeat(terminal_width));

        // Print each row
        for row in 0..=max_row {
            let left = format_row(
                row,
                lhs_max_row,
                &lhs_snapshot,
                &lhs_blocks,
                &lhs_row_infos,
                &lhs_cumulative,
                side_width,
            );
            let right = format_row(
                row,
                rhs_max_row,
                &rhs_snapshot,
                &rhs_blocks,
                &rhs_row_infos,
                &rhs_cumulative,
                side_width,
            );
            eprintln!("{}{}{}", left, separator, right);
        }

        eprintln!("{}", "═".repeat(terminal_width));
        eprintln!("Legend: + added, - deleted, ~ modified, ~~~ block/spacer row");
        eprintln!();
    }

    fn check_excerpt_invariants(&self, quiesced: bool, cx: &gpui::App) {
        let lhs = self.lhs.as_ref().expect("should have lhs editor");

        let rhs_snapshot = self.rhs_multibuffer.read(cx).snapshot(cx);
        let rhs_excerpts = rhs_snapshot.excerpts().collect::<Vec<_>>();
        let lhs_snapshot = lhs.multibuffer.read(cx).snapshot(cx);
        let lhs_excerpts = lhs_snapshot.excerpts().collect::<Vec<_>>();
        assert_eq!(lhs_excerpts.len(), rhs_excerpts.len());

        for (lhs_excerpt, rhs_excerpt) in lhs_excerpts.into_iter().zip(rhs_excerpts) {
            assert_eq!(
                lhs_snapshot
                    .path_for_buffer(lhs_excerpt.context.start.buffer_id)
                    .unwrap(),
                rhs_snapshot
                    .path_for_buffer(rhs_excerpt.context.start.buffer_id)
                    .unwrap(),
                "corresponding excerpts should have the same path"
            );
            let diff = self
                .rhs_multibuffer
                .read(cx)
                .diff_for(rhs_excerpt.context.start.buffer_id)
                .expect("missing diff");
            assert_eq!(
                lhs_excerpt.context.start.buffer_id,
                diff.read(cx).base_text(cx).remote_id(),
                "corresponding lhs excerpt should show diff base text"
            );

            if quiesced {
                let diff_snapshot = diff.read(cx).snapshot(cx);
                let lhs_buffer_snapshot = lhs_snapshot
                    .buffer_for_id(lhs_excerpt.context.start.buffer_id)
                    .unwrap();
                let rhs_buffer_snapshot = rhs_snapshot
                    .buffer_for_id(rhs_excerpt.context.start.buffer_id)
                    .unwrap();
                let lhs_range = lhs_excerpt.context.to_point(&lhs_buffer_snapshot);
                let rhs_range = rhs_excerpt.context.to_point(&rhs_buffer_snapshot);
                let expected_lhs_range = buffer_range_to_base_text_range(
                    &rhs_range,
                    &diff_snapshot,
                    &rhs_buffer_snapshot,
                );
                assert_eq!(
                    lhs_range, expected_lhs_range,
                    "corresponding lhs excerpt should have a matching range"
                )
            }
        }
    }
}

impl Item for SplittableEditor {
    type Event = EditorEvent;

    fn tab_content_text(&self, detail: usize, cx: &App) -> ui::SharedString {
        self.rhs_editor.read(cx).tab_content_text(detail, cx)
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<ui::SharedString> {
        self.rhs_editor.read(cx).tab_tooltip_text(cx)
    }

    fn tab_icon(&self, window: &Window, cx: &App) -> Option<ui::Icon> {
        self.rhs_editor.read(cx).tab_icon(window, cx)
    }

    fn tab_content(&self, params: TabContentParams, window: &Window, cx: &App) -> gpui::AnyElement {
        self.rhs_editor.read(cx).tab_content(params, window, cx)
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.rhs_editor.read(cx).for_each_project_item(cx, f)
    }

    fn buffer_kind(&self, cx: &App) -> ItemBufferKind {
        self.rhs_editor.read(cx).buffer_kind(cx)
    }

    fn active_project_path(&self, cx: &App) -> Option<project::ProjectPath> {
        self.rhs_editor.read(cx).active_project_path(cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).has_conflict(cx)
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).has_deleted_file(cx)
    }

    fn capability(&self, cx: &App) -> language::Capability {
        self.rhs_editor.read(cx).capability(cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).can_save(cx)
    }

    fn can_save_as(&self, cx: &App) -> bool {
        self.rhs_editor.read(cx).can_save_as(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        self.rhs_editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: project::ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        self.rhs_editor
            .update(cx, |editor, cx| editor.save_as(project, path, window, cx))
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        self.rhs_editor
            .update(cx, |editor, cx| editor.reload(project, window, cx))
    }

    fn navigate(
        &mut self,
        data: Arc<dyn std::any::Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.focused_editor()
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_editor().update(cx, |editor, cx| {
            editor.deactivated(window, cx);
        });
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace = workspace.weak_handle();
        self.rhs_editor.update(cx, |rhs_editor, cx| {
            rhs_editor.added_to_workspace(workspace, window, cx);
        });
        if let Some(lhs) = &self.lhs {
            lhs.editor.update(cx, |lhs_editor, cx| {
                lhs_editor.added_to_workspace(workspace, window, cx);
            });
        }
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        self.rhs_editor.read(cx).breadcrumb_location(cx)
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        self.rhs_editor.read(cx).breadcrumbs(cx)
    }

    fn pixel_position_of_cursor(&self, cx: &App) -> Option<gpui::Point<gpui::Pixels>> {
        self.focused_editor().read(cx).pixel_position_of_cursor(cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: std::any::TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == std::any::TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == std::any::TypeId::of::<Editor>() {
            Some(self.rhs_editor.clone().into())
        } else {
            None
        }
    }
}

impl SearchableItem for SplittableEditor {
    type Match = Range<Anchor>;

    fn clear_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rhs_editor.update(cx, |editor, cx| {
            editor.clear_matches(window, cx);
        });
        if let Some(lhs_editor) = self.lhs_editor() {
            lhs_editor.update(cx, |editor, cx| {
                editor.clear_matches(window, cx);
            })
        }
    }

    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        active_match_index: Option<usize>,
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.editor_for_token(token) else {
            return;
        };
        target.update(cx, |editor, cx| {
            editor.update_matches(matches, active_match_index, token, window, cx);
        });
    }

    fn search_bar_visibility_changed(
        &mut self,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if visible {
            let side = self.focused_side();
            self.searched_side = Some(side);
            match side {
                SplitSide::Left => {
                    self.rhs_editor.update(cx, |editor, cx| {
                        editor.clear_matches(window, cx);
                    });
                }
                SplitSide::Right => {
                    if let Some(lhs) = &self.lhs {
                        lhs.editor.update(cx, |editor, cx| {
                            editor.clear_matches(window, cx);
                        });
                    }
                }
            }
        } else {
            self.searched_side = None;
        }
    }

    fn query_suggestion(
        &mut self,
        seed_query_override: Option<SeedQuerySetting>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.focused_editor().update(cx, |editor, cx| {
            editor.query_suggestion(seed_query_override, window, cx)
        })
    }

    fn activate_match(
        &mut self,
        index: usize,
        matches: &[Self::Match],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.editor_for_token(token) else {
            return;
        };
        target.update(cx, |editor, cx| {
            editor.activate_match(index, matches, token, window, cx);
        });
    }

    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.editor_for_token(token) else {
            return;
        };
        target.update(cx, |editor, cx| {
            editor.select_matches(matches, token, window, cx);
        });
    }

    fn replace(
        &mut self,
        identifier: &Self::Match,
        query: &project::search::SearchQuery,
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.editor_for_token(token) else {
            return;
        };
        target.update(cx, |editor, cx| {
            editor.replace(identifier, query, token, window, cx);
        });
    }

    fn find_matches(
        &mut self,
        query: Arc<project::search::SearchQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<Vec<Self::Match>> {
        self.focused_editor()
            .update(cx, |editor, cx| editor.find_matches(query, window, cx))
    }

    fn find_matches_with_token(
        &mut self,
        query: Arc<project::search::SearchQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<(Vec<Self::Match>, SearchToken)> {
        let token = self.search_token();
        let editor = self.focused_editor().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let Some(matches) = editor
                .update_in(cx, |editor, window, cx| {
                    editor.find_matches(query, window, cx)
                })
                .ok()
            else {
                return (Vec::new(), token);
            };
            (matches.await, token)
        })
    }

    fn active_match_index(
        &mut self,
        direction: workspace::searchable::Direction,
        matches: &[Self::Match],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.editor_for_token(token)?.update(cx, |editor, cx| {
            editor.active_match_index(direction, matches, token, window, cx)
        })
    }
}

impl EventEmitter<EditorEvent> for SplittableEditor {}
impl EventEmitter<SearchEvent> for SplittableEditor {}
impl Focusable for SplittableEditor {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.focused_editor().read(cx).focus_handle(cx)
    }
}

impl Render for SplittableEditor {
    fn render(
        &mut self,
        _window: &mut ui::Window,
        cx: &mut ui::Context<Self>,
    ) -> impl ui::IntoElement {
        let is_split = self.lhs.is_some();
        let inner = if is_split {
            let style = self.rhs_editor.read(cx).create_style(cx);
            SplitEditorView::new(cx.entity(), style, self.split_state.clone()).into_any_element()
        } else {
            self.rhs_editor.clone().into_any_element()
        };

        let this = cx.entity().downgrade();
        let last_width = self.last_width;

        div()
            .id("splittable-editor")
            .on_action(cx.listener(Self::toggle_split))
            .on_action(cx.listener(Self::activate_pane_left))
            .on_action(cx.listener(Self::activate_pane_right))
            .on_action(cx.listener(Self::intercept_toggle_breakpoint))
            .on_action(cx.listener(Self::intercept_enable_breakpoint))
            .on_action(cx.listener(Self::intercept_disable_breakpoint))
            .on_action(cx.listener(Self::intercept_edit_log_breakpoint))
            .on_action(cx.listener(Self::intercept_inline_assist))
            .capture_action(cx.listener(Self::toggle_soft_wrap))
            .size_full()
            .child(inner)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let width = bounds.size.width;
                        if last_width == Some(width) {
                            return;
                        }
                        window.defer(cx, move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.width_changed(width, window, cx);
                            })
                            .ok();
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::{any::TypeId, sync::Arc};

    use buffer_diff::BufferDiff;
    use collections::{HashMap, HashSet};
    use fs::FakeFs;
    use gpui::{AppContext as _, Entity, Pixels, VisualTestContext};
    use gpui::{BorrowAppContext as _, Element as _};
    use language::language_settings::SoftWrap;
    use language::{Buffer, Capability};
    use multi_buffer::{MultiBuffer, PathKey};
    use pretty_assertions::assert_eq;
    use project::Project;
    use rand::rngs::StdRng;
    use settings::{DiffViewStyle, SettingsStore};
    use ui::{VisualContext as _, div, px};
    use util::rel_path::rel_path;
    use workspace::{Item, MultiWorkspace};

    use crate::display_map::{
        BlockPlacement, BlockProperties, BlockStyle, Crease, FoldPlaceholder,
    };
    use crate::inlays::Inlay;
    use crate::test::{editor_content_with_blocks_and_width, set_block_content_for_tests};
    use crate::{Editor, SplittableEditor};
    use multi_buffer::MultiBufferOffset;

    async fn init_test(
        cx: &mut gpui::TestAppContext,
        soft_wrap: SoftWrap,
        style: DiffViewStyle,
    ) -> (Entity<SplittableEditor>, &mut VisualTestContext) {
        init_test_with_headers(cx, soft_wrap, style, true).await
    }

    /// `rhs_shows_headers` picks which `MultiBuffer` constructor the right pane
    /// gets. `false` is a *non-singleton* headerless multibuffer — the shape a
    /// single-file commit diff and an agent edit review build, and the one that
    /// used to leave the left pane two rows lower than the right.
    async fn init_test_with_headers(
        cx: &mut gpui::TestAppContext,
        soft_wrap: SoftWrap,
        style: DiffViewStyle,
        rhs_shows_headers: bool,
    ) -> (Entity<SplittableEditor>, &mut VisualTestContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(style);
                    // These tests pin the pane width and assert exactly where
                    // each line wraps, so the text area they get must not move
                    // when the gutter's reserved digit count is retuned.
                    settings
                        .editor
                        .gutter
                        .get_or_insert_default()
                        .min_line_number_digits = Some(4);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs as Arc<dyn fs::Fs>, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let rhs_multibuffer = cx.new(|cx| {
            let mut multibuffer = if rhs_shows_headers {
                MultiBuffer::new(Capability::ReadWrite)
            } else {
                MultiBuffer::without_headers(Capability::ReadWrite)
            };
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new_window_entity(|window, cx| {
            let editor = SplittableEditor::new(
                style,
                rhs_multibuffer.clone(),
                project.clone(),
                workspace,
                window,
                cx,
            );
            editor.update_editors(cx, |editor, cx| {
                editor.set_soft_wrap_mode(soft_wrap, cx);
            });
            editor
        });
        (editor, cx)
    }

    /// A split diff whose right pane is a genuine singleton buffer — the shape
    /// the git panel's solo file diff builds, and the one where the two panes
    /// used to compose different gutters.
    async fn init_singleton_test<'a>(
        cx: &'a mut gpui::TestAppContext,
        base_text: &str,
        current_text: &str,
    ) -> (Entity<SplittableEditor>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Split);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs as Arc<dyn fs::Fs>, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let (buffer, diff) = buffer_with_diff(base_text, current_text, cx);
        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });
        let rhs_multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::singleton(buffer.clone(), cx);
            multibuffer.add_diff(diff, cx);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new_window_entity(|window, cx| {
            SplittableEditor::new(
                DiffViewStyle::Split,
                rhs_multibuffer.clone(),
                project.clone(),
                workspace,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| editor.split(window, cx));
        cx.run_until_parked();
        (editor, cx)
    }

    fn buffer_with_diff(
        base_text: &str,
        current_text: &str,
        cx: &mut VisualTestContext,
    ) -> (Entity<Buffer>, Entity<BufferDiff>) {
        let buffer = cx.new(|cx| Buffer::local(current_text.to_string(), cx));
        let diff = cx.new(|cx| {
            BufferDiff::new_with_base_text(base_text, &buffer.read(cx).text_snapshot(), cx)
        });
        (buffer, diff)
    }

    #[track_caller]
    fn assert_split_content(
        editor: &Entity<SplittableEditor>,
        expected_rhs: String,
        expected_lhs: String,
        cx: &mut VisualTestContext,
    ) {
        assert_split_content_with_widths(
            editor,
            px(3000.0),
            px(3000.0),
            expected_rhs,
            expected_lhs,
            cx,
        );
    }

    #[track_caller]
    fn assert_split_content_with_widths(
        editor: &Entity<SplittableEditor>,
        rhs_width: Pixels,
        lhs_width: Pixels,
        expected_rhs: String,
        expected_lhs: String,
        cx: &mut VisualTestContext,
    ) {
        let (rhs_editor, lhs_editor) = editor.update(cx, |editor, _cx| {
            let lhs = editor.lhs.as_ref().expect("should have lhs editor");
            (editor.rhs_editor.clone(), lhs.editor.clone())
        });

        // Make sure both sides learn if the other has soft-wrapped
        let _ = editor_content_with_blocks_and_width(&rhs_editor, rhs_width, cx);
        cx.run_until_parked();
        let _ = editor_content_with_blocks_and_width(&lhs_editor, lhs_width, cx);
        cx.run_until_parked();

        let rhs_content = editor_content_with_blocks_and_width(&rhs_editor, rhs_width, cx);
        let lhs_content = editor_content_with_blocks_and_width(&lhs_editor, lhs_width, cx);

        if rhs_content != expected_rhs || lhs_content != expected_lhs {
            editor.update(cx, |editor, cx| editor.debug_print(cx));
        }

        assert_eq!(rhs_content, expected_rhs, "rhs");
        assert_eq!(lhs_content, expected_lhs, "lhs");
    }

    #[gpui::test(iterations = 25)]
    async fn test_random_split_editor(mut rng: StdRng, cx: &mut gpui::TestAppContext) {
        use multi_buffer::ExpandExcerptDirection;
        use rand::prelude::*;
        use util::RandomCharIter;

        let (editor, cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;
        let operations = std::env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(10);
        let rng = &mut rng;
        for _ in 0..operations {
            let buffers = editor.update(cx, |editor, cx| {
                editor.rhs_editor.read(cx).buffer().read(cx).all_buffers()
            });

            if buffers.is_empty() {
                log::info!("creating initial buffer");
                let len = rng.random_range(200..1000);
                let base_text: String = RandomCharIter::new(&mut *rng).take(len).collect();
                let buffer = cx.new(|cx| Buffer::local(base_text.clone(), cx));
                let buffer_snapshot = buffer.read_with(cx, |b, _| b.text_snapshot());
                let diff =
                    cx.new(|cx| BufferDiff::new_with_base_text(&base_text, &buffer_snapshot, cx));
                let edit_count = rng.random_range(3..8);
                buffer.update(cx, |buffer, cx| {
                    buffer.randomly_edit(rng, edit_count, cx);
                });
                let buffer_snapshot = buffer.read_with(cx, |b, _| b.text_snapshot());
                diff.update(cx, |diff, cx| {
                    diff.recalculate_diff_sync(&buffer_snapshot, cx);
                });
                let diff_snapshot = diff.read_with(cx, |diff, cx| diff.snapshot(cx));
                let ranges = diff_snapshot
                    .hunks(&buffer_snapshot)
                    .map(|hunk| hunk.range)
                    .collect::<Vec<_>>();
                let context_lines = rng.random_range(0..2);
                editor.update(cx, |editor, cx| {
                    let path = PathKey::for_buffer(&buffer, cx);
                    editor.update_excerpts_for_path(path, buffer, ranges, context_lines, diff, cx);
                });
                editor.update(cx, |editor, cx| {
                    editor.check_invariants(true, cx);
                });
                continue;
            }

            let mut quiesced = false;

            match rng.random_range(0..100) {
                0..=14 if buffers.len() < 6 => {
                    log::info!("creating new buffer and setting excerpts");
                    let len = rng.random_range(200..1000);
                    let base_text: String = RandomCharIter::new(&mut *rng).take(len).collect();
                    let buffer = cx.new(|cx| Buffer::local(base_text.clone(), cx));
                    let buffer_snapshot = buffer.read_with(cx, |b, _| b.text_snapshot());
                    let diff = cx
                        .new(|cx| BufferDiff::new_with_base_text(&base_text, &buffer_snapshot, cx));
                    let edit_count = rng.random_range(3..8);
                    buffer.update(cx, |buffer, cx| {
                        buffer.randomly_edit(rng, edit_count, cx);
                    });
                    let buffer_snapshot = buffer.read_with(cx, |b, _| b.text_snapshot());
                    diff.update(cx, |diff, cx| {
                        diff.recalculate_diff_sync(&buffer_snapshot, cx);
                    });
                    let diff_snapshot = diff.read_with(cx, |diff, cx| diff.snapshot(cx));
                    let ranges = diff_snapshot
                        .hunks(&buffer_snapshot)
                        .map(|hunk| hunk.range)
                        .collect::<Vec<_>>();
                    let context_lines = rng.random_range(0..2);
                    editor.update(cx, |editor, cx| {
                        let path = PathKey::for_buffer(&buffer, cx);
                        editor.update_excerpts_for_path(
                            path,
                            buffer,
                            ranges,
                            context_lines,
                            diff,
                            cx,
                        );
                    });
                }
                15..=29 => {
                    log::info!("randomly editing multibuffer");
                    let edit_count = rng.random_range(1..5);
                    editor.update(cx, |editor, cx| {
                        editor.rhs_multibuffer.update(cx, |multibuffer, cx| {
                            multibuffer.randomly_edit(rng, edit_count, cx);
                        });
                    });
                }
                30..=44 => {
                    log::info!("randomly editing individual buffer");
                    let buffer = buffers.iter().choose(rng).unwrap();
                    let edit_count = rng.random_range(1..3);
                    buffer.update(cx, |buffer, cx| {
                        buffer.randomly_edit(rng, edit_count, cx);
                    });
                }
                45..=54 => {
                    log::info!("recalculating diff and resetting excerpts for single buffer");
                    let buffer = buffers.iter().choose(rng).unwrap();
                    let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
                    let diff = editor.update(cx, |editor, cx| {
                        editor
                            .rhs_multibuffer
                            .read(cx)
                            .diff_for(buffer.read(cx).remote_id())
                            .unwrap()
                    });
                    diff.update(cx, |diff, cx| {
                        diff.recalculate_diff_sync(&buffer_snapshot, cx);
                    });
                    cx.run_until_parked();
                    let diff_snapshot = diff.read_with(cx, |diff, cx| diff.snapshot(cx));
                    let ranges = diff_snapshot
                        .hunks(&buffer_snapshot)
                        .map(|hunk| hunk.range)
                        .collect::<Vec<_>>();
                    let context_lines = rng.random_range(0..2);
                    let buffer = buffer.clone();
                    editor.update(cx, |editor, cx| {
                        let path = PathKey::for_buffer(&buffer, cx);
                        editor.update_excerpts_for_path(
                            path,
                            buffer,
                            ranges,
                            context_lines,
                            diff,
                            cx,
                        );
                    });
                }
                55..=64 => {
                    log::info!("randomly undoing/redoing in single buffer");
                    let buffer = buffers.iter().choose(rng).unwrap();
                    buffer.update(cx, |buffer, cx| {
                        buffer.randomly_undo_redo(rng, cx);
                    });
                }
                65..=74 => {
                    log::info!("removing excerpts for a random path");
                    let ids = editor.update(cx, |editor, cx| {
                        let snapshot = editor.rhs_multibuffer.read(cx).snapshot(cx);
                        snapshot.all_buffer_ids().collect::<Vec<_>>()
                    });
                    if let Some(id) = ids.choose(rng) {
                        editor.update(cx, |editor, cx| {
                            let snapshot = editor.rhs_multibuffer.read(cx).snapshot(cx);
                            let path = snapshot.path_for_buffer(*id).unwrap();
                            editor.remove_excerpts_for_path(path.clone(), cx);
                        });
                    }
                }
                75..=79 => {
                    log::info!("unsplit, scroll stale lhs, and resplit");
                    let Some(lhs_editor) = editor.update(cx, |editor, _cx| {
                        editor.lhs.as_ref().map(|lhs| lhs.editor.clone())
                    }) else {
                        continue;
                    };
                    let lhs_max_row = lhs_editor.update(cx, |editor, cx| {
                        editor.display_snapshot(cx).max_point().row().0
                    });
                    editor.update_in(cx, |editor, window, cx| {
                        editor.unsplit(window, cx);
                    });
                    cx.run_until_parked();

                    if lhs_max_row > 0 {
                        lhs_editor.update_in(cx, |editor, window, cx| {
                            editor.set_scroll_position(gpui::Point::new(0., 1.), window, cx);
                        });
                        editor.update(cx, |editor, cx| {
                            editor.check_invariants(false, cx);
                        });
                    }

                    editor.update_in(cx, |editor, window, cx| {
                        editor.split(window, cx);
                    });
                }
                80..=89 => {
                    let snapshot = editor.update(cx, |editor, cx| {
                        editor.rhs_multibuffer.read(cx).snapshot(cx)
                    });
                    let excerpts = snapshot.excerpts().collect::<Vec<_>>();
                    if !excerpts.is_empty() {
                        let count = rng.random_range(1..=excerpts.len().min(3));
                        let chosen: Vec<_> =
                            excerpts.choose_multiple(rng, count).cloned().collect();
                        let line_count = rng.random_range(1..5);
                        log::info!("expanding {count} excerpts by {line_count} lines");
                        editor.update(cx, |editor, cx| {
                            editor.expand_excerpts(
                                chosen.into_iter().map(|excerpt| {
                                    snapshot.anchor_in_excerpt(excerpt.context.start).unwrap()
                                }),
                                line_count,
                                ExpandExcerptDirection::UpAndDown,
                                cx,
                            );
                        });
                    }
                }
                _ => {
                    log::info!("quiescing");
                    for buffer in buffers {
                        let buffer_snapshot =
                            buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
                        let diff = editor.update(cx, |editor, cx| {
                            editor
                                .rhs_multibuffer
                                .read(cx)
                                .diff_for(buffer.read(cx).remote_id())
                                .unwrap()
                        });
                        diff.update(cx, |diff, cx| {
                            diff.recalculate_diff_sync(&buffer_snapshot, cx);
                        });
                        cx.run_until_parked();
                        let diff_snapshot = diff.read_with(cx, |diff, cx| diff.snapshot(cx));
                        let ranges = diff_snapshot
                            .hunks(&buffer_snapshot)
                            .map(|hunk| hunk.range)
                            .collect::<Vec<_>>();
                        editor.update(cx, |editor, cx| {
                            let path = PathKey::for_buffer(&buffer, cx);
                            editor.update_excerpts_for_path(path, buffer, ranges, 2, diff, cx);
                        });
                    }
                    quiesced = true;
                }
            }

            editor.update(cx, |editor, cx| {
                editor.check_invariants(quiesced, cx);
            });
        }
    }

    #[gpui::test]
    async fn test_expand_excerpt_with_hunk_before_excerpt_start(cx: &mut gpui::TestAppContext) {
        use rope::Point;

        let (editor, cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "aaaaaaa rest_of_line\nsecond_line\nthird_line\nfourth_line";
        let current_text = "aaaaaaa rest_of_line\nsecond_line\nMODIFIED\nfourth_line";
        let (buffer, diff) = buffer_with_diff(base_text, current_text, cx);

        let buffer_snapshot = buffer.read_with(cx, |b, _| b.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });
        cx.run_until_parked();

        let diff_snapshot = diff.read_with(cx, |diff, cx| diff.snapshot(cx));
        let ranges = diff_snapshot
            .hunks(&buffer_snapshot)
            .map(|hunk| hunk.range)
            .collect::<Vec<_>>();

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(path, buffer.clone(), ranges, 0, diff.clone(), cx);
        });
        cx.run_until_parked();

        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(Point::new(0, 7)..Point::new(1, 7), "\nnew_line\n")],
                None,
                cx,
            );
        });

        let excerpts = editor.update(cx, |editor, cx| {
            let snapshot = editor.rhs_multibuffer.read(cx).snapshot(cx);
            snapshot
                .excerpts()
                .map(|excerpt| snapshot.anchor_in_excerpt(excerpt.context.start).unwrap())
                .collect::<Vec<_>>()
        });
        editor.update(cx, |editor, cx| {
            editor.expand_excerpts(
                excerpts.into_iter(),
                2,
                multi_buffer::ExpandExcerptDirection::UpAndDown,
                cx,
            );
        });
    }

    /// A right pane may suppress buffer headers without being a singleton: the
    /// single-file commit diff (`git_ui::commit_view`) and an agent's edit
    /// review both build `MultiBuffer::without_headers` over real path
    /// excerpts. `SplittableEditor::split` used to decide the left pane's own
    /// header setting from `rhs.is_singleton()` rather than from
    /// `rhs.show_headers()`, so those cases got a left pane built with
    /// `MultiBuffer::new` — headers on — and a `Block::BufferHeader` of
    /// `FILE_HEADER_HEIGHT` (two) rows that the right pane did not have. Every
    /// line in the left pane, gutter and text alike, then painted
    /// `2 * line_height` low, and the connector ribbons ramped across the gap
    /// because they believed it.
    #[gpui::test]
    async fn test_headerless_right_pane_does_not_offset_the_left_pane(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::display_map::{Block, DisplayRow, DisplaySnapshot};
        use rope::Point;
        use text::Bias;
        use unindent::Unindent as _;

        let (editor, mut cx) =
            init_test_with_headers(cx, SoftWrap::EditorWidth, DiffViewStyle::Split, false).await;

        // The maintainer's repro: exactly one line differs.
        let base_text = "
            <project>
              <properties>
                <revision>3.29.0</revision>
              </properties>
            </project>
        "
        .unindent();
        let current_text = base_text.replace("3.29.0", "3.29.1");

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            editor.update_excerpts_for_path(
                PathKey::sorted(0),
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });
        cx.run_until_parked();

        let (lhs_snapshot, rhs_snapshot) = editor.update(cx, |editor, cx| {
            let lhs = editor.lhs.as_ref().expect("should have split");
            (
                lhs.editor
                    .update(cx, |editor, cx| editor.display_snapshot(cx)),
                editor
                    .rhs_editor
                    .update(cx, |editor, cx| editor.display_snapshot(cx)),
            )
        });

        fn header_block_rows(snapshot: &DisplaySnapshot) -> Vec<DisplayRow> {
            snapshot
                .blocks_in_range(DisplayRow(0)..snapshot.max_point().row() + 1)
                .filter(|(_, block)| {
                    matches!(
                        block,
                        Block::BufferHeader { .. } | Block::ExcerptBoundary { .. }
                    )
                })
                .map(|(row, _)| row)
                .collect()
        }

        assert_eq!(
            header_block_rows(&lhs_snapshot),
            Vec::<DisplayRow>::new(),
            "the left pane must not invent a buffer header the right pane lacks",
        );
        assert_eq!(
            header_block_rows(&lhs_snapshot),
            header_block_rows(&rhs_snapshot),
            "both panes reserve the same block rows",
        );
        assert_eq!(
            lhs_snapshot.max_point().row(),
            rhs_snapshot.max_point().row(),
            "both panes span the same number of display rows",
        );

        // The panes share one scroll anchor, so a row's y is decided entirely
        // by its display row: `split_connectors::row_y` is
        // `top + (row - scroll_top) * line_height`, and `scroll_top` is 0 here.
        const LINE_HEIGHT: Pixels = px(23.);
        let y_of = |snapshot: &DisplaySnapshot, buffer_row: u32| {
            let display_row = snapshot
                .point_to_display_point(Point::new(buffer_row, 0), Bias::Left)
                .row();
            LINE_HEIGHT * display_row.0 as f32
        };

        assert_eq!(
            (y_of(&lhs_snapshot, 0), y_of(&rhs_snapshot, 0)),
            (px(0.), px(0.)),
            "both panes put their first line of text at the top of the viewport",
        );
        // `<revision>` is buffer row 2 on both sides — a one-line modification
        // changes no line counts — so both panes owe it the same y.
        assert_eq!(
            (y_of(&lhs_snapshot, 2), y_of(&rhs_snapshot, 2)),
            (px(46.), px(46.)),
            "the changed line must sit on the same y in both panes; before the \
             fix the left pane put it at 46 + 2 * 23 = 92",
        );

        editor.update(cx, |editor, cx| editor.check_invariants(true, cx));
    }

    #[gpui::test]
    async fn test_basic_alignment(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
            ccc
            ddd
            eee
            fff
        "
        .unindent();
        let current_text = "
            aaa
            ddd
            eee
            fff
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            § spacer
            § spacer
            ddd
            eee
            fff"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee
            fff"
            .unindent(),
            &mut cx,
        );

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(3, 0)..Point::new(3, 3), "FFF")], None, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            § spacer
            § spacer
            ddd
            eee
            FFF"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee
            fff"
            .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            § spacer
            § spacer
            ddd
            eee
            FFF"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee
            fff"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_deleting_unmodified_lines(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text1 = "
            aaa
            bbb
            ccc
            ddd
            eee"
        .unindent();

        let base_text2 = "
            fff
            ggg
            hhh
            iii
            jjj"
        .unindent();

        let (buffer1, diff1) = buffer_with_diff(&base_text1, &base_text1, &mut cx);
        let (buffer2, diff2) = buffer_with_diff(&base_text2, &base_text2, &mut cx);

        editor.update(cx, |editor, cx| {
            let path1 = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path1,
                buffer1.clone(),
                vec![Point::new(0, 0)..buffer1.read(cx).max_point()],
                0,
                diff1.clone(),
                cx,
            );
            let path2 = PathKey::sorted(1);
            editor.update_excerpts_for_path(
                path2,
                buffer2.clone(),
                vec![Point::new(0, 0)..buffer2.read(cx).max_point()],
                1,
                diff2.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        buffer1.update(cx, |buffer, cx| {
            buffer.edit(
                [
                    (Point::new(0, 0)..Point::new(1, 0), ""),
                    (Point::new(3, 0)..Point::new(4, 0), ""),
                ],
                None,
                cx,
            );
        });
        buffer2.update(cx, |buffer, cx| {
            buffer.edit(
                [
                    (Point::new(0, 0)..Point::new(1, 0), ""),
                    (Point::new(3, 0)..Point::new(4, 0), ""),
                ],
                None,
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc
            § spacer
            eee
            § <no file>
            § -----
            § spacer
            ggg
            hhh
            § spacer
            jjj"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee
            § <no file>
            § -----
            fff
            ggg
            hhh
            iii
            jjj"
            .unindent(),
            &mut cx,
        );

        let buffer1_snapshot = buffer1.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff1.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer1_snapshot, cx);
        });
        let buffer2_snapshot = buffer2.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff2.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer2_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc
            § spacer
            eee
            § <no file>
            § -----
            § spacer
            ggg
            hhh
            § spacer
            jjj"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee
            § <no file>
            § -----
            fff
            ggg
            hhh
            iii
            jjj"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_deleting_added_line(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
            ccc
            ddd
        "
        .unindent();

        let current_text = "
            aaa
            NEW1
            NEW2
            ccc
            ddd
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            NEW1
            NEW2
            ccc
            ddd"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            § spacer
            ccc
            ddd"
            .unindent(),
            &mut cx,
        );

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(2, 0)..Point::new(3, 0), "")], None, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            NEW1
            ccc
            ddd"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd"
            .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            NEW1
            ccc
            ddd"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_inserting_consecutive_blank_line(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb





            ccc
            ddd
        "
        .unindent();
        let current_text = "
            aaa
            bbb





            CCC
            ddd
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(1, 3)..Point::new(1, 3), "\n")], None, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb






            CCC
            ddd"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            § spacer





            ccc
            ddd"
            .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb






            CCC
            ddd"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb





            ccc
            § spacer
            ddd"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_reverting_deletion_hunk(cx: &mut gpui::TestAppContext) {
        use git::Restore;
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
            ccc
            ddd
            eee
        "
        .unindent();
        let current_text = "
            aaa
            ddd
            eee
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            § spacer
            § spacer
            ddd
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            &mut cx,
        );

        let rhs_editor = editor.update(cx, |editor, _cx| editor.rhs_editor.clone());
        cx.update_window_entity(&rhs_editor, |editor, window, cx| {
            editor.change_selections(crate::SelectionEffects::no_scroll(), window, cx, |s| {
                s.select_ranges([Point::new(1, 0)..Point::new(1, 0)]);
            });
            editor.git_restore(&Restore, window, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_deleting_added_lines(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            old1
            old2
            old3
            old4
            zzz
        "
        .unindent();

        let current_text = "
            aaa
            new1
            new2
            new3
            new4
            zzz
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [
                    (Point::new(2, 0)..Point::new(3, 0), ""),
                    (Point::new(4, 0)..Point::new(5, 0), ""),
                ],
                None,
                cx,
            );
        });
        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            new1
            new3
            § spacer
            § spacer
            zzz"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            old1
            old2
            old3
            old4
            zzz"
            .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            new1
            new3
            § spacer
            § spacer
            zzz"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            old1
            old2
            old3
            old4
            zzz"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_soft_wrap_at_end_of_excerpt(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let text = "aaaa bbbb cccc dddd eeee ffff";

        let (buffer1, diff1) = buffer_with_diff(text, text, &mut cx);
        let (buffer2, diff2) = buffer_with_diff(text, text, &mut cx);

        editor.update(cx, |editor, cx| {
            let end = Point::new(0, text.len() as u32);
            let path1 = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path1,
                buffer1.clone(),
                vec![Point::new(0, 0)..end],
                0,
                diff1.clone(),
                cx,
            );
            let path2 = PathKey::sorted(1);
            editor.update_excerpts_for_path(
                path2,
                buffer2.clone(),
                vec![Point::new(0, 0)..end],
                0,
                diff2.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(200.0),
            px(400.0),
            "
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff"
                .unindent(),
            "
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            § spacer
            § spacer
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            § spacer
            § spacer"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_soft_wrap_before_modification_hunk(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaaa bbbb cccc dddd eeee ffff
            old line one
            old line two
        "
        .unindent();

        let current_text = "
            aaaa bbbb cccc dddd eeee ffff
            new line
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(200.0),
            px(400.0),
            "
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff
            new line
            § spacer"
                .unindent(),
            "
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            § spacer
            § spacer
            old line one
            old line two"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_soft_wrap_before_deletion_hunk(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaaa bbbb cccc dddd eeee ffff
            deleted line one
            deleted line two
            after
        "
        .unindent();

        let current_text = "
            aaaa bbbb cccc dddd eeee ffff
            after
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(400.0),
            px(200.0),
            "
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            § spacer
            § spacer
            § spacer
            § spacer
            § spacer
            § spacer
            after"
                .unindent(),
            "
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff
            deleted line\x20
            one
            deleted line\x20
            two
            after"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_soft_wrap_spacer_after_editing_second_line(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let text = "
            aaaa bbbb cccc dddd eeee ffff
            short
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&text, &text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(400.0),
            px(200.0),
            "
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            § spacer
            § spacer
            short"
                .unindent(),
            "
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff
            short"
                .unindent(),
            &mut cx,
        );

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(1, 0)..Point::new(1, 5), "modified")], None, cx);
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(400.0),
            px(200.0),
            "
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            § spacer
            § spacer
            modified"
                .unindent(),
            "
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff
            short"
                .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(400.0),
            px(200.0),
            "
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            § spacer
            § spacer
            modified"
                .unindent(),
            "
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff
            short"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_no_base_text(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let (buffer1, diff1) = buffer_with_diff("xxx\nyyy", "xxx\nyyy", &mut cx);

        let current_text = "
            aaa
            bbb
            ccc
        "
        .unindent();

        let buffer2 = cx.new(|cx| Buffer::local(current_text.to_string(), cx));
        let diff2 = cx.new(|cx| BufferDiff::new(&buffer2.read(cx).text_snapshot(), None, None, cx));

        editor.update(cx, |editor, cx| {
            let path1 = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path1,
                buffer1.clone(),
                vec![Point::new(0, 0)..buffer1.read(cx).max_point()],
                0,
                diff1.clone(),
                cx,
            );

            let path2 = PathKey::sorted(1);
            editor.update_excerpts_for_path(
                path2,
                buffer2.clone(),
                vec![Point::new(0, 0)..buffer2.read(cx).max_point()],
                1,
                diff2.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            xxx
            yyy
            § <no file>
            § -----
            aaa
            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            xxx
            yyy
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer"
                .unindent(),
            &mut cx,
        );

        buffer1.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(0, 3)..Point::new(0, 3), "z")], None, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            xxxz
            yyy
            § <no file>
            § -----
            aaa
            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            xxx
            yyy
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_deleting_char_in_added_line(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
            ccc
        "
        .unindent();

        let current_text = "
            NEW1
            NEW2
            ccc
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            NEW1
            NEW2
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc"
            .unindent(),
            &mut cx,
        );

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(1, 3)..Point::new(1, 4), "")], None, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            NEW1
            NEW
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_soft_wrap_spacer_before_added_line(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "aaaa bbbb cccc dddd eeee ffff\n";

        let current_text = "
            aaaa bbbb cccc dddd eeee ffff
            added line
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(400.0),
            px(200.0),
            "
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            § spacer
            § spacer
            added line"
                .unindent(),
            "
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff
            § spacer"
                .unindent(),
            &mut cx,
        );

        assert_split_content_with_widths(
            &editor,
            px(200.0),
            px(400.0),
            "
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff
            added line"
                .unindent(),
            "
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            § spacer
            § spacer
            § spacer"
                .unindent(),
            &mut cx,
        );
    }

    // Upstream added this test already ignored, in the same commit that introduced
    // the split diff (618f848c1f): it encodes a desired alignment that was never
    // implemented. Joining an added line into the unmodified line below it renders
    // `NEWeee` at the top of the deleted block followed by three spacers; the test
    // asserts the spacers come first so the joined line stays on the unmodified
    // line's original row.
    #[gpui::test]
    #[ignore = "asserts an unimplemented alignment: the joined line renders above the spacers, not below"]
    async fn test_joining_added_line_with_unmodified_line(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
            ccc
            ddd
            eee
        "
        .unindent();

        let current_text = "
            aaa
            NEW
            eee
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            NEW
            § spacer
            § spacer
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            &mut cx,
        );

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(1, 3)..Point::new(2, 0), "")], None, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            § spacer
            § spacer
            § spacer
            NEWeee"
                .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            NEWeee
            § spacer
            § spacer
            § spacer"
                .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_added_file_at_end(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "";
        let current_text = "
            aaaa bbbb cccc dddd eeee ffff
            bbb
            ccc
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaaa bbbb cccc dddd eeee ffff
            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer"
                .unindent(),
            &mut cx,
        );

        assert_split_content_with_widths(
            &editor,
            px(200.0),
            px(200.0),
            "
            § <no file>
            § -----
            aaaa bbbb\x20
            cccc dddd\x20
            eeee ffff
            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer
            § spacer
            § spacer"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_adding_line_to_addition_hunk(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
            ccc
        "
        .unindent();

        let current_text = "
            aaa
            bbb
            xxx
            yyy
            ccc
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            xxx
            yyy
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            § spacer
            § spacer
            ccc"
            .unindent(),
            &mut cx,
        );

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(3, 3)..Point::new(3, 3), "\nzzz")], None, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            xxx
            yyy
            zzz
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            § spacer
            § spacer
            § spacer
            ccc"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_scrolling(cx: &mut gpui::TestAppContext) {
        use crate::test::editor_content_with_blocks_and_size;
        use gpui::size;
        use rope::Point;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let long_line = "x".repeat(200);
        let mut lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        lines[25] = long_line;
        let content = lines.join("\n");

        let (buffer, diff) = buffer_with_diff(&content, &content, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        let (rhs_editor, lhs_editor) = editor.update(cx, |editor, _cx| {
            let lhs = editor.lhs.as_ref().expect("should have lhs editor");
            (editor.rhs_editor.clone(), lhs.editor.clone())
        });

        rhs_editor.update_in(cx, |e, window, cx| {
            e.set_scroll_position(gpui::Point::new(0., 10.), window, cx);
        });

        let rhs_pos =
            rhs_editor.update_in(cx, |e, window, cx| e.snapshot(window, cx).scroll_position());
        let lhs_pos =
            lhs_editor.update_in(cx, |e, window, cx| e.snapshot(window, cx).scroll_position());
        assert_eq!(rhs_pos.y, 10., "RHS should be scrolled to row 10");
        assert_eq!(
            lhs_pos.y, rhs_pos.y,
            "LHS should have same scroll position as RHS after set_scroll_position"
        );

        let draw_size = size(px(300.), px(300.));

        rhs_editor.update_in(cx, |e, window, cx| {
            e.change_selections(Some(crate::Autoscroll::fit()).into(), window, cx, |s| {
                s.select_ranges([Point::new(25, 150)..Point::new(25, 150)]);
            });
        });

        let _ = editor_content_with_blocks_and_size(&rhs_editor, draw_size, &mut cx);
        cx.run_until_parked();
        let _ = editor_content_with_blocks_and_size(&lhs_editor, draw_size, &mut cx);
        cx.run_until_parked();

        let rhs_pos =
            rhs_editor.update_in(cx, |e, window, cx| e.snapshot(window, cx).scroll_position());
        let lhs_pos =
            lhs_editor.update_in(cx, |e, window, cx| e.snapshot(window, cx).scroll_position());

        assert!(
            rhs_pos.y > 0.,
            "RHS should have scrolled vertically to show cursor at row 25"
        );
        assert!(
            rhs_pos.x > 0.,
            "RHS should have scrolled horizontally to show cursor at column 150"
        );
        assert_eq!(
            lhs_pos.y, rhs_pos.y,
            "LHS should have same vertical scroll position as RHS after autoscroll"
        );
        assert_eq!(
            lhs_pos.x, rhs_pos.x,
            "LHS should have same horizontal scroll position as RHS after autoscroll"
        )
    }

    #[gpui::test]
    async fn test_edit_line_before_soft_wrapped_line_preceding_hunk(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            first line
            aaaa bbbb cccc dddd eeee ffff
            original
        "
        .unindent();

        let current_text = "
            first line
            aaaa bbbb cccc dddd eeee ffff
            modified
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(400.0),
            px(200.0),
            "
                    § <no file>
                    § -----
                    first line
                    aaaa bbbb cccc dddd eeee ffff
                    § spacer
                    § spacer
                    modified"
                .unindent(),
            "
                    § <no file>
                    § -----
                    first line
                    aaaa bbbb\x20
                    cccc dddd\x20
                    eeee ffff
                    original"
                .unindent(),
            &mut cx,
        );

        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(Point::new(0, 0)..Point::new(0, 10), "edited first")],
                None,
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(400.0),
            px(200.0),
            "
                    § <no file>
                    § -----
                    edited first
                    aaaa bbbb cccc dddd eeee ffff
                    § spacer
                    § spacer
                    modified"
                .unindent(),
            "
                    § <no file>
                    § -----
                    first line
                    aaaa bbbb\x20
                    cccc dddd\x20
                    eeee ffff
                    original"
                .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content_with_widths(
            &editor,
            px(400.0),
            px(200.0),
            "
                    § <no file>
                    § -----
                    edited first
                    aaaa bbbb cccc dddd eeee ffff
                    § spacer
                    § spacer
                    modified"
                .unindent(),
            "
                    § <no file>
                    § -----
                    first line
                    aaaa bbbb\x20
                    cccc dddd\x20
                    eeee ffff
                    original"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_custom_block_sync_between_split_views(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "
            bbb
            ccc
        "
        .unindent();
        let current_text = "
            aaa
            bbb
            ccc
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc"
            .unindent(),
            &mut cx,
        );

        let block_ids = editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                let snapshot = rhs_editor.buffer().read(cx).snapshot(cx);
                let anchor = snapshot.anchor_before(Point::new(2, 0));
                rhs_editor.insert_blocks(
                    [BlockProperties {
                        placement: BlockPlacement::Above(anchor),
                        height: Some(1),
                        style: BlockStyle::Fixed,
                        render: Arc::new(|_| div().into_any()),
                        priority: 0,
                    }],
                    None,
                    cx,
                )
            })
        });

        let rhs_editor = editor.read_with(cx, |editor, _| editor.rhs_editor.clone());
        let lhs_editor =
            editor.read_with(cx, |editor, _| editor.lhs.as_ref().unwrap().editor.clone());

        cx.update(|_, cx| {
            set_block_content_for_tests(&rhs_editor, block_ids[0], cx, |_| {
                "custom block".to_string()
            });
        });

        let lhs_block_id = lhs_editor.read_with(cx, |lhs_editor, cx| {
            let display_map = lhs_editor.display_map.read(cx);
            let companion = display_map.companion().unwrap().read(cx);
            let mapping = companion
                .custom_block_to_balancing_block(rhs_editor.read(cx).display_map.entity_id());
            *mapping.borrow().get(&block_ids[0]).unwrap()
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&lhs_editor, lhs_block_id, cx, |_| {
                "custom block".to_string()
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            § custom block
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            § custom block
            ccc"
            .unindent(),
            &mut cx,
        );

        editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                rhs_editor.remove_blocks(HashSet::from_iter(block_ids), None, cx);
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_custom_block_deletion_and_resplit_sync(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "
            bbb
            ccc
        "
        .unindent();
        let current_text = "
            aaa
            bbb
            ccc
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc"
            .unindent(),
            &mut cx,
        );

        let block_ids = editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                let snapshot = rhs_editor.buffer().read(cx).snapshot(cx);
                let anchor1 = snapshot.anchor_before(Point::new(2, 0));
                let anchor2 = snapshot.anchor_before(Point::new(3, 0));
                rhs_editor.insert_blocks(
                    [
                        BlockProperties {
                            placement: BlockPlacement::Above(anchor1),
                            height: Some(1),
                            style: BlockStyle::Fixed,
                            render: Arc::new(|_| div().into_any()),
                            priority: 0,
                        },
                        BlockProperties {
                            placement: BlockPlacement::Above(anchor2),
                            height: Some(1),
                            style: BlockStyle::Fixed,
                            render: Arc::new(|_| div().into_any()),
                            priority: 0,
                        },
                    ],
                    None,
                    cx,
                )
            })
        });

        let rhs_editor = editor.read_with(cx, |editor, _| editor.rhs_editor.clone());
        let lhs_editor =
            editor.read_with(cx, |editor, _| editor.lhs.as_ref().unwrap().editor.clone());

        cx.update(|_, cx| {
            set_block_content_for_tests(&rhs_editor, block_ids[0], cx, |_| {
                "custom block 1".to_string()
            });
            set_block_content_for_tests(&rhs_editor, block_ids[1], cx, |_| {
                "custom block 2".to_string()
            });
        });

        let (lhs_block_id_1, lhs_block_id_2) = lhs_editor.read_with(cx, |lhs_editor, cx| {
            let display_map = lhs_editor.display_map.read(cx);
            let companion = display_map.companion().unwrap().read(cx);
            let mapping = companion
                .custom_block_to_balancing_block(rhs_editor.read(cx).display_map.entity_id());
            (
                *mapping.borrow().get(&block_ids[0]).unwrap(),
                *mapping.borrow().get(&block_ids[1]).unwrap(),
            )
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&lhs_editor, lhs_block_id_1, cx, |_| {
                "custom block 1".to_string()
            });
            set_block_content_for_tests(&lhs_editor, lhs_block_id_2, cx, |_| {
                "custom block 2".to_string()
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            § custom block 1
            ccc
            § custom block 2"
                .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            § custom block 1
            ccc
            § custom block 2"
                .unindent(),
            &mut cx,
        );

        editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                rhs_editor.remove_blocks(HashSet::from_iter([block_ids[0]]), None, cx);
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            § custom block 2"
                .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc
            § custom block 2"
                .unindent(),
            &mut cx,
        );

        editor.update_in(cx, |splittable_editor, window, cx| {
            splittable_editor.unsplit(window, cx);
        });

        cx.run_until_parked();

        editor.update_in(cx, |splittable_editor, window, cx| {
            splittable_editor.split(window, cx);
        });

        cx.run_until_parked();

        let lhs_editor =
            editor.read_with(cx, |editor, _| editor.lhs.as_ref().unwrap().editor.clone());

        let lhs_block_id_2 = lhs_editor.read_with(cx, |lhs_editor, cx| {
            let display_map = lhs_editor.display_map.read(cx);
            let companion = display_map.companion().unwrap().read(cx);
            let mapping = companion
                .custom_block_to_balancing_block(rhs_editor.read(cx).display_map.entity_id());
            *mapping.borrow().get(&block_ids[1]).unwrap()
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&lhs_editor, lhs_block_id_2, cx, |_| {
                "custom block 2".to_string()
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            § custom block 2"
                .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc
            § custom block 2"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_custom_block_sync_with_unsplit_start(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "
            bbb
            ccc
        "
        .unindent();
        let current_text = "
            aaa
            bbb
            ccc
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        editor.update_in(cx, |splittable_editor, window, cx| {
            splittable_editor.unsplit(window, cx);
        });

        cx.run_until_parked();

        let rhs_editor = editor.read_with(cx, |editor, _| editor.rhs_editor.clone());

        let block_ids = editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                let snapshot = rhs_editor.buffer().read(cx).snapshot(cx);
                let anchor1 = snapshot.anchor_before(Point::new(2, 0));
                let anchor2 = snapshot.anchor_before(Point::new(3, 0));
                rhs_editor.insert_blocks(
                    [
                        BlockProperties {
                            placement: BlockPlacement::Above(anchor1),
                            height: Some(1),
                            style: BlockStyle::Fixed,
                            render: Arc::new(|_| div().into_any()),
                            priority: 0,
                        },
                        BlockProperties {
                            placement: BlockPlacement::Above(anchor2),
                            height: Some(1),
                            style: BlockStyle::Fixed,
                            render: Arc::new(|_| div().into_any()),
                            priority: 0,
                        },
                    ],
                    None,
                    cx,
                )
            })
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&rhs_editor, block_ids[0], cx, |_| {
                "custom block 1".to_string()
            });
            set_block_content_for_tests(&rhs_editor, block_ids[1], cx, |_| {
                "custom block 2".to_string()
            });
        });

        cx.run_until_parked();

        let rhs_content = editor_content_with_blocks_and_width(&rhs_editor, px(3000.0), &mut cx);
        assert_eq!(
            rhs_content,
            "
            § <no file>
            § -----
            aaa
            bbb
            § custom block 1
            ccc
            § custom block 2"
                .unindent(),
            "rhs content before split"
        );

        editor.update_in(cx, |splittable_editor, window, cx| {
            splittable_editor.split(window, cx);
        });

        cx.run_until_parked();

        let lhs_editor =
            editor.read_with(cx, |editor, _| editor.lhs.as_ref().unwrap().editor.clone());

        let (lhs_block_id_1, lhs_block_id_2) = lhs_editor.read_with(cx, |lhs_editor, cx| {
            let display_map = lhs_editor.display_map.read(cx);
            let companion = display_map.companion().unwrap().read(cx);
            let mapping = companion
                .custom_block_to_balancing_block(rhs_editor.read(cx).display_map.entity_id());
            (
                *mapping.borrow().get(&block_ids[0]).unwrap(),
                *mapping.borrow().get(&block_ids[1]).unwrap(),
            )
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&lhs_editor, lhs_block_id_1, cx, |_| {
                "custom block 1".to_string()
            });
            set_block_content_for_tests(&lhs_editor, lhs_block_id_2, cx, |_| {
                "custom block 2".to_string()
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            § custom block 1
            ccc
            § custom block 2"
                .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            § custom block 1
            ccc
            § custom block 2"
                .unindent(),
            &mut cx,
        );

        editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                rhs_editor.remove_blocks(HashSet::from_iter([block_ids[0]]), None, cx);
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            § custom block 2"
                .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc
            § custom block 2"
                .unindent(),
            &mut cx,
        );

        editor.update_in(cx, |splittable_editor, window, cx| {
            splittable_editor.unsplit(window, cx);
        });

        cx.run_until_parked();

        editor.update_in(cx, |splittable_editor, window, cx| {
            splittable_editor.split(window, cx);
        });

        cx.run_until_parked();

        let lhs_editor =
            editor.read_with(cx, |editor, _| editor.lhs.as_ref().unwrap().editor.clone());

        let lhs_block_id_2 = lhs_editor.read_with(cx, |lhs_editor, cx| {
            let display_map = lhs_editor.display_map.read(cx);
            let companion = display_map.companion().unwrap().read(cx);
            let mapping = companion
                .custom_block_to_balancing_block(rhs_editor.read(cx).display_map.entity_id());
            *mapping.borrow().get(&block_ids[1]).unwrap()
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&lhs_editor, lhs_block_id_2, cx, |_| {
                "custom block 2".to_string()
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            § custom block 2"
                .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc
            § custom block 2"
                .unindent(),
            &mut cx,
        );

        let new_block_ids = editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                let snapshot = rhs_editor.buffer().read(cx).snapshot(cx);
                let anchor = snapshot.anchor_before(Point::new(2, 0));
                rhs_editor.insert_blocks(
                    [BlockProperties {
                        placement: BlockPlacement::Above(anchor),
                        height: Some(1),
                        style: BlockStyle::Fixed,
                        render: Arc::new(|_| div().into_any()),
                        priority: 0,
                    }],
                    None,
                    cx,
                )
            })
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&rhs_editor, new_block_ids[0], cx, |_| {
                "custom block 3".to_string()
            });
        });

        let lhs_block_id_3 = lhs_editor.read_with(cx, |lhs_editor, cx| {
            let display_map = lhs_editor.display_map.read(cx);
            let companion = display_map.companion().unwrap().read(cx);
            let mapping = companion
                .custom_block_to_balancing_block(rhs_editor.read(cx).display_map.entity_id());
            *mapping.borrow().get(&new_block_ids[0]).unwrap()
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&lhs_editor, lhs_block_id_3, cx, |_| {
                "custom block 3".to_string()
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            § custom block 3
            ccc
            § custom block 2"
                .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            § custom block 3
            ccc
            § custom block 2"
                .unindent(),
            &mut cx,
        );

        editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                rhs_editor.remove_blocks(HashSet::from_iter([new_block_ids[0]]), None, cx);
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            § custom block 2"
                .unindent(),
            "
            § <no file>
            § -----
            § spacer
            bbb
            ccc
            § custom block 2"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_buffer_folding_sync(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Unified).await;

        let base_text1 = "
            aaa
            bbb
            ccc"
        .unindent();
        let current_text1 = "
            aaa
            bbb
            ccc"
        .unindent();

        let base_text2 = "
            ddd
            eee
            fff"
        .unindent();
        let current_text2 = "
            ddd
            eee
            fff"
        .unindent();

        let (buffer1, diff1) = buffer_with_diff(&base_text1, &current_text1, &mut cx);
        let (buffer2, diff2) = buffer_with_diff(&base_text2, &current_text2, &mut cx);

        let buffer1_id = buffer1.read_with(cx, |buffer, _| buffer.remote_id());
        let buffer2_id = buffer2.read_with(cx, |buffer, _| buffer.remote_id());

        editor.update(cx, |editor, cx| {
            editor.update_excerpts_for_path(
                PathKey::sorted(0),
                buffer1.clone(),
                vec![Point::new(0, 0)..buffer1.read(cx).max_point()],
                0,
                diff1.clone(),
                cx,
            );
            editor.update_excerpts_for_path(
                PathKey::sorted(1),
                buffer2.clone(),
                vec![Point::new(0, 0)..buffer2.read(cx).max_point()],
                1,
                diff2.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        editor.update(cx, |editor, cx| {
            editor.rhs_editor.update(cx, |rhs_editor, cx| {
                rhs_editor.fold_buffer(buffer1_id, cx);
            });
        });

        cx.run_until_parked();

        let rhs_buffer1_folded = editor.read_with(cx, |editor, cx| {
            editor.rhs_editor.read(cx).is_buffer_folded(buffer1_id, cx)
        });
        assert!(
            rhs_buffer1_folded,
            "buffer1 should be folded in rhs before split"
        );

        editor.update_in(cx, |editor, window, cx| {
            editor.split(window, cx);
        });

        cx.run_until_parked();

        let (rhs_editor, lhs_editor) = editor.read_with(cx, |editor, _cx| {
            (
                editor.rhs_editor.clone(),
                editor.lhs.as_ref().unwrap().editor.clone(),
            )
        });

        let rhs_buffer1_folded =
            rhs_editor.read_with(cx, |editor, cx| editor.is_buffer_folded(buffer1_id, cx));
        assert!(
            rhs_buffer1_folded,
            "buffer1 should be folded in rhs after split"
        );

        let base_buffer1_id = diff1.read_with(cx, |diff, cx| diff.base_text(cx).remote_id());
        let lhs_buffer1_folded = lhs_editor.read_with(cx, |editor, cx| {
            editor.is_buffer_folded(base_buffer1_id, cx)
        });
        assert!(
            lhs_buffer1_folded,
            "buffer1 should be folded in lhs after split"
        );

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            § <no file>
            § -----
            ddd
            eee
            fff"
            .unindent(),
            "
            § <no file>
            § -----
            § <no file>
            § -----
            ddd
            eee
            fff"
            .unindent(),
            &mut cx,
        );

        editor.update(cx, |editor, cx| {
            editor.rhs_editor.update(cx, |rhs_editor, cx| {
                rhs_editor.fold_buffer(buffer2_id, cx);
            });
        });

        cx.run_until_parked();

        let rhs_buffer2_folded =
            rhs_editor.read_with(cx, |editor, cx| editor.is_buffer_folded(buffer2_id, cx));
        assert!(rhs_buffer2_folded, "buffer2 should be folded in rhs");

        let base_buffer2_id = diff2.read_with(cx, |diff, cx| diff.base_text(cx).remote_id());
        let lhs_buffer2_folded = lhs_editor.read_with(cx, |editor, cx| {
            editor.is_buffer_folded(base_buffer2_id, cx)
        });
        assert!(lhs_buffer2_folded, "buffer2 should be folded in lhs");

        let rhs_buffer1_still_folded =
            rhs_editor.read_with(cx, |editor, cx| editor.is_buffer_folded(buffer1_id, cx));
        assert!(
            rhs_buffer1_still_folded,
            "buffer1 should still be folded in rhs"
        );

        let lhs_buffer1_still_folded = lhs_editor.read_with(cx, |editor, cx| {
            editor.is_buffer_folded(base_buffer1_id, cx)
        });
        assert!(
            lhs_buffer1_still_folded,
            "buffer1 should still be folded in lhs"
        );

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            § <no file>
            § -----"
                .unindent(),
            "
            § <no file>
            § -----
            § <no file>
            § -----"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_custom_block_in_middle_of_added_hunk(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "
            ddd
            eee
        "
        .unindent();
        let current_text = "
            aaa
            bbb
            ccc
            ddd
            eee
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer
            ddd
            eee"
            .unindent(),
            &mut cx,
        );

        let block_ids = editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                let snapshot = rhs_editor.buffer().read(cx).snapshot(cx);
                let anchor = snapshot.anchor_before(Point::new(2, 0));
                rhs_editor.insert_blocks(
                    [BlockProperties {
                        placement: BlockPlacement::Above(anchor),
                        height: Some(1),
                        style: BlockStyle::Fixed,
                        render: Arc::new(|_| div().into_any()),
                        priority: 0,
                    }],
                    None,
                    cx,
                )
            })
        });

        let rhs_editor = editor.read_with(cx, |editor, _| editor.rhs_editor.clone());
        let lhs_editor =
            editor.read_with(cx, |editor, _| editor.lhs.as_ref().unwrap().editor.clone());

        cx.update(|_, cx| {
            set_block_content_for_tests(&rhs_editor, block_ids[0], cx, |_| {
                "custom block".to_string()
            });
        });

        let lhs_block_id = lhs_editor.read_with(cx, |lhs_editor, cx| {
            let display_map = lhs_editor.display_map.read(cx);
            let companion = display_map.companion().unwrap().read(cx);
            let mapping = companion
                .custom_block_to_balancing_block(rhs_editor.read(cx).display_map.entity_id());
            *mapping.borrow().get(&block_ids[0]).unwrap()
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&lhs_editor, lhs_block_id, cx, |_| {
                "custom block".to_string()
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            § custom block
            ccc
            ddd
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer
            § custom block
            ddd
            eee"
            .unindent(),
            &mut cx,
        );

        editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                rhs_editor.remove_blocks(HashSet::from_iter(block_ids), None, cx);
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer
            ddd
            eee"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_custom_block_below_in_middle_of_added_hunk(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "
            ddd
            eee
        "
        .unindent();
        let current_text = "
            aaa
            bbb
            ccc
            ddd
            eee
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer
            ddd
            eee"
            .unindent(),
            &mut cx,
        );

        let block_ids = editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                let snapshot = rhs_editor.buffer().read(cx).snapshot(cx);
                let anchor = snapshot.anchor_after(Point::new(1, 3));
                rhs_editor.insert_blocks(
                    [BlockProperties {
                        placement: BlockPlacement::Below(anchor),
                        height: Some(1),
                        style: BlockStyle::Fixed,
                        render: Arc::new(|_| div().into_any()),
                        priority: 0,
                    }],
                    None,
                    cx,
                )
            })
        });

        let rhs_editor = editor.read_with(cx, |editor, _| editor.rhs_editor.clone());
        let lhs_editor =
            editor.read_with(cx, |editor, _| editor.lhs.as_ref().unwrap().editor.clone());

        cx.update(|_, cx| {
            set_block_content_for_tests(&rhs_editor, block_ids[0], cx, |_| {
                "custom block".to_string()
            });
        });

        let lhs_block_id = lhs_editor.read_with(cx, |lhs_editor, cx| {
            let display_map = lhs_editor.display_map.read(cx);
            let companion = display_map.companion().unwrap().read(cx);
            let mapping = companion
                .custom_block_to_balancing_block(rhs_editor.read(cx).display_map.entity_id());
            *mapping.borrow().get(&block_ids[0]).unwrap()
        });

        cx.update(|_, cx| {
            set_block_content_for_tests(&lhs_editor, lhs_block_id, cx, |_| {
                "custom block".to_string()
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            § custom block
            ccc
            ddd
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer
            § custom block
            ddd
            eee"
            .unindent(),
            &mut cx,
        );

        editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                rhs_editor.remove_blocks(HashSet::from_iter(block_ids), None, cx);
            });
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc
            ddd
            eee"
            .unindent(),
            "
            § <no file>
            § -----
            § spacer
            § spacer
            § spacer
            ddd
            eee"
            .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_custom_block_resize_syncs_balancing_block(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "
            bbb
            ccc
        "
        .unindent();
        let current_text = "
            aaa
            bbb
            ccc
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        let block_ids = editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                let snapshot = rhs_editor.buffer().read(cx).snapshot(cx);
                let anchor = snapshot.anchor_before(Point::new(2, 0));
                rhs_editor.insert_blocks(
                    [BlockProperties {
                        placement: BlockPlacement::Above(anchor),
                        height: Some(1),
                        style: BlockStyle::Fixed,
                        render: Arc::new(|_| div().into_any()),
                        priority: 0,
                    }],
                    None,
                    cx,
                )
            })
        });

        let rhs_editor = editor.read_with(cx, |editor, _| editor.rhs_editor.clone());
        let lhs_editor =
            editor.read_with(cx, |editor, _| editor.lhs.as_ref().unwrap().editor.clone());

        let lhs_block_id = lhs_editor.read_with(cx, |lhs_editor, cx| {
            let display_map = lhs_editor.display_map.read(cx);
            let companion = display_map.companion().unwrap().read(cx);
            let mapping = companion
                .custom_block_to_balancing_block(rhs_editor.read(cx).display_map.entity_id());
            *mapping.borrow().get(&block_ids[0]).unwrap()
        });

        cx.run_until_parked();

        let get_block_height = |editor: &Entity<crate::Editor>,
                                block_id: crate::CustomBlockId,
                                cx: &mut VisualTestContext| {
            editor.update_in(cx, |editor, window, cx| {
                let snapshot = editor.snapshot(window, cx);
                snapshot
                    .block_for_id(crate::BlockId::Custom(block_id))
                    .map(|block| block.height())
            })
        };

        assert_eq!(
            get_block_height(&rhs_editor, block_ids[0], &mut cx),
            Some(1)
        );
        assert_eq!(
            get_block_height(&lhs_editor, lhs_block_id, &mut cx),
            Some(1)
        );

        editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                let mut heights = HashMap::default();
                heights.insert(block_ids[0], 3);
                rhs_editor.resize_blocks(heights, None, cx);
            });
        });

        cx.run_until_parked();

        assert_eq!(
            get_block_height(&rhs_editor, block_ids[0], &mut cx),
            Some(3)
        );
        assert_eq!(
            get_block_height(&lhs_editor, lhs_block_id, &mut cx),
            Some(3)
        );

        editor.update(cx, |splittable_editor, cx| {
            splittable_editor.rhs_editor.update(cx, |rhs_editor, cx| {
                let mut heights = HashMap::default();
                heights.insert(block_ids[0], 5);
                rhs_editor.resize_blocks(heights, None, cx);
            });
        });

        cx.run_until_parked();

        assert_eq!(
            get_block_height(&rhs_editor, block_ids[0], &mut cx),
            Some(5)
        );
        assert_eq!(
            get_block_height(&lhs_editor, lhs_block_id, &mut cx),
            Some(5)
        );
    }

    #[gpui::test]
    async fn test_edit_spanning_excerpt_boundaries_then_resplit(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
            ccc
            ddd
            eee
            fff
            ggg
            hhh
            iii
            jjj
            kkk
            lll
        "
        .unindent();
        let current_text = base_text.clone();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![
                    Point::new(0, 0)..Point::new(3, 3),
                    Point::new(5, 0)..Point::new(8, 3),
                    Point::new(10, 0)..Point::new(11, 3),
                ],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(1, 0)..Point::new(10, 0), "")], None, cx);
        });

        cx.run_until_parked();

        editor.update_in(cx, |splittable_editor, window, cx| {
            splittable_editor.unsplit(window, cx);
        });

        cx.run_until_parked();

        editor.update_in(cx, |splittable_editor, window, cx| {
            splittable_editor.split(window, cx);
        });

        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_range_folds_removed_on_split(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Unified).await;

        let base_text = "
            aaa
            bbb
            ccc
            ddd
            eee"
        .unindent();
        let current_text = "
            aaa
            bbb
            ccc
            ddd
            eee"
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor.update(cx, |rhs_editor, cx| {
                rhs_editor.fold_creases(
                    vec![Crease::simple(
                        Point::new(1, 0)..Point::new(3, 0),
                        FoldPlaceholder::test(),
                    )],
                    false,
                    window,
                    cx,
                );
            });
        });

        cx.run_until_parked();

        editor.update_in(cx, |editor, window, cx| {
            editor.split(window, cx);
        });

        cx.run_until_parked();

        let (rhs_editor, lhs_editor) = editor.read_with(cx, |editor, _cx| {
            (
                editor.rhs_editor.clone(),
                editor.lhs.as_ref().unwrap().editor.clone(),
            )
        });

        let rhs_has_folds_after_split = rhs_editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            snapshot
                .folds_in_range(MultiBufferOffset(0)..snapshot.buffer_snapshot().len())
                .next()
                .is_some()
        });
        assert!(
            !rhs_has_folds_after_split,
            "rhs should not have range folds after split"
        );

        let lhs_has_folds = lhs_editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            snapshot
                .folds_in_range(MultiBufferOffset(0)..snapshot.buffer_snapshot().len())
                .next()
                .is_some()
        });
        assert!(!lhs_has_folds, "lhs should not have any range folds");
    }

    #[gpui::test]
    async fn test_multiline_inlays_create_spacers(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
            ccc
            ddd
        "
        .unindent();
        let current_text = base_text.clone();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..Point::new(3, 3)],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        let rhs_editor = editor.read_with(cx, |e, _| e.rhs_editor.clone());
        rhs_editor.update(cx, |rhs_editor, cx| {
            let snapshot = rhs_editor.buffer().read(cx).snapshot(cx);
            rhs_editor.splice_inlays(
                &[],
                vec![
                    Inlay::edit_prediction(
                        0,
                        snapshot.anchor_after(Point::new(0, 3)),
                        "\nINLAY_WITHIN",
                    ),
                    Inlay::edit_prediction(
                        1,
                        snapshot.anchor_after(Point::new(1, 3)),
                        "\nINLAY_MID_1\nINLAY_MID_2",
                    ),
                    Inlay::edit_prediction(
                        2,
                        snapshot.anchor_after(Point::new(3, 3)),
                        "\nINLAY_END_1\nINLAY_END_2",
                    ),
                ],
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            INLAY_WITHIN
            bbb
            INLAY_MID_1
            INLAY_MID_2
            ccc
            ddd
            INLAY_END_1
            INLAY_END_2"
                .unindent(),
            "
            § <no file>
            § -----
            aaa
            § spacer
            bbb
            § spacer
            § spacer
            ccc
            ddd
            § spacer
            § spacer"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_split_after_removing_folded_buffer(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Unified).await;

        let base_text_a = "
            aaa
            bbb
            ccc
        "
        .unindent();
        let current_text_a = "
            aaa
            bbb modified
            ccc
        "
        .unindent();

        let base_text_b = "
            xxx
            yyy
            zzz
        "
        .unindent();
        let current_text_b = "
            xxx
            yyy modified
            zzz
        "
        .unindent();

        let (buffer_a, diff_a) = buffer_with_diff(&base_text_a, &current_text_a, &mut cx);
        let (buffer_b, diff_b) = buffer_with_diff(&base_text_b, &current_text_b, &mut cx);

        let path_a = PathKey::sorted(0);
        let path_b = PathKey::sorted(1);

        editor.update(cx, |editor, cx| {
            editor.update_excerpts_for_path(
                path_a.clone(),
                buffer_a.clone(),
                vec![Point::new(0, 0)..buffer_a.read(cx).max_point()],
                0,
                diff_a.clone(),
                cx,
            );
            editor.update_excerpts_for_path(
                path_b.clone(),
                buffer_b.clone(),
                vec![Point::new(0, 0)..buffer_b.read(cx).max_point()],
                0,
                diff_b.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        let buffer_a_id = buffer_a.read_with(cx, |buffer, _| buffer.remote_id());
        editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |right_editor, cx| {
                right_editor.fold_buffer(buffer_a_id, cx)
            });
        });

        cx.run_until_parked();

        editor.update(cx, |editor, cx| {
            editor.remove_excerpts_for_path(path_a.clone(), cx);
        });
        cx.run_until_parked();

        editor.update_in(cx, |editor, window, cx| editor.split(window, cx));
        cx.run_until_parked();

        editor.update(cx, |editor, cx| {
            editor.update_excerpts_for_path(
                path_a.clone(),
                buffer_a.clone(),
                vec![Point::new(0, 0)..buffer_a.read(cx).max_point()],
                0,
                diff_a.clone(),
                cx,
            );
            assert!(
                !editor
                    .lhs_editor()
                    .unwrap()
                    .read(cx)
                    .is_buffer_folded(buffer_a_id, cx)
            );
            assert!(
                !editor
                    .rhs_editor()
                    .read(cx)
                    .is_buffer_folded(buffer_a_id, cx)
            );
        });
    }

    #[gpui::test]
    async fn test_two_path_keys_for_one_buffer(cx: &mut gpui::TestAppContext) {
        use multi_buffer::PathKey;
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
            ccc
        "
        .unindent();
        let current_text = "
            aaa
            bbb modified
            ccc
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        let path_key_1 = PathKey {
            sort_prefix: Some(0),
            path: rel_path("file1.txt").into(),
        };
        let path_key_2 = PathKey {
            sort_prefix: Some(1),
            path: rel_path("file1.txt").into(),
        };

        editor.update(cx, |editor, cx| {
            editor.update_excerpts_for_path(
                path_key_1.clone(),
                buffer.clone(),
                vec![Point::new(0, 0)..Point::new(1, 0)],
                0,
                diff.clone(),
                cx,
            );
            editor.update_excerpts_for_path(
                path_key_2.clone(),
                buffer.clone(),
                vec![Point::new(1, 0)..buffer.read(cx).max_point()],
                1,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_spacer_blocks_revert_after_temporary_edit(cx: &mut gpui::TestAppContext) {
        use rope::Point;
        use unindent::Unindent as _;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        let base_text = "
            aaa
            bbb
        "
        .unindent();
        let current_text = "
            aaa
            bbb
            ccc
        "
        .unindent();

        let (buffer, diff) = buffer_with_diff(&base_text, &current_text, &mut cx);

        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(
                path,
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            § spacer"
                .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(0, 3)..Point::new(0, 3), "\n")], None, cx);
            buffer.text_snapshot()
        });
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa

            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            § spacer
            bbb
            § spacer"
                .unindent(),
            &mut cx,
        );

        let buffer_snapshot = buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(0, 3)..Point::new(1, 0), "")], None, cx);
            buffer.text_snapshot()
        });
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });

        cx.run_until_parked();

        assert_split_content(
            &editor,
            "
            § <no file>
            § -----
            aaa
            bbb
            ccc"
            .unindent(),
            "
            § <no file>
            § -----
            aaa
            bbb
            § spacer"
                .unindent(),
            &mut cx,
        );
    }

    #[gpui::test]
    async fn test_act_as_type(cx: &mut gpui::TestAppContext) {
        let (splittable_editor, cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;
        let editor = splittable_editor.read_with(cx, |editor, cx| {
            editor.act_as_type(TypeId::of::<Editor>(), &splittable_editor, cx)
        });

        assert!(
            editor.is_some(),
            "SplittableEditor should be able to act as Editor"
        );
    }

    #[gpui::test]
    async fn test_connector_rows_pair_hunks_across_panes(cx: &mut gpui::TestAppContext) {
        use crate::split_connectors::connector_rows;
        use buffer_diff::DiffHunkStatusKind;

        let (editor, cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let base_text =
            "one\nTWO_OLD\nthree\nfour\nOLD_A\nOLD_B\nfive\nDELETED_1\nDELETED_2\nsix\n";
        let current_text = "one\nTWO_NEW_1\nTWO_NEW_2\nTWO_NEW_3\nthree\nfour\nNEW_A\nfive\nsix\n";
        let (buffer, diff) = buffer_with_diff(base_text, current_text, cx);

        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.recalculate_diff_sync(&buffer_snapshot, cx);
        });
        cx.run_until_parked();

        let diff_snapshot = diff.read_with(cx, |diff, cx| diff.snapshot(cx));
        let ranges = diff_snapshot
            .hunks(&buffer_snapshot)
            .map(|hunk| hunk.range)
            .collect::<Vec<_>>();
        editor.update(cx, |editor, cx| {
            let path = PathKey::sorted(0);
            editor.update_excerpts_for_path(path, buffer.clone(), ranges, 1, diff.clone(), cx);
        });
        cx.run_until_parked();

        let (rhs_editor, lhs_editor) = editor.update(cx, |editor, _| {
            let lhs = editor.lhs.as_ref().expect("should have lhs editor");
            (editor.rhs_editor.clone(), lhs.editor.clone())
        });

        // Force a layout on both panes so their display maps (and the spacer
        // blocks that keep the two row counts equal) are up to date.
        let _ = editor_content_with_blocks_and_width(&rhs_editor, px(3000.), cx);
        let _ = editor_content_with_blocks_and_width(&lhs_editor, px(3000.), cx);
        cx.run_until_parked();

        let lhs_snapshot =
            lhs_editor.update_in(cx, |editor, window, cx| editor.snapshot(window, cx));
        let rhs_snapshot =
            rhs_editor.update_in(cx, |editor, window, cx| editor.snapshot(window, cx));

        let connectors = connector_rows(&lhs_snapshot, &rhs_snapshot, 1000.);
        assert_eq!(
            connectors.len(),
            3,
            "expected one connector per hunk, got {connectors:?}"
        );

        for connector in &connectors {
            assert_eq!(
                connector.left.start, connector.right.start,
                "hunk tops must align across panes: {connector:?}"
            );
        }

        let row_counts = connectors
            .iter()
            .map(|connector| {
                (
                    connector.left.end.0 - connector.left.start.0,
                    connector.right.end.0 - connector.right.start.0,
                    connector.status,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            row_counts,
            vec![
                (1, 3, DiffHunkStatusKind::Modified),
                (2, 1, DiffHunkStatusKind::Modified),
                (2, 0, DiffHunkStatusKind::Deleted),
            ]
        );
    }

    /// A split pane derives its alignment spacers from the companion's wrap
    /// rows, so changing either pane's wrap width must notify the companion at
    /// all — otherwise nothing ever asks for the frame that reconciles them.
    ///
    /// Scope, because it is narrower than it looks: this drives frames through
    /// `VisualTestContext::draw`, which does not run gpui's
    /// `record_entities_accessed`, so the panes are never registered as live
    /// window entities and the draw-phase gate that swallows invalidations
    /// raised during layout never applies here. It therefore cannot tell an
    /// inline notify (dropped in the real app) from a deferred one — that is
    /// what `test_split_panes_repaint_after_wrap_width_change` is for.
    #[gpui::test]
    async fn test_split_alignment_survives_wrap_width_change(cx: &mut gpui::TestAppContext) {
        use crate::RowExt as _;
        use rope::Point;
        use std::cell::Cell;
        use std::rc::Rc;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        // One long unchanged line per excerpt: how many display rows it takes
        // depends purely on the pane's wrap width, so whichever pane keeps it on
        // fewer rows needs spacers to stay level with the other one.
        let text = "aaaa bbbb cccc dddd eeee ffff";
        let (buffer1, diff1) = buffer_with_diff(text, text, &mut cx);
        let (buffer2, diff2) = buffer_with_diff(text, text, &mut cx);

        editor.update(cx, |editor, cx| {
            let end = Point::new(0, text.len() as u32);
            editor.update_excerpts_for_path(
                PathKey::sorted(0),
                buffer1.clone(),
                vec![Point::new(0, 0)..end],
                0,
                diff1.clone(),
                cx,
            );
            editor.update_excerpts_for_path(
                PathKey::sorted(1),
                buffer2.clone(),
                vec![Point::new(0, 0)..end],
                0,
                diff2.clone(),
                cx,
            );
        });
        cx.run_until_parked();

        let (rhs_editor, lhs_editor) = editor.update(cx, |editor, _| {
            let lhs = editor.lhs.as_ref().expect("should have lhs editor");
            (editor.rhs_editor.clone(), lhs.editor.clone())
        });

        let redraw_requested = Rc::new(Cell::new(false));
        let _observers = cx.update(|_, cx| {
            [&lhs_editor, &rhs_editor]
                .into_iter()
                .map(|pane| {
                    let redraw_requested = redraw_requested.clone();
                    cx.observe(pane, move |_, _| redraw_requested.set(true))
                })
                .collect::<Vec<_>>()
        });

        // Settle both panes at the same width: the line wraps identically on
        // both sides, so neither side needs a spacer.
        for _ in 0..2 {
            let _ = editor_content_with_blocks_and_width(&rhs_editor, px(400.), &mut cx);
            let _ = editor_content_with_blocks_and_width(&lhs_editor, px(400.), &mut cx);
        }
        cx.run_until_parked();

        fn second_excerpt_row(snapshot: &crate::EditorSnapshot) -> crate::DisplayRow {
            let max_row = snapshot.max_point().row();
            snapshot
                .blocks_in_range(crate::DisplayRow(0)..max_row.next_row())
                .filter(|(_, block)| {
                    matches!(block, crate::display_map::Block::BufferHeader { .. })
                })
                .map(|(row, _)| row)
                .nth(1)
                .expect("each pane should have a header for each of the two excerpts")
        }

        // Now drag the divider: the left pane narrows slightly (not enough to
        // rewrap) while the right pane narrows enough to wrap onto three rows.
        // Keep drawing for as long as the editor asks to be redrawn, then
        // require that the two panes agree about where the second excerpt's
        // unchanged line sits.
        let mut alignment = None;
        for _ in 0..8 {
            redraw_requested.set(false);

            let _ = editor_content_with_blocks_and_width(&lhs_editor, px(380.), &mut cx);
            let lhs_snapshot =
                lhs_editor.update_in(cx, |editor, window, cx| editor.snapshot(window, cx));
            let _ = editor_content_with_blocks_and_width(&rhs_editor, px(200.), &mut cx);
            let rhs_snapshot =
                rhs_editor.update_in(cx, |editor, window, cx| editor.snapshot(window, cx));

            alignment = Some((
                (
                    second_excerpt_row(&lhs_snapshot),
                    lhs_snapshot.max_point().row(),
                ),
                (
                    second_excerpt_row(&rhs_snapshot),
                    rhs_snapshot.max_point().row(),
                ),
            ));

            cx.run_until_parked();
            if !redraw_requested.get() {
                break;
            }
        }

        let (lhs, rhs) = alignment.expect("the loop runs at least once");
        assert_eq!(
            lhs.0, rhs.0,
            "the same unchanged line must sit on the same display row in both panes"
        );
        assert_eq!(
            lhs.1, rhs.1,
            "both panes must span the same number of display rows"
        );
    }

    /// Replaces the single excerpt with a fresh diff and reports where the
    /// "before" pane wants an insertion marker, together with its rendered
    /// content so a failure shows the rows the marker is being placed among.
    #[track_caller]
    fn lhs_insertion_markers(
        editor: &Entity<SplittableEditor>,
        base_text: &str,
        current_text: &str,
        cx: &mut VisualTestContext,
    ) -> (Vec<crate::DisplayRow>, String) {
        let (lhs_editor, _) = replace_excerpt_with_diff(editor, base_text, current_text, cx);
        let lhs_content = editor_content_with_blocks_and_width(&lhs_editor, px(3000.), cx);
        (markers_of(&lhs_editor, cx), lhs_content)
    }

    #[track_caller]
    fn replace_excerpt_with_diff(
        editor: &Entity<SplittableEditor>,
        base_text: &str,
        current_text: &str,
        cx: &mut VisualTestContext,
    ) -> (Entity<Editor>, Entity<Editor>) {
        use rope::Point;

        let (buffer, diff) = buffer_with_diff(base_text, current_text, cx);
        editor.update(cx, |editor, cx| {
            editor.update_excerpts_for_path(
                PathKey::sorted(0),
                buffer.clone(),
                vec![Point::new(0, 0)..buffer.read(cx).max_point()],
                0,
                diff.clone(),
                cx,
            );
        });
        cx.run_until_parked();

        let (rhs_editor, lhs_editor) = editor.update(cx, |editor, _| {
            let lhs = editor.lhs.as_ref().expect("should have lhs editor");
            (editor.rhs_editor.clone(), lhs.editor.clone())
        });

        // Lay both panes out so each learns the other's row count and grows the
        // spacers that balance them.
        let _ = editor_content_with_blocks_and_width(&rhs_editor, px(3000.), cx);
        let _ = editor_content_with_blocks_and_width(&lhs_editor, px(3000.), cx);
        cx.run_until_parked();

        (lhs_editor, rhs_editor)
    }

    #[track_caller]
    fn markers_of(pane: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<crate::DisplayRow> {
        use crate::RowExt as _;

        let snapshot = pane.update_in(cx, |editor, window, cx| editor.snapshot(window, cx));
        crate::split_connectors::insertion_marker_rows(
            &snapshot,
            crate::DisplayRow(0)..snapshot.max_point().row().next_row(),
        )
    }

    #[track_caller]
    fn display_row_of(
        pane: &Entity<Editor>,
        buffer_row: u32,
        cx: &mut VisualTestContext,
    ) -> crate::DisplayRow {
        let snapshot = pane.update_in(cx, |editor, window, cx| editor.snapshot(window, cx));
        snapshot
            .point_to_display_point(
                multi_buffer::MultiBufferPoint::new(buffer_row, 0),
                text::Bias::Left,
            )
            .row()
    }

    /// Lines that only exist on the right leave the left pane with a neutral
    /// hatched gap; the marker is what says the gap is an insertion, and it
    /// belongs at the top of that gap.
    #[gpui::test]
    async fn test_insertion_marker_marks_the_gap_in_the_before_pane(cx: &mut gpui::TestAppContext) {
        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let (markers, lhs_content) = lhs_insertion_markers(
            &editor,
            "line1\nline2\nline3\nline4\n",
            "line1\nline1.1\nline2\nline3\nline4\n",
            &mut cx,
        );

        assert_eq!(
            lhs_content,
            "§ <no file>\n§ -----\nline1\n§ spacer\nline2\nline3\nline4"
        );
        assert_eq!(markers, vec![crate::DisplayRow(3)], "{lhs_content}");
    }

    /// A pure deletion shows its old lines in the left pane already, and a
    /// replacement of equal length leaves no gap at all: neither may be marked.
    #[gpui::test]
    async fn test_no_insertion_marker_for_deletions_or_replacements(cx: &mut gpui::TestAppContext) {
        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let (markers, lhs_content) = lhs_insertion_markers(
            &editor,
            "line1\nline2\nline3\nline4\n",
            "line1\nline3\nline4\n",
            &mut cx,
        );
        assert_eq!(markers, Vec::new(), "pure deletion: {lhs_content}");

        let (markers, lhs_content) = lhs_insertion_markers(
            &editor,
            "line1\nline2\nline3\nline4\n",
            "line1\nLINE2\nline3\nline4\n",
            &mut cx,
        );
        assert_eq!(markers, Vec::new(), "balanced replace: {lhs_content}");

        // Growing a replacement does open a gap, but its old line is right
        // there in the deleted colour, so it is not an insertion point.
        let (markers, lhs_content) = lhs_insertion_markers(
            &editor,
            "line1\nline2\nline3\nline4\n",
            "line1\nLINE2a\nLINE2b\nline3\nline4\n",
            &mut cx,
        );
        assert_eq!(markers, Vec::new(), "unbalanced replace: {lhs_content}");
    }

    /// The panes are also padded apart when the same unchanged text wraps onto
    /// a different number of rows on the other side. Those spacers are not
    /// insertions and must stay grey.
    #[gpui::test]
    async fn test_no_insertion_marker_for_soft_wrap_spacers(cx: &mut gpui::TestAppContext) {
        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        // One inserted line near the top, and one long *unchanged* line at the
        // bottom that the narrower pane wraps onto more rows than the wider one.
        let long_line = "aaaa bbbb cccc dddd eeee ffff";
        let base_text = format!("aaa\nzzz\n{long_line}\n");
        let current_text = format!("aaa\nINSERTED\nzzz\n{long_line}\n");

        let (lhs_editor, rhs_editor) =
            replace_excerpt_with_diff(&editor, &base_text, &current_text, &mut cx);

        // Settle the panes at widths that make the long line wrap differently.
        let mut lhs_content = String::new();
        for _ in 0..4 {
            lhs_content = editor_content_with_blocks_and_width(&lhs_editor, px(380.), &mut cx);
            let _ = editor_content_with_blocks_and_width(&rhs_editor, px(200.), &mut cx);
            cx.run_until_parked();
        }

        let spacer_count = {
            use crate::RowExt as _;
            let snapshot =
                lhs_editor.update_in(cx, |editor, window, cx| editor.snapshot(window, cx));
            let max_row = snapshot.max_point().row();
            snapshot
                .blocks_in_range(crate::DisplayRow(0)..max_row.next_row())
                .filter(|(_, block)| matches!(block, crate::display_map::Block::Spacer { .. }))
                .count()
        };
        assert!(
            spacer_count >= 2,
            "the wrap difference should add a spacer of its own: {lhs_content}"
        );

        let markers = markers_of(&lhs_editor, &mut cx);
        let zzz_row = display_row_of(&lhs_editor, 1, &mut cx);
        assert_eq!(
            markers,
            vec![crate::DisplayRow(zzz_row.0 - 1)],
            "only the inserted line may be marked: {lhs_content}"
        );
    }

    /// The first and last line are still real boundaries: the marker has to
    /// land on the gap there too, not on the neighbouring text row.
    #[gpui::test]
    async fn test_insertion_marker_at_start_and_end_of_file(cx: &mut gpui::TestAppContext) {
        let (editor, mut cx) = init_test(cx, SoftWrap::None, DiffViewStyle::Split).await;

        let (markers, lhs_content) = lhs_insertion_markers(
            &editor,
            "line1\nline2\nline3\nline4\n",
            "line0\nline1\nline2\nline3\nline4\n",
            &mut cx,
        );
        assert_eq!(
            lhs_content,
            "§ <no file>\n§ -----\n§ spacer\nline1\nline2\nline3\nline4"
        );
        assert_eq!(
            markers,
            vec![crate::DisplayRow(2)],
            "insertion above the first line: {lhs_content}"
        );

        let (markers, lhs_content) = lhs_insertion_markers(
            &editor,
            "line1\nline2\nline3\nline4\n",
            "line1\nline2\nline3\nline4\nline5\n",
            &mut cx,
        );
        assert_eq!(
            lhs_content,
            "§ <no file>\n§ -----\nline1\nline2\nline3\nline4\n§ spacer"
        );
        assert_eq!(
            markers,
            vec![crate::DisplayRow(6)],
            "insertion below the last line: {lhs_content}"
        );
    }

    /// The live regression: dragging the split divider left the two panes
    /// painting different rows for the same unchanged line, and no amount of
    /// redrawing fixed it — only a scroll did.
    ///
    /// The panes lay out one after the other inside a single frame, so the one
    /// that lays out first computes its alignment spacers against the
    /// companion's *previous* wrap width. The companion's later layout does
    /// repair the block map (`fresh` below is always correct), but the first
    /// pane has already painted. The repair is therefore only visible if
    /// another frame is drawn, and gpui deliberately drops any invalidation
    /// raised while a draw is in flight (`WindowInvalidator::invalidate_view`
    /// returns false unless `draw_phase` is `None`) — which is exactly when a
    /// rewrap is discovered. Hence the deferred notify in
    /// `DisplayMap::invalidate_companion_alignment`.
    ///
    /// This test drives the real `Window::draw` path (the editor is a live item
    /// in the workspace) because that is the only path where gpui tracks the
    /// panes as window entities and applies that draw-phase gate at all.
    #[gpui::test]
    async fn test_split_panes_repaint_after_wrap_width_change(cx: &mut gpui::TestAppContext) {
        use crate::RowExt as _;
        use gpui::size;
        use rope::Point;

        let (editor, mut cx) = init_test(cx, SoftWrap::EditorWidth, DiffViewStyle::Split).await;

        // A long soft-wrapping line, a modification hunk, and a trailing
        // unchanged line whose display row is the thing that must agree.
        let base = "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll\nold\ntail line one two three four five six seven\n";
        let current = "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll\nnew one\nnew two\ntail line one two three four five six seven\n";

        for path in [PathKey::sorted(0), PathKey::sorted(1)] {
            let (buffer, diff) = buffer_with_diff(base, current, &mut cx);
            let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
            diff.update(cx, |diff, cx| {
                diff.recalculate_diff_sync(&buffer_snapshot, cx)
            });
            cx.run_until_parked();
            editor.update(cx, |editor, cx| {
                editor.update_excerpts_for_path(
                    path,
                    buffer,
                    vec![Point::new(0, 0)..Point::new(3, 0)],
                    0,
                    diff,
                    cx,
                );
            });
            cx.run_until_parked();
        }

        let workspace = editor.read_with(cx, |editor, _| editor.workspace.clone());
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            })
            .expect("workspace should be alive");
        cx.run_until_parked();

        let (rhs_editor, lhs_editor) = editor.update(cx, |editor, _| {
            let lhs = editor.lhs.as_ref().expect("should have lhs editor");
            (editor.rhs_editor.clone(), lhs.editor.clone())
        });

        fn second_header(snapshot: &crate::EditorSnapshot) -> Option<crate::DisplayRow> {
            snapshot
                .blocks_in_range(crate::DisplayRow(0)..snapshot.max_point().row().next_row())
                .filter(|(_, block)| {
                    matches!(block, crate::display_map::Block::BufferHeader { .. })
                })
                .map(|(row, _)| row)
                .nth(1)
        }

        // What each pane last put on screen, as opposed to what a fresh
        // snapshot would say — the two diverge precisely when this bug is live.
        let painted = |cx: &mut gpui::VisualTestContext| {
            let pane_row = |pane: &Entity<Editor>, cx: &mut gpui::VisualTestContext| {
                pane.read_with(cx, |pane, _| {
                    second_header(
                        &pane
                            .last_position_map
                            .as_ref()
                            .expect("pane should have been painted")
                            .snapshot,
                    )
                    .expect("painted snapshot should contain both excerpt headers")
                })
            };
            (pane_row(&lhs_editor, cx), pane_row(&rhs_editor, cx))
        };

        cx.simulate_resize(size(px(1700.), px(900.)));
        cx.run_until_parked();
        let (lhs_even, rhs_even) = painted(&mut cx);
        assert_eq!(
            lhs_even, rhs_even,
            "panes of equal width should already agree"
        );

        // Drag the divider: both panes change width in the same frame, so the
        // one that lays out first sees a stale companion wrap width.
        editor.update(cx, |editor, cx| {
            editor
                .split_state
                .update(cx, |state, _| state.set_left_ratio_for_test(0.75));
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let (lhs_painted, rhs_painted) = painted(&mut cx);
        assert_eq!(
            lhs_painted, rhs_painted,
            "after the divider moves, both panes must paint the same unchanged \
             line on the same display row without any intervening scroll"
        );

        let lhs_fresh = lhs_editor.update_in(cx, |editor, window, cx| {
            second_header(&editor.snapshot(window, cx))
        });
        assert_eq!(
            Some(lhs_painted),
            lhs_fresh,
            "the left pane must have painted its repaired layout, not a stale one"
        );
    }

    /// The two panes meet at the divider, so any column one of them reserves
    /// and the other does not shows up as a width difference and as a ragged
    /// centre strip. The right pane can be a singleton buffer (the git panel's
    /// solo file diff) while the left pane is always a synthesized multi-buffer
    /// of base text, and the singleton gutter reserves an indicator column and
    /// a wider fold column that the multi-buffer one does not.
    #[gpui::test]
    async fn test_split_panes_reserve_identical_gutters(cx: &mut gpui::TestAppContext) {
        let (editor, cx) = init_singleton_test(cx, "one\ntwo\nthree\n", "one\nTWO\nthree\n").await;

        let (rhs_editor, lhs_editor) = editor.update(cx, |editor, _| {
            let lhs = editor.lhs.as_ref().expect("should have split");
            (editor.rhs_editor.clone(), lhs.editor.clone())
        });

        // `Editor::gutter_dimensions` is written during prepaint, so both panes
        // have to be laid out before they can be compared.
        let _ = editor_content_with_blocks_and_width(&rhs_editor, px(600.), cx);
        let _ = editor_content_with_blocks_and_width(&lhs_editor, px(600.), cx);
        cx.run_until_parked();

        let lhs = lhs_editor.read_with(cx, |editor, _| editor.gutter_dimensions);
        let rhs = rhs_editor.read_with(cx, |editor, _| editor.gutter_dimensions);

        assert!(lhs.width > px(0.), "the left pane must have been laid out");
        assert_eq!(
            lhs.width, rhs.width,
            "the two panes must reserve the same gutter width, got {lhs:?} vs {rhs:?}"
        );
        assert_eq!(lhs.left_padding, rhs.left_padding);
        assert_eq!(lhs.right_padding, rhs.right_padding);
        assert_eq!(lhs.indicator_column_width, rhs.indicator_column_width);
        assert_eq!(
            lhs.indicator_column_width,
            px(0.),
            "a diff pane renders no runnables or bookmarks, so it reserves no \
             indicator column"
        );

        // The left pane's gutter is right-aligned against the divider (#57) and
        // the right pane's is mirrored, so the gap each one puts against the
        // divider is measured from the opposite edge.
        assert!(!lhs.mirrored);
        assert!(rhs.mirrored);
        // The two are the same subtraction taken from opposite ends, so they
        // agree only to within float rounding.
        let same = |a: Pixels, b: Pixels| (a - b).abs() < px(0.01);

        let lhs_gap_at_divider = lhs.width - lhs.line_number_area().end;
        let rhs_gap_at_divider = rhs.line_number_area().start;
        assert!(
            same(lhs_gap_at_divider, rhs_gap_at_divider),
            "the centre strip must be symmetric: {lhs_gap_at_divider:?} on the \
             left of the divider vs {rhs_gap_at_divider:?} on the right"
        );

        let lhs_gap_at_text = lhs.line_number_area().start;
        let rhs_gap_at_text = rhs.width - rhs.line_number_area().end;
        assert!(
            same(lhs_gap_at_text, rhs_gap_at_text),
            "and so must the space between the numbers and the code: \
             {lhs_gap_at_text:?} vs {rhs_gap_at_text:?}"
        );
    }

    /// Which side a pane is on belongs to the *editor*, not to whoever happens
    /// to build the element. `Editor`'s own `Render` impl constructs a bare
    /// `EditorElement::new` and has no opportunity to declare a side, yet the
    /// left pane's gutter still has to land against the divider. While the
    /// element carried its own manually-set copy of `split_side`, this path
    /// drew a correctly *sized* gutter on the wrong edge.
    #[gpui::test]
    async fn test_split_pane_element_derives_its_side_from_the_editor(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = init_singleton_test(cx, "one\ntwo\nthree\n", "one\nTWO\nthree\n").await;

        let (rhs_editor, lhs_editor) = editor.update(cx, |editor, _| {
            let lhs = editor.lhs.as_ref().expect("should have split");
            (editor.rhs_editor.clone(), lhs.editor.clone())
        });

        // This draws each pane through `Render for Editor`, not through
        // `SplitEditorView` — no setter is called on either element.
        let _ = editor_content_with_blocks_and_width(&rhs_editor, px(600.), cx);
        let _ = editor_content_with_blocks_and_width(&lhs_editor, px(600.), cx);
        cx.run_until_parked();

        let drawn_pane = |pane: &Entity<Editor>, cx: &mut VisualTestContext| {
            pane.read_with(cx, |editor, _| {
                let position_map = editor
                    .last_position_map
                    .clone()
                    .expect("the pane must have been drawn");
                (
                    editor.last_bounds.expect("the pane must have been drawn"),
                    position_map.gutter_hitbox.bounds,
                    position_map.text_hitbox.bounds,
                )
            })
        };
        // The gutter origin is the pane's right edge minus the gutter width, so
        // the two ends agree only to within float rounding.
        let same = |a: Pixels, b: Pixels| (a - b).abs() < px(0.01);

        let (bounds, gutter, text) = drawn_pane(&lhs_editor, cx);
        assert!(
            gutter.size.width > px(0.),
            "the left pane must have been laid out with a gutter"
        );
        assert!(
            same(gutter.right(), bounds.right()),
            "the left pane's gutter must be right-aligned against the divider: \
             gutter ends at {:?}, pane at {:?}",
            gutter.right(),
            bounds.right()
        );
        assert!(
            same(text.left(), bounds.left()),
            "and its text must lie to the left of that gutter, got {:?} vs {:?}",
            text.left(),
            bounds.left()
        );

        let (bounds, gutter, text) = drawn_pane(&rhs_editor, cx);
        assert!(
            same(gutter.left(), bounds.left()),
            "the right pane's gutter stays on its own left edge, got {:?} vs {:?}",
            gutter.left(),
            bounds.left()
        );
        assert!(
            same(text.left(), gutter.right()),
            "and its text follows the gutter, got {:?} vs {:?}",
            text.left(),
            gutter.right()
        );
    }

    /// Breakpoints and bookmarks are meaningless while reviewing a diff, and
    /// leaving them on is what filled the gutter context menu with entries the
    /// pane cannot act on. The right pane opted out from the start; the left
    /// pane did not.
    #[gpui::test]
    async fn test_split_panes_offer_no_breakpoint_or_bookmark_entries(
        cx: &mut gpui::TestAppContext,
    ) {
        let (editor, cx) = init_singleton_test(cx, "one\ntwo\nthree\n", "one\nTWO\nthree\n").await;

        let (rhs_editor, lhs_editor) = editor.update(cx, |editor, _| {
            let lhs = editor.lhs.as_ref().expect("should have split");
            (editor.rhs_editor.clone(), lhs.editor.clone())
        });

        for (name, pane) in [("left", &lhs_editor), ("right", &rhs_editor)] {
            let (breakpoints, bookmarks, runnables) = pane.read_with(cx, |editor, _| {
                (
                    editor.show_breakpoints,
                    editor.show_bookmarks,
                    editor.show_runnables,
                )
            });
            assert_eq!(
                breakpoints,
                Some(false),
                "{name} pane must not offer breakpoints"
            );
            assert_eq!(
                bookmarks,
                Some(false),
                "{name} pane must not offer bookmarks"
            );
            assert_eq!(
                runnables,
                Some(false),
                "{name} pane has runnables disabled, so it must not reserve their column"
            );
        }
    }
}
