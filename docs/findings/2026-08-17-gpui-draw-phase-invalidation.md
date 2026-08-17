# gpui drops invalidations raised during a draw — and `workspace.screenshot` never draws

**Date:** 2026-08-17
**Status:** both facts confirmed by measurement on a live headless build
**Crates:** `gpui`, `editor`, `image_viewer`, `editor_mcp`

Two independent facts, found while fixing split-diff pane misalignment. Each
one on its own turns a correct-looking fix into a no-op, and together they
produce a fix that passes its unit test, looks verified in a screenshot, and
does nothing in the app.

## 1. `cx.notify()` inside layout or paint is discarded, not deferred

`Window::invalidate_view` (`crates/gpui/src/window.rs`):

```rust
if inner.draw_phase == DrawPhase::None {
    inner.dirty = true;
    cx.push_effect(Effect::Notify { emitter: entity });
    true
} else {
    false          // dropped outright
}
```

`App::notify` routes through this for any live window entity, and
`Window::refresh()` is gated the same way. So a `cx.notify()` raised from
`request_layout` / `prepaint` / `paint` pushes no effect at all: no deferred
delivery, no next-frame redraw, nothing. It is silent — the call compiles,
runs, and returns.

Two live bugs came from exactly this:

- **Split-diff panes misaligning** (`FORK.md` #62). Each pane's alignment
  spacers come from the companion's wrap rows, and a wrap width is only ever
  discovered from `EditorElement::prepaint`. Notifying the companion from there
  did nothing, so the frame that would paint the reconciled layout was never
  requested. Dragging the divider left the panes showing the same unchanged
  line on different rows until an unrelated redraw came along.
- **Image-viewer zoom readout going stale.** `ImageContentElement::prepaint`
  mirrors the rendered zoom back onto the view; the toolbar re-renders via
  `cx.observe`, so the readout kept the last level an event-path update had
  given it.

**Fix pattern:** `cx.defer(move |cx| entity.update(cx, |_, cx| cx.notify()))`,
which lands in the effect cycle after the frame. **Guard it on the value
actually having changed** — an unconditional deferred notify from prepaint
re-notifies every frame and spins the app.

**Testing trap:** `VisualTestContext::draw` does not run gpui's
`record_entities_accessed`, so the views under test are never registered as
live window entities and `App::notify` takes its "no live invalidators" branch,
pushing `Effect::Notify` regardless of draw phase. A test on that path
**cannot distinguish an inline notify from a deferred one** — it passes either
way. To cover this, drive frames through the real `Window::draw` (add the view
to a workspace), as `split::tests::test_split_panes_repaint_after_wrap_width_change`
does.

## 2. `workspace.screenshot` renders the retained scene; it does not run prepaint

`WgpuHeadlessRenderer::render_to_image` renders the `Scene` the window already
holds. It does **not** trigger a draw. Traced during this session: after a
divider drag, three consecutive `workspace.screenshot` calls and a
`windows.hover_at` produced zero further `DisplayMap::snapshot` calls on either
pane.

Consequences for agent-driven verification:

- **A screenshot can never show you a frame that was never drawn.** "I took
  three screenshots and the bug is still there" is not evidence that state is
  wrong — it may only mean nothing redrew. That is precisely how this session
  first mis-diagnosed the alignment fix as ineffective.
- Conversely, "the screenshot did not change" is not evidence that state did
  not change.
- Drive a **real event** before snapping when you need a fresh frame: a click,
  a keystroke, a dock toggle, a settings write. A second `windows.hover_at` a
  pixel away from the first is the cheapest nudge, and it is what makes a
  `ContextMenu` submenu actually appear in the PNG.
- Because a submenu or a prompt may already be open but unpainted, treat a
  missing overlay in a screenshot as "possibly not repainted yet", not as "the
  UI did not open it".

## How the two combine

The alignment fix was correct in the tree, invisible in the app (fact 1), and
its invisibility was then confirmed by screenshots that could not have shown
the fixed frame anyway (fact 2). Both had to be understood before the repro
could be trusted in either direction. When a UI fix "does not work", establish
first that a frame was drawn at all.
