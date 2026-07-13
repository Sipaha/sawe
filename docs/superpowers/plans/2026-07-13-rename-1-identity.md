# Rename Phase 1 — Identity Model Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the slug-derived TEXT identities of solutions, members and catalog projects with surrogate integer counters, migrate every existing row across both databases without loss, and make `solution_sessions.member_id` the source of truth for a session's project (replacing cwd-equality inference).

**Architecture:** Ids become SQLite `INTEGER PRIMARY KEY` rowids in `SolutionsDb`. A new (third) migration in `crates/solutions/src/db.rs` rebuilds `catalog_projects`, `solutions`, `solution_members` and `active_member` under numeric ids by renaming the old tables aside, copying through `ROW_NUMBER()`-assigned counters, writing a row-count report row, and dropping the legacy tables. A persistent `solution_legacy_ids(old_id TEXT, new_id INTEGER)` map survives the migration so the *second* database (`solution_agent.db`, a plain `sqlez::Connection`, no `Domain`/`MIGRATIONS` machinery) can remap `solution_sessions.solution_id` from the old slug to the new counter at startup, and backfill the new `solution_sessions.member_id` column by matching each session's `cwd` against member `local_path`s. Every downstream crate is then converted to the numeric types; the per-solution MCP socket directory becomes `<runtime>/solutions/<numeric id>/mcp.sock` and stale slug-named directories are swept at startup.

**Tech Stack:** Rust (2024 edition), GPUI, `sqlez` / `sqlez_macros::sql!` / `db::query!` (solutions DB, migration-based `Domain`), raw `sqlez::connection::Connection` + `apply_idempotent_add_column` (solution_agent DB), `anyhow`, `schemars`/`serde` for MCP tool schemas, `cargo test` (debug).

## Global Constraints

- **Debug builds only.** Verify with `cargo test -p solutions`, `cargo test -p solution_agent`, `cargo test -p console_panel`, `cargo test -p editor_mcp`, `cargo check --workspace --all-targets`. Never `cargo build --release` and never `script/bundle-*`.
- **`INSERT OR REPLACE` is BANNED on any table that is an FK parent with `ON DELETE CASCADE`.** REPLACE deletes the parent row before re-inserting it, cascading its children away. This caused real data loss (fixed in `132e89f5a7`, see `docs/findings/2026-07-13-rename-solution-cascade-data-loss.md`). Use `INSERT … ON CONFLICT(<pk>) DO UPDATE SET …`. After this plan the FK parents are `solutions` (parents: `solution_members`, `active_member`), `solution_members` (parent: `active_member.member_id`) and `catalog_projects` (parent: `solution_members.origin_catalog_id`) — so `save_solution`, `set_solution_member` and `save_catalog_project` all MUST be `ON CONFLICT DO UPDATE`.
- **Never edit an existing entry in `SolutionsDb::MIGRATIONS`.** `sqlez` hashes/compares every previously-applied migration string and panics on a mismatch. Only append.
- **No string literals inside `sql!(…)`.** The `sqlez_macros::sql!` proc-macro tokenises with the Rust lexer: `'solutions'` lexes as a lifetime followed by an unterminated char literal and `''` as an empty char literal. Every migration in this plan is written without a single quoted literal.
- **The migration must fail loudly, never half-migrate.** Row counts of every converted table are captured in `identity_migration_report` *inside* the migration transaction; `solutions::migrate::verify_identity_migration` reads it at startup and returns `Err` on any mismatch.
- **Shared contract — other phase plans depend on these EXACT names. Do not rename:**
  - `pub struct SolutionId(pub i64)`, `pub struct MemberId(pub i64)`, `pub struct CatalogId(pub i64)` in `crates/solutions/src/model.rs`
  - `Solution { id: SolutionId, name: String, root: PathBuf, members: Vec<SolutionMember>, last_opened_at: Option<i64> }`
  - `SolutionMember { id: MemberId, name: String, local_path: PathBuf, origin_catalog_id: Option<CatalogId> }`
  - `SolutionStore::find_solution(&self, id: SolutionId) -> Result<&Solution>`, `SolutionStore::find_member(&self, id: MemberId) -> Result<&SolutionMember>`
  - DB fns keep their names: `save_solution`, `set_solution_member`, `delete_solution_member`, `set_active_member`, `load_all_solutions_with_members`.
- `last_opened_at` becomes `Option<i64>` (epoch millis) on `Solution` — the `chrono::DateTime<Utc>` field is gone. Convert at the UI/MCP edge with `chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)`.
- **Commit per task**, imperative subject line, **no `Co-Authored-By` trailer**, no `git commit --amend`.
- **Intermediate breakage is expected and accepted.** Task 1 flips `solutions`' public types; the crates that consume them (`solutions_ui`, `console_panel`, `git_ui`, …) do not compile again until Task 6. Each task's own verification command is scoped to crates that *do* compile at that point. Do not stop between tasks.
- Rust guidelines from `CLAUDE.md` apply: no `unwrap()` in non-test code, no `let _ =` on fallible calls, comments explain *why* only.

---

### Task 1: Numeric identity in the `solutions` crate (schema, migration, types, store)

**Files:**
- Modify: `crates/solutions/src/db.rs` (append migration #3; rewrite every `query!` to numeric ids; add report + legacy-map readers; rewrite the `mod tests`)
- Modify: `crates/solutions/src/model.rs` (whole file)
- Modify: `crates/solutions/src/store.rs:1-45,151-209,290-368` (hydration + fields + test helpers)
- Modify: `crates/solutions/src/store/lifecycle.rs` (whole file)
- Modify: `crates/solutions/src/store/members.rs` (whole file)
- Modify: `crates/solutions/src/store/catalog.rs` (mechanical: `CatalogId(String)` → `CatalogId(i64)`)
- Modify: `crates/solutions/src/add_member.rs` (member creation now allocates a `MemberId`; folder name derives from the catalog *name*)
- Modify: `crates/solutions/src/migrate.rs` (legacy JSON import allocates ids; add `verify_identity_migration`)
- Modify: `crates/solutions/src/persistence.rs` (`SolutionsConfig` is now an in-memory hydration struct only)
- Modify: `crates/solutions/src/mcp/solutions_lifecycle.rs`, `crates/solutions/src/mcp/member_mgmt.rs`, `crates/solutions/src/mcp/catalog.rs` (tool params/results take `i64` ids)
- Modify: `crates/solutions/src/event_sources.rs`, `crates/solutions/src/branch_protection.rs`, `crates/solutions/src/tests/persistence_e2e.rs` (mechanical)
- Test: `crates/solutions/src/db.rs` (`mod tests`), `crates/solutions/src/model.rs` (`mod tests`), `crates/solutions/src/store.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing (first task).
- Produces:
  - `solutions::{SolutionId, MemberId, CatalogId}` — `pub struct X(pub i64)`, each `#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)] #[serde(transparent)]`.
  - `solutions::Solution { id: SolutionId, name: String, root: PathBuf, members: Vec<SolutionMember>, last_opened_at: Option<i64> }`
  - `solutions::SolutionMember { id: MemberId, name: String, local_path: PathBuf, origin_catalog_id: Option<CatalogId> }`
  - `SolutionStore::find_solution(&self, id: SolutionId) -> anyhow::Result<&Solution>`
  - `SolutionStore::find_member(&self, id: MemberId) -> anyhow::Result<&SolutionMember>`
  - `SolutionStore::member_of(&self, id: MemberId) -> Option<SolutionId>`
  - `SolutionStore::active_member(&self, solution: SolutionId) -> Option<MemberId>`
  - `SolutionStore::set_active_member(&mut self, solution: SolutionId, member: MemberId, cx: &mut Context<Self>)`
  - `SolutionStore::remove_member(&mut self, member: MemberId, cx: &mut Context<Self>) -> Result<()>`
  - `SolutionStore::reorder_members(&mut self, solution: SolutionId, new_order: Vec<MemberId>, cx: &mut Context<Self>) -> Result<()>`
  - `SolutionsDb::load_solution_legacy_ids() -> Result<Vec<(String, i64)>>` (old slug → new counter; consumed by Task 2)
  - `SolutionsDb::insert_solution(name: String, root: String, last_opened_at: Option<i64>) -> Result<i64>`
  - `SolutionsDb::insert_solution_member(solution_id: i64, name: String, local_path: String, position: i32, origin_catalog_id: Option<i64>) -> Result<i64>`
  - `SolutionsDb::insert_catalog_project(name: String, remote_url: String, default_branch: Option<String>) -> Result<i64>`
  - `SolutionsDb::save_solution(id: i64, name: String, root: String, last_opened_at: Option<i64>) -> Result<()>`
  - `SolutionsDb::set_solution_member(id: i64, solution_id: i64, name: String, local_path: String, position: i32, origin_catalog_id: Option<i64>) -> Result<()>`
  - `SolutionsDb::delete_solution_member(id: i64) -> Result<()>`
  - `SolutionsDb::set_active_member(solution_id: i64, member_id: i64) -> Result<()>`
  - `SolutionsDb::load_all_solutions_with_members() -> Result<Vec<(i64, String, String, Option<i64>, i64, String, String, i32, Option<i64>)>>`
  - `solutions::migrate::verify_identity_migration(db: &SolutionsDb) -> anyhow::Result<()>`

- [x] **Step 1: Write the failing migration test**

Append to `mod tests` in `crates/solutions/src/db.rs` (keep the existing tests for now; they are rewritten in Step 8):

```rust
    use db::sqlez::connection::Connection;

    /// Build an in-memory DB at the PRE-identity schema (migrations 0 and 1
    /// only), seed it with rows shaped exactly like the user's production DB,
    /// then apply the full migration list so migration #2 (the identity
    /// migration) runs over real legacy data.
    fn legacy_seeded_connection(name: &str) -> Connection {
        let connection = Connection::open_memory(Some(name));
        connection
            .migrate(
                "SolutionsDb",
                &<SolutionsDb as Domain>::MIGRATIONS[..2],
                &mut |_, _, _| false,
            )
            .expect("legacy migrations");

        connection
            .exec("INSERT INTO catalog_projects (id, name, remote_url, default_branch) VALUES ('ecos-base', 'ECOS Base', 'git@x:ecos-base.git', 'master')")
            .expect("prepare catalog insert")()
        .expect("catalog insert");
        connection
            .exec("INSERT INTO solutions (id, name, root, last_opened_at) VALUES ('spk-solutions', 'Sawe', '/home/u/sol/spk-solutions', 1700000000000)")
            .expect("prepare solution insert")()
        .expect("solution insert");
        connection
            .exec("INSERT INTO solutions (id, name, root, last_opened_at) VALUES ('other', 'Other', '/home/u/sol/other', NULL)")
            .expect("prepare solution insert 2")()
        .expect("solution insert 2");
        connection
            .exec("INSERT INTO solution_members (solution_id, catalog_id, local_path, position) VALUES ('spk-solutions', 'ecos-base', '/home/u/sol/spk-solutions/ecos-base', 0)")
            .expect("prepare member insert")()
        .expect("member insert");
        connection
            .exec("INSERT INTO solution_members (solution_id, catalog_id, local_path, position) VALUES ('spk-solutions', 'sawe', '/home/u/sol/spk-solutions/sawe', 1)")
            .expect("prepare member insert 2")()
        .expect("member insert 2");
        connection
            .exec("INSERT INTO active_member (solution_id, catalog_id) VALUES ('spk-solutions', 'sawe')")
            .expect("prepare active insert")()
        .expect("active insert");
        connection
    }

    fn apply_all_migrations(connection: &Connection) {
        connection
            .migrate(
                "SolutionsDb",
                <SolutionsDb as Domain>::MIGRATIONS,
                &mut |_, _, _| false,
            )
            .expect("identity migration");
    }

    #[gpui::test]
    async fn identity_migration_converts_text_ids_to_counters() {
        let connection = legacy_seeded_connection("identity_migration_counters");
        apply_all_migrations(&connection);

        let solutions = connection
            .select::<(i64, String, String, Option<i64>)>(
                "SELECT id, name, root, last_opened_at FROM solutions ORDER BY id",
            )
            .expect("prepare select solutions")()
        .expect("select solutions");
        assert_eq!(solutions.len(), 2, "both solutions must survive");
        assert!(
            solutions.iter().all(|(id, _, _, _)| *id > 0),
            "ids must be positive counters: {solutions:?}"
        );
        assert!(
            solutions
                .iter()
                .any(|(_, name, root, ts)| name == "Sawe"
                    && root == "/home/u/sol/spk-solutions"
                    && *ts == Some(1_700_000_000_000)),
            "name/root/last_opened_at must be carried over verbatim: {solutions:?}"
        );

        let members = connection
            .select::<(i64, i64, String, String, i32, Option<i64>)>(
                "SELECT id, solution_id, name, local_path, position, origin_catalog_id
                 FROM solution_members ORDER BY solution_id, position",
            )
            .expect("prepare select members")()
        .expect("select members");
        assert_eq!(members.len(), 2);
        assert_eq!(
            members.iter().map(|m| m.2.as_str()).collect::<Vec<_>>(),
            vec!["ecos-base", "sawe"],
            "member name seeds from the old catalog_id"
        );
        assert_eq!(members[0].3, "/home/u/sol/spk-solutions/ecos-base");

        let catalog = connection
            .select::<(i64, String)>("SELECT id, name FROM catalog_projects")
            .expect("prepare select catalog")()
        .expect("select catalog");
        assert_eq!(catalog.len(), 1);
        assert!(catalog[0].0 > 0, "catalog id must be a counter");
        assert_eq!(
            members[0].5,
            Some(catalog[0].0),
            "origin_catalog_id points at the catalog row whose slug matched"
        );
        assert_eq!(
            members[1].5, None,
            "a member with no matching catalog row gets a NULL origin"
        );

        let active = connection
            .select::<(i64, i64)>("SELECT solution_id, member_id FROM active_member")
            .expect("prepare select active")()
        .expect("select active");
        assert_eq!(active.len(), 1);
        let sawe_member = members
            .iter()
            .find(|m| m.2 == "sawe")
            .expect("sawe member exists");
        assert_eq!(
            active[0],
            (sawe_member.1, sawe_member.0),
            "active_member must be remapped to the numeric member id"
        );

        let legacy = connection
            .select::<(String, i64)>("SELECT old_id, new_id FROM solution_legacy_ids ORDER BY old_id")
            .expect("prepare select legacy map")()
        .expect("select legacy map");
        assert_eq!(legacy.len(), 2, "the cross-DB map must retain every slug");
        assert!(
            legacy.iter().any(|(old, new)| old == "spk-solutions"
                && Some(*new)
                    == solutions
                        .iter()
                        .find(|s| s.1 == "Sawe")
                        .map(|s| s.0)),
            "the map must point the old slug at the new counter: {legacy:?}"
        );
    }

    #[gpui::test]
    async fn identity_migration_records_matching_row_counts() {
        let connection = legacy_seeded_connection("identity_migration_report");
        apply_all_migrations(&connection);

        let report = connection
            .select::<(i64, i64, i64, i64, i64, i64, i64, i64)>(
                "SELECT solutions_before, solutions_after, members_before, members_after,
                        active_before, active_after, catalog_before, catalog_after
                 FROM identity_migration_report",
            )
            .expect("prepare select report")()
        .expect("select report");
        assert_eq!(report.len(), 1, "exactly one report row");
        let r = report[0];
        assert_eq!((r.0, r.1), (2, 2), "solutions before/after");
        assert_eq!((r.2, r.3), (2, 2), "members before/after");
        assert_eq!((r.4, r.5), (1, 1), "active_member before/after");
        assert_eq!((r.6, r.7), (1, 1), "catalog before/after");
    }

    #[gpui::test]
    async fn identity_migration_is_idempotent() {
        let connection = legacy_seeded_connection("identity_migration_idempotent");
        apply_all_migrations(&connection);
        apply_all_migrations(&connection);

        let count = connection
            .select::<i64>("SELECT COUNT(*) FROM solutions")
            .expect("prepare count")()
        .expect("count");
        assert_eq!(count, vec![2], "re-running the migrator must not duplicate rows");
    }
```

- [x] **Step 2: Run the migration tests to verify they fail**

Run: `cargo test -p solutions --lib db::tests::identity_migration -- --nocapture`
Expected: FAIL — `identity_migration_converts_text_ids_to_counters` panics on `select solutions: no such column: last_opened_at` / `no such table: solution_legacy_ids` (only two migrations exist, so nothing was converted).

- [x] **Step 3: Append migration #3 to `MIGRATIONS`**

In `crates/solutions/src/db.rs`, append a third `sql!(…)` entry to `MIGRATIONS` (after the `active_member` migration, never editing the first two). Legacy tables are renamed aside first — SQLite rewrites the children's FK clauses to follow the renamed parent, so the legacy island stays internally consistent while the new tables take over the canonical names:

```rust
        sql!(
            ALTER TABLE solution_members RENAME TO solution_members_legacy;
            ALTER TABLE active_member RENAME TO active_member_legacy;
            ALTER TABLE catalog_projects RENAME TO catalog_projects_legacy;
            ALTER TABLE solutions RENAME TO solutions_legacy;

            CREATE TABLE catalog_projects (
                id             INTEGER PRIMARY KEY,
                name           TEXT NOT NULL,
                remote_url     TEXT NOT NULL,
                default_branch TEXT
            );

            CREATE TABLE solutions (
                id             INTEGER PRIMARY KEY,
                name           TEXT NOT NULL,
                root           TEXT NOT NULL,
                last_opened_at INTEGER
            );

            CREATE TABLE solution_members (
                id                INTEGER PRIMARY KEY,
                solution_id       INTEGER NOT NULL REFERENCES solutions(id) ON DELETE CASCADE,
                name              TEXT    NOT NULL,
                local_path        TEXT    NOT NULL,
                position          INTEGER NOT NULL,
                origin_catalog_id INTEGER REFERENCES catalog_projects(id) ON DELETE SET NULL
            );

            CREATE TABLE active_member (
                solution_id INTEGER PRIMARY KEY REFERENCES solutions(id) ON DELETE CASCADE,
                member_id   INTEGER NOT NULL REFERENCES solution_members(id) ON DELETE CASCADE
            );

            CREATE TABLE catalog_legacy_ids (
                old_id TEXT PRIMARY KEY,
                new_id INTEGER NOT NULL
            );

            CREATE TABLE solution_legacy_ids (
                old_id TEXT PRIMARY KEY,
                new_id INTEGER NOT NULL
            );

            CREATE TABLE member_legacy_ids (
                solution_old_id TEXT NOT NULL,
                catalog_old_id  TEXT NOT NULL,
                new_id          INTEGER NOT NULL,
                PRIMARY KEY (solution_old_id, catalog_old_id)
            );

            INSERT INTO catalog_legacy_ids (old_id, new_id)
                SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM catalog_projects_legacy;

            INSERT INTO solution_legacy_ids (old_id, new_id)
                SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM solutions_legacy;

            INSERT INTO member_legacy_ids (solution_old_id, catalog_old_id, new_id)
                SELECT solution_id, catalog_id,
                       ROW_NUMBER() OVER (ORDER BY solution_id, position, catalog_id)
                FROM solution_members_legacy;

            INSERT INTO catalog_projects (id, name, remote_url, default_branch)
                SELECT map.new_id, c.name, c.remote_url, c.default_branch
                FROM catalog_projects_legacy c
                JOIN catalog_legacy_ids map ON map.old_id = c.id;

            INSERT INTO solutions (id, name, root, last_opened_at)
                SELECT map.new_id, s.name, s.root, s.last_opened_at
                FROM solutions_legacy s
                JOIN solution_legacy_ids map ON map.old_id = s.id;

            INSERT INTO solution_members (id, solution_id, name, local_path, position, origin_catalog_id)
                SELECT mmap.new_id, smap.new_id, m.catalog_id, m.local_path, m.position, cmap.new_id
                FROM solution_members_legacy m
                JOIN member_legacy_ids mmap
                    ON mmap.solution_old_id = m.solution_id AND mmap.catalog_old_id = m.catalog_id
                JOIN solution_legacy_ids smap ON smap.old_id = m.solution_id
                LEFT JOIN catalog_legacy_ids cmap ON cmap.old_id = m.catalog_id;

            INSERT INTO active_member (solution_id, member_id)
                SELECT smap.new_id, mmap.new_id
                FROM active_member_legacy a
                JOIN solution_legacy_ids smap ON smap.old_id = a.solution_id
                JOIN member_legacy_ids mmap
                    ON mmap.solution_old_id = a.solution_id AND mmap.catalog_old_id = a.catalog_id;

            CREATE TABLE identity_migration_report (
                id               INTEGER PRIMARY KEY,
                solutions_before INTEGER NOT NULL,
                solutions_after  INTEGER NOT NULL,
                members_before   INTEGER NOT NULL,
                members_after    INTEGER NOT NULL,
                active_before    INTEGER NOT NULL,
                active_after     INTEGER NOT NULL,
                catalog_before   INTEGER NOT NULL,
                catalog_after    INTEGER NOT NULL
            );

            INSERT INTO identity_migration_report (
                id,
                solutions_before, solutions_after,
                members_before, members_after,
                active_before, active_after,
                catalog_before, catalog_after
            )
            VALUES (
                1,
                (SELECT COUNT(*) FROM solutions_legacy), (SELECT COUNT(*) FROM solutions),
                (SELECT COUNT(*) FROM solution_members_legacy), (SELECT COUNT(*) FROM solution_members),
                (SELECT COUNT(*) FROM active_member_legacy), (SELECT COUNT(*) FROM active_member),
                (SELECT COUNT(*) FROM catalog_projects_legacy), (SELECT COUNT(*) FROM catalog_projects)
            );

            DROP TABLE active_member_legacy;
            DROP TABLE solution_members_legacy;
            DROP TABLE solutions_legacy;
            DROP TABLE catalog_projects_legacy;
            DROP TABLE catalog_legacy_ids;
            DROP TABLE member_legacy_ids;

            CREATE INDEX idx_solution_members_position
                ON solution_members(solution_id, position);
        ),
```

Note for the implementer: `active_member_legacy` rows whose `catalog_id` no longer matches any member row are dropped by the inner `JOIN` — that is intentional (a dangling selection is not data), and it is why `active_before`/`active_after` are compared as counts: if the production DB has such a row, verification fails loudly at startup and the operator sees exactly which table lost a row instead of a silent drop. If that happens on the real DB, fix forward by deleting the dangling `active_member` row *before* upgrading.

`solution_legacy_ids` is deliberately kept forever: it is the only bridge that lets `solution_agent.db` (a separate file, separate connection, no `ATTACH`) remap its `solution_sessions.solution_id` slugs in Task 2.

- [x] **Step 4: Run the migration tests to verify they pass**

Run: `cargo test -p solutions --lib db::tests::identity_migration -- --nocapture`
Expected: PASS (3 tests). The rest of `cargo test -p solutions` still fails — the old `query!` fns bind `String` ids into the new INTEGER columns. That is fixed next.

- [x] **Step 5: Write the failing numeric-DB-API test**

Replace the whole `#[cfg(test)] mod tests` *pre-existing* body in `crates/solutions/src/db.rs` (the `open_test_db_applies_migration` / `catalog_save_and_load_roundtrips` / `solution_with_members_roundtrips` / `solution_with_no_members_still_returned` / `resaving_solution_preserves_members_and_active_member` / `active_member_roundtrips` tests) with the numeric versions below. Keep the three `identity_migration_*` tests and the two helpers from Step 1.

```rust
    #[gpui::test]
    async fn catalog_insert_allocates_counter_ids() {
        let db = SolutionsDb::open_test_db("solutions_db_catalog_counters").await;
        let a = db
            .insert_catalog_project("Alpha".into(), "git@a:a.git".into(), Some("main".into()))
            .await
            .expect("insert alpha");
        let b = db
            .insert_catalog_project("Beta".into(), "git@b:b.git".into(), None)
            .await
            .expect("insert beta");
        assert!(a > 0 && b > a, "ids must be increasing counters: {a} {b}");

        let mut rows = db.load_all_catalog_projects().await.expect("load catalog");
        rows.sort_by_key(|r| r.0);
        assert_eq!(
            rows,
            vec![
                (a, "Alpha".into(), "git@a:a.git".into(), Some("main".into())),
                (b, "Beta".into(), "git@b:b.git".into(), None),
            ]
        );

        db.delete_catalog_project(a).await.expect("delete alpha");
        let rows = db.load_all_catalog_projects().await.expect("reload catalog");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, b);
    }

    #[gpui::test]
    async fn solution_with_members_roundtrips() {
        let db = SolutionsDb::open_test_db("solutions_db_numeric_roundtrip").await;
        let sol = db
            .insert_solution("Alpha".into(), "/tmp/alpha".into(), Some(1_700_000_000_000))
            .await
            .expect("insert solution");
        let cat = db
            .insert_catalog_project("Cat".into(), "git@x:cat.git".into(), None)
            .await
            .expect("insert catalog");
        let m1 = db
            .insert_solution_member(sol, "cat".into(), "/tmp/alpha/cat".into(), 0, Some(cat))
            .await
            .expect("insert member 1");
        let m2 = db
            .insert_solution_member(sol, "empty".into(), "/tmp/alpha/empty".into(), 1, None)
            .await
            .expect("insert member 2");

        let rows = db
            .load_all_solutions_with_members()
            .await
            .expect("load solutions");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().map(|r| r.4).collect::<Vec<_>>(), vec![m1, m2]);
        assert_eq!(rows[0].5, "cat");
        assert_eq!(rows[0].8, Some(cat));
        assert_eq!(rows[1].8, None);

        db.delete_solution_member(m1).await.expect("delete member 1");
        let rows = db
            .load_all_solutions_with_members()
            .await
            .expect("reload solutions");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].4, m2);

        db.delete_solution_row(sol).await.expect("delete solution");
        let rows = db
            .load_all_solutions_with_members()
            .await
            .expect("reload after delete");
        assert!(rows.is_empty());
    }

    #[gpui::test]
    async fn solution_with_no_members_still_returned() {
        let db = SolutionsDb::open_test_db("solutions_db_numeric_empty").await;
        let sol = db
            .insert_solution("E".into(), "/x/empty".into(), None)
            .await
            .expect("insert solution");
        let rows = db
            .load_all_solutions_with_members()
            .await
            .expect("load solutions");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, sol);
        assert_eq!(
            rows[0].4, 0,
            "the LEFT JOIN's NULL member id reads back as 0 — the no-member marker"
        );
    }

    // `rename_solution` re-saves an existing solution row. If save_solution
    // deleted the parent row first (INSERT OR REPLACE), the ON DELETE CASCADE
    // on solution_members / active_member would silently wipe them.
    #[gpui::test]
    async fn resaving_solution_preserves_members_and_active_member() {
        let db = SolutionsDb::open_test_db("solutions_db_numeric_resave").await;
        let sol = db
            .insert_solution("Alpha".into(), "/tmp/alpha".into(), Some(1_700_000_000_000))
            .await
            .expect("insert solution");
        let m1 = db
            .insert_solution_member(sol, "a".into(), "/tmp/alpha/a".into(), 0, None)
            .await
            .expect("insert member 1");
        let m2 = db
            .insert_solution_member(sol, "b".into(), "/tmp/alpha/b".into(), 1, None)
            .await
            .expect("insert member 2");
        db.set_active_member(sol, m2).await.expect("set active");

        db.save_solution(
            sol,
            "Renamed".into(),
            "/tmp/renamed".into(),
            Some(1_700_000_000_000),
        )
        .await
        .expect("re-save solution");

        let rows = db
            .load_all_solutions_with_members()
            .await
            .expect("load solutions");
        assert_eq!(rows.len(), 2, "members must survive a re-save: {rows:?}");
        assert_eq!(rows[0].1, "Renamed");
        assert_eq!(rows[0].2, "/tmp/renamed");
        assert_eq!(rows.iter().map(|r| r.4).collect::<Vec<_>>(), vec![m1, m2]);

        let active = db
            .load_all_active_members()
            .await
            .expect("load active members");
        assert_eq!(
            active,
            vec![(sol, m2)],
            "active member must survive a re-save"
        );
    }

    // `set_solution_member` re-saves a member row (the reorder path). The member
    // table is the FK parent of active_member.member_id — INSERT OR REPLACE here
    // would cascade the active selection away.
    #[gpui::test]
    async fn resaving_member_preserves_active_member() {
        let db = SolutionsDb::open_test_db("solutions_db_numeric_resave_member").await;
        let sol = db
            .insert_solution("Alpha".into(), "/tmp/alpha".into(), None)
            .await
            .expect("insert solution");
        let m = db
            .insert_solution_member(sol, "a".into(), "/tmp/alpha/a".into(), 0, None)
            .await
            .expect("insert member");
        db.set_active_member(sol, m).await.expect("set active");

        db.set_solution_member(m, sol, "a".into(), "/tmp/alpha/a".into(), 3, None)
            .await
            .expect("re-save member");

        let active = db
            .load_all_active_members()
            .await
            .expect("load active members");
        assert_eq!(active, vec![(sol, m)], "active member must survive a member re-save");
    }

    #[gpui::test]
    async fn active_member_roundtrips() {
        let db = SolutionsDb::open_test_db("solutions_db_numeric_active").await;
        let sol = db
            .insert_solution("S1".into(), "/tmp/s1".into(), None)
            .await
            .expect("insert solution");
        let a = db
            .insert_solution_member(sol, "a".into(), "/tmp/s1/a".into(), 0, None)
            .await
            .expect("insert member a");
        let b = db
            .insert_solution_member(sol, "b".into(), "/tmp/s1/b".into(), 1, None)
            .await
            .expect("insert member b");
        db.set_active_member(sol, a).await.expect("set active a");
        db.set_active_member(sol, b).await.expect("set active b");
        let rows = db
            .load_all_active_members()
            .await
            .expect("load active members");
        assert_eq!(rows, vec![(sol, b)]);

        db.clear_active_member(sol).await.expect("clear active");
        let rows = db
            .load_all_active_members()
            .await
            .expect("reload active members");
        assert!(rows.is_empty());
    }
```

- [x] **Step 6: Run the DB API tests to verify they fail**

Run: `cargo test -p solutions --lib db::tests -- --nocapture`
Expected: FAIL to compile — `error[E0599]: no method named 'insert_solution' found for struct 'SolutionsDb'` (and the same for `insert_solution_member` / `insert_catalog_project`), plus mismatched-type errors on the existing `String`-taking fns.

- [x] **Step 7: Rewrite the `impl SolutionsDb` query block**

Replace the whole `impl SolutionsDb { … }` block in `crates/solutions/src/db.rs` (everything between `use db::query;` and `#[cfg(test)] mod tests`) with:

```rust
use anyhow::Context as _;
use db::query;

impl SolutionsDb {
    /// Insert a new catalog project and return its freshly-allocated counter id.
    pub async fn insert_catalog_project(
        &self,
        name: String,
        remote_url: String,
        default_branch: Option<String>,
    ) -> anyhow::Result<i64> {
        self.write(move |connection| {
            connection
                .select_row_bound::<(String, String, Option<String>), i64>(sql!(
                    INSERT INTO catalog_projects (name, remote_url, default_branch)
                    VALUES (?1, ?2, ?3)
                    RETURNING id
                ))?((name, remote_url, default_branch))?
                .context("insert_catalog_project: RETURNING id produced no row")
        })
        .await
    }

    // Must be a real UPSERT, not INSERT OR REPLACE: `catalog_projects` is the
    // parent of `solution_members.origin_catalog_id`. REPLACE deletes the
    // existing parent row before re-inserting it, which would null out every
    // member's provenance on a plain catalog edit.
    query! {
        pub async fn save_catalog_project(
            id: i64,
            name: String,
            remote_url: String,
            default_branch: Option<String>
        ) -> Result<()> {
            INSERT INTO catalog_projects (id, name, remote_url, default_branch)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                remote_url = excluded.remote_url,
                default_branch = excluded.default_branch
        }
    }

    query! {
        pub async fn delete_catalog_project(id: i64) -> Result<()> {
            DELETE FROM catalog_projects WHERE id = ?
        }
    }

    query! {
        pub async fn load_all_catalog_projects()
            -> Result<Vec<(i64, String, String, Option<String>)>>
        {
            SELECT id, name, remote_url, default_branch FROM catalog_projects
        }
    }

    /// Insert a new solution and return its freshly-allocated counter id.
    pub async fn insert_solution(
        &self,
        name: String,
        root: String,
        last_opened_at: Option<i64>,
    ) -> anyhow::Result<i64> {
        self.write(move |connection| {
            connection
                .select_row_bound::<(String, String, Option<i64>), i64>(sql!(
                    INSERT INTO solutions (name, root, last_opened_at)
                    VALUES (?1, ?2, ?3)
                    RETURNING id
                ))?((name, root, last_opened_at))?
                .context("insert_solution: RETURNING id produced no row")
        })
        .await
    }

    // Must be a real UPSERT, not INSERT OR REPLACE: `solutions` is the parent of
    // `solution_members` and `active_member`, both ON DELETE CASCADE. REPLACE
    // deletes the existing parent row before re-inserting it, so re-saving an
    // existing solution (the rename path) would cascade-delete all of its
    // members and its active member.
    query! {
        pub async fn save_solution(
            id: i64,
            name: String,
            root: String,
            last_opened_at: Option<i64>
        ) -> Result<()> {
            INSERT INTO solutions (id, name, root, last_opened_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                root = excluded.root,
                last_opened_at = excluded.last_opened_at
        }
    }

    query! {
        pub async fn delete_solution_row(id: i64) -> Result<()> {
            DELETE FROM solutions WHERE id = ?
        }
    }

    query! {
        pub async fn update_last_opened(id: i64, last_opened_at: i64) -> Result<()> {
            UPDATE solutions SET last_opened_at = ?2 WHERE id = ?1
        }
    }

    /// Insert a new member and return its freshly-allocated counter id.
    pub async fn insert_solution_member(
        &self,
        solution_id: i64,
        name: String,
        local_path: String,
        position: i32,
        origin_catalog_id: Option<i64>,
    ) -> anyhow::Result<i64> {
        self.write(move |connection| {
            connection
                .select_row_bound::<(i64, String, String, i32, Option<i64>), i64>(sql!(
                    INSERT INTO solution_members
                        (solution_id, name, local_path, position, origin_catalog_id)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    RETURNING id
                ))?((solution_id, name, local_path, position, origin_catalog_id))?
                .context("insert_solution_member: RETURNING id produced no row")
        })
        .await
    }

    // Must be a real UPSERT, not INSERT OR REPLACE: `solution_members` is the
    // parent of `active_member.member_id` ON DELETE CASCADE, so REPLACE on the
    // reorder path would drop the solution's active-member selection.
    query! {
        pub async fn set_solution_member(
            id: i64,
            solution_id: i64,
            name: String,
            local_path: String,
            position: i32,
            origin_catalog_id: Option<i64>
        ) -> Result<()> {
            INSERT INTO solution_members
                (id, solution_id, name, local_path, position, origin_catalog_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(id) DO UPDATE SET
                solution_id = excluded.solution_id,
                name = excluded.name,
                local_path = excluded.local_path,
                position = excluded.position,
                origin_catalog_id = excluded.origin_catalog_id
        }
    }

    query! {
        pub async fn delete_solution_member(id: i64) -> Result<()> {
            DELETE FROM solution_members WHERE id = ?
        }
    }

    // LEFT JOIN keeps solutions with zero members in the result. NULL member
    // columns are read as 0 / empty strings by sqlez's i64/String Column impls
    // (column_int returns 0 on NULL, column_text returns ""). Counter ids start
    // at 1, so `member_id == 0` is the unambiguous "no member row" marker the
    // caller groups on.
    query! {
        pub async fn load_all_solutions_with_members()
            -> Result<Vec<(
                i64,
                String,
                String,
                Option<i64>,
                i64,
                String,
                String,
                i32,
                Option<i64>
            )>>
        {
            SELECT s.id, s.name, s.root, s.last_opened_at,
                   m.id, m.name, m.local_path, m.position, m.origin_catalog_id
            FROM solutions s
            LEFT JOIN solution_members m ON m.solution_id = s.id
            ORDER BY s.id, m.position
        }
    }

    query! {
        pub async fn set_active_member(solution_id: i64, member_id: i64) -> Result<()> {
            INSERT INTO active_member (solution_id, member_id)
            VALUES (?1, ?2)
            ON CONFLICT(solution_id) DO UPDATE SET
                member_id = excluded.member_id
        }
    }

    query! {
        pub async fn load_all_active_members() -> Result<Vec<(i64, i64)>> {
            SELECT solution_id, member_id FROM active_member
        }
    }

    query! {
        pub async fn clear_active_member(solution_id: i64) -> Result<()> {
            DELETE FROM active_member WHERE solution_id = ?
        }
    }

    /// Old TEXT slug → new counter id, written once by the identity migration.
    /// The bridge that lets `solution_agent.db` (a separate connection with no
    /// visibility into this file) remap its own `solution_id` column.
    query! {
        pub async fn load_solution_legacy_ids() -> Result<Vec<(String, i64)>> {
            SELECT old_id, new_id FROM solution_legacy_ids
        }
    }

    /// Row counts captured inside the identity migration's transaction.
    /// `None` on a DB that was created fresh at the new schema.
    query! {
        pub async fn load_identity_migration_report()
            -> Result<Option<(i64, i64, i64, i64, i64, i64, i64, i64)>>
        {
            SELECT solutions_before, solutions_after,
                   members_before, members_after,
                   active_before, active_after,
                   catalog_before, catalog_after
            FROM identity_migration_report WHERE id = 1
        }
    }
}
```

- [x] **Step 8: Flip the identity types in `model.rs`**

Replace the whole non-test body of `crates/solutions/src/model.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Surrogate counter ids. They carry no meaning, are never derived from a name,
/// and never change — which is what makes rename cheap: the per-solution MCP
/// socket dir and every FK stay put across a rename.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CatalogId(pub i64);

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SolutionId(pub i64);

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemberId(pub i64);

impl std::fmt::Display for CatalogId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for SolutionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for MemberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogProject {
    pub id: CatalogId,
    pub name: String,
    pub remote_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

/// A project inside a Solution. Independent of the catalog entry it was
/// instantiated from: `origin_catalog_id` records provenance and nothing
/// depends on it — editing or deleting the template never touches the member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolutionMember {
    pub id: MemberId,
    pub name: String,
    pub local_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_catalog_id: Option<CatalogId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Solution {
    pub id: SolutionId,
    pub name: String,
    pub root: PathBuf,
    #[serde(default)]
    pub members: Vec<SolutionMember>,
    /// Epoch millis. Was `DateTime<Utc>`; the DB column is INTEGER and every
    /// consumer that needs a formatted timestamp converts at its own edge with
    /// `chrono::DateTime::<chrono::Utc>::from_timestamp_millis`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_opened_at: Option<i64>,
}

impl Solution {
    pub fn first_member(&self) -> Option<&SolutionMember> {
        self.members.first()
    }

    pub fn member(&self, id: MemberId) -> Option<&SolutionMember> {
        self.members.iter().find(|m| m.id == id)
    }

    /// The member whose `local_path` is `path` or an ancestor of it. Used by the
    /// session/tab binding backfill to place a cwd inside a project.
    pub fn member_for_path(&self, path: &std::path::Path) -> Option<&SolutionMember> {
        self.members
            .iter()
            .filter(|m| path.starts_with(&m.local_path))
            .max_by_key(|m| m.local_path.as_os_str().len())
    }
}
```

Replace `model.rs`'s `mod tests` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solution_member_carries_local_path() {
        let m = SolutionMember {
            id: MemberId(7),
            name: "foo".into(),
            local_path: PathBuf::from("/tmp/foo"),
            origin_catalog_id: Some(CatalogId(3)),
        };
        assert_eq!(m.local_path, PathBuf::from("/tmp/foo"));
        assert_eq!(m.id, MemberId(7));
    }

    #[test]
    fn solution_first_member_returns_none_when_empty() {
        let s = Solution {
            id: SolutionId(1),
            name: "A".into(),
            root: PathBuf::from("/x"),
            members: vec![],
            last_opened_at: None,
        };
        assert!(s.first_member().is_none());
        assert!(s.member_for_path(std::path::Path::new("/x/foo")).is_none());
    }

    #[test]
    fn member_for_path_picks_the_longest_matching_member() {
        let s = Solution {
            id: SolutionId(1),
            name: "A".into(),
            root: PathBuf::from("/x"),
            members: vec![
                SolutionMember {
                    id: MemberId(1),
                    name: "foo".into(),
                    local_path: "/x/foo".into(),
                    origin_catalog_id: None,
                },
                SolutionMember {
                    id: MemberId(2),
                    name: "foo-nested".into(),
                    local_path: "/x/foo/nested".into(),
                    origin_catalog_id: None,
                },
            ],
        last_opened_at: None,
        };
        assert_eq!(
            s.member_for_path(std::path::Path::new("/x/foo/nested/src/a.rs"))
                .map(|m| m.id),
            Some(MemberId(2))
        );
        assert_eq!(
            s.member_for_path(std::path::Path::new("/x/foo/src/a.rs"))
                .map(|m| m.id),
            Some(MemberId(1))
        );
        assert_eq!(s.member_for_path(std::path::Path::new("/x")), None);
    }
}
```

Then export the new type from `crates/solutions/src/solutions.rs`:

```rust
pub use model::{CatalogId, CatalogProject, MemberId, Solution, SolutionId, SolutionMember};
```

- [x] **Step 9: Rewrite the store's hydration and caches**

In `crates/solutions/src/store.rs`:

Change the imports and the two id-keyed fields:

```rust
use crate::model::{CatalogId, CatalogProject, MemberId, Solution, SolutionId, SolutionMember};
```

```rust
    pub(crate) in_flight_adds: HashMap<(SolutionId, CatalogId), InFlightAdd>,
    /// Solution-wide active member selection. Hydrated from the `active_member`
    /// DB table at init and updated through `set_active_member`.
    pub(crate) active_member: HashMap<SolutionId, MemberId>,
```

Replace `init_with_db`'s active-member hydration and `load_from_db_blocking` with:

```rust
    fn init_with_db(db: SolutionsDb, cx: &mut App) {
        let json_path = paths::config_dir().join("solutions.json");
        if let Err(err) = crate::migrate::run_one_time_migration(&db, &json_path) {
            log::error!("solutions::store: legacy import failed: {err}. Continuing with empty DB.");
        }
        if let Err(err) = crate::migrate::verify_identity_migration(&db) {
            log::error!("solutions::store: IDENTITY MIGRATION VERIFICATION FAILED: {err}");
        }
        let config = match Self::load_from_db_blocking(&db) {
            Ok(cfg) => cfg,
            Err(err) => {
                log::error!("solutions::store: failed to hydrate from DB: {err}");
                SolutionsConfig {
                    version: CURRENT_VERSION,
                    ..Default::default()
                }
            }
        };
        let active_member_rows = match gpui::block_on(db.load_all_active_members()) {
            Ok(rows) => rows,
            Err(err) => {
                log::error!("solutions::store: failed to load active_member: {err}");
                Vec::new()
            }
        };
        let mut active_member: HashMap<SolutionId, MemberId> = HashMap::default();
        for (sid, mid) in active_member_rows {
            active_member.insert(SolutionId(sid), MemberId(mid));
        }
        let store = cx.new(|_| SolutionStore {
            config,
            db: Some(db),
            fs_lock: Arc::new(smol::lock::Mutex::new(())),
            in_flight_adds: HashMap::default(),
            tab_snapshots: TabSnapshots::default(),
            active_member,
            open_solutions: HashSet::default(),
        });
        cx.set_global(GlobalSolutionStore(store));
    }

    fn load_from_db_blocking(db: &SolutionsDb) -> anyhow::Result<SolutionsConfig> {
        let catalog_rows = gpui::block_on(db.load_all_catalog_projects())?;
        let catalog: Vec<CatalogProject> = catalog_rows
            .into_iter()
            .map(|(id, name, remote_url, default_branch)| CatalogProject {
                id: CatalogId(id),
                name,
                remote_url,
                default_branch,
            })
            .collect();

        let solution_rows = gpui::block_on(db.load_all_solutions_with_members())?;
        let mut by_id: collections::HashMap<i64, Solution> = collections::HashMap::default();
        let mut order: Vec<i64> = Vec::new();
        for (
            sid,
            sname,
            sroot,
            last_opened_at,
            member_id,
            member_name,
            local_path,
            _position,
            origin_catalog_id,
        ) in solution_rows
        {
            let entry = by_id.entry(sid).or_insert_with(|| {
                order.push(sid);
                Solution {
                    id: SolutionId(sid),
                    name: sname,
                    root: PathBuf::from(sroot),
                    members: vec![],
                    last_opened_at,
                }
            });
            // The LEFT JOIN yields a NULL member id for a memberless solution;
            // sqlez reads that back as 0, and counter ids start at 1.
            if member_id != 0 {
                entry.members.push(SolutionMember {
                    id: MemberId(member_id),
                    name: member_name,
                    local_path: PathBuf::from(local_path),
                    origin_catalog_id: origin_catalog_id.map(CatalogId),
                });
            }
        }
        let solutions: Vec<Solution> = order.into_iter().filter_map(|k| by_id.remove(&k)).collect();

        Ok(SolutionsConfig {
            version: CURRENT_VERSION,
            catalog,
            solutions,
        })
    }
```

Replace the `#[cfg(any(test, feature = "test-support"))]` helpers at the bottom of the `impl SolutionStore` block (`create_for_test_minimal`, `test_force_add_catalog`, `test_force_add_member`, `test_add_member_with_path`) with counter-allocating versions:

```rust
    /// Monotonic id source for `for_test` stores that have no DB to allocate
    /// counters. Shared across the three test helpers so a test can't mint two
    /// entities with the same id.
    #[cfg(any(test, feature = "test-support"))]
    fn next_test_id(&self) -> i64 {
        let max_solution = self.config.solutions.iter().map(|s| s.id.0).max().unwrap_or(0);
        let max_member = self
            .config
            .solutions
            .iter()
            .flat_map(|s| s.members.iter().map(|m| m.id.0))
            .max()
            .unwrap_or(0);
        let max_catalog = self.config.catalog.iter().map(|c| c.id.0).max().unwrap_or(0);
        max_solution.max(max_member).max(max_catalog) + 1
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn create_for_test_minimal(&mut self, name: &str, cx: &mut Context<Self>) -> SolutionId {
        let id = SolutionId(self.next_test_id());
        let root = std::env::temp_dir()
            .join("spke-test-solutions")
            .join(crate::slug::slugify(name));
        self.config.solutions.push(Solution {
            id,
            name: name.into(),
            root,
            members: vec![],
            last_opened_at: None,
        });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        id
    }

    /// Push a catalog row bypassing the uniqueness checks — the only way to
    /// reproduce the duplicate rows that predate them (and that
    /// `merge_catalog_project` exists to clean up).
    #[cfg(test)]
    pub fn test_force_add_catalog(&mut self, name: &str, remote_url: &str) -> CatalogId {
        let id = CatalogId(self.next_test_id());
        self.config.catalog.push(CatalogProject {
            id,
            name: name.into(),
            remote_url: remote_url.into(),
            default_branch: None,
        });
        id
    }

    #[cfg(test)]
    pub fn test_force_add_member(&mut self, sid: SolutionId, cid: CatalogId) -> MemberId {
        let member_id = MemberId(self.next_test_id());
        let name = self
            .config
            .catalog
            .iter()
            .find(|c| c.id == cid)
            .map(|c| crate::slug::slugify(&c.name))
            .unwrap_or_else(|| format!("member-{}", member_id.0));
        let sol = self
            .config
            .solutions
            .iter_mut()
            .find(|s| s.id == sid)
            .expect("test_force_add_member: solution not found");
        let local_path = sol.root.join(&name);
        sol.members.push(SolutionMember {
            id: member_id,
            name,
            local_path,
            origin_catalog_id: Some(cid),
        });
        member_id
    }

    /// Push a member with an explicit `local_path`, bypassing catalog resolution
    /// and DB writes. Lets downstream crates (e.g. `solution_agent`) set up
    /// member directories for orphan-GC tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_add_member_with_path(
        &mut self,
        sid: SolutionId,
        name: &str,
        local_path: PathBuf,
    ) -> MemberId {
        let member_id = MemberId(self.next_test_id());
        let sol = self
            .config
            .solutions
            .iter_mut()
            .find(|s| s.id == sid)
            .expect("test_add_member_with_path: solution not found");
        sol.members.push(SolutionMember {
            id: member_id,
            name: name.into(),
            local_path,
            origin_catalog_id: None,
        });
        member_id
    }
```

Also update `refresh_active_solution_for_branch_protection` — `max_by_key(|s| s.last_opened_at)` still compiles (`Option<i64>: Ord`), no change needed there.

- [x] **Step 10: Rewrite `store/lifecycle.rs`**

`create_solution` now allocates the id from the DB (or from `next_test_id` when there is no DB), and the root folder name comes from the slug of the display name (unchanged behaviour — Phase 2 replaces `slugify` with the Unicode-preserving derivation):

```rust
use super::{SolutionStore, SolutionStoreEvent};
use crate::model::{Solution, SolutionId};
use crate::slug::{slugify, unique_slug};
use anyhow::{Context as _, Result, bail};
use gpui::Context;
use std::path::PathBuf;

impl SolutionStore {
    pub fn create_solution(
        &mut self,
        name: &str,
        root_base: PathBuf,
        cx: &mut gpui::Context<Self>,
    ) -> Result<SolutionId> {
        let taken: Vec<String> = self
            .config
            .solutions
            .iter()
            .filter_map(|s| {
                s.root
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
            })
            .collect();
        let folder = unique_slug(name, &taken);
        let root = root_base.join(&folder);
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

        let id = match self.db.as_ref() {
            Some(db) => SolutionId(gpui::block_on(db.insert_solution(
                name.to_string(),
                root.to_string_lossy().into_owned(),
                None,
            ))?),
            None => SolutionId(self.next_id_without_db()),
        };
        self.config.solutions.push(Solution {
            id,
            name: name.into(),
            root,
            members: vec![],
            last_opened_at: None,
        });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(id)
    }

    pub fn rename_solution(
        &mut self,
        id: SolutionId,
        new_name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let sol = self.find_solution_mut(id)?;
        sol.name = new_name.into();
        let updated = sol.clone();
        self.db_save_solution(&updated)?;
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    pub fn delete_solution(&mut self, id: SolutionId, cx: &mut gpui::Context<Self>) -> Result<()> {
        // Capture the root before removal so the `Deleted` event can carry it —
        // subscribers can no longer look the solution up by id once it's gone.
        let root = self
            .config
            .solutions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.root.clone());
        let before = self.config.solutions.len();
        self.config.solutions.retain(|s| s.id != id);
        if self.config.solutions.len() == before {
            bail!("solution not found: {id}");
        }
        self.db_delete_solution(id)?;
        self.active_member.remove(&id);
        let seq_opt = editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
            .map(|c| c.next_seq());
        if let Some(seq) = seq_opt {
            editor_mcp::emit_notification(
                cx,
                "workspace.solution_deleted",
                serde_json::json!({
                    "seq": seq,
                    "solution_id": id.0,
                }),
            );
        }
        cx.emit(SolutionStoreEvent::Changed);
        if let Some(root) = root {
            cx.emit(SolutionStoreEvent::Deleted { id, root });
        }
        cx.notify();
        Ok(())
    }

    pub fn touch_last_opened(&mut self, id: SolutionId, cx: &mut gpui::Context<Self>) -> Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let sol = self.find_solution_mut(id)?;
        sol.last_opened_at = Some(now_ms);
        self.db_update_last_opened(id, now_ms)?;
        cx.emit(SolutionStoreEvent::Changed);
        cx.emit(SolutionStoreEvent::ActiveSolutionChanged(id));
        cx.notify();
        Ok(())
    }

    /// Returns `true` if the solution's desktop window is currently tracked as open.
    pub fn is_open(&self, id: SolutionId) -> bool {
        self.open_solutions.contains(&id)
    }

    pub fn mark_open(&mut self, id: SolutionId, cx: &mut Context<Self>) {
        if !self.open_solutions.insert(id) {
            return;
        }
        let solution_json = self
            .config
            .solutions
            .iter()
            .find(|s| s.id == id)
            .map(|sol| {
                serde_json::json!({
                    "id": sol.id.0,
                    "name": sol.name,
                    "root": sol.root.to_string_lossy(),
                    "member_count": sol.members.len(),
                    "last_opened_at": sol
                        .last_opened_at
                        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                        .map(|t| t.to_rfc3339()),
                    "open": true,
                    "main_window_id": serde_json::Value::Null,
                })
            });
        let seq_opt = editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
            .map(|c| c.next_seq());
        if let Some(seq) = seq_opt {
            editor_mcp::emit_notification(
                cx,
                "workspace.solution_opened",
                serde_json::json!({
                    "seq": seq,
                    "solution": solution_json,
                    "sessions": [],
                }),
            );
        }
        cx.emit(SolutionStoreEvent::Opened { id });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
    }

    pub fn mark_closed(&mut self, id: SolutionId, cx: &mut Context<Self>) {
        if !self.open_solutions.remove(&id) {
            return;
        }
        let seq_opt = editor_mcp::workspace_seq::WorkspaceEventCoordinator::try_global(cx)
            .map(|c| c.next_seq());
        if let Some(seq) = seq_opt {
            editor_mcp::emit_notification(
                cx,
                "workspace.solution_closed",
                serde_json::json!({
                    "seq": seq,
                    "solution_id": id.0,
                }),
            );
        }
        cx.emit(SolutionStoreEvent::Closed { id });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
    }

    pub fn paths_for_open(&self, id: SolutionId) -> Result<Vec<PathBuf>> {
        let sol = self.find_solution(id)?;
        Ok(sol.members.iter().map(|m| m.local_path.clone()).collect())
    }

    /// Id allocation for `for_test` stores with no DB. Production stores always
    /// go through `INSERT … RETURNING id`.
    pub(crate) fn next_id_without_db(&self) -> i64 {
        let max_solution = self.config.solutions.iter().map(|s| s.id.0).max().unwrap_or(0);
        let max_member = self
            .config
            .solutions
            .iter()
            .flat_map(|s| s.members.iter().map(|m| m.id.0))
            .max()
            .unwrap_or(0);
        let max_catalog = self.config.catalog.iter().map(|c| c.id.0).max().unwrap_or(0);
        max_solution.max(max_member).max(max_catalog) + 1
    }

    fn db_save_solution(&self, s: &Solution) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.save_solution(
            s.id.0,
            s.name.clone(),
            s.root.to_string_lossy().into_owned(),
            s.last_opened_at,
        ))
    }

    fn db_delete_solution(&self, id: SolutionId) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.delete_solution_row(id.0))
    }

    fn db_update_last_opened(&self, id: SolutionId, ts_ms: i64) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.update_last_opened(id.0, ts_ms))
    }
}
```

Delete `next_test_id` from Step 9 and have the three test helpers call `self.next_id_without_db()` instead (one allocator, not two). `slugify` is imported for `create_for_test_minimal`'s root path.

- [x] **Step 11: Rewrite `store/members.rs` around `MemberId`**

```rust
use super::{SolutionStore, SolutionStoreEvent};
use crate::model::{MemberId, Solution, SolutionId, SolutionMember};
use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext as _, Context, Entity};
use project::WorktreeId;
use util::ResultExt as _;

impl SolutionStore {
    pub fn find_solution(&self, id: SolutionId) -> Result<&Solution> {
        self.config
            .solutions
            .iter()
            .find(|s| s.id == id)
            .with_context(|| format!("solution not found: {id}"))
    }

    pub fn find_member(&self, id: MemberId) -> Result<&SolutionMember> {
        self.config
            .solutions
            .iter()
            .flat_map(|s| s.members.iter())
            .find(|m| m.id == id)
            .with_context(|| format!("member not found: {id}"))
    }

    /// The solution a member belongs to.
    pub fn member_of(&self, id: MemberId) -> Option<SolutionId> {
        self.config
            .solutions
            .iter()
            .find(|s| s.members.iter().any(|m| m.id == id))
            .map(|s| s.id)
    }

    pub fn active_member(&self, solution: SolutionId) -> Option<MemberId> {
        self.active_member.get(&solution).copied()
    }

    /// Path of the solution's active member, falling back to the solution root
    /// when no member is selected. The one place that answers "where do new
    /// terminals / chats start".
    pub fn active_member_path(&self, solution: SolutionId) -> Option<std::path::PathBuf> {
        let sol = self.find_solution(solution).ok()?;
        if let Some(member) = self
            .active_member(solution)
            .and_then(|id| sol.member(id))
        {
            return Some(member.local_path.clone());
        }
        Some(sol.root.clone())
    }

    pub fn set_active_member(
        &mut self,
        solution: SolutionId,
        member: MemberId,
        cx: &mut Context<Self>,
    ) {
        if self.active_member.get(&solution) == Some(&member) {
            return;
        }
        self.active_member.insert(solution, member);
        if let Some(db) = self.db.clone() {
            let (sid, mid) = (solution.0, member.0);
            cx.background_spawn(async move {
                db.set_active_member(sid, mid).await.log_err();
            })
            .detach();
        }
        cx.emit(SolutionStoreEvent::ActiveMemberChanged {
            solution,
            member: Some(member),
        });
        cx.notify();
    }

    pub fn ensure_active_member(
        &mut self,
        solution: SolutionId,
        members: &[SolutionMember],
        cx: &mut Context<Self>,
    ) -> Option<MemberId> {
        if let Some(existing) = self.active_member.get(&solution).copied()
            && members.iter().any(|m| m.id == existing)
        {
            return Some(existing);
        }
        let first = members.first()?.id;
        self.set_active_member(solution, first, cx);
        Some(first)
    }

    pub(crate) fn seed_active_member_if_unset(
        &mut self,
        solution: SolutionId,
        cx: &mut Context<Self>,
    ) {
        if self.active_member.contains_key(&solution) {
            return;
        }
        let first = self
            .find_solution(solution)
            .ok()
            .and_then(|s| s.members.first())
            .map(|m| m.id);
        if let Some(first) = first {
            self.set_active_member(solution, first, cx);
        }
    }

    /// Resolve the active member for `solution` to the matching worktree in
    /// `project`, using prefix matching on `member.local_path`.
    pub fn active_member_worktree(
        &self,
        solution: &Solution,
        project: &Entity<project::Project>,
        cx: &App,
    ) -> Option<(MemberId, WorktreeId)> {
        let member_id = self.active_member(solution.id)?;
        let member = solution.member(member_id)?;
        let worktree = project
            .read(cx)
            .worktrees(cx)
            .find(|w| w.read(cx).abs_path().starts_with(&member.local_path))?;
        Some((member_id, worktree.read(cx).id()))
    }

    pub fn remove_member(&mut self, member_id: MemberId, cx: &mut gpui::Context<Self>) -> Result<()> {
        let Some(solution_id) = self.member_of(member_id) else {
            bail!("member not in any solution: {member_id}");
        };
        let sol = self.find_solution_mut(solution_id)?;
        sol.members.retain(|m| m.id != member_id);
        self.db_delete_member(member_id)?;
        // If the removed member was the active one, repoint to a remaining
        // member or clear the selection. Member-scoped panels rebuild on
        // `ActiveMemberChanged` (not `Changed`), so the `None` case is what
        // takes the just-removed project's tree off screen.
        if self.active_member.get(&solution_id) == Some(&member_id) {
            let next = self
                .find_solution(solution_id)
                .ok()
                .and_then(|s| s.members.first())
                .map(|m| m.id);
            match next {
                Some(next) => self.set_active_member(solution_id, next, cx),
                None => {
                    self.active_member.remove(&solution_id);
                    if let Some(db) = self.db.clone() {
                        let sid = solution_id.0;
                        cx.background_spawn(async move {
                            db.clear_active_member(sid).await.log_err();
                        })
                        .detach();
                    }
                    cx.emit(SolutionStoreEvent::ActiveMemberChanged {
                        solution: solution_id,
                        member: None,
                    });
                }
            }
        }
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    pub fn reorder_members(
        &mut self,
        solution_id: SolutionId,
        new_order: Vec<MemberId>,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let sol = self.find_solution_mut(solution_id)?;
        let mut by_id: collections::HashMap<MemberId, SolutionMember> = sol
            .members
            .drain(..)
            .map(|m| (m.id, m))
            .collect();
        for id in &new_order {
            if let Some(m) = by_id.remove(id) {
                sol.members.push(m);
            }
        }
        let mut leftovers: Vec<SolutionMember> = by_id.into_values().collect();
        leftovers.sort_by_key(|m| m.id);
        sol.members.append(&mut leftovers);
        let snapshot: Vec<SolutionMember> = sol.members.clone();
        for (i, m) in snapshot.iter().enumerate() {
            self.db_set_member(solution_id, m, i as i32)?;
        }
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    pub(crate) fn db_set_member(
        &self,
        sol_id: SolutionId,
        m: &SolutionMember,
        position: i32,
    ) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.set_solution_member(
            m.id.0,
            sol_id.0,
            m.name.clone(),
            m.local_path.to_string_lossy().into_owned(),
            position,
            m.origin_catalog_id.map(|c| c.0),
        ))
    }

    pub(crate) fn db_delete_member(&self, member_id: MemberId) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.delete_solution_member(member_id.0))
    }

    pub(crate) fn find_solution_mut(&mut self, id: SolutionId) -> Result<&mut Solution> {
        self.config
            .solutions
            .iter_mut()
            .find(|s| s.id == id)
            .with_context(|| format!("solution not found: {id}"))
    }
}
```

In `store.rs`, rename the event payload field to match (`SolutionStoreEvent::ActiveMemberChanged { solution: SolutionId, member: Option<MemberId> }`), and change `MemberAddProgress` / `MemberAddCompleted` / `Deleted` / `Closed` / `Opened` to carry `SolutionId` / `CatalogId` by value (they are `Copy` now):

```rust
    ActiveSolutionChanged(SolutionId),
    MemberAddProgress {
        solution: SolutionId,
        catalog: CatalogId,
        stage: String,
        percent: Option<u8>,
    },
    MemberAddCompleted {
        solution: SolutionId,
        catalog: CatalogId,
        /// `None` on success; `Some(msg)` on failure or cancellation.
        error: Option<String>,
    },
    ActiveMemberChanged {
        solution: SolutionId,
        member: Option<MemberId>,
    },
    Deleted {
        id: SolutionId,
        root: std::path::PathBuf,
    },
    Closed {
        id: SolutionId,
    },
    Opened {
        id: SolutionId,
    },
```

Also update `SolutionStore::tab_snapshot(&self, id: SolutionId)` / `store_tab_snapshot(&mut self, id: SolutionId, …)` and `solution_for_path` (unchanged body).

- [x] **Step 12: Convert `store/catalog.rs`**

Mechanical, driven by the compiler. The three load-bearing changes:

- `add_catalog_project` allocates through the DB instead of slugging:

```rust
    pub fn add_catalog_project(
        &mut self,
        name: &str,
        remote_url: &str,
        default_branch: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) -> Result<CatalogId> {
        let name_taken = self
            .config
            .catalog
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(name));
        if name_taken {
            bail!("duplicate_name: a catalog project named {name} already exists");
        }
        let normalized = normalize_remote(remote_url);
        if self
            .config
            .catalog
            .iter()
            .any(|c| normalize_remote(&c.remote_url) == normalized)
        {
            bail!("duplicate_remote: {remote_url} is already in the catalog");
        }
        let id = match self.db.as_ref() {
            Some(db) => CatalogId(gpui::block_on(db.insert_catalog_project(
                name.to_string(),
                remote_url.to_string(),
                default_branch.clone(),
            ))?),
            None => CatalogId(self.next_id_without_db()),
        };
        self.config.catalog.push(CatalogProject {
            id,
            name: name.into(),
            remote_url: remote_url.into(),
            default_branch,
        });
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(id)
    }
```

(Keep whatever duplicate-name / duplicate-remote helper the file already has — `normalize_remote` above stands for the existing helper; do not introduce a second one.)

- `merge_catalog_project(&mut self, duplicate: CatalogId, canonical: CatalogId, …)` repoints `member.origin_catalog_id` instead of `member.catalog_id`:

```rust
        for sol in self.config.solutions.iter_mut() {
            for member in sol.members.iter_mut() {
                if member.origin_catalog_id == Some(duplicate) {
                    member.origin_catalog_id = Some(canonical);
                    repointed += 1;
                }
            }
        }
```

- `remove_catalog_project` / `remove_catalog_project_cascade` / `solutions_referencing` match on `m.origin_catalog_id == Some(id)`.

Note the semantic shift the spec mandates: `origin_catalog_id` is provenance only, so `remove_catalog_project` no longer needs to refuse when members reference it. Keep the existing refusal for now (Phase 1 is "no behavior change"); the FK is `ON DELETE SET NULL`, so a future phase can drop the guard without orphaning anything.

- [x] **Step 13: Convert `add_member.rs`**

Three changes:

- The clone target folder is derived from the catalog project's **name** (previously the catalog id, which was the slug of the name — same string, different source):

```rust
        let folder = crate::slug::slugify(&cat.name);
        let target = sol.root.join(&folder);
```

- On success, the member row is inserted through the DB so it gets a counter id, and the in-memory member carries it:

```rust
                                    let member_id = match store.db.as_ref() {
                                        Some(db) => MemberId(gpui::block_on(
                                            db.insert_solution_member(
                                                solution_id.0,
                                                folder.clone(),
                                                target.to_string_lossy().into_owned(),
                                                position,
                                                Some(catalog_id.0),
                                            ),
                                        )?),
                                        None => MemberId(store.next_id_without_db()),
                                    };
                                    let member = SolutionMember {
                                        id: member_id,
                                        name: folder.clone(),
                                        local_path: target.clone(),
                                        origin_catalog_id: Some(catalog_id),
                                    };
                                    sol.members.push(member);
```

  (`position` = `sol.members.len() as i32` computed before the push. The old `db_set_member` call on this path goes away — `insert_solution_member` already wrote the row.)

- `add_empty_member` returns a `MemberId` and no longer mints a `CatalogId`:

```rust
    pub fn add_empty_member(
        &mut self,
        solution_id: SolutionId,
        name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<MemberId> {
        let sol = self.find_solution(solution_id)?;
        let taken: Vec<String> = sol.members.iter().map(|m| m.name.clone()).collect();
        let folder = crate::slug::unique_slug(name, &taken);
        let local_path = sol.root.join(&folder);
        let position = sol.members.len() as i32;
        std::fs::create_dir_all(&local_path)
            .with_context(|| format!("creating {}", local_path.display()))?;
        init_empty_git_repo(&local_path).log_err();

        let member_id = match self.db.as_ref() {
            Some(db) => MemberId(gpui::block_on(db.insert_solution_member(
                solution_id.0,
                folder.clone(),
                local_path.to_string_lossy().into_owned(),
                position,
                None,
            ))?),
            None => MemberId(self.next_id_without_db()),
        };
        let sol = self.find_solution_mut(solution_id)?;
        sol.members.push(SolutionMember {
            id: member_id,
            name: folder,
            local_path,
            origin_catalog_id: None,
        });
        self.seed_active_member_if_unset(solution_id, cx);
        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(member_id)
    }
```

`in_flight_adds` stays keyed by `(SolutionId, CatalogId)` — an in-flight clone has no member row yet, so the catalog template is still the right key. `PendingAddView.catalog_id` stays a `CatalogId`.

Update this file's own tests to assert on `m.name` / `m.origin_catalog_id` instead of `m.catalog_id`, and to call `add_empty_member(sol_id, "name", cx)` by value.

- [x] **Step 14: Convert `mcp/*.rs`, `event_sources.rs`, `branch_protection.rs`, `persistence.rs`**

- `mcp/solutions_lifecycle.rs`: `SolutionSummary.id: i64`; every tool input's `solution_id: i64`; `build_summary` renders `last_opened_at` via `chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|t| t.to_rfc3339())`. `is_open(sol.id)` takes the id by value.
  **Ordering note:** `editor_mcp::solution_socket_path` / `open_solution_socket` / `close_solution_socket` still take `&str` at this point (Task 5 flips them to `i64`). Call them with the numeric id rendered as a string so this task compiles on its own — the directory name is already the right one:

```rust
    let mcp_socket = open.then(|| {
        editor_mcp::solution_socket_path(&sol.id.0.to_string())
            .to_string_lossy()
            .into_owned()
    });
```

  The same `&sol.id.0.to_string()` shim applies to the `open_solution_socket` / `close_solution_socket` calls in `event_sources.rs`. Task 5 Step 3 removes both shims.
- `mcp/member_mgmt.rs`: `AddMemberParams { solution_id: i64, catalog_id: i64 }`, `AddEmptyMemberParams { solution_id: i64, name: String }` → result `{ member_id: i64 }`, `RemoveMemberParams { member_id: i64 }`, `ReorderMembersParams { solution_id: i64, member_ids: Vec<i64> }`, `SetActiveMemberParams { solution_id: i64, member_id: i64 }`. Member listings return `{ id: i64, name: String, local_path: String, origin_catalog_id: Option<i64> }`.
- `mcp/catalog.rs`: `catalog.{remove_project,merge_project,edit_project}` take `i64` ids; `catalog.add_project` returns `{ id: i64 }`.
- `event_sources.rs`: the `solution_changed` / `solution_active_changed` / `solution_member_*` / `solution_active_member_changed` payloads emit `id.0` (a JSON number) instead of `id.as_str()`; the `ActiveMemberChanged` arm reads `member` instead of `catalog`.
- `branch_protection.rs`: mechanical (`Solution` field types only).
- `persistence.rs`: `SolutionsConfig` stays as the in-memory hydration struct, but its `sample_config()` test fixture moves to numeric ids and the new `SolutionMember` shape. The legacy-JSON `Deserialize` path still has to parse the OLD file format, so keep a separate private struct for it:

```rust
/// The legacy `solutions.json` shape (TEXT slugs). Only `migrate.rs` parses it;
/// the live config uses `SolutionsConfig`, whose ids are counters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacySolutionsConfig {
    #[serde(default)]
    pub catalog: Vec<LegacyCatalogProject>,
    #[serde(default)]
    pub solutions: Vec<LegacySolution>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyCatalogProject {
    pub id: String,
    pub name: String,
    pub remote_url: String,
    #[serde(default)]
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacySolution {
    pub id: String,
    pub name: String,
    pub root: std::path::PathBuf,
    #[serde(default)]
    pub members: Vec<LegacySolutionMember>,
    #[serde(default)]
    pub last_opened_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacySolutionMember {
    pub catalog_id: String,
    pub local_path: std::path::PathBuf,
}

pub fn load_or_default(path: &Path) -> Result<LegacySolutionsConfig, LoadError> {
    if !path.exists() {
        return Ok(LegacySolutionsConfig::default());
    }
    let raw = std::fs::read_to_string(path)?;
    let cfg: LegacySolutionsConfig = serde_json::from_str(&raw)?;
    Ok(cfg)
}
```

(`CURRENT_VERSION` and the version guard stay on `SolutionsConfig` for the in-memory struct; the legacy loader drops the version check — a file that old has no newer-version case left to guard.)

- [x] **Step 15: Rewrite `migrate.rs` (JSON import + identity verification)**

```rust
//! One-time import of the legacy solutions.json file into SolutionsDb, plus
//! verification of the identity migration's row counts.

use crate::db::SolutionsDb;
use crate::persistence::load_or_default;
use anyhow::{Context, Result, bail};
use std::path::Path;

pub fn run_one_time_migration(db: &SolutionsDb, json_path: &Path) -> Result<()> {
    if !json_path.exists() {
        return Ok(());
    }
    let existing_catalog = gpui::block_on(db.load_all_catalog_projects())?;
    let existing_solutions = gpui::block_on(db.load_all_solutions_with_members())?;
    if !existing_catalog.is_empty() || !existing_solutions.is_empty() {
        log::info!(
            "solutions::migrate: skipping import — DB already has {} catalog rows and {} solution rows",
            existing_catalog.len(),
            existing_solutions.len()
        );
        return Ok(());
    }

    let cfg = load_or_default(json_path).context("parse legacy solutions.json")?;
    log::info!(
        "solutions::migrate: importing {} catalog and {} solution(s) from {}",
        cfg.catalog.len(),
        cfg.solutions.len(),
        json_path.display()
    );

    let mut catalog_ids: collections::HashMap<String, i64> = collections::HashMap::default();
    for c in &cfg.catalog {
        let id = gpui::block_on(db.insert_catalog_project(
            c.name.clone(),
            c.remote_url.clone(),
            c.default_branch.clone(),
        ))?;
        catalog_ids.insert(c.id.clone(), id);
    }
    for s in &cfg.solutions {
        let solution_id = gpui::block_on(db.insert_solution(
            s.name.clone(),
            s.root.to_string_lossy().into_owned(),
            s.last_opened_at.map(|t| t.timestamp_millis()),
        ))?;
        for (i, m) in s.members.iter().enumerate() {
            gpui::block_on(db.insert_solution_member(
                solution_id,
                m.catalog_id.clone(),
                m.local_path.to_string_lossy().into_owned(),
                i as i32,
                catalog_ids.get(&m.catalog_id).copied(),
            ))?;
        }
    }

    let bak = json_path.with_extension("json.migrated.bak");
    std::fs::rename(json_path, &bak)
        .with_context(|| format!("renaming {} -> {}", json_path.display(), bak.display()))?;
    log::info!(
        "solutions::migrate: import done; original file moved to {}",
        bak.display()
    );
    Ok(())
}

/// Read the row-count report the identity migration wrote inside its own
/// transaction and fail loudly if any table lost a row. `Ok(())` when the report
/// is absent — that means the DB was created fresh at the numeric schema.
pub fn verify_identity_migration(db: &SolutionsDb) -> Result<()> {
    let Some((
        solutions_before,
        solutions_after,
        members_before,
        members_after,
        active_before,
        active_after,
        catalog_before,
        catalog_after,
    )) = gpui::block_on(db.load_identity_migration_report())?
    else {
        return Ok(());
    };

    let mismatches: Vec<String> = [
        ("solutions", solutions_before, solutions_after),
        ("solution_members", members_before, members_after),
        ("active_member", active_before, active_after),
        ("catalog_projects", catalog_before, catalog_after),
    ]
    .iter()
    .filter(|(_, before, after)| before != after)
    .map(|(table, before, after)| format!("{table}: {before} rows before, {after} after"))
    .collect();

    if !mismatches.is_empty() {
        bail!(
            "identity migration lost rows — {}. The pre-migration data is NOT recoverable from \
             this DB; restore the backup taken before the upgrade.",
            mismatches.join("; ")
        );
    }
    log::info!(
        "solutions::migrate: identity migration verified — {solutions_after} solution(s), \
         {members_after} member(s), {active_after} active selection(s), {catalog_after} catalog row(s)"
    );
    Ok(())
}
```

Add a test for the verifier in `migrate.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SolutionsDb;

    #[gpui::test]
    async fn verify_passes_on_a_fresh_db() {
        let db = SolutionsDb::open_test_db("solutions_migrate_verify_fresh").await;
        verify_identity_migration(&db).expect("a fresh DB has no report row and must verify");
    }

    #[gpui::test]
    async fn verify_fails_when_the_report_shows_a_lost_row() {
        let db = SolutionsDb::open_test_db("solutions_migrate_verify_lost").await;
        db.write(|connection| {
            connection
                .exec(
                    "INSERT INTO identity_migration_report (
                         id, solutions_before, solutions_after, members_before, members_after,
                         active_before, active_after, catalog_before, catalog_after)
                     VALUES (1, 16, 16, 40, 39, 5, 5, 12, 12)",
                )
                .expect("prepare report insert")()
            .expect("report insert");
        })
        .await;
        let err = verify_identity_migration(&db).expect_err("a lost member row must fail");
        assert!(
            err.to_string().contains("solution_members: 40 rows before, 39 after"),
            "the error must name the table and the counts; got: {err}"
        );
    }
}
```

- [x] **Step 16: Run the whole `solutions` suite**

Run: `cargo test -p solutions`
Expected: PASS. Fix compile errors in `store.rs`'s own `mod tests`, `tests/persistence_e2e.rs` and `add_member.rs`'s tests as the compiler surfaces them — the substitutions are mechanical: `SolutionId("x".into())` → the id returned by `create_solution` / `create_for_test_minimal`; `store.remove_member(&sol, &cat, cx)` → `store.remove_member(member_id, cx)`; `s.active_member(&sol)` → `s.active_member(sol)`; `m.catalog_id` → `m.name` / `m.origin_catalog_id`.

- [x] **Step 17: Commit**

```bash
git add crates/solutions/src
git commit -m "Give solutions, members and catalog projects numeric identities"
```

---

### Task 2: Numeric solution ids + `member_id` column in the solution_agent DB

**Files:**
- Modify: `crates/solution_agent/src/db.rs:120-170` (add the `member_id` column), `crates/solution_agent/src/db.rs:305-330` (add the identity-migration entry points)
- Modify: `crates/solution_agent/src/db/sessions.rs` (bind `i64` solution ids; carry `member_id` through the metadata INSERT/SELECT)
- Modify: `crates/solution_agent/src/model.rs:1012-1057` (`SolutionSessionMetadata.member_id`)
- Modify: `crates/solution_agent/src/solution_agent.rs:87-110` (run the identity migration at init)
- Modify (mechanical, compiler-driven): `crates/solution_agent/src/store.rs`, `store/hydration.rs`, `store/teardown.rs`, `store/connection_pool.rs`, `pool.rs`, `mcp/*.rs`, `session_view/subagent_strip.rs`, `store/test_support.rs`, `store/tests/*.rs`
- Test: `crates/solution_agent/src/db/tests.rs`

**Interfaces:**
- Consumes: `solutions::{SolutionId, MemberId}` (`pub struct X(pub i64)`), `SolutionsDb::load_solution_legacy_ids() -> Result<Vec<(String, i64)>>`, `SolutionStore::solutions()`, `SolutionMember { id, name, local_path, origin_catalog_id }` (Task 1).
- Produces:
  - `solution_agent::db::IdentityMigrationReport { pub sessions_total: i64, pub sessions_remapped: i64, pub sessions_unmapped: Vec<String>, pub member_ids_backfilled: i64 }`
  - `SolutionAgentDb::migrate_identity(&self, legacy_solution_ids: Vec<(String, i64)>, members: Vec<(i64, i64, String)>) -> Task<Result<IdentityMigrationReport>>` — `members` is `(member_id, solution_id, local_path)`
  - `SolutionSessionMetadata.member_id: Option<MemberId>`
  - `SolutionAgentDb::set_session_member(&self, id: SolutionSessionId, member_id: Option<MemberId>) -> Task<Result<()>>`

- [x] **Step 1: Write the failing migration test**

Append to `crates/solution_agent/src/db/tests.rs`:

```rust
    use sqlez::connection::Connection;

    /// Seed a DB whose `solution_sessions` rows still carry TEXT slug
    /// `solution_id`s (what every existing user has) and no `member_id`.
    fn seed_legacy_sessions(db: &SolutionAgentDb) {
        let connection = db.connection.lock();
        connection
            .exec(
                "INSERT INTO solution_sessions
                    (id, solution_id, agent_id, acp_session_id, title, created_at,
                     last_activity_at, context_count, cwd)
                 VALUES
                    ('11111111-1111-4111-8111-111111111111', 'spk-solutions', 'claude-acp', 'acp-1',
                     'One', 1, 1, 1, '/home/u/sol/spk-solutions/sawe'),
                    ('22222222-2222-4222-8222-222222222222', 'spk-solutions', 'claude-acp', 'acp-2',
                     'Two', 2, 2, 1, '/home/u/sol/spk-solutions'),
                    ('33333333-3333-4333-8333-333333333333', 'ghost', 'claude-acp', 'acp-3',
                     'Three', 3, 3, 1, '/home/u/sol/ghost')",
            )
            .expect("prepare legacy session insert")()
        .expect("legacy session insert");
    }

    #[gpui::test]
    async fn migrate_identity_remaps_slugs_and_backfills_member_ids(cx: &mut gpui::TestAppContext) {
        let db = SolutionAgentDb::open(cx.executor()).expect("open db");
        seed_legacy_sessions(&db);

        let report = db
            .migrate_identity(
                vec![("spk-solutions".to_string(), 7)],
                vec![(
                    42,
                    7,
                    "/home/u/sol/spk-solutions/sawe".to_string(),
                )],
            )
            .await
            .expect("migrate identity");

        assert_eq!(report.sessions_total, 3);
        assert_eq!(report.sessions_remapped, 2);
        assert_eq!(
            report.sessions_unmapped,
            vec!["ghost".to_string()],
            "a session whose solution no longer exists must be reported, not silently dropped"
        );
        assert_eq!(report.member_ids_backfilled, 1);

        let rows = {
            let connection = db.connection.lock();
            connection
                .select::<(String, String, Option<i64>)>(
                    "SELECT id, solution_id, member_id FROM solution_sessions ORDER BY id",
                )
                .expect("prepare select sessions")()
            .expect("select sessions")
        };
        assert_eq!(rows[0].1, "7", "the slug is replaced by the numeric id");
        assert_eq!(rows[0].2, Some(42), "cwd == member.local_path binds member_id");
        assert_eq!(rows[1].1, "7");
        assert_eq!(
            rows[1].2, None,
            "a session at the solution root keeps a NULL member_id (the ROOT label)"
        );
        assert_eq!(
            rows[2].1, "ghost",
            "an unmapped session's id is left alone so the row is still inspectable"
        );
    }

    #[gpui::test]
    async fn migrate_identity_is_idempotent(cx: &mut gpui::TestAppContext) {
        let db = SolutionAgentDb::open(cx.executor()).expect("open db");
        seed_legacy_sessions(&db);
        let mapping = vec![("spk-solutions".to_string(), 7)];
        let members = vec![(42, 7, "/home/u/sol/spk-solutions/sawe".to_string())];

        db.migrate_identity(mapping.clone(), members.clone())
            .await
            .expect("first run");
        let second = db
            .migrate_identity(mapping, members)
            .await
            .expect("second run");

        assert_eq!(
            second.sessions_remapped, 0,
            "already-numeric rows must not be touched again"
        );
        assert_eq!(
            second.member_ids_backfilled, 0,
            "already-bound sessions must not be rewritten"
        );
        let count = {
            let connection = db.connection.lock();
            connection
                .select::<i64>("SELECT COUNT(*) FROM solution_sessions")
                .expect("prepare count")()
            .expect("count")
        };
        assert_eq!(count, vec![3], "no rows created or destroyed");
    }
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p solution_agent --lib db::tests::migrate_identity -- --nocapture`
Expected: FAIL to compile — `error[E0599]: no method named 'migrate_identity' found for struct 'SolutionAgentDb'`.

- [x] **Step 3: Add the `member_id` column**

In `crates/solution_agent/src/db.rs`, after the `cached_models` ALTER (line ~168):

```rust
        // Phase 1 (rename/identity): the session's project, as a fact rather
        // than an inference. NULL = the session runs at the solution root (the
        // "ROOT" label). Previously the project was derived by comparing `cwd`
        // to each member's `local_path` with exact equality, so any path drift
        // silently degraded the label to ROOT. No FK: `solution_agent.db` is a
        // different file from the solutions DB, so a dangling member_id degrades
        // to "unknown project" instead of corrupting the row.
        apply_idempotent_add_column(&connection, "member_id INTEGER");
```

- [x] **Step 4: Implement `migrate_identity`**

In `crates/solution_agent/src/db.rs`, add next to `delete_for_solution`:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityMigrationReport {
    pub sessions_total: i64,
    pub sessions_remapped: i64,
    /// Slugs that had no entry in the solutions DB's legacy map — their rows are
    /// left untouched so the operator can inspect them. Non-empty means a
    /// solution was deleted from `solutions.db` while its sessions survived.
    pub sessions_unmapped: Vec<String>,
    pub member_ids_backfilled: i64,
}

impl SolutionAgentDb {
    /// Rewrite `solution_sessions.solution_id` / `solution_session_attachment
    /// .solution_id` from the pre-identity TEXT slug to the numeric counter id
    /// the solutions DB now uses, and bind each session to the member whose
    /// `local_path` equals its `cwd`.
    ///
    /// Idempotent: rows whose `solution_id` already parses as an integer are
    /// skipped, and `member_id` is only written where it is still NULL.
    ///
    /// `legacy_solution_ids` is `(old_slug, new_id)` from
    /// `SolutionsDb::load_solution_legacy_ids`; `members` is
    /// `(member_id, solution_id, local_path)`.
    pub fn migrate_identity(
        &self,
        legacy_solution_ids: Vec<(String, i64)>,
        members: Vec<(i64, i64, String)>,
    ) -> Task<Result<IdentityMigrationReport>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            migrate_identity_fn(&connection, &legacy_solution_ids, &members)
        })
    }

    pub fn set_session_member(
        &self,
        id: SolutionSessionId,
        member_id: Option<solutions::MemberId>,
    ) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            let mut update = connection.exec_bound::<(Option<i64>, String)>(indoc! {"
                UPDATE solution_sessions SET member_id = ?1 WHERE id = ?2
            "})?;
            update((member_id.map(|m| m.0), id.to_string()))?;
            Ok(())
        })
    }
}

fn migrate_identity_fn(
    connection: &Connection,
    legacy_solution_ids: &[(String, i64)],
    members: &[(i64, i64, String)],
) -> Result<IdentityMigrationReport> {
    let sessions = connection.select::<(String, String, Option<String>, Option<i64>)>(indoc! {"
        SELECT id, solution_id, cwd, member_id FROM solution_sessions
    "})?()?;

    let mut report = IdentityMigrationReport {
        sessions_total: sessions.len() as i64,
        ..Default::default()
    };

    let mut remap = connection.exec_bound::<(String, String)>(indoc! {"
        UPDATE solution_sessions SET solution_id = ?1 WHERE id = ?2
    "})?;
    let mut remap_attachments = connection.exec_bound::<(String, String)>(indoc! {"
        UPDATE solution_session_attachment SET solution_id = ?1 WHERE solution_id = ?2
    "})?;
    let mut bind_member = connection.exec_bound::<(i64, String)>(indoc! {"
        UPDATE solution_sessions SET member_id = ?1 WHERE id = ?2
    "})?;

    let mut unmapped: Vec<String> = Vec::new();
    for (session_id, solution_id, cwd, member_id) in &sessions {
        // Already-numeric rows were migrated by an earlier run.
        let numeric_solution: i64 = match solution_id.parse::<i64>() {
            Ok(numeric) => numeric,
            Err(_) => {
                let Some((_, new_id)) = legacy_solution_ids
                    .iter()
                    .find(|(old, _)| old == solution_id)
                else {
                    if !unmapped.contains(solution_id) {
                        unmapped.push(solution_id.clone());
                    }
                    continue;
                };
                remap((new_id.to_string(), session_id.clone()))?;
                remap_attachments((new_id.to_string(), solution_id.clone()))?;
                report.sessions_remapped += 1;
                *new_id
            }
        };

        if member_id.is_some() {
            continue;
        }
        let Some(cwd) = cwd.as_deref().filter(|c| !c.is_empty()) else {
            continue;
        };
        // Exact match only: `cwd` is the member root at spawn time. A cwd that is
        // the solution root (or anything else) stays NULL — that IS the ROOT label.
        let Some((matched_member, _, _)) = members
            .iter()
            .find(|(_, solution, path)| *solution == numeric_solution && path == cwd)
        else {
            continue;
        };
        bind_member((*matched_member, session_id.clone()))?;
        report.member_ids_backfilled += 1;
    }

    report.sessions_unmapped = unmapped;
    Ok(report)
}
```

`IdentityMigrationReport` must be re-exported from `crates/solution_agent/src/solution_agent.rs`:

```rust
pub use db::{IdentityMigrationReport, SolutionAgentDb};
```

- [x] **Step 5: Run the migration tests to verify they pass**

Run: `cargo test -p solution_agent --lib db::tests::migrate_identity -- --nocapture`
Expected: FAIL to compile still — `solutions::SolutionId` is now `i64` and `db/sessions.rs` binds it as a `String`. Fix that next; the two new tests are the target of Step 8.

- [x] **Step 6: Carry numeric ids and `member_id` through the metadata read/write**

In `crates/solution_agent/src/model.rs`, add to `SolutionSessionMetadata` (after `cwd`):

```rust
    /// The member this session belongs to. `None` = the solution root (the
    /// "ROOT" label). Source of truth for the project label and console-tab
    /// scoping — replaces the old cwd-equality inference.
    pub member_id: Option<solutions::MemberId>,
```

In `crates/solution_agent/src/db/sessions.rs`, `insert_or_update_metadata`: the column list grows to 17, so the third tuple becomes 5-wide and `solution_id` binds as `i64`:

```rust
    let mut insert = connection.exec_bound::<(
        (String, i64, String, Arc<str>, String),
        (
            i64,
            i64,
            Option<String>,
            Option<i64>,
            i64,
            Option<String>,
            Option<String>,
        ),
        (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
        ),
    )>(indoc! {"
        INSERT INTO solution_sessions (
            id, solution_id, agent_id, acp_session_id, title,
            created_at, last_activity_at, preview, total_tokens,
            context_count, cwd, parent_session_id,
            desired_model, desired_effort, cached_models, tab_order, member_id
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
        ON CONFLICT(id) DO UPDATE SET
            solution_id        = excluded.solution_id,
            agent_id           = excluded.agent_id,
            acp_session_id     = excluded.acp_session_id,
            title              = excluded.title,
            created_at         = excluded.created_at,
            last_activity_at   = excluded.last_activity_at,
            preview            = COALESCE(excluded.preview, preview),
            total_tokens       = COALESCE(excluded.total_tokens, total_tokens),
            context_count      = excluded.context_count,
            cwd                = COALESCE(excluded.cwd, cwd),
            parent_session_id  = COALESCE(excluded.parent_session_id, parent_session_id),
            desired_model      = COALESCE(excluded.desired_model, desired_model),
            desired_effort     = COALESCE(excluded.desired_effort, desired_effort),
            cached_models      = COALESCE(excluded.cached_models, cached_models),
            tab_order          = COALESCE(excluded.tab_order, solution_sessions.tab_order),
            member_id          = COALESCE(excluded.member_id, solution_sessions.member_id)
    "})?;
```

and the bind site:

```rust
    insert((
        (
            meta.id.to_string(),
            meta.solution_id.0,
            meta.agent_id.to_string(),
            meta.acp_session_id.0.clone(),
            meta.title.to_string(),
        ),
        (
            meta.created_at.timestamp_millis(),
            meta.last_activity_at.timestamp_millis(),
            meta.preview.as_ref().map(|s| s.to_string()),
            meta.total_tokens.map(|t| t as i64),
            meta.context_count as i64,
            cwd_str,
            meta.parent_session_id.map(|id| id.to_string()),
        ),
        (
            meta.desired_model.clone(),
            meta.desired_effort.clone(),
            cached_models_json,
            meta.tab_order,
            meta.member_id.map(|m| m.0),
        ),
    ))?;
```

`select_metadata_for_solution` mirrors it: `select_bound::<i64, (…)>`, `SELECT … , tab_order, member_id`, bind `solution_id.0`, and construct `member_id: member_id.map(solutions::MemberId)`, `solution_id: SolutionId(solution_id)`. Every other fn in this file that binds a `SolutionId` (`select_sessions_closed_before`, `select_closed_session_ids`, `delete_by_solution`) binds `solution_id.0` as `i64`.

- [x] **Step 7: Run the identity migration at startup**

In `crates/solution_agent/src/solution_agent.rs`'s `init`, where `SolutionAgentDb::connect(cx)` is awaited and wired into the store, run the migration before the store hydrates. `solutions::init` (which builds `SolutionStore` and runs the solutions-side migration) is called at `crates/zed/src/main.rs:792`, before `solution_agent::init` at `:794`, so the legacy map and the member list are both available:

```rust
    let db_task = db::SolutionAgentDb::connect(cx);
    cx.spawn(async move |cx| {
        let db = match db_task.await {
            Ok(db) => db,
            Err(err) => {
                log::error!("solution_agent: DB unavailable: {err}");
                return;
            }
        };
        let inputs = cx.update(|cx| {
            let solutions_db = solutions::db::SolutionsDb::global(cx).clone();
            let store = solutions::SolutionStore::global(cx);
            let members: Vec<(i64, i64, String)> = store.read_with(cx, |store, _| {
                store
                    .solutions()
                    .iter()
                    .flat_map(|solution| {
                        solution.members.iter().map(move |member| {
                            (
                                member.id.0,
                                solution.id.0,
                                member.local_path.to_string_lossy().into_owned(),
                            )
                        })
                    })
                    .collect()
            });
            (solutions_db, members)
        });
        let Ok((solutions_db, members)) = inputs else {
            return;
        };
        let legacy = match solutions_db.load_solution_legacy_ids().await {
            Ok(legacy) => legacy,
            Err(err) => {
                log::error!("solution_agent: cannot read the solutions legacy id map: {err}");
                return;
            }
        };
        match db.migrate_identity(legacy, members).await {
            Ok(report) => {
                if !report.sessions_unmapped.is_empty() {
                    log::error!(
                        "solution_agent: {} session(s) reference solution slug(s) {:?} that no \
                         longer exist — their rows were left untouched",
                        report.sessions_total - report.sessions_remapped,
                        report.sessions_unmapped
                    );
                }
                log::info!(
                    "solution_agent: identity migration — {} of {} session(s) remapped, {} member \
                     binding(s) backfilled",
                    report.sessions_remapped,
                    report.sessions_total,
                    report.member_ids_backfilled
                );
            }
            Err(err) => log::error!("solution_agent: identity migration failed: {err}"),
        }
        // … existing "wire db into the store" code continues here, unchanged …
    })
    .detach();
```

Keep whatever the existing spawn already does after the DB resolves — the migration is inserted *before* that work, in the same task, so hydration never reads un-remapped rows.

- [x] **Step 8: Sweep the rest of the crate and run the suite**

`SolutionId` is `Copy` and numeric now, so the compiler will point at every `&solution_id` / `.0.clone()` / `SolutionId(string)` site in `store.rs`, `store/hydration.rs`, `store/teardown.rs`, `store/connection_pool.rs`, `pool.rs`, `mcp/{read,lifecycle,debug}.rs`, `session_view/subagent_strip.rs`, `message_generator.rs`, `reopen_session_modal.rs` and the test modules. The substitutions are mechanical: drop the `&`, drop the `.clone()`, and in the MCP tool params change `solution_id: String` to `solution_id: i64` (`solutions.*` and `solution_agent.*` both address by number now). `SolutionSessionMetadata` literals gain `member_id: None`.

Run: `cargo test -p solution_agent`
Expected: PASS (including `migrate_identity_remaps_slugs_and_backfills_member_ids` and `migrate_identity_is_idempotent`).

- [x] **Step 9: Commit**

```bash
git add crates/solution_agent/src
git commit -m "Migrate solution_agent sessions to numeric solution ids and bind them to members"
```

---

### Task 3: The session's project comes from `member_id`, not from its cwd

**Files:**
- Modify: `crates/solution_agent/src/store.rs:518-536` (delete `project_name_for_cwd`, add `project_label`), `crates/solution_agent/src/store.rs:704-800` (`create_session*` takes a `member_id`), `crates/solution_agent/src/store.rs:899` (title seeding)
- Modify: `crates/solution_agent/src/status_row.rs:160`
- Modify: `crates/solution_agent/src/model.rs` (`SolutionSession.member_id`)
- Modify: `crates/solution_agent/src/store/hydration.rs` (carry `member_id` from metadata onto the session)
- Test: `crates/solution_agent/src/store/tests/misc.rs`

**Interfaces:**
- Consumes: `SolutionSessionMetadata.member_id: Option<MemberId>`, `SolutionAgentDb::set_session_member` (Task 2); `SolutionStore::find_member`, `SolutionStore::active_member`, `Solution::member` (Task 1).
- Produces:
  - `solution_agent::store::project_label(solution: &Solution, member_id: Option<MemberId>, cx: &App) -> Option<SharedString>` — `None` means "solution root" (callers render `ROOT`).
  - `SolutionSession.member_id: Option<MemberId>` (public field on the session entity).
  - `SolutionAgentStore::create_session_with_parent(..., member_id: Option<MemberId>, ...)` — the `cwd` parameter stays (it is what the subprocess is spawned in) but `member_id` is what the label and tab scoping read.

- [x] **Step 1: Write the failing label test**

Append to `crates/solution_agent/src/store/tests/misc.rs`:

```rust
    /// The project label is a stored fact (`member_id`), not a cwd comparison.
    /// A session whose cwd has drifted from the member's `local_path` — exactly
    /// what a folder rename produces — must still show its project.
    #[gpui::test]
    async fn project_label_reads_member_id_not_cwd(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| solutions::SolutionStore::for_test(dir.path().join("s.json"), cx));
        cx.update(|cx| solutions::install_global_for_test(store.clone(), cx));

        let solution_id = store
            .update(cx, |s, cx| {
                s.create_solution("Sol", dir.path().to_path_buf(), cx)
            })
            .expect("create solution");
        let member_id = store.update(cx, |s, _| {
            s.test_add_member_with_path(solution_id, "sawe", dir.path().join("sol/sawe"))
        });
        let solution = store.read_with(cx, |s, _| {
            s.find_solution(solution_id).expect("solution").clone()
        });

        cx.update(|cx| {
            assert_eq!(
                crate::store::project_label(&solution, Some(member_id), cx)
                    .as_deref(),
                Some("sawe"),
                "the label comes from the member row"
            );
            assert_eq!(
                crate::store::project_label(&solution, None, cx),
                None,
                "no member = the solution root, which callers render as ROOT"
            );
            assert_eq!(
                crate::store::project_label(&solution, Some(solutions::MemberId(9999)), cx),
                None,
                "a dangling member_id degrades to ROOT rather than panicking"
            );
        });
    }
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p solution_agent --lib store::tests::misc::project_label_reads_member_id_not_cwd -- --nocapture`
Expected: FAIL to compile — `error[E0425]: cannot find function 'project_label' in module 'crate::store'`.

- [x] **Step 3: Replace `project_name_for_cwd` with `project_label`**

In `crates/solution_agent/src/store.rs`, delete `project_name_for_cwd` (lines 518-536 region) and put in its place:

```rust
/// The project label for a session: the name of the member it is bound to.
/// `None` means the session runs at the solution root — the status row renders
/// that as "ROOT" and the default title falls back to `solution.name`.
///
/// This is a lookup, not an inference. The previous implementation compared the
/// session's `cwd` to each member's `local_path` with exact equality, so any
/// path drift (which a folder rename produces by construction) silently degraded
/// every session in the renamed project to ROOT.
pub(crate) fn project_label(
    solution: &Solution,
    member_id: Option<solutions::MemberId>,
    _cx: &App,
) -> Option<SharedString> {
    let member_id = member_id?;
    // A dangling member_id (member removed while a session survived) degrades to
    // ROOT — `solution_agent.db` has no FK into the solutions DB by design.
    let member = solution.member(member_id)?;
    Some(SharedString::from(member.name.clone()))
}
```

- [x] **Step 4: Bind `member_id` onto the session at create and hydrate**

Add the field to `SolutionSession` in `crates/solution_agent/src/model.rs`:

```rust
    /// The member this session belongs to; `None` = solution root. Persisted in
    /// `solution_sessions.member_id`.
    pub member_id: Option<solutions::MemberId>,
```

In `store.rs`, thread it through the create chain. `create_session` picks the solution's **active member** (that is what "new chat from the + menu" means today, since the cwd it passes is `active_member_path`):

```rust
    pub fn create_session(
        &mut self,
        solution_id: SolutionId,
        agent_id: AgentServerId,
        project: Entity<project::Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SolutionSessionId>> {
        let member_id = solutions::SolutionStore::try_global(cx)
            .and_then(|store| store.read(cx).active_member(solution_id));
        self.create_session_with_cwd(solution_id, agent_id, project, None, member_id, None, None, cx)
    }
```

`create_session_with_cwd` and `create_session_with_parent` gain a `member_id: Option<MemberId>` parameter (placed directly after `cwd`, so the two stay adjacent), store it on the `SolutionSession`, and put it on the `SolutionSessionMetadata` they persist. Where the session's cwd is resolved today, resolve it *from* the member when one is given:

```rust
        let session_cwd = match cwd {
            Some(cwd) => cwd,
            None => member_id
                .and_then(|id| {
                    solutions::SolutionStore::try_global(cx)?
                        .read(cx)
                        .find_member(id)
                        .ok()
                        .map(|m| m.local_path.clone())
                })
                .unwrap_or_else(|| solution.root.clone()),
        };
```

The title seed at store.rs:899 becomes:

```rust
                let title_base: SharedString = project_label(&solution, member_id, cx)
                    .unwrap_or_else(|| SharedString::from(solution.name.clone()));
```

In `store/hydration.rs`, copy `meta.member_id` onto the rebuilt `SolutionSession`.

- [x] **Step 5: Point the status row at the new function**

`crates/solution_agent/src/status_row.rs:160`:

```rust
        .and_then(|solution| crate::store::project_label(&solution, s.member_id, cx))
```

- [x] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p solution_agent`
Expected: PASS. Existing call sites of `create_session_with_cwd` / `create_session_with_parent` in `console_panel`, `git_ui` and `solution_git` do not compile yet (they are fixed in Tasks 4 and 6) — that does not affect `-p solution_agent`.

- [x] **Step 7: Commit**

```bash
git add crates/solution_agent/src
git commit -m "Label AI sessions from their bound member instead of their cwd"
```

---

### Task 4: Console-tab scoping by member id

**Files:**
- Modify: `crates/console_panel/src/panel.rs:71-98` (`active_member_path`, `tab_cwd_in_scope`), `:672,826,957-1020,1388` (call sites), `:1970-1990` (tests)
- Modify: `crates/console_panel/src/chat_provider.rs` (pass `member_id` when creating a chat)
- Test: `crates/console_panel/src/panel.rs` (`mod tests`)

**Interfaces:**
- Consumes: `SolutionStore::{active_member, active_member_path, find_solution}`, `Solution::member_for_path`, `MemberId` (Task 1); `SolutionSession.member_id` (Task 3).
- Produces:
  - `console_panel::panel::TabScope` — `enum TabScope { Member(MemberId), Root, Unscoped }`
  - `console_panel::panel::tab_in_scope(scope: TabScope, active_member: Option<MemberId>) -> bool`

- [x] **Step 1: Write the failing scoping test**

Replace `tab_cwd_in_scope_filters_by_active_member` in `crates/console_panel/src/panel.rs`'s `mod tests` with:

```rust
    #[test]
    fn tab_in_scope_filters_by_active_member() {
        let a = MemberId(1);
        let b = MemberId(2);

        // No active member → the panel shows everything.
        assert!(tab_in_scope(TabScope::Member(a), None));
        assert!(tab_in_scope(TabScope::Root, None));
        assert!(tab_in_scope(TabScope::Unscoped, None));

        // A tab bound to the active member is in scope; one bound to a sibling
        // is not — even though both live under the same solution root, and even
        // if one of the folders was renamed out from under the tab's cwd.
        assert!(tab_in_scope(TabScope::Member(a), Some(a)));
        assert!(!tab_in_scope(TabScope::Member(b), Some(a)));

        // A solution-root tab is hidden while a member is selected (it is not
        // part of that project), matching the pre-existing prefix behaviour.
        assert!(!tab_in_scope(TabScope::Root, Some(a)));

        // A tab we cannot place must never silently vanish — hiding it would
        // leave an un-closeable ghost in the strip.
        assert!(tab_in_scope(TabScope::Unscoped, Some(a)));
    }
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p console_panel --lib panel::tests::tab_in_scope_filters_by_active_member -- --nocapture`
Expected: FAIL to compile — `error[E0433]: failed to resolve: use of undeclared type 'TabScope'`.

- [x] **Step 3: Implement `TabScope` / `tab_in_scope`**

Replace `tab_cwd_in_scope` (and rewrite `active_member_path`) in `crates/console_panel/src/panel.rs`:

```rust
use solutions::{MemberId, SolutionId, SolutionStore};

/// Which project a console tab belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabScope {
    /// The tab is bound to a member — either as a fact (a chat carries the
    /// session's `member_id`) or by placement (a terminal's cwd lives inside
    /// the member's folder).
    Member(MemberId),
    /// The tab sits at the solution root and belongs to no project.
    Root,
    /// The tab cannot be placed at all (a terminal opened by the bare
    /// `NewTerminal` keybinding with no cwd, or one restored with a NULL cwd).
    Unscoped,
}

/// Whether a tab is visible under the current active-member selection. A `None`
/// active member means "no filter". An `Unscoped` tab is always shown: a tab we
/// can't place must never silently vanish (that would leave an un-closeable
/// ghost tab in the strip).
pub(crate) fn tab_in_scope(scope: TabScope, active_member: Option<MemberId>) -> bool {
    match (scope, active_member) {
        (_, None) => true,
        (TabScope::Unscoped, _) => true,
        (TabScope::Member(member), Some(active)) => member == active,
        (TabScope::Root, Some(_)) => false,
    }
}

/// Folder of the solution's *active* project, falling back to the solution root
/// when there is no active member. The `cwd` for new terminals / AI chats.
fn active_member_path(solution_id: SolutionId, cx: &App) -> Option<PathBuf> {
    let store = SolutionStore::try_global(cx)?;
    store.read(cx).active_member_path(solution_id)
}
```

Add the per-tab scope resolver as a `ConsolePanel` method (next to the existing `tab_cwd`):

```rust
    /// A chat tab's scope is a stored fact — the session's `member_id`. A
    /// terminal has no member binding, so it is placed by its cwd (longest
    /// matching member wins; anything under the root but in no member is `Root`).
    fn tab_scope(&self, tab: &ConsoleTab, cx: &App) -> TabScope {
        if let ConsoleTab::Chat(chat) = tab
            && let Some(session) = chat.read(cx).session(cx)
        {
            return match session.read(cx).member_id {
                Some(member_id) => TabScope::Member(member_id),
                None => TabScope::Root,
            };
        }
        let Some(cwd) = self.tab_cwd(tab, cx) else {
            return TabScope::Unscoped;
        };
        let Some(solution_id) = self.solution_id(cx) else {
            return TabScope::Unscoped;
        };
        let Some(store) = SolutionStore::try_global(cx) else {
            return TabScope::Unscoped;
        };
        let store = store.read(cx);
        let Ok(solution) = store.find_solution(solution_id) else {
            return TabScope::Unscoped;
        };
        match solution.member_for_path(&cwd) {
            Some(member) => TabScope::Member(member.id),
            None if cwd.starts_with(&solution.root) => TabScope::Root,
            None => TabScope::Unscoped,
        }
    }
```

(Adapt `chat.read(cx).session(cx)` and `self.solution_id(cx)` to whatever the file's existing accessors are named — the panel already reaches both the chat's session entity and its solution id for `active_member_path`.)

Update the three filter sites (`:982`, `:1017`, and the render loop that computes `in_scope`) from

```rust
                tab_cwd_in_scope(self.tab_cwd(tab, cx).as_deref(), member_path.as_deref())
```

to

```rust
                tab_in_scope(self.tab_scope(tab, cx), active_member)
```

where `active_member` is `self.solution_id(cx).and_then(|id| SolutionStore::try_global(cx).and_then(|s| s.read(cx).active_member(id)))`. `active_member_path` is still used for *creating* tabs (`:672`, `:1388`) — keep those.

- [x] **Step 4: Pass the member id when starting a chat**

In `crates/console_panel/src/chat_provider.rs`, where the panel calls `create_session_with_cwd`, pass the active member so the new session is bound rather than inferred:

```rust
        let member_id = SolutionStore::try_global(cx)
            .and_then(|store| store.read(cx).active_member(solution_id));
        store.update(cx, |store, cx| {
            store.create_session_with_cwd(solution_id, agent_id, project, cwd, member_id, None, None, cx)
        })
```

- [x] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p console_panel`
Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add crates/console_panel/src
git commit -m "Scope console tabs by member id instead of cwd prefix"
```

---

### Task 5: Numeric per-solution MCP sockets + stale slug-dir sweep

**Files:**
- Modify: `crates/editor_mcp/src/lifecycle.rs:294-300` (`solution_socket_path`), `:301-330` (`start_server` — add the sweep), `:382-412` (`open_solution_socket` / `close_solution_socket`)
- Modify: `crates/context_server/src/listener.rs:45,95-125,253,307,336,471-501` (`bound_solution_id: Option<i64>`)
- Modify: `crates/solutions/src/mcp/solutions_lifecycle.rs`, `crates/solutions/src/event_sources.rs` (drop the `&id.0.to_string()` shims from Task 1)
- Modify: `crates/editor_mcp/tests/*_e2e_test.rs` (string `solution_id` literals become numbers)
- Test: `crates/editor_mcp/src/lifecycle.rs` (`mod tests`) — create it if the file has none

**Interfaces:**
- Consumes: nothing from earlier tasks at the type level (`editor_mcp` does not depend on `solutions`), but the *values* passed in are now `SolutionId(i64).0`.
- Produces:
  - `editor_mcp::solution_socket_path(solution_id: i64) -> PathBuf` — `<runtime>/solutions/<id>/mcp.sock`
  - `editor_mcp::open_solution_socket(cx: &mut App, solution_id: i64, root: PathBuf)`
  - `editor_mcp::close_solution_socket(cx: &mut App, solution_id: i64)`
  - `editor_mcp::lifecycle::remove_stale_solution_socket_dirs(runtime: &Path) -> usize` (returns how many were removed)
  - `context_server::listener::McpServer::set_bound_solution(&self, id: i64)` — the injected `solution_id` argument is now a JSON number.

- [ ] **Step 1: Write the failing sweep test**

In `crates/editor_mcp/src/lifecycle.rs`, add (or extend) `mod tests`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_slug_socket_dirs_are_removed_numeric_ones_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let solutions = dir.path().join("solutions");
        for name in ["spk-solutions", "ecos-platform", "12", "7"] {
            std::fs::create_dir_all(solutions.join(name)).expect("mkdir");
            std::fs::write(solutions.join(name).join("mcp.sock"), b"").expect("touch sock");
        }

        let removed = remove_stale_solution_socket_dirs(dir.path());

        assert_eq!(removed, 2, "both slug-named dirs must go");
        assert!(!solutions.join("spk-solutions").exists());
        assert!(!solutions.join("ecos-platform").exists());
        assert!(
            solutions.join("12").exists() && solutions.join("7").exists(),
            "numeric dirs are live socket homes and must be left alone"
        );
    }

    #[test]
    fn sweep_is_a_no_op_when_the_solutions_dir_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(remove_stale_solution_socket_dirs(dir.path()), 0);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p editor_mcp --lib lifecycle::tests -- --nocapture`
Expected: FAIL to compile — `error[E0425]: cannot find function 'remove_stale_solution_socket_dirs' in this scope`.

- [ ] **Step 3: Implement the numeric socket path and the sweep**

In `crates/editor_mcp/src/lifecycle.rs`:

```rust
/// Deterministic path of a Solution's per-solution MCP socket. Pure — the socket
/// only exists between [`open_solution_socket`] and [`close_solution_socket`].
/// The directory component is the Solution's numeric id: an id is stable across
/// a rename, which is the whole point of the identity model.
pub fn solution_socket_path(solution_id: i64) -> PathBuf {
    runtime_dir()
        .join("solutions")
        .join(solution_id.to_string())
        .join("mcp.sock")
}

/// Delete `<runtime>/solutions/<name>` directories whose name is not a numeric
/// solution id. Those are leftovers from the pre-identity build, where the
/// directory was the solution's slug; nothing will ever bind them again and a
/// stale `mcp.sock` in one is an attractive nuisance for an agent that reads the
/// directory listing instead of `solutions.get`. Returns the number removed.
pub(crate) fn remove_stale_solution_socket_dirs(runtime: &Path) -> usize {
    let solutions_dir = runtime.join("solutions");
    let Ok(entries) = std::fs::read_dir(&solutions_dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.parse::<i64>().is_ok() {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => {
                log::info!(
                    "editor_mcp: removed stale slug-named socket dir {}",
                    entry.path().display()
                );
                removed += 1;
            }
            Err(err) => log::warn!(
                "editor_mcp: could not remove stale socket dir {}: {err}",
                entry.path().display()
            ),
        }
    }
    removed
}
```

Call it from `start_server`, right after the `SingleInstanceLock` is acquired (we hold the single-instance lock, so no other editor process owns those sockets):

```rust
    remove_stale_solution_socket_dirs(&runtime_dir());
```

Change `open_solution_socket(cx: &mut App, solution_id: i64, root: PathBuf)` and `close_solution_socket(cx: &mut App, solution_id: i64)` to take the id by value; key `active.solution_sockets` (the `HashMap`) on `i64` instead of `String`; `McpServer::set_bound_solution` now takes the `i64` directly (no `Arc<str>`).

Then delete the `&sol.id.0.to_string()` shims Task 1 introduced — `crates/solutions/src/mcp/solutions_lifecycle.rs` (`build_summary`) and `crates/solutions/src/event_sources.rs` now pass `sol.id.0`:

```rust
    let mcp_socket = open.then(|| {
        editor_mcp::solution_socket_path(sol.id.0)
            .to_string_lossy()
            .into_owned()
    });
```

- [ ] **Step 4: Make the bound solution id numeric in the listener**

In `crates/context_server/src/listener.rs`, change the field, the constructor local, the setter and the injection:

```rust
    bound_solution_id: Rc<RefCell<Option<i64>>>,
```

```rust
    pub fn set_bound_solution(&self, id: i64) {
        *self.bound_solution_id.borrow_mut() = Some(id);
    }
```

```rust
                    // Per-solution socket: force the bound `solution_id` into
                    // every solution-scoped tool's params so a scoped subagent
                    // cannot target another Solution by passing a different id.
                    let mut arguments = params.arguments;
                    if tool.wants_solution_id
                        && let Some(id) = *bound_solution_id.borrow()
                    {
                        let obj = arguments.get_or_insert_with(|| {
                            serde_json::Value::Object(serde_json::Map::new())
                        });
                        if let Some(map) = obj.as_object_mut() {
                            map.insert(
                                "solution_id".to_string(),
                                serde_json::Value::Number(serde_json::Number::from(id)),
                            );
                        }
                    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p editor_mcp && cargo test -p context_server`
Expected: PASS. `editor_mcp`'s `tests/*_e2e_test.rs` files that pass a string `solution_id` need the literal changed to a number — the compiler and the JSON schema will both point at them.

- [ ] **Step 6: Commit**

```bash
git add crates/editor_mcp/src crates/editor_mcp/tests crates/context_server/src
git commit -m "Key per-solution MCP sockets on the numeric id and sweep stale slug dirs"
```

---

### Task 6: Convert the remaining consumers and green the workspace

**Files:**
- Modify: `crates/solutions_ui/src/*.rs` (18 files — `project_tab.rs`, `project_tab_strip.rs`, `solution_tab.rs`, `solution_tab_strip.rs`, `picker.rs`, `solution_picker_dropdown.rs`, `switch.rs`, `open.rs`, `welcome.rs`, `window_helpers.rs`, `member_layout.rs`, `empty_solution_page.rs`, `add_member_picker.rs`, `add_project_picker.rs`, `modals.rs`, `modals/*.rs`, `solutions_ui.rs`)
- Modify: `crates/project_panel/src/project_panel.rs` (`active_member_path`)
- Modify: `crates/git_ui/src/project_diff.rs`, `crates/git_graph/src/*.rs`, `crates/solution_git/src/{ai_cherry_pick_suggest,cross_cherry_pick}.rs`
- Modify: `crates/workspace_events/src/{dto,lifecycle,shutdown,workspace_events}.rs`, `crates/workspace_events/tests/snapshot_test.rs`
- Modify: `crates/zed/src/main.rs`, `crates/zed/src/notification_focus.rs`
- Modify: `docs/INDEX.md` is NOT touched; `FORK.md` gains one row under "Key architectural decisions"

**Interfaces:**
- Consumes: everything produced by Tasks 1-5.
- Produces: a compiling workspace. No new API.

- [ ] **Step 1: Sweep `solutions_ui`**

The compiler drives this. The recurring substitutions:

```rust
// before                                  // after
sol.id.as_str()                            sol.id.0            // or format!("{}", sol.id)
SolutionId(raw_string)                      SolutionId(raw_i64)
store.remove_member(&sol_id, &cat_id, cx)   store.remove_member(member_id, cx)
store.set_active_member(sol.clone(), cat.clone(), cx)
                                            store.set_active_member(sol, member_id, cx)
store.active_member(&sol_id)                store.active_member(sol_id)
member.catalog_id                           member.name / member.origin_catalog_id
sol.last_opened_at.map(|t| t.to_rfc3339())  sol.last_opened_at
                                                .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                                                .map(|t| t.to_rfc3339())
```

The project tab strip (`project_tab.rs`, `project_tab_strip.rs`) currently labels a tab by looking the catalog project up from `member.catalog_id` and falling back to the slug. That whole lookup collapses to one field:

```rust
    let label = member.name.clone();
```

`reorder_members` now takes `Vec<MemberId>`, and the drag-and-drop payload in `project_tab_strip.rs` carries a `MemberId`.

Run: `cargo check -p solutions_ui --all-targets`
Expected: clean.

- [ ] **Step 2: Sweep the panels and git crates**

`project_panel`, `git_ui`, `git_graph`, `solution_git` only *read* the active member and its path. Point them at the new store accessor:

```rust
    fn active_member_path(&self, cx: &App) -> Option<PathBuf> {
        let solution_id = self.solution_id(cx)?;
        SolutionStore::try_global(cx)?
            .read(cx)
            .active_member_path(solution_id)
    }
```

`solution_git`'s AI helpers call `create_session_with_cwd`; add the `member_id` argument (`None` for the hidden one-shot helper sessions — they are not user-visible chats and carry no project label).

Run: `cargo check -p project_panel -p git_ui -p git_graph -p solution_git --all-targets`
Expected: clean.

- [ ] **Step 3: Sweep `workspace_events` and `zed`**

`workspace_events/src/dto.rs` serialises solution ids onto the mobile wire. Change the DTO field to `i64`:

```rust
pub struct SolutionDto {
    pub id: i64,
    pub name: String,
    pub root: String,
    // …
}
```

and the session DTO gains `member_id: Option<i64>` so the mobile client can render the project label from the same fact the desktop uses. `crates/zed/src/main.rs` and `notification_focus.rs` only pass ids through — mechanical.

Run: `cargo check -p workspace_events -p zed --all-targets`
Expected: clean.

- [ ] **Step 4: Green the whole workspace**

Run: `cargo check --workspace --all-targets`
Expected: clean (warnings are acceptable; `error` lines are not).

Then run the affected suites:

Run: `cargo test -p solutions -p solution_agent -p console_panel -p editor_mcp -p workspace_events`
Expected: PASS.

- [ ] **Step 5: Record the decision in `FORK.md`**

Add one numbered entry under "Key architectural decisions":

```markdown
N. **Solution / member / catalog ids are surrogate counters, not slugs.**
   *Why:* the id was a slug of the display name and doubled as a path component
   (`root = <settings.root>/<slug>`, `local_path = root/<catalog_id>`, MCP socket
   at `<runtime>/solutions/<id>/mcp.sock`), so "rename" meant "change the primary
   key" — which is why rename silently degraded to a label-only change and the
   folder drifted from the name forever.
   *How to apply:* address solutions and members by `SolutionId(i64)` /
   `MemberId(i64)` everywhere (MCP tools included); `name` and `local_path`/`root`
   are ordinary mutable columns. A session's project is `solution_sessions
   .member_id`, never an inference from its cwd. Never `INSERT OR REPLACE` into a
   table that is an FK parent with `ON DELETE CASCADE` (see
   `docs/findings/2026-07-13-rename-solution-cascade-data-loss.md`).
```

- [ ] **Step 6: Commit**

```bash
git add crates FORK.md
git commit -m "Convert every solutions consumer to numeric ids"
```

---

### Task 7: Rehearse the migration against the real databases

**Files:**
- Create: `crates/solutions/tests/identity_migration_rehearsal.rs`

**Interfaces:**
- Consumes: `SolutionsDb` (Task 1), `SolutionAgentDb::migrate_identity` (Task 2).
- Produces: nothing consumed by later plans. This task is the acceptance gate for "no data was lost".

- [ ] **Step 1: Write the rehearsal test**

The user's live DBs are at `~/.local/share/sawe/db/…/db.sqlite` (solutions, via `db::static_connection!`) and `~/.local/share/sawe/solution_agent/solution_agent.db`. The test is `#[ignore]`d by default (it needs the operator's real data and must never run in a normal `cargo test`), copies both files to a tempdir, migrates the copies, and asserts nothing was lost.

Create `crates/solutions/tests/identity_migration_rehearsal.rs`:

```rust
//! Dry-run of the identity migration against the operator's REAL databases.
//!
//! Ignored by default: it reads `$SAWE_SOLUTIONS_DB` / `$SAWE_AGENT_DB` (or the
//! default data-dir locations), copies them into a tempdir, and migrates the
//! copies. The originals are opened read-only and never written.
//!
//! Run with:
//!   cargo test -p solutions --test identity_migration_rehearsal -- --ignored --nocapture

use db::sqlez::connection::Connection;
use db::sqlez::domain::Domain;
use solutions::db::SolutionsDb;

fn count(connection: &Connection, table: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let rows = connection
        .select::<i64>(&sql)
        .expect("prepare count")()
    .expect("count");
    rows.into_iter().next().unwrap_or(0)
}

#[test]
#[ignore = "reads the operator's real database"]
fn real_solutions_db_migrates_without_losing_rows() {
    let source = std::env::var("SAWE_SOLUTIONS_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            paths::data_dir()
                .join("db")
                .join("0-stable")
                .join("db.sqlite")
        });
    assert!(
        source.exists(),
        "no solutions DB at {} — set SAWE_SOLUTIONS_DB",
        source.display()
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let copy = dir.path().join("db.sqlite");
    std::fs::copy(&source, &copy).expect("copy the real DB");

    let connection = Connection::open_file(&copy.to_string_lossy());
    let solutions_before = count(&connection, "solutions");
    let members_before = count(&connection, "solution_members");
    let active_before = count(&connection, "active_member");
    let catalog_before = count(&connection, "catalog_projects");
    println!(
        "before: {solutions_before} solution(s), {members_before} member(s), \
         {active_before} active, {catalog_before} catalog"
    );

    connection
        .migrate(
            "SolutionsDb",
            <SolutionsDb as Domain>::MIGRATIONS,
            &mut |_, _, _| false,
        )
        .expect("identity migration must apply to the real DB");

    let report = connection
        .select::<(i64, i64, i64, i64, i64, i64, i64, i64)>(
            "SELECT solutions_before, solutions_after, members_before, members_after,
                    active_before, active_after, catalog_before, catalog_after
             FROM identity_migration_report",
        )
        .expect("prepare report")()
    .expect("report");
    let r = report.into_iter().next().expect("a report row must exist");
    assert_eq!((r.0, r.1), (solutions_before, solutions_before), "solutions");
    assert_eq!((r.2, r.3), (members_before, members_before), "members");
    assert_eq!((r.4, r.5), (active_before, active_before), "active_member");
    assert_eq!((r.6, r.7), (catalog_before, catalog_before), "catalog");

    // Every member must still point at a live solution and carry a name.
    let orphans = connection
        .select::<i64>(
            "SELECT COUNT(*) FROM solution_members m
             LEFT JOIN solutions s ON s.id = m.solution_id
             WHERE s.id IS NULL OR m.name IS NULL OR LENGTH(m.name) = 0",
        )
        .expect("prepare orphan check")()
    .expect("orphan check");
    assert_eq!(orphans, vec![0], "no orphaned or unnamed members");

    let legacy = count(&connection, "solution_legacy_ids");
    assert_eq!(
        legacy, solutions_before,
        "every old slug must be in the cross-DB map"
    );
}
```

- [ ] **Step 2: Run the rehearsal against the real DB**

Run: `cargo test -p solutions --test identity_migration_rehearsal -- --ignored --nocapture`
Expected: PASS, and the printed line reports the operator's real numbers (16 solutions, ~40 members). If the migration drops an `active_member` row (a selection pointing at a member that no longer exists), the report assertion fails and names the table — delete the dangling row from the *real* DB and re-run before shipping.

- [ ] **Step 3: Back up the real databases before the first launch of the new build**

```bash
cp -v ~/.local/share/sawe/db/0-stable/db.sqlite ~/.local/share/sawe/db/0-stable/db.sqlite.pre-identity.bak
cp -v ~/.local/share/sawe/solution_agent/solution_agent.db ~/.local/share/sawe/solution_agent/solution_agent.db.pre-identity.bak
```

(Adjust the paths if `paths::data_dir()` resolves elsewhere on this machine — `script/run-mcp --debug` prints the resolved runtime dir on launch.)

- [ ] **Step 4: Commit**

```bash
git add crates/solutions/tests/identity_migration_rehearsal.rs
git commit -m "Add a dry-run rehearsal of the identity migration against the real DBs"
```
