# Solution-Scoped Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the member-project binding from AI sessions entirely — sessions are created at the solution root, are never filtered by the active member project, and carry no `member_id`.

**Architecture:** Peel the binding off in compiler-checked layers: first stop *filtering* chat tabs by member (UI), then stop *stamping* member_id/cwd at creation (store + callers), then delete the field and every mirror of it (metadata, DB read/write, DTO, label), then migrate existing DB rows and drop the now-false "member-less solution" guards. Each task compiles and passes tests on its own.

**Tech Stack:** Rust, GPUI, sqlez (sqlite). Crates: `solution_agent`, `console_panel`.

**Spec:** `docs/plans/2026-08-26-solution-band-ai-dialogs-design.md` (§1 "Scoping model"). This plan is phase 1 of 3 (phase 2 = sandwich layout, phase 3 = git panel Commit tab — planned separately).

## Global Constraints

- Existing sessions' `cwd` values must NOT be rewritten — claude-acp buckets transcripts by encoded cwd (`~/.claude/projects/<encoded-cwd>/…`), so changing a stored cwd breaks resume (`Resource not found`). Only `member_id` is nulled.
- The `solution_sessions.member_id` DB **column stays in place** (no destructive schema change); code stops reading/writing it.
- MCP `solution_agent.create_session` keeps its explicit `cwd` input (supervisor worktree children depend on it); only the member *inference* is removed.
- Terminals / GitGraph / Debug stay member-scoped — do not touch terminal `TabScope` logic.
- Build: `cargo build --bin sawe` (debug, no `--release`). Lint: `./script/clippy`. Never pipe cargo output through `tail` without `set -o pipefail`.
- Commits: imperative messages, no `Co-Authored-By`, never amend. Push to `origin main` after each green task is fine (solo repo, pre-authorized).
- Watch out: rust-analyzer's flycheck may hold `target/debug/.cargo-lock`; if a build blocks on the lock, `pkill -f "cargo check --workspace"`.

---

### Task 1: Chat tabs stop being filtered by the active member

**Files:**
- Modify: `crates/console_panel/src/panel.rs` (`tab_scope`, ~line 1053–1086; doc comments at 107–132)
- Test: `crates/console_panel/src/panel.rs` (unit tests module, ~line 2132)

**Interfaces:**
- Consumes: `TabScope` / `tab_in_scope` (existing, unchanged signatures).
- Produces: `ConsolePanel::tab_scope` returns `TabScope::Unscoped` for every `ConsoleTab::Chat` (Unscoped = always visible per `tab_in_scope`). Terminals keep their `origin_cwd`-based placement.

- [ ] **Step 1: Write the failing test**

In the existing `#[cfg(test)]` module in `panel.rs` (next to `tab_in_scope_filters_by_active_member`), add:

```rust
#[test]
fn chat_tabs_are_always_in_scope_regardless_of_active_member() {
    // Chats are solution-scoped: whatever member is active, a chat tab
    // must be visible. Unscoped is the TabScope variant with that meaning.
    assert!(tab_in_scope(TabScope::Unscoped, None));
    assert!(tab_in_scope(TabScope::Unscoped, Some(MemberId(1))));
    assert!(tab_in_scope(TabScope::Unscoped, Some(MemberId(999))));
}
```

This passes trivially — the real change is in `tab_scope` (an `&self` method needing a full panel, unwieldy in a unit test), so the load-bearing check is the compile-time removal in Step 3 plus the existing filtering tests updated in Step 4.

- [ ] **Step 2: Find the chat arm of `tab_scope`**

Read `crates/console_panel/src/panel.rs:1053-1086`. The current chat arm is:

```rust
return match session.read(cx).member_id {
    Some(member_id) => TabScope::Member(member_id),
    None => TabScope::Root,
};
```

- [ ] **Step 3: Replace the chat arm**

```rust
// Chats are solution-scoped (spec 2026-08-26): never filtered by the
// active member. Unscoped = always visible.
return TabScope::Unscoped;
```

Also update the `TabScope` doc comment at ~line 110 (delete the "a chat carries the session's `member_id`" sentence — only terminals are placed now) and the module doc at 107 if it mentions chat scoping.

- [ ] **Step 4: Update existing filtering tests**

Look at the tests around line 2132 (`tab_in_scope_filters_by_active_member`, and any test that builds a chat tab and asserts it is hidden for a non-matching member). Any assertion of the form "chat tab with member A is hidden when member B is active" flips to "chat tab is visible whatever member is active". Pure-`tab_in_scope` assertions about `TabScope::Member`/`Root` stay — terminals still use them.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p console_panel`
Expected: PASS (all).

- [ ] **Step 6: Commit**

```bash
git add crates/console_panel/src/panel.rs
git commit -m "console_panel: stop filtering chat tabs by active member"
```

---

### Task 2: Creation paths stop stamping member_id and member cwd

**Files:**
- Modify: `crates/solution_agent/src/store.rs:789-811` (`create_session`), `:817-837` (`create_ephemeral_session`), `:846-870` (`create_session_with_cwd`), `:878-1035` (`create_session_with_parent`)
- Modify: `crates/console_panel/src/panel.rs:1495-1553` (`add_chat_tab`, `add_chat_tab_with_cwd`)
- Modify: `crates/console_panel/src/chat_provider.rs:92-130` (`new_tab`)
- Modify: `crates/solution_agent/src/mcp/lifecycle.rs:174-210` (MCP create_session handler)
- Test: `crates/solution_agent/src/store/tests/misc.rs`

**Interfaces:**
- Consumes: `SolutionStore::active_member` (no longer called from these paths).
- Produces: **new signatures** —
  `create_session_with_cwd(solution_id, agent_id, project, cwd: Option<PathBuf>, model: Option<String>, effort: Option<String>, cx)` and
  `create_session_with_parent(solution_id, agent_id, project, cwd: Option<PathBuf>, parent_session_id, model, effort, ephemeral_supervisor, ephemeral, cx)` — the `member_id` parameter is GONE from both. `cwd: None` now means "solution root", full stop.

- [ ] **Step 1: Write the failing test**

In `crates/solution_agent/src/store/tests/misc.rs` (pattern-match the existing `create_session_spawns_subprocess_once_per_pair` test at ~line 88 for scaffolding — solution registration, project, store setup):

```rust
#[gpui::test]
async fn create_session_roots_cwd_at_solution_root(cx: &mut TestAppContext) {
    let (store, solution_id, project) = init_store_with_solution(cx).await; // reuse this file's existing helper (exact name may differ — copy whichever helper create_session_spawns_subprocess_once_per_pair uses)
    let session_id = store
        .update(cx, |s, cx| {
            s.create_session(solution_id, agent_id(), project, cx)
        })
        .await
        .unwrap();
    store.read_with(cx, |s, cx| {
        let session = s.session(session_id).unwrap();
        let session = session.read(cx);
        // The solution root, not the active member's folder.
        assert_eq!(session.cwd, solution_root_path(), "cwd must be the solution root");
    });
}
```

Adapt helper names to what the file actually uses (the test scaffolding in that file already registers a solution with members and an active member — the point of the assertion is that the ACTIVE MEMBER's path is NOT chosen).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p solution_agent create_session_roots_cwd_at_solution_root`
Expected: FAIL — cwd equals the active member's folder (today `create_session` looks up `active_member`).

- [ ] **Step 3: Strip the member lookup from `create_session`**

`crates/solution_agent/src/store.rs:789-811` becomes:

```rust
pub fn create_session(
    &mut self,
    solution_id: SolutionId,
    agent_id: AgentServerId,
    project: Entity<project::Project>,
    cx: &mut Context<Self>,
) -> Task<Result<SolutionSessionId>> {
    // Sessions are solution-scoped: they start at the solution root and
    // the agent walks wherever it needs (spec 2026-08-26).
    self.create_session_with_cwd(solution_id, agent_id, project, None, None, None, cx)
}
```

- [ ] **Step 4: Drop the `member_id` parameter from the chain**

In `create_session_with_cwd` and `create_session_with_parent` delete the `member_id: Option<solutions::MemberId>` parameter (and its doc comment "The member the session is bound to…"). In `create_session_with_parent`'s body:
- work-dir resolution (`store.rs:948-961`) collapses to:

```rust
let work_dir = cwd.unwrap_or_else(|| solution.root.clone());
```

- title base (`store.rs:1019-1021`) collapses to:

```rust
let title_base: SharedString = SharedString::from(solution.name.clone());
let title = unique_session_title(&title_base, store, &solution_id, cx);
```

- delete the `s.member_id = member_id;` line (~1032). (`new_idle` already defaults it to `None` — the field itself dies in Task 3.)

Fix `create_ephemeral_session` (`store.rs:824-836`) by removing its `None, // member_id` argument.

- [ ] **Step 5: Fix the callers**

- `crates/console_panel/src/panel.rs:1495-1508` — `add_chat_tab` no longer computes `active_member_path`; it just delegates: `self.add_chat_tab_with_cwd(solution_id, project, None, window, cx);` (update its comment: None = solution root by design now). In `add_chat_tab_with_cwd` (~1536-1552) delete the `member_id` lookup and drop the argument from `create_session_with_cwd`.
- `crates/console_panel/src/chat_provider.rs:110-129` — delete the `member_id` lookup block and the argument.
- `crates/solution_agent/src/mcp/lifecycle.rs:176-210` — delete the member-inference block (lines 176-191, the `member_for_path`/`active_member` lookup) and the `member_id` argument. **Keep the `cwd` input** — explicit cwd still wins (worktree children).
- Grep for any remaining caller: `rg -n "create_session_with_cwd|create_session_with_parent" crates/` and fix argument lists (there is at least also `store/queue.rs` or supervisor code if it spawns children — follow the compiler).

- [ ] **Step 6: Build + run the tests**

Run: `cargo build -p solution_agent -p console_panel && cargo test -p solution_agent -p console_panel`
Expected: the new test PASSES; existing create-path tests may need their expectations updated from member-path cwd to solution root — update them (that behavior change is the point of this plan).

- [ ] **Step 7: Commit**

```bash
git add crates/solution_agent crates/console_panel
git commit -m "solution_agent: create sessions at the solution root, without a member binding"
```

---

### Task 3: Delete the `member_id` field and every mirror of it

**Files:**
- Modify: `crates/solution_agent/src/model.rs:356-360` (field), `:645` (new_idle), `:1164` (`SolutionSessionMetadata`)
- Modify: `crates/solution_agent/src/store.rs:567-577` (delete `project_label`), `:1256`, `:2034` (metadata mirrors), `:2882-2908` (`seed_cold_session`)
- Modify: `crates/solution_agent/src/store/queue.rs:855`, `crates/solution_agent/src/store/hydration.rs:684,740,996,1295,1504`
- Modify: `crates/solution_agent/src/session_view.rs:859`
- Modify: `crates/solution_agent/src/status_row.rs:146-160` (delete `cwd_label`), `:851` (delete the `Label::new(cwd_label)` element)
- Modify: `crates/solution_agent/src/db/sessions.rs:293,312,349,618,637,685` (drop the column from INSERT/SELECT/UPSERT)
- Modify: `crates/solution_agent/src/mcp/dto.rs:71,275` (field stays on the wire, hardcoded `None`)
- Modify: test struct literals — `model/tests.rs:96`, `db/tests.rs:91`, `mcp/tests.rs:2471`, `store/tests/hydration.rs` (multiple `member_id: None` lines)

**Interfaces:**
- Consumes: nothing new.
- Produces: `SolutionSession` and `SolutionSessionMetadata` have **no `member_id` field**. `mcp/dto.rs` keeps `pub member_id: Option<i64>` (wire compat with the mobile app) but always serializes `None`.

- [ ] **Step 1: Delete the fields and lean on the compiler**

Remove `pub member_id: Option<solutions::MemberId>` from `SolutionSession` (model.rs:356-360, including its doc comment) and from `SolutionSessionMetadata` (model.rs:1164). Run `cargo build -p solution_agent 2>&1 | rg "^error" -A3` and fix every site the compiler names:
- metadata construction mirrors (store.rs:1256, 2034; queue.rs:855; session_view.rs:859) — delete the line.
- hydration assignments (hydration.rs:684, 740, 996, 1295, 1504 — `s.member_id = meta.member_id;` / `session.member_id = meta.member_id;`) — delete.
- `seed_cold_session` (store.rs:2882-2908): the binding block becomes root-only —

```rust
// Seed at the solution root — chats are solution-scoped and never filtered,
// so no member binding is needed for the seed to be visible.
let root = SolutionStore::try_global(cx)
    .and_then(|store| {
        store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == solution_id)
                .map(|sol| sol.root.clone())
        })
    })
    .unwrap_or_default();
```

and delete `s.member_id = member_id;` (keep `s.cwd = root;`).
- test struct literals: delete each `member_id: None,` line.

- [ ] **Step 2: Delete `project_label` and the status-row project label**

- store.rs:567-577: delete the whole `project_label` fn (its only remaining caller was removed in Task 2; the compiler confirms).
- status_row.rs:146-160: delete the `cwd_label` computation. At status_row.rs:~851 delete the `Label::new(cwd_label)` element (and any separator/dot element that visually pairs with it — read the surrounding `div` chain and remove the orphaned separator too).

- [ ] **Step 3: Drop the column from DB read/write**

`crates/solution_agent/src/db/sessions.rs`:
- INSERT/UPSERT (~293, 312, 349): remove `member_id` from the column list, the `excluded.member_id` COALESCE line, and the `meta.member_id.map(|m| m.0)` bind.
- SELECT (~618, 637, 685): remove `member_id` from the column list, the tuple type, and the `member_id: member_id.map(solutions::MemberId)` construction.
The physical column stays in the schema (`db.rs:192` `apply_idempotent_add_column` may stay or go — leave it; it's idempotent and documents the column's existence for old DBs).

- [ ] **Step 4: Freeze the wire DTO**

`crates/solution_agent/src/mcp/dto.rs:275`: change to `member_id: None,` with a comment:

```rust
// Sessions are solution-scoped since 2026-08-26; the field is kept on the
// wire so older mobile clients keep deserializing, but it is always null.
member_id: None,
```

- [ ] **Step 5: Build, clippy, test**

Run: `cargo build -p solution_agent -p console_panel && ./script/clippy -p solution_agent -p console_panel && cargo test -p solution_agent -p console_panel`
Expected: PASS. Hydration tests that asserted `member_id` round-trips will need the assertion removed — delete those assertions, not the tests.

- [ ] **Step 6: Commit**

```bash
git add crates/solution_agent crates/console_panel
git commit -m "solution_agent: delete the session member_id field and project label"
```

---

### Task 4: DB migration — null existing member_ids, drop the backfill

**Files:**
- Modify: `crates/solution_agent/src/db.rs:383-490` (backfill fns + report), `crates/solution_agent/src/solution_agent.rs:162-206` (startup migration call/log)
- Test: `crates/solution_agent/src/db/tests.rs:1173-1261`

**Interfaces:**
- Consumes: the identity-migration entry point in `db.rs` (slug remap part stays).
- Produces: `MigrationReport` loses `member_ids_backfilled`; gains `member_ids_cleared: i64`. On every DB open, `UPDATE solution_sessions SET member_id = NULL WHERE member_id IS NOT NULL` runs (idempotent).

- [ ] **Step 1: Write the failing test**

In `db/tests.rs`, next to the existing migration test at :1194 (reuse its fixture-building scaffolding — it inserts legacy rows with member_ids):

```rust
#[gpui::test]
async fn migration_clears_member_ids(cx: &mut gpui::TestAppContext) {
    // Build a DB with sessions that carry member_id values (reuse the
    // fixture from migrate_identity_remaps_slugs_and_backfills_member_ids,
    // inserting rows with explicit non-NULL member_id).
    let db = build_legacy_db_with_member_ids(cx).await;
    let report = db.run_migrations().await.unwrap();
    assert!(report.member_ids_cleared >= 1);
    let remaining: i64 = db
        .select_row("SELECT COUNT(*) FROM solution_sessions WHERE member_id IS NOT NULL")
        .unwrap()()
        .unwrap()
        .unwrap();
    assert_eq!(remaining, 0, "no session may keep a member binding");
    // cwd must be untouched — resume depends on it.
}
```

(Adapt the exact query/exec helper calls to this file's existing style — it already runs raw SELECTs against `solution_sessions` at :1219.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p solution_agent migration_clears_member_ids`
Expected: FAIL (compile error on `member_ids_cleared` — field doesn't exist yet).

- [ ] **Step 3: Replace the backfill with the clear**

In `db.rs`:
- Delete the backfill logic at :383-411 and :435-490 (`backfill member_id` UPDATE loop, the `(member_id, solution_id, local_path)` matching). The slug-remap part of the migration stays.
- In the migration body add:

```rust
let cleared = connection.exec(indoc! {"
    UPDATE solution_sessions SET member_id = NULL WHERE member_id IS NOT NULL
"})?;
report.member_ids_cleared = /* rows affected via changes() — use the same
    rows-affected mechanism the surrounding migration code already uses */;
```

(If the surrounding code has no rows-affected helper, `SELECT changes()` right after the exec — matching the file's existing raw-SQL style.)
- Replace `member_ids_backfilled: i64` with `member_ids_cleared: i64` in `MigrationReport` (:425).

In `solution_agent.rs:162-206`: update the doc comment (":162 …backfill `member_id`…" no longer true) and the log line at :203-206 to report `member_ids_cleared`.

- [ ] **Step 4: Update the old migration test**

`db/tests.rs:1194` (`migrate_identity_remaps_slugs_and_backfills_member_ids`): rename to `migrate_identity_remaps_slugs` and flip its member assertions — after migration ALL rows have NULL member_id (the :1213/:1228/:1233/:1261 assertions change accordingly). Keep the slug-remap assertions untouched.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p solution_agent db::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/solution_agent
git commit -m "solution_agent: migrate away member bindings instead of backfilling them"
```

---

### Task 5: Remove the member-less-solution guards

**Files:**
- Modify: `crates/solution_agent/src/mcp/lifecycle.rs:134-162` (`solution_has_no_members` guard)
- Modify: `crates/console_panel/src/panel.rs` — the `workspace_has_project` gate on chat creation (grep `workspace_has_project` in `console_panel`; also check `crates/solution_agent` for a same-named helper)
- Test: `crates/solution_agent/src/mcp/tests.rs:3395` (`create_session_in_a_member_less_solution_is_rejected`), `:3423` (`create_session_with_a_member_clears_the_member_guard`)

**Interfaces:**
- Produces: creating a session in a solution with zero members is now legal — the session roots at `solution.root` (which always exists; the agent can add members itself via `solutions.add_empty_member`).

- [ ] **Step 1: Flip the MCP tests**

Rewrite `create_session_in_a_member_less_solution_is_rejected` as `create_session_in_a_member_less_solution_roots_at_solution_root`: same fixture, but the call now must SUCCEED and the created session's cwd equals the solution root. Delete `create_session_with_a_member_clears_the_member_guard` (the guard it exercises is gone; the happy path is covered by the renamed test plus Task 2's test).

- [ ] **Step 2: Run to verify the new test fails**

Run: `cargo test -p solution_agent create_session_in_a_member_less_solution`
Expected: FAIL — the guard still rejects.

- [ ] **Step 3: Delete the guards**

- `mcp/lifecycle.rs:134-162`: delete the whole `solution_has_no_members` block (comment included — its rationale "there is no legitimate member-less session" is now false by spec).
- Console UI: grep `workspace_has_project` in `crates/console_panel/` (and `crates/solution_agent/` — the lifecycle comment calls it "the UI guard"). Remove the *chat-creation* gating only (the "+ → New AI Chat" disable). If the same helper also gates terminal creation, keep the terminal gating — terminals still need a member folder to cd into.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p solution_agent -p console_panel`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/solution_agent crates/console_panel
git commit -m "solution_agent: allow sessions in member-less solutions"
```

---

### Task 6: Full verification + live check + docs

**Files:**
- Modify: `FORK.md` (decisions #5 `:187-191`, #27 `:429-445`, crates-table row for `console_panel` `:49`; new decision entry)
- Modify: `docs/INDEX.md` (if it references the plan table, add this plan's row)

- [ ] **Step 1: Whole-workspace build + test sweep**

Run: `cargo build --bin sawe && cargo test -p solution_agent -p console_panel -p editor_mcp`
Expected: PASS everywhere. (`editor_mcp` e2e tests exercise `solution_agent.create_session` over the socket — they must still pass with the changed semantics.)

- [ ] **Step 2: Live smoke test via MCP**

Launch `script/run-mcp --debug --headless` (build first — run-mcp only compiles if the binary is *missing*). Then over the per-solution socket:
1. `solution_agent.create_session` → `get_session` → assert `cwd` == solution root and `member_id` is null in the DTO.
2. `solutions.set_active_member` to each member in turn; after each, `workspace.screenshot {solution_id, format:"png"}` → the chat tab strip must show the same chat tabs in both screenshots (drive a real event between screenshots — e.g. the set_active_member itself repaints; use `windows.hover_at` a pixel apart if a nudge is needed).
Read the PNGs with the Read tool to confirm visually.

- [ ] **Step 3: Update FORK.md**

- Decision #5 (`:187-191`): rewrite — "cwd = solution.root (always)" is TRUE again as of this change; note the 2026-06→08 interlude where per-member cwd/member_id existed and why it was removed (spec 2026-08-26).
- Decision #27 (`:429-445`): the "AI dialogs stay agnostic" sentence is enforced again — add a pointer to the new decision entry.
- Crates-table row for `console_panel` (`:49`): fix the stale scoping sentence (chat tabs are no longer member-filtered; the `tab_cwd_in_scope` name mentioned there is long gone).
- Add a new numbered decision entry: "AI sessions are solution-scoped (member binding removed)" — why (sessions belong to the Solution; member filtering hid dialogs on project switch and contradicted #27) and how-to-apply (never reintroduce member_id reads; new session surfaces must not filter by active member; DB column is dead-but-present).

- [ ] **Step 4: Commit + push**

```bash
git add FORK.md docs/INDEX.md docs/plans/2026-08-26-solution-scoped-sessions.md docs/plans/2026-08-26-solution-band-ai-dialogs-design.md
git commit -m "FORK.md: record the solution-scoped sessions decision"
git push origin main
```

---

## Self-review notes

- Spec §1 coverage: member_id removal (Tasks 2–3), migration (Task 4), cwd = solution root (Task 2), filtering death (Task 1), project-label death (Task 3), terminals untouched (explicit constraint). Guards (Task 5) are the spec's "agents walk wherever they need" consequence.
- Spec §2–5 (layout, ConsolePanel split, git panel) are OUT of this plan — phases 2–3.
- Type consistency: `create_session_with_cwd(…, cwd, model, effort, cx)` / `create_session_with_parent(…, cwd, parent_session_id, model, effort, ephemeral_supervisor, ephemeral, cx)` used consistently in Tasks 2, 5.
