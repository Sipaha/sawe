# Unify the two single-file diff views

**Status:** in progress (2026-09-02)
**Maintainer ruling that started this:** *«Вообще мне видится, что это должен быть
один компонент с флагом "editable". Я за рефакторинг.»*

Two types render a single file's diff today:

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
`SplittableEditor::sync_lhs_blame_sources` (`crates/editor/src/split.rs:627`)
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

```
1. An existing SoloDiffView anywhere in the workspace whose source equals this
   one → activate it (focus per the gesture) and stop. Never unpreview it.
2. Retarget, and the active pane's preview slot does not hold a SoloDiffView
   → do nothing and stop.
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
