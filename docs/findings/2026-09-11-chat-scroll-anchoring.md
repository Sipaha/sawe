# Stable upward conversation scrolling

Scope: `crates/gpui/src/elements/list.rs`.

## Cause

Solution chat uses the variable-height GPUI ListState. Wheel scrolling converted a pixel delta immediately into a new row index and an offset from its top using the SumTree's cached heights. A previously invisible wrapped row may be taller than its cached size (or completely unknown, whose summary height is zero). On the next layout, the row's real height was measured, but the estimated offset from its top survived. A 50px move into a row cached as 100px and actually 300px therefore landed 50px from its top instead of 50px from its bottom, moving existing text by an extra 200px.

## Fix

Upward wheel movements crossing the previous viewport's top row preserve that known row plus the signed pixel displacement as a temporary FromBelow anchor. Layout walks backward from it, measuring each crossed row before subtracting its height from the requested travel. It rebuilds only that contiguous measured slice in the SumTree, then uses the normal visible-item layout. Extra discovered height extends above the visible position.

MeasureLast uses the known anchor, not the provisional estimate-derived destination, when deciding prefetch. This prevents a cold zero-height prefix from turning a small wheel movement into a full-history measurement request. Existing per-item absolute streaming anchors and proportional resize anchors remain in place. The temporary wheel anchor is cleared by explicit navigation/reset/scrollbar actions, preserved across remeasurement, and shifted across splices (or discarded if its row is removed).

No Solution rendering code, prompt/status/compact UI, estimate floor or prefetch batch size changed. No all-history eager layout introduced. Reverse measurement costs one traversal of actually crossed rows plus a SumTree slice update; visible rows may be laid out again by the existing render pass.

The paint callback retains the original painted anchor across multiple events. Generic `ScrollDelta::coalesce` discarded accumulated motion on direction reversal, so the list now accumulates signed pixel deltas locally. Up 70px then down 20px before repaint therefore means up 50px. Other widgets retain their existing coalescing behavior.

## Deterministic regressions

- `test_upward_scroll_anchors_expanding_row_after_coalesced_events`: row 2 was measured at 100px, goes offscreen and grows to 300px; viewport starts at row 3. Coalesced wheel events of +70px then -20px before the next draw yield row 2 offset 250px and keeps row 3 at screen y=50px.
- `test_upward_scroll_measures_only_crossed_cold_rows`: 1000 rows with an unmeasured prefix, viewport at row 900. A 50px upward movement measures row 899 (300px), lands at offset 250px, renders fewer than 10 rows, does not reengage tail-follow, and still supports explicit return to tail.

## Verification

All 24 GPUI list tests passed on the merged tree. The real headless chat probe uses long wrapped paragraphs and fixed wheel deltas; a premeasured baseline may not reproduce the stale-height race, so the stale-height and cold-prefix regressions provide the direct proof.

The final debug editor smoke moved a wrapped paragraph marker from y=754 to y=904 for an upward 150px wheel movement across a message boundary. Both cold Errored context-menu actions were enabled at 25% usage. Screenshots are stored outside Git.

Debug and release-fast builds and scoped debug clippy completed successfully.
