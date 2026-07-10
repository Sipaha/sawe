    use super::*;

    use agent_client_protocol::schema as acp;
    use chrono::{TimeZone, Utc};
    use gpui::SharedString;
    use solutions::SolutionId;

    use crate::model::{SolutionSessionId, SolutionSessionMetadata};

    #[gpui::test]
    async fn supervisor_state_roundtrips(cx: &mut gpui::TestAppContext) {
        use crate::supervisor::{StoppedReason, SupervisorState, SupervisorStatus};
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let id = crate::model::SolutionSessionId::parse("zzzz9999").unwrap();
        let mut st = SupervisorState::new(id);
        st.enabled = true;
        st.custom_prompt = Some("don't stop before tests pass".into());
        st.consecutive_continues = 3;
        st.next_eligible_ms = Some(1_700_000_000_000);
        st.status = SupervisorStatus::Watching;
        db.save_supervisor_state(st.clone()).await.unwrap();

        // overwrite (upsert)
        st.consecutive_continues = 4;
        st.next_eligible_ms = Some(1_700_000_999_000);
        st.status = SupervisorStatus::Stopped(StoppedReason::Quota);
        db.save_supervisor_state(st).await.unwrap();

        let all = db.load_supervisor_states().await.unwrap();
        let got = all.iter().find(|s| s.session_id == id).unwrap();
        assert!(got.enabled);
        assert_eq!(got.consecutive_continues, 4);
        assert_eq!(
            got.custom_prompt.as_deref(),
            Some("don't stop before tests pass")
        );
        assert_eq!(got.status, SupervisorStatus::Stopped(StoppedReason::Quota));
        assert_eq!(got.next_eligible_ms, Some(1_700_000_999_000));
    }

    // A row persisted mid-`Judging` is a phantom after a restart: the judge
    // lives only in the transient `judge_sessions` map and never survives.
    // Loading it as `Judging` would wedge the status row at "reviewing" (no
    // judge to finish it, and `supersede_judge_on_user_reply` no-ops because
    // the map is empty). `load_supervisor_states` must coerce it back to
    // `Watching` and drop the stale `last_fired_at`.
    #[gpui::test]
    async fn judging_status_coerced_to_watching_on_load(cx: &mut gpui::TestAppContext) {
        use crate::supervisor::{SupervisorState, SupervisorStatus};
        let db = SolutionAgentDb::open(cx.executor()).unwrap();

        let id = crate::model::SolutionSessionId::parse("aaaa1111").unwrap();
        let mut st = SupervisorState::new(id);
        st.enabled = true;
        st.status = SupervisorStatus::Judging;
        st.last_fired_at = Some(1_700_000_000_000);
        db.save_supervisor_state(st).await.unwrap();

        let all = db.load_supervisor_states().await.unwrap();
        let got = all.iter().find(|s| s.session_id == id).unwrap();
        assert_eq!(got.status, SupervisorStatus::Watching);
        assert_eq!(got.last_fired_at, None);
        // A non-Judging status is preserved verbatim (only the phantom is coerced).
        assert!(got.enabled);
    }

    fn make_meta(seq: u32, sol: &str) -> SolutionSessionMetadata {
        SolutionSessionMetadata {
            id: SolutionSessionId::new(),
            solution_id: SolutionId(sol.into()),
            agent_id: SharedString::from("claude-acp"),
            acp_session_id: acp::SessionId::new(format!("acp-{seq}")),
            title: SharedString::from(format!("session {seq}")),
            created_at: Utc
                .timestamp_millis_opt(1_700_000_000_000 + seq as i64 * 1000)
                .unwrap(),
            last_activity_at: Utc
                .timestamp_millis_opt(1_700_000_000_000 + seq as i64 * 1000)
                .unwrap(),
            preview: None,
            total_tokens: None,
            context_count: 1,
            cwd: std::path::PathBuf::new(),
            parent_session_id: None,
            desired_model: None,
            desired_effort: None,
            cached_models: vec![],
            tab_order: None,
        }
    }

    #[gpui::test]
    async fn save_then_list_returns_inserted_rows(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        db.save_metadata(make_meta(1, "sol-a")).await.unwrap();
        db.save_metadata(make_meta(2, "sol-a")).await.unwrap();
        db.save_metadata(make_meta(3, "sol-b")).await.unwrap();

        let in_a = db
            .list_for_solution(SolutionId("sol-a".into()))
            .await
            .unwrap();
        assert_eq!(in_a.len(), 2);
        let in_b = db
            .list_for_solution(SolutionId("sol-b".into()))
            .await
            .unwrap();
        assert_eq!(in_b.len(), 1);
        let in_c = db
            .list_for_solution(SolutionId("sol-c".into()))
            .await
            .unwrap();
        assert_eq!(in_c.len(), 0);
    }

    #[gpui::test]
    async fn cwd_roundtrips_through_save_and_list(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let mut with_cwd = make_meta(1, "sol-a");
        with_cwd.cwd = std::path::PathBuf::from("/tmp/sol-a/member-x");
        let without_cwd = make_meta(2, "sol-a"); // empty PathBuf — legacy row

        db.save_metadata(with_cwd.clone()).await.unwrap();
        db.save_metadata(without_cwd.clone()).await.unwrap();

        let listed = db
            .list_for_solution(SolutionId("sol-a".into()))
            .await
            .unwrap();
        let by_id = |id| listed.iter().find(|m| m.id == id).expect("row present");
        assert_eq!(by_id(with_cwd.id).cwd, with_cwd.cwd);
        assert_eq!(by_id(without_cwd.id).cwd, std::path::PathBuf::new());
    }

    #[gpui::test]
    async fn save_blob_then_load_roundtrips(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let meta = make_meta(1, "sol-a");
        db.save_metadata(meta.clone()).await.unwrap();
        let blob = b"\x01\x02\x03 example payload".to_vec();
        db.save_blob(meta.id, blob.clone()).await.unwrap();

        let loaded = db.load_blob(meta.id).await.unwrap();
        assert_eq!(loaded, Some(blob));
    }

    #[gpui::test]
    async fn delete_removes_row(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let meta = make_meta(1, "sol-a");
        db.save_metadata(meta.clone()).await.unwrap();

        db.delete(meta.id).await.unwrap();

        let listing = db
            .list_for_solution(meta.solution_id.clone())
            .await
            .unwrap();
        assert!(listing.is_empty());
    }

    #[gpui::test]
    async fn tab_order_roundtrips_per_solution(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m1 = make_meta(1, "sol-a");
        let m2 = make_meta(2, "sol-a");
        let m3 = make_meta(3, "sol-a");
        let other = make_meta(4, "sol-b");
        for m in [&m1, &m2, &m3, &other] {
            db.save_metadata(m.clone()).await.unwrap();
        }

        db.update_tab_orders(SolutionId("sol-a".into()), vec![m2.id, m3.id, m1.id])
            .await
            .unwrap();
        db.update_tab_orders(SolutionId("sol-b".into()), vec![other.id])
            .await
            .unwrap();

        let in_a = db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap();
        assert_eq!(in_a, vec![m2.id, m3.id, m1.id]);
        let in_b = db.list_open_tabs(SolutionId("sol-b".into())).await.unwrap();
        assert_eq!(in_b, vec![other.id]);
    }

    #[gpui::test]
    async fn update_tab_orders_clears_omitted_sessions(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m1 = make_meta(1, "sol-a");
        let m2 = make_meta(2, "sol-a");
        let m3 = make_meta(3, "sol-a");
        for m in [&m1, &m2, &m3] {
            db.save_metadata(m.clone()).await.unwrap();
        }

        db.update_tab_orders(SolutionId("sol-a".into()), vec![m1.id, m2.id, m3.id])
            .await
            .unwrap();
        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m1.id, m2.id, m3.id]
        );

        db.update_tab_orders(SolutionId("sol-a".into()), vec![m2.id])
            .await
            .unwrap();
        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m2.id]
        );
    }

    #[gpui::test]
    async fn tab_order_survives_update_before_insert(cx: &mut gpui::TestAppContext) {
        // Lost-update race at create time: `create_session_with_parent` writes
        // the metadata row (`save_metadata`) and the strip position
        // (`update_tab_orders`) as two independent detached DB writes with no
        // happens-before. `update_tab_orders` is an UPDATE-only path, so if it
        // wins the race it no-ops (the metadata row doesn't exist yet) — the
        // strip position can only survive if the metadata write itself carries
        // the tab_order. The store fixes this by re-persisting the row AFTER
        // pinning, so the `save_metadata` here carries `Some(0)`.
        //
        // This test exercises the DB contract that makes that durable: a
        // metadata INSERT carrying a concrete tab_order lands it, and the
        // outcome does not depend on whether the bare UPDATE ran first or never
        // matched a row.
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let mut m = make_meta(1, "sol-a");

        // UPDATE first against a non-existent row: a genuine no-op, mirroring
        // the metadata-INSERT-loses-the-race ordering.
        db.update_tab_orders(SolutionId("sol-a".into()), vec![m.id])
            .await
            .unwrap();
        // INSERT second, carrying the real tab_order the store stamped in
        // memory before re-persisting. The row must end up pinned regardless of
        // the lost UPDATE above.
        m.tab_order = Some(0);
        db.save_metadata(m.clone()).await.unwrap();

        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m.id],
            "a metadata INSERT carrying tab_order must persist it even when a \
             prior UPDATE found no row"
        );
    }

    #[gpui::test]
    async fn tab_order_set_after_insert_still_works(cx: &mut gpui::TestAppContext) {
        // The benign order (INSERT then UPDATE) must keep working unchanged.
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m = make_meta(1, "sol-a");
        db.save_metadata(m.clone()).await.unwrap();
        db.update_tab_orders(SolutionId("sol-a".into()), vec![m.id])
            .await
            .unwrap();

        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m.id]
        );
    }

    #[gpui::test]
    async fn save_metadata_does_not_wipe_existing_tab_order(cx: &mut gpui::TestAppContext) {
        // A follow-up `save_metadata` (e.g. a token/preview update) carries
        // tab_order None, but must NOT clear a tab_order a prior
        // `update_tab_orders` legitimately set — the ON CONFLICT clause
        // COALESCE(excluded.tab_order=NULL, solution_sessions.tab_order) keeps it.
        // This is the load-bearing half of the order-independent fix: even if a
        // late metadata write lands after the strip position is durable, it
        // never reverts the row to NULL.
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m = make_meta(1, "sol-a");
        db.save_metadata(m.clone()).await.unwrap();
        db.update_tab_orders(SolutionId("sol-a".into()), vec![m.id])
            .await
            .unwrap();
        // Re-save metadata (tab_order None) as the live store would on a later
        // activity-driven update.
        db.save_metadata(m.clone()).await.unwrap();

        assert_eq!(
            db.list_open_tabs(SolutionId("sol-a".into())).await.unwrap(),
            vec![m.id],
            "a follow-up save_metadata(None) must not clear an existing tab_order"
        );
    }

    #[gpui::test]
    async fn reopen_session_clears_stale_tab_order(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let m = make_meta(1, "sol-a");
        db.save_metadata(m.clone()).await.unwrap();

        // Pin the session into the strip, then close it. `close_session` marks
        // `closed_at` but deliberately leaves `tab_order` set, so a closed row
        // keeps a dangling strip slot — reproduced here directly.
        db.update_tab_orders(SolutionId("sol-a".into()), vec![m.id])
            .await
            .unwrap();
        let closed_at = Utc.timestamp_millis_opt(1_700_000_500_000).unwrap();
        db.mark_closed(m.id, Some(closed_at)).await.unwrap();

        // While closed it is excluded from the open-tab strip (the closed_at
        // filter), even though its tab_order is still set.
        assert!(
            db.list_open_tabs(SolutionId("sol-a".into()))
                .await
                .unwrap()
                .is_empty()
        );

        // Reopen. This must clear `closed_at` (live again) AND the stale
        // `tab_order`. If it only cleared `closed_at`, the row would
        // immediately satisfy `list_open_tabs` (`tab_order IS NOT NULL AND
        // closed_at IS NULL`); hydration would re-stamp the stale order, and
        // `open_session_in_strip` would early-return on its `already_pinned`
        // guard without emitting `TabsChanged` — the reopened-but-invisible
        // tab bug.
        db.reopen_session(m.id).await.unwrap();

        // Live again:
        assert_eq!(
            db.list_open_session_ids(SolutionId("sol-a".into()))
                .await
                .unwrap(),
            vec![m.id]
        );
        // …but NOT pinned: the strip set is empty, so the reopen path re-pins
        // it fresh via `open_session_in_strip` and emits `TabsChanged`.
        assert!(
            db.list_open_tabs(SolutionId("sol-a".into()))
                .await
                .unwrap()
                .is_empty(),
            "reopen must clear the stale tab_order so the session re-pins fresh"
        );
    }

    #[gpui::test]
    async fn background_agent_round_trip(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let row = BackgroundAgentRow {
            solution_session_id: "ses-1".into(),
            agent_id: "a30f92a688e431edc".into(),
            jsonl_path: "/tmp/x.jsonl".into(),
            registered_at_ms: 1_700_000_000_000,
            last_seen_label: Some("Bash: ls".into()),
            last_mtime_ms: Some(1_700_000_001_000),
            stop_reason: None,
        };
        db.save_background_agent(row.clone()).await.unwrap();
        let loaded = db.load_background_agents("ses-1".into()).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], row);
    }

    #[gpui::test]
    async fn background_agent_delete(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let row = BackgroundAgentRow {
            solution_session_id: "ses-1".into(),
            agent_id: "a30f92a688e431edc".into(),
            jsonl_path: "/tmp/x.jsonl".into(),
            registered_at_ms: 1_700_000_000_000,
            last_seen_label: None,
            last_mtime_ms: None,
            stop_reason: None,
        };
        db.save_background_agent(row).await.unwrap();
        db.delete_background_agent("ses-1".into(), "a30f92a688e431edc".into())
            .await
            .unwrap();
        let loaded = db.load_background_agents("ses-1".into()).await.unwrap();
        assert!(loaded.is_empty());
    }

    #[gpui::test]
    async fn background_shell_round_trip(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let row = BackgroundShellRow {
            solution_session_id: "ses-1".into(),
            shell_id: "bvb4ful1z".into(),
            command: "npm run watch".into(),
            output_path: "/tmp/bvb4ful1z.output".into(),
            registered_at_ms: 1_700_000_000_000,
            last_tail: Some("Watching for changes...".into()),
            last_mtime_ms: Some(1_700_000_001_000),
            state_text: "running".into(),
        };
        db.save_background_shell(row.clone()).await.unwrap();
        let loaded = db.load_background_shells("ses-1".into()).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], row);

        // Verify None variants for optional fields also round-trip.
        let row_no_opts = BackgroundShellRow {
            solution_session_id: "ses-1".into(),
            shell_id: "xyz123".into(),
            command: "sleep 60".into(),
            output_path: "/tmp/xyz123.output".into(),
            registered_at_ms: 1_700_000_002_000,
            last_tail: None,
            last_mtime_ms: None,
            state_text: "exited:0".into(),
        };
        db.save_background_shell(row_no_opts.clone()).await.unwrap();
        let loaded = db.load_background_shells("ses-1".into()).await.unwrap();
        assert_eq!(loaded.len(), 2);
        let found = loaded.iter().find(|r| r.shell_id == "xyz123").unwrap();
        assert_eq!(found, &row_no_opts);
    }

    #[gpui::test]
    async fn background_shell_delete_by_id(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let row = BackgroundShellRow {
            solution_session_id: "ses-1".into(),
            shell_id: "bvb4ful1z".into(),
            command: "npm run watch".into(),
            output_path: "/tmp/bvb4ful1z.output".into(),
            registered_at_ms: 1_700_000_000_000,
            last_tail: None,
            last_mtime_ms: None,
            state_text: "running".into(),
        };
        db.save_background_shell(row).await.unwrap();
        db.delete_background_shell("ses-1".into(), "bvb4ful1z".into())
            .await
            .unwrap();
        let loaded = db.load_background_shells("ses-1".into()).await.unwrap();
        assert!(loaded.is_empty());
    }

    #[gpui::test]
    async fn background_shell_delete_for_session_only_removes_that_session(
        cx: &mut gpui::TestAppContext,
    ) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let make_row = |session: &str, shell: &str| BackgroundShellRow {
            solution_session_id: session.into(),
            shell_id: shell.into(),
            command: "echo hi".into(),
            output_path: format!("/tmp/{shell}.output"),
            registered_at_ms: 1_700_000_000_000,
            last_tail: None,
            last_mtime_ms: None,
            state_text: "killed".into(),
        };

        db.save_background_shell(make_row("ses-1", "shell-a"))
            .await
            .unwrap();
        db.save_background_shell(make_row("ses-1", "shell-b"))
            .await
            .unwrap();
        db.save_background_shell(make_row("ses-2", "shell-c"))
            .await
            .unwrap();

        db.delete_background_shells_for_session("ses-1".into())
            .await
            .unwrap();

        let ses1 = db.load_background_shells("ses-1".into()).await.unwrap();
        assert!(ses1.is_empty());
        let ses2 = db.load_background_shells("ses-2".into()).await.unwrap();
        assert_eq!(ses2.len(), 1);
        assert_eq!(ses2[0].shell_id, "shell-c");
    }

    #[gpui::test]
    async fn attachments_round_trip_and_cascade(cx: &mut gpui::TestAppContext) {
        let db = SolutionAgentDb::open(cx.executor()).unwrap();
        db.record_attachment("ses-1".into(), "sol-a".into(), "/inbox/a.png".into(), 1)
            .await
            .unwrap();
        db.record_attachment("ses-1".into(), "sol-a".into(), "/inbox/b.png".into(), 2)
            .await
            .unwrap();
        db.record_attachment("ses-2".into(), "sol-a".into(), "/inbox/c.png".into(), 3)
            .await
            .unwrap();
        db.record_attachment("ses-3".into(), "sol-b".into(), "/inbox/d.png".into(), 4)
            .await
            .unwrap();

        let mut by_ses1 = db
            .attachment_paths_for_session("ses-1".into())
            .await
            .unwrap();
        by_ses1.sort();
        assert_eq!(by_ses1, vec!["/inbox/a.png", "/inbox/b.png"]);

        let by_sol_a = db
            .attachment_paths_for_solution("sol-a".into())
            .await
            .unwrap();
        assert_eq!(by_sol_a.len(), 3);

        // Delete by session removes only that session's rows.
        db.delete_attachments_for_session("ses-1".into())
            .await
            .unwrap();
        assert!(
            db.attachment_paths_for_session("ses-1".into())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.attachment_paths_for_session("ses-2".into())
                .await
                .unwrap()
                .len(),
            1
        );

        // delete_for_solution cascades to attachment rows for that solution only.
        db.delete_for_solution(SolutionId("sol-a".into()))
            .await
            .unwrap();
        assert!(
            db.attachment_paths_for_solution("sol-a".into())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.attachment_paths_for_solution("sol-b".into())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[gpui::test]
    async fn solution_session_entries_table_and_index_exist(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        let connection = db.connection.lock();

        // Check table exists
        let mut tables = connection
            .select::<String>(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='solution_session_entries'",
            )
            .unwrap();
        let table_names = tables().unwrap();
        assert!(
            table_names.iter().any(|n| n == "solution_session_entries"),
            "solution_session_entries table must exist"
        );

        // Check index exists
        let mut indexes = connection
            .select::<String>(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_session_entry_modseq'",
            )
            .unwrap();
        let index_names = indexes().unwrap();
        assert!(
            index_names.iter().any(|n| n == "idx_session_entry_modseq"),
            "idx_session_entry_modseq index must exist"
        );

        // Check epoch column exists on solution_sessions
        assert!(
            column_exists(&connection, "solution_sessions", "epoch"),
            "epoch column must exist on solution_sessions"
        );
    }

    #[gpui::test]
    async fn delete_for_solution_cascades(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();
        db.save_metadata(make_meta(1, "sol-a")).await.unwrap();
        db.save_metadata(make_meta(2, "sol-a")).await.unwrap();
        db.save_metadata(make_meta(3, "sol-b")).await.unwrap();

        db.delete_for_solution(SolutionId("sol-a".into()))
            .await
            .unwrap();

        assert!(
            db.list_for_solution(SolutionId("sol-a".into()))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.list_for_solution(SolutionId("sol-b".into()))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// Solution-level hard purge must sweep ALL six per-session tables for the
    /// target solution (not just `solution_sessions` + attachments), while
    /// leaving another solution's rows untouched. The background tables key on
    /// `solution_session_id` and `supervisor_state` keys on `session_id` with
    /// no `solution_id` column, so the sweep relies on a subselect over
    /// `solution_sessions` for the doomed solution.
    #[gpui::test]
    async fn delete_for_solution_removes_rows_from_all_six_tables(cx: &mut gpui::TestAppContext) {
        use crate::supervisor::SupervisorState;

        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        // One session in the doomed solution, one in a survivor solution.
        let doomed_meta = make_meta(1, "sol-doomed");
        let doomed = doomed_meta.id;
        db.save_metadata(doomed_meta).await.unwrap();
        let keep_meta = make_meta(2, "sol-keep");
        let keep = keep_meta.id;
        db.save_metadata(keep_meta).await.unwrap();

        for (id, sol, tag) in [(doomed, "sol-doomed", "x"), (keep, "sol-keep", "y")] {
            db.upsert_entry(id, 0, 0, 0, None, tag.as_bytes().to_vec())
                .await
                .unwrap();
            db.record_attachment(id.to_string(), sol.into(), format!("/inbox/{tag}.png"), 1)
                .await
                .unwrap();
            db.save_background_agent(BackgroundAgentRow {
                solution_session_id: id.to_string(),
                agent_id: format!("agent-{tag}"),
                jsonl_path: format!("/tmp/{tag}.jsonl"),
                registered_at_ms: 1,
                last_seen_label: None,
                last_mtime_ms: None,
                stop_reason: None,
            })
            .await
            .unwrap();
            db.save_background_shell(BackgroundShellRow {
                solution_session_id: id.to_string(),
                shell_id: format!("shell-{tag}"),
                command: "echo hi".into(),
                output_path: format!("/tmp/shell-{tag}.output"),
                registered_at_ms: 1,
                last_tail: None,
                last_mtime_ms: None,
                state_text: "running".into(),
            })
            .await
            .unwrap();
            db.save_supervisor_state(SupervisorState::new(id))
                .await
                .unwrap();
        }

        db.delete_for_solution(SolutionId("sol-doomed".into()))
            .await
            .unwrap();

        // Every table is empty for the doomed solution's session.
        assert!(db.load_entries(doomed).await.unwrap().is_empty());
        assert!(
            db.attachment_paths_for_session(doomed.to_string())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.load_background_agents(doomed.to_string())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.load_background_shells(doomed.to_string())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.load_supervisor_states()
                .await
                .unwrap()
                .iter()
                .all(|s| s.session_id != doomed)
        );
        assert!(
            db.list_for_solution(SolutionId("sol-doomed".into()))
                .await
                .unwrap()
                .is_empty()
        );

        // The survivor solution's session keeps every row.
        assert_eq!(db.load_entries(keep).await.unwrap().len(), 1);
        assert_eq!(
            db.attachment_paths_for_session(keep.to_string())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.load_background_agents(keep.to_string())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.load_background_shells(keep.to_string())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.load_supervisor_states()
                .await
                .unwrap()
                .iter()
                .any(|s| s.session_id == keep)
        );
        assert_eq!(
            db.list_for_solution(SolutionId("sol-keep".into()))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[gpui::test]
    async fn entry_upsert_and_load_ordered_by_idx(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let session_id = SolutionSessionId::new();
        db.upsert_entry(session_id, 1, 10, 1_000, None, b"second".to_vec())
            .await
            .unwrap();
        db.upsert_entry(
            session_id,
            0,
            5,
            500,
            Some("agent-a".into()),
            b"first".to_vec(),
        )
        .await
        .unwrap();

        let rows = db.load_entries(session_id).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].idx, 0);
        assert_eq!(rows[0].payload, b"first".to_vec());
        assert_eq!(rows[0].subagent_id, Some("agent-a".into()));
        assert_eq!(rows[1].idx, 1);
        assert_eq!(rows[1].payload, b"second".to_vec());
        assert_eq!(rows[1].subagent_id, None);
    }

    #[gpui::test]
    async fn entry_upsert_same_idx_updates_in_place(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let session_id = SolutionSessionId::new();
        db.upsert_entry(session_id, 0, 1, 100, None, b"original".to_vec())
            .await
            .unwrap();
        db.upsert_entry(
            session_id,
            0,
            2,
            200,
            Some("sub".into()),
            b"updated".to_vec(),
        )
        .await
        .unwrap();

        let rows = db.load_entries(session_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].idx, 0);
        assert_eq!(rows[0].mod_seq, 2);
        assert_eq!(rows[0].created_ms, 200);
        assert_eq!(rows[0].subagent_id, Some("sub".into()));
        assert_eq!(rows[0].payload, b"updated".to_vec());
    }

    #[gpui::test]
    async fn delete_entries_from_leaves_earlier_rows(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let session_id = SolutionSessionId::new();
        for i in 0i64..3 {
            db.upsert_entry(session_id, i, i, i * 100, None, vec![i as u8])
                .await
                .unwrap();
        }

        db.delete_entries_from(session_id, 1).await.unwrap();

        let rows = db.load_entries(session_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].idx, 0);
    }

    #[gpui::test]
    async fn save_and_load_epoch_roundtrips(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        // epoch lives on solution_sessions, so the row must exist first.
        let meta = make_meta(1, "sol-epoch");
        db.save_metadata(meta.clone()).await.unwrap();

        // Before setting it, load_epoch returns None.
        let before = db.load_epoch(meta.id).await.unwrap();
        assert_eq!(before, None);

        db.save_epoch(meta.id, 42).await.unwrap();
        let after = db.load_epoch(meta.id).await.unwrap();
        assert_eq!(after, Some(42));

        // Update to a new value.
        db.save_epoch(meta.id, 99).await.unwrap();
        let updated = db.load_epoch(meta.id).await.unwrap();
        assert_eq!(updated, Some(99));
    }

    #[gpui::test]
    async fn save_and_load_change_seq_roundtrips(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        // change_seq lives on solution_sessions, so the row must exist first.
        let meta = make_meta(1, "sol-change-seq");
        db.save_metadata(meta.clone()).await.unwrap();

        // Before setting it, load_change_seq returns None (legacy/unset).
        let before = db.load_change_seq(meta.id).await.unwrap();
        assert_eq!(before, None);

        db.save_change_seq(meta.id, 7).await.unwrap();
        let after = db.load_change_seq(meta.id).await.unwrap();
        assert_eq!(after, Some(7));

        // Update to a new (higher) value.
        db.save_change_seq(meta.id, 42).await.unwrap();
        let updated = db.load_change_seq(meta.id).await.unwrap();
        assert_eq!(updated, Some(42));

        // The UPDATE is `max`-guarded: a stale lower write (e.g. a detached
        // background flush that lands out of order) must NOT roll the durable
        // value back below an already-issued cursor.
        db.save_change_seq(meta.id, 10).await.unwrap();
        let guarded = db.load_change_seq(meta.id).await.unwrap();
        assert_eq!(
            guarded,
            Some(42),
            "a lower write must not overwrite a higher durable change_seq"
        );
    }

    #[gpui::test]
    async fn delete_entries_for_session_removes_all_rows(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        let session_a = SolutionSessionId::new();
        let session_b = SolutionSessionId::new();

        for i in 0i64..3 {
            db.upsert_entry(session_a, i, i, i * 100, None, vec![i as u8])
                .await
                .unwrap();
        }
        db.upsert_entry(session_b, 0, 0, 0, None, b"keep".to_vec())
            .await
            .unwrap();

        db.delete_entries_for_session(session_a).await.unwrap();

        let rows_a = db.load_entries(session_a).await.unwrap();
        assert!(rows_a.is_empty());
        let rows_b = db.load_entries(session_b).await.unwrap();
        assert_eq!(rows_b.len(), 1);
    }

    #[gpui::test]
    async fn purge_session_removes_rows_from_all_six_tables(cx: &mut gpui::TestAppContext) {
        use crate::supervisor::SupervisorState;

        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        // The session to purge, plus a sibling whose rows must survive.
        let meta = make_meta(1, "sol-purge");
        let target = meta.id;
        db.save_metadata(meta).await.unwrap();

        let sibling_meta = make_meta(2, "sol-purge");
        let sibling = sibling_meta.id;
        db.save_metadata(sibling_meta).await.unwrap();

        // Populate all six tables for both sessions, to prove purge is scoped.
        for (id, tag) in [(target, "x"), (sibling, "y")] {
            db.upsert_entry(id, 0, 0, 0, None, tag.as_bytes().to_vec())
                .await
                .unwrap();
            db.record_attachment(
                id.to_string(),
                "sol-purge".into(),
                format!("/inbox/{tag}.png"),
                1,
            )
            .await
            .unwrap();
            db.save_background_agent(BackgroundAgentRow {
                solution_session_id: id.to_string(),
                agent_id: format!("agent-{tag}"),
                jsonl_path: format!("/tmp/{tag}.jsonl"),
                registered_at_ms: 1,
                last_seen_label: None,
                last_mtime_ms: None,
                stop_reason: None,
            })
            .await
            .unwrap();
            db.save_background_shell(BackgroundShellRow {
                solution_session_id: id.to_string(),
                shell_id: format!("shell-{tag}"),
                command: "echo hi".into(),
                output_path: format!("/tmp/shell-{tag}.output"),
                registered_at_ms: 1,
                last_tail: None,
                last_mtime_ms: None,
                state_text: "running".into(),
            })
            .await
            .unwrap();
            db.save_supervisor_state(SupervisorState::new(id))
                .await
                .unwrap();
        }

        db.purge_session(target).await.unwrap();

        // Every table is empty for `target`.
        assert!(db.load_entries(target).await.unwrap().is_empty());
        assert!(
            db.attachment_paths_for_session(target.to_string())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.load_background_agents(target.to_string())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.load_background_shells(target.to_string())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.load_supervisor_states()
                .await
                .unwrap()
                .iter()
                .all(|s| s.session_id != target)
        );
        let listed = db
            .list_for_solution(SolutionId("sol-purge".into()))
            .await
            .unwrap();
        assert!(listed.iter().all(|m| m.id != target));

        // The sibling's rows all survive.
        assert_eq!(db.load_entries(sibling).await.unwrap().len(), 1);
        assert_eq!(
            db.attachment_paths_for_session(sibling.to_string())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.load_background_agents(sibling.to_string())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.load_background_shells(sibling.to_string())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.load_supervisor_states()
                .await
                .unwrap()
                .iter()
                .any(|s| s.session_id == sibling)
        );
        assert!(listed.iter().any(|m| m.id == sibling));
    }

    // ── Task 3a: session model/effort/cached_models columns ──────────────────

    /// Helper: build a `ModelInfo` for use in tests.
    fn make_model_info(value: &str) -> claude_native::ModelInfo {
        claude_native::ModelInfo {
            value: value.into(),
            display_name: format!("{value} Display"),
            description: format!("{value} description"),
        }
    }

    /// (a) Round-trip: fields written in save_metadata come back from
    /// list_for_solution intact.
    /// (b) COALESCE: a second save with all-None/empty doesn't clobber the
    /// values from the first.
    /// (c) cached_models JSON serialises and deserialises without data loss.
    #[gpui::test]
    async fn session_settings_roundtrip_and_coalesce(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let db = SolutionAgentDb::open(executor).unwrap();

        // (a) round-trip
        let mut meta = make_meta(1, "sol-settings");
        meta.desired_model = Some("claude-opus-4-5".into());
        meta.desired_effort = Some("high".into());
        meta.cached_models = vec![
            make_model_info("claude-opus-4-5"),
            make_model_info("claude-sonnet-4-5"),
        ];
        db.save_metadata(meta.clone()).await.unwrap();

        let listed = db
            .list_for_solution(SolutionId("sol-settings".into()))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        let loaded = &listed[0];
        assert_eq!(loaded.desired_model, Some("claude-opus-4-5".into()));
        assert_eq!(loaded.desired_effort, Some("high".into()));
        assert_eq!(loaded.cached_models, meta.cached_models);

        // (b) COALESCE: second write with None/empty must not clobber
        let mut meta_nones = make_meta(1, "sol-settings");
        // Override id to match the existing row so ON CONFLICT fires.
        meta_nones.id = meta.id;
        meta_nones.desired_model = None;
        meta_nones.desired_effort = None;
        meta_nones.cached_models = vec![];
        db.save_metadata(meta_nones).await.unwrap();

        let listed2 = db
            .list_for_solution(SolutionId("sol-settings".into()))
            .await
            .unwrap();
        assert_eq!(listed2.len(), 1);
        let loaded2 = &listed2[0];
        // Original values must still be present.
        assert_eq!(
            loaded2.desired_model,
            Some("claude-opus-4-5".into()),
            "COALESCE must not clobber desired_model"
        );
        assert_eq!(
            loaded2.desired_effort,
            Some("high".into()),
            "COALESCE must not clobber desired_effort"
        );
        assert_eq!(
            loaded2.cached_models, meta.cached_models,
            "COALESCE must not clobber cached_models"
        );

        // (c) cached_models JSON round-trip: a write with ≥1 model and then a
        // read must preserve all fields of ModelInfo.
        let mut meta2 = make_meta(2, "sol-settings");
        meta2.cached_models = vec![make_model_info("model-x")];
        db.save_metadata(meta2.clone()).await.unwrap();

        let listed3 = db
            .list_for_solution(SolutionId("sol-settings".into()))
            .await
            .unwrap();
        let loaded3 = listed3
            .iter()
            .find(|m| m.id == meta2.id)
            .expect("row must be present");
        assert_eq!(loaded3.cached_models.len(), 1);
        assert_eq!(loaded3.cached_models[0].value, "model-x");
        assert_eq!(loaded3.cached_models[0].display_name, "model-x Display");
        assert_eq!(loaded3.cached_models[0].description, "model-x description");
    }

    #[gpui::test]
    async fn list_sessions_closed_before_returns_only_old_closed(cx: &mut gpui::TestAppContext) {
        let db = SolutionAgentDb::open(cx.executor()).unwrap();
        let day = 86_400_000i64;
        let now = 1_800_000_000_000i64;

        let old = make_meta(1, "sol-reap");
        let recent = make_meta(2, "sol-reap");
        let open = make_meta(3, "sol-reap");
        let (old_id, recent_id, open_id) = (old.id, recent.id, open.id);
        for m in [old, recent, open] {
            db.save_metadata(m).await.unwrap();
        }

        // `old` was soft-closed 40 days ago (past the 30d cutoff), `recent` 5
        // days ago (inside it), `open` is still open (no `closed_at`).
        db.mark_closed(
            old_id,
            Some(Utc.timestamp_millis_opt(now - 40 * day).unwrap()),
        )
        .await
        .unwrap();
        db.mark_closed(
            recent_id,
            Some(Utc.timestamp_millis_opt(now - 5 * day).unwrap()),
        )
        .await
        .unwrap();

        let cutoff = now - 30 * day;
        let ids = db
            .list_sessions_closed_before(SolutionId("sol-reap".into()), cutoff)
            .await
            .unwrap();
        assert_eq!(
            ids,
            vec![old_id],
            "only the long-ago-closed session is returned"
        );

        // A reopen clears `closed_at`, so a restored session is no longer
        // eligible even though it was once closed long ago.
        db.reopen_session(old_id).await.unwrap();
        let ids = db
            .list_sessions_closed_before(SolutionId("sol-reap".into()), cutoff)
            .await
            .unwrap();
        assert!(
            ids.is_empty(),
            "a reopened session is excluded (closed_at cleared)"
        );
        let _ = (recent_id, open_id);
    }
