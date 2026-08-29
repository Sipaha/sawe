# Git Panel `Changes | Commit` (phase 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the git panel's tabs become **Changes | Commit**. **History is deleted outright.** The git graph loses its inline commit-details surface entirely; selecting a commit in the graph instead opens a **closable Commit tab** in the git panel, and clicking one of that commit's files opens the file's diff *for that commit* in the centre pane. A multi-row graph selection shows a bare "N commits selected" summary.

**Spec:** `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md` §5. Phases 1, 2a and 2b are complete (`docs/plans/2026-08-26-solution-scoped-sessions.md`, `…-solution-band-layout.md`, `2026-08-27-solution-band-height.md`, `2026-08-27-solution-band-utility-section.md`). Phase 2b is what makes this phase necessary: the graph now lives in the band's compact utility half, where a 300px-minimum detail sidebar does not fit.

**Tech Stack:** Rust, GPUI. Crates touched: `git_ui`, `git_graph`, and the three keymap assets. No `workspace` / `solution_agent` / `zed` changes are expected.

**Architecture:** the graph's detail surface is *relocated*, not rebuilt. Its changed-files tree, message rendering and diff-stat fold move down into `git_ui` (`git_graph` depends on `git_ui`, never the reverse), where a new `crates/git_ui/src/git_panel/commit_tab.rs` — a sibling `impl GitPanel` block next to `changes_list.rs` — renders them as the Commit tab. Communication is **graph → panel by direct typed call** (the graph already looks the panel up), and **panel → graph by GPUI event** (`GitPanel` emits, `GitGraph` subscribes). Neither direction needs the string-named-action IoC trick.

---

## Facts established by recon (authoritative — do not re-derive)

Two read-only agents mapped the panel and the graph. Everything below is cited; where the spec was wrong, that is called out.

### Dependency direction and wiring

1. **`git_graph` depends on `git_ui`; `git_ui` does NOT depend on `git_graph`** (`crates/git_graph/Cargo.toml:36` `git_ui.workspace = true`; no `git_graph` entry in `crates/git_ui/Cargo.toml`). Today `git_ui` reaches the graph only through string-named action dispatch (`crates/git_ui/src/commit_context_menu.rs:361`) and the `providers` IoC registry (`crates/git_ui/src/providers/log_data_source.rs:10,56`). **This is the single biggest constraint on the phase and spec §5 does not mention it.**
2. **The graph can already look the panel up**: `workspace.panel::<git_ui::git_panel::GitPanel>(cx)` at `crates/git_graph/src/git_graph.rs:1457`, and `GitGraph` holds `workspace: WeakEntity<Workspace>` (`git_graph.rs:1588`, set in `GitGraph::new`, `:2160`).
3. **`GitPanel` already emits two event types** — `impl EventEmitter<Event>` (`crates/git_ui/src/git_panel.rs:6652`) and `impl EventEmitter<PanelEvent>` (`:6654`). `pub enum Event { Focus }` (`git_panel.rs:417-420`) is the extension point for a panel→graph signal.
4. **`GitGraphPanel` re-creates its inner `Entity<GitGraph>` whenever the active repository changes** (`git_graph_panel.rs:113-131`, constructing at `:126-129`). Any subscription installed on the graph must be installed in `GitGraph::new` (or re-installed there), not once at panel construction. `GitGraphPanel` itself emits nothing and has no `Panel` impl (`git_graph_panel.rs:1-14`); it is installed into the band's keyed slot from `crates/zed/src/zed.rs:818-819` + `:891-896`.
5. **The band holds an `AnyView`** (`crates/workspace/src/workspace.rs:1466`, read back at `:3412-3414`) and never downcasts `UtilityKind::GitGraph`. The band is **not** a viable message bus; go through `Workspace`.

### The graph's detail surface (what is being deleted)

6. **SPEC CORRECTION — it is a right-hand sidebar, not a bottom strip.** `GitGraph::render` is an `h_flex()` (`git_graph.rs:4477`) whose last two children, gated on `selected_entry_idx.is_some()`, are a **column** divider (`render_commit_view_resize_handle`, `:4332-4365`, `cursor_col_resize`) and the panel itself (`:3846-3852`, `v_flex().min_w(px(300.))`). What *is* a bottom strip is the message region **inside** that sidebar, split from the file tree by a `cursor_row_resize` handle (`:4300-4330`). Deleting it therefore reclaims **horizontal** width — which is what the compact band actually needed.
7. **Visibility is purely `selected_entry_idx.is_some()`** (`git_graph.rs:4695`). No toggle action, no setting, no keybinding anywhere references the sidebar. Neither split ratio is persisted (`serialize`'s column list, `git_graph.rs:5017-5099`, has no split columns); the only persisted selection value is `selected_sha` (`:5024-5027`).
8. **State to delete** (fields on `GitGraph`): `selected_commit_diff` (`:1632`), `selected_commit_branches` (`:1636`), `commit_detail_text` (`:1637`), `selected_changed_file` (`:1641`), `_commit_diff_task` (`:1642`), `_commit_branches_task` (`:1643`), `commit_details_split_state` (`:1644`), `commit_detail_split_state` (`:1645`), `changed_files_scroll_handle` (`:1647`), `collapsed_changed_dirs` (`:1651`), `commit_message_scroll_handle` (`:1652`); initialisers at `:2255`, `:2263-2270`, `:2278-2282`.
9. **Types to delete**: `DraggedSplitHandle` (`:213`), `DraggedDetailSplitHandle` (`:215`), `SelectedCommitDiff` (`:597`), `LoadedCommitDiff` (`:603`), `CommitDetailText` (`:615`), `CommitBranches` (`:491`), `SplitState` (`:639-691`), `DetailSplitState` (`:693-755`), `MIN_DETAIL_REGION_HEIGHT` (`:700`), `DEFAULT_MESSAGE_HEIGHT` (`:704`), `BRANCHES_CONTAINING_DEBOUNCE` (`:200`), `RESIZE_HANDLE_WIDTH` (`:197`), `MAX_LISTED_BRANCHES` + `format_branches_containing` (`:491-528`).
10. **Methods to delete**: `toggle_changed_directory` (`:3193`), `deploy_commit_detail_context_menu` (`:3503-3585`), `deploy_changed_file_context_menu` (`:3592-3609`), `commit_detail_text` (`:3680-3709`), `commit_detail_for_render` (`:3716-3730`), `render_commit_detail_panel` (`:3732-4057`, ~325 lines), `render_commit_detail_resize_handle` (`:4300`), `render_commit_view_resize_handle` (`:4332`); the `render` block at `:4686-4698`; the two async loads inside `select_entry` (`:2986-3067`); the cleanup lines in `clear_selection` (`:2848-2856`) and `select_entry` (`:2960-2972`).
11. **Code that must MOVE to `git_ui`, not be deleted** — the Commit tab needs all of it:
    - `split_commit_message` (`:530-559`) and `commit_identity_source` (`:561-595`), plus their tests `test_split_commit_message` (`:8427`) and `test_commit_identity_source` (`:8454`).
    - `ChangedFileEntry` (`:218-380`, incl. its `render` at `:289-380`), `ChangedFileRow` (`:381-397`), `build_changed_file_rows`, `render_changed_directory_row` (`:434-490`), plus `test_build_changed_file_rows_groups_by_directory` (`:8510`) and helpers `changed_file_entry` (`:8500`) / `describe_changed_file_row` (`:8836`).
    - `compute_diff_stats` (`:1567-1581`) — the **only** implementation of a commit's +/− totals anywhere in the tree.
    - `detail_text_style` (`:1527-1565`) — the Markdown style for the message block.
12. **Shared with the rest of the graph — do NOT delete**: `get_remote` (`:3292-3310`, also used by `deploy_commit_context_menu`), `render_chip` (`:2520-2559`, also used by `render_table_rows` at `:2727`), `is_head_ref` (`:2501`), and every `fetch_commit_data` call (the row renderer depends on them, `:2600`, `:2673`). `CommitView::open` still backs `open_commit_view` (`:3264`); only the `CommitView::open_file_diff` call site (`:259-276`) leaves the graph.

### The graph's selection model

13. `selected_entry_idx: Option<usize>` (`:1592`), `selected_entry_idxs: HashSet<usize>` (`:1593-1598`, documented invariant: empty exactly while `selected_entry_idx` is `None`, otherwise always contains it), `selection_anchor_idx` (`:1599-1602`). **All private; `selected_commit_sha()` (`:1807-1810`) is private too** — phase 3 must add accessors.
14. Indices are **view-space**; view row 0 can be a synthetic "Local Changes" row. Convert with `view_to_data_idx` / `data_to_view_idx` (`:2904-2918`).
15. Mouse selection: `on_row_click` (`:3206-3230`) → `RowSelectionGesture` (`:1660-1671`) → `apply_row_click_selection` (`:3232-3254`) → pure `fold_row_click` (`:1673-1758`) → `select_entry`. Keyboard: `select_first/prev/next/last` (`:2858-2896`). Programmatic: `select_commit_by_sha` (`:3123-3170`, public), `set_repo_id` (`:3106-3121`), and the re-anchor in `invalidate_state` (`:1786-1805`). **Every route except a Ctrl/Shift click collapses to one row** (`select_entry` clears and re-inserts, `:2937-2942`).
16. Deselect: `clear_selection()` (`:2848-2856`, `fn(&mut self)` with no `cx` — it does not notify; callers do). Callers: `cancel` (Escape → `menu::Cancel`), the sidebar's own ✕ (`:3915-3919`), `set_repo_id` (`:3116`). There is no click-empty-space-to-deselect.
17. **There is NO usable selection-change event today.** The only emitter is `impl EventEmitter<ItemEvent> for GitGraph` (`:4765`); selection emits `ItemEvent::Edit` (`:3069`), a payload-free "re-serialize me" signal also fired for filter and toolbar changes (`should_serialize`, `:5100-5105`).
18. `select_entry` is reached from `invalidate_state` / deserialize paths, so a synchronous `workspace.update(...)` inside it risks re-entrancy. The `cx.defer_in` precedent is `git_graph_panel.rs:141-143`.

### The git panel's tab model

19. `enum GitPanelTab { Changes, History }` (`crates/git_ui/src/git_panel.rs:431-434`, private), field `active_tab` (`:841`, initialised `Changes` at `:1072`), `render_tab_bar` (`:5284-5350`, called from `Render` at `:6585-6587` and **hidden while `commit_editor_expanded`**), body dispatch `match self.active_tab` (`:6588-6627`), private `set_active_tab` (`:5437-5456`).
20. **No tab is closable today.** `render_tab_bar` hand-rolls two `h_flex().flex_1()` rows; `ui::Tab` is imported only for `Tab::container_height(cx)` (`:5330`, `:4676`). The ✕ must be built by hand.
21. **Tab selection is not persisted and has no setting.** `SerializedGitPanel { amend_pending, signoff_enabled }` (`:422-428`); nothing tab-related in `git_panel_settings.rs:47-69`. The panel always boots on Changes.
22. `dispatch_context` (`:1215-1227`) adds `GitPanel` + (`CommitEditor` | `menu`+`ChangesList`) and **does not vary by active tab**.
23. Actions `ActivateChangesTab` / `ActivateHistoryTab` (`:120-123`), handlers (`:5419-5435`), registration (`:6577-6578`), keymaps `ctrl-1`/`ctrl-2` in context `"GitPanel"` (`assets/keymaps/default-linux.json:1017-1021`, `default-windows.json:987-991`, `default-macos.json:1074-1078`).

### History's footprint

24. **Delete outright**: fields `commit_history_scroll_handle` / `commit_history_shas` / `focused_history_entry` / `history_keyboard_nav` (`:842-845`, init `:1073-1076`); `render_history_tab` (`:5351-5364`); `select_next_history_entry` (`:5366`), `select_previous_history_entry` (`:5382`), `open_selected_history_commit` (`:5398-5417`); `history_log_source` (`:5458-5478`), `preload_commit_history` (`:5480-5493`), `load_commit_history` (`:5495-5518`), `fetch_commit_history_shas` (`:5520-5540`), `git_remote` (`:5542-5551` — **no other caller; confirm before deleting**), `render_commit_history` (`:5553-5767`); the `ActivateHistoryTab` action + handler + registration; the History tab in `render_tab_bar` (`:5342-5348`); the `GitPanelTab::History` match arm (`:6626`); the one test `test_history_drops_previous_repository_commits` (`:9097-9160`); the three keymap lines.
25. **EDIT, do not delete** (shared with Changes): `focus_in` (`:1237-1240`), `open_diff`'s History early-return (`:1409-1412`), `open_accordion_diff`'s (`:1433-1436`), `set_active_repository` (`:3918-3941` — the documented single seam that clears History rows and subscriptions), `schedule_update` (`:3944-3949` — `preload_commit_history` is called **unconditionally**, so the warm-up goes too), `set_active_tab` (`:5437-5456`), and the `show_changes` / `changes_count` badge flag in the `tab` closure (`:5311-5316`).
26. **`_repo_subscriptions: Vec<Subscription>` (`:846`) is declared generically but used only by History** (`:5497-5514`, cleared at `:3936`, `:5453`). **Keep the field** — the Commit tab wants exactly this.

### Data APIs

27. `GitPanel.active_repository: Option<Entity<Repository>>` (`:786`, accessor `pub fn active_repository` at `:6501`), swapped by `set_active_repository` (`:3922`) which prefers `solutions::active_member_repository` (`:3905-3910`).
28. `Repository::show(commit) -> oneshot::Receiver<Result<CommitDetails>>` (`crates/project/src/git_store.rs:5690`), async. `CommitDetails { sha, message, commit_timestamp, author_email, author_name }` (`crates/git/src/repository.rs:519-525`, `short_sha()` at `:566`); `message` is the **full** message. There is no separate author date and no committer identity. A ready-made wrapper already exists: `pub fn GitPanel::load_commit_details(sha, cx) -> Task<Result<CommitDetails>>` (`git_panel.rs:6116-6128`).
29. `Repository::load_commit_diff(commit) -> oneshot::Receiver<Result<CommitDiff>>` (`git_store.rs:5718`), async. `CommitDiff { files: Vec<CommitFile> }`; `CommitFile { path, old_text, new_text, is_binary }` (`repository.rs:545-551`) with `fn status() -> CommitFileStatus` (`:553-560`). **No line stats on the wire** — +/− must be computed client-side, which is what `compute_diff_stats` does.
30. **`CommitView::open_file_diff(commit_sha, repo, workspace, file: RepoPath, window, cx)`** (`crates/git_ui/src/commit_view.rs:270`) is exactly spec §5's "clicking a file opens that file's diff for the commit in the centre pane" — already built, `pub`, fire-and-forget, and its doc-comment literally names the git-graph changed-files list as its caller. Dedup on `(sha, single_file)` at `:429-455`. **Reuse verbatim.**
31. **Do NOT embed `CommitView` itself.** It is a workspace pane `Item` (`commit_view.rs:1371`) that owns a `SplittableEditor`; hosting it inside a dock panel would put a second diff editor in a narrow column.
32. `SoloDiffView` is **not** usable for commits — it resolves through `project.open_uncommitted_diff` (`solo_diff_view.rs:156`) and has no commit-sha input path. Same for `multi_diff_view.rs` / `file_diff_view.rs` (both read from disk).
33. Reusable UI pieces already in `git_ui`: `CommitAvatar::new(&sha, author_email, remote)` / `::from_commit_details` (`commit_tooltip.rs:37`, `:50`), `GitStatusIcon::new(FileStatus)` (`git_ui.rs:1130-1135`) and `git_status_icon(status)` (`git_ui.rs:429`), `ui::DiffStat::new(id, added, removed)` (`crates/ui/src/components/diff_stat.rs:15`).

### Landmines

34. **`git::status::DiffStat` (data, `crates/git/src/status.rs:584`) and `ui::DiffStat` (component) are both in scope in `git_panel.rs`.** The UI one is always spelled `ui::DiffStat`. Do not let this collide in the new module.
35. **`SHOW_PRE_COMMIT_SECTION` is a hard-coded `false` gate** parked inside the exact `match self.active_tab` arm this phase edits (`git_panel.rs:6612-6624`, comment at `:6612-6620`: *"hidden per user request (2026-07-02) … kept behind a `false` gate — not deleted"*). Its accessors `pre_commit_config` (`:6310-6313`) and `pre_commit_no_verify` (`:6352-6355`) carry `#[allow(dead_code)]`. **Preserve the gate verbatim; do not "clean it up".**
36. `./script/clippy` runs `--all-features --all-targets`, so `-p` scoping does **not** exclude the seven pre-existing `git_ui` findings (the maintainer's own WIP). The honest gate is *"zero findings naming code this plan wrote or moved"*, not exit 0. Candidates seen statically: the two `#[allow(dead_code)]` pre-commit accessors above, `changes_list.rs:869` TODO, `git_panel.rs:350` TODO, `git_panel.rs:690` commented-out field, and `#[allow(dead_code)]` in `stashes.rs:66` / `pre_commit.rs:702` / `shelf.rs:42` / `project_diff.rs:487`. **Do not touch any of them.**
37. Two **pre-existing** defects found during recon, both out of scope, both recorded so nobody re-finds them: (a) `git_graph.rs:5024-5027` indexes `graph_data.commits` with the **view** index without `view_to_data_idx`, an off-by-one whenever the local-changes row is present; (b) the `GitGraph`-context keymaps bind `git_graph::{FocusNextTabStop,FocusPreviousTabStop,ScrollDown,ScrollUp}` (`default-macos.json:1681-1692`, `default-windows.json:1588-1599`, `vim.json:1044-1064`) — **none of those actions exists anywhere in `crates/`**.
38. Graph geometry in a compact band: columns are fractions `0.74 / 0.13 / 0.13` (`git_graph.rs:2202-2219`) with **no horizontal scroll and no table min-width**; the graph overlay reserves at least `LEFT_PADDING(8) + LANE_WIDTH(16) * MIN_GRAPH_LANES(4) = 72px` of the Description column because the floor beats the 0.4 cap (`canvas_geometry.rs:60-84`). The sidebar's `min_w(px(300.))` is the only min-width in the view and it is what goes away. Retuning the fractions is **out of scope** for this plan; so is `log_toolbar.rs`.
39. There are **no `GitGraphPanel` tests at all**; the band-slot tests use a `BandProbe` stub (`crates/workspace/src/workspace.rs:11447-11540`). All `git_ui` and `git_graph` tests are inline `#[cfg(test)] mod tests` — there is no `crates/git_ui/tests/` directory.

### Corrections established while executing Task 1 (authoritative; supersede facts 11 and 34 where they differ)

40. **`commit_identity_source` does not stand alone.** It depends on three private helpers fact 11 omitted — `escape_markdown_inline`, `format_detail_timestamp`, `detail_timestamp_format`. All three are used only by that chain and moved with it (private in `commit_tab.rs`). Verified byte-identical by the task review.
41. **`git_ui` cannot read `ProjectPanelSettings`** — `crates/project_panel/Cargo.toml:26` has `git_ui.workspace = true`, so the reverse dependency is a cycle. `ChangedFileEntry::render` therefore takes `indent: Pixels` as a parameter; `git_graph` computes the byte-identical `px(18.0 + ProjectPanelSettings::get_global(cx).indent_size)` at the top of the same `uniform_list` render closure. The hoist out of the per-row loop is **not** observable (same `cx`, same paint, nothing takes `&mut App` in between) — confirmed by review.
42. **The row renderers are hosted through a closure bundle, not generics:**
    ```rust
    pub struct ChangedFileRowHandlers {
        pub select_file: Rc<dyn Fn(&RepoPath, &mut Window, &mut App)>,
        pub deploy_file_context_menu: Rc<dyn Fn(&RepoPath, Point<Pixels>, &mut Window, &mut App)>,
        pub toggle_directory: Rc<dyn Fn(&SharedString, &mut Window, &mut App)>,
    }
    ```
    Each closure captures the host's `WeakEntity` and ends in `.ok()` — which is not new, it moved verbatim from the graph's own closures. No reference cycle; the `Rc`s drop with the frame.
43. **`commit_tab.rs` opens with `use super::*`** (the `changes_list.rs:28` precedent), which inherits `git_panel.rs:29`'s `use git::status::{DiffStat, StageStatus}`. So inside that module **bare `DiffStat` already resolves to the DATA type**. The +/− component must be written `ui::DiffStat::new(...)`, fully qualified, exactly as `git_panel.rs:4710` does.
44. **`ChangedFileEntry::render`'s `commit_sha` must be the FULL sha**, not `short_sha()` — it forwards straight to `CommitView::open_file_diff`. The graph passes `full_sha` (`git_graph.rs:3394`, `:3567`). A short sha compiles fine and opens an empty or wrong centre-pane tab.
45. **Indent numbers, measured.** The graph's commit file row is `px(18.0 + indent_size)` = **38px** at the shipped default (`assets/settings/default.json:793`, `indent_size: 20`). The Changes tab's rows use `content_row_padding(depth)` = `px(ROW_LEFT_PADDING(6) + SECTION_CONTENT_INDENT(16) + depth * TREE_INDENT(16))` (`changes_list.rs:46`, constants `git_panel.rs:352-362`) = 22px at depth 0, 38px at depth 1. A bare `px(TREE_INDENT)` would be **16px — a 22px regression** against what ships today. Also note `render_changed_directory_row` gives its directory header **no** left padding at all (`commit_tab.rs:241`, plus the list's `.ml_neg_1()`), while a Changes-tab section header sits at 6px, so no file-row constant alone aligns the two trees.
46. **The `Rc<dyn Fn>` bundle erases the host entity, so a `GitPanel` double-lease is not a compile error.** In the graph this never bites because the tree is not rendered from inside the host's own `update`; the Commit tab **is** rendered under a live `Context<GitPanel>` lease. **Binding rule:** these closures may only be installed into event callbacks. Never invoke one during `request_layout` / `prepaint` / `paint`, and build the bundle from `cx.weak_entity()` at the top of render, never inside a nested `update`. A wrongly-shaped unit test drawing through `VisualTestContext` will not catch this; the editor panics the first time a user opens the tab.
47. **Deleting the sidebar in Task 4 removes the only `ProjectPanelSettings::get_global` caller in `git_graph`** (`git_graph.rs:3590`), so that import goes with it.

---

### Corrections established while executing Task 2 (authoritative)

48. **The Commit tab is one field, not a spray of them.** `CommitSelection`,
    `CommitTabState`, `LoadState` and the render live in `commit_tab.rs`;
    `git_panel.rs` holds `commit_tab: Option<CommitTabState>` and re-exports
    `pub use commit_tab::CommitSelection;`, so the path the Task-2 interface
    block names (`git_ui::git_panel::CommitSelection`) resolves. `Some` *is*
    the tab's presence in the tab bar — `commit_tab_is_open()` is `is_some()`,
    so "open" and "has something to show" cannot drift apart.
49. **Neither `show_commit_selection` nor `close_commit_tab` touches focus.**
    Task 3 pushes from the graph mid-click / mid-arrow-key, and focusing the
    panel there would kill the graph's own keyboard navigation. Only the
    user-driven routes (the tab-bar row, `git_panel::ActivateCommitTab`) go
    through `set_active_tab`, which does focus. If Task 3 wants the panel
    focused on a graph selection it has to ask for that explicitly.
50. **CORRECTED by the Task 2 review — the ✕ does NOT let the row's click
    through.** `ButtonLike`'s click wrapper calls `cx.stop_propagation()` before
    the handler (`crates/ui/src/components/button/button_like.rs:767-773`), and
    GPUI bubbles listeners with `for … in mouse_listeners.iter_mut().rev() { … if
    !cx.propagate_event { break } }` (`crates/gpui/src/window.rs:4669-4677`) while
    `Interactivity::paint` registers the parent's listeners *before* the child's
    (`crates/gpui/src/elements/div.rs:2236` vs `:2262`) — so the nested ✕ runs
    first and the tab row's `on_click` never runs at all. A `stop_propagation`
    dance IS in play; it is just `ButtonLike`'s, not ours. `set_active_tab`'s
    refusal of `GitPanelTab::Commit` while the tab is closed is therefore
    belt-and-braces, not the mechanism. **Do not build on the original claim** —
    replacing the ✕ with an element that does not stop propagation would strand
    the panel on an empty tab.
51. **`close_commit_tab` emits `Event::CommitTabClosed` only when a tab was
    actually open** (early return on `take().is_none()`). A redundant close is
    already silent, so Task 3's feedback-loop guard has one less case.
52. **The loads go through `selection.repository`** (`Repository::show` /
    `Repository::load_commit_diff`), *not* `GitPanel::load_commit_details`,
    which resolves against `active_repository` and would contradict the "the
    push carries the repository" ruling. The avatar's provider likewise comes
    from `commit_tab::commit_remote(&selection.repository, cx)`, not
    `GitPanel::git_remote` — so **Task 5's `git_remote` grep is unchanged: it
    still has no caller outside History.**
53. **Indent, as shipped.** `COMMIT_TREE_INDENT = 18.0 + TREE_INDENT` (34px)
    and **no container inset**: `ButtonLike`'s own 4px horizontal padding is the
    directory header's indent, which lands the file rows at 38px — the same
    left edge as the Changes tab's depth-1 rows *and* as the graph sidebar's
    file rows. The graph's `.ml_neg_1()` is deliberately not carried over. The
    **Density caveat (Task 2 review):** `ButtonLike`'s 4px is
    `DynamicSpacing::Base04`, whose tuple is `(2, 4, 6)`
    (`crates/ui/src/styles/spacing.rs`) — 4px only at `UiDensity::Default`, and it
    is `rems()`, so it scales with `ui_font_size`. `COMMIT_TREE_INDENT` and the
    Changes tab's `content_row_padding` are absolute px, so the two trees align
    exactly on stock settings and drift a couple of pixels otherwise. The
    headers then sit 2px inside the Changes tab's 6px section headers; closing
    that last 2px would mean either editing the shared `render_changed_directory_row`
    (which would move the graph's still-live sidebar too) or a magic negative
    margin, and the file rows are the edge the eye tracks.

### Corrections established by the Task 3 review fixes (authoritative)

54. **`show_commit_selection` grew a source argument — this is the contract
    Tasks 4/5/6 build on.**
    ```rust
    pub enum CommitSelectionSource { UserGesture, Background }
    pub fn show_commit_selection(&mut self, selection: CommitSelection,
        source: CommitSelectionSource, window: &mut Window, cx: &mut Context<Self>);
    ```
    `GitGraph::select_entry` and `GitGraph::select_commit_by_sha` carry the same
    argument through from their call sites. `UserGesture` re-activates the
    Commit tab (what makes "select a commit, switch to Changes, click that row
    again" work); `Background` refreshes an already-open tab in place, never
    changes `active_tab`, and does nothing at all when the tab is closed. The
    `Background` callers are exactly the two re-anchors in `on_repository_event`
    and the deserialize path's `select_commit_by_sha`; everything else is a
    gesture. Without the split a `git fetch` landing in a terminal re-anchored
    the selection and swapped the panel body out from under a user who had gone
    back to Changes to stage files and type a commit message.
55. **`Event::CommitTabClosed` carries `Vec<Oid>`** — the shas the closing tab
    was describing. The event reaches every `GitGraph` in the window, so the
    handler clears only when the payload equals its own `selected_commit_shas()`.
    This **subsumes** the old `is_empty()` bounce guard: with the synthetic
    local-changes row selected the graph's shas are `[]` while the event carries
    the outgoing commit, so the mismatch already stops the bounce (verified by
    mutation — dropping the payload check fails
    `test_local_changes_row_is_never_pushed_as_a_commit`).
56. **A failed re-anchor closes the tab.** `invalidate_state` clears the
    selection silently and parks the sha in `pending_select_sha`; when the
    refetched log no longer contains it (`git commit --amend` in a terminal) no
    push ever happens, so `on_repository_event` calls
    `GitGraph::close_vanished_commit_tab`, which closes the panel's tab only
    while it describes exactly that one sha. `invalidate_state`'s doc comment
    now carries that story; `clear_selection`'s is unchanged (it is not on this
    path).

---

### Corrections established while executing Task 4

57. **Fact 12 is WRONG about `get_remote`.** It claimed `deploy_commit_context_menu`
    also used it. It did not: `get_remote`'s only two callers were both inside the
    deleted sidebar (`deploy_commit_detail_context_menu`'s permalink and the
    sidebar avatar), so it was deleted with them, along with the `GitRemote` /
    `BuildCommitPermalinkParams` / `ParsedGitRemote` imports it pulled in. The
    review confirmed `deploy_commit_context_menu` and
    `deploy_multi_commit_context_menu` are **byte-identical** across the commit
    and resolve their provider inline (`default_remote_url` →
    `GitHostingProviderRegistry::default_global` → `parse_git_remote_url`), so no
    permalink entry broke. `git_ui`'s `commit_tab::commit_remote` is the live
    implementation (fact 52).
58. **Fact 10 misses `changed_file_row_handlers`** — added by Task 1, its only
    caller was `render_commit_detail_panel`, so it died with the sidebar.
59. **`render_chip`'s `truncate` parameter now has one caller that always passes
    `true`.** The `false` caller was the sidebar's `flex_wrap()` ref-chip row. The
    parameter was left in place (no drive-by refactors) but its doc comment still
    explains the flag through a wrapping-row scenario that no longer exists in
    the file — Task 6 fixes the comment.
60. **`select_entry` lost an incidental early return.** The old body bailed on an
    unresolvable repository *before* `cx.emit(ItemEvent::Edit); cx.notify();`; the
    rewrite bails only on `target_sha.is_none()`, so that case now emits and
    notifies where it previously did neither. Judged a latent-bug fix — the
    selection genuinely did change and should re-serialize — but it is untested
    and was not in the commit message.
61. **Deliberately accepted losses from the sidebar's context menu.** It offered
    Copy SHA / Copy Message / Copy Author Email / Copy Web URL / `markdown::Copy`.
    Copy SHA and Copy Web URL survive in the graph's row menu; message
    selection-copy survives as `ctrl-c` → `markdown::Copy` in the Commit tab.
    **"Copy Author Email" has no equivalent anywhere and "Copy Message" degrades
    to "Copy Subject"** — accepted, because a message-block context menu on the
    Commit tab is new scope beyond spec §5. Recorded in the deferred backlog, not
    a Task 6 decision round.

---

## Rulings made up front (binding; a reviewer judges against these)

- **Graph → panel is a direct typed call; panel → graph is a GPUI event.** The graph already looks the panel up (fact 2) and may name `git_ui` types; `git_ui` may not name `git_graph` types (fact 1). So `GitGraph` calls `GitPanel::show_commit_selection(...)` / `close_commit_tab(...)`, and `GitPanel` extends its existing `pub enum Event` with `CommitTabClosed`, which `GitGraph` subscribes to in `GitGraph::new` (re-installed automatically because `GitGraphPanel` rebuilds the graph on repo switch, fact 4). *Rules out:* the string-named-action IoC trick and any new registry — neither is needed here, and both are harder to test. *Cost if wrong:* the graph gains a compile-time reference to the panel's API, which it already has.

- **The Commit tab's content is a relocation of the graph's sidebar, not a new design.** Its file tree, message Markdown rendering and diff-stat fold move into `git_ui` essentially verbatim (fact 11). *Rules out:* genericising `commit_view/affected_files.rs` — its rows are **not clickable** and it is hard-typed to `Context<CommitView>`, so adapting it means changing a working surface to gain nothing. *Cost if wrong:* `git_ui` ends up hosting three changed-files trees (working changes, `affected_files`, the moved commit tree), which is exactly the extraction trigger FORK.md #55 named. That extraction is **explicitly deferred** — record it, do not do it here.

- **The Commit tab lives on `GitPanel`, in a new `crates/git_ui/src/git_panel/commit_tab.rs` `impl GitPanel` block.** That is the grain `changes_list.rs` already set (fact: the Changes tab has no state struct of its own — it is a slice of `GitPanel`'s fields). *Rules out:* a separate `Entity` view for the tab. *Cost if wrong:* more fields on an already-large struct.

- **The push carries the `Entity<Repository>` the graph is showing, not just shas.** The panel resolves its own `active_repository` through `solutions::active_member_repository` and the two can disagree transiently. *Rules out:* the Commit tab ever describing a commit from a different repository than the graph row the user clicked. *Cost if wrong:* one extra field of state to keep in sync.

- **`GitPanel::set_active_repository` closes the Commit tab**, using the same seam that used to clear History's rows and subscriptions (fact 25). *Rules out:* a stale commit from the previous repo surviving a member switch — the exact bug class the History code's long doc-comment at `git_panel.rs:3915-3921` exists to document.

- **The Commit tab is ephemeral: never persisted, never restored.** The panel keeps booting on Changes. *Rules out:* a `SerializedGitPanel` migration. *Cost if wrong:* a restart loses the open commit — which the graph's own selection *is* persisted (`selected_sha`) and could re-drive later if the maintainer asks.

- **Content is exactly spec §5 and no more:** full commit message, `short hash · author · date`, the changed-files tree with status colouring, and **whole-commit** +/− totals in the header — mirroring what the graph's sidebar showed. **Per-file +/− counts are out of scope** (`CommitFile` carries no numstat; FORK.md #55 already ruled this out once). **Ref chips and the "In N branches" line are dropped**, together with `CommitBranches` / `branches_containing` / `format_branches_containing` and its test; both remain available in the full `CommitView` (`commit_view/refs_bar.rs`, `commit_view/contains_panel.rs`). *Cost if wrong:* two pieces of information move one click away, to the same place the full sha already lives.

- **No resize split inside the Commit tab.** The sidebar's files↔message drag handle and both `SplitState` types die with it. Layout is fixed: message block on top (bounded, scrollable), metadata row, then the file tree filling the rest — the order spec §5 lists. *Rules out:* persisting a split the panel's own width already constrains.

- **`ctrl-1` keeps activating Changes; `ctrl-2` is rebound to a new `git_panel::ActivateCommitTab`, which is a no-op when no commit is selected.** *Rules out:* leaving `ctrl-2` bound to a deleted action (which is exactly defect 37b, and we are not adding a second instance of it).

- **Single click on a file row selects it; double click opens `CommitView::open_file_diff` in the centre pane** — the graph's existing gesture split (`git_graph.rs:259-276`), preserved verbatim. *Rules out:* mouse-walking the list spraying centre-pane tabs. *Cost if wrong:* it differs from the Changes tab, where a single click opens a preview tab (FORK.md #54); matching that would need a commit-aware preview path `CommitView` does not expose.

- **Task order is load-bearing: the Commit tab is built and wired BEFORE either deletion.** Every intermediate commit must leave `main` with a working way to read a commit's details — first the old sidebar, then the new tab. *Rules out:* deleting the sidebar in task 1 and shipping a window with no commit details at all.

## Global Constraints

- **GPUI double-lease:** reading an entity that is already under a `&mut` borrow panics at runtime, compiles clean, and wrongly-shaped unit tests miss it. The graph→panel push happens from inside `GitGraph`'s own update; reach the panel through `self.workspace.upgrade()` and **defer** (`cx.defer_in`, precedent `git_graph_panel.rs:141-143`) — `select_entry` is reachable from `invalidate_state` and the deserialize path (fact 18).
- **A `cx.notify()` raised during a draw is discarded, not deferred** (`Window::invalidate_view` returns false when `draw_phase != None`). Never derive-and-notify from `render`.
- **The band's three-part layout invariant must survive** (FORK.md): `min_h_0()` on the workspace column, `flex_none()` on the status bar, a shrinkable band. No task here should touch `Workspace::render` or the status bar — if one does, it must leave all three intact.
- **Debug builds only** for agent verification: `cargo build`, `cargo test`, never `--release`.
- **Never pipe cargo output through `tail` without `set -o pipefail`** — the pipe reports `tail`'s status and a failed build looks green.
- **Harness `<new-diagnostics>` blocks in this repo are frequently stale mid-edit snapshots.** Confirm with `set -o pipefail; cargo check -p git_ui -p git_graph --all-targets`.
- **`mcp__sawe__*` tools drive the maintainer's LIVE running editor.** Never verify with them. `cargo build --bin sawe` first (`script/run-mcp` only compiles a *missing* binary), then `script/run-mcp --debug --headless`, and drive **that** socket.
- **`workspace.screenshot` renders the retained scene and does not run a draw.** Drive a real event, then re-capture. Any user-visible change needs a screenshot before it counts as done.
- **The disk has ~284 GB free with `target/` at 403 GB.** An unexplained compile failure may be ENOSPC. Do not delete anything beyond rebuildable caches.
- Commit messages: imperative, crate-prefixed, **no `Co-Authored-By`**, never `git commit --amend`. **Implementers do not push.**
- Rust style: no `unwrap()` outside tests; comments explain *why*; no organizational comments; never `let _ =` on a fallible call; no `mod.rs`.
- **You may come back with a documented negative result.** If a fact above is wrong, or the task as briefed is the wrong shape, say so with citations and stop — that is a success, not a failure. Twelve agents did exactly this across phases 2a/2b and were right every time.

---

### Task 1: Move the commit-detail building blocks down into `git_ui`

**Files:** `crates/git_ui/src/git_panel/commit_tab.rs` (new), `crates/git_ui/src/git_panel.rs`, `crates/git_graph/src/git_graph.rs`.

Pure relocation. No behaviour changes anywhere: the graph keeps rendering its sidebar, but the pieces now live one crate down where the Commit tab will use them.

**Interfaces produced:**
```rust
// crates/git_ui/src/git_panel/commit_tab.rs
pub fn split_commit_message(message: &str) -> (String, Option<String>);   // preserve the existing signature
pub fn commit_identity_source(/* preserve */) -> /* preserve */;
pub fn compute_diff_stats(diff: &CommitDiff) -> (usize, usize);
pub fn detail_text_style(/* preserve */) -> MarkdownStyle;
pub struct ChangedFileEntry { /* preserve fields */ }
pub struct ChangedFileRow  { /* preserve */ }
pub fn build_changed_file_rows(/* preserve */) -> Vec<ChangedFileRow>;
```

- [x] Create `crates/git_ui/src/git_panel/commit_tab.rs` and declare it from `git_panel.rs` beside `changes_list.rs`. Move `split_commit_message`, `commit_identity_source`, `compute_diff_stats`, `detail_text_style`, `ChangedFileEntry`, `ChangedFileRow`, `build_changed_file_rows` and `render_changed_directory_row` from `git_graph.rs` (fact 11), keeping signatures and bodies intact wherever the borrow types allow.
- [x] `ChangedFileEntry::render` and `render_changed_directory_row` are typed against `Context<GitGraph>` today. Make them generic over the hosting view (`cx: &mut Context<V>` plus explicit callbacks) or take plain `&mut App` + closures — whichever keeps the graph's call sites compiling **unchanged in behaviour**. Do not change what they paint.
- [x] `git_graph.rs` re-imports them from `git_ui::git_panel::commit_tab::*` and deletes its local copies. The sidebar must look and behave **identically** after this task.
- [x] Move the three tests too: `test_split_commit_message`, `test_commit_identity_source`, `test_build_changed_file_rows_groups_by_directory`, plus helpers `changed_file_entry` / `describe_changed_file_row` (fact 11). They now live in `commit_tab.rs`'s test module.
- [x] Watch the `DiffStat` name collision (fact 34): in the new module spell the component `ui::DiffStat` and the data type `git::status::DiffStat`, always qualified.
- [x] Gate: `set -o pipefail; cargo check -p git_ui -p git_graph --all-targets`; `cargo test -p git_ui -p git_graph`.

### Task 2: Add the Commit tab, opened programmatically

**Files:** `crates/git_ui/src/git_panel.rs`, `crates/git_ui/src/git_panel/commit_tab.rs`.

History stays untouched and reachable; the panel simply grows a third tab that nothing opens yet except a test. `main` keeps working throughout.

**Interfaces produced:**
```rust
// crates/git_ui/src/git_panel.rs
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum GitPanelTab { Changes, History, Commit }   // History leaves in Task 5

#[derive(Clone)]
pub struct CommitSelection {
    pub repository: Entity<Repository>,
    pub shas: Vec<Oid>,          // non-empty; len 1 = detail view, len > 1 = "N commits selected"
}

impl GitPanel {
    pub fn show_commit_selection(&mut self, selection: CommitSelection, window: &mut Window, cx: &mut Context<Self>);
    pub fn close_commit_tab(&mut self, window: &mut Window, cx: &mut Context<Self>);
    pub fn commit_tab_is_open(&self) -> bool;
}

pub enum Event { Focus, CommitTabClosed }   // extends git_panel.rs:417-420
```

- [x] Add the `Commit` variant plus the Commit tab's state (selection, loaded `CommitDetails`, loaded `CommitDiff` + its computed totals, the file tree's `collapsed_dirs` / scroll handle / selected file, and the load tasks). Reuse the existing `_repo_subscriptions` field (fact 26) rather than adding another.
- [x] `show_commit_selection` stores the selection, activates the tab, and — for a single sha — kicks off `load_commit_details` (fact 28) and `repository.load_commit_diff` (fact 29). **Guard both against stale responses by comparing the sha on completion**, exactly as `git_graph.rs:3011-3016` does. For a multi-sha selection it loads nothing.
- [x] `close_commit_tab` clears the state, returns `active_tab` to `Changes`, and **emits `Event::CommitTabClosed`**.
- [x] `render_tab_bar` (`:5284-5350`) renders the Commit tab **only while it is open**, with a hand-built ✕ (fact 20) whose click calls `close_commit_tab`. Changes keeps its `changes_count` badge; Commit passes `show_changes = false`.
- [x] Commit tab body, in spec §5's order: the full message (subject + body, Markdown-rendered via the relocated `detail_text_style`), a `short hash · author · date` row (`CommitAvatar`, `short_sha()`, `time_format`), a header carrying the file count and `ui::DiffStat::new(id, added, removed)` from `compute_diff_stats`, then the changed-files tree. Single click selects a file; **double click** calls `CommitView::open_file_diff(sha, repo, workspace, path, window, cx)` (fact 30). Multi-sha selection renders only a centred "N commits selected" label.
- [x] Indent: introduce a documented `COMMIT_TREE_INDENT` in `commit_tab.rs` valued `18.0 + TREE_INDENT` (34px) rather than a bare `TREE_INDENT`, per fact 45 — 16px would be a visible regression once Task 4 deletes the sidebar. Decide the directory-header padding question explicitly and say what you chose.
- [x] Explicit loading and error states — a commit whose diff fails to load must say so, not render an empty file list.
- [x] Add `git_panel::ActivateCommitTab` beside `ActivateChangesTab` (`:120-123`), with a handler that is a **no-op when the tab is not open**, and register it at `:6577-6578`.
- [x] Preserve `SHOW_PRE_COMMIT_SECTION`'s `false` gate verbatim while restructuring the `match self.active_tab` arms (fact 35).
- [x] Tests: `show_commit_selection` with one sha activates the Commit tab and `commit_tab_is_open()`; with three shas renders the count summary; `close_commit_tab` returns to Changes, emits `CommitTabClosed`, and leaves `commit_tab_is_open()` false; `ActivateCommitTab` while closed does nothing.
- [x] Gate: `set -o pipefail; cargo check -p git_ui --all-targets`; `cargo test -p git_ui`.

### Task 3: Wire the graph's selection to the panel, both ways

**Files:** `crates/git_graph/src/git_graph.rs`, `crates/git_ui/src/git_panel.rs`.

After this task both surfaces are live at once (the sidebar has not been deleted yet) — that is deliberate and lets a reviewer compare them side by side.

**Interfaces produced:**
```rust
// crates/git_graph/src/git_graph.rs
impl GitGraph {
    pub fn selected_commit_shas(&self) -> Vec<Oid>;   // data-space, view→data converted
}
```

- [x] Add a private `push_selection_to_git_panel(&self, window, cx)` that resolves `self.workspace.upgrade()` → `workspace.panel::<GitPanel>(cx)` (fact 2) and calls `show_commit_selection` (non-empty selection) or `close_commit_tab` (empty). **Go through `cx.defer_in`** (fact 18) — `select_entry` is reachable from `invalidate_state` and the deserialize path, and a synchronous `workspace.update` there is a re-entrancy panic waiting to happen.
- [x] Call it from `select_entry` (after the selection sets settle) and from every `clear_selection` caller — note `clear_selection` itself takes no `cx` (fact 16), so either give it one or push from the call sites; pick one and be consistent.
- [x] Convert view indices to data indices with `view_to_data_idx` (fact 14) — the synthetic "Local Changes" row must never be pushed as a commit. `fold_row_click` already refuses to multi-select it (`test_fold_row_click_never_multi_selects_the_local_changes_row`, `:8710`); make sure the single-select path is equally safe.
- [x] Subscribe to the panel in `GitGraph::new` (fact 4 — `GitGraphPanel` rebuilds the graph on repo switch, so this re-installs itself): on `git_ui::git_panel::Event::CommitTabClosed`, call `clear_selection` + notify. **Guard against a feedback loop**: closing the tab clears the graph, which must not push another close.
- [x] `GitPanel::set_active_repository` closes the Commit tab (ruling above), reusing the seam at `:3922`.
- [x] **Verify and report:** does `apply_row_click_selection` still reach `select_entry` when the user re-clicks the row that is already selected? If it early-returns, say so — the ✕-then-reclick path depends on it and the design may need adjusting.
- [x] Tests: selecting a graph row opens the Commit tab with that sha; Ctrl-clicking a second row shows "2 commits selected"; `menu::Cancel` (Escape) closes the tab; closing the tab via ✕ clears `selected_entry_idx` and does not re-enter; switching the active repository closes the tab.
- [x] Gate: `set -o pipefail; cargo check -p git_ui -p git_graph --all-targets`; `cargo test -p git_ui -p git_graph`.

### Task 4: Delete the graph's inline commit-details sidebar

**Files:** `crates/git_graph/src/git_graph.rs`.

**Interfaces produced:** none — this is a deletion.

- [x] Delete the state, types, constants and methods enumerated in facts 8, 9 and 10, and the `render` block at `:4686-4698` including its drag listeners.
- [x] Delete the two async loads inside `select_entry` (`:2986-3067`) and the cleanup lines in `clear_selection` / `select_entry`. **Keep** everything in fact 12 — `get_remote`, `render_chip`, `is_head_ref`, every `fetch_commit_data` call, and `CommitView::open` behind `open_commit_view`.
- [x] Delete `CommitBranches` / `MAX_LISTED_BRANCHES` / `format_branches_containing` / `BRANCHES_CONTAINING_DEBOUNCE` and the test `test_format_branches_containing` (`:8479`) — the "In N branches" line is dropped per the rulings.
- [x] Sweep the imports that fact 13 lists as newly unused. Let `cargo check` name them rather than guessing; **do not** blanket-`#[allow(unused_imports)]`.
- [x] Delete `test_detail_split_state_is_sized_in_pixels` (`:8310`) and `test_commit_detail_text_entities_are_cached_per_commit` (`:8361`). **Rewrite** `test_commit_details_survive_external_commit` (`:8210`) against the Commit tab — its invariant (a commit's details stay paired with its sha across a refetch) has moved, not disappeared. Keep every selection test (fact: `:8555`, `:8616`, `:8634`, `:8661`, `:8710`, `:8736`, `:8765`).
- [x] Leave the two pre-existing defects in fact 37 alone; they are recorded in this plan and go to the backlog.
- [x] Gate: `set -o pipefail; cargo check -p git_ui -p git_graph --all-targets`; `cargo test -p git_graph`.

### Task 5: Delete the History tab

**Files:** `crates/git_ui/src/git_panel.rs`, `assets/keymaps/default-{linux,windows,macos}.json`.

**Interfaces produced:** `enum GitPanelTab { Changes, Commit }`.

- [ ] Delete everything in fact 24. Before removing `git_remote` (`:5542-5551`), grep the crate and **confirm** it has no other caller; if it does, keep it and say so.
- [ ] Edit — do not delete — every site in fact 25. In particular `schedule_update` must stop calling `preload_commit_history` **unconditionally**, or the panel keeps issuing `graph_data` calls for a tab that no longer exists.
- [ ] Rebind `ctrl-2` / `cmd-2` from `git_panel::ActivateHistoryTab` to `git_panel::ActivateCommitTab` in all three keymaps (fact 23). Do not leave a binding pointing at a deleted action.
- [ ] **Close the command-palette hole the Task 2 fix wave left open.** Narrowing `dispatch_context` makes the Commit tab inert to the *keymap*, but the panel's `.on_action` registrations are still live, so palette-dispatching `git::ToggleStaged` / `git::RestoreFile` / `menu::Confirm` while the Commit tab shows still acts on the hidden Changes selection. That hole was identical on History and was left alone while History existed; once History is gone, Commit is the only other tab and it becomes the whole exposure. Guard the staging / restore / open-diff handlers on `active_tab == GitPanelTab::Changes`, with a test.
- [ ] Delete `test_history_drops_previous_repository_commits` (`:9097-9160`). Its repo-switch invariant is now covered by the Commit-tab test added in Task 3.
- [ ] Confirm the surviving tests still pass, especially `test_open_diff` (`:8238`) and `test_dispatch_context_with_focus_states` (`:8690`).
- [ ] Gate: `set -o pipefail; cargo check -p git_ui -p git_graph --all-targets`; `cargo test -p git_ui`; `cargo test -p zed test_action_namespaces` (deleting an action can move that pin).

### Task 6: Live verification, gates and docs

**Files:** `FORK.md`, `docs/INDEX.md`, this plan.

- [ ] `cargo build --bin sawe`, then `script/run-mcp --debug --headless`, and drive **that** socket (never `mcp__sawe__*`). Open the standing fixture Solution 33 (`WideGraph`, a real multi-branch repo in `~/.spk/sawe-dev/`).
- [ ] Screenshot each state, driving a real event before every capture (the retained-scene trap): git panel showing **Changes | (no Commit tab)**; a commit selected in the band's graph → **Changes | Commit ✕** with message, `hash · author · date`, +/− totals and the file tree; a file double-clicked → its commit diff in the centre pane; a Ctrl-click multi-selection → "N commits selected"; ✕ pressed → back to Changes **and the graph row deselected**; and the graph itself with **no sidebar**, filling the band's utility half.
- [ ] Confirm the band's three-part layout invariant still holds — run a `windows.resize` to something short (e.g. 1280×384, debug-only tool) and check the status bar still has visible pixels.
- [ ] Full gates: `cargo test -p git_ui -p git_graph -p zed -p workspace`; `cargo fmt --all --check`; `./script/clippy -p git_ui -p git_graph` with the honest reading from fact 36 — zero findings naming code this plan wrote or moved.
- [ ] `FORK.md`: a numbered decision entry for phase 3 (what moved, why the graph→panel/panel→graph split, what was dropped and where it still lives), touched-files rows for `crates/git_ui/src/git_panel/commit_tab.rs` and any first-time-modified upstream file, and an amendment to #55 noting that its three-trees extraction trigger has now fired and is deferred.
- [ ] `docs/INDEX.md`: mark this plan complete in the plans table with its commit chain.
- [ ] **Visibility sweep — bigger than originally scoped.** Task 1 made `commit_tab` a `pub mod` with ~10 `pub` items (`ChangedFileRowHandlers`, `ChangedFileEntry` + `from_commit_file`/`render`, `ChangedFileRow`, `build_changed_file_rows`, `render_changed_directory_row`, `split_commit_message`, `commit_identity_source`, `detail_text_style`, `compute_diff_stats`) *solely* so `git_graph` could import them. After Task 4 nothing outside `crates/git_ui/src/git_panel/` names `commit_tab::` at all — and `pub` inside a `pub mod` raises no `dead_code` warning, so this will never surface on its own. Take it back to `mod commit_tab` plus a `pub` → `pub(super)`/private pass; `git_panel.rs`'s `pub use commit_tab::{CommitSelection, CommitSelectionSource};` is the only export any other crate still needs. Include `ChangedFileEntry`'s three reader-less `pub` fields and `GitGraph::selected_commit_shas`.
- [ ] Fix `render_chip`'s doc comment, which still explains its `truncate` flag through the wrapping ref-chip row that died with the sidebar (fact 59).
- [ ] Tick every checkbox in this document and record the deferred items: the `affected_files`/commit-tree extraction (FORK.md #55), the two defects in fact 37, and the graph's column fractions in a compact band (fact 38).
