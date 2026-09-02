# Unify the two single-file diff views

**Status:** complete (2026-09-02) — commits `85fdf77f38..4875ae53c5`, thirteen on `main`.
FORK.md decisions **#136** (the unification + the gesture model) and **#137** (who paints
the diff style controls), with **#125** amended and **#100**'s stale call reference
corrected. See "What actually shipped" at the bottom for every place the implementation
diverged from what is written above.

**Maintainer ruling that started this:** *«Вообще мне видится, что это должен быть
один компонент с флагом "editable". Я за рефакторинг.»*

Two types rendered a single file's diff *before this work*. The table below, and every
present tense in the sections up to "What actually shipped", describe that starting state —
they are kept as the record of what was merged, not as a description of the code today.

| | `SoloDiffView` (`crates/git_ui/src/solo_diff_view.rs`) | `CommitView` single-file mode (`crates/git_ui/src/commit_view.rs`) |
|---|---|---|
| Right-hand side | live project buffer | detached historic blob (`GitBlob`, `DiskState::Historic`) |
| Left-hand side | `project.open_uncommitted_diff` base (`git show :<path>` — the **index**) | the file's text at `<sha>^` |
| Editable / saveable | yes | no (`Capability::ReadOnly`) |
| Hunk staging | yes (editor default controls) | no (`disable_diff_hunk_controls`) |
| Left-pane blame | `set_lhs_blame_base(Some("HEAD"))` | unwired |
| Multibuffer | `MultiBuffer::singleton` over the project buffer | `MultiBuffer::without_headers(ReadOnly)` + `update_excerpts_for_path` |
| Toolbars | `SoloDiffStyleToolbar` (hunk nav, unified/split, soft wrap) + `SoloDiffGitToolbar` (status, `+N −M`, "N differences") | `CommitViewToolbar` (`+N −M`, "N differences", buffer search, Show in Git Graph, View on provider) |
| `can_split` | `false` (trait default) | `true`, with a real `clone_on_split` |
| Hunk count | memoised (`HunkCountCache`) | recomputed on every toolbar render |
| First-hunk jump on open | yes | no |
| Soft wrap | toolbar button | nothing |
| Opened by | Changes tab | Commit tab |

Everything else is drift, not a difference.

Two facts worth stating because they are easy to get backwards:

- **`CommitViewToolbar` has no mode branch.** It matches on
  `act_as_type::<CommitView>` alone, so a single-file commit diff *does*
  currently get the diff stat, the "N differences" label, buffer search, Show
  in Git Graph and View on provider. The merge must carry those over or it is a
  regression. (The merge-parent toggle is hidden only incidentally — `parents`
  is never loaded in compact mode.)
- **Read-only-ness lives on the multibuffer, not the buffer.** `build_buffer`
  builds every historic blob `Capability::ReadWrite`
  (`commit_view.rs:1397`); it is `MultiBuffer::without_headers(ReadOnly)` that
  makes the view read-only. The merged view must set the capability from the
  source, not from the buffer.

## The two essential differences — and the one that isn't

**Essential:** editability and hunk staging. Both follow from the single fact
that the right-hand side is a live project buffer in one case and a detached
blob in the other.

**Not essential — blame.** The maintainer challenged the earlier claim that
blame was a third essential difference and was right: the blame base is a
*parameter* (`HEAD` for the working tree, `<sha>^` for a commit), not a
property of the view. The one real obstacle is mechanical:
`SplittableEditor::sync_lhs_blame_sources` (`crates/editor/src/split.rs`)
resolves `(repository, repo_path)` through `repository_and_path_for_buffer_id`
on the **right-hand** buffer id, which a detached blob cannot answer. FORK.md
#59 records this as "deliberately unwired", not impossible.

**This refactor does not wire commit-mode blame.** It makes the base a value
derived from the source (so it can never drift from what the view is showing),
and leaves the commit source's base as `None` — exactly today's behaviour —
with the obstacle written down. Wiring it needs an explicit repository
override on `SplittableEditor`, which is a separate change with its own tests.

## The gesture model — the maintainer's, superseding FORK.md #125

> *«Если вкладка с диффом закрыта, то двойной клик на любом файле в changes и в
> commit ее открывает. Дальше любой клик на файле в changes и в commit меняет
> содержимое вкладки.»*

One shared diff tab per pane, across **both** tabs:

| gesture | behaviour |
|---|---|
| double click (either tab) | **summon** the shared diff into the pane's preview slot; never pin; focus stays in the panel |
| — with `preview_tabs.enabled: false` | no shared slot exists, so every summon falls back to a **permanent, focused** tab (`add_to_pane`). Both halves of the row above are off under that setting, deliberately. |
| single click (either tab) | **retarget** the shared diff if it is open; do nothing if it is not |
| arrow-key step (Changes) | same as single click — retarget only |
| `menu::Confirm` / Enter (Changes) | summon **and** focus the diff; still never pins |
| `menu::SecondaryConfirm` (cmd-click / alt-enter) | unchanged — the stacked `ProjectDiff` accordion |

**Double click must not pin, and neither must Enter.** Pinning promotes the item
out of the preview slot, the retarget guard goes false, and the next single
click summons a *second* tab — the exact complaint #125 was written to fix.
Pinning stays reachable through Zed's own double-click-on-the-*tab* gesture and
`TogglePreviewTab`.

This is a **behaviour change for the Changes tab**, which today pins on double
click (`SoloDiffOpen::Permanent`) and summons a preview from nothing on every
single click and every arrow-key step (`changes_list.rs:855-869`,
`git_panel.rs:1583`). After this change the Changes tab behaves exactly like the
Commit tab already does.

### The one open algorithm both tabs call

`SoloDiffOpen { Preview, Permanent }` is replaced by a gesture that says what
the *user* did, not where the tab should land:

```rust
pub enum DiffOpen {
    /// Double click, or Enter. Opens the shared diff if it is not open.
    Summon { focus: bool },
    /// Single click, arrow-key step. Only ever changes what an already-open
    /// shared diff is showing.
    Retarget,
}
```

**Corrected after implementation — steps 1 and 2 ship in the opposite order.** The plan said
search first; `SoloDiffView::resolve_gesture` declines first. Gating the whole call is what
the pre-unification Changes tab did (`preview_is_solo_diff` in `move_diff_to_entry`), so a
declined arrow step never reached a workspace-wide activate. Search-first lets an arrow-key
step flip a pane onto a pinned diff and never flip back — a live regression in the
maintainer's daily path. The guard is the *only* mode-specific step: a `Retarget` that passes
it reaches the same workspace-wide reuse as a `Summon` and will activate a match in any pane.
A declined retarget never searches — which is the whole cost, and it is the intended one.

```
1. Retarget, and the active pane's preview slot does not hold a SoloDiffView
   → do nothing and stop.
2. An existing SoloDiffView anywhere in the workspace whose source equals this
   one → activate it (focus per the gesture) and stop. Never unpreview it.
3. Load the source; build the view.
4. Previews enabled → take the preview slot (`replace_preview_item_id`) and
   add the item with `focus` from the gesture.
   Previews disabled → append a permanent tab, focused. (This is the
   pre-preview fallback `test_previews_disabled_falls_back_to_permanent_tabs`
   already pins; it stays.)
```

Nothing in this path calls `unpreview_item_if_preview` any more — that call is
what pinning was, and pinning is gone.

**Conflict routing follows the summon gesture, not the pin gesture.** Today a
conflicted file opens the merge resolver on `Permanent` and does nothing on
`Preview` (`git_panel.rs:1734-1739`), because `Preview` fired on every
arrow-key step. Under the new model, `Summon` opens the resolver and `Retarget`
does nothing — same rule, expressed against the gesture that survived.

## Design

### `DiffSource` — one enum, capabilities derived

```rust
pub enum DiffSource {
    /// Uncommitted changes. The right-hand side is a live project buffer, so
    /// the view is editable and its hunks are stageable.
    WorkingTree { repository: Entity<Repository>, repo_path: RepoPath },
    /// A file as of a commit, against its parent. Both sides are detached
    /// blobs: read-only, no staging.
    Commit { repository: Entity<Repository>, repo_path: RepoPath, sha: SharedString },
}
```

Everything the two modes disagree about is a method on the source, not an `if`
scattered through the view: `is_editable()`, `capability()`, `blame_base()`,
`tab_icon()`, `tab_title()`, `tab_tooltip()`, `identity()` (for dedupe and for
the panel's open-diff mark).

The type stays `SoloDiffView` in `solo_diff_view.rs`. "Solo" already means *one
file's diff*, which is true of both sources; renaming it would churn the two
toolbars, `zed.rs`, `git_panel.rs` and every test for no functional gain.

### Task breakdown

**1. Extract the historic-blob loader.** `GitBlob`, `build_buffer`,
`build_buffer_diff` and the per-file "blob → (buffer, BufferDiff, FileStatus)"
body of `CommitView::new`'s loop move into a new
`crates/git_ui/src/commit_blob.rs`. Pure extraction, no behaviour change:
`CommitView` calls it in its loop, and the unified view will call it once.
`CommitDiffAddon` stays in `commit_view.rs` for now (it holds a
`WeakEntity<CommitView>`); the extracted loader returns the `FileStatus` so
either caller can build its own addon.

**2. Teach `SoloDiffView` the `Commit` source.** Add `DiffSource`, derive the
multibuffer shape (singleton vs `without_headers`), the capability, the hunk
controls, the diff-review button, the blame base, the tab title / icon /
tooltip. Add `SoloDiffView::open_commit_file(...)`. Drop the redundant second
`SettingsStore` observer (`SplittableEditor::new` already installs one —
`split.rs:561`).

**3. Repoint both tabs and unify the gestures.** The Commit tab opens
`SoloDiffView` with a `Commit` source; `CommitView` loses `single_file`,
`open_file_diff` and `preview_holds_single_file_diff`. One guard —
"the preview slot holds a `SoloDiffView`" — serves both tabs, because the two
surfaces no longer have separate item types to type-check each other with;
retargeting is by *source identity*, not by path. `OpenDiff::from_active_item`
reads the source; the `CommitView` downcast survives only for the whole-commit
case (`file: None`).

**4. Converge the toolbars and `can_split`.** `SoloDiffStyleToolbar` now
applies to commit diffs too (they gain hunk nav, unified/split and soft wrap,
which they never had). `SoloDiffGitToolbar` becomes source-aware: working-tree
status icon + `+N −M` for `WorkingTree`, short sha + `+N −M` for `Commit`,
"N differences" for both (now memoised in both cases), plus buffer search, Show
in Git Graph and View on provider for `Commit` so the single-file commit diff
does not lose what `CommitViewToolbar` was giving it. `CommitViewToolbar`
itself is untouched and keeps serving whole-commit and compare-range views.

Also converge `can_split`: implement `can_split` + `clone_on_split` on the
unified view for both sources (a new `SplittableEditor` over the same
multibuffer, the way `CommitView::clone_on_split` already does it). Without it
the commit source loses a gesture it has today.

**5. Port the tests and the docs.** The five gesture tests in
`git_panel/commit_tab.rs` and `test_open_diff` / the two open-diff-mark tests
in `git_panel.rs` are **ported, not deleted** — they encode the model and must
keep encoding it after the type changes. New tests for the Changes-tab
gesture change (single click summons nothing; double click does not pin).
FORK.md: amend #125 to the new model, add a decision for the unification.

`ProjectDiff` (the stacked accordion) is out of scope. So is whole-commit
`CommitView` and its `compare_range` mode.

### Things that will bite

- `HunkCountCache` and `difference_count_label` live in `solo_diff_view.rs` and
  are consumed by `project_diff.rs` and `commit_view.rs`. They must survive.
- `SoloDiffView` implements `EventEmitter<EditorEvent>` and `to_item_events` but
  never subscribes to its editor and never emits — so no `ItemEvent` ever
  reaches the pane, which is why an edit does not currently promote the preview
  tab out of the slot. Do not "fix" that here: it would make every edit in a
  working-tree diff pin the shared tab and break the gesture model. (It is also
  why `is_dirty` is missing — a separately-tracked bug, out of scope.)
- `SplittableEditor::new` already installs a `SettingsStore` observer and its
  own deferred `split()`; `SoloDiffView` installs a second. Remove ours.
- `sync_lhs_blame_sources` prunes entries whose base buffer is no longer
  excerpted, so it must run **after** `update_excerpts_for_path`, never before.
- `rustfmt` descends into `mod` declarations: formatting `commit_view.rs`
  reformats `commit_view/*.rs` too.
- `CommitView::open`'s `file_filter` parameter is **not** the single-file mode —
  it narrows the diff while keeping the whole-commit chrome. Only
  `open_internal`'s separate `single_file_mode: bool` selects the mode. Every
  current `open` caller passes `file_filter = None`; do not delete the
  parameter as part of removing `single_file`.
- `ExplainCommit` is bound on `CommitView`'s root element unconditionally, so
  it is dispatchable in single-file mode today and spawns an AI task whose
  output nothing renders. Removing single-file mode from `CommitView` closes
  that by construction — do not re-add the binding to the unified view.
- The Changes list is `crates/git_ui/src/git_panel/changes_list.rs`
  (`render_status_entry`, click handler at `:854-869`). One explore pass
  claimed that file does not exist; it does.
- The Commit tab has **no keyboard route at all** — `commit_tab.rs` has zero
  `on_action`, and `git_panel.rs:6585` registers the selection movers and
  `open_diff` only when `active_tab == GitPanelTab::Changes`. This refactor
  does not add one; the maintainer asked about clicks.
- `SoloDiffView::open_or_focus` searches the whole workspace for an existing
  item; `CommitView::open_internal` searches the active pane only. The merged
  algorithm takes the workspace-wide search — it is the wider of the two and
  only ever prevents a duplicate tab.
- `Pane::set_preview_item_id` is `pub(crate)` to `workspace`; `git_ui` can only
  reach the slot through `replace_preview_item_id` /
  `unpreview_item_if_preview` / `preview_item_id`.

## Done when

One view type serves both tabs; the gesture model above holds in a live
headless probe; the ported tests pass; `cargo check --workspace --all-targets`
is clean of **errors and warnings**; FORK.md #125 is amended.

---

## What actually shipped

Thirteen commits, `85fdf77f38..4875ae53c5`, all on `main`. Five tasks: extract the blob
loader (`1bbd811424`), teach `SoloDiffView` the `Commit` source (`5a3b279610`), repoint both
tabs and unify the gestures (`68ef040316` `20c695d7fb`), converge the toolbars and
`can_split` (`b26c6bd0ea` `a35b037b9a` + three fix rounds `8dc64b6a10` `eed754f552`
`8754c9d60c` `bbfe0be8be` `182df8b042` `4875ae53c5`), and this documentation pass. Final
gates: `cargo check --workspace --all-targets` clean of errors **and** warnings; `search` 70
tests, `git_ui` 387, `editor` 799.

Where the implementation diverged from the design above:

- **The retarget/reuse step order is reversed** — corrected inline in "The one open algorithm
  both tabs call" above.
- **`CommitView::open_internal` was folded into `open`**, and its now-permanently-false
  `preview` parameter deleted. All eleven production callers passed `file_filter = None` and
  reached the non-preview branch. `file_filter` itself survives, exactly as the plan warned.
- **Buffer search is offered for *both* sources, not commit-only.** `as_searchable` already
  returns this view's editor whatever it is showing, `QuickActionBar` downcasts to the
  concrete `Editor` type and so is genuinely hidden here, and a button that blinked out
  whenever a single click retargeted the shared tab from an uncommitted change to a commit's
  file is the chrome drift this refactor exists to undo.
- **`load_commit_file_blob` kept `&mut AsyncWindowContext`** rather than narrowing to
  `AsyncApp`, overturning Task 2's brief: in this GPUI tree `AsyncApp::update` is infallible
  and reaches its `App` through `.upgrade().expect(..)`, so narrowing would trade five
  recoverable `?` short-circuits for a production panic. The gain that is actually lost is
  small — a commit-file load now survives its window closing and wastes the work.
- **A fifth crate got involved that the plan did not anticipate: `search`.** Making the
  commit source's multibuffer non-singleton exposed that `BufferSearchBar` paints its own
  copy of the diff style controls into the same `PrimaryLeft` slot as `SoloDiffStyleToolbar`.
  Closing that took three review rounds and produced FORK.md #137 — `SplittableEditor::
  set_style_controls_painted_by_consumer`, `BufferSearchBar::{paints_diff_style_controls,
  keeps_primary_left, has_files_to_collapse}`, and paint tests over both the dismissed and
  the shown element tree.
- **`file_diff_view` gained the diff style controls it never had** — prev/next hunk, Unified,
  Split — as a deliberate, reviewed widening. It is the only `SplittableEditor` consumer
  affected; its sibling `text_diff_view` already had them; and the carve-out that would have
  excluded it is the exact class of gate that caused two of the three regressions.
- **The async toolbar-location hole is compare-range only**, not "any multi-file commit
  view": `MultiBuffer::new` sets `show_headers: true` in the constructor and
  `without_headers` leaves it `false`, so a headered multibuffer answers the collapse
  predicate `true` even with zero buffers. Only `CommitView`'s compact mode
  (`compact = compare_range.is_some()`) is headerless.
- **The gesture tests moved from task 5 to task 3.** Task 3 could not compile with the five
  `commit_tab.rs` gesture tests and `test_open_diff*` unchanged, so it ported them in the
  same commit. Task 5 is documentation only.

## Deferred, with evidence

Recorded in `TODO.md` (section C), because they are durable and user-visible:

- **A diff pane split twice trips a debug assertion** (`crates/editor/src/display_map.rs:303`
  via `SplittableEditor::unsplit` on a clone that shares its multibuffer). Pre-existing —
  `CommitView` has had `can_split = true` and a multibuffer-sharing `clone_on_split` all
  along (`20c695d7fb:crates/git_ui/src/commit_view.rs:1233,1259`). What is **newly exposed**
  is the working-tree diff, which had no `can_split` before task 4. → `TODO.md` C6.
- **`SoloDiffView` does not override `Item::is_dirty`** while `can_save` is true for the
  working-tree source. → `TODO.md` C7.
- **`CommitView::select_parent_index` refreshes only `diff_files`**, so the merge-parent
  toggle changes the affected-files list and not the diff. → `TODO.md` C8.
- **A split pane has no preview item**, so a single-click retarget from the git panel does
  nothing there until the user activates the original pane. → `TODO.md` C9.

Deliberately *not* in `TODO.md` — real, but too narrow to earn a row there:

- `commit_view.rs`'s replace-lookup matches on `view.commit.sha == commit_sha`, which also
  matches a `compare_range` view whose head sha equals it, so opening the whole-commit view
  for X after a `base..X` comparison removes the comparison tab. The pre-change predicate
  matched identically; it is a one-line predicate fix whenever someone is next in that
  function.
- Buffer search offers Replace on the read-only commit diff; `Editor::edit` early-returns on
  `read_only`, so it silently no-ops. Pre-existing for `CommitView`, and only reachable by
  explicitly toggling replace mode.
- Two `MultiBufferSnapshot` clones per render on the commit path, and a `multibuffer` field
  on `SoloDiffView` that duplicates a handle reachable through the editor — `SplittableEditor`
  has no `rhs_multibuffer()` accessor, which is why.
- The paint tests assert three of the four style buttons; Split is skipped because its icon
  alternates `DiffSplit`/`DiffSplitAuto`, so a change dropping only that button would survive.
  Recorded in FORK.md #137.
- `Event::UpdateLocation` (`crates/search/src/buffer_search.rs`) is emitted three times and
  subscribed nowhere — a dead event, pre-existing.
- `has_collapse_button` now means "the leading group is non-empty" rather than "there is a
  collapse chevron"; the alignment use is correct but the name has drifted.
- `test_reopening_the_commit_view_spares_the_file_diff_tab` still reproduces the original
  pane geometry and still guards the bug, but can no longer express the original mutation
  (`single_file` dropped from the lookup key) — that mutation is unexpressible now.

From the final whole-branch review, which found **nothing that must be fixed before push**
(and six earlier ledger minors already closed by later rounds):

- **Visibility that the extraction widened past its callers.** Four items in
  `commit_blob.rs` are `pub(crate)` with no cross-module caller, and its `build_buffer_diff`
  now name-collides with an unrelated one in `file_diff_view.rs`. `DiffSource::repository()`
  is `pub` with a single same-file caller. `SplittableEditor::diff_hunk_controls_disabled()`
  and `lhs_blame_base()` are `pub` `editor` API whose only callers are `git_ui` tests — real
  API surface bought for test observability, which is a trade worth making deliberately
  rather than by accident.
- **`alignment_element()`'s fixed `size_5` spacer under-indents the replace row** now that
  the leading group can be five buttons wide. Pre-existing, and only visible at that width.
- **`CommitView::open`'s `file_filter` is dead in practice**, with two latent
  inconsistencies behind it: `OpenDiff::from_active_item` hardcodes `file: None` for any
  `CommitView`, and the replace-lookup dedupe keys on sha alone. Neither is reachable while
  every caller passes `None`; both become wrong the moment one does not. Whoever revives
  `file_filter` owns all three.
- **The LSP Logs toolbar changed** as a side effect of `has_files_to_collapse` — the leading
  group's lone collapse chevron is gone and the bar no longer holds `PrimaryLeft` while
  dismissed. The button was never functional there; recorded in FORK.md #137 so the next
  person does not read it as a regression.
