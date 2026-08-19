//! Pure canvas geometry for the commit graph: lane and row coordinates, the
//! cubic that carries a line from one lane to another, and the commit dot.
//! Nothing here reads [`GitGraph`](crate::GitGraph) state — every result is a
//! function of the bounds, the row height and the lane indices the renderer
//! passes in, which is what makes the whole module unit-testable on its own.

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Point, Window, point, px};

// Dot geometry follows IDEA's proportions: the dot's diameter is half the lane
// pitch, so dots read as solid markers with a clear gap between neighbouring
// lanes. It stays well under the row height (~22px), so growing it does not
// grow the row.
pub(crate) const COMMIT_CIRCLE_RADIUS: Pixels = px(4.0);
pub(crate) const COMMIT_CIRCLE_STROKE_WIDTH: Pixels = px(1.5);
// IDEA-style lane pitch: wide enough that a lane change reads as a curve rather
// than a hairline kink, and that adjacent dots don't visually merge. It is also
// the unit a transition's height is measured in (see
// `LANE_TRANSITION_ROWS_PER_EXTRA_LANE`), so anything below ~14px turns the
// crossings into near-vertical slivers.
const LANE_WIDTH: Pixels = px(16.0);
const LEFT_PADDING: Pixels = px(8.0);
// The commit-graph column is not user-resizable (IDEA-style); it is sized to
// the number of lanes in the loaded history, but never narrower than
// `MIN_GRAPH_LANES` so a linear history still reserves sensible space. The same
// floor applies per row (`graph_row_extent`), so a linear stretch reserves the
// full four lanes of indent instead of shifting the subject text around as the
// lane count flickers.
pub(crate) const MIN_GRAPH_LANES: usize = 4;
// Upper bound on the graph's share of the region it shares with the commit
// subject (the Description column). A hard lane cap clipped the DAG on any
// history wider than the cap, so the bound is viewport-relative instead: the
// graph grows with the real lane count and only stops once it would leave the
// subject text less than 60% of its column. On a 900px Description column
// that is ~35 lanes, far past any history a human reads.
const MAX_GRAPH_WIDTH_FRACTION: f32 = 0.4;
// Edges are drawn slightly heavier than before so a diagonal run — which is
// anti-aliased across two axes and therefore reads lighter than an axis-aligned
// one of the same width — keeps the same visual weight as the dots.
pub(crate) const LINE_WIDTH: Pixels = px(2.0);

pub(crate) fn lane_center_x(bounds: Bounds<Pixels>, lane: f32) -> Pixels {
    bounds.origin.x + LEFT_PADDING + lane * LANE_WIDTH + LANE_WIDTH / 2.0
}

/// Horizontal distance from the left edge of the graph area to the right edge
/// of the rightmost of `columns` occupied lanes: the leading padding plus one
/// `LANE_WIDTH` slot per column (a lane's dot is centred in its own slot, so
/// the last slot ends exactly `LANE_WIDTH / 2` past the last dot's centre).
///
/// This is what the commit subject is indented by on a given row — IDEA-style,
/// the text starts right after that row's own lanes, so a narrow stretch of
/// history pulls the text back left instead of every row being indented by the
/// widest row in the log.
///
/// `columns` is floored at [`MIN_GRAPH_LANES`] so the per-row indent can never
/// undercut the column width, which has the same floor: on a linear stretch the
/// subject would otherwise slam against the left edge and then jitter sideways
/// on every 1↔2 lane change. Above the floor the indent tracks the row's real
/// occupancy again.
pub(crate) fn graph_row_extent(columns: usize) -> Pixels {
    LEFT_PADDING + LANE_WIDTH * columns.max(MIN_GRAPH_LANES) as f32
}

/// Width of the whole commit-graph column: the extent of the widest row plus a
/// trailing padding, floored at `MIN_GRAPH_LANES` and capped at
/// `MAX_GRAPH_WIDTH_FRACTION` of `available` (the width of the region the graph
/// shares with the commit subject).
///
/// `available == 0` means "not measured yet" (the column state caches the
/// container width during prepaint, so the very first frame has none) — the
/// natural width is used then, uncapped.
///
/// The floor wins over the cap: in a pane too narrow for even
/// `MIN_GRAPH_LANES`, a graph squeezed to a couple of pixels is worse than one
/// that overruns its share.
pub(crate) fn graph_column_width_for(lanes: usize, available: Pixels) -> Pixels {
    // `graph_row_extent` already applies the `MIN_GRAPH_LANES` floor.
    let natural = graph_row_extent(lanes) + LEFT_PADDING;
    if available <= px(0.) {
        return natural;
    }
    let floor = graph_row_extent(MIN_GRAPH_LANES) + LEFT_PADDING;
    natural.min((available * MAX_GRAPH_WIDTH_FRACTION).max(floor))
}

pub(crate) fn to_row_center(
    to_row: usize,
    row_height: Pixels,
    scroll_offset: Pixels,
    bounds: Bounds<Pixels>,
) -> Pixels {
    bounds.origin.y + to_row as f32 * row_height + row_height / 2.0 - scroll_offset
}

fn distance_between(from: Point<Pixels>, to: Point<Pixels>) -> f32 {
    let dx = f32::from(to.x - from.x);
    let dy = f32::from(to.y - from.y);
    dx.hypot(dy)
}

/// The point `distance` pixels from `from` along the way to `to`, clamped to the
/// segment.
fn along(from: Point<Pixels>, to: Point<Pixels>, distance: f32) -> Point<Pixels> {
    let length = distance_between(from, to);
    if length <= f32::EPSILON {
        return from;
    }
    let fraction = (distance / length).clamp(0., 1.);
    point(
        from.x + (to.x - from.x) * fraction,
        from.y + (to.y - from.y) * fraction,
    )
}

/// How much vertical room a lane change gets per lane it has to cross, beyond
/// the first, as a fraction of a row. A transition capped at a constant number
/// of rows degenerates into a horizontal streak as soon as the jump is wide — a
/// fan-in of fifteen lanes crossed the entire graph inside a single row — so the
/// room scales with the distance instead and the slope stays readable however
/// wide the fan is.
const LANE_TRANSITION_ROWS_PER_EXTRA_LANE: f32 = 0.5;
/// Where the lane-change curve's control points sit, as a fraction of the
/// transition's height. They are pulled along the *vertical* axis only: that is
/// what makes the curve leave and arrive parallel to the lanes it joins. Any
/// value in `(0, 1]` keeps the curve monotonic in y; a half reads as the
/// symmetric S of the reference.
const LANE_TRANSITION_CONTROL_FRACTION: f32 = 0.5;

/// Vertical distance a lane change is allowed to take, given the horizontal
/// distance `lane_span` it has to cover and the `available` vertical room.
/// Scales with the row height, so a transition keeps its proportions at any UI
/// scale, and with the number of lanes crossed, so a wide jump is given a slope
/// instead of a streak. Always a magnitude: the caller applies the direction of
/// travel.
pub(crate) fn lane_transition_height(
    row_height: Pixels,
    lane_span: Pixels,
    available: Pixels,
) -> Pixels {
    let lanes = (f32::from(lane_span.abs()) / f32::from(LANE_WIDTH)).max(1.0);
    let rows = 1.0 + (lanes - 1.0) * LANE_TRANSITION_ROWS_PER_EXTRA_LANE;
    (row_height * rows).min(available.abs())
}

/// The two control points of the cubic that carries a line from one lane to
/// another. Both are offset from their end along the vertical only, so the
/// curve is tangent to the lanes at both ends and its joins with the straight
/// runs are invisible.
fn lane_transition_controls(
    from: Point<Pixels>,
    to: Point<Pixels>,
) -> (Point<Pixels>, Point<Pixels>) {
    let offset = (to.y - from.y) * LANE_TRANSITION_CONTROL_FRACTION;
    (point(from.x, from.y + offset), point(to.x, to.y - offset))
}

/// Pulls the ends of a lane change back by the given clearances, along the
/// curve's own direction at each end — the lane it leaves and the lane it lands
/// in — so an edge stops short of the commit dots it runs between instead of
/// painting over them. A transition with no vertical room to bend in has no
/// tangent to follow and backs off along the straight line between its ends
/// instead. Neither end can be pulled past the curve's own control point.
pub(crate) fn clear_lane_transition_dots(
    from: Point<Pixels>,
    to: Point<Pixels>,
    from_clearance: f32,
    to_clearance: f32,
) -> (Point<Pixels>, Point<Pixels>) {
    let (control_a, control_b) = lane_transition_controls(from, to);
    let has_leaving_tangent = distance_between(from, control_a) > f32::EPSILON;
    let has_arriving_tangent = distance_between(control_b, to) > f32::EPSILON;
    let leaves_towards = if has_leaving_tangent { control_a } else { to };
    let arrives_from = if has_arriving_tangent {
        control_b
    } else {
        from
    };
    // With a tangent, each end stops at its own control point, which sits at
    // most half the transition away — the two ends cannot meet. Without one
    // (a transition with no vertical room) both ends back off along the same
    // straight segment towards each other, and `along` only clamps each end
    // against the WHOLE segment, so a clearance wider than half the span would
    // let them cross and invert the stroke. Cap those ends at the midpoint.
    let half_span = distance_between(from, to) / 2.;
    let from_clearance = if has_leaving_tangent {
        from_clearance
    } else {
        from_clearance.min(half_span)
    };
    let to_clearance = if has_arriving_tangent {
        to_clearance
    } else {
        to_clearance.min(half_span)
    };
    (
        along(from, leaves_towards, from_clearance),
        along(to, arrives_from, to_clearance),
    )
}

/// Strokes the lane change itself: one cubic from `from` to `to` that leaves
/// and arrives vertically, spreading the bend over the whole transition. This
/// replaces the rounded elbow that used to be drawn here — rounding a corner
/// only softens the crease, the lane change still happens at a single joint.
pub(crate) fn stroke_lane_transition(
    builder: &mut PathBuilder,
    from: Point<Pixels>,
    to: Point<Pixels>,
) {
    let (control_a, control_b) = lane_transition_controls(from, to);
    builder.move_to(from);
    builder.cubic_bezier_to(to, control_a, control_b);
    builder.move_to(to);
}

pub(crate) fn draw_commit_circle(
    center_x: Pixels,
    center_y: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let radius = COMMIT_CIRCLE_RADIUS;

    // A quad whose corner radius equals half its side renders as a filled circle.
    // This is reliable across GPU backends; a hand-built two-arc fill path
    // rasterized blocky ("square") at this tiny radius.
    let bounds = Bounds::new(
        point(center_x - radius, center_y - radius),
        gpui::Size {
            width: radius * 2.0,
            height: radius * 2.0,
        },
    );
    window.paint_quad(gpui::fill(bounds, color).corner_radii(gpui::Corners::all(radius)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_column_width_is_capped_by_viewport_not_lane_count() {
        let floor = graph_row_extent(MIN_GRAPH_LANES) + LEFT_PADDING;

        // A linear history still reserves the minimum column.
        assert_eq!(graph_column_width_for(0, px(1000.)), floor);
        assert_eq!(graph_column_width_for(1, px(1000.)), floor);

        // Past the old hard-coded 12-lane cap the column keeps growing, so the
        // DAG is no longer silently clipped.
        assert_eq!(
            graph_column_width_for(20, px(1000.)),
            graph_row_extent(20) + LEFT_PADDING
        );
        assert!(graph_column_width_for(20, px(1000.)) > graph_column_width_for(12, px(1000.)));

        // A pathological history is bounded by the viewport instead: 40% of the
        // Description column, leaving the subject text the rest.
        assert_eq!(graph_column_width_for(100, px(500.)), px(200.));

        // Unmeasured container (first frame) means no cap at all.
        assert_eq!(
            graph_column_width_for(100, px(0.)),
            graph_row_extent(100) + LEFT_PADDING
        );

        // In a pane too narrow for even the minimum lanes the floor wins over
        // the fraction — a two-pixel graph is worse than one that overruns.
        assert_eq!(graph_column_width_for(8, px(100.)), floor);
    }

    #[test]
    fn test_graph_row_extent_floors_at_min_lanes() {
        // Anything at or below the floor reserves the same four lanes, so the
        // subject text on a linear stretch cannot slam into the graph and cannot
        // jitter as the lane count flickers between one and two.
        let floor = graph_row_extent(MIN_GRAPH_LANES);
        assert_eq!(graph_row_extent(0), floor);
        assert_eq!(graph_row_extent(1), floor);
        assert_eq!(graph_row_extent(MIN_GRAPH_LANES - 1), floor);
        assert_eq!(floor, LEFT_PADDING + LANE_WIDTH * MIN_GRAPH_LANES as f32);

        // Above the floor the indent tracks the row's real occupancy again, one
        // lane at a time.
        assert_eq!(
            graph_row_extent(MIN_GRAPH_LANES + 1) - floor,
            LANE_WIDTH,
            "each lane past the floor adds exactly one lane of indent"
        );
        assert_eq!(graph_row_extent(9) - graph_row_extent(8), LANE_WIDTH);

        // The per-row indent never exceeds the column the graph is painted in,
        // which carries the same floor.
        assert_eq!(
            graph_column_width_for(1, px(1000.)),
            graph_row_extent(1) + LEFT_PADDING
        );
    }

    #[test]
    fn test_lane_geometry_reads_like_the_reference() {
        // Lanes are spaced far enough apart that a lane change spans a visible
        // diagonal rather than a hairline kink.
        assert!(LANE_WIDTH >= px(14.));

        // Dots fill half the lane pitch, leaving a clear gap between the dots of
        // neighbouring lanes.
        assert_eq!(COMMIT_CIRCLE_RADIUS * 4.0, LANE_WIDTH);

        let bounds = Bounds::new(point(px(0.), px(0.)), gpui::Size::new(px(200.), px(200.)));
        assert_eq!(
            lane_center_x(bounds, 1.) - lane_center_x(bounds, 0.),
            LANE_WIDTH
        );
        // The first lane's dot clears the left padding.
        assert!(lane_center_x(bounds, 0.) - COMMIT_CIRCLE_RADIUS >= LEFT_PADDING);

        let row_height = px(22.);
        // Growing the dot must not grow the row.
        assert!(COMMIT_CIRCLE_RADIUS * 2.0 < row_height);

        // A hop into the neighbouring lane takes one row when there is room for
        // one, and less when there is not.
        assert_eq!(
            lane_transition_height(row_height, LANE_WIDTH, px(80.)),
            row_height
        );
        assert_eq!(
            lane_transition_height(row_height, LANE_WIDTH, px(9.)),
            px(9.)
        );
        assert_eq!(
            lane_transition_height(row_height, LANE_WIDTH, px(-9.)),
            px(9.)
        );
    }

    #[test]
    fn test_along_walks_towards_the_target() {
        let from = point(px(0.), px(0.));
        let to = point(px(30.), px(40.));

        // Half of a 3-4-5 diagonal.
        assert_eq!(along(from, to, 25.), point(px(15.), px(20.)));
        // Overshooting clamps to the target instead of running past it.
        assert_eq!(along(from, to, 500.), to);
        // A zero-length run has no direction to walk in.
        assert_eq!(along(from, from, 5.), from);
    }

    /// A point on the cubic that [`stroke_lane_transition`] hands to the path
    /// builder, so the tests can assert the shape the builder will rasterize.
    fn lane_transition_point(from: Point<Pixels>, to: Point<Pixels>, t: f32) -> Point<Pixels> {
        let (control_a, control_b) = lane_transition_controls(from, to);
        let inverse = 1.0 - t;
        let weights = [
            inverse * inverse * inverse,
            3.0 * t * inverse * inverse,
            3.0 * t * t * inverse,
            t * t * t,
        ];
        let nodes = [from, control_a, control_b, to];
        let mut result = point(px(0.), px(0.));
        for (node, weight) in nodes.iter().zip(weights) {
            result.x += node.x * weight;
            result.y += node.y * weight;
        }
        result
    }

    /// The steepest horizontal rate the curve reaches, in pixels sideways per
    /// pixel down — the number that decides whether a transition reads as a
    /// flow or as a horizontal streak.
    fn steepest_horizontal_rate(from: Point<Pixels>, to: Point<Pixels>) -> f32 {
        const SAMPLES: usize = 200;
        let mut steepest: f32 = 0.;
        let mut previous = from;
        for step in 1..=SAMPLES {
            let current = lane_transition_point(from, to, step as f32 / SAMPLES as f32);
            let dx = f32::from(current.x - previous.x).abs();
            let dy = f32::from(current.y - previous.y).abs();
            if dy > f32::EPSILON {
                steepest = steepest.max(dx / dy);
            }
            previous = current;
        }
        steepest
    }

    #[test]
    fn test_lane_transition_height_follows_the_row_height_and_the_distance() {
        let row_height = px(22.);
        let room = px(10_000.);

        // The row height is the unit the transition is measured in: at twice
        // the line height the same lane change is twice as tall.
        assert_eq!(
            lane_transition_height(row_height * 2.0, LANE_WIDTH, room),
            lane_transition_height(row_height, LANE_WIDTH, room) * 2.0
        );

        // A hop to the neighbouring lane is one row; anything shorter still gets
        // a whole row rather than a sliver.
        assert_eq!(
            lane_transition_height(row_height, LANE_WIDTH, room),
            row_height
        );
        assert_eq!(
            lane_transition_height(row_height, LANE_WIDTH / 4.0, room),
            row_height
        );

        // Every further lane buys more vertical room, so a wide jump is never
        // squeezed into the same single row as a narrow one.
        let one_lane = lane_transition_height(row_height, LANE_WIDTH, room);
        let three_lanes = lane_transition_height(row_height, LANE_WIDTH * 3.0, room);
        let fifteen_lanes = lane_transition_height(row_height, LANE_WIDTH * 15.0, room);
        assert!(three_lanes > one_lane);
        assert!(fifteen_lanes > three_lanes);
        assert_eq!(fifteen_lanes, row_height * 8.0);

        // Direction of the jump does not change its height.
        assert_eq!(
            lane_transition_height(row_height, LANE_WIDTH * -15.0, room),
            fifteen_lanes
        );

        // But the room the segment actually has always wins: a transition never
        // runs past the row it has to land on, in either direction.
        assert_eq!(
            lane_transition_height(row_height, LANE_WIDTH * 15.0, row_height),
            row_height
        );
        assert_eq!(
            lane_transition_height(row_height, LANE_WIDTH * 15.0, -row_height),
            row_height
        );
    }

    #[test]
    fn test_lane_transition_controls_are_vertical_offsets() {
        let from = point(px(100.), px(40.));
        let to = point(px(20.), px(150.));
        let (control_a, control_b) = lane_transition_controls(from, to);

        // Both handles sit on their own lane, which is what makes the curve
        // leave and arrive parallel to the lanes instead of kinking into them.
        assert_eq!(control_a.x, from.x);
        assert_eq!(control_b.x, to.x);

        // And both are pulled along the transition's height, so the bend is
        // spread over it rather than concentrated at one end.
        let height = to.y - from.y;
        assert_eq!(
            control_a.y - from.y,
            height * LANE_TRANSITION_CONTROL_FRACTION
        );
        assert_eq!(
            to.y - control_b.y,
            height * LANE_TRANSITION_CONTROL_FRACTION
        );

        // A transition that runs upwards is the same curve mirrored, not an
        // inverted one.
        let (up_a, up_b) = lane_transition_controls(to, from);
        assert_eq!(up_a.x, to.x);
        assert_eq!(up_b.x, from.x);
        assert!(up_a.y < to.y);
        assert!(up_b.y > from.y);
    }

    #[test]
    fn test_lane_transition_curve_stays_inside_its_corridor() {
        let from = point(px(100.), px(40.));
        let to = point(px(20.), px(150.));

        assert_eq!(lane_transition_point(from, to, 0.), from);
        assert_eq!(lane_transition_point(from, to, 1.), to);

        let mut previous = from;
        for step in 1..=100 {
            let current = lane_transition_point(from, to, step as f32 / 100.);
            // Monotonic on both axes: the curve never doubles back over a row it
            // has already passed, nor over a lane it has already crossed.
            assert!(current.y >= previous.y, "y went back at step {step}");
            assert!(current.x <= previous.x, "x went back at step {step}");
            // And it stays within the rectangle spanned by its ends, so it can
            // only ever paint over the lanes it actually crosses.
            assert!(current.x >= to.x && current.x <= from.x);
            assert!(current.y >= from.y && current.y <= to.y);
            previous = current;
        }

        // The ends are tangent to the lanes: a step along the curve moves far
        // less sideways than it does down.
        let near_start = lane_transition_point(from, to, 0.02);
        assert!(f32::from(from.x - near_start.x) < f32::from(near_start.y - from.y));
        let near_end = lane_transition_point(from, to, 0.98);
        assert!(f32::from(near_end.x - to.x) < f32::from(to.y - near_end.y));
    }

    #[test]
    fn test_lane_transition_dot_clearance_follows_the_curve() {
        let from = point(px(100.), px(40.));
        let to = point(px(20.), px(150.));

        // Each end backs off along its own lane — the direction the curve
        // actually leaves and arrives in — and only the end that was asked for.
        let (head, tail) = clear_lane_transition_dots(from, to, 6., 0.);
        assert_eq!(head, point(from.x, from.y + px(6.)));
        assert_eq!(tail, to);
        let (head, tail) = clear_lane_transition_dots(from, to, 0., 6.);
        assert_eq!(head, from);
        assert_eq!(tail, point(to.x, to.y - px(6.)));

        // A clearance wider than the curve's own handle stops at the handle
        // rather than turning the transition inside out.
        let (head, tail) = clear_lane_transition_dots(from, to, 500., 500.);
        let (control_a, control_b) = lane_transition_controls(from, to);
        assert_eq!(head, control_a);
        assert_eq!(tail, control_b);

        // An upward transition backs off upward.
        let (head, _) = clear_lane_transition_dots(to, from, 6., 0.);
        assert_eq!(head, point(to.x, to.y - px(6.)));

        // A transition with no room to bend has no tangent to follow, so it
        // backs off along itself instead of failing to clear the dot at all.
        let flat_from = point(px(100.), px(40.));
        let flat_to = point(px(60.), px(40.));
        let (head, tail) = clear_lane_transition_dots(flat_from, flat_to, 6., 6.);
        assert_eq!(head, point(flat_from.x - px(6.), flat_from.y));
        assert_eq!(tail, point(flat_to.x + px(6.), flat_to.y));

        // …and on that path both ends walk the SAME segment towards each other,
        // so a clearance wider than half the span must stop at the midpoint
        // instead of letting the two ends cross over and invert the stroke.
        let (head, tail) = clear_lane_transition_dots(flat_from, flat_to, 30., 30.);
        let midpoint = point(px(80.), px(40.));
        assert_eq!(head, midpoint);
        assert_eq!(tail, midpoint);
        assert!(
            head.x >= tail.x,
            "the head must never overshoot past the tail"
        );
    }

    #[test]
    fn test_wide_lane_transitions_do_not_flatten_into_streaks() {
        let row_height = px(22.);
        let room = px(10_000.);
        let lane_change = |lanes: f32| {
            let span = LANE_WIDTH * lanes;
            let height = lane_transition_height(row_height, span, room);
            (point(px(0.), px(0.)), point(span, height))
        };

        let (narrow_from, narrow_to) = lane_change(1.0);
        let narrow_rate = steepest_horizontal_rate(narrow_from, narrow_to);

        let (wide_from, wide_to) = lane_change(15.0);
        let wide_rate = steepest_horizontal_rate(wide_from, wide_to);

        // A fifteen-lane fan-in is allowed to lean further than a single-lane
        // hop, but only by a small factor — it must still read as a diagonal.
        assert!(wide_rate > narrow_rate);
        assert!(
            wide_rate < narrow_rate * 2.0,
            "wide transition leans {wide_rate} px sideways per px down"
        );

        // Capping the height at one row — what this replaced — would have made
        // the same jump an order of magnitude flatter.
        let capped = steepest_horizontal_rate(wide_from, point(wide_to.x, row_height));
        assert!(capped > wide_rate * 5.0, "one-row cap leans {capped}");
    }
}
