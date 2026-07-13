use context_server::listener::McpServerTool;
    use super::*;
    use crate::SolutionStore;
    use gpui::TestAppContext;
    use tempfile::tempdir;

    #[gpui::test]
    async fn list_returns_empty_when_store_empty(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store, cx));

        let response = cx
            .update(|cx| {
                let tool = ListSolutionsTool;
                cx.spawn(async move |cx| tool.run(ListSolutionsParams {}, cx).await)
            })
            .await
            .expect("run task");

        assert_eq!(response.structured_content.solutions.len(), 0);
    }

    #[gpui::test]
    async fn list_returns_created_solutions(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        store
            .update(cx, |s, cx| {
                s.create_solution("Test Sol", dir.path().to_path_buf(), cx)
            })
            .expect("create");

        let response = cx
            .update(|cx| {
                let tool = ListSolutionsTool;
                cx.spawn(async move |cx| tool.run(ListSolutionsParams {}, cx).await)
            })
            .await
            .expect("run task");

        let arr = response.structured_content.solutions;
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].name, "Test Sol");
        assert_eq!(arr[0].member_count, 0);
        assert!(!arr[0].open);
    }

    #[test]
    fn list_params_deserialize_from_null() {
        let _: ListSolutionsParams = serde_json::from_value(serde_json::Value::Null).expect("null");
    }

    #[test]
    fn get_params_round_trip() {
        let p: GetSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
    }

    #[test]
    fn get_params_accepts_null() {
        let p: GetSolutionParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
    }

    #[test]
    fn create_params_round_trip() {
        let p: CreateSolutionParams =
            serde_json::from_value(serde_json::json!({"name": "Demo"})).expect("parse");
        assert_eq!(p.name, "Demo");
    }

    #[test]
    fn create_params_accepts_null() {
        let p: CreateSolutionParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.name.is_empty());
    }

    #[test]
    fn rename_params_round_trip() {
        let p: RenameSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": 3,
            "new_name": "Renamed"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 3);
        assert_eq!(p.new_name, "Renamed");
    }

    #[test]
    fn delete_params_round_trip() {
        let p: DeleteSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": 3
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 3);
    }

    #[test]
    fn open_params_with_focus() {
        let p: OpenSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": 3,
            "focus": false
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 3);
        assert_eq!(p.focus, Some(false));
    }

    #[test]
    fn close_params_round_trip() {
        let p: CloseSolutionParams = serde_json::from_value(serde_json::json!({
            "solution_id": 3
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 3);
    }

    // NOTE: live-runner test for `solutions.create` requires a `SettingsStore`
    // (the tool reads `root` from `SolutionsSettings::get_global`). Setting
    // that up here is gnarly; the create path is exercised end-to-end in the
    // Phase 8 integration tests where a real editor `App` is available.
    // `rename` and `delete` go through the store directly and need no
    // settings, so we cover them here.

    #[gpui::test]
    async fn rename_solution_updates_store(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("Original", dir.path().to_path_buf(), cx)
            })
            .expect("create");

        let response = cx
            .update(|cx| {
                let tool = RenameSolutionTool;
                let id = sol_id.0;
                cx.spawn(async move |cx| {
                    tool.run(
                        RenameSolutionParams {
                            solution_id: id,
                            new_name: "New Name".into(),
                        },
                        cx,
                    )
                    .await
                })
            })
            .await
            .expect("run task");

        assert_eq!(response.structured_content.solution_id, sol_id.0);

        let new_name = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == sol_id)
                .map(|sol| sol.name.clone())
        });
        assert_eq!(new_name, Some("New Name".to_string()));
    }

    #[gpui::test]
    async fn delete_solution_removes_from_store(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("Demo", dir.path().to_path_buf(), cx)
            })
            .expect("create");

        let response = cx
            .update(|cx| {
                let tool = DeleteSolutionTool;
                let id = sol_id.0;
                cx.spawn(async move |cx| {
                    tool.run(DeleteSolutionParams { solution_id: id }, cx).await
                })
            })
            .await
            .expect("run task");

        assert!(response.structured_content.deleted);
        let count = store.read_with(cx, |s, _| s.solutions().len());
        assert_eq!(count, 0);
    }

    #[test]
    fn list_catalog_params_accepts_null() {
        let _: ListCatalogParams = serde_json::from_value(serde_json::Value::Null).expect("null");
    }

    #[test]
    fn add_catalog_params_round_trip() {
        let p: AddCatalogProjectParams = serde_json::from_value(serde_json::json!({
            "name": "Demo",
            "remote_url": "git@example.com:demo.git",
            "default_branch": "main"
        }))
        .expect("parse");
        assert_eq!(p.name, "Demo");
        assert_eq!(p.remote_url, "git@example.com:demo.git");
        assert_eq!(p.default_branch.as_deref(), Some("main"));
    }

    #[test]
    fn add_catalog_params_accepts_null() {
        let p: AddCatalogProjectParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.name.is_empty());
        assert!(p.remote_url.is_empty());
        assert!(p.default_branch.is_none());
    }

    #[test]
    fn remove_catalog_params_round_trip() {
        let p: RemoveCatalogProjectParams = serde_json::from_value(serde_json::json!({
            "catalog_id": 5
        }))
        .expect("parse");
        assert_eq!(p.catalog_id, 5);
    }

    #[test]
    fn edit_catalog_params_partial() {
        let p: EditCatalogProjectParams = serde_json::from_value(serde_json::json!({
            "catalog_id": 5,
            "name": "Renamed"
        }))
        .expect("parse");
        assert_eq!(p.catalog_id, 5);
        assert_eq!(p.name.as_deref(), Some("Renamed"));
        assert!(p.default_branch.is_none());
    }

    #[test]
    fn refresh_cache_params_round_trip() {
        let p: RefreshCacheParams = serde_json::from_value(serde_json::json!({
            "catalog_id": 5
        }))
        .expect("parse");
        assert_eq!(p.catalog_id, 5);
    }

    #[gpui::test]
    async fn add_catalog_project_persists(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        let response = cx
            .update(|cx| {
                let tool = AddCatalogProjectTool;
                cx.spawn(async move |cx| {
                    tool.run(
                        AddCatalogProjectParams {
                            name: "Demo".into(),
                            remote_url: "git@example.com:demo.git".into(),
                            default_branch: Some("main".into()),
                        },
                        cx,
                    )
                    .await
                })
            })
            .await
            .expect("run task");

        assert!(
            response.structured_content.catalog_id > 0,
            "a counter id must be allocated"
        );
        let count = store.read_with(cx, |s, _| s.catalog().len());
        assert_eq!(count, 1);
    }

    #[test]
    fn add_member_params_round_trip() {
        let p: AddMemberParams = serde_json::from_value(serde_json::json!({
            "solution_id": 1,
            "catalog_id": 2
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 1);
        assert_eq!(p.catalog_id, 2);
    }

    #[test]
    fn remove_member_params_accepts_null() {
        let p: RemoveMemberParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.member_id, 0);
    }

    #[test]
    fn reorder_members_params_round_trip() {
        let p: ReorderMembersParams = serde_json::from_value(serde_json::json!({
            "solution_id": 1,
            "member_ids": [3, 1, 2]
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 1);
        assert_eq!(p.member_ids, vec![3, 1, 2]);
    }

    #[gpui::test]
    async fn remove_member_updates_store(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));

        let cat_id = store
            .update(cx, |s, cx| {
                s.add_catalog_project("Demo", "git@x:demo.git", None, cx)
            })
            .expect("add catalog");
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("Sol", dir.path().to_path_buf(), cx)
            })
            .expect("create");
        let member_id = store.update(cx, |s, _| s.test_force_add_member(sol_id, cat_id));

        let count_before = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == sol_id)
                .map(|sol| sol.members.len())
                .unwrap_or(0)
        });
        assert_eq!(count_before, 1);

        let response = cx
            .update(|cx| {
                let tool = RemoveMemberTool;
                let member_id = member_id.0;
                cx.spawn(async move |cx| {
                    tool.run(RemoveMemberParams { member_id }, cx).await
                })
            })
            .await
            .expect("run task");

        assert!(response.structured_content.removed);
        let count_after = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|sol| sol.id == sol_id)
                .map(|sol| sol.members.len())
                .unwrap_or(0)
        });
        assert_eq!(count_after, 0);
    }

    #[test]
    fn list_buffers_params_round_trip() {
        let p: ListBuffersParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
    }

    #[test]
    fn list_buffers_params_accepts_null() {
        let p: ListBuffersParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
    }

    #[test]
    fn get_effective_settings_params_round_trip() {
        let p: GetEffectiveSettingsParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "src/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.path.as_deref(), Some("src/foo.rs"));
    }

    #[test]
    fn get_effective_settings_params_accepts_null() {
        let p: GetEffectiveSettingsParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_none());
    }

    #[test]
    fn dispatch_action_params_with_args() {
        let p: DispatchActionParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "action_name": "workspace::ToggleLeftDock",
            "args": null
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.action_name, "workspace::ToggleLeftDock");
    }

    #[test]
    fn dispatch_action_params_accepts_null() {
        let p: DispatchActionParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.action_name.is_empty());
    }

    #[test]
    fn screenshot_params_round_trip() {
        let p: ScreenshotParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "format": "jpeg",
            "quality": 75,
            "max_dimension": 1280
        }))
        .expect("parse");
        assert_eq!(p.solution_id, Some(7));
        assert_eq!(p.format.as_deref(), Some("jpeg"));
        assert_eq!(p.quality, Some(75));
        assert_eq!(p.max_dimension, Some(1280));
    }

    #[test]
    fn screenshot_params_by_window_id() {
        let p: ScreenshotParams = serde_json::from_value(serde_json::json!({
            "window_id": "window:3",
            "format": "png"
        }))
        .expect("parse");
        assert!(p.solution_id.is_none());
        assert_eq!(p.window_id.as_deref(), Some("window:3"));
        assert_eq!(p.format.as_deref(), Some("png"));
    }

    #[test]
    fn screenshot_params_accepts_null() {
        let p: ScreenshotParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert!(p.solution_id.is_none());
        assert!(p.window_id.is_none());
        assert!(p.format.is_none());
        assert!(p.quality.is_none());
        assert!(p.max_dimension.is_none());
    }

    #[test]
    fn dump_visual_params_round_trip() {
        let p: DumpVisualStructureParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
    }

    #[test]
    fn dump_visual_params_accepts_null() {
        let p: DumpVisualStructureParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
    }

    #[test]
    fn diagnostics_params_round_trip() {
        let p: GetDiagnosticsParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "buffer_path": "src/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.buffer_path.as_deref(), Some("src/foo.rs"));
    }

    #[test]
    fn diagnostics_params_accepts_null() {
        let p: GetDiagnosticsParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.buffer_path.is_none());
    }

    #[test]
    fn list_files_params_round_trip() {
        let p: ListFilesParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "glob": "**/*.rs",
            "scope": "first_worktree",
            "max": 50
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.glob.as_deref(), Some("**/*.rs"));
        assert_eq!(p.scope.as_deref(), Some("first_worktree"));
        assert_eq!(p.max, Some(50));
    }

    #[test]
    fn list_files_params_accepts_null() {
        let p: ListFilesParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.glob.is_none());
        assert!(p.scope.is_none());
        assert!(p.cursor.is_none());
        assert!(p.max.is_none());
    }

    #[gpui::test]
    async fn validate_path_rejects_relative(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store, cx));
        let result = cx.update(|cx| validate_path_in_solution(1, "relative/path.rs", cx));
        assert!(matches!(result, Err(PathValidationError::InvalidPath)));
    }

    #[gpui::test]
    async fn validate_path_rejects_unknown_solution(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store, cx));
        let result = cx.update(|cx| validate_path_in_solution(999_999, "/tmp/foo", cx));
        assert!(matches!(result, Err(PathValidationError::SolutionNotFound)));
    }

    #[gpui::test]
    async fn validate_path_rejects_outside_solution(cx: &mut TestAppContext) {
        let dir = tempdir().expect("tempdir");
        let store = cx.update(|cx| SolutionStore::for_test(dir.path().join("c.json"), cx));
        cx.update(|cx| crate::store::install_global_for_test(store.clone(), cx));
        let sol_id = store
            .update(cx, |s, cx| {
                s.create_solution("Sol", dir.path().to_path_buf(), cx)
            })
            .expect("create");
        let result = cx.update(|cx| validate_path_in_solution(sol_id.0, "/etc/passwd", cx));
        assert!(matches!(
            result,
            Err(PathValidationError::PathOutsideSolution)
        ));
    }

    #[test]
    fn read_buffer_params_round_trip() {
        let p: ReadBufferParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "/abs/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.path, "/abs/foo.rs");
    }

    #[test]
    fn read_buffer_params_accepts_null() {
        let p: ReadBufferParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_empty());
    }

    #[test]
    fn apply_edit_params_round_trip() {
        let p: ApplyEditParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "/abs/foo.rs",
            "edits": [{
                "range": {"start": {"line": 0, "col": 0}, "end": {"line": 0, "col": 5}},
                "new_text": "hello"
            }]
        }))
        .expect("parse");
        assert_eq!(p.edits.len(), 1);
        assert_eq!(p.edits[0].new_text, "hello");
        assert_eq!(p.edits[0].range.start.line, 0);
        assert_eq!(p.edits[0].range.end.col, 5);
    }

    #[test]
    fn apply_edit_params_accepts_null() {
        let p: ApplyEditParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_empty());
        assert!(p.edits.is_empty());
    }

    #[test]
    fn save_buffer_params_round_trip() {
        let p: SaveBufferParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "/abs/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.path, "/abs/foo.rs");
    }

    #[test]
    fn save_buffer_params_accepts_null() {
        let p: SaveBufferParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_empty());
    }

    #[test]
    fn open_file_params_round_trip() {
        let p: OpenFileParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "/abs/foo.rs",
            "focus": false
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.focus, Some(false));
    }

    #[test]
    fn open_file_params_accepts_null() {
        let p: OpenFileParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_empty());
        assert!(p.focus.is_none());
    }

    #[test]
    fn close_buffer_params_round_trip() {
        let p: CloseBufferParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "/abs/foo.rs",
            "save": true
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.save, Some(true));
    }

    #[test]
    fn close_buffer_params_accepts_null() {
        let p: CloseBufferParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_empty());
        assert!(p.save.is_none());
    }

    #[test]
    fn create_file_params_round_trip() {
        let p: CreateFileParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "/abs/foo.rs",
            "content": "hello"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.content.as_deref(), Some("hello"));
    }

    #[test]
    fn create_file_params_accepts_null() {
        let p: CreateFileParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_empty());
        assert!(p.content.is_none());
    }

    #[test]
    fn delete_file_params_round_trip() {
        let p: DeleteFileParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "/abs/foo.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.path, "/abs/foo.rs");
    }

    #[test]
    fn delete_file_params_accepts_null() {
        let p: DeleteFileParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_empty());
    }

    #[test]
    fn rename_file_params_round_trip() {
        let p: RenameFileParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "from": "/abs/old.rs",
            "to": "/abs/new.rs"
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.from, "/abs/old.rs");
        assert_eq!(p.to, "/abs/new.rs");
    }

    #[test]
    fn rename_file_params_accepts_null() {
        let p: RenameFileParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.from.is_empty());
        assert!(p.to.is_empty());
    }

    #[test]
    fn find_in_buffers_params_round_trip() {
        let p: FindInBuffersParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "query": "TODO",
            "case_sensitive": true,
            "regex": false,
            "scope": "all_files",
            "file_glob": "**/*.rs",
            "cursor": "/tmp|src/foo.rs",
            "max": 50
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.query, "TODO");
        assert_eq!(p.case_sensitive, Some(true));
        assert_eq!(p.regex, Some(false));
        assert_eq!(p.scope.as_deref(), Some("all_files"));
        assert_eq!(p.file_glob.as_deref(), Some("**/*.rs"));
        assert_eq!(p.cursor.as_deref(), Some("/tmp|src/foo.rs"));
        assert_eq!(p.max, Some(50));
    }

    #[test]
    fn find_in_buffers_params_accepts_null() {
        let p: FindInBuffersParams = serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.query.is_empty());
        assert!(p.case_sensitive.is_none());
        assert!(p.regex.is_none());
        assert!(p.scope.is_none());
        assert!(p.file_glob.is_none());
        assert!(p.cursor.is_none());
        assert!(p.max.is_none());
    }

    #[test]
    fn goto_definition_params_round_trip() {
        let p: GotoDefinitionParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "/abs/foo.rs",
            "line": 12,
            "col": 4
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.line, 12);
        assert_eq!(p.col, 4);
    }

    #[test]
    fn goto_definition_params_accepts_null() {
        let p: GotoDefinitionParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_empty());
        assert_eq!(p.line, 0);
        assert_eq!(p.col, 0);
    }

    #[test]
    fn find_references_params_round_trip() {
        let p: FindReferencesParams = serde_json::from_value(serde_json::json!({
            "solution_id": 7,
            "path": "/abs/foo.rs",
            "line": 7,
            "col": 9,
            "include_declaration": true
        }))
        .expect("parse");
        assert_eq!(p.solution_id, 7);
        assert_eq!(p.path, "/abs/foo.rs");
        assert_eq!(p.line, 7);
        assert_eq!(p.col, 9);
        assert_eq!(p.include_declaration, Some(true));
    }

    #[test]
    fn find_references_params_accepts_null() {
        let p: FindReferencesParams =
            serde_json::from_value(serde_json::Value::Null).expect("null");
        assert_eq!(p.solution_id, 0);
        assert!(p.path.is_empty());
        assert_eq!(p.line, 0);
        assert_eq!(p.col, 0);
        assert!(p.include_declaration.is_none());
    }
