//! SQLite persistence for the Solutions registry. Replaces the previous
//! solutions.json file. Schema is owned by the Domain impl on
//! SolutionsDb; queries live in impl SolutionsDb blocks.

use db::sqlez::domain::Domain;
use db::sqlez::thread_safe_connection::ThreadSafeConnection;
use db::sqlez_macros::sql;

pub struct SolutionsDb(ThreadSafeConnection);

impl Domain for SolutionsDb {
    const NAME: &str = stringify!(SolutionsDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE catalog_projects (
                id             TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                remote_url     TEXT NOT NULL,
                default_branch TEXT
            );

            CREATE TABLE solutions (
                id             TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                root           TEXT NOT NULL,
                last_opened_at INTEGER
            );

            CREATE TABLE solution_members (
                solution_id  TEXT    NOT NULL REFERENCES solutions(id) ON DELETE CASCADE,
                catalog_id   TEXT    NOT NULL,
                local_path   TEXT    NOT NULL,
                position     INTEGER NOT NULL,
                PRIMARY KEY (solution_id, catalog_id)
            );

            CREATE INDEX idx_solution_members_position
                ON solution_members(solution_id, position);

            CREATE TABLE panel_member_selections (
                solution_id  TEXT NOT NULL REFERENCES solutions(id) ON DELETE CASCADE,
                panel_kind   TEXT NOT NULL,
                catalog_id   TEXT NOT NULL,
                PRIMARY KEY (solution_id, panel_kind)
            );
        ),
        sql!(
            CREATE TABLE active_member (
                solution_id TEXT PRIMARY KEY REFERENCES solutions(id) ON DELETE CASCADE,
                catalog_id  TEXT NOT NULL
            );
            INSERT INTO active_member (solution_id, catalog_id)
                SELECT solution_id, catalog_id FROM panel_member_selections pms
                WHERE panel_kind = (
                    SELECT MAX(panel_kind) FROM panel_member_selections
                    WHERE solution_id = pms.solution_id
                );
            DROP TABLE panel_member_selections;
        ),
        // Identity migration: TEXT slug ids → surrogate INTEGER counters.
        // The legacy tables are renamed aside first — SQLite rewrites the
        // children's FK clauses to follow the renamed parent, so the legacy
        // island stays internally consistent while the new tables take over
        // the canonical names. `solution_legacy_ids` is kept forever: it is
        // the only bridge that lets `solution_agent.db` (a separate file,
        // separate connection, no ATTACH) remap its own solution_id column.
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
    ];
}

db::static_connection!(SolutionsDb, []);

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

    // Old TEXT slug → new counter id, written once by the identity migration.
    // The bridge that lets `solution_agent.db` (a separate connection with no
    // visibility into this file) remap its own `solution_id` column.
    query! {
        pub async fn load_solution_legacy_ids() -> Result<Vec<(String, i64)>> {
            SELECT old_id, new_id FROM solution_legacy_ids
        }
    }

    // Row counts captured inside the identity migration's transaction. A DB
    // created fresh at the numeric schema still runs the migration (over empty
    // legacy tables), so its report reads all zeroes rather than `None`.
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


#[cfg(test)]
mod tests {
    use super::*;
    use db::sqlez::connection::Connection;

    /// Build an in-memory DB at the PRE-identity schema (migrations 0 and 1
    /// only), seed it with rows shaped exactly like the user's production DB,
    /// then apply the full migration list so the identity migration runs over
    /// real legacy data.
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
            solutions.iter().any(|(_, name, root, ts)| name == "Sawe"
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
            .select::<(String, i64)>(
                "SELECT old_id, new_id FROM solution_legacy_ids ORDER BY old_id",
            )
            .expect("prepare select legacy map")()
        .expect("select legacy map");
        assert_eq!(legacy.len(), 2, "the cross-DB map must retain every slug");
        assert!(
            legacy.iter().any(|(old, new)| old == "spk-solutions"
                && Some(*new) == solutions.iter().find(|s| s.1 == "Sawe").map(|s| s.0)),
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
        assert_eq!(
            count,
            vec![2],
            "re-running the migrator must not duplicate rows"
        );
    }

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
        let rows = db
            .load_all_catalog_projects()
            .await
            .expect("reload catalog");
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
        assert_eq!(
            active,
            vec![(sol, m)],
            "active member must survive a member re-save"
        );
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

}
