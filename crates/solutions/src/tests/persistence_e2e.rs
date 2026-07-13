use crate::db::SolutionsDb;
use gpui::TestAppContext;

#[gpui::test]
async fn store_loads_catalog_and_solutions_from_db(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let db = SolutionsDb::open_test_db("solutions_store_e2e_init").await;
    let cat = db
        .insert_catalog_project("Cat A".into(), "git@x:a".into(), None)
        .await
        .expect("insert catalog");
    let sol = db
        .insert_solution("Sol 1".into(), "/tmp/s1".into(), None)
        .await
        .expect("insert solution");
    let member = db
        .insert_solution_member(sol, "cat-a".into(), "/tmp/s1/cat-a".into(), 0, Some(cat))
        .await
        .expect("insert member");

    let db_for_init = db.clone();
    cx.update(|cx| {
        crate::store::SolutionStore::init_global_for_test(db_for_init, cx);
        let store = crate::store::SolutionStore::global(cx);
        store.read_with(cx, |s, _| {
            assert_eq!(s.catalog().len(), 1);
            assert_eq!(s.catalog()[0].id.0, cat);
            assert_eq!(s.solutions().len(), 1);
            assert_eq!(s.solutions()[0].id.0, sol);
            assert_eq!(s.solutions()[0].members.len(), 1);
            assert_eq!(s.solutions()[0].members[0].id.0, member);
            assert_eq!(s.solutions()[0].members[0].name, "cat-a");
            assert_eq!(
                s.solutions()[0].members[0].origin_catalog_id.map(|c| c.0),
                Some(cat)
            );
        });
    });
}

#[gpui::test]
async fn add_catalog_project_persists_to_db(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SolutionsDb::open_test_db("solutions_store_e2e_add_catalog").await;
    let db_for_init = db.clone();
    cx.update(|cx| {
        crate::store::SolutionStore::init_global_for_test(db_for_init, cx);
        let store = crate::store::SolutionStore::global(cx);
        store.update(cx, |s, cx| {
            s.add_catalog_project("Foo", "git@x:foo.git", Some("main".into()), cx)
                .expect("add catalog");
        });
    });

    let rows = db.load_all_catalog_projects().await.expect("load catalog");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "Foo");
    assert!(rows[0].0 > 0, "the DB must have allocated a counter id");
}

#[gpui::test]
async fn create_and_remove_solution_persists(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SolutionsDb::open_test_db("solutions_store_e2e_create_remove").await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_for_init = db.clone();
    cx.update(|cx| {
        crate::store::SolutionStore::init_global_for_test(db_for_init, cx);
        let store = crate::store::SolutionStore::global(cx);
        let id = store.update(cx, |s, cx| {
            s.create_solution("My Sol", tmp.path().to_path_buf(), cx)
                .expect("create solution")
        });
        store.update(cx, |s, cx| {
            s.touch_last_opened(id, cx).expect("touch");
            s.delete_solution(id, cx).expect("delete");
        });
    });

    let rows = db
        .load_all_solutions_with_members()
        .await
        .expect("load solutions");
    assert!(rows.is_empty(), "delete_solution should remove the row");
}

#[gpui::test]
async fn touch_last_opened_persists_timestamp(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SolutionsDb::open_test_db("solutions_store_e2e_touch_last").await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_for_init = db.clone();
    cx.update(|cx| {
        crate::store::SolutionStore::init_global_for_test(db_for_init, cx);
        let store = crate::store::SolutionStore::global(cx);
        let id = store.update(cx, |s, cx| {
            s.create_solution("S", tmp.path().to_path_buf(), cx)
                .expect("create solution")
        });
        store.update(cx, |s, cx| s.touch_last_opened(id, cx).expect("touch"));
    });
    let rows = db
        .load_all_solutions_with_members()
        .await
        .expect("load solutions");
    assert!(
        rows.iter().any(|r| r.3.is_some()),
        "last_opened_at should be set"
    );
}

#[gpui::test]
async fn set_active_member_persists_and_emits(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SolutionsDb::open_test_db("solutions_store_e2e_active_member").await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_for_init = db.clone();
    let (sol_id, member_id) = cx.update(|cx| {
        crate::store::SolutionStore::init_global_for_test(db_for_init, cx);
        let store = crate::store::SolutionStore::global(cx);
        store.update(cx, |s, cx| {
            let sol_id = s
                .create_solution("S", tmp.path().to_path_buf(), cx)
                .expect("create solution");
            let member_id = s
                .add_empty_member(sol_id, "Proj", cx)
                .expect("add empty member");
            (sol_id, member_id)
        })
    });
    cx.update(|cx| {
        let store = crate::store::SolutionStore::global(cx);
        store.update(cx, |s, cx| s.set_active_member(sol_id, member_id, cx));
    });
    // The DB write happens on a background task; let it complete.
    cx.run_until_parked();

    let rows = db
        .load_all_active_members()
        .await
        .expect("load active members");
    assert_eq!(rows, vec![(sol_id.0, member_id.0)]);
}

use std::fs;

#[gpui::test]
async fn migration_imports_old_json_once(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let tmp = tempfile::tempdir().expect("tempdir");
    let json_path = tmp.path().join("solutions.json");

    let old = serde_json::json!({
        "version": 1,
        "catalog": [
            { "id": "cat-a", "name": "Cat A", "remote_url": "git@x:a", "default_branch": "main" }
        ],
        "solutions": [
            {
                "id": "sol-1",
                "name": "Sol One",
                "root": "/tmp/sol-1",
                "members": [
                    { "catalog_id": "cat-a", "local_path": "/tmp/sol-1/cat-a" }
                ]
            }
        ]
    });
    fs::write(
        &json_path,
        serde_json::to_string_pretty(&old).expect("serialize"),
    )
    .expect("write json");

    let db = SolutionsDb::open_test_db("solutions_migrate_imports").await;
    crate::migrate::run_one_time_migration(&db, &json_path).expect("import");

    let cat = db.load_all_catalog_projects().await.expect("load catalog");
    assert_eq!(cat.len(), 1);
    let sol = db
        .load_all_solutions_with_members()
        .await
        .expect("load solutions");
    assert_eq!(sol.len(), 1);
    // The old member's `catalog_id` slug becomes the member's name, and its
    // provenance points at the freshly-allocated catalog counter.
    assert_eq!(sol[0].5, "cat-a");
    assert_eq!(sol[0].8, Some(cat[0].0));

    assert!(!json_path.exists());
    let bak = tmp.path().join("solutions.json.migrated.bak");
    assert!(bak.exists());

    crate::migrate::run_one_time_migration(&db, &json_path).expect("second run is a no-op");
    let cat = db.load_all_catalog_projects().await.expect("reload catalog");
    assert_eq!(cat.len(), 1);
}

#[gpui::test]
async fn active_member_persists_across_reinit(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SolutionsDb::open_test_db("solutions_active_member_reinit").await;
    let tmp = tempfile::tempdir().expect("tempdir");

    let db_first = db.clone();
    let (sol_id, member_id) = cx.update(|cx| {
        crate::store::SolutionStore::init_global_for_test(db_first, cx);
        let store = crate::store::SolutionStore::global(cx);
        let ids = store.update(cx, |s, cx| {
            let sol_id = s
                .create_solution("S", tmp.path().to_path_buf(), cx)
                .expect("create solution");
            let member_id = s
                .add_empty_member(sol_id, "Proj", cx)
                .expect("add empty member");
            s.set_active_member(sol_id, member_id, cx);
            (sol_id, member_id)
        });
        cx.remove_global::<crate::store::GlobalSolutionStore>();
        ids
    });

    // Allow the background DB write spawned by set_active_member to complete.
    cx.run_until_parked();

    cx.update(|cx| {
        crate::store::SolutionStore::init_global_for_test(db, cx);
        let store = crate::store::SolutionStore::global(cx);
        store.read_with(cx, |s, _| {
            assert_eq!(s.active_member(sol_id), Some(member_id));
        });
    });
}

#[gpui::test]
async fn full_lifecycle_persists_across_reinit(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SolutionsDb::open_test_db("solutions_full_lifecycle").await;
    let tmp = tempfile::tempdir().expect("tempdir");

    let db_first = db.clone();
    let cat_id = cx.update(|cx| {
        crate::store::SolutionStore::init_global_for_test(db_first, cx);
        let store = crate::store::SolutionStore::global(cx);
        let cat_id = store.update(cx, |s, cx| {
            let cat_id = s
                .add_catalog_project("Cat", "git@x:c.git", None, cx)
                .expect("add catalog");
            let sol_id = s
                .create_solution("Sol", tmp.path().to_path_buf(), cx)
                .expect("create solution");
            s.add_empty_member(sol_id, "c", cx).expect("add member");
            cat_id
        });
        cx.remove_global::<crate::store::GlobalSolutionStore>();
        cat_id
    });

    cx.update(|cx| {
        crate::store::SolutionStore::init_global_for_test(db, cx);
        let store = crate::store::SolutionStore::global(cx);
        store.read_with(cx, |s, _| {
            assert_eq!(s.catalog().len(), 1);
            assert_eq!(s.catalog()[0].id, cat_id);
            assert_eq!(s.solutions().len(), 1);
            assert_eq!(s.solutions()[0].members.len(), 1);
            assert_eq!(s.solutions()[0].members[0].name, "c");
        });
    });
}
