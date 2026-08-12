//! IDEA-style connector ribbons for the side-by-side diff.
//!
//! The strip between the two line-number columns is filled with polygons that
//! tie each block of the old text on the left to the block it became on the
//! right. Both panes share a scroll anchor and are row-count balanced with
//! [`crate::display_map::Block::Spacer`], so corresponding hunks start on the
//! same display row; a ribbon therefore has a flat top and a sloped bottom
//! whenever the two blocks differ in length.

use std::ops::Range;

use buffer_diff::DiffHunkStatusKind;
use gpui::{
    AbsoluteLength, App, Bounds, Entity, Hsla, IntoElement, PathBuilder, Pixels, Styled, Window,
    canvas, point, px,
};
use multi_buffer::{MultiBufferDiffHunk, MultiBufferPoint, MultiBufferRow};
use text::Bias;
use theme::ActiveTheme;
use util::ResultExt as _;

use crate::{DisplayPoint, DisplayRow, Editor, EditorSnapshot, EditorStyle, RowExt as _};

/// Total width of the connector strip that sits between the two gutters.
pub(crate) const CONNECTOR_STRIP_WIDTH: Pixels = px(36.);

/// Inset of the ribbon endpoints from the strip edges, so the polygons do not
/// paint over the 1px separator lines that delimit the strip.
const RIBBON_EDGE_INSET: Pixels = px(1.);

/// A ribbon may belong to a hunk that extends far past the viewport. Endpoint
/// coordinates are clamped to a few screen heights around the strip so the
/// tessellator never sees absurd values; the clamped corners are off-screen, so
/// the visible part of the curve is unaffected.
const OFFSCREEN_CLAMP_SCREENS: f32 = 3.0;

struct Ribbon {
    left_top: Pixels,
    left_bottom: Pixels,
    right_top: Pixels,
    right_bottom: Pixels,
    color: Hsla,
}

/// One old block and the new block it became, in display rows of their
/// respective panes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConnectorRows {
    pub(crate) left: Range<DisplayRow>,
    pub(crate) right: Range<DisplayRow>,
    pub(crate) status: DiffHunkStatusKind,
}

/// Paints the connector polygons. Meant to be a `size_full` child of the
/// connector strip, so the element bounds *are* the strip.
pub(crate) fn connector_ribbons(
    lhs_editor: Entity<Editor>,
    rhs_editor: Entity<Editor>,
    style: EditorStyle,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            layout_ribbons(&lhs_editor, &rhs_editor, &style, bounds, window, cx)
        },
        |bounds, ribbons: Vec<Ribbon>, window, _cx| {
            if ribbons.is_empty() {
                return;
            }
            window.paint_layer(bounds, |window| {
                let left_x = bounds.left() + RIBBON_EDGE_INSET;
                let right_x = bounds.right() - RIBBON_EDGE_INSET;
                for ribbon in &ribbons {
                    paint_ribbon(ribbon, left_x, right_x, window);
                }
            });
        },
    )
    .size_full()
}

fn layout_ribbons(
    lhs_editor: &Entity<Editor>,
    rhs_editor: &Entity<Editor>,
    style: &EditorStyle,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Vec<Ribbon> {
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return Vec::new();
    }

    let rem_size = editor_rem_size(style).unwrap_or_else(|| window.rem_size());
    let line_height = style.text.line_height_in_pixels(rem_size);
    if line_height <= px(0.) {
        return Vec::new();
    }

    let lhs_snapshot = lhs_editor.update(cx, |editor, cx| editor.snapshot(window, cx));
    let rhs_snapshot = rhs_editor.update(cx, |editor, cx| editor.snapshot(window, cx));

    let visible_rows = f64::from(bounds.size.height / line_height);
    let connectors = connector_rows(&lhs_snapshot, &rhs_snapshot, visible_rows);

    let lhs_scroll_top = lhs_snapshot.scroll_position().y;
    let rhs_scroll_top = rhs_snapshot.scroll_position().y;

    let clamp_margin = bounds.size.height * OFFSCREEN_CLAMP_SCREENS;
    let clamp_range = (bounds.top() - clamp_margin)..(bounds.bottom() + clamp_margin);

    let colors = cx.theme().colors();

    let mut ribbons = Vec::new();
    for connector in connectors {
        let left_top = row_y(bounds.top(), connector.left.start, lhs_scroll_top, line_height);
        let left_bottom = row_y(bounds.top(), connector.left.end, lhs_scroll_top, line_height);
        let right_top = row_y(
            bounds.top(),
            connector.right.start,
            rhs_scroll_top,
            line_height,
        );
        let right_bottom = row_y(
            bounds.top(),
            connector.right.end,
            rhs_scroll_top,
            line_height,
        );

        if left_top.min(right_top) > bounds.bottom() || left_bottom.max(right_bottom) < bounds.top()
        {
            continue;
        }

        let color = match connector.status {
            DiffHunkStatusKind::Added => colors.version_control_added,
            DiffHunkStatusKind::Deleted => colors.version_control_deleted,
            DiffHunkStatusKind::Modified => colors.version_control_modified,
        };

        ribbons.push(Ribbon {
            left_top: clamp(left_top, &clamp_range),
            left_bottom: clamp(left_bottom, &clamp_range),
            right_top: clamp(right_top, &clamp_range),
            right_bottom: clamp(right_bottom, &clamp_range),
            color,
        });
    }

    ribbons
}

/// Pairs each hunk of the left pane with the hunk it became on the right and
/// resolves both to display rows. `visible_rows` is the viewport height in
/// rows; hunks well outside it are skipped.
///
/// The two panes are kept hunk-for-hunk in sync — see the quiesced assertion in
/// `SplittableEditor::check_invariants` — so the two hunk sequences can simply
/// be zipped. A frame rendered mid-edit can still catch them out of step, in
/// which case pairing stops rather than connecting unrelated hunks.
pub(crate) fn connector_rows(
    lhs_snapshot: &EditorSnapshot,
    rhs_snapshot: &EditorSnapshot,
    visible_rows: f64,
) -> Vec<ConnectorRows> {
    let lhs_visible = visible_buffer_rows(lhs_snapshot, visible_rows);
    let rhs_visible = visible_buffer_rows(rhs_snapshot, visible_rows);

    let mut connectors = Vec::new();
    let lhs_hunks = lhs_snapshot.buffer_snapshot().diff_hunks();
    let rhs_hunks = rhs_snapshot.buffer_snapshot().diff_hunks();

    for (lhs_hunk, rhs_hunk) in lhs_hunks.zip(rhs_hunks) {
        if lhs_hunk.diff_base_byte_range != rhs_hunk.diff_base_byte_range {
            break;
        }

        if lhs_hunk.row_range.end.0 < lhs_visible.start
            && rhs_hunk.row_range.end.0 < rhs_visible.start
        {
            continue;
        }
        if lhs_hunk.row_range.start.0 > lhs_visible.end
            && rhs_hunk.row_range.start.0 > rhs_visible.end
        {
            break;
        }

        let Some(left) = hunk_display_rows(lhs_snapshot, &lhs_hunk) else {
            continue;
        };
        let Some(right) = hunk_display_rows(rhs_snapshot, &rhs_hunk) else {
            continue;
        };

        connectors.push(ConnectorRows {
            left,
            right,
            status: lhs_hunk.status.kind,
        });
    }

    connectors
}

fn clamp(value: Pixels, range: &Range<Pixels>) -> Pixels {
    value.max(range.start).min(range.end)
}

fn row_y(top: Pixels, row: DisplayRow, scroll_top: f64, line_height: Pixels) -> Pixels {
    top + Pixels::from((row.as_f64() - scroll_top) * f64::from(line_height))
}

/// Multibuffer rows currently on screen, widened by a row on each side so a
/// hunk that only just touches the viewport is still considered.
fn visible_buffer_rows(snapshot: &EditorSnapshot, visible_rows: f64) -> Range<u32> {
    let scroll_top = snapshot.scroll_position().y;
    let max_row = snapshot.max_point().row();
    let start_row = DisplayRow((scroll_top.floor().max(0.) as u32).min(max_row.0));
    let end_row = DisplayRow(
        ((scroll_top + visible_rows).ceil().max(0.) as u32)
            .saturating_add(1)
            .min(max_row.0),
    );

    let start = snapshot
        .display_point_to_point(DisplayPoint::new(start_row, 0), Bias::Left)
        .row;
    let end = snapshot
        .display_point_to_point(DisplayPoint::new(end_row, 0), Bias::Right)
        .row;
    start.saturating_sub(1)..end.saturating_add(1)
}

/// Display rows spanned by a hunk. Mirrors `EditorSnapshot::display_diff_hunks_for_rows`,
/// but keeps hunks whose row range is empty — those are exactly the "collapsed"
/// end of a pure insertion or deletion, and they are what gives a ribbon its
/// slope.
fn hunk_display_rows(
    snapshot: &EditorSnapshot,
    hunk: &MultiBufferDiffHunk,
) -> Option<Range<DisplayRow>> {
    let start_point = MultiBufferPoint::new(hunk.row_range.start.0, 0);
    let display_start = snapshot.point_to_display_point(start_point, Bias::Left);
    if display_start.column() != 0 {
        // The hunk start is inside a fold; there is no row band to connect to.
        return None;
    }

    if hunk.row_range.end <= hunk.row_range.start {
        return Some(display_start.row()..display_start.row());
    }

    let last_row = MultiBufferRow(hunk.row_range.end.0 - 1);
    let end_point =
        MultiBufferPoint::new(last_row.0, snapshot.buffer_snapshot().line_len(last_row));
    let display_end = snapshot
        .point_to_display_point(end_point, Bias::Right)
        .row();
    Some(display_start.row()..DisplayRow(display_end.0 + 1))
}

fn paint_ribbon(ribbon: &Ribbon, left_x: Pixels, right_x: Pixels, window: &mut Window) {
    if ribbon.left_top == ribbon.left_bottom && ribbon.right_top == ribbon.right_bottom {
        // A hunk that collapsed to nothing on both sides has no ribbon; the
        // gutter strips still mark it.
        return;
    }

    let mut fill = PathBuilder::fill();
    fill.move_to(point(left_x, ribbon.left_top));
    horizontal_s_curve(
        &mut fill,
        left_x,
        ribbon.left_top,
        right_x,
        ribbon.right_top,
    );
    fill.line_to(point(right_x, ribbon.right_bottom));
    horizontal_s_curve(
        &mut fill,
        right_x,
        ribbon.right_bottom,
        left_x,
        ribbon.left_bottom,
    );
    fill.line_to(point(left_x, ribbon.left_top));
    if let Some(path) = fill.build().log_err() {
        window.paint_path(path, ribbon.color.opacity(0.22));
    }

    let mut outline = PathBuilder::stroke(px(1.));
    outline.move_to(point(left_x, ribbon.left_top));
    horizontal_s_curve(
        &mut outline,
        left_x,
        ribbon.left_top,
        right_x,
        ribbon.right_top,
    );
    if let Some(path) = outline.build().log_err() {
        window.paint_path(path, ribbon.color.opacity(0.8));
    }

    let mut outline = PathBuilder::stroke(px(1.));
    outline.move_to(point(left_x, ribbon.left_bottom));
    horizontal_s_curve(
        &mut outline,
        left_x,
        ribbon.left_bottom,
        right_x,
        ribbon.right_bottom,
    );
    if let Some(path) = outline.build().log_err() {
        window.paint_path(path, ribbon.color.opacity(0.8));
    }
}

/// Draws an S-shaped curve from `(from_x, from_y)` (assumed to be the current
/// point) to `(to_x, to_y)`, flattening to a straight line when the two ends
/// share a y coordinate.
fn horizontal_s_curve(
    builder: &mut PathBuilder,
    from_x: Pixels,
    from_y: Pixels,
    to_x: Pixels,
    to_y: Pixels,
) {
    if from_y == to_y {
        builder.line_to(point(to_x, to_y));
        return;
    }

    let mid_x = (from_x + to_x) / 2.;
    let mid_y = (from_y + to_y) / 2.;
    builder.curve_to(point(mid_x, mid_y), point(mid_x, from_y));
    builder.curve_to(point(to_x, to_y), point(mid_x, to_y));
}

/// The editor's own rem size, which is what its line height was resolved
/// against. Mirrors `SplitBufferHeadersElement::rem_size`.
fn editor_rem_size(style: &EditorStyle) -> Option<Pixels> {
    match style.text.font_size {
        AbsoluteLength::Pixels(pixels) => {
            let default_font_size_scale = 14. / ui::BASE_REM_SIZE_IN_PX;
            let default_font_size_delta = 1. - default_font_size_scale;
            Some(pixels * (1. + default_font_size_delta))
        }
        AbsoluteLength::Rems(rems) => Some(rems.to_pixels(ui::BASE_REM_SIZE_IN_PX.into())),
    }
}
