# GPUI can assert on the painted element tree, and these crates were not using it

**Date:** 2026-09-02 · **Status:** confirmed · **Crates:** `gpui`, `ui`, `search`, `git_ui`, `editor`

Three regressions in one task (`docs/plans/2026-09-02-unify-the-two-diff-views.md`, task 4)
had the same shape: a predicate decided whether some UI got painted, the test asserted the
**predicate**, and the painted consequence was wrong. All three shipped green and were caught
by a reviewer reading the diff.

The stronger assertion exists and needs no new harness:

```rust
// after driving a real frame in a VisualTestContext
assert!(cx.debug_bounds("ICON-ArrowUp").is_some());
assert!(cx.debug_bounds("ICON-DiffUnified").is_none());
```

- `VisualTestContext::debug_bounds(selector) -> Option<Bounds<Pixels>>` looks up an element
  by selector **in the tree that was actually painted**, so it distinguishes "not rendered"
  from "rendered and empty" — which a predicate assertion cannot.
- `IconButton` registers its selector automatically: `ICON-{IconName:?}`
  (`crates/ui/src/components/button/icon_button.rs:47`). No test-only code in the component.
- Anything else opts in with `div().debug_selector(|| "…".into())`
  (`crates/gpui/src/elements/div.rs:796`). The `debug_selector` body is compiled out of
  release, so this costs nothing shipped.
- Precedent that already existed and was not noticed:
  `crates/search/src/buffer_search.rs` (`ICON-ReplaceNext`),
  `crates/agent_ui/src/agent_panel.rs` (`MENU_ITEM-Skills`, `KEY_BINDING-l`),
  `crates/editor/src/edit_prediction_tests.rs` (a plain `debug_selector` string, used for an
  occlusion check).

Traps worth knowing before reaching for it:

- **A selector that alternates is untestable by name.** The Unified/Split toggle draws
  `IconName::DiffSplit` or `IconName::DiffSplitAuto` depending on state, so neither name is a
  reliable assertion and that button is the one hole left in the diff toolbar's paint tests.
  If a button's icon is state-dependent, give the wrapper an explicit `debug_selector`.
- **You must drive a real frame.** `debug_bounds` reads the last painted tree; asserting
  before anything drew, or after only mutating state, tells you nothing. (Same family as the
  MCP `workspace.screenshot` trap: it renders the *retained* scene and cannot show a frame
  the app never painted — `docs/findings/2026-08-17-gpui-draw-phase-invalidation.md`.)
- **Assert both sides.** Two of the three regressions were one-sided coverage: something
  asserted the `false` branch and nothing asserted the `true` one, so the fix that broke the
  `true` branch stayed green.
