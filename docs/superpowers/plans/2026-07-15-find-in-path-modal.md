# Find in Path Modal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an IntelliJ-IDEA-style centered "Find in Path" modal (`ctrl-shift-f` / `ctrl-shift-r`) with scope tabs, grouped streaming results, a live read-only preview, and replace — reusing the existing `Project::search` backend.

**Architecture:** New module `crates/search/src/find_in_path.rs` inside the `search` crate (reuses its crate-private input-row helpers, `SearchOptions`, and replace idioms). A `FindInPath` view implements `workspace::ModalView`, opened via `workspace.toggle_modal`. It drives `Project::search` directly (streaming `SearchResults<SearchResult>`), builds a grouped `MatchList`, renders it in a `uniform_list`, and shows a `read_only` `Editor::for_buffer` preview of the selected match. A Solution is ONE `Project` with each member mounted as a worktree, so scope = include-pattern shaping on a single search call (no fan-out).

**Tech Stack:** Rust, GPUI, `project::search::{SearchQuery, SearchResult, SearchResults}`, `editor::Editor`, `crates/search` helpers, `solutions::SolutionStore`.

## Global Constraints

- Feature lives in `crates/search` only (no new crate). Register from `search::init` (`crates/search/src/search.rs:23-27`).
- Never `unwrap()`/`expect()` on fallible paths; propagate with `?` or `.log_err()`. No `let _ =` on fallible ops.
- GPUI test timers: use `cx.background_executor().timer(..)`, never `smol::Timer::after`.
- Build for agent verification with `cargo build --bin sawe` (debug) + `cargo test -p search --lib` (`set -o pipefail`, `timeout: 600000`). Do NOT run release builds for agent checks.
- `docs/superpowers/` is git-ignored → commit plan/spec edits with `git add -f`.
- Any UI-touching task is "done" only after a `workspace.screenshot` of the running editor via `script/run-mcp --debug --headless` is captured and looks correct.
- Locked identifiers (display name `Sawe`, etc.) are out of scope — do not touch.
- Reference spec: `docs/superpowers/specs/2026-07-15-find-in-path-modal-design.md`.

## Solution → scope mapping (decisive facts)

- `workspace.project()` → `&Entity<Project>` (`crates/workspace/src/workspace.rs:2798`). One Project per Solution; members are worktrees.
- `project.read(cx).visible_worktrees(cx)` → worktrees; `worktree.read(cx).root_name_str()` (`crates/worktree/src/worktree.rs:2677`), `worktree.read(cx).abs_path()` (`:741`).
- Active member: `solutions::SolutionStore::global(cx)` (`crates/solutions/src/store.rs:252`) → `store.read(cx).active_member_path(sol_id)` (`crates/solutions/src/store/members.rs:45-51`); resolve the Solution via `store.read(cx).solution_for_path(&worktree.read(cx).abs_path())` (`store.rs:321-326`).
- `Project::search(query, cx) -> SearchResults<SearchResult>` (`crates/project/src/project.rs:4690`), called as `project.update(cx, |p, cx| p.search(query, cx))`.
- `match_full_paths = project.read(cx).visible_worktrees(cx).count() > 1`. When true, include/exclude patterns match against `root_name/<relative>`, so an In-Project include must be `<root_name_str>/**`.
- `PathMatcher::new(globs, project.read(cx).path_style(cx))` (`crates/util/src/paths.rs:959`). Empty include ⇒ matches everything (In Solution).

---

## File Structure

- **Create** `crates/search/src/find_in_path.rs` — the whole feature: `init`, actions, `FindInPath` view (`ModalView`), `Scope` enum, `MatchList`/`FileGroup`/`MatchRow`/`Row`, query builder, streaming, results render, preview, replace.
- **Create** `crates/search/src/find_in_path_tests.rs` — GPUI unit tests (included via `#[cfg(test)] mod find_in_path_tests;` from `find_in_path.rs`).
- **Modify** `crates/search/src/search.rs` — add `mod find_in_path;` (via `Cargo.toml` lib? no — `search` uses `[lib] path`; add `mod find_in_path;` in the crate root file) and call `find_in_path::init(cx);` inside `init`.
- **Modify** `assets/keymaps/default-linux.json:680`, `default-windows.json:651`, `default-macos.json:686` — rebind `ctrl-shift-f`/`cmd-shift-f` → `find_in_path::Toggle`, add `ctrl-shift-r`/`cmd-shift-r` → `find_in_path::ToggleReplace` in the `Workspace` context.
- **Modify** `FORK.md` — one touched-files row + a "Key architectural decisions" entry.

Find the `search` crate root file first: run `grep -n '^path' crates/search/Cargo.toml` — the `[lib] path = "src/search.rs"` file is where `mod find_in_path;` goes.

---

## Task 1: Module scaffold, actions, and an empty modal that opens

**Files:**
- Create: `crates/search/src/find_in_path.rs`
- Create: `crates/search/src/find_in_path_tests.rs`
- Modify: `crates/search/src/search.rs` (add `mod find_in_path;`, call `find_in_path::init(cx)`)

**Interfaces:**
- Produces: `pub fn init(cx: &mut App)`; `struct FindInPath` (`impl ModalView`); actions `find_in_path::Toggle { replace_enabled: bool }` and `find_in_path::ToggleReplace`.

- [ ] **Step 1: Write the failing test** — dispatching `Toggle` puts a `FindInPath` modal on the workspace.

In `crates/search/src/find_in_path_tests.rs`:
```rust
use super::*;
use gpui::{TestAppContext, VisualTestContext};
use project::{FakeFs, Project};
use serde_json::json;
use settings::SettingsStore;
use workspace::{AppState, Workspace};

#[gpui::test]
async fn test_toggle_opens_modal(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "hello world\n" })).await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let (workspace, cx) =
        cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

    cx.dispatch_action(Toggle::default());
    workspace.update(cx, |workspace, cx| {
        assert!(
            workspace.active_modal::<FindInPath>(cx).is_some(),
            "Toggle should open the FindInPath modal"
        );
    });
}

fn init_test(cx: &mut TestAppContext) -> std::sync::Arc<AppState> {
    cx.update(|cx| {
        let state = AppState::test(cx);
        theme::init(theme::LoadThemes::JustBase, cx);
        language::init(cx);
        editor::init(cx);
        workspace::init(state.clone(), cx);
        super::init(cx);
        state
    })
}
```
(Model `init_test` on the existing `crates/search/src/project_search.rs` test harness — copy its exact `init_test` if the above helpers differ; grep `fn init_test` in `project_search.rs`.)

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p search --lib find_in_path -- --nocapture`
Expected: FAIL — `cannot find function init`/`FindInPath` unresolved.

- [ ] **Step 3: Scaffold the module**

In `crates/search/src/find_in_path.rs`:
```rust
use gpui::{
    actions, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Styled, Window,
};
use serde::Deserialize;
use schemars::JsonSchema;
use ui::prelude::*;
use workspace::{DismissDecision, ModalView, Workspace};

#[cfg(test)]
mod find_in_path_tests;

/// Opens the Find in Path modal (project-wide search overlay).
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, gpui::Action)]
#[action(namespace = find_in_path)]
#[serde(deny_unknown_fields)]
pub struct Toggle {
    #[serde(default)]
    pub replace_enabled: bool,
}

actions!(
    find_in_path,
    [
        /// Opens the Find in Path modal with the replace field revealed.
        ToggleReplace
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

fn register(workspace: &mut Workspace, _window: Option<&mut Window>, _cx: &mut Context<Workspace>) {
    workspace.register_action(|workspace, action: &Toggle, window, cx| {
        FindInPath::toggle(workspace, action.replace_enabled, window, cx);
    });
    workspace.register_action(|workspace, _: &ToggleReplace, window, cx| {
        FindInPath::toggle(workspace, true, window, cx);
    });
}

pub struct FindInPath {
    focus_handle: FocusHandle,
    replace_enabled: bool,
}

impl FindInPath {
    fn toggle(
        workspace: &mut Workspace,
        replace_enabled: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(existing) = workspace.active_modal::<Self>(cx) {
            existing.update(cx, |this, cx| {
                this.replace_enabled |= replace_enabled;
                this.focus_handle.focus(window);
                cx.notify();
            });
            return;
        }
        workspace.toggle_modal(window, cx, |_window, cx| Self {
            focus_handle: cx.focus_handle(),
            replace_enabled,
        });
    }
}

impl Focusable for FindInPath {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for FindInPath {}

impl ModalView for FindInPath {
    fn fade_out_background(&self) -> bool {
        true
    }
    fn debug_kind(&self) -> &'static str {
        "FindInPath"
    }
}

impl Render for FindInPath {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Placeholder shell — replaced in Task 4 with the real header/results/preview.
        v_flex()
            .key_context("FindInPath")
            .track_focus(&self.focus_handle)
            .w(rems(60.))
            .h(rems(30.))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_lg()
            .child("Find in Path")
    }
}
```
(`DismissEvent` comes from `gpui`; add it to the `use gpui::{..}` list. Confirm `ManagedView`/`ModalView` require `EventEmitter<DismissEvent>` + `Focusable` — they do; `file_finder.rs` shows the same bounds.)

- [ ] **Step 4: Wire into `search::init`**

In the `search` crate root (`crates/search/src/search.rs`), add near the other `mod` decls:
```rust
pub mod find_in_path;
```
and inside `pub fn init(cx: &mut App)` (`search.rs:23-27`) add:
```rust
    find_in_path::init(cx);
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p search --lib find_in_path -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Visual smoke-check**

Build + launch: `cargo build --bin sawe && script/run-mcp --debug --headless` (opens Solution). Then via the per-solution socket: `windows.send_keystroke {keystroke:"ctrl-shift-f"}` (after temporarily adding the keymap — or dispatch `windows.dispatch_action {action_name:"find_in_path::Toggle"}`), then `workspace.screenshot {solution_id, format:"png"}`. Expect a centered placeholder box over a dimmed backdrop. (Keymap swap is Task 10; use `dispatch_action` here.)

- [ ] **Step 7: Commit**

```bash
git add crates/search/src/find_in_path.rs crates/search/src/find_in_path_tests.rs crates/search/src/search.rs
git commit -m "search: Scaffold find_in_path modal (opens empty overlay)"
```

---

## Task 2: Scope model + `SearchQuery` builder (pure logic, full TDD)

**Files:**
- Modify: `crates/search/src/find_in_path.rs`
- Modify: `crates/search/src/find_in_path_tests.rs`

**Interfaces:**
- Produces:
  - `pub enum Scope { Solution, Project, Directory(std::path::PathBuf) }`
  - `fn active_member_root(workspace: &Workspace, cx: &App) -> Option<std::path::PathBuf>`
  - `fn include_patterns_for_scope(scope: &Scope, project: &Entity<Project>, cx: &App) -> Vec<String>`
  - `fn build_query(&self, cx: &App) -> Option<SearchQuery>` on `FindInPath` (consumes query text, options, scope, mask editors).

- [ ] **Step 1: Write the failing tests** for scope→include patterns.

Append to `find_in_path_tests.rs`:
```rust
#[gpui::test]
async fn test_include_patterns_for_scope(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/alpha", json!({ "a.rs": "" })).await;
    fs.insert_tree("/beta", json!({ "b.rs": "" })).await;
    let project = Project::test(fs, ["/alpha".as_ref(), "/beta".as_ref()], cx).await;

    project.read_with(cx, |project, cx| {
        // In Solution → no include restriction.
        assert!(super::include_patterns_for_scope(&Scope::Solution, &_entity(project), cx).is_empty());
    });
    // Directory scope → the dir path as a recursive glob (root-name prefixed since >1 worktree).
    let dir = std::path::PathBuf::from("/alpha/src");
    // (Exact expected string is asserted in Step 3 once root_name prefixing is settled.)
}
```
Note: because `include_patterns_for_scope` needs the `Entity<Project>`, write the test to call it through `project` (the `_entity` shim is illustrative — in practice pass the `Entity<Project>` you already hold: `let project: Entity<Project> = project.clone()`). Rewrite the assertion bodies concretely in Step 3 to match the implementation's exact strings (e.g. In Project on worktree `alpha` yields `["alpha/**"]`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p search --lib find_in_path::test_include_patterns_for_scope`
Expected: FAIL — `Scope` / `include_patterns_for_scope` unresolved.

- [ ] **Step 3: Implement scope + query builder**

Add to `find_in_path.rs` (imports: `project::{Project, search::SearchQuery}`, `util::paths::PathMatcher`, `std::path::PathBuf`):
```rust
#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    Solution,
    Project,
    Directory(PathBuf),
}

fn active_member_root(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let store = solutions::SolutionStore::global(cx);
    let project = workspace.project().read(cx);
    let first_root = project.visible_worktrees(cx).next()?.read(cx).abs_path();
    let solution = store.read(cx).solution_for_path(&first_root)?;
    store.read(cx).active_member_path(solution.id)
}

/// Build include globs restricting the search to `scope`. Empty ⇒ whole solution.
fn include_patterns_for_scope(scope: &Scope, project: &Entity<Project>, cx: &App) -> Vec<String> {
    let project_ref = project.read(cx);
    let multi = project_ref.visible_worktrees(cx).count() > 1;
    let root_glob = |abs: &std::path::Path| -> Option<String> {
        // Find the worktree containing `abs`, build "<root_name>/<relative>/**" (or "<rel>/**").
        for wt in project_ref.visible_worktrees(cx) {
            let wt = wt.read(cx);
            let wt_abs = wt.abs_path();
            if let Ok(rel) = abs.strip_prefix(&wt_abs) {
                let mut prefix = if multi { format!("{}/", wt.root_name_str()) } else { String::new() };
                if rel.as_os_str().is_empty() {
                    prefix.push_str("**");
                } else {
                    prefix.push_str(&rel.to_string_lossy());
                    prefix.push_str("/**");
                }
                return Some(prefix);
            }
        }
        None
    };
    match scope {
        Scope::Solution => Vec::new(),
        Scope::Project => {
            // Restrict to the active member's worktree root(s).
            // (active member root resolved by caller; here fall back to all-of-active-worktree.)
            Vec::new() // placeholder — real impl fills from active_member_root, see below
        }
        Scope::Directory(dir) => root_glob(dir).into_iter().collect(),
    }
}
```
Then implement `FindInPath::build_query(&self, cx)` mirroring `ProjectSearchView::build_search_query` (`crates/search/src/project_search.rs:1388-1529`): read `self.query_editor` text, `self.search_options` (a `SearchOptions` field), parse include/exclude editors via a local `parse_path_matches` copy (`project_search.rs:1541-1549`), THEN **merge** the scope include patterns into the include `PathMatcher`. Because `PathMatcher::new` takes a glob list, build the final include as `scope_patterns ++ user_include_patterns` (empty scope list ⇒ user patterns only ⇒ whole solution). Set `match_full_paths = project.read(cx).visible_worktrees(cx).count() > 1`. Use `SearchQuery::regex(..)` when `SearchOptions::REGEX` is set else `SearchQuery::text(..)` (signatures: `crates/project/src/search.rs:96,148`).

For `Scope::Project`, replace the placeholder: resolve `active_member_root(workspace, cx)` at modal-construction time and store it on `FindInPath` (the modal doesn't hold `&Workspace`), then in `include_patterns_for_scope` use `root_glob(&stored_member_root)`.

- [ ] **Step 4: Make the test concrete and pass**

Fill the Step-1 assertions with the exact strings the implementation produces (In Project on worktree `alpha` ⇒ `["alpha/**"]`; Directory `/alpha/src` ⇒ `["alpha/src/**"]`; Solution ⇒ `[]`). Run:
`cargo test -p search --lib find_in_path::test_include_patterns_for_scope`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/search/src/find_in_path.rs crates/search/src/find_in_path_tests.rs
git commit -m "search: find_in_path scope model + SearchQuery builder"
```

---

## Task 3: Streaming search into a grouped `MatchList`

**Files:**
- Modify: `crates/search/src/find_in_path.rs`
- Modify: `crates/search/src/find_in_path_tests.rs`

**Interfaces:**
- Produces:
  - `struct MatchRow { range: Range<text::Anchor>, line: u32, snippet: SharedString }`
  - `struct FileGroup { path: Arc<Path>, buffer: Entity<Buffer>, matches: Vec<MatchRow> }`
  - `enum Row { Header(usize /*group idx*/), Match(usize /*group*/, usize /*match*/) }`
  - `struct MatchList { groups: Vec<FileGroup>, rows: Vec<Row> }` with `fn push_result(&mut self, buffer, ranges, cx)`, `fn rebuild_rows(&mut self)`, `fn total_matches(&self)`, `fn file_count(&self)`.
  - `FindInPath::perform_search(&mut self, cx)` — debounced; spawns the stream drain; updates `self.results` + `self.status`.

- [ ] **Step 1: Write the failing test** for `MatchList` grouping/flatten (deterministic, no real search).

Append to `find_in_path_tests.rs`:
```rust
#[gpui::test]
async fn test_matchlist_groups_and_flattens(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "foo\nfoo\n", "b.txt": "foo\n" })).await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let buffer_a = project
        .update(cx, |p, cx| p.open_local_buffer("/root/a.txt", cx))
        .await
        .unwrap();
    let buffer_b = project
        .update(cx, |p, cx| p.open_local_buffer("/root/b.txt", cx))
        .await
        .unwrap();

    let mut list = MatchList::default();
    cx.update(|cx| {
        let a_ranges = buffer_a.read(cx).snapshot().as_rope(); // build 2 anchors — see note
        // For the test, use text::Anchor::MIN..MIN twice to represent 2 matches.
        list.push_result(buffer_a.clone(), vec![anchor_range(&buffer_a, cx), anchor_range(&buffer_a, cx)], cx);
        list.push_result(buffer_b.clone(), vec![anchor_range(&buffer_b, cx)], cx);
        list.rebuild_rows();
    });

    assert_eq!(list.file_count(), 2);
    assert_eq!(list.total_matches(), 3);
    // rows = [Header(0), Match(0,0), Match(0,1), Header(1), Match(1,0)]
    assert_eq!(list.rows.len(), 5);
    assert!(matches!(list.rows[0], Row::Header(0)));
    assert!(matches!(list.rows[1], Row::Match(0, 0)));
    assert!(matches!(list.rows[3], Row::Header(1)));
}
```
(Add a small `anchor_range(buffer, cx)` test helper returning `buffer.read(cx).anchor_before(0)..buffer.read(cx).anchor_after(0)`. `text::Anchor` API: grep `anchor_before` in `crates/language/src/buffer.rs`.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p search --lib find_in_path::test_matchlist_groups`
Expected: FAIL — `MatchList` unresolved.

- [ ] **Step 3: Implement `MatchList`** (grouping by buffer, snippet = the matched line's text). `push_result` computes each match's line + a trimmed snippet from the buffer snapshot; `rebuild_rows` flattens groups → `rows`. `total_matches`/`file_count` are counts. Keep it panic-free (bounds-checked indexing).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p search --lib find_in_path::test_matchlist_groups`
Expected: PASS.

- [ ] **Step 5: Wire real streaming search** — add fields to `FindInPath`: `results: MatchList`, `status: SearchStatus`, `search_task: Option<Task<()>>`, `search_options: SearchOptions`, `scope: Scope`, `project: Entity<Project>`, `member_root: Option<PathBuf>`. Implement `perform_search`:
```rust
fn perform_search(&mut self, cx: &mut Context<Self>) {
    let Some(query) = self.build_query(cx) else {
        self.results = MatchList::default();
        self.status = SearchStatus::Idle;
        self.search_task = None;
        cx.notify();
        return;
    };
    let project = self.project.clone();
    self.search_task = Some(cx.spawn(async move |this, cx| {
        // debounce
        cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
        let results = project.update(cx, |p, cx| p.search(query, cx));
        let Ok(project::search::SearchResults { rx, _task_handle }) = results else { return };
        let mut chunks = std::pin::pin!(rx.ready_chunks(1024));
        let mut list = MatchList::default();
        while let Some(batch) = futures::StreamExt::next(&mut chunks).await {
            for result in batch {
                match result {
                    project::search::SearchResult::Buffer { buffer, ranges } => {
                        this.update(cx, |this, cx| this.results.push_result(buffer, ranges, cx)).ok();
                    }
                    project::search::SearchResult::LimitReached => { /* set status */ }
                    _ => {}
                }
            }
            this.update(cx, |this, cx| { this.results.rebuild_rows(); cx.notify(); }).ok();
            futures::future::yield_now().await;
        }
        drop(_task_handle);
    }));
    cx.notify();
}
```
(Mirror the batching/`ready_chunks(1024)`/`yield_now` shape from `crates/search/src/project_search.rs:447-517`. Adjust `project.update(cx, ..)` to `AsyncApp` form — see `project_search.rs:423-441` for the exact async-context call.)

- [ ] **Step 6: Manual/e2e smoke** — after Task 4 wires the input, typing a query streams grouped results (verified by screenshot in Task 4). For now, `cargo test -p search --lib find_in_path` all green.

- [ ] **Step 7: Commit**

```bash
git add crates/search/src/find_in_path.rs crates/search/src/find_in_path_tests.rs
git commit -m "search: find_in_path streaming grouped MatchList"
```

---

## Task 4: Header UI (query editor + option toggles) driving search

**Files:**
- Modify: `crates/search/src/find_in_path.rs`

**Interfaces:**
- Consumes: `render_text_input` (`crates/search/src/search_bar.rs:97`, `pub(crate)`), `SearchOption::as_button` (`crates/search/src/search.rs:112`), `SearchOptions`.
- Produces: a `query_editor: Entity<Editor>`; option toggle handlers; `Render` shows header + results count.

- [ ] **Step 1: Add the query editor + options row.** In `FindInPath::toggle`'s builder, create `query_editor = cx.new(|cx| Editor::single_line(window, cx))`, subscribe to its edits to call `perform_search` (mirror how `ProjectSearchBar` subscribes to `query_editor` — grep `cx.subscribe(&self.query_editor` in `project_search.rs`). Store `search_options: SearchOptions::from_settings(SearchSettings::get_global(cx))`.

- [ ] **Step 2: Implement option toggle actions** — register `on_action` handlers for `search::ToggleCaseSensitive`, `ToggleWholeWord`, `ToggleRegex` that flip the corresponding `SearchOptions` bit and re-run `perform_search`. Render the toggles with `SearchOption::CaseSensitive.as_button(self.search_options, SearchSource::Buffer, self.focus_handle.clone())` etc. (the `Buffer` source dispatches the action, which your `on_action` catches).

- [ ] **Step 3: Replace the placeholder `Render`** with the real header:
```rust
v_flex()
    .key_context("FindInPath")
    .track_focus(&self.focus_handle)
    .w(relative(0.85)).h(relative(0.80)) // ~85% x 80% viewport
    .bg(cx.theme().colors().elevated_surface_background)
    .border_1().border_color(cx.theme().colors().border).rounded_lg()
    .on_action(cx.listener(Self::toggle_case_sensitive))
    .on_action(cx.listener(Self::toggle_whole_word))
    .on_action(cx.listener(Self::toggle_regex))
    .child(
        h_flex().p_2().gap_1()
            .child(search_bar::input_base_styles(cx.theme().colors().border, |d| d)
                .child(search_bar::render_text_input(&self.query_editor, None, cx)))
            .child(SearchOption::CaseSensitive.as_button(self.search_options, SearchSource::Buffer, self.focus_handle.clone()))
            .child(SearchOption::WholeWord.as_button(self.search_options, SearchSource::Buffer, self.focus_handle.clone()))
            .child(SearchOption::Regex.as_button(self.search_options, SearchSource::Buffer, self.focus_handle.clone())),
    )
    .child(div().flex_1()) // results area — Task 5
    .child(h_flex().p_1().child(self.status_label())) // status bar
```
(`search_bar::input_base_styles`/`render_text_input` are `pub(crate)` — same crate, callable directly. `SearchSource` is `pub`.)

- [ ] **Step 4: Verify with a screenshot** — `cargo build --bin sawe`, `script/run-mcp --debug --headless`, `dispatch_action find_in_path::Toggle`, `send_text "fn "`, `workspace.screenshot`. Expect the header with query text, three toggles, and a "N matches in M files" status line. Read the PNG to confirm.

- [ ] **Step 5: Commit**

```bash
git add crates/search/src/find_in_path.rs
git commit -m "search: find_in_path header input + option toggles"
```

---

## Task 5: Grouped results list (uniform_list) + keyboard navigation

**Files:**
- Modify: `crates/search/src/find_in_path.rs`

**Interfaces:**
- Consumes: `uniform_list(id, count, f)` (`crates/gpui/src/elements/uniform_list.rs:22`), `self.results.rows`.
- Produces: `selected_row: usize`; `select_next`/`select_prev`; `render_results(&mut self, cx) -> impl IntoElement`.

- [ ] **Step 1: Render `self.results.rows` in a `uniform_list`.** Each `Row::Header(g)` → a file-path header row (`self.results.groups[g].path`); each `Row::Match(g, m)` → line number + snippet, indented. Highlight `selected_row`. Use `cx.processor(...)` for the render closure (see `crates/picker/src/picker.rs:872-888`). Place it in the flex-1 area from Task 4, occupying the left ~40% (an `h_flex` with results | preview).

- [ ] **Step 2: Keyboard nav** — register `menu::SelectNext`/`menu::SelectPrevious` (or dedicated actions) on the modal that move `selected_row` skipping `Header` rows onto the next `Match`, `cx.notify()`, and (Task 6) update the preview. Clicking a match row sets `selected_row` without stealing input focus.

- [ ] **Step 3: Verify with a screenshot** — search a common token, confirm grouped list (file headers + indented match lines with line numbers), and that Down/Up move the highlight. Screenshot + `dump_visual_structure` `clickables` to confirm rows are present.

- [ ] **Step 4: Commit**

```bash
git add crates/search/src/find_in_path.rs
git commit -m "search: find_in_path grouped results list + keyboard nav"
```

---

## Task 6: Live read-only preview pane

**Files:**
- Modify: `crates/search/src/find_in_path.rs`

**Interfaces:**
- Consumes: `Editor::for_buffer` (`crates/editor/src/editor.rs:1681`), `set_read_only` (`:3062`), `highlight_background` (`:8804`), `request_autoscroll(Autoscroll::center(), cx)`.
- Produces: `preview_editor: Option<(Entity<Buffer>, Entity<Editor>)>`; `update_preview(&mut self, window, cx)`.

- [ ] **Step 1: Implement `update_preview`** — from `selected_row`, resolve the `FileGroup` + `MatchRow`. If the group's buffer differs from the current preview buffer, build a fresh editor:
```rust
let editor = cx.new(|cx| {
    let mut e = Editor::for_buffer(buffer.clone(), Some(self.project.clone()), window, cx);
    e.set_read_only(true);
    e.set_show_gutter(true, cx); // grep exact setter name in editor.rs
    e
});
```
Then always: `editor.update(cx, |e, cx| { e.highlight_background::<PreviewHighlight>(HighlightKey::Type(..), &[match.range.clone()], |_, theme| theme.colors().search_match_background, cx); e.request_autoscroll(Autoscroll::center(), cx); })`. (Define a marker `enum PreviewHighlight {}`; confirm the `highlight_background` key type via its signature — it takes `HighlightKey`.) Same-file line change → skip rebuild, just re-highlight + autoscroll.

- [ ] **Step 2: Render the preview** in the right ~60% of the results/preview `h_flex`: `.child(div().w(relative(0.6)).children(self.preview_editor.as_ref().map(|(_, e)| e.clone())))`. When no selection, show a muted "Select a match" placeholder.

- [ ] **Step 3: Call `update_preview`** on every selection change (Task 5 nav + click).

- [ ] **Step 4: Verify with a screenshot** — search, arrow down through matches, confirm the right pane shows the file with the selected match highlighted and scrolled into view, syntax-highlighted. Screenshot each of two different files to confirm the editor rebuilds per file.

- [ ] **Step 5: Commit**

```bash
git add crates/search/src/find_in_path.rs
git commit -m "search: find_in_path live read-only preview pane"
```

---

## Task 7: Scope tabs (In Solution / In Project / Directory) + mask fields

**Files:**
- Modify: `crates/search/src/find_in_path.rs`
- Modify: `crates/search/src/find_in_path_tests.rs`

**Interfaces:**
- Consumes: `Scope`, `include_patterns_for_scope`, `build_query` (Task 2).
- Produces: scope-tab UI + `included_files_editor`/`excluded_files_editor` single-line editors; `set_scope(&mut self, Scope, cx)`.

- [ ] **Step 1: Add a test** that switching scope changes the produced query's include matcher (In Project restricts to the active member; In Solution does not). Assert via `build_query(cx).unwrap().as_inner()` include patterns, or via a match/no-match probe against a path outside the active member. Run → fail.

- [ ] **Step 2: Render scope tabs** — a segmented row (`In Solution | In Project | Directory`) using `ui` toggle buttons; selecting one calls `set_scope` and re-runs `perform_search`. `Directory` reveals a path input (single-line editor) whose text becomes `Scope::Directory(PathBuf)`.

- [ ] **Step 3: Render mask fields** — two single-line editors ("File mask" include, "Exclude") using `render_text_input`; subscribe to edits → `perform_search`. `build_query` already merges these (Task 2).

- [ ] **Step 4: Wire `active_member_root`** at construction (store `member_root`) so In Project uses the active member; if none, In Project falls back to the first worktree.

- [ ] **Step 5: Run the Step-1 test → pass.** Then screenshot: switch to In Project, confirm results shrink to the active member; type a mask `*.rs`, confirm filtering. Screenshot the Directory path field.

- [ ] **Step 6: Commit**

```bash
git add crates/search/src/find_in_path.rs crates/search/src/find_in_path_tests.rs
git commit -m "search: find_in_path scope tabs + file mask fields"
```

---

## Task 8: Replace (field + Replace / Replace All) + ctrl-shift-r

**Files:**
- Modify: `crates/search/src/find_in_path.rs`
- Modify: `crates/search/src/find_in_path_tests.rs`

**Interfaces:**
- Consumes: `SearchQuery::with_replacement` (`crates/project/src/search.rs:356`), the buffer-level replace (see below).
- Produces: `replace_editor: Entity<Editor>`; `replace_next`/`replace_all` handlers; `replace_enabled` toggles the field's visibility.

- [ ] **Step 1: Write a failing replace test** — build a project with `a.txt: "foo foo\n"`, run a Find-in-Path search for `foo`, invoke `replace_all` with replacement `bar`, assert the buffer text becomes `bar bar\n`.
```rust
#[gpui::test]
async fn test_replace_all(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "foo foo\n" })).await;
    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    let (workspace, cx) = cx.add_window_view(|w, cx| Workspace::test_new(project.clone(), w, cx));
    cx.dispatch_action(Toggle { replace_enabled: true });
    let modal = workspace.update(cx, |w, cx| w.active_modal::<FindInPath>(cx).unwrap());
    modal.update_in(cx, |modal, window, cx| {
        modal.query_editor.update(cx, |e, cx| e.set_text("foo", window, cx));
        modal.replace_editor.update(cx, |e, cx| e.set_text("bar", window, cx));
    });
    cx.run_until_parked();
    modal.update_in(cx, |modal, window, cx| modal.replace_all(&ReplaceAll, window, cx));
    cx.run_until_parked();
    let buffer = project.update(cx, |p, cx| p.open_local_buffer("/root/a.txt", cx)).await.unwrap();
    buffer.read_with(cx, |b, _| assert_eq!(b.text(), "bar bar\n"));
}
```
(Import `search::ReplaceAll`.)

- [ ] **Step 2: Run → fail** (`replace_all` unresolved).

- [ ] **Step 3: Implement replace.** For each `FileGroup`, apply the replacement to that buffer's match ranges. Reuse the buffer-editing path: build `let query = self.active_query.clone()?.with_replacement(replacement)`, then for each group construct/hold an `Editor` (or edit the buffer directly via `buffer.update(cx, |b, cx| b.edit(edits, None, cx))` computing replacement text from `query.replacement()` and each range). The simplest robust route: iterate groups, and for each, apply `SearchQuery`-driven replacement text to `ranges` on the `Entity<Buffer>` with `buffer.update(cx, |b, cx| b.edit(ranges.iter().map(|r| (r.clone(), replacement_for(r))), None, cx))`. For regex replacement, use `query.replacement()` + the regex capture expansion helper (grep `fn replacement_for` / how `editor.replace` computes text in `crates/editor/src/items.rs:1778`). Register `on_action(search::ReplaceAll)` and `on_action(search::ReplaceNext)`. After replace, re-run `perform_search`.

- [ ] **Step 4: Toggle replace field** — when `replace_enabled`, render `replace_editor` row (via `render_text_input`) + `Replace` / `Replace All` buttons (`search_bar::render_action_button` with `search::ReplaceNext`/`ReplaceAll`). `ctrl-shift-r` already routes here (Task 1 `ToggleReplace`).

- [ ] **Step 5: Run → pass.** Then screenshot the replace row + do a live replace via MCP and screenshot the updated results.

- [ ] **Step 6: Commit**

```bash
git add crates/search/src/find_in_path.rs crates/search/src/find_in_path_tests.rs
git commit -m "search: find_in_path replace field + Replace/Replace All"
```

---

## Task 9: "Open in Find Window" + open-match-on-Enter + Esc-to-close

**Files:**
- Modify: `crates/search/src/find_in_path.rs`

**Interfaces:**
- Consumes: `workspace::DeploySearch` (`crates/workspace/src/pane.rs:189`), `workspace.open_path`/`open_path_preview`.
- Produces: status-bar button + `open_selected(&mut self, window, cx)`.

- [ ] **Step 1: "Open in Find Window" button** in the status bar. On click: dispatch `workspace::DeploySearch { query: Some(self.query_editor.read(cx).text(cx)), regex: Some(self.search_options.contains(SearchOptions::REGEX)), case_sensitive: .., whole_word: .., replace_enabled: self.replace_enabled, included_files: .., excluded_files: .., .. }` on the window, then dismiss the modal (`cx.emit(DismissEvent)`). This reuses the existing `ProjectSearchView` pane tab.

- [ ] **Step 2: Enter opens the selected match** — `open_selected` resolves the selected `MatchRow`'s buffer path + line, then `workspace.update(cx, |w, cx| w.open_path(project_path, None, true, window, cx))` and scrolls to the line; dismiss the modal. Register on `menu::Confirm` / `Enter`.

- [ ] **Step 3: Esc closes** — the `ModalLayer` already dismisses on `menu::Cancel`/overlay click; confirm Esc works (add `on_action(menu::Cancel → cx.emit(DismissEvent))` if needed).

- [ ] **Step 4: Verify with screenshots + e2e** — Enter on a match opens the file at the right line (screenshot the editor); "Open in Find Window" opens the old project-search tab pre-filled (screenshot).

- [ ] **Step 5: Commit**

```bash
git add crates/search/src/find_in_path.rs
git commit -m "search: find_in_path open-in-find-window, open-on-enter, esc-close"
```

---

## Task 10: Keymap swap (Linux/Windows/macOS) + coexistence

**Files:**
- Modify: `assets/keymaps/default-linux.json:680`, `assets/keymaps/default-windows.json:651`, `assets/keymaps/default-macos.json:686`

**Interfaces:** none (keymap only).

- [ ] **Step 1: Rebind in the `Workspace` context.** In `default-linux.json`, change line 680 and add the replace bind:
```json
      "ctrl-shift-f": "find_in_path::Toggle",
      "ctrl-shift-r": ["find_in_path::Toggle", { "replace_enabled": true }],
```
Do the same in `default-windows.json:651` (`ctrl-shift-*`) and `default-macos.json:686` (`cmd-shift-f` / `cmd-shift-r`). Leave `shift-find` → `pane::DeploySearch` as the discoverable path to the old tab search. Do NOT touch: `search::FocusSearch` (in-editor, `default-linux.json:453`), `project_search::ToggleFocus` (Pane, :523), `buffer_search::Deploy` (Terminal, :1259), jetbrains `NewSearchInDirectory`.

- [ ] **Step 2: Validate JSON** — `python3` comment/trailing-comma strip + `json.loads` (per the recipe used for the ctrl-shift-n change).

- [ ] **Step 3: Rebuild + verify the real keystroke** — `cargo build --bin sawe`, `script/run-mcp --debug --headless`, `windows.send_keystroke {keystroke:"ctrl-shift-f"}` → modal opens; `ctrl-shift-r` → modal opens with replace field. Screenshot both.

- [ ] **Step 4: Commit**

```bash
git add assets/keymaps/default-linux.json assets/keymaps/default-windows.json assets/keymaps/default-macos.json
git commit -m "keymap: Bind ctrl-shift-f/r to find_in_path modal (IDEA Find in Path)"
```

---

## Task 11: Docs + full regression pass

**Files:**
- Modify: `FORK.md`
- Modify: `docs/superpowers/specs/2026-07-15-find-in-path-modal-design.md` (status → shipped)

- [ ] **Step 1: FORK.md** — add a touched-files row for `crates/search/src/find_in_path.rs` (new fork-local module) and the keymap files, and a "Key architectural decisions" entry: *IDEA-style Find-in-Path is a bespoke `ModalView` in `crates/search`, not a re-shell of `ProjectSearchView`; scope tabs map to include-pattern shaping on one `Project::search` because a Solution is one Project with members as worktrees.*

- [ ] **Step 2: Full test run** — `set -o pipefail; cargo test -p search --lib 2>&1 | tail -40`. Expected: all green (existing `search` tests + new `find_in_path` tests). Investigate any regression in `project_search` tests.

- [ ] **Step 3: Full e2e screenshot pass** — open the modal, search, navigate, preview, switch scopes, apply a mask, do a replace, "Open in Find Window", open-on-Enter. Capture screenshots for each; read them to confirm.

- [ ] **Step 4: release-fast handoff build** — `cargo build --bin sawe --profile release-fast` in background so the user can hands-on test (assets/keymap embedded → rebuild required).

- [ ] **Step 5: Commit + push**

```bash
git add -f docs/superpowers/specs/2026-07-15-find-in-path-modal-design.md
git add FORK.md
git commit -m "docs: Mark find_in_path modal shipped; FORK.md decision + touched files"
git push origin main
```

---

## Self-Review

**Spec coverage:** §3 layout → Tasks 4/5/6/7; §4 components → Tasks 1–8; §5 data flow (scope→query, streaming, debounce, cancel) → Tasks 2/3; §6 replace → Task 8; §7 keymap+coexistence (Open in Find Window) → Tasks 9/10; §8 testing → per-task tests + Task 11; §9 risks R1(crate)→Task1, R2(composite/sizing)→Tasks4-6, R3(preview)→Task6, R4(streaming list)→Task3; §10 phasing → Task order; §11 out-of-scope (resize handle, history dropdown, custom scopes, preserve-case) → intentionally omitted. The §5 open question (Solution↔Project) is resolved in this plan's "Solution → scope mapping" section (one Project, members = worktrees).

**Placeholder scan:** The scope `Scope::Project` branch in Task 2 Step 3 is explicitly marked placeholder-then-resolved within the same task (Step 3 final paragraph + Task 7 Step 4 wire `member_root`); the `MatchList` test's anchor construction is flagged with the exact helper to add. No "TBD/implement later" left dangling across task boundaries.

**Type consistency:** `SearchOptions` (bitflags) vs `SearchOption` (enum with `as_button`) used correctly (toggles via `SearchOption::_::as_button`, state via `SearchOptions` field). `SearchQuery::{text,regex,with_replacement}`, `SearchResults{rx,_task_handle}`, `SearchResult::Buffer{buffer,ranges}`, `Editor::{for_buffer,set_read_only,highlight_background,request_autoscroll}`, `ModalView::{fade_out_background,debug_kind}` all match the quoted signatures. `Row`/`FileGroup`/`MatchRow`/`MatchList` names are consistent across Tasks 3/5/6/8.

## Notes for the implementer

- Copy the exact `init_test` helper from `crates/search/src/project_search.rs` (grep `fn init_test`) — the sketch in Task 1 may miss an init call the real harness needs.
- Several UI steps say "screenshot to verify" rather than asserting pixels — GPUI layout (flex ratios, sizing inside the `top_20()`-pinned `ModalLayer`) is iterated visually; the exact `.w()/.h()` and gaps are tuned against the screenshot, not guessed up front. If the modal renders off-screen or zero-height, the fix is an explicit fixed size on the outer `v_flex` (spec risk R2).
- If a needed MCP input primitive is missing during e2e, ADD it in-session (CLAUDE.md rule) rather than punting to manual testing.
- The buffer-level replace (Task 8 Step 3) is the least-templated part; prefer reusing `editor.replace_all` on a transient `Editor::for_buffer` per group (exactly what `ProjectSearchView` does with its `results_editor`) over hand-rolling regex capture expansion.
