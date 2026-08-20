use std::cmp;

use collections::{HashMap, HashSet};
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Context, DragMoveEvent, Element, Entity,
    Global, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, Length,
    ParentElement,
    Pixels, StatefulInteractiveElement, Styled, TextStyleRefinement, Window, deferred, div,
    linear_color_stop, linear_gradient, point, px, size,
};
use multi_buffer::{Anchor, ExcerptBoundaryInfo};
use settings::Settings;
use smallvec::smallvec;
use text::BufferId;
use theme::ActiveTheme;
use ui::scrollbars::ShowScrollbar;
use ui::{h_flex, prelude::*, v_flex};

use gpui::ContentMask;

use crate::{
    DisplayRow, Editor, EditorSettings, EditorSnapshot, EditorStyle, FILE_HEADER_HEIGHT,
    MULTI_BUFFER_EXCERPT_HEADER_HEIGHT, RowExt, StickyHeaderExcerpt,
    display_map::Block,
    element::{EditorElement, header_jump_data, render_buffer_header},
    scroll::ScrollOffset,
    split::SplittableEditor,
    split_connectors::{CONNECTOR_STRIP_WIDTH, connector_ribbons, editor_rem_size},
};

#[derive(Debug, Clone)]
struct DraggedSplitHandle;

/// Where the user last put the split divider. Every diff gets its own
/// `SplitEditorState`, so without this, stepping to the next file in a commit
/// silently throws away the position the user just chose — they move the
/// divider to read the "after" side, click another file, and it snaps back to
/// the middle. Process-wide rather than per-view precisely because the point is
/// to survive the view.
struct LastSplitRatio(f32);

impl Global for LastSplitRatio {}

const DEFAULT_SPLIT_RATIO: f32 = 0.5;

pub struct SplitEditorState {
    left_ratio: f32,
    visible_left_ratio: f32,
    cached_width: Pixels,
}

impl SplitEditorState {
    pub fn new(cx: &mut App) -> Self {
        let ratio = cx
            .try_global::<LastSplitRatio>()
            .map_or(DEFAULT_SPLIT_RATIO, |remembered| remembered.0);
        Self {
            left_ratio: ratio,
            visible_left_ratio: ratio,
            cached_width: px(0.),
        }
    }

    #[allow(clippy::misnamed_getters)]
    pub fn left_ratio(&self) -> f32 {
        self.visible_left_ratio
    }

    pub fn right_ratio(&self) -> f32 {
        1.0 - self.visible_left_ratio
    }

    fn on_drag_move(
        &mut self,
        drag_event: &DragMoveEvent<DraggedSplitHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let drag_position = drag_event.event.position;
        let bounds = drag_event.bounds;
        let bounds_width = bounds.right() - bounds.left();

        if bounds_width > px(0.) {
            self.cached_width = bounds_width;
        }

        let min_ratio = 0.1;
        let max_ratio = 0.9;

        let new_ratio = (drag_position.x - bounds.left()) / bounds_width;
        self.visible_left_ratio = new_ratio.clamp(min_ratio, max_ratio);
    }

    fn commit_ratio(&mut self, cx: &mut App) {
        self.left_ratio = self.visible_left_ratio;
        cx.set_global(LastSplitRatio(self.left_ratio));
    }

    #[cfg(test)]
    pub(crate) fn set_left_ratio_for_test(&mut self, ratio: f32) {
        self.left_ratio = ratio;
        self.visible_left_ratio = ratio;
    }

    fn on_double_click(&mut self, cx: &mut App) {
        self.left_ratio = DEFAULT_SPLIT_RATIO;
        self.visible_left_ratio = DEFAULT_SPLIT_RATIO;
        cx.set_global(LastSplitRatio(DEFAULT_SPLIT_RATIO));
    }
}

#[derive(IntoElement)]
pub struct SplitEditorView {
    splittable_editor: Entity<SplittableEditor>,
    style: EditorStyle,
    split_state: Entity<SplitEditorState>,
}

impl SplitEditorView {
    pub fn new(
        splittable_editor: Entity<SplittableEditor>,
        style: EditorStyle,
        split_state: Entity<SplitEditorState>,
    ) -> Self {
        Self {
            splittable_editor,
            style,
            split_state,
        }
    }
}

/// The strip between the two line-number columns: connector ribbons, a
/// separator line on each side, and the drag-to-resize handle. The whole strip
/// is the drag target — none of it carries text, and a 36px target is far
/// easier to grab than the 12px one the bare 1px divider used to offer.
fn render_connector_strip(
    state: &Entity<SplitEditorState>,
    lhs_editor: Entity<Editor>,
    rhs_editor: Entity<Editor>,
    style: EditorStyle,
    separator_color: Hsla,
    background_color: Hsla,
    window: &mut Window,
    _cx: &mut App,
) -> AnyElement {
    let state_for_click = state.clone();
    let rhs_editor_for_scroll = rhs_editor.clone();
    let scroll_line_height = style
        .text
        .line_height_in_pixels(editor_rem_size(&style).unwrap_or(window.rem_size()));

    let separator = |align_right: bool| {
        let separator = div()
            .absolute()
            .top_0()
            .h_full()
            .w(px(1.))
            .bg(separator_color);
        if align_right {
            separator.right_0()
        } else {
            separator.left_0()
        }
    };

    div()
        .id("split-resize-container")
        .relative()
        .h_full()
        .flex_shrink_0()
        .w(CONNECTOR_STRIP_WIDTH)
        // The ribbons are drawn by a bare `canvas()`, which installs no content
        // mask of its own, so without this a stray path would be clipped only
        // by the window and could paint over a pane's line numbers.
        .overflow_hidden()
        .bg(background_color)
        // The strip is a flex sibling of the two panes, not an overlay on top
        // of them, so no editor's hitbox covers it and the wheel would do
        // nothing here. Widening the divider from 1px to 36px would otherwise
        // carve a dead band out of the middle of the diff, where the cursor
        // naturally rests while comparing hunks. Both panes share one scroll
        // anchor, so driving the right one carries the left with it.
        .on_scroll_wheel(move |event: &gpui::ScrollWheelEvent, window, cx| {
            // The zoom gesture is shared with `EditorElement` so the strip is
            // not a patch of the diff where the same gesture means something
            // else. Horizontal deltas are the one deliberate omission: the
            // strip has no text of its own to scroll sideways past, and the two
            // panes scroll horizontally on their own.
            if crate::element::mouse::handle_wheel_zoom_shortcut(event, &rhs_editor_for_scroll, cx)
            {
                return;
            }
            // `window.line_height()` is the ambient UI line height during event
            // dispatch — the text-style stack is empty here, so it has nothing
            // to do with the buffer font a pixel delta must be measured in.
            let lines = match event.delta {
                gpui::ScrollDelta::Pixels(pixels) if scroll_line_height > px(0.) => {
                    ScrollOffset::from(pixels.y / scroll_line_height)
                }
                gpui::ScrollDelta::Pixels(_) => return,
                gpui::ScrollDelta::Lines(lines) => ScrollOffset::from(lines.y),
            };
            if lines == 0. {
                return;
            }
            let sensitivity = if event.modifiers.alt {
                EditorSettings::get_global(cx).fast_scroll_sensitivity
            } else {
                EditorSettings::get_global(cx).scroll_sensitivity
            }
            .max(0.01);
            let scrolled = rhs_editor_for_scroll.update(cx, |editor, cx| {
                let current = editor.scroll_position(cx);
                let target = point(
                    current.x,
                    (current.y - lines * ScrollOffset::from(sensitivity)).max(0.),
                );
                if target == current {
                    return false;
                }
                editor.set_scroll_position(target, window, cx);
                true
            });
            if scrolled {
                cx.stop_propagation();
            }
        })
        .child(connector_ribbons(lhs_editor, rhs_editor, style))
        .child(separator(false))
        .child(separator(true))
        // Deferred so the handle's hitbox is painted above the ribbons and
        // above whichever pane happens to be painted last — same treatment the
        // dock resize handle gets in `workspace::dock`.
        .child(deferred(
            div()
                .id("split-resize-handle")
                .absolute()
                .inset_0()
                .cursor_col_resize()
                .block_mouse_except_scroll()
                .on_click(move |event, _, cx| {
                    if event.click_count() >= 2 {
                        state_for_click.update(cx, |state, cx| {
                            state.on_double_click(cx);
                        });
                    }
                    cx.stop_propagation();
                })
                .on_drag(DraggedSplitHandle, |_, _, _, cx| cx.new(|_| gpui::Empty)),
        ))
        .into_any_element()
}

impl RenderOnce for SplitEditorView {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let splittable_editor = self.splittable_editor.read(cx);

        assert!(
            splittable_editor.lhs_editor().is_some(),
            "`SplitEditorView` requires `SplittableEditor` to be in split mode"
        );

        let lhs_editor = splittable_editor.lhs_editor().unwrap().clone();
        let rhs_editor = splittable_editor.rhs_editor().clone();

        // The panes do not declare their side here: `EditorElement` reads it
        // from the editor's own `split_side`, set by `SplittableEditor`.
        let lhs = EditorElement::new(&lhs_editor, self.style.clone());
        let rhs = EditorElement::new(&rhs_editor, self.style.clone());

        let left_ratio = self.split_state.read(cx).left_ratio();
        let right_ratio = self.split_state.read(cx).right_ratio();

        let separator_color = cx.theme().colors().border_variant;
        let strip_background = cx.theme().colors().editor_gutter_background;

        let connector_strip = render_connector_strip(
            &self.split_state,
            lhs_editor.clone(),
            rhs_editor.clone(),
            self.style.clone(),
            separator_color,
            strip_background,
            window,
            cx,
        );

        let state_for_drag = self.split_state.downgrade();
        let state_for_drop = self.split_state.downgrade();

        let buffer_headers = SplitBufferHeadersElement::new(rhs_editor.clone(), self.style.clone());

        let lhs_editor_for_order = lhs_editor;
        let rhs_editor_for_order = rhs_editor;

        div()
            .id("split-editor-view-container")
            .size_full()
            .relative()
            .child(
                h_flex()
                    .with_dynamic_prepaint_order(move |_window, cx| {
                        let lhs_needs = lhs_editor_for_order.read(cx).has_autoscroll_request();
                        let rhs_needs = rhs_editor_for_order.read(cx).has_autoscroll_request();
                        match (lhs_needs, rhs_needs) {
                            (false, true) => smallvec![2, 1, 0],
                            _ => smallvec![0, 1, 2],
                        }
                    })
                    .id("split-editor-view")
                    .size_full()
                    .on_drag_move::<DraggedSplitHandle>(move |event, window, cx| {
                        state_for_drag
                            .update(cx, |state, cx| {
                                state.on_drag_move(event, window, cx);
                            })
                            .ok();
                    })
                    .on_drop::<DraggedSplitHandle>(move |_, _, cx| {
                        state_for_drop
                            .update(cx, |state, cx| {
                                state.commit_ratio(cx);
                            })
                            .ok();
                    })
                    .child(
                        div()
                            .id("split-editor-left")
                            .flex_shrink_1()
                            .min_w_0()
                            .h_full()
                            .flex_basis(DefiniteLength::Fraction(left_ratio))
                            .overflow_hidden()
                            .child(lhs),
                    )
                    .child(connector_strip)
                    .child(
                        div()
                            .id("split-editor-right")
                            .flex_shrink_1()
                            .min_w_0()
                            .h_full()
                            .flex_basis(DefiniteLength::Fraction(right_ratio))
                            .overflow_hidden()
                            .child(rhs),
                    ),
            )
            .child(buffer_headers)
    }
}

struct SplitBufferHeadersElement {
    editor: Entity<Editor>,
    style: EditorStyle,
}

impl SplitBufferHeadersElement {
    fn new(editor: Entity<Editor>, style: EditorStyle) -> Self {
        Self { editor, style }
    }
}

struct BufferHeaderLayout {
    element: AnyElement,
}

struct SplitBufferHeadersPrepaintState {
    sticky_header: Option<AnyElement>,
    non_sticky_headers: Vec<BufferHeaderLayout>,
}

impl IntoElement for SplitBufferHeadersElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SplitBufferHeadersElement {
    type RequestLayoutState = ();
    type PrepaintState = SplitBufferHeadersPrepaintState;

    fn id(&self) -> Option<gpui::ElementId> {
        Some("split-buffer-headers".into())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = gpui::Style::default();
        style.position = gpui::Position::Absolute;
        style.inset.top = DefiniteLength::Fraction(0.0).into();
        style.inset.left = DefiniteLength::Fraction(0.0).into();
        style.size.width = Length::Definite(DefiniteLength::Fraction(1.0));
        style.size.height = Length::Definite(DefiniteLength::Fraction(1.0));
        let layout_id = window.request_layout(style, [], _cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return SplitBufferHeadersPrepaintState {
                sticky_header: None,
                non_sticky_headers: Vec::new(),
            };
        }

        let rem_size = self.rem_size();
        let text_style = TextStyleRefinement {
            font_size: Some(self.style.text.font_size),
            line_height: Some(self.style.text.line_height),
            ..Default::default()
        };

        window.with_rem_size(rem_size, |window| {
            window.with_text_style(Some(text_style), |window| {
                Self::prepaint_inner(self, bounds, window, cx)
            })
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let rem_size = self.rem_size();
        let text_style = TextStyleRefinement {
            font_size: Some(self.style.text.font_size),
            line_height: Some(self.style.text.line_height),
            ..Default::default()
        };

        window.with_rem_size(rem_size, |window| {
            window.with_text_style(Some(text_style), |window| {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    for header_layout in &mut prepaint.non_sticky_headers {
                        header_layout.element.paint(window, cx);
                    }

                    if let Some(mut sticky_header) = prepaint.sticky_header.take() {
                        sticky_header.paint(window, cx);
                    }
                });
            });
        });
    }
}

impl SplitBufferHeadersElement {
    fn rem_size(&self) -> Option<Pixels> {
        editor_rem_size(&self.style)
    }

    fn prepaint_inner(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> SplitBufferHeadersPrepaintState {
        let line_height = window.line_height();

        let snapshot = self
            .editor
            .update(cx, |editor, cx| editor.snapshot(window, cx));
        let scroll_position = snapshot.scroll_position();

        // Compute right margin to avoid overlapping the scrollbar
        let settings = EditorSettings::get_global(cx);
        let scrollbars_shown = settings.scrollbar.show != ShowScrollbar::Never;
        let vertical_scrollbar_width = (scrollbars_shown
            && settings.scrollbar.axes.vertical
            && self.editor.read(cx).show_scrollbars.vertical)
            .then_some(EditorElement::SCROLLBAR_WIDTH)
            .unwrap_or_default();
        let available_width = bounds.size.width - vertical_scrollbar_width;

        let visible_height_in_lines = bounds.size.height / line_height;
        let max_row = snapshot.max_point().row();
        let start_row = cmp::min(DisplayRow(scroll_position.y.floor() as u32), max_row);
        let end_row = cmp::min(
            (scroll_position.y + visible_height_in_lines as f64).ceil() as u32,
            max_row.next_row().0,
        );
        let end_row = DisplayRow(end_row);

        let (selected_buffer_ids, latest_selection_anchors) =
            self.compute_selection_info(&snapshot, cx);

        let sticky_header = if snapshot.buffer_snapshot().show_headers() {
            snapshot
                .sticky_header_excerpt(scroll_position.y)
                .map(|sticky_excerpt| {
                    self.build_sticky_header(
                        sticky_excerpt,
                        &snapshot,
                        scroll_position,
                        bounds,
                        available_width,
                        line_height,
                        &selected_buffer_ids,
                        &latest_selection_anchors,
                        start_row,
                        end_row,
                        window,
                        cx,
                    )
                })
        } else {
            None
        };

        let sticky_header_excerpt_id = snapshot
            .sticky_header_excerpt(scroll_position.y)
            .map(|e| e.excerpt);

        let non_sticky_headers = self.build_non_sticky_headers(
            &snapshot,
            scroll_position,
            bounds,
            available_width,
            line_height,
            start_row,
            end_row,
            &selected_buffer_ids,
            &latest_selection_anchors,
            sticky_header_excerpt_id,
            window,
            cx,
        );

        SplitBufferHeadersPrepaintState {
            sticky_header,
            non_sticky_headers,
        }
    }

    fn compute_selection_info(
        &self,
        snapshot: &EditorSnapshot,
        cx: &App,
    ) -> (HashSet<BufferId>, HashMap<BufferId, Anchor>) {
        let editor = self.editor.read(cx);
        let all_selections = editor
            .selections
            .all::<crate::Point>(&snapshot.display_snapshot);
        let all_anchor_selections = editor.selections.all_anchors(&snapshot.display_snapshot);

        let mut selected_buffer_ids = HashSet::default();
        for selection in &all_selections {
            for buffer_id in snapshot
                .buffer_snapshot()
                .buffer_ids_for_range(selection.range())
            {
                selected_buffer_ids.insert(buffer_id);
            }
        }

        let mut anchors_by_buffer: HashMap<BufferId, (usize, Anchor)> = HashMap::default();
        for selection in all_anchor_selections.iter() {
            let head = selection.head();
            if let Some((text_anchor, _)) = snapshot.buffer_snapshot().anchor_to_buffer_anchor(head)
            {
                anchors_by_buffer
                    .entry(text_anchor.buffer_id)
                    .and_modify(|(latest_id, latest_anchor)| {
                        if selection.id > *latest_id {
                            *latest_id = selection.id;
                            *latest_anchor = head;
                        }
                    })
                    .or_insert((selection.id, head));
            }
        }
        let latest_selection_anchors = anchors_by_buffer
            .into_iter()
            .map(|(buffer_id, (_, anchor))| (buffer_id, anchor))
            .collect();

        (selected_buffer_ids, latest_selection_anchors)
    }

    fn build_sticky_header(
        &self,
        StickyHeaderExcerpt { excerpt }: StickyHeaderExcerpt<'_>,
        snapshot: &EditorSnapshot,
        scroll_position: gpui::Point<ScrollOffset>,
        bounds: Bounds<Pixels>,
        available_width: Pixels,
        line_height: Pixels,
        selected_buffer_ids: &HashSet<BufferId>,
        latest_selection_anchors: &HashMap<BufferId, Anchor>,
        start_row: DisplayRow,
        end_row: DisplayRow,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let jump_data = header_jump_data(
            snapshot,
            DisplayRow(scroll_position.y as u32),
            FILE_HEADER_HEIGHT + MULTI_BUFFER_EXCERPT_HEADER_HEIGHT,
            excerpt,
            latest_selection_anchors,
        );

        let editor_bg_color = cx.theme().colors().editor_background;
        let selected = selected_buffer_ids.contains(&excerpt.buffer_id());

        let mut header = v_flex()
            .id("sticky-buffer-header")
            .w(available_width)
            .relative()
            .child(
                div()
                    .w(available_width)
                    .h(FILE_HEADER_HEIGHT as f32 * line_height)
                    .bg(linear_gradient(
                        0.,
                        linear_color_stop(editor_bg_color.opacity(0.), 0.),
                        linear_color_stop(editor_bg_color, 0.6),
                    ))
                    .absolute()
                    .top_0(),
            )
            .child(
                render_buffer_header(
                    &self.editor,
                    excerpt,
                    false,
                    selected,
                    true,
                    jump_data,
                    window,
                    cx,
                )
                .into_any_element(),
            )
            .into_any_element();

        let mut origin = bounds.origin;

        for (block_row, block) in snapshot.blocks_in_range(start_row..end_row) {
            if !block.is_buffer_header() {
                continue;
            }

            if block_row.0 <= scroll_position.y as u32 {
                continue;
            }

            let max_row = block_row.0.saturating_sub(FILE_HEADER_HEIGHT);
            let offset = scroll_position.y - max_row as f64;

            if offset > 0.0 {
                origin.y -= Pixels::from(offset * f64::from(line_height));
            }
            break;
        }

        let available_size = size(
            AvailableSpace::Definite(available_width),
            AvailableSpace::MinContent,
        );

        header.prepaint_as_root(origin, available_size, window, cx);

        header
    }

    fn build_non_sticky_headers(
        &self,
        snapshot: &EditorSnapshot,
        scroll_position: gpui::Point<ScrollOffset>,
        bounds: Bounds<Pixels>,
        available_width: Pixels,
        line_height: Pixels,
        start_row: DisplayRow,
        end_row: DisplayRow,
        selected_buffer_ids: &HashSet<BufferId>,
        latest_selection_anchors: &HashMap<BufferId, Anchor>,
        sticky_header: Option<&ExcerptBoundaryInfo>,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<BufferHeaderLayout> {
        let mut headers = Vec::new();

        for (block_row, block) in snapshot.blocks_in_range(start_row..end_row) {
            let (excerpt, is_folded) = match block {
                Block::BufferHeader { excerpt, .. } => {
                    if sticky_header == Some(excerpt) {
                        continue;
                    }
                    (excerpt, false)
                }
                Block::FoldedBuffer { first_excerpt, .. } => (first_excerpt, true),
                // ExcerptBoundary is just a separator line, not a buffer header
                Block::ExcerptBoundary { .. } | Block::Custom(_) | Block::Spacer { .. } => continue,
            };

            let selected = selected_buffer_ids.contains(&excerpt.buffer_id());
            let jump_data = header_jump_data(
                snapshot,
                block_row,
                block.height(),
                excerpt,
                latest_selection_anchors,
            );

            let mut header = render_buffer_header(
                &self.editor,
                excerpt,
                is_folded,
                selected,
                false,
                jump_data,
                window,
                cx,
            )
            .into_any_element();

            let y_offset = (block_row.0 as f64 - scroll_position.y) * f64::from(line_height);
            let origin = point(bounds.origin.x, bounds.origin.y + Pixels::from(y_offset));

            let available_size = size(
                AvailableSpace::Definite(available_width),
                AvailableSpace::MinContent,
            );

            header.prepaint_as_root(origin, available_size, window, cx);

            headers.push(BufferHeaderLayout { element: header });
        }

        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// Each file's diff builds its own `SplitEditorState`, so the divider
    /// position has to live outside the view or stepping through a commit's
    /// files throws away the position the user just set.
    #[gpui::test]
    fn test_split_ratio_survives_a_new_split_editor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let first = cx.new(|cx| SplitEditorState::new(cx));
            assert_eq!(first.read(cx).left_ratio(), DEFAULT_SPLIT_RATIO);

            first.update(cx, |state, cx| {
                state.visible_left_ratio = 0.25;
                state.commit_ratio(cx);
            });

            let second = cx.new(|cx| SplitEditorState::new(cx));
            assert_eq!(second.read(cx).left_ratio(), 0.25);

            second.update(cx, |state, cx| state.on_double_click(cx));

            let third = cx.new(|cx| SplitEditorState::new(cx));
            assert_eq!(third.read(cx).left_ratio(), DEFAULT_SPLIT_RATIO);
        });
    }
}
