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
    ];
}

db::static_connection!(SolutionsDb, []);

use db::query;

impl SolutionsDb {
    query! {
        pub async fn save_catalog_project(
            id: String,
            name: String,
            remote_url: String,
            default_branch: Option<String>
        ) -> Result<()> {
            INSERT OR REPLACE INTO catalog_projects (id, name, remote_url, default_branch)
            VALUES (?, ?, ?, ?)
        }
    }

    query! {
        pub async fn delete_catalog_project(id: String) -> Result<()> {
            DELETE FROM catalog_projects WHERE id = ?
        }
    }

    query! {
        pub async fn load_all_catalog_projects()
            -> Result<Vec<(String, String, String, Option<String>)>>
        {
            SELECT id, name, remote_url, default_branch FROM catalog_projects
        }
    }

    // Must be a real UPSERT, not INSERT OR REPLACE: `solutions` is the parent of
    // `solution_members` and `active_member`, both ON DELETE CASCADE. REPLACE
    // deletes the existing parent row before re-inserting it, so re-saving an
    // existing solution (the rename path) would cascade-delete all of its
    // members and its active member.
    query! {
        pub async fn save_solution(
            id: String,
            name: String,
            root: String,
            last_opened_at: Option<i64>
        ) -> Result<()> {
            INSERT INTO solutions (id, name, root, last_opened_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                root = excluded.root,
                last_opened_at = excluded.last_opened_at
        }
    }

    query! {
        pub async fn delete_solution_row(id: String) -> Result<()> {
            DELETE FROM solutions WHERE id = ?
        }
    }

    query! {
        pub async fn update_last_opened(id: String, last_opened_at: i64) -> Result<()> {
            UPDATE solutions SET last_opened_at = ?2 WHERE id = ?1
        }
    }

    query! {
        pub async fn set_solution_member(
            solution_id: String,
            catalog_id: String,
            local_path: String,
            position: i32
        ) -> Result<()> {
            INSERT OR REPLACE INTO solution_members (solution_id, catalog_id, local_path, position)
            VALUES (?, ?, ?, ?)
        }
    }

    query! {
        pub async fn delete_solution_member(
            solution_id: String,
            catalog_id: String
        ) -> Result<()> {
            DELETE FROM solution_members WHERE solution_id = ? AND catalog_id = ?
        }
    }

    // LEFT JOIN keeps solutions with zero members in the result. NULL
    // member columns are read as empty strings / 0 by sqlez's String/i32
    // Column impls (column_text returns "" on NULL, column_int returns
    // 0). Caller groups by solution id and treats catalog_id == "" as
    // "no member row". COALESCE in the SELECT would be cleaner, but the
    // sqlez_macros sql! proc-macro tokenises with the Rust lexer and
    // rejects empty single-quoted SQL string literals as empty char
    // literals — so we let sqlez handle the NULL coercion instead.
    query! {
        pub async fn load_all_solutions_with_members()
            -> Result<Vec<(
                String,
                String,
                String,
                Option<i64>,
                String,
                String,
                i32
            )>>
        {
            SELECT s.id, s.name, s.root, s.last_opened_at,
                   m.catalog_id, m.local_path, m.position
            FROM solutions s
            LEFT JOIN solution_members m ON m.solution_id = s.id
            ORDER BY s.id, m.position
        }
    }

    query! {
        pub async fn set_active_member(solution_id: String, catalog_id: String) -> Result<()> {
            INSERT OR REPLACE INTO active_member (solution_id, catalog_id)
            VALUES (?, ?)
        }
    }

    query! {
        pub async fn load_all_active_members() -> Result<Vec<(String, String)>> {
            SELECT solution_id, catalog_id FROM active_member
        }
    }

    query! {
        pub async fn clear_active_member(solution_id: String) -> Result<()> {
            DELETE FROM active_member WHERE solution_id = ?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn open_test_db_applies_migration() {
        let db = SolutionsDb::open_test_db("solutions_db_open_test").await;
        db.write(|conn| {
            conn.exec("INSERT INTO catalog_projects (id, name, remote_url) VALUES ('x', 'X', 'u')")
                .unwrap()()
            .unwrap();
            conn.exec("DELETE FROM catalog_projects WHERE id = 'x'")
                .unwrap()()
            .unwrap();
        })
        .await;
    }

    #[gpui::test]
    async fn catalog_save_and_load_roundtrips() {
        let db = SolutionsDb::open_test_db("solutions_db_catalog_roundtrip").await;
        db.save_catalog_project(
            "a".into(),
            "Alpha".into(),
            "git@a:a.git".into(),
            Some("main".into()),
        )
        .await
        .unwrap();
        db.save_catalog_project("b".into(), "Beta".into(), "git@b:b.git".into(), None)
            .await
            .unwrap();

        let mut rows = db.load_all_catalog_projects().await.unwrap();
        rows.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(
            rows,
            vec![
                (
                    "a".into(),
                    "Alpha".into(),
                    "git@a:a.git".into(),
                    Some("main".into())
                ),
                ("b".into(), "Beta".into(), "git@b:b.git".into(), None),
            ]
        );

        db.delete_catalog_project("a".into()).await.unwrap();
        let rows = db.load_all_catalog_projects().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "b");
    }

    #[gpui::test]
    async fn solution_with_members_roundtrips() {
        let db = SolutionsDb::open_test_db("solutions_db_solution_roundtrip").await;
        db.save_solution(
            "sol-1".into(),
            "Alpha".into(),
            "/tmp/sol-1".into(),
            Some(1_700_000_000_000),
        )
        .await
        .unwrap();
        db.set_solution_member("sol-1".into(), "cat-a".into(), "/tmp/sol-1/cat-a".into(), 0)
            .await
            .unwrap();
        db.set_solution_member("sol-1".into(), "cat-b".into(), "/tmp/sol-1/cat-b".into(), 1)
            .await
            .unwrap();

        let rows = db.load_all_solutions_with_members().await.unwrap();
        assert_eq!(rows.len(), 2);
        let cat_ids: Vec<String> = rows.iter().map(|r| r.4.clone()).collect();
        assert_eq!(cat_ids, vec!["cat-a", "cat-b"]);

        db.delete_solution_member("sol-1".into(), "cat-a".into())
            .await
            .unwrap();
        let rows = db.load_all_solutions_with_members().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].4, "cat-b");

        db.delete_solution_row("sol-1".into()).await.unwrap();
        let rows = db.load_all_solutions_with_members().await.unwrap();
        assert!(rows.is_empty());
    }

    #[gpui::test]
    async fn solution_with_no_members_still_returned() {
        let db = SolutionsDb::open_test_db("solutions_db_solution_empty").await;
        db.save_solution("empty".into(), "E".into(), "/x/empty".into(), None)
            .await
            .unwrap();
        let rows = db.load_all_solutions_with_members().await.unwrap();
        // The LEFT JOIN should yield exactly one row with empty member columns.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "empty");
        assert!(rows[0].4.is_empty());
    }

    // `rename_solution` re-saves an existing solution row. If save_solution
    // deleted the parent row first (INSERT OR REPLACE), the ON DELETE CASCADE
    // on solution_members / active_member would silently wipe them.
    #[gpui::test]
    async fn resaving_solution_preserves_members_and_active_member() {
        let db = SolutionsDb::open_test_db("solutions_db_resave_preserves_children").await;
        db.save_solution(
            "sol-1".into(),
            "Alpha".into(),
            "/tmp/sol-1".into(),
            Some(1_700_000_000_000),
        )
        .await
        .unwrap();
        db.set_solution_member("sol-1".into(), "cat-a".into(), "/tmp/sol-1/cat-a".into(), 0)
            .await
            .unwrap();
        db.set_solution_member("sol-1".into(), "cat-b".into(), "/tmp/sol-1/cat-b".into(), 1)
            .await
            .unwrap();
        db.set_active_member("sol-1".into(), "cat-b".into())
            .await
            .unwrap();

        db.save_solution(
            "sol-1".into(),
            "Renamed".into(),
            "/tmp/sol-1".into(),
            Some(1_700_000_000_000),
        )
        .await
        .unwrap();

        let rows = db.load_all_solutions_with_members().await.unwrap();
        assert_eq!(rows.len(), 2, "members must survive a re-save: {rows:?}");
        assert_eq!(rows[0].1, "Renamed");
        assert_eq!(rows[0].3, Some(1_700_000_000_000));
        let cat_ids: Vec<String> = rows.iter().map(|r| r.4.clone()).collect();
        assert_eq!(cat_ids, vec!["cat-a", "cat-b"]);

        let active = db.load_all_active_members().await.unwrap();
        assert_eq!(
            active,
            vec![("sol-1".to_string(), "cat-b".to_string())],
            "active member must survive a re-save"
        );
    }

    #[gpui::test]
    async fn active_member_roundtrips() {
        let db = SolutionsDb::open_test_db("active_member_roundtrips").await;
        db.save_solution("s1".into(), "S1".into(), "/tmp/s1".into(), None)
            .await
            .unwrap();
        db.set_active_member("s1".into(), "cat-a".into())
            .await
            .unwrap();
        db.set_active_member("s1".into(), "cat-b".into())
            .await
            .unwrap();
        let rows = db.load_all_active_members().await.unwrap();
        assert_eq!(rows, vec![("s1".to_string(), "cat-b".to_string())]);
    }
}
