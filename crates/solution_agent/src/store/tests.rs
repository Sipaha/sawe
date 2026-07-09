use super::*;
use crate::adapter::AdapterRegistry;
use crate::model::SessionState;
use crate::test_support::{MockAgentServer, MockConnection};
use chrono::Utc;
use gpui::{Entity, SharedString, TestAppContext};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Insert a minimal cold session (no `acp_thread`) directly into the store
/// for tests that need a pre-existing session without going through the full
/// `create_session` → ACP-handshake flow.
pub(crate) fn insert_cold_session(
    session_id: crate::model::SolutionSessionId,
    solution_id: solutions::SolutionId,
    agent_id: gpui::SharedString,
    cached_total_tokens: Option<u64>,
    project: Option<Entity<project::Project>>,
    store: &mut SolutionAgentStore,
    cx: &mut gpui::Context<SolutionAgentStore>,
) -> Entity<crate::model::SolutionSession> {
    let session = cx.new(|_| {
        let mut s = crate::model::SolutionSession::new_idle(
            session_id,
            solution_id.clone(),
            agent_id,
            agent_client_protocol::schema::SessionId::new("acp-cold"),
        );
        s.title = SharedString::from("Cold");
        s.project = project;
        s.cached_total_tokens = cached_total_tokens;
        s
    });
    store.sessions.insert(session_id, session.clone());
    store
        .by_solution
        .entry(solution_id)
        .or_default()
        .push(session_id);
    session
}

#[gpui::test]
fn close_session_removes_from_indices(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let id = SolutionSessionId::new();
            let entity = cx.new(|_| {
                let mut s = SolutionSession::new_idle(
                    id,
                    SolutionId("sol-a".into()),
                    SharedString::from("claude-acp"),
                    agent_client_protocol::schema::SessionId::new("acp-1"),
                );
                s.title = SharedString::from("test");
                s
            });
            store.sessions.insert(id, entity);
            store
                .by_solution
                .entry(SolutionId("sol-a".into()))
                .or_default()
                .push(id);

            assert_eq!(store.sessions_for(&SolutionId("sol-a".into())).len(), 1);
            store.close_session(id, cx).expect("close_session");
            assert_eq!(store.sessions_for(&SolutionId("sol-a".into())).len(), 0);
            assert!(store.session(id).is_none());
        });
    });
}

/// Regression: a long-running supervised session that is closed must drop ALL
/// of its per-session in-memory state, not just the session entity + indices.
/// Before this, `close_session` left `supervisor_states`, the background-agent /
/// shell watcher tasks, the backoff timer, the parent-jsonl scan cursor, and any
/// in-flight judge/auditor handle behind — each accumulating for the editor's
/// whole lifetime over thousands of open/close cycles (and an orphaned judge
/// handle never released its pooled `claude` subprocess).
#[gpui::test]
fn close_session_clears_supervisor_and_watcher_maps(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");
            let id = SolutionSessionId::new();
            insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);

            // Populate every per-session runtime map for `id`.
            store
                .supervisor_states
                .insert(id, crate::supervisor::SupervisorState::new(id));
            store.background_agent_watchers.insert(id, Task::ready(()));
            store.background_shell_watchers.insert(id, Task::ready(()));
            store.backoff_timers.insert(id, Task::ready(()));
            store.parent_jsonl_scan_offsets.insert(id, 0);
            store
                .metrics_emitter
                .last_emit
                .lock()
                .insert(id, std::time::Instant::now());
            // A judge whose create has not resolved (judge_id None) — finish_judge
            // must still drop the handle (no child session to close).
            store.judge_sessions.insert(
                id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    _task: Task::ready(()),
                },
            );
            store.auditor_sessions.insert(
                id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    _task: Task::ready(()),
                },
            );

            store.close_session(id, cx).expect("close_session");

            assert!(store.session(id).is_none());
            assert!(
                !store.supervisor_states.contains_key(&id),
                "supervisor_states leaked"
            );
            assert!(
                !store.background_agent_watchers.contains_key(&id),
                "background_agent_watchers leaked"
            );
            assert!(
                !store.background_shell_watchers.contains_key(&id),
                "background_shell_watchers leaked"
            );
            assert!(
                !store.backoff_timers.contains_key(&id),
                "backoff_timers leaked"
            );
            assert!(
                !store.parent_jsonl_scan_offsets.contains_key(&id),
                "parent_jsonl_scan_offsets leaked"
            );
            assert!(
                !store.judge_sessions.contains_key(&id),
                "judge_sessions leaked"
            );
            assert!(
                !store.auditor_sessions.contains_key(&id),
                "auditor_sessions leaked"
            );
            assert!(
                !store.metrics_emitter.last_emit.lock().contains_key(&id),
                "metrics_emitter.last_emit leaked"
            );
        });
    });
}

#[test]
fn push_and_evict_transcripts_keeps_window() {
    use std::collections::VecDeque;
    let mut h: VecDeque<String> = VecDeque::new();
    // keep = 3 → the live transcript is implicit, so 2 abandoned are retained.
    assert!(super::push_and_evict_transcripts(&mut h, "a".into(), 3).is_empty());
    assert!(super::push_and_evict_transcripts(&mut h, "b".into(), 3).is_empty());
    // The third abandoned id evicts the oldest ("a").
    assert_eq!(
        super::push_and_evict_transcripts(&mut h, "c".into(), 3),
        vec!["a".to_string()]
    );
    assert_eq!(
        super::push_and_evict_transcripts(&mut h, "d".into(), 3),
        vec!["b".to_string()]
    );
    assert_eq!(h.len(), 2);
    assert_eq!(h.front().map(String::as_str), Some("c"));
}

#[gpui::test]
async fn close_session_purges_inbox_attachments(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let (inbox_dir, db) = store.update(cx, |store, cx| {
        (store.session_inbox_dir(id, cx), store.persistence())
    });
    let db = db.expect("seeded store has persistence");

    std::fs::create_dir_all(&inbox_dir).unwrap();
    let file = inbox_dir.join("shot.png");
    std::fs::write(&file, b"png").unwrap();
    db.record_attachment(
        id.to_string(),
        "sol".into(),
        file.to_string_lossy().into_owned(),
        1,
    )
    .await
    .unwrap();
    assert!(file.exists());
    assert_eq!(
        db.attachment_paths_for_session(id.to_string())
            .await
            .unwrap()
            .len(),
        1
    );

    store.update(cx, |store, cx| store.close_session(id, cx).unwrap());
    cx.run_until_parked();

    assert!(!inbox_dir.exists(), "inbox dir must be removed on close");
    assert!(
        db.attachment_paths_for_session(id.to_string())
            .await
            .unwrap()
            .is_empty(),
        "attachment rows must be cleared on close"
    );
}

/// `purge_session_hard` is the HARD teardown used when a session's member dir
/// is removed: it must drop the live entity + indices, delete the whole
/// `<solution_root>/.agents/<sid>/` tree (observer files, compacts, inbox),
/// and hard-delete the persisted rows (not soft-close them).
#[gpui::test]
async fn purge_session_hard_removes_entity_disk_tree_and_rows(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let (archive_dir, db, sol) = store.update(cx, |store, cx| {
        let sol = store.session(id).unwrap().read(cx).solution_id.clone();
        let root = store.solution_root_for_app(id, cx).expect("solution root");
        (
            root.join(".agents").join(id.to_string()),
            store.persistence().expect("persistence"),
            sol,
        )
    });

    // Lay down the on-disk session tree (diary + a nested inbox file).
    std::fs::create_dir_all(archive_dir.join("inbox")).unwrap();
    std::fs::write(archive_dir.join("diary.md"), b"notes").unwrap();
    std::fs::write(archive_dir.join("inbox").join("a.png"), b"png").unwrap();
    assert!(archive_dir.exists());

    // Persist a metadata row + an entry so we can prove the HARD delete.
    db.save_metadata(crate::model::SolutionSessionMetadata {
        id,
        solution_id: sol.clone(),
        agent_id: SharedString::from("claude-acp"),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-cold"),
        title: SharedString::from("Cold"),
        created_at: Utc::now(),
        last_activity_at: Utc::now(),
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    })
    .await
    .unwrap();
    db.upsert_entry(id, 0, 0, 0, None, b"e".to_vec())
        .await
        .unwrap();

    store.update(cx, |store, cx| store.purge_session_hard(id, None, cx));
    cx.run_until_parked();

    store.update(cx, |store, _| {
        assert!(store.session(id).is_none(), "entity must be gone");
        assert!(
            !store.sessions_for(&sol).iter().any(|_| true)
                || store
                    .by_solution
                    .get(&sol)
                    .map_or(true, |v| !v.contains(&id)),
            "id must be removed from by_solution"
        );
    });
    assert!(!archive_dir.exists(), ".agents/<sid> tree must be deleted");
    assert!(
        db.list_for_solution(sol.clone())
            .await
            .unwrap()
            .iter()
            .all(|m| m.id != id),
        "session row must be HARD-deleted, not soft-closed"
    );
    assert!(
        db.load_entries(id).await.unwrap().is_empty(),
        "entry rows must be hard-deleted"
    );
}

/// `purge_solution_fully` is the SINGLE solution-level hard primitive: it must
/// drop every hydrated session of the solution (entity + `.agents/<sid>/` tree),
/// hard-delete every persisted row across all six tables (incl. non-hydrated
/// rows via `delete_for_solution`), and nuke the whole `<root>/.agents` tree.
/// This is what the `Deleted { id, root }` store event funnels into.
#[gpui::test]
async fn purge_solution_fully_clears_sessions_disk_and_rows(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let (root, agents_dir, archive_dir, db, sol) = store.update(cx, |store, cx| {
        let sol = store.session(id).unwrap().read(cx).solution_id.clone();
        let root = store.solution_root_for_app(id, cx).expect("solution root");
        let agents = root.join(".agents");
        (
            root,
            agents.clone(),
            agents.join(id.to_string()),
            store.persistence().expect("persistence"),
            sol,
        )
    });

    // Lay down the hydrated session's on-disk tree plus a stray archive dir for
    // a never-hydrated session id (proves the wholesale `.agents` removal).
    std::fs::create_dir_all(archive_dir.join("inbox")).unwrap();
    std::fs::write(archive_dir.join("diary.md"), b"notes").unwrap();
    let stray = agents_dir.join("ses-never-loaded");
    std::fs::create_dir_all(&stray).unwrap();
    assert!(archive_dir.exists() && stray.exists());

    // Persist the hydrated session's metadata + an entry, plus a supervisor row.
    db.save_metadata(crate::model::SolutionSessionMetadata {
        id,
        solution_id: sol.clone(),
        agent_id: SharedString::from("claude-acp"),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-cold"),
        title: SharedString::from("Cold"),
        created_at: Utc::now(),
        last_activity_at: Utc::now(),
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    })
    .await
    .unwrap();
    db.upsert_entry(id, 0, 0, 0, None, b"e".to_vec())
        .await
        .unwrap();
    db.save_supervisor_state(crate::supervisor::SupervisorState::new(id))
        .await
        .unwrap();

    store.update(cx, |store, cx| {
        store.purge_solution_fully(sol.clone(), Some(root.clone()), cx)
    });
    cx.run_until_parked();

    store.update(cx, |store, _| {
        assert!(store.session(id).is_none(), "entity must be gone");
        assert!(
            store.by_solution.get(&sol).map_or(true, |v| v.is_empty()),
            "by_solution entry for the deleted solution must be gone"
        );
    });
    assert!(
        !agents_dir.exists(),
        ".agents tree must be wholesale-removed"
    );
    assert!(
        db.list_for_solution(sol.clone()).await.unwrap().is_empty(),
        "session rows must be hard-deleted"
    );
    assert!(
        db.load_entries(id).await.unwrap().is_empty(),
        "entries gone"
    );
    assert!(
        db.load_supervisor_states()
            .await
            .unwrap()
            .iter()
            .all(|s| s.session_id != id),
        "supervisor_state must be hard-deleted"
    );
}

/// `close_session` is SOFT: it keeps the persisted row (mark_closed), keeps the
/// `<root>/.agents/<sid>/` on-disk tree, and keeps the supervisor_state row so a
/// later reopen restores both the transcript and supervision settings.
#[gpui::test]
async fn close_session_is_soft_keeps_archive_dir_and_supervisor_row(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let (archive_dir, db, sol) = store.update(cx, |store, cx| {
        let sol = store.session(id).unwrap().read(cx).solution_id.clone();
        let root = store.solution_root_for_app(id, cx).expect("solution root");
        (
            root.join(".agents").join(id.to_string()),
            store.persistence().expect("persistence"),
            sol,
        )
    });

    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::write(archive_dir.join("diary.md"), b"notes").unwrap();
    // `mark_closed` stamps `closed_at` on the existing `solution_sessions` row,
    // so the row must exist before the soft close for the stamp to land.
    db.save_metadata(crate::model::SolutionSessionMetadata {
        id,
        solution_id: sol.clone(),
        agent_id: SharedString::from("claude-acp"),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-cold"),
        title: SharedString::from("Cold"),
        created_at: Utc::now(),
        last_activity_at: Utc::now(),
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    })
    .await
    .unwrap();
    db.save_supervisor_state(crate::supervisor::SupervisorState::new(id))
        .await
        .unwrap();

    store.update(cx, |store, cx| store.close_session(id, cx).unwrap());
    cx.run_until_parked();

    assert!(
        archive_dir.exists(),
        ".agents/<sid> tree must SURVIVE a soft close"
    );
    assert!(
        db.closed_at(id).await.unwrap().is_some(),
        "soft close must keep the row and stamp closed_at"
    );
    assert!(
        db.load_supervisor_states()
            .await
            .unwrap()
            .iter()
            .any(|s| s.session_id == id),
        "supervisor_state must survive a soft close (reopen needs it)"
    );
}

/// `gc_orphan_members` purges only sessions whose `cwd` is no longer under any
/// alive member path (nor the solution root). Sessions under a member dir, or
/// at the solution root, survive.
#[gpui::test]
async fn gc_orphan_members_purges_only_removed_member_sessions(cx: &mut gpui::TestAppContext) {
    use solutions::{CatalogId, SolutionStore};

    let registry = Arc::new(AdapterRegistry::new());
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("solutions.json");
    let solutions_root = dir.path().join("solutions");
    std::fs::create_dir_all(&solutions_root).unwrap();

    let (sol, root, member_path) = cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        let solution_store = SolutionStore::for_test(cfg_path, cx);
        solutions::install_global_for_test(solution_store.clone(), cx);
        let sol = solution_store
            .update(cx, |s, cx| {
                s.create_solution("Sol", solutions_root.clone(), cx)
            })
            .expect("create_solution");
        let root = solution_store.read(cx).solutions()[0].root.clone();
        let member_path = root.join("kept-member");
        solution_store.update(cx, |s, _| {
            s.test_add_member_with_path(&sol, &CatalogId("kept".into()), member_path.clone());
        });
        (sol, root, member_path)
    });

    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));

    let agent = SharedString::from("claude-acp");
    let under_member = SolutionSessionId::new();
    let at_root = SolutionSessionId::new();
    let orphan = SolutionSessionId::new();

    store.update(cx, |store, cx| {
        for (sid, cwd) in [
            (under_member, member_path.join("sub")),
            (at_root, root.clone()),
            (orphan, root.join("removed-member")),
        ] {
            let session =
                insert_cold_session(sid, sol.clone(), agent.clone(), None, None, store, cx);
            session.update(cx, |s, _| s.cwd = cwd);
        }
        store.gc_orphan_members(cx);
    });
    cx.run_until_parked();

    store.update(cx, |store, _| {
        assert!(
            store.session(under_member).is_some(),
            "member-dir session kept"
        );
        assert!(store.session(at_root).is_some(), "root session kept");
        assert!(
            store.session(orphan).is_none(),
            "removed-member session purged"
        );
    });
}

#[gpui::test]
async fn reap_stale_closed_sessions_purges_old_closed_only(cx: &mut TestAppContext) {
    use chrono::TimeZone;
    // seed_store_with_session installs a SolutionStore (so the reaper resolves
    // the root) + a persistence DB.
    let (store, seeded, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let sol = store.read_with(cx, |s, cx| {
        s.session(seeded)
            .expect("seeded")
            .read(cx)
            .solution_id
            .clone()
    });
    let db = store
        .read_with(cx, |s, _| s.persistence())
        .expect("persistence");

    // Two persisted sessions in the same solution: one soft-closed 40 days ago
    // (past the 30d TTL → reap), one 5 days ago (inside it → keep).
    let old = SolutionSessionId::new();
    let recent = SolutionSessionId::new();
    store.update(cx, |store, cx| {
        for id in [old, recent] {
            insert_cold_session(
                id,
                sol.clone(),
                SharedString::from("claude-acp"),
                None,
                None,
                store,
                cx,
            );
            store.persist_session_row(id, cx);
        }
    });
    cx.run_until_parked();

    let day = 86_400_000i64;
    let now = Utc::now().timestamp_millis();
    db.mark_closed(old, Some(Utc.timestamp_millis_opt(now - 40 * day).unwrap()))
        .await
        .unwrap();
    db.mark_closed(
        recent,
        Some(Utc.timestamp_millis_opt(now - 5 * day).unwrap()),
    )
    .await
    .unwrap();

    store.update(cx, |store, cx| {
        store.reap_stale_closed_sessions(sol.clone(), cx)
    });
    cx.run_until_parked();

    let ids: Vec<SolutionSessionId> = db
        .list_for_solution(sol)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert!(!ids.contains(&old), "session closed 40d ago is hard-purged");
    assert!(ids.contains(&recent), "session closed 5d ago is kept");
}

/// `cold_close_solution` bypasses `close_session` (it drops live entities
/// without soft-closing the persisted sessions), so it must prune the same
/// per-session runtime maps itself or they leak when a solution's window closes.
#[gpui::test]
fn cold_close_solution_clears_supervisor_and_watcher_maps(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");
            let id = SolutionSessionId::new();
            insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);

            store
                .supervisor_states
                .insert(id, crate::supervisor::SupervisorState::new(id));
            store.background_agent_watchers.insert(id, Task::ready(()));
            store.background_shell_watchers.insert(id, Task::ready(()));
            store.backoff_timers.insert(id, Task::ready(()));
            store.parent_jsonl_scan_offsets.insert(id, 0);
            store.judge_sessions.insert(
                id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    _task: Task::ready(()),
                },
            );

            store.cold_close_solution(&sol, cx);

            assert!(store.session(id).is_none());
            assert!(
                !store.supervisor_states.contains_key(&id),
                "supervisor_states leaked"
            );
            assert!(
                !store.background_agent_watchers.contains_key(&id),
                "background_agent_watchers leaked"
            );
            assert!(
                !store.background_shell_watchers.contains_key(&id),
                "background_shell_watchers leaked"
            );
            assert!(
                !store.backoff_timers.contains_key(&id),
                "backoff_timers leaked"
            );
            assert!(
                !store.judge_sessions.contains_key(&id),
                "judge_sessions leaked"
            );
        });
    });
}

#[gpui::test]
fn visible_session_count_excludes_live_judges(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");

            // Two ordinary user sessions + one judge child session.
            let supervised = SolutionSessionId::new();
            let other = SolutionSessionId::new();
            let judge = SolutionSessionId::new();
            for id in [supervised, other, judge] {
                insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);
            }

            // Before the judge is registered, all three count.
            assert_eq!(store.sessions_for(&sol).len(), 3);
            assert_eq!(store.visible_session_count(&sol), 3);

            // Register the judge handle (as `spawn_judge` does once create
            // resolves): supervised_id -> JudgeHandle { judge_id }.
            store.judge_sessions.insert(
                supervised,
                JudgeHandle {
                    judge_id: Some(judge),
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    _task: Task::ready(()),
                },
            );

            // The live judge is now excluded from the badge count, but the
            // raw session list is unchanged (judge is still reachable).
            assert_eq!(store.sessions_for(&sol).len(), 3);
            assert_eq!(store.visible_session_count(&sol), 2);

            // A judge handle whose create has not resolved yet (judge_id None)
            // excludes nothing.
            store.judge_sessions.remove(&supervised);
            store.judge_sessions.insert(
                supervised,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    _task: Task::ready(()),
                },
            );
            assert_eq!(store.visible_session_count(&sol), 3);
        });
    });
}

/// Bug #1: a HUMAN reply into a session whose supervisor is mid-`Judging`
/// supersedes the in-flight judge — its verdict is now stale (the user took
/// over direction), so it must NOT push an Observer nudge afterwards. The
/// reply tears the judge down and returns supervision to `Watching`; a verdict
/// that still races in is dropped by `apply_verdict`'s staleness guard.
#[gpui::test]
fn user_reply_supersedes_in_flight_judge(cx: &mut TestAppContext) {
    use crate::supervisor::{SupervisorState, SupervisorStatus, VerdictAction};
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");
            let id = SolutionSessionId::new();
            insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);

            // Supervisor is mid-Judging with a live judge handle (judge_id None
            // so finish_judge has no child session to close).
            let mut st = SupervisorState::new(id);
            st.enabled = true;
            st.status = SupervisorStatus::Judging;
            store.supervisor_states.insert(id, st);
            store.judge_sessions.insert(
                id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    _task: Task::ready(()),
                },
            );

            // The human replies -> supersede the judge.
            store.supersede_judge_on_user_reply(id, cx);
            assert!(
                !store.judge_sessions.contains_key(&id),
                "in-flight judge must be torn down on a user reply"
            );
            assert!(
                matches!(
                    store.supervisor_states[&id].status,
                    SupervisorStatus::Watching
                ),
                "supervisor returns to Watching after the reply, got {:?}",
                store.supervisor_states[&id].status
            );

            // A verdict that races in AFTER the teardown is stale: it must NOT
            // act (no nudge, no continue-counter increment).
            store.apply_verdict(
                id,
                VerdictAction::Continue,
                "looks unfinished".into(),
                Some("Keep going.".into()),
                None,
                None,
                None,
                cx,
            );
            assert_eq!(
                store.supervisor_states[&id].consecutive_continues, 0,
                "a superseded verdict must be suppressed (no nudge / no counter bump)"
            );
        });
    });
}

/// Control for bug #1: when the judge is STILL in flight (no user reply), a
/// `Continue` verdict applies normally — it increments the continue counter and
/// nudges. Proves the staleness guard discriminates on the judge handle rather
/// than suppressing every verdict.
#[gpui::test]
fn verdict_applies_while_judge_in_flight(cx: &mut TestAppContext) {
    use crate::supervisor::{SupervisorState, SupervisorStatus, VerdictAction};
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");
            let id = SolutionSessionId::new();
            insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);

            let mut st = SupervisorState::new(id);
            st.enabled = true;
            st.status = SupervisorStatus::Judging;
            store.supervisor_states.insert(id, st);
            store.judge_sessions.insert(
                id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    _task: Task::ready(()),
                },
            );

            store.apply_verdict(
                id,
                VerdictAction::Continue,
                "looks unfinished".into(),
                Some("Keep going.".into()),
                None,
                None,
                None,
                cx,
            );
            assert_eq!(
                store.supervisor_states[&id].consecutive_continues, 1,
                "a live verdict must apply (counter bumped)"
            );
        });
    });
}

/// Set up SolutionStore with one Solution rooted at a tempdir, plus
/// a `Project::test` whose worktree is that root. Returns
/// (`SolutionId`, `tempdir`, `Project`). Hold the tempdir for the
/// lifetime of the test — `create_solution` writes to it.
pub(crate) async fn setup_solution_and_project(
    cx: &mut TestAppContext,
) -> (
    SolutionId,
    tempfile::TempDir,
    gpui::Entity<project::Project>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("solutions.json");
    let solutions_root = dir.path().join("solutions");
    std::fs::create_dir_all(&solutions_root).expect("solutions root");
    let store = cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        let store = solutions::SolutionStore::for_test(cfg_path, cx);
        solutions::install_global_for_test(store.clone(), cx);
        store
    });
    let solution_id = store
        .update(cx, |store, cx| {
            store.create_solution("Sol", solutions_root.clone(), cx)
        })
        .expect("create_solution");
    let solution_root: PathBuf = store.read_with(cx, |store, _| {
        store
            .solutions()
            .iter()
            .find(|s| s.id == solution_id)
            .map(|s| s.root.clone())
            .expect("solution exists")
    });

    let fs = fs::FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(solution_root.clone(), serde_json::json!({ ".keep": "" }))
        .await;
    let project = project::Project::test(fs, [solution_root.as_path()], cx).await;

    (solution_id, dir, project)
}

#[gpui::test]
async fn pool_release_arms_60s_shutdown_then_drops(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let key = (SolutionId("sol-a".into()), SharedString::from("mock-agent"));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.pool_pretend_session_added(key.clone(), Rc::new(MockConnection::new()));
            assert_eq!(store.pool_size(), 1);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.pool_release_session(key.clone(), cx);
        });
    });

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(30));
    cx.executor().run_until_parked();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| assert_eq!(store.pool_size(), 1));
    });

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(35));
    cx.executor().run_until_parked();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| assert_eq!(store.pool_size(), 0));
    });
}

/// Regression: `close_session` must release the pool refcount so the shared
/// `claude` connection shuts down once its last session closes. Before the fix
/// `close_session` never called `pool_release_session` (only the failed-spawn
/// rollback did), so the refcount only ever climbed — the 60s debounce never
/// armed and the pooled subprocess (plus every per-session `claude` child it
/// spawned for judges/auditors) leaked for the editor's lifetime.
#[gpui::test]
async fn close_session_releases_pooled_connection(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            assert_eq!(
                store.pool_size(),
                1,
                "a spawned session holds one pooled connection"
            );
        });
    });

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.close_session(session_id, cx).expect("close_session");
        });
    });

    // The release arms the 60s debounce; drain it and the connection drops.
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(61));
    cx.executor().run_until_parked();
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            assert_eq!(
                store.pool_size(),
                0,
                "closing the last session must release + shut down the pooled connection"
            );
        });
    });
}

#[gpui::test]
async fn shutdown_cancels_when_session_re_added(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));
    let key = (SolutionId("sol-a".into()), SharedString::from("mock-agent"));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.pool_pretend_session_added(key.clone(), Rc::new(MockConnection::new()));
            store.pool_release_session(key.clone(), cx);
        });
    });

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(30));
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.pool_pretend_session_added(key.clone(), Rc::new(MockConnection::new()));
        });
    });

    cx.executor()
        .advance_clock(std::time::Duration::from_secs(60));
    cx.executor().run_until_parked();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| assert_eq!(store.pool_size(), 1));
    });
}

#[gpui::test]
async fn create_session_spawns_subprocess_once_per_pair(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::new(connect_count.clone())),
            );
        });
    });

    let session_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            assert!(store.session(session_id).is_some());
            assert_eq!(store.pool_size(), 1);
        });
    });
    assert_eq!(connect_count.load(Ordering::SeqCst), 1);
}

#[gpui::test]
async fn parallel_create_session_for_same_pair_spawns_only_once(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    // Gate `connect()` until both create_session calls have observed the
    // pool entry — this guarantees the second call sees `Pending` and
    // doesn't race past into a fresh spawn before the first one inserts.
    let (gate_tx, gate_rx) = async_channel::bounded(1);
    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_gate(connect_count.clone(), gate_rx)),
            );
        });
    });

    let task1 = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
        })
    });
    let task2 = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
        })
    });

    // Pump scheduler so both tasks reach the await on `connect_task`.
    cx.executor().run_until_parked();
    // Now release the gate, letting connect() resolve.
    gate_tx.send(()).await.expect("gate send");
    gate_tx.close();

    let id1 = task1.await.expect("task1");
    let id2 = task2.await.expect("task2");
    assert_ne!(id1, id2);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            assert_eq!(store.pool_size(), 1);
            assert!(store.session(id1).is_some());
            assert!(store.session(id2).is_some());
        });
    });
    assert_eq!(connect_count.load(Ordering::SeqCst), 1);
}

/// Create a real session (via `create_session`) backed by `MockAgentServer`/
/// `MockConnection`, then return both its id and a clone of the underlying
/// `Entity<AcpThread>` so tests can emit synthetic `AcpThreadEvent`s.
pub(crate) async fn create_session_with_thread(
    cx: &mut TestAppContext,
) -> (
    SolutionSessionId,
    gpui::Entity<acp_thread::AcpThread>,
    tempfile::TempDir,
) {
    let (solution_id, tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::new(connect_count.clone())),
            );
        });
    });

    let session_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");

    let acp_thread = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .acp_thread()
            .cloned()
            .expect("acp_thread populated")
    });

    (session_id, acp_thread, tmp)
}

/// Like `create_session_with_thread`, but backs the session with a
/// `MockConnection` whose `cancel()` increments the returned counter, so a
/// test can assert how many times the store forwarded a stop to the backend.
async fn create_session_with_cancel_counter(
    cx: &mut TestAppContext,
) -> (SolutionSessionId, Arc<AtomicUsize>, tempfile::TempDir) {
    let (solution_id, tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let connect_count = Arc::new(AtomicUsize::new(0));
    let cancel_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_cancel_count(
                    connect_count.clone(),
                    cancel_count.clone(),
                )),
            );
        });
    });

    let session_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");

    (session_id, cancel_count, tmp)
}

#[gpui::test]
async fn cancel_turn_sets_stopping_and_is_idempotent(cx: &mut TestAppContext) {
    let (session_id, cancel_calls, _tmp) = create_session_with_cancel_counter(cx).await;

    // Put the session in Running so cancel has an in-flight turn to stop.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    let read_state = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store
                .read(cx)
                .session(session_id)
                .expect("session")
                .read(cx)
                .state
                .clone()
        })
    };

    // First cancel: Running -> Stopping, connection.cancel forwarded once.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.cancel_turn(session_id, cx))
    })
    .expect("cancel_turn");
    assert!(matches!(read_state(cx), SessionState::Stopping { .. }));
    assert_eq!(cancel_calls.load(Ordering::SeqCst), 1);

    // Second cancel while Stopping: no-op, no extra forward.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.cancel_turn(session_id, cx))
    })
    .expect("cancel_turn idempotent");
    assert!(matches!(read_state(cx), SessionState::Stopping { .. }));
    assert_eq!(cancel_calls.load(Ordering::SeqCst), 1);
}

/// Regression for the 2026-05-24 "Stopping forever" bug. The
/// `MockConnection::cancel` is a counter-only no-op (it does NOT fire
/// `AcpThreadEvent::Stopped`), so without the safety net the session
/// would sit in `Stopping` for the rest of the process lifetime. With
/// the net armed in `cancel_turn`, advancing the executor past
/// `STOPPING_SAFETY_NET` must force-flip the state back to `Idle`.
#[gpui::test]
async fn stopping_safety_net_force_flips_to_idle(cx: &mut TestAppContext) {
    let (session_id, cancel_calls, _tmp) = create_session_with_cancel_counter(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.cancel_turn(session_id, cx))
    })
    .expect("cancel_turn");

    // cancel forwarded once → Stopping → safety net armed.
    assert_eq!(cancel_calls.load(Ordering::SeqCst), 1);
    let state_after_cancel = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .state
            .clone()
    });
    assert!(
        matches!(state_after_cancel, SessionState::Stopping { .. }),
        "expected Stopping after cancel, got {state_after_cancel:?}"
    );

    // Net is 40s. Advance past it; the spawned timer fires and
    // mutate_state flips the session back to Idle.
    cx.executor().advance_clock(
        crate::store::queue::STOPPING_SAFETY_NET + std::time::Duration::from_secs(1),
    );
    cx.executor().run_until_parked();

    let state_after_net = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .state
            .clone()
    });
    assert!(
        matches!(state_after_net, SessionState::Idle),
        "expected Idle after safety net, got {state_after_net:?}"
    );
}

/// Sibling regression: when the natural `Stopped` chain DOES fire
/// (the happy path), the safety net must NOT later overwrite a
/// legitimate new state. The `mutate_state` cleanup hook drops the
/// task on the `Stopping → Idle` transition; if that cleanup ever
/// regresses, a delayed timer would force a healthy `Running` turn
/// (started after the cancel) back to `Idle` spuriously.
#[gpui::test]
async fn stopping_safety_net_does_not_fire_after_natural_recovery(cx: &mut TestAppContext) {
    let (session_id, _acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.cancel_turn(session_id, cx))
    })
    .expect("cancel_turn");

    // Natural recovery: Stopped event fires (the way claude_native does
    // in the happy path). This must drop the safety-net task via
    // `mutate_state` cleanup.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let thread = session.read(cx).acp_thread().cloned().expect("live thread");
        thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                agent_client_protocol::schema::StopReason::Cancelled,
            ));
        });
    });
    cx.executor().run_until_parked();

    // Now flip back to Running (a follow-up turn the user kicked off)
    // and advance past the safety-net window. If the cleanup hook
    // dropped the task properly, state stays Running. If not, the
    // delayed timer would force it back to Idle.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(
                session_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    }
                },
                cx,
            );
        });
    });
    cx.executor().advance_clock(
        crate::store::queue::STOPPING_SAFETY_NET + std::time::Duration::from_secs(1),
    );
    cx.executor().run_until_parked();

    let final_state = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .state
            .clone()
    });
    assert!(
        matches!(final_state, SessionState::Running { .. }),
        "safety-net must not fire after natural Stopped recovery; got {final_state:?}"
    );
}

#[gpui::test]
async fn turn_complete_event_transitions_running_to_idle(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    cx.update(|cx| {
        acp_thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                agent_client_protocol::schema::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::Idle),
                "expected Idle, got {:?}",
                state
            );
        });
    });
}

/// Regression: sending a message while a tool call is blocked
/// `WaitingForConfirmation` must NOT leave the turn stuck. The send path
/// declines the pending authorization (reject) to unblock the turn, then
/// the message rides the normal queue/flush. Here we assert the unblock:
/// the tool call leaves `WaitingForConfirmation` (becomes `Rejected`) and
/// the user's message is queued (not dropped) for the next turn.
#[gpui::test]
async fn send_while_waiting_for_confirmation_unblocks_the_turn(cx: &mut TestAppContext) {
    use acp_thread::{AgentThreadEntry, ToolCallStatus};
    use agent_client_protocol::schema as acp;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Stage a tool call sitting in WaitingForConfirmation with a flat
    // allow/reject option pair, and HOLD the returned auth task so the
    // oneshot the turn awaits stays alive (dropping it would itself flip
    // the call off WaitingForConfirmation and defeat the test).
    let _auth_task = cx.update(|cx| {
        acp_thread.update(cx, |thread, cx| {
            let update = acp::ToolCallUpdate::new(
                acp::ToolCallId::new("call-auth-1"),
                acp::ToolCallUpdateFields::new()
                    .kind(acp::ToolKind::Execute)
                    .title("Bash".to_string()),
            );
            let options = acp_thread::PermissionOptions::Flat(vec![
                acp::PermissionOption::new(
                    "opt-allow",
                    "Allow".to_string(),
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    "opt-reject",
                    "Reject".to_string(),
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ]);
            thread
                .request_tool_call_authorization(
                    update,
                    options,
                    acp_thread::AuthorizationKind::PermissionGrant,
                    cx,
                )
                .expect("stage waiting-for-confirmation")
        })
    });
    cx.executor().run_until_parked();

    // The turn is blocked → session is Running while it waits.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        });
    });

    // User types a new message instead of clicking a button.
    let send_task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.send_message_blocks(
                session_id,
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "never mind, do this instead".to_string(),
                ))],
                cx,
            )
        })
    });
    send_task.await.expect("send_message_blocks");
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            // The pending authorization must be resolved: the tool call has
            // left WaitingForConfirmation (rejected), so the turn is no
            // longer blocked.
            let status_is_waiting = acp_thread.read(cx).entries().iter().any(|entry| {
                matches!(
                    entry,
                    AgentThreadEntry::ToolCall(call)
                        if matches!(call.status, ToolCallStatus::WaitingForConfirmation { .. })
                )
            });
            assert!(
                !status_is_waiting,
                "tool call should no longer be WaitingForConfirmation after the send"
            );
            let rejected = acp_thread.read(cx).entries().iter().any(|entry| {
                matches!(
                    entry,
                    AgentThreadEntry::ToolCall(call)
                        if matches!(call.status, ToolCallStatus::Rejected)
                )
            });
            assert!(rejected, "tool call should be Rejected (declined to unblock)");

            // The user's message must not be lost — it's queued for the
            // flush-on-Stopped that the now-unblocked turn will trigger.
            let session = store.session(session_id).expect("session exists");
            assert_eq!(
                session.read(cx).pending_messages.len(),
                1,
                "the user's message must be queued for delivery, not dropped"
            );
            // Fix 2 wired: the resolve branch set the one-shot
            // `flush_after_cancel` so the queue survives even if the agent
            // treats the rejection as a Cancelled stop rather than EndTurn.
            assert!(
                session.read(cx).flush_after_cancel,
                "flush_after_cancel must be set so a Cancelled-stop rejection still flushes the queue"
            );
        });
    });

    // Drive the now-unblocked turn to completion. On `Stopped(EndTurn)` the
    // store's queue handler drains `pending_messages` and re-sends them as
    // the next turn — end-to-end delivery, not just enqueue.
    cx.update(|cx| {
        acp_thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                agent_client_protocol::schema::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            // The queued message was actually delivered: the flush drained
            // it (and re-sent it as the next turn), so the queue is empty.
            assert!(
                session.read(cx).pending_messages.is_empty(),
                "the queued message must be delivered (queue drained) after the turn stops, not dropped"
            );
        });
    });
}

#[gpui::test]
async fn error_event_transitions_to_errored_state(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Error);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::Errored(_)),
                "expected Errored, got {:?}",
                state
            );
        });
    });
}

/// Bug #5: once `Error` latched the session to `Errored`, genuine agent
/// activity (a new entry, or a streaming in-place update) must clear it back
/// to `Running` so the status row stops showing red "Error: …" while text
/// streams. An editor-injected SystemNote must NOT clear it (it isn't agent
/// activity).
#[gpui::test]
async fn streaming_activity_clears_latched_errored_state(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let check = move |cx: &mut TestAppContext, want_running: bool, ctx: &str| {
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                let state = store
                    .session(session_id)
                    .expect("session exists")
                    .read(cx)
                    .state
                    .clone();
                if want_running {
                    assert!(
                        matches!(state, SessionState::Running { .. }),
                        "{ctx}: expected Running, got {state:?}"
                    );
                } else {
                    assert!(
                        matches!(state, SessionState::Errored(_)),
                        "{ctx}: expected Errored, got {state:?}"
                    );
                }
            });
        });
    };

    // 1. An agent error latches the session red.
    cx.update(|cx| acp_thread.update(cx, |_t, cx| cx.emit(acp_thread::AcpThreadEvent::Error)));
    cx.executor().run_until_parked();
    check(cx, false, "after Error");

    // 2. An Observer breadcrumb (SystemNote) is editor-originated, not agent
    //    activity — it must NOT clear the error.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_system_note(
                acp_thread::SystemNoteLevel::Observer,
                "Наблюдатель направил агента",
                cx,
            );
        })
    });
    cx.executor().run_until_parked();
    check(cx, false, "after SystemNote");

    // 3. A genuine new assistant entry (NewEntry) means the agent recovered —
    //    clears Errored -> Running.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("recovering".to_string()),
                ),
                false,
                cx,
            );
        })
    });
    cx.executor().run_until_parked();
    check(cx, true, "after recovered NewEntry");

    // 4. Re-latch Errored, then an in-place streaming EntryUpdated on the same
    //    assistant entry must also clear it (the visible "red while streaming"
    //    symptom is EntryUpdated-driven).
    cx.update(|cx| acp_thread.update(cx, |_t, cx| cx.emit(acp_thread::AcpThreadEvent::Error)));
    cx.executor().run_until_parked();
    check(cx, false, "after second Error");
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(" more".to_string()),
                ),
                false,
                cx,
            );
        })
    });
    cx.executor().run_until_parked();
    check(cx, true, "after streaming EntryUpdated");
}

/// The stuck watchdog must NOT treat a usage/session-limit wall as a hang. A
/// turn that hits the limit prints the wall as its last assistant message and
/// then stalls (Running, silent past STUCK_TURN_SECS, no in-progress tool) —
/// which looks wedged. Reconnecting + "carry on" there just re-hits the wall
/// and burns quota (the reported loop). Instead the session is stopped with
/// the wall message (NOT `reconnecting…`) and the supervisor is parked at
/// `Stopped(Quota)` (no parseable reset in this message).
#[gpui::test]
async fn stuck_usage_limit_wall_stops_without_reconnect(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // The turn's last assistant message is the limit wall.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(
                        "You've hit your session limit".to_string(),
                    ),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Force the wedged shape: Running, last activity well past STUCK_TURN_SECS
    // (5 min), no in-progress tool. Set the stale timestamp AFTER the push
    // (which bumps last_activity to now). Supervision on, to observe the stop.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.set_supervision_enabled(session_id, true, cx);
            let session = store.session(session_id).unwrap();
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(6 * 60);
            });
            store.tick_stuck_sessions(cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let state = store.session(session_id).unwrap().read(cx).state.clone();
            match state {
                SessionState::Errored(msg) => {
                    assert!(
                        msg.contains("session limit"),
                        "must stop with the wall message, got {msg:?}"
                    );
                    assert!(
                        !msg.contains("reconnect"),
                        "must NOT take the reconnect path, got {msg:?}"
                    );
                }
                other => panic!("expected Errored(wall), got {other:?}"),
            }
            let st = store.supervisor_state(session_id).unwrap();
            assert_eq!(
                st.status,
                crate::supervisor::SupervisorStatus::Stopped(
                    crate::supervisor::StoppedReason::Quota
                ),
                "supervisor parked at Stopped(Quota) for a no-reset limit"
            );
            assert!(!st.enabled);
        });
    });
}

#[gpui::test]
async fn tool_authorization_request_transitions_to_awaiting_input(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::ToolAuthorizationRequested(
                agent_client_protocol::schema::ToolCallId::new("test-tool"),
            ));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::AwaitingInput),
                "expected AwaitingInput, got {:?}",
                state
            );
        });
    });
}

#[gpui::test]
async fn send_message_starts_running_state_immediately(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    // Use a prompt-gated MockConnection so prompt() stays pending until we
    // release the gate — this lets us observe the synchronous Running flip
    // before the underlying ACP turn completes.
    let (prompt_gate_tx, prompt_gate_rx) = async_channel::bounded::<()>(1);
    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_prompt_gate(
                    connect_count.clone(),
                    prompt_gate_rx,
                )),
            );
        });
    });

    let session_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");

    // Force `Idle` so we can prove `send_message` flips it to `Running`
    // synchronously rather than just observing pre-existing `Running`.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.state = SessionState::Idle);
        });
    });

    // Kick off the prompt. We deliberately don't await `task` here — we
    // want to read the state BEFORE the prompt resolves.
    let task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.send_message(session_id, "hi".into(), cx)
        })
    });

    // Synchronous post-condition: state is already Running.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::Running { .. }),
                "expected Running synchronously after send_message, got {:?}",
                state
            );
        });
    });

    // Now release the prompt gate so the spawned future resolves.
    prompt_gate_tx.send(()).await.expect("release prompt gate");
    prompt_gate_tx.close();
    task.await.expect("send_message task");
}

#[gpui::test]
async fn queued_messages_get_per_message_timestamp_prefix(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    // Force `Running` so send_message_blocks takes the queueing branch.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("first thought".to_string()),
            )];
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert_eq!(s.pending_messages.len(), 1, "one queued bundle after first enqueue");
            let bundle = &s.pending_messages[0];
            let first = match &bundle.blocks[0] {
                agent_client_protocol::schema::ContentBlock::Text(t) => t.text.as_str(),
                other => panic!("first block must be Text, got {other:?}"),
            };
            assert!(first.starts_with('['), "first block starts with timestamp prefix, got {first:?}");
            let inner = first.strip_prefix('[').and_then(|s| s.split_once("] ")).map(|(ts, _)| ts);
            assert!(
                matches!(inner, Some(ts) if ts.len() == 8 && ts.as_bytes()[2] == b':' && ts.as_bytes()[5] == b':'),
                "expected [HH:MM:SS] prefix, got {first:?}"
            );
            let payload: String = bundle
                .blocks
                .iter()
                .filter_map(|b| match b {
                    agent_client_protocol::schema::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(
                payload.contains("first thought"),
                "user content preserved after prefix, got {payload:?}"
            );
            assert!(
                !payload.contains("queued in advance"),
                "old verbose marker must not appear, got {payload:?}"
            );
        });
    });

    // Second enqueue while still Running — each follow-up gets its own timestamp prefix.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("follow-up".to_string()),
            )];
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert_eq!(s.pending_messages.len(), 1, "still one bundle after second enqueue");
            let bundle = &s.pending_messages[0];
            let payload: String = bundle
                .blocks
                .iter()
                .filter_map(|b| match b {
                    agent_client_protocol::schema::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(
                payload.contains("first thought") && payload.contains("follow-up"),
                "both messages preserved, got {payload:?}"
            );
            let stamp_count = bundle
                .blocks
                .iter()
                .filter(|b| matches!(b,
                    agent_client_protocol::schema::ContentBlock::Text(t)
                        if t.text.starts_with('[')
                            && t.text.strip_prefix('[')
                                .and_then(|s| s.split_once("] "))
                                .map(|(ts, _)| ts.len() == 8 && ts.as_bytes()[2] == b':' && ts.as_bytes()[5] == b':')
                                .unwrap_or(false)
                ))
                .count();
            assert_eq!(stamp_count, 2, "each follow-up gets its own timestamp prefix");
        });
    });
}

#[gpui::test]
async fn take_pending_for_delivery_drains_pushes_and_formats(cx: &mut TestAppContext) {
    let (session_id, thread, _tmp) = create_session_with_thread(cx).await;
    let entries_before = cx.update(|cx| thread.read(cx).entries().len());

    // Force Running so send_message enqueues, then queue one follow-up.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message(session_id, "hello".to_string(), cx)
                .detach_and_log_err(cx);
        });
    });

    // Mid-work delivery: text has the timestamp, NO hint; queue drained; one timeline entry pushed.
    let mid = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, None, false, cx)
        })
    });
    let text = mid.expect("pending present");
    assert!(
        text.contains("hello"),
        "delivers user content, got {text:?}"
    );
    assert!(
        !text.contains(crate::store::queue::QUEUE_HINT_LINE),
        "no hint mid-work, got {text:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                0,
                "queue drained"
            );
        });
    });
    assert_eq!(
        cx.update(|cx| thread.read(cx).entries().len()),
        entries_before + 1,
        "one user entry pushed"
    );

    // End-of-turn delivery carries the hint.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message(session_id, "again".to_string(), cx)
                .detach_and_log_err(cx);
        });
    });
    let end = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.take_pending_for_delivery(session_id, None, true, cx)
            })
        })
        .expect("pending present");
    assert!(
        end.contains(crate::store::queue::QUEUE_HINT_LINE),
        "Stop/idle delivery carries the hint, got {end:?}"
    );
}

/// Pure contract tests for the routing predicates — cheap, no GPUI needed.
#[test]
fn queue_target_matches_hook_routes_by_agent_id() {
    use crate::model::QueueTarget;
    // Main bundles drain on the main agent's hook (no agent_id), never on a
    // teammate's hook.
    assert!(QueueTarget::Main.matches_hook(None));
    assert!(!QueueTarget::Main.matches_hook(Some("agent-1")));
    // Subagent bundles drain ONLY on their own teammate's hook.
    let sub = QueueTarget::Subagent(SharedString::from("agent-1"));
    assert!(sub.matches_hook(Some("agent-1")));
    assert!(!sub.matches_hook(Some("agent-2")));
    assert!(!sub.matches_hook(None));
}

/// A `Subagent`-targeted follow-up drains only on the matching teammate's
/// hook; a non-matching teammate hook and the main agent's hook leave it
/// queued, and the main agent's own bundle is independent.
#[gpui::test]
async fn take_pending_routes_by_target(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    // Force Running, then queue one Main-targeted and one Subagent-targeted
    // follow-up (distinct addressees ⇒ two bundles, not a merge).
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message(session_id, "for main".to_string(), cx)
                .detach_and_log_err(cx);
            store
                .send_message_blocks_targeted(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "for teammate".to_string(),
                    ))],
                    crate::model::QueueTarget::Subagent(SharedString::from("agent-1")),
                    true,
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                2,
                "distinct targets must NOT merge into one bundle"
            );
        });
    });

    // A non-matching teammate hook drains nothing and leaves both bundles.
    let miss = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, Some("agent-2"), false, cx)
        })
    });
    assert!(miss.is_none(), "non-matching teammate hook drains nothing");

    // The matching teammate hook drains ONLY its own bundle (the Main bundle
    // stays queued).
    let sub = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.take_pending_for_delivery(session_id, Some("agent-1"), false, cx)
            })
        })
        .expect("teammate bundle delivered");
    assert!(sub.contains("for teammate"), "got {sub:?}");
    assert!(
        !sub.contains("for main"),
        "must not leak the Main bundle, got {sub:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                1,
                "Main bundle stays queued after the teammate drains its own"
            );
        });
    });

    // The main agent's hook now drains the remaining Main bundle.
    let main = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.take_pending_for_delivery(session_id, None, false, cx)
            })
        })
        .expect("main bundle delivered");
    assert!(main.contains("for main"), "got {main:?}");
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                0,
                "queue empty after both addressees drained"
            );
        });
    });
}

/// A `Subagent`-targeted follow-up enqueued while running, then never drained
/// by its teammate (the teammate finishes), is DROPPED — not delivered — when
/// the parent turn ends. We can't easily fire the real `Stopped` event in a
/// unit test, so assert the inverse half of the contract directly: the main
/// agent's hook must never drain a subagent-targeted bundle.
#[gpui::test]
async fn main_hook_never_drains_subagent_bundle(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message_blocks_targeted(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "orphan".to_string(),
                    ))],
                    crate::model::QueueTarget::Subagent(SharedString::from("ghost")),
                    true,
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    // Main agent's hook fires — must NOT swallow the teammate's bundle.
    let pulled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, None, true, cx)
        })
    });
    assert!(
        pulled.is_none(),
        "main hook must not drain a subagent-targeted bundle, got {pulled:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                1,
                "subagent bundle stays queued for its own (now-gone) addressee"
            );
        });
    });
}

/// A queued bundle carrying an IMAGE must not be delivered via the mid-turn
/// hook (its `additionalContext` is text-only — the image bytes would be lost
/// and the agent would never see the screenshot). It stays queued for the
/// `Stopped` idle-flush, which re-sends the full content blocks.
#[gpui::test]
async fn take_pending_holds_image_bundle_for_idle_flush(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message_blocks(
                    session_id,
                    vec![
                        acp::ContentBlock::Text(acp::TextContent::new(
                            "look at [image #1]".to_string(),
                        )),
                        acp::ContentBlock::Image(acp::ImageContent::new(
                            "ZGF0YQ==".to_string(),
                            "image/png".to_string(),
                        )),
                    ],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    // The main hook fires (even at end-of-turn) — the image bundle is NOT
    // drained text-only; it stays queued for the idle-flush.
    let pulled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, None, true, cx)
        })
    });
    assert!(
        pulled.is_none(),
        "image bundle must not be delivered via the text-only hook, got {pulled:?}"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                1,
                "image bundle stays queued for the idle-flush (full-content re-send)"
            );
        });
    });
}

/// MID-TURN (a `PostToolUse` hook, `is_end_of_turn = false`), a queued bundle
/// carrying an image IS delivered: the bytes are written to an inbox file and
/// the agent-facing text points at that path with a `Read`-tool instruction,
/// instead of being deferred to the next turn end. This is the fix for
/// "image follow-ups don't reach the agent until the (long) turn finishes".
#[gpui::test]
async fn take_pending_delivers_image_as_readable_path_mid_turn(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message_blocks(
                    session_id,
                    vec![
                        acp::ContentBlock::Text(acp::TextContent::new("look at this".to_string())),
                        // base64 "ZGF0YQ==" decodes to the bytes b"data".
                        acp::ContentBlock::Image(acp::ImageContent::new(
                            "ZGF0YQ==".to_string(),
                            "image/png".to_string(),
                        )),
                    ],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });

    // Mid-turn hook (is_end_of_turn = false) — the image bundle is delivered.
    let pulled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.take_pending_for_delivery(session_id, None, false, cx)
        })
    });
    let text = pulled.expect("image bundle must be delivered mid-turn as a path reference");
    assert!(
        text.contains("Use the Read tool"),
        "delivered text must instruct the agent to Read the saved image, got: {text:?}"
    );
    assert!(
        text.contains("look at this"),
        "delivered text must keep the user's words, got: {text:?}"
    );

    // The referenced file must exist on disk and hold the decoded bytes.
    let path_str = text
        .split("saved to ")
        .nth(1)
        .and_then(|rest| rest.split(". Use the Read tool").next())
        .expect("delivered text must carry the saved path")
        .to_string();
    let path = std::path::PathBuf::from(&path_str);
    assert!(
        path.extension().is_some_and(|e| e == "png"),
        "path {path:?} keeps the png extension"
    );
    let written = std::fs::read(&path).expect("inbox image file must exist");
    assert_eq!(
        written, b"data",
        "inbox file must hold the decoded image bytes"
    );
    let _ = std::fs::remove_file(&path);

    // Queue is drained — nothing left for the idle-flush to re-send.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .is_empty(),
                "image bundle must be drained after mid-turn delivery"
            );
        });
    });
}

#[test]
fn stale_archive_dirs_gates_on_count_then_age() {
    let now = Utc::now();
    let root = std::path::Path::new("/sol/root");
    let make = |n: usize, days_ago: i64| crate::model::SolutionSessionMetadata {
        id: crate::model::SolutionSessionId::new(),
        solution_id: SolutionId("sol".into()),
        agent_id: SharedString::from("claude-acp"),
        acp_session_id: agent_client_protocol::schema::SessionId::new(format!("acp-{n}")),
        title: SharedString::from("s"),
        created_at: now,
        last_activity_at: now - chrono::Duration::days(days_ago),
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    };

    // <= the min-session gate: keep everything, even ancient archives.
    let small: Vec<_> = (0..ARCHIVE_REAP_MIN_SESSIONS)
        .map(|n| make(n, 999))
        .collect();
    assert!(
        stale_archive_dirs(root, &small, now).is_empty(),
        "small workspaces keep their full history"
    );

    // Over the gate: reap only the sessions inactive past the age cutoff.
    let recent: Vec<_> = (0..8).map(|n| make(n, 1)).collect();
    let stale: Vec<_> = (8..14)
        .map(|n| make(n, ARCHIVE_REAP_MAX_AGE_DAYS + 5))
        .collect();
    let mut metas = recent.clone();
    metas.extend(stale.iter().cloned());

    let reaped = stale_archive_dirs(root, &metas, now);
    assert_eq!(
        reaped.len(),
        stale.len(),
        "only the stale sessions are reaped"
    );
    for m in &stale {
        assert!(
            reaped.contains(&root.join(".agents").join(m.id.to_string())),
            "stale session {} must be reaped",
            m.id
        );
    }
    for m in &recent {
        assert!(
            !reaped.contains(&root.join(".agents").join(m.id.to_string())),
            "recently-active session {} must be kept",
            m.id
        );
    }
}

#[gpui::test]
async fn reset_context_swaps_acp_thread_without_bumping_count(cx: &mut TestAppContext) {
    let (session_id, old_thread, _tmp) = create_session_with_thread(cx).await;

    // Snapshot pre-reset state. Bump context_count to 7 so we can prove the
    // reset path does NOT touch it (rotate_context, by contrast, would
    // increment to 8). Also stamp a fake usage onto the old thread so a
    // later "no usage on the new thread" assertion has signal.
    let (old_acp_session_id, old_thread_id) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.context_count = 7);
            let s = session.read(cx);
            (s.acp_session_id.clone(), old_thread.entity_id())
        })
    });

    let result = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| store.reset_context(session_id, cx))
        })
        .await
        .expect("reset_context");
    assert_eq!(
        result, session_id,
        "reset_context returns the same SolutionSessionId"
    );

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            let new_thread = s.acp_thread().cloned().expect("new acp_thread populated");
            assert_ne!(
                new_thread.entity_id(),
                old_thread_id,
                "reset_context swapped the AcpThread entity"
            );
            assert_ne!(
                s.acp_session_id, old_acp_session_id,
                "new acp_session_id differs from the old one"
            );
            assert_eq!(
                s.context_count, 7,
                "context_count unchanged (rotate_context would have bumped to 8)"
            );
            assert!(
                matches!(s.state, SessionState::Idle),
                "state is Idle, got {:?}",
                s.state
            );
            assert!(
                new_thread.read(cx).entries().is_empty(),
                "new thread has no entries"
            );
        });
    });
}

#[gpui::test]
async fn late_send_error_is_dropped_when_session_was_reset(cx: &mut TestAppContext) {
    // Race regression guard: `/clear` (reset_context) swapping the
    // AcpThread mid-turn must not let the OLD turn's late `Err`
    // clobber the freshly-Idle state with `Errored("...")`. Without
    // the `expected_acp_session_id` check in `send_message_blocks`,
    // this test fails — the dropped gate makes the mock prompt return
    // Err, which the spawn's Err branch unconditionally writes as
    // `SessionState::Errored(...)`.
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let (prompt_gate_tx, prompt_gate_rx) = async_channel::bounded::<()>(1);
    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_prompt_gate(
                    connect_count.clone(),
                    prompt_gate_rx,
                )),
            );
        });
    });

    let session_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");

    // Send a message; the mock will park on the gate.
    let send_task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.send_message(session_id, "hi".into(), cx)
        })
    });

    // Reset the session while the in-flight prompt is still parked. The
    // new ACP thread takes over; state should land on `Idle`.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            assert!(
                matches!(session.read(cx).state, SessionState::Idle),
                "post-reset state is Idle, got {:?}",
                session.read(cx).state
            );
        });
    });

    // Now release the OLD turn with an error (drop the sender without
    // sending) — without the rotation-race guard, this clobbers Idle
    // with `Errored`.
    prompt_gate_tx.close();
    drop(prompt_gate_tx);
    // The spawned send_task should now resolve to Err. We don't care
    // about its return value; we only care that the side-effect on
    // SessionState was suppressed because the acp_session_id changed.
    let _ = send_task.await;
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            assert!(
                matches!(session.read(cx).state, SessionState::Idle),
                "late Err on rotated session must NOT clobber Idle, got {:?}",
                session.read(cx).state
            );
        });
    });
}

#[gpui::test]
async fn restore_open_tabs_hydrates_cold_sessions(cx: &mut TestAppContext) {
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let id_b = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();

    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("session A"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    };
    let meta_b = crate::model::SolutionSessionMetadata {
        id: id_b,
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-b"),
        title: SharedString::from("session B"),
        ..meta_a.clone()
    };
    db.save_metadata(meta_a).await.expect("meta a");
    db.save_metadata(meta_b).await.expect("meta b");

    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "session A".into(),
        entries: vec![PersistedEntry {
            role: PersistedRole::User,
            markdown: "first prompt".into(),
        }],
        entry_summaries: vec!["first prompt".into()],
        entries_v2: vec![],
        entry_created_ms: vec![],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");

    db.update_tab_orders(solution_id.clone(), vec![id_b, id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_b, id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            let sb = store.session(id_b).expect("session B restored");
            sa.read_with(cx, |s, _| {
                assert!(s.is_cold(), "restored session should be cold");
                assert_eq!(s.entries.len(), 1);
                // v1 blobs hydrate as Assistant-shaped legacy rows
                // (the old `role` field is no longer round-tripped —
                // structured v2 carries the real role per variant).
                assert!(matches!(
                    s.entries[0].kind,
                    crate::session_entry::SessionEntryKind::AssistantMessage { .. }
                ));
            });
            sb.read_with(cx, |s, _| {
                assert!(s.is_cold());
                // No blob saved for B → entries empty.
                assert!(s.entries.is_empty());
            });
            // sessions_for is what the navigator's reconcile path
            // reads; insertion order into `by_solution` must match
            // the `tab_order ASC` returned by the DB so the strip
            // ends up identical to what the user closed last time.
            let listed: Vec<_> = store
                .sessions_for(&solution_id)
                .into_iter()
                .map(|entity| entity.read(cx).id)
                .collect();
            assert_eq!(listed, vec![id_b, id_a]);
        });
    });
}

/// End-to-end regression for the "session not found" / `unknown session`
/// bug after restarting on a brand-new chat that never received a message.
///
/// `create_session_with_parent` persists the metadata row (`save_metadata`)
/// and the strip position (`update_tab_orders`) as two independent detached
/// DB writes with no happens-before. `update_tab_orders` is UPDATE-only, so
/// if it wins the race against the metadata INSERT it no-ops (no row yet),
/// and the INSERT used to land the row with `tab_order = NULL` — invisible to
/// `select_open_tabs` / `restore_open_tabs`, so the session was never
/// re-hydrated on restart. The fix re-persists the row AFTER pinning so the
/// metadata write carries the real tab_order, and the INSERT's COALESCE
/// ON CONFLICT keeps it order-independent. Here we drive the real create flow
/// with persistence wired, let every detached write drain, and assert the row
/// is durably pinned even though the session never received a message.
#[gpui::test]
async fn create_session_persists_tab_order_for_restart(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let connect_count = Arc::new(AtomicUsize::new(0));
    let db = {
        let executor = cx.executor();
        Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"))
    };
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::new(connect_count.clone())),
            );
            store.set_persistence(db.clone(), cx);
        });
    });

    let session_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");

    // Drain the detached metadata / tab_order writes issued by the create flow.
    cx.run_until_parked();

    // The session never received a message — but its strip position must still
    // be durable, so a restart's `restore_open_tabs` (which queries
    // `list_open_tabs`) re-hydrates it instead of raising "unknown session".
    let open_tabs = db
        .list_open_tabs(solution_id.clone())
        .await
        .expect("list_open_tabs");
    assert_eq!(
        open_tabs,
        vec![session_id],
        "a freshly-created, never-messaged session must persist its tab_order \
         so it survives an editor restart"
    );

    // The metadata row itself must carry the concrete tab_order (not NULL):
    // this proves the create flow re-persists the row AFTER pinning, so the
    // value is durable regardless of which detached write won the race.
    let metas = db
        .list_for_solution(solution_id.clone())
        .await
        .expect("list_for_solution");
    let row = metas
        .iter()
        .find(|m| m.id == session_id)
        .expect("metadata row for created session");
    assert_eq!(
        row.tab_order,
        Some(0),
        "the persisted metadata row must carry the strip position, not NULL"
    );
}

/// Regression for the close→reopen empty-history bug: the extracted
/// blob→cold_entries helper must produce exactly the same shape from
/// the same input regardless of which call site invokes it. Pre-fix,
/// the v2 reconstruction was inlined in `restore_open_tabs` only; the
/// `resume_session` ELSE branch silently created an empty
/// `cold_entries` because `claude --resume` doesn't re-emit the
/// transcript. This test pins the helper's contract: a structured v2
/// blob round-trips into a same-length `AgentThreadEntry` vector
/// (one entry per `PersistedEntryV2`) and a 1:1 `entry_created_ms`
/// vector — so a future inline-it-back regression in either call
/// site fails here, not silently in the UI.
#[gpui::test]
async fn cold_entries_from_persisted_v2_reconstructs_per_entry(cx: &mut TestAppContext) {
    use crate::cold_persistence::{
        PersistedAssistantChunk, PersistedAssistantMessage, PersistedEntryV2, PersistedUserMessage,
    };
    let persisted = PersistedSession {
        title: "demo".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![
            PersistedEntryV2::User(PersistedUserMessage {
                id: None,
                content_md: "first prompt".into(),
                chunks: vec![],
            }),
            PersistedEntryV2::Assistant(PersistedAssistantMessage {
                chunks: vec![PersistedAssistantChunk::Message("reply".into())],
            }),
        ],
        entry_created_ms: vec![1_700_000_000_000, 1_700_000_001_000],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    };
    let (cold_entries, created_ms) =
        cx.update(|cx| crate::store::cold_entries_from_persisted(Some(persisted), cx));
    assert_eq!(cold_entries.len(), 2, "v2 reconstruction must be 1:1");
    assert_eq!(created_ms, vec![1_700_000_000_000, 1_700_000_001_000]);
    assert!(matches!(
        cold_entries[0],
        acp_thread::AgentThreadEntry::UserMessage(_)
    ));
    assert!(matches!(
        cold_entries[1],
        acp_thread::AgentThreadEntry::AssistantMessage(_)
    ));

    // None-blob path returns empty vectors (no panic, no garbage).
    let (cold_entries, created_ms) =
        cx.update(|cx| crate::store::cold_entries_from_persisted(None, cx));
    assert!(cold_entries.is_empty());
    assert!(created_ms.is_empty());
}

#[test]
fn persisted_session_roundtrips_with_structured_entries() {
    let original = PersistedSession {
        title: "demo".into(),
        entries: vec![
            PersistedEntry {
                role: PersistedRole::User,
                markdown: "Hello".into(),
            },
            PersistedEntry {
                role: PersistedRole::Assistant,
                markdown: "Hi there!".into(),
            },
            PersistedEntry {
                role: PersistedRole::Tool,
                markdown: "ran tool x".into(),
            },
        ],
        entry_summaries: vec!["Hello".into(), "Hi there!".into(), "ran tool x".into()],
        entries_v2: vec![],
        entry_created_ms: vec![],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    };
    let bytes = serde_json::to_vec(&original).unwrap();
    let decoded: PersistedSession = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.title, original.title);
    assert_eq!(decoded.entries.len(), 3);
    assert!(matches!(decoded.entries[0].role, PersistedRole::User));
    assert!(matches!(decoded.entries[1].role, PersistedRole::Assistant));
    assert!(matches!(decoded.entries[2].role, PersistedRole::Tool));
    assert_eq!(decoded.entries[0].markdown, "Hello");
    assert_eq!(decoded.entry_summaries.len(), 3);
}

/// Task 5: cold-restored session exposes transcript via `entries` (not `cold_entries`).
/// `is_cold()` is true (no live thread) and `entries` is non-empty.
#[gpui::test]
async fn cold_restore_populates_entries_directly(cx: &mut TestAppContext) {
    use crate::cold_persistence::{
        PersistedAssistantChunk, PersistedAssistantMessage, PersistedEntryV2, PersistedUserMessage,
    };
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();

    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("session A"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    };
    db.save_metadata(meta_a).await.expect("meta a");

    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "session A".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![
            PersistedEntryV2::User(PersistedUserMessage {
                id: None,
                content_md: "first prompt".into(),
                chunks: vec![],
            }),
            PersistedEntryV2::Assistant(PersistedAssistantMessage {
                chunks: vec![PersistedAssistantChunk::Message("reply".into())],
            }),
        ],
        entry_created_ms: vec![1_700_000_000_000, 1_700_000_001_000],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            sa.read_with(cx, |s, _| {
                assert!(s.is_cold(), "restored session should be cold");
                assert_eq!(
                    s.entries.len(),
                    2,
                    "entries must hold the 2 restored entries"
                );
                assert_eq!(
                    s.live_base, 0,
                    "cold session has live_base = 0 (no live thread)"
                );
                assert!(
                    matches!(
                        s.entries[0].kind,
                        crate::session_entry::SessionEntryKind::UserMessage { .. }
                    ),
                    "first entry must be UserMessage"
                );
                assert!(
                    matches!(
                        s.entries[1].kind,
                        crate::session_entry::SessionEntryKind::AssistantMessage { .. }
                    ),
                    "second entry must be AssistantMessage"
                );
                assert_eq!(s.entries[0].created_ms, 1_700_000_000_000);
                assert_eq!(s.entries[1].created_ms, 1_700_000_001_000);
            });
        });
    });
}

#[test]
fn persisted_session_legacy_blob_decodes_with_empty_entries() {
    let legacy_json = serde_json::json!({
        "title": "old session",
        "entry_summaries": ["one", "two"],
    });
    let bytes = serde_json::to_vec(&legacy_json).unwrap();
    let decoded: PersistedSession = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.title, "old session");
    assert!(
        decoded.entries.is_empty(),
        "legacy blobs have no entries field"
    );
    assert_eq!(
        decoded.entry_summaries,
        vec!["one".to_string(), "two".to_string()]
    );
}

// ----- Task 4: row-based cold load + lazy blob→rows migration -----

/// `entries_from_rows` unit test: a corrupt-payload row is skipped (log::warn,
/// not a panic) while every well-formed row decodes IN ORDER, preserving the
/// per-row meta (mod_seq / created_ms / subagent_id).
#[test]
fn entries_from_rows_skips_corrupt_and_preserves_order() {
    let good_user = crate::session_entry::SessionEntryKind::UserMessage {
        id: None,
        content_md: "hello".into(),
        chunks: vec![],
    };
    let good_assistant = crate::session_entry::SessionEntryKind::AssistantMessage {
        chunks: vec![crate::session_entry::AssistantChunk::Message("hi".into())],
    };
    let rows = vec![
        crate::db::EntryRow {
            idx: 0,
            mod_seq: 1,
            created_ms: 1_700_000_000_000,
            subagent_id: None,
            payload: serde_json::to_vec(&good_user).unwrap(),
        },
        crate::db::EntryRow {
            idx: 1,
            mod_seq: 2,
            created_ms: 1_700_000_001_000,
            subagent_id: None,
            payload: b"{not valid json".to_vec(),
        },
        crate::db::EntryRow {
            idx: 2,
            mod_seq: 3,
            created_ms: 1_700_000_002_000,
            subagent_id: Some("sub-7".into()),
            payload: serde_json::to_vec(&good_assistant).unwrap(),
        },
    ];

    let entries = crate::store::entries_from_rows(rows);
    assert_eq!(entries.len(), 2, "the corrupt middle row must be dropped");
    assert!(matches!(
        entries[0].kind,
        crate::session_entry::SessionEntryKind::UserMessage { .. }
    ));
    assert_eq!(entries[0].mod_seq, 1);
    assert_eq!(entries[0].created_ms, 1_700_000_000_000);
    assert_eq!(entries[0].subagent_id, None);
    assert!(matches!(
        entries[1].kind,
        crate::session_entry::SessionEntryKind::AssistantMessage { .. }
    ));
    assert_eq!(entries[1].mod_seq, 3);
    assert_eq!(entries[1].created_ms, 1_700_000_002_000);
    assert_eq!(
        entries[1].subagent_id,
        Some(SharedString::from("sub-7")),
        "subagent_id column must carry over"
    );
}

/// (a) A session whose transcript is already stored as ROWS (no blob touched)
/// cold-restores from those rows and reads the persisted epoch verbatim
/// (NO bump — a restart loading the same transcript must not look like a new
/// generation to the mobile delta client).
#[gpui::test]
async fn cold_restore_loads_from_rows_and_reads_epoch(cx: &mut TestAppContext) {
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("session A"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    };
    db.save_metadata(meta_a).await.expect("meta a");

    // Write rows directly (the row-native shape) + a non-trivial epoch.
    let user = crate::session_entry::SessionEntryKind::UserMessage {
        id: None,
        content_md: "first prompt".into(),
        chunks: vec![],
    };
    let assistant = crate::session_entry::SessionEntryKind::AssistantMessage {
        chunks: vec![crate::session_entry::AssistantChunk::Message(
            "reply".into(),
        )],
    };
    db.upsert_entry(
        id_a,
        0,
        1,
        1_700_000_000_000,
        None,
        serde_json::to_vec(&user).unwrap(),
    )
    .await
    .expect("row 0");
    db.upsert_entry(
        id_a,
        1,
        2,
        1_700_000_001_000,
        None,
        serde_json::to_vec(&assistant).unwrap(),
    )
    .await
    .expect("row 1");
    db.save_epoch(id_a, 7).await.expect("epoch");
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(s.entries.len(), 2, "entries must come from the 2 rows");
                assert!(matches!(
                    s.entries[0].kind,
                    crate::session_entry::SessionEntryKind::UserMessage { .. }
                ));
                assert!(matches!(
                    s.entries[1].kind,
                    crate::session_entry::SessionEntryKind::AssistantMessage { .. }
                ));
                assert_eq!(s.entries[0].mod_seq, 1);
                assert_eq!(s.entries[1].mod_seq, 2);
                assert_eq!(s.entries[0].created_ms, 1_700_000_000_000);
                assert_eq!(s.entries[1].created_ms, 1_700_000_001_000);
                assert_eq!(
                    s.epoch, 7,
                    "rows branch must READ the persisted epoch, not bump it"
                );
            });
        });
    });
}

/// Phase 5, Task 5.1b core regression: a session whose `change_seq` advanced
/// PAST `max(mod_seq)` via section bumps (state/queue/subagents) — without
/// creating an entry — persists that `change_seq` and, on cold restore, anchors
/// on the PERSISTED value (NOT `max(mod_seq)`), seeding the three watermarks
/// above it. Pre-fix the cursor reseated to `max(mod_seq)`, dropping below an
/// already-issued client cursor and silently losing every new entry.
#[gpui::test]
async fn cold_restore_anchors_change_seq_on_persisted_value(cx: &mut TestAppContext) {
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("session A"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    };
    db.save_metadata(meta_a).await.expect("meta a");

    // 2 entries → max(mod_seq) = 2. The persisted change_seq is 9: it advanced
    // past max(mod_seq) via section watermark bumps (state/queue/subagents
    // transitions) that allocate change_seq without stamping an entry.
    let user = crate::session_entry::SessionEntryKind::UserMessage {
        id: None,
        content_md: "first prompt".into(),
        chunks: vec![],
    };
    let assistant = crate::session_entry::SessionEntryKind::AssistantMessage {
        chunks: vec![crate::session_entry::AssistantChunk::Message(
            "reply".into(),
        )],
    };
    db.upsert_entry(
        id_a,
        0,
        1,
        1_700_000_000_000,
        None,
        serde_json::to_vec(&user).unwrap(),
    )
    .await
    .expect("row 0");
    db.upsert_entry(
        id_a,
        1,
        2,
        1_700_000_001_000,
        None,
        serde_json::to_vec(&assistant).unwrap(),
    )
    .await
    .expect("row 1");
    const PERSISTED_CHANGE_SEQ: i64 = 9;
    db.save_change_seq(id_a, PERSISTED_CHANGE_SEQ)
        .await
        .expect("change_seq");
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_a]);

    let next_live_mod_seq = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            sa.update(cx, |s, _| {
                assert_eq!(s.entries.len(), 2, "entries restored from rows");
                // Anchored on the PERSISTED change_seq (9), then the 3 section
                // watermarks are allocated off the shared clock above it, so
                // change_seq lands at anchor + 3 = 12. The discriminating fact:
                // had it reseated from max(mod_seq)=2 (the pre-fix behavior) it
                // would be 5, not 12 — so 12 proves the persisted anchor was used.
                assert_eq!(
                    s.change_seq, 12,
                    "change_seq = persisted anchor 9 + 3 watermark bumps; NOT the \
                     max(mod_seq)=2 → 5 the pre-fix path would produce"
                );
                assert!(
                    s.change_seq >= PERSISTED_CHANGE_SEQ as u64,
                    "restored change_seq must stay >= the persisted cursor (monotonic)"
                );
                // The three section watermarks seed strictly above the anchor.
                assert_eq!(s.queue_seq, 10, "queue_seq = anchor + 1");
                assert_eq!(s.subagents_seq, 11, "subagents_seq = anchor + 2");
                assert_eq!(s.state_seq, 12, "state_seq = anchor + 3");
                for w in [s.queue_seq, s.subagents_seq, s.state_seq] {
                    assert!(
                        w > PERSISTED_CHANGE_SEQ as u64,
                        "watermark {w} must exceed the persisted cursor"
                    );
                }
                // Lost-entry guard: a fresh live NewEntry stamps the NEXT
                // change_seq, which must exceed the cursor a delta client was
                // already handed (= the restored change_seq, 9). If the cursor
                // had reseated to max(mod_seq)=2, this stamp would be < 9 and the
                // entry would silently drop out of every delta with since_seq=9.
                s.bump_change_seq()
            })
        })
    });
    assert!(
        next_live_mod_seq > PERSISTED_CHANGE_SEQ as u64,
        "a new live entry's mod_seq ({next_live_mod_seq}) must exceed the previously \
         issued client cursor ({PERSISTED_CHANGE_SEQ}) — lost-entry guard"
    );
}

/// Phase 5, Task 5.1b legacy fallback: a session row with a NULL `change_seq`
/// column (predates the feature; no delta client could have been issued a
/// cursor) cold-restores with `change_seq` anchored on `max(mod_seq)`.
#[gpui::test]
async fn cold_restore_legacy_null_change_seq_falls_back_to_max_mod_seq(cx: &mut TestAppContext) {
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-legacy"),
        title: SharedString::from("legacy session"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    };
    db.save_metadata(meta_a).await.expect("meta a");

    let user = crate::session_entry::SessionEntryKind::UserMessage {
        id: None,
        content_md: "first prompt".into(),
        chunks: vec![],
    };
    let assistant = crate::session_entry::SessionEntryKind::AssistantMessage {
        chunks: vec![crate::session_entry::AssistantChunk::Message(
            "reply".into(),
        )],
    };
    db.upsert_entry(
        id_a,
        0,
        1,
        1_700_000_000_000,
        None,
        serde_json::to_vec(&user).unwrap(),
    )
    .await
    .expect("row 0");
    db.upsert_entry(
        id_a,
        1,
        2,
        1_700_000_001_000,
        None,
        serde_json::to_vec(&assistant).unwrap(),
    )
    .await
    .expect("row 1");
    // Intentionally do NOT call save_change_seq → column stays NULL.
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.restore_open_tabs(solution_id.clone(), cx)
        })
    })
    .await
    .expect("restore");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            sa.read_with(cx, |s, _| {
                // Legacy fallback: anchor = max(mod_seq) = 2, then +3 watermark
                // bumps → change_seq 5, watermarks 3/4/5 (identical to the
                // pre-Task-5.1b `init_change_seq_from_entries` behavior).
                assert_eq!(
                    s.change_seq, 5,
                    "NULL change_seq column must fall back to the max(mod_seq)=2 anchor \
                     (→ change_seq 5 after the 3 watermark bumps)"
                );
                assert_eq!(s.queue_seq, 3);
                assert_eq!(s.subagents_seq, 4);
                assert_eq!(s.state_seq, 5);
            });
        });
    });
}

/// (b) A v2 blob with NO rows migrates to rows on cold-restore: `entries`
/// matches the blob, `db.load_entries` becomes non-empty, and a SECOND
/// cold-restore returns the same entries straight from rows (idempotent — no
/// double-migrate, the blob is preserved as the model/effort fallback).
#[gpui::test]
async fn v2_blob_migrates_to_rows_and_is_idempotent(cx: &mut TestAppContext) {
    use crate::cold_persistence::{
        PersistedAssistantChunk, PersistedAssistantMessage, PersistedEntryV2, PersistedUserMessage,
    };
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("session A"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    };
    db.save_metadata(meta_a).await.expect("meta a");

    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "session A".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![
            PersistedEntryV2::User(PersistedUserMessage {
                id: None,
                content_md: "first prompt".into(),
                chunks: vec![],
            }),
            PersistedEntryV2::Assistant(PersistedAssistantMessage {
                chunks: vec![PersistedAssistantChunk::Message("reply".into())],
            }),
        ],
        entry_created_ms: vec![1_700_000_000_000, 1_700_000_001_000],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");
    // No rows written: this is the lazy-migration trigger.
    assert!(
        db.load_entries(id_a).await.expect("load rows").is_empty(),
        "precondition: no rows before migration"
    );
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(s.entries.len(), 2, "entries must match the v2 blob");
                assert!(matches!(
                    s.entries[0].kind,
                    crate::session_entry::SessionEntryKind::UserMessage { .. }
                ));
                assert!(matches!(
                    s.entries[1].kind,
                    crate::session_entry::SessionEntryKind::AssistantMessage { .. }
                ));
            });
        });
    });

    // The migration (persist_all_rows) is spawned + detached; let it land.
    cx.run_until_parked();

    let rows = db
        .load_entries(id_a)
        .await
        .expect("load rows after migrate");
    assert_eq!(rows.len(), 2, "migration must have written rows");

    // Second cold-restore: drop the in-memory session, restore again — now the
    // rows branch must serve the same entries (idempotent, no double-migrate).
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.sessions.remove(&id_a);
            store.by_solution.remove(&solution_id);
        });
    });
    let ordered2 = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore 2");
    assert_eq!(ordered2, vec![id_a]);
    cx.run_until_parked();

    let rows_after = db
        .load_entries(id_a)
        .await
        .expect("load rows after 2nd restore");
    assert_eq!(
        rows_after.len(),
        2,
        "second restore must NOT double-migrate (still exactly 2 rows)"
    );
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session A re-restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(
                    s.entries.len(),
                    2,
                    "2nd restore loads same entries from rows"
                );
            });
        });
    });
}

/// Regression guard: a v2 blob with `desired_model` set migrates on the first
/// restore (MIGRATE branch recovers model from blob and writes rows). On the
/// SECOND cold-restore (ROWS branch — no blob deserialization), the session
/// must still carry the same `desired_model`. This proves that
/// `persist_session_row` is called during migration, flushing the recovered
/// model/effort to the metadata columns before the blob path is bypassed.
#[gpui::test]
async fn migrated_session_retains_model_on_second_restore(cx: &mut TestAppContext) {
    use crate::cold_persistence::{PersistedEntryV2, PersistedUserMessage};
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = chrono::Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("model session"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: std::path::PathBuf::new(),
        parent_session_id: None,
        // No model in the DB metadata column yet — simulates pre-Task-3a row.
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    };
    db.save_metadata(meta_a).await.expect("meta a");

    // Write a v2 blob that carries desired_model; no rows yet (migration trigger).
    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "model session".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![PersistedEntryV2::User(PersistedUserMessage {
            id: None,
            content_md: "hello".into(),
            chunks: vec![],
        })],
        entry_created_ms: vec![1_700_000_000_000],
        available_models: vec![],
        desired_model: Some("some-model".into()),
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");
    assert!(
        db.load_entries(id_a).await.expect("load rows").is_empty(),
        "precondition: no rows before first restore"
    );
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    // First restore — MIGRATE branch: recovers desired_model from blob.
    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("first restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(
                    s.desired_model.as_deref(),
                    Some("some-model"),
                    "migrate branch must recover desired_model from blob"
                );
            });
        });
    });

    // Let persist_all_rows + persist_session_row tasks land.
    cx.run_until_parked();

    // Confirm rows were written and metadata column was backfilled.
    let rows = db
        .load_entries(id_a)
        .await
        .expect("load rows after migrate");
    assert_eq!(rows.len(), 1, "migration must have written 1 row");
    let metas = db
        .list_for_solution(solution_id.clone())
        .await
        .expect("list metas");
    let db_meta = metas.iter().find(|m| m.id == id_a).expect("meta in db");
    assert_eq!(
        db_meta.desired_model.as_deref(),
        Some("some-model"),
        "persist_session_row must have written desired_model to the metadata column"
    );

    // Drop the in-memory session so the second restore starts cold.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.sessions.remove(&id_a);
            store.by_solution.remove(&solution_id);
        });
    });

    // Second restore — ROWS branch: no blob deserialization, reads columns only.
    let ordered2 = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("second restore");
    assert_eq!(ordered2, vec![id_a]);
    cx.run_until_parked();

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let sa = store.session(id_a).expect("session re-restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(
                    s.desired_model.as_deref(),
                    Some("some-model"),
                    "second restore (rows branch) must retain desired_model from column"
                );
            });
        });
    });
}

/// (c) MANDATORY Phase-2 regression guard: a LEGACY v1 blob (entries_v2 EMPTY,
/// entry_summaries populated) migrates losslessly — `entries` carries the
/// summary text (history NOT lost) and rows are written. This is the exact
/// regression Phase 2 fixed; it must stay fixed.
#[gpui::test]
async fn legacy_v1_blob_migrates_losslessly(cx: &mut TestAppContext) {
    let (solution_id, _tmp, _project) = setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let id_a = crate::model::SolutionSessionId::new();
    let agent_id = SharedString::from("claude-acp");
    let now = Utc::now();
    let meta_a = crate::model::SolutionSessionMetadata {
        id: id_a,
        solution_id: solution_id.clone(),
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("legacy session"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: None,
        desired_effort: None,
        cached_models: vec![],
        tab_order: None,
    };
    db.save_metadata(meta_a).await.expect("meta a");

    // Legacy v1 shape: entries_v2 EMPTY, history only in entry_summaries.
    let blob_a = serde_json::to_vec(&PersistedSession {
        title: "legacy session".into(),
        entries: vec![],
        entry_summaries: vec![
            "user said hello".to_string(),
            "assistant replied hi".to_string(),
        ],
        entries_v2: vec![],
        entry_created_ms: vec![],
        available_models: vec![],
        desired_model: None,
        desired_effort: None,
    })
    .unwrap();
    db.save_blob(id_a, blob_a).await.expect("blob a");
    db.update_tab_orders(solution_id.clone(), vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id.clone(), cx)
            })
        })
        .await
        .expect("restore");
    assert_eq!(ordered, vec![id_a]);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sa = store.session(id_a).expect("legacy session restored");
            sa.read_with(cx, |s, _| {
                assert_eq!(
                    s.entries.len(),
                    2,
                    "legacy v1 history must NOT be lost (Phase-2 regression guard)"
                );
                // Legacy summaries hydrate as Assistant-shaped entries carrying
                // the flat markdown text.
                let carries_text = s.entries.iter().any(|e| {
                    matches!(
                        &e.kind,
                        crate::session_entry::SessionEntryKind::AssistantMessage { chunks }
                            if chunks.iter().any(|c| matches!(
                                c,
                                crate::session_entry::AssistantChunk::Message(m)
                                    if m.contains("user said hello")
                            ))
                    )
                });
                assert!(carries_text, "summary text must survive into entries");
            });
        });
    });

    // Migration writes rows. As of phase 6b the persist authority is the
    // COALESCED Main stream (`streams[StreamId::Main]`), not the flat `entries`:
    // the two legacy assistant-shaped summaries are adjacent same-source
    // (subagent_id None) messages, so demux merges them into ONE Main bubble and
    // migration writes ONE row. That is still lossless — both summary texts are
    // preserved as chunks inside the single coalesced row (asserted below) — and
    // the next restore is row-native.
    cx.run_until_parked();
    let rows = db
        .load_entries(id_a)
        .await
        .expect("load rows after migrate");
    assert_eq!(
        rows.len(),
        1,
        "legacy migration writes the coalesced Main stream as one row-native entry"
    );
    // Losslessness at the persist authority: the single coalesced row must carry
    // BOTH legacy summary texts (no history dropped by the coalesce-then-persist).
    let migrated_kind = crate::session_entry::kind_from_payload(&rows[0].payload)
        .expect("migrated row payload decodes to a kind");
    let crate::session_entry::SessionEntryKind::AssistantMessage { chunks } = migrated_kind else {
        panic!("legacy summaries must migrate as an AssistantMessage row");
    };
    let migrated_text: String = chunks
        .iter()
        .filter_map(|c| match c {
            crate::session_entry::AssistantChunk::Message(m) => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        migrated_text.contains("user said hello")
            && migrated_text.contains("assistant replied hi"),
        "coalesced migration row must preserve both legacy summaries, got: {migrated_text:?}"
    );
    // Blob must be PRESERVED (Task 5 owns blob removal + model/effort backfill).
    assert!(
        db.load_blob(id_a).await.expect("load blob").is_some(),
        "migration must NOT null the blob (model/effort fallback safety net)"
    );
}

/// `EntriesRemoved` covers thread-local truncation; the `cleared` arm
/// fires when `entries()` is empty after the event (the only in-tree
/// producer is rewind-to-zero from refusal-truncation). This test pins
/// that the live thread's `token_usage` and the session's
/// `cached_total_tokens`/`last_turn_duration` mirrors all reset on the
/// rewind-to-zero path; the partial-rewind sibling
/// (`entries_removed_partial_rewind_preserves_token_state`) pins the
/// negative case. The user-facing `/clear` flow is covered by
/// `reset_context_resets_token_meter`.
#[gpui::test]
async fn entries_removed_to_zero_resets_token_state(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Stamp pre-clear state on both the live thread (token_usage) and
    // the session (cached_total_tokens, last_turn_duration). All three
    // must be cleared on full /clear.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.update_token_usage(
                Some(acp_thread::TokenUsage {
                    used_tokens: 12_345,
                    max_tokens: 1_000_000,
                    ..Default::default()
                }),
                cx,
            );
        });
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.cached_total_tokens = Some(12_345);
                s.last_turn_duration = Some(std::time::Duration::from_secs(7));
            });
        });
    });

    // Emit EntriesRemoved. Range payload is informational; the handler
    // discriminates full clear vs partial rewind by checking
    // `entries().is_empty()` post-event. The mock thread starts empty
    // and we never appended, so this exercises the cleared-arm.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(0..0));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert!(
                s.cached_total_tokens.is_none(),
                "cached_total_tokens reset, got {:?}",
                s.cached_total_tokens
            );
            assert!(
                s.last_turn_duration.is_none(),
                "last_turn_duration reset, got {:?}",
                s.last_turn_duration
            );
        });
        let usage = acp_thread.read(cx).token_usage().cloned();
        assert!(
            usage.is_none(),
            "live thread token_usage reset, got {usage:?}"
        );
    });
}

/// Sibling of `entries_removed_full_clear_resets_token_state`: when
/// `EntriesRemoved` fires but the live thread still has surviving
/// entries (a `rewind` to a specific user message rather than a full
/// `/clear`), the agent will emit a fresh `TokenUsageUpdated` reflecting
/// the surviving prefix's usage — so we must NOT preemptively wipe
/// token state. This pins the partial-rewind branch; the existence of
/// this test plus the full-clear sibling means a future "always reset
/// on EntriesRemoved" mutation breaks one of them.
#[gpui::test]
async fn entries_removed_partial_rewind_preserves_token_state(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Seed the live thread with a surviving user entry so the handler's
    // `entries().is_empty()` discriminator returns false.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                "survivor".into(),
                cx,
            );
            t.update_token_usage(
                Some(acp_thread::TokenUsage {
                    used_tokens: 9_999,
                    max_tokens: 1_000_000,
                    ..Default::default()
                }),
                cx,
            );
        });
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.cached_total_tokens = Some(9_999);
                s.last_turn_duration = Some(std::time::Duration::from_secs(11));
            });
        });
    });

    // Emit EntriesRemoved over an arbitrary range — what discriminates
    // partial-rewind from full-clear is `entries().is_empty()` on the
    // live thread, not the event payload. With one surviving entry,
    // the cleared arm must be skipped.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(0..1));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert_eq!(
                s.cached_total_tokens,
                Some(9_999),
                "cached_total_tokens preserved on partial rewind",
            );
            assert_eq!(
                s.last_turn_duration,
                Some(std::time::Duration::from_secs(11)),
                "last_turn_duration preserved on partial rewind",
            );
        });
        let usage = acp_thread.read(cx).token_usage().cloned();
        assert!(
            usage.is_some_and(|u| u.used_tokens == 9_999),
            "live thread token_usage preserved on partial rewind",
        );
    });
}

/// Phase 4b regression: a rewind that splits a coalesced same-source assistant
/// group must re-stamp the surviving boundary entry so the per-stream delta
/// re-delivers it. Two consecutive parent `AssistantMessage`s coalesce into ONE
/// stream entry (keeping the FIRST fragment's mod_seq); removing the later
/// fragment shrinks that entry's content but leaves `total_count` unchanged, so
/// without the re-stamp a delta client caught up past the coalesced seq would
/// silently render stale text. The `EntriesRemoved` handler bumps the survivor's
/// mod_seq so the stream watermark rises above every issued cursor.
#[gpui::test]
async fn entries_removed_restamps_survivor_on_coalesce_split(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let asst = |n: u64, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::AssistantMessage {
            chunks: vec![AssistantChunk::Message(text.into())],
        },
    };
    let cursor_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.live_base = 0;
                s.entries = vec![
                    SessionEntry {
                        created_ms: 1,
                        mod_seq: 1,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "u".into(),
                            chunks: vec![],
                        },
                    },
                    asst(7, "a1 "),
                    asst(8, "a2"),
                ];
                s.change_seq = 8;
                s.rebuild_streams();
            });
            let s = session.read(cx);
            let main = &s.streams[&crate::stream::StreamId::Main];
            assert_eq!(main.entries.len(), 2, "user + coalesced(a1+a2)");
            main.seq
        })
    });
    assert_eq!(cursor_before, 8, "coalesced entry carries the first fragment's seq");

    // Remove only the last fragment (a2) at global index 2 → splits the group.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(2..3));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        let main = &s.streams[&crate::stream::StreamId::Main];
        assert_eq!(main.entries.len(), 2, "user + a1 (a2 removed)");
        assert!(
            main.seq > cursor_before,
            "survivor re-stamped so the delta re-delivers the shrunk entry (seq {} !> {cursor_before})",
            main.seq,
        );
    });
}

/// Phase 6b keystone: after the #3 revert `AcpThread` (and thus the flat
/// `session.entries` ingest mirror) may be TORN — a teammate chunk interleaved
/// between two parent deltas splits the parent into two flat entries. But the
/// PERSIST authority is now `streams[StreamId::Main]` (the coalesced demux), so
/// the demux re-unites the parent into ONE Main bubble and `persist_main_stream`
/// writes Main-LOCAL coalesced rows (subagent_id None), never the torn fragments.
/// This pins the whole coupling: torn flat entries → one Main bubble → coalesced
/// persisted rows → watermark advanced.
#[gpui::test]
async fn interleaved_flat_entries_persist_as_coalesced_main_rows(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let asst = |n: u64, subagent: Option<&str>, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: subagent.map(SharedString::from),
        kind: SessionEntryKind::AssistantMessage {
            chunks: vec![AssistantChunk::Message(text.into())],
        },
    };

    // Torn interleave: parent "Three ", teammate "noise", parent "scouts". In the
    // flat ingest buffer the parent is SPLIT across indices 0 and 2 (index 1 is
    // the teammate). The demux groups by source, so Main = coalesce(0, 2).
    let (main_len, main_seq) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, cx| {
                s.live_base = 0;
                s.change_seq = 3;
                s.set_entries(
                    vec![
                        asst(1, None, "Three "),
                        asst(2, Some("T1"), "noise"),
                        asst(3, None, "scouts"),
                    ],
                    cx,
                );
            });
            let s = session.read(cx);
            // Flat entries are TORN: three entries, the parent split at 0 and 2.
            assert_eq!(s.entries.len(), 3, "flat ingest mirror stays torn");
            assert_eq!(s.entries[0].subagent_id, None);
            assert_eq!(
                s.entries[1].subagent_id,
                Some(SharedString::from("T1")),
                "teammate chunk interleaves between the parent's two deltas"
            );
            assert_eq!(s.entries[2].subagent_id, None);
            // But the demux re-unites the parent into ONE Main bubble.
            let main = &s.streams[&crate::stream::StreamId::Main];
            assert_eq!(
                main.entries.len(),
                1,
                "the parent's two torn fragments coalesce into one Main bubble"
            );
            assert_eq!(main.entries[0].subagent_id, None);
            assert!(
                s.streams
                    .contains_key(&crate::stream::StreamId::Teammate("T1".into())),
                "the teammate gets its own stream, out of Main"
            );
            (main.entries.len(), main.seq)
        })
    });

    // Persist the Main stream and let the detached task land.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.persist_main_stream(session_id, cx);
        });
    });
    cx.executor().run_until_parked();

    // The persist watermark advanced to the Main stream's seq.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        assert_eq!(
            session.read(cx).persisted_main_seq,
            main_seq,
            "persisted_main_seq advanced to the Main stream watermark"
        );
    });

    // Persisted rows are the Main-LOCAL coalesced entries: exactly one row, no
    // teammate fragment, subagent_id None, and losslessly carrying both parent
    // deltas.
    let rows = db.load_entries(session_id).await.expect("load rows");
    assert_eq!(
        rows.len(),
        main_len,
        "row count == streams[Main].entries.len() (coalesced, no teammate row)"
    );
    assert_eq!(rows[0].idx, 0, "Main-local index");
    assert_eq!(
        rows[0].subagent_id, None,
        "persisted Main rows carry no subagent tag"
    );
    let kind = crate::session_entry::kind_from_payload(&rows[0].payload)
        .expect("row payload decodes");
    let crate::session_entry::SessionEntryKind::AssistantMessage { chunks } = kind else {
        panic!("expected an AssistantMessage row");
    };
    let text: String = chunks
        .iter()
        .filter_map(|c| match c {
            AssistantChunk::Message(m) => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("Three ") && text.contains("scouts"),
        "coalesced Main row must preserve both parent deltas, got: {text:?}"
    );
    assert!(
        !text.contains("noise"),
        "the teammate fragment must NOT leak into the persisted Main row"
    );
}

/// Phase-6b keystone regression: a pre-6b session persisted teammate-tagged rows
/// at GLOBAL flat indices, interleaved with Main rows. Under 6b, persistence keys
/// on Main-LOCAL indices, so on cold-load the physical row layout no longer
/// matches — the first incremental `persist_main_stream` would overwrite a Main
/// slot with the wrong entry (losing a Main message) and strand the stale tagged
/// row forever, unless the load forces a realign. `hydrate_streams_main_only`
/// seeds `persisted_main_seq = 0` whenever a hydration orphan (a legacy tagged
/// row) is present, so the first persist re-writes the WHOLE Main stream at
/// Main-local indices and `delete_entries_from(Main.len)` trims the leftovers.
#[gpui::test]
async fn legacy_teammate_tagged_rows_realign_to_main_local_on_cold_load(
    cx: &mut TestAppContext,
) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let asst = |n: u64, subagent: Option<&str>, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: subagent.map(SharedString::from),
        kind: SessionEntryKind::AssistantMessage {
            chunks: vec![AssistantChunk::Message(text.into())],
        },
    };
    let user = |n: u64, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: text.into(),
            chunks: vec![],
        },
    };

    // LEGACY on-disk layout: Main "alpha"@0, teammate "noise"@1, Main user
    // "bravo"@2. "bravo" is a USER message so it does NOT coalesce with "alpha":
    // Main is TWO entries whose Main-local indices (0, 1) differ from their
    // physical row idx (0, 2). Write them straight to the DB as a pre-6b build
    // would (tagged teammate row included).
    let legacy = [
        asst(1, None, "alpha"),
        asst(2, Some("T1"), "noise"),
        user(3, "bravo"),
    ];
    for (idx, entry) in legacy.iter().enumerate() {
        db.upsert_entry(
            session_id,
            idx as i64,
            entry.mod_seq as i64,
            entry.created_ms,
            entry.subagent_id.as_ref().map(|s| s.to_string()),
            entry.to_payload(),
        )
        .await
        .expect("seed legacy row");
    }

    // Cold-load: reconstruct the flat mirror from the legacy rows, then collapse
    // to a Main-only view (records T1 as a hydration orphan → watermark seeded 0).
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, cx| {
                s.entries = vec![
                    asst(1, None, "alpha"),
                    asst(2, Some("T1"), "noise"),
                    user(3, "bravo"),
                ];
                s.hydrate_streams_main_only();
                cx.notify();
            });
            let s = session.read(cx);
            // The flat mirror (3) is longer than the Main stream (2) because of
            // the tagged teammate row, so the realign trigger fires: watermark 0.
            assert_eq!(
                s.entries.len(),
                3,
                "flat mirror keeps the interleaved teammate row"
            );
            assert_eq!(
                s.streams[&crate::stream::StreamId::Main].entries.len(),
                2,
                "Main = [alpha, bravo]; the teammate is excluded"
            );
            assert_eq!(
                s.persisted_main_seq, 0,
                "legacy layout (flat longer than Main) forces a realign: watermark 0"
            );
        });
    });

    // A live resume appends one more Main message; the ingest rebuilds streams and
    // the first `persist_main_stream` after the realign-seed rewrites the Main
    // stream in full.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, cx| {
                // mod_seq 100 is comfortably above every loaded entry AND the 0
                // realign watermark, so it — and the re-written A/B — all persist.
                s.entries.push(asst(100, None, "charlie"));
                s.rebuild_streams();
                cx.notify();
            });
            store.persist_main_stream(session_id, cx);
        });
    });
    cx.executor().run_until_parked();

    // The realign rewrote the whole Main stream at contiguous Main-LOCAL indices
    // and trimmed the stale tagged row: 3 rows [alpha, bravo, charlie], all
    // subagent_id None, "bravo" preserved (NOT lost), teammate "noise" gone.
    let rows = db.load_entries(session_id).await.expect("load rows");
    assert_eq!(
        rows.len(),
        3,
        "exactly the 3 Main-local rows; the tagged teammate row was trimmed"
    );
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.idx, i as i64, "contiguous Main-local index");
        assert_eq!(
            row.subagent_id, None,
            "no teammate tag survives the realign"
        );
    }
    let texts: Vec<String> = rows
        .iter()
        .map(|r| {
            match crate::session_entry::kind_from_payload(&r.payload).expect("decode") {
                SessionEntryKind::AssistantMessage { chunks } => chunks
                    .iter()
                    .filter_map(|c| match c {
                        AssistantChunk::Message(m) => Some(m.clone()),
                        _ => None,
                    })
                    .collect(),
                SessionEntryKind::UserMessage { content_md, .. } => content_md,
                _ => String::new(),
            }
        })
        .collect();
    assert_eq!(
        texts,
        vec!["alpha", "bravo", "charlie"],
        "Main entries preserved + realigned; teammate 'noise' is gone"
    );
}

/// User-facing `/clear` is intercepted client-side and routed through
/// `reset_context`, which spawns a brand-new `AcpThread` (the old one
/// is dropped without emitting any events). Without an explicit reset
/// at the swap site, `cached_total_tokens` / `last_turn_duration` on
/// the session entity persist across the swap and the status-row meter
/// keeps reading the pre-clear count (because the meter falls back to
/// `cached_total_tokens` when the live thread has no `token_usage`,
/// which it doesn't on a fresh thread). This test pins the reset at
/// the swap site — the actual user-visible bug.
#[gpui::test]
async fn reset_context_resets_token_meter(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    // Stamp pre-clear cached values on the session.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.cached_total_tokens = Some(33_333);
                s.last_turn_duration = Some(std::time::Duration::from_secs(13));
            });
        });
    });

    // Drive the actual `/clear` flow.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert!(
                s.cached_total_tokens.is_none(),
                "cached_total_tokens reset, got {:?}",
                s.cached_total_tokens
            );
            assert!(
                s.last_turn_duration.is_none(),
                "last_turn_duration reset, got {:?}",
                s.last_turn_duration
            );
            // Sanity: live thread is fresh and has no usage.
            let new_thread = s.acp_thread().cloned().expect("new thread populated");
            assert!(
                new_thread.read(cx).token_usage().is_none(),
                "fresh thread has no token_usage"
            );
        });
    });
}

/// Same invariant for `/compact` (rotate_context) — same swap pattern,
/// same risk of stale meter.
#[gpui::test]
async fn rotate_context_resets_token_meter(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| {
                s.cached_total_tokens = Some(44_444);
                s.last_turn_duration = Some(std::time::Duration::from_secs(17));
            });
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            assert!(
                s.cached_total_tokens.is_none(),
                "cached_total_tokens reset on rotate, got {:?}",
                s.cached_total_tokens
            );
            assert!(
                s.last_turn_duration.is_none(),
                "last_turn_duration reset on rotate, got {:?}",
                s.last_turn_duration
            );
        });
    });
}

/// `/clear` (reset_context) must emit `SessionContextReset` so remote
/// clients (mobile, WS proxy) learn the transcript was wiped — they
/// only get `SessionStateChanged` otherwise, which doesn't carry
/// "context gone" semantics, so their cached entry list goes stale
/// until a foreground/cold refresh.
#[gpui::test]
async fn reset_context_emits_session_context_reset(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    // Stamp a known context_count so the assertion below can verify it
    // is forwarded as-is (reset does NOT bump the counter).
    let stamped_count: crate::model::SessionContextCount = 4;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.context_count = stamped_count);
            cx.notify();
        });
    });

    let observed = Rc::new(std::cell::RefCell::new(Vec::<
        crate::model::SessionContextCount,
    >::new()));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let observed = observed.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionContextReset { id, context_count } = event
                && *id == session_id
            {
                observed.borrow_mut().push(*context_count);
            }
        })
    });

    // Phase 5 Task 5.3 Part C: confirm /clear bumps the transcript epoch.
    // The `agent_session_context_reset` push (forwarded from this very
    // SessionContextReset event by `event_sources`) is what tells the
    // cache-first mobile client to full-reload — it must coincide with an
    // epoch bump so the client's `(epoch, current_seq)` cursor invalidates.
    let epoch_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.read(cx).epoch
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");
    cx.executor().run_until_parked();

    let collected = observed.borrow().clone();
    assert_eq!(
        collected,
        vec![stamped_count],
        "/clear must fire exactly one SessionContextReset with the unchanged context_count",
    );

    let epoch_after = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.read(cx).epoch
    });
    assert_eq!(
        epoch_after,
        epoch_before + 1,
        "/clear must bump the session epoch so the mobile delta cursor invalidates",
    );
}

/// `/compact` (rotate_context) must emit `SessionContextReset` and the
/// `context_count` carried on the event must be the POST-rotation
/// value (previous + 1) — clients use it to render the "now on
/// rotation #N" badge without a follow-up `get_session` round-trip.
#[gpui::test]
async fn rotate_context_emits_session_context_reset_with_incremented_count(
    cx: &mut TestAppContext,
) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    let initial_count = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.read(cx).context_count
        })
    });

    let observed = Rc::new(std::cell::RefCell::new(Vec::<
        crate::model::SessionContextCount,
    >::new()));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let observed = observed.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionContextReset { id, context_count } = event
                && *id == session_id
            {
                observed.borrow_mut().push(*context_count);
            }
        })
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");
    cx.executor().run_until_parked();

    let collected = observed.borrow().clone();
    assert_eq!(
        collected,
        vec![initial_count.saturating_add(1)],
        "/compact must fire exactly one SessionContextReset with the incremented context_count",
    );
}

/// Regression: `/compact` (rotate_context) must preserve the session's
/// `cwd`. Earlier the new ACP thread was always spawned with
/// `work_dirs = solution.root` regardless of which member sub-dir the
/// tab was bound to, so the agent's bash tool silently switched from
/// e.g. `voxelcraft/` to the solution root after the first compact and
/// then failed any command depending on `Cargo.toml` / `.git`.
#[gpui::test]
async fn rotate_context_preserves_session_cwd(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    // Stamp a non-root cwd onto the session — simulating "tab was
    // opened against the `member-x` subdir".
    let member_cwd = std::path::PathBuf::from("/tmp/sol-x/member-x");
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.cwd = member_cwd.clone());
            cx.notify();
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let new_thread = session
                .read(cx)
                .acp_thread()
                .cloned()
                .expect("new thread populated");
            let paths = new_thread
                .read(cx)
                .work_dirs()
                .expect("work_dirs propagated to AcpThread")
                .paths()
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert_eq!(
                paths,
                vec![member_cwd.to_string_lossy().into_owned()],
                "rotate_context must reuse session.cwd, not solution.root"
            );
            // session.cwd itself must also be unchanged.
            assert_eq!(session.read(cx).cwd, member_cwd);
        });
    });
}

/// Same regression for `/clear` (reset_context): a context wipe must
/// not reset the session's working directory away from the member
/// sub-dir.
#[gpui::test]
async fn reset_context_preserves_session_cwd(cx: &mut TestAppContext) {
    let (session_id, _old_thread, _tmp) = create_session_with_thread(cx).await;

    let member_cwd = std::path::PathBuf::from("/tmp/sol-x/member-x");
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, _| s.cwd = member_cwd.clone());
            cx.notify();
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let new_thread = session
                .read(cx)
                .acp_thread()
                .cloned()
                .expect("new thread populated");
            let paths = new_thread
                .read(cx)
                .work_dirs()
                .expect("work_dirs propagated to AcpThread")
                .paths()
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert_eq!(
                paths,
                vec![member_cwd.to_string_lossy().into_owned()],
                "reset_context must reuse session.cwd, not solution.root"
            );
            assert_eq!(session.read(cx).cwd, member_cwd);
        });
    });
}

/// `build_session_meta` shapes the system prompt into the exact JSON
/// envelope claude-agent-acp expects: `{ "systemPrompt": { "append": "<text>" } }`.
/// A wrong key name or nesting level silently drops the prompt — the
/// agent ignores unknown `_meta` keys per the ACP spec, so a typo here
/// would not surface as an error and the bug would only manifest as
/// "agent has no idea it's in a Solution". Pin the shape AND the empty-
/// prompt None path so future adapter changes can't regress either.
#[gpui::test]
fn build_session_meta_emits_correct_json_shape(cx: &mut TestAppContext) {
    use crate::claude_adapter::{CLAUDE_ACP_AGENT_ID, ClaudeAcpAdapter};
    use solutions::{CatalogId, Solution, SolutionMember};

    let mut registry = AdapterRegistry::new();
    registry.register(Arc::new(ClaudeAcpAdapter));
    cx.update(|cx| SolutionAgentStore::init_global(cx, Arc::new(registry)));

    let solution = Solution {
        id: SolutionId("sol-meta".into()),
        name: "test-meta".into(),
        root: PathBuf::from("/tmp/sol-meta"),
        members: vec![SolutionMember {
            catalog_id: CatalogId("cat-foo".into()),
            local_path: PathBuf::from("/tmp/sol-meta/foo"),
        }],
        last_opened_at: Some(Utc::now()),
    };

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let meta = store
                .build_session_meta(
                    &SharedString::from(CLAUDE_ACP_AGENT_ID),
                    &solution,
                    None,
                    None,
                    cx,
                )
                .expect("registered ClaudeAcpAdapter produces a non-empty prompt");
            let system_prompt = meta
                .get("systemPrompt")
                .expect("meta carries `systemPrompt` key (camelCase, not snake_case — claude-agent-acp matches exactly)")
                .as_object()
                .expect("`systemPrompt` is an object (not a bare string — agent reads `append` field)");
            let append = system_prompt
                .get("append")
                .expect("`append` key present (vs. replacing the preset entirely)")
                .as_str()
                .expect("`append` value is a string");
            assert!(
                append.contains("/tmp/sol-meta"),
                "prompt mentions solution root, got {append:?}"
            );
            assert!(
                append.contains("foo"),
                "prompt mentions member project, got {append:?}"
            );

            // Unknown agent → None (registry lookup fails)
            let none_meta =
                store.build_session_meta(
                    &SharedString::from("not-registered"),
                    &solution,
                    None,
                    None,
                    cx,
                );
            assert!(none_meta.is_none(), "unknown agent yields None");
        });
    });

    // Empty-prompt branch: a registered adapter that produces an empty
    // string must yield None so we don't ship a `_meta.systemPrompt:
    // {append: ""}` envelope (claude-agent-acp would then append nothing
    // to the preset and the round-trip wastes bandwidth + clutters the
    // request log).
    struct EmptyAdapter;
    impl crate::adapter::SolutionAgentAdapter for EmptyAdapter {
        fn agent_id(&self) -> AgentServerId {
            SharedString::from("empty-adapter")
        }
        fn display_name(&self) -> SharedString {
            SharedString::from("empty")
        }
        fn icon(&self) -> ui::IconName {
            ui::IconName::Sparkle
        }
        fn build_initial_system_prompt(&self, _: &Solution) -> String {
            String::new()
        }
    }
    cx.update(|cx| {
        let mut empty_registry = AdapterRegistry::new();
        empty_registry.register(Arc::new(EmptyAdapter));
        SolutionAgentStore::init_global(cx, Arc::new(empty_registry));
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let meta = store.build_session_meta(
                &SharedString::from("empty-adapter"),
                &solution,
                None,
                None,
                cx,
            );
            assert!(meta.is_none(), "empty prompt yields None");
        });
    });
}

// =====================================================================
// Auto-wake for MCP `send_message_blocks` on cold sessions
// =====================================================================

/// Sending to a cold session whose owning Solution is gone from
/// `SolutionStore` returns the structured `unknown_solution` error —
/// not the legacy "session has no ACP thread yet" — so MCP clients can
/// distinguish "the agent isn't running yet (we'll wake it)" from
/// "this session is orphaned (give up)".
#[gpui::test]
async fn cold_send_unknown_solution_returns_structured_error(cx: &mut TestAppContext) {
    // Use a SolutionId that won't be in SolutionStore. We still need
    // SolutionStore initialised because `SolutionAgentStore::init_global`
    // subscribes to it.
    let dir = tempfile::tempdir().expect("tempdir");
    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        let solutions_store =
            solutions::SolutionStore::for_test(dir.path().join("solutions.json"), cx);
        solutions::install_global_for_test(solutions_store, cx);
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
    });

    let orphan_solution_id = SolutionId("orphan-sol".into());
    let session_id = SolutionSessionId::new();
    let agent_id = SharedString::from("mock-agent");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            insert_cold_session(
                session_id,
                orphan_solution_id.clone(),
                agent_id.clone(),
                None,
                None,
                store,
                cx,
            );
        });
    });

    let task = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("hello".to_string()),
            )];
            store.send_message_blocks(session_id, blocks, cx)
        })
    });

    let err = task.await.expect_err("orphan solution must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown_solution"),
        "expected structured 'unknown_solution' error, got {msg:?}"
    );
    assert!(
        !msg.contains("has no ACP thread yet"),
        "auto-wake should replace the legacy 'no ACP thread' error, got {msg:?}"
    );
}

/// Hot-path passthrough: when a session has a live `acp_thread`,
/// `send_message_blocks` flips the state to Running synchronously
/// without entering the wake path — the wake helper must not interfere
/// with already-attached sessions.
#[gpui::test]
async fn hot_send_does_not_enter_wake_path(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("hot path".to_string()),
            )];
            // Detach — we only care that the synchronous state flip
            // happened. The actual prompt path uses the MockConnection
            // which returns Err without a gate (see test_support); that
            // would arrive as `Errored` after the spawn resolves.
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let state = session.read(cx).state.clone();
            assert!(
                matches!(state, SessionState::Running { .. }),
                "hot path should flip to Running synchronously, got {state:?}"
            );
        });
    });
}

#[gpui::test]
async fn append_stamps_entry_created_ms_once_per_index(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append a user entry. `push_user_content_block` creates a new
    // UserMessage entry (no existing user message last), so `push_entry`
    // fires, which emits `AcpThreadEvent::NewEntry`.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Append an assistant entry. The thread's last entry is now UserMessage,
    // so `push_assistant_content_block` also calls `push_entry` → `NewEntry`.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let (stamp0, stamp1) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "two appends → two entries");
        (s.entries[0].created_ms, s.entries[1].created_ms)
    });
    assert!(stamp0 > 0, "first stamp must be positive");
    assert!(stamp1 >= stamp0, "timestamps are non-decreasing");

    // Now drive an in-place EntryUpdated on the last entry (streaming more
    // text into the existing assistant message). `push_assistant_content_block`
    // with an existing assistant entry as the last entry emits `EntryUpdated`,
    // NOT `NewEntry` — so the entries count and stamps must NOT change.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(" more text".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "in-place update must not add an entry");
        assert_eq!(
            s.entries[1].created_ms, stamp1,
            "existing stamp must be unchanged after EntryUpdated"
        );
    });
}

/// Phase 4 Task 3: each transcript mutation must write the matching
/// `solution_session_entries` rows incrementally (not a wholesale blob).
/// Drives NewEntry×2, EntryUpdated(last), then EntriesRemoved and asserts
/// `db.load_entries(session_id)` mirrors `session.entries` after every step:
/// same count, ascending idx 0..n, mod_seq parity, and the payload decoding
/// back to the entry's kind.
#[gpui::test]
async fn transcript_mutations_persist_entry_rows(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    // Assert the persisted rows match the in-memory Main stream exactly. As of
    // phase 6b the persist authority is `streams[StreamId::Main]` (coalesced,
    // Main-local index), NOT the flat `entries` ingest buffer — a rewind that
    // splits a coalesce group re-stamps the Main survivor's mod_seq without
    // touching the flat entry, so the two mod_seqs legitimately diverge.
    async fn assert_rows_match(
        cx: &mut TestAppContext,
        db: &crate::db::SolutionAgentDb,
        session_id: SolutionSessionId,
    ) {
        let entries = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session exists");
            session
                .read(cx)
                .streams
                .get(&crate::stream::StreamId::Main)
                .map(|s| s.entries.clone())
                .unwrap_or_default()
        });
        let rows = db.load_entries(session_id).await.expect("load entries");
        assert_eq!(
            rows.len(),
            entries.len(),
            "row count must match the in-memory Main stream"
        );
        for (idx, (row, entry)) in rows.iter().zip(entries.iter()).enumerate() {
            assert_eq!(row.idx, idx as i64, "rows must be in ascending idx order");
            assert_eq!(
                row.mod_seq, entry.mod_seq as i64,
                "row mod_seq must mirror the Main-stream entry's mod_seq"
            );
            let kind = crate::session_entry::kind_from_payload(&row.payload)
                .expect("payload decodes to a kind");
            assert_eq!(
                kind, entry.kind,
                "row payload must decode to the entry kind"
            );
        }
    }

    // NewEntry #1 (user message).
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    assert_rows_match(cx, &db, session_id).await;

    // NewEntry #2 (assistant message — distinct entry).
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    assert_rows_match(cx, &db, session_id).await;

    // EntryUpdated on the last (assistant) entry — streaming more text into
    // the existing entry emits EntryUpdated, not NewEntry. The row at that
    // idx must be re-upserted with the new payload + bumped mod_seq.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(" more".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    assert_rows_match(cx, &db, session_id).await;

    // EntriesRemoved(1..2) — drop the last entry. The persisted rows must
    // shrink in lockstep so a stale idx-1 row can't corrupt a cold load.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(1..2));
        });
    });
    cx.executor().run_until_parked();
    assert_rows_match(cx, &db, session_id).await;
    let remaining = db.load_entries(session_id).await.expect("load entries");
    assert_eq!(
        remaining.len(),
        1,
        "EntriesRemoved must delete the trailing row"
    );
}

/// Ephemeral supervisor judge/auditor sessions must leave NO durable trace: a
/// persisted row reloads (without the in-memory `is_supervisor_ephemeral` flag)
/// as a normal child session, leaking the judge's private reasoning as a
/// visible session chip after a restart. Every persist-to-DB path guards on the
/// flag, so an ephemeral session writes neither entry rows nor a
/// `solution_sessions` row, even when the entry-append + explicit flushes fire.
#[gpui::test]
async fn ephemeral_session_is_never_persisted(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));

    // Stamp the session ephemeral, then wire persistence.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| s.is_supervisor_ephemeral = true);
            store.set_persistence(db.clone(), cx);
        });
    });

    // Append an entry (NewEntry → the auto persist_upsert path) AND fire the
    // explicit row/all-rows flushes — every one must skip the ephemeral session.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(
                        "private supervisor reasoning".to_string(),
                    ),
                ),
                cx,
            );
        });
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.persist_session_row(session_id, cx);
            store.persist_all_rows(session_id, cx);
        });
    });
    cx.executor().run_until_parked();

    assert!(
        db.load_entries(session_id)
            .await
            .expect("load entries")
            .is_empty(),
        "ephemeral session entries must never be persisted"
    );
    assert!(
        db.load_change_seq(session_id)
            .await
            .expect("load change_seq")
            .is_none(),
        "ephemeral session must leave no solution_sessions row"
    );
}

#[gpui::test]
async fn append_after_resumed_unstamped_history_does_not_fabricate(cx: &mut TestAppContext) {
    use crate::model::NO_TIMESTAMP_MS;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append two entries (user + assistant). These get real stamps on the
    // normal path.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Simulate a resumed pre-feature session: force the two existing entries'
    // created_ms to the absent sentinel — as if they were loaded from a legacy
    // blob that had no timestamps. The next NewEntry must NOT overwrite these
    // (no restamp on pre-existing entries) and must give the new entry a real
    // positive stamp.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.update(cx, |s, _| {
            for e in s.entries.iter_mut() {
                e.created_ms = NO_TIMESTAMP_MS;
            }
        });
    });

    // Now the user sends a new message → a genuinely-new entry arrives at
    // `global_entry_index == 2` (the thread already has 2 historical entries).
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("new".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);

        // All three entries must be present (2 historical + 1 new).
        assert_eq!(s.entries.len(), 3, "gap-fill must keep entries aligned");
        // The two historical (pre-existing) entries keep their sentinel.
        assert_eq!(
            s.entries[0].created_ms, NO_TIMESTAMP_MS,
            "historical entry must keep sentinel, not be restamped"
        );
        assert_eq!(
            s.entries[1].created_ms, NO_TIMESTAMP_MS,
            "historical entry must keep sentinel, not be restamped"
        );
        // Only the just-appended entry gets a real positive timestamp.
        assert!(
            s.entries[2].created_ms > 0,
            "the genuinely-new entry must hold a real positive timestamp, got {}",
            s.entries[2].created_ms
        );
    });
}

#[gpui::test]
async fn reset_context_clears_entries(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append one user entry so `entries` is non-empty.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let len_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .entries
            .len()
    });
    assert_eq!(len_before, 1, "one append → one entry before reset");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    let len_after = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .entries
            .len()
    });
    assert_eq!(len_after, 0, "reset_context clears entries");
}

/// `EntriesRemoved` must truncate `session.entries` at `cold_count + range.start`,
/// keeping the surviving prefix aligned with the surviving thread entries.
/// This exercises the actual truncation path on a populated entries list;
/// `entries_removed_partial_rewind_preserves_token_state` covers the token
/// state side independently.
#[gpui::test]
async fn entries_removed_truncates_entries(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append two entries so entries.len() == 2.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("first".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("second".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let stamp0 = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.entries.len(),
            2,
            "two appends → two entries before removal"
        );
        s.entries[0].created_ms
    });

    // Emit EntriesRemoved(1..2) — removes the last entry. The live thread
    // still has one surviving entry (the user message), so this is a
    // partial rewind: the handler truncates entries to length 1 (cold=0 +
    // range.start=1) but does NOT reset token state.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntriesRemoved(1..2));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.entries.len(),
            1,
            "EntriesRemoved(1..2) must truncate entries to length 1"
        );
        assert_eq!(
            s.entries[0].created_ms, stamp0,
            "surviving entry's created_ms must be unchanged"
        );
    });
}

/// `rotate_context` swaps the underlying ACP thread and clears `entries`.
/// Without this, entries from the old thread would bleed into the new context.
#[gpui::test]
async fn rotate_context_clears_entries(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append one user entry so `entries` is non-empty before rotation.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let len_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .entries
            .len()
    });
    assert_eq!(len_before, 1, "one append → one entry before rotation");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");

    let len_after = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .entries
            .len()
    });
    assert_eq!(len_after, 0, "rotate_context clears entries");
}

/// The `AcpThreadEvent::EntryUpdated` handler debounces re-emits of
/// `SessionMessageAppended`: a 500 ms quiet window collapses a streaming
/// burst into a single emit, and a 2 s max-stale guard forces an emit even
/// when an entry is continuously dirty so the consumer never starves.
///
/// We observe the emit count by subscribing to the store's
/// `SessionMessageAppended` events directly (no MCP socket needed) and
/// driving GPUI's test clock with `advance_clock`.
#[gpui::test]
async fn entry_updated_burst_coalesces_then_force_emits(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Seed one real entry so EntryUpdated(0) targets a live index.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("seed".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Count SessionMessageAppended emits for our index via a store
    // subscription. The subscription is held for the test's lifetime.
    let appended = Rc::new(std::cell::RefCell::new(0usize));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let appended = appended.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionMessageAppended(id, idx) = event
                && *id == session_id
                && *idx == 0
            {
                *appended.borrow_mut() += 1;
            }
        })
    });

    // (a) Burst coalesces: fire several EntryUpdated(0) within the 500 ms
    // quiet window. Each replaces the prior debounce Task, cancelling its
    // timer, so only the final window's task survives.
    for _ in 0..5 {
        cx.update(|cx| {
            acp_thread.update(cx, |_t, cx| {
                cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
            });
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(50));
    }
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        0,
        "no emit before the 500 ms quiet window elapses"
    );

    // Let the quiet window expire → exactly one coalesced emit.
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(600));
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "burst of 5 updates coalesces into a single emit"
    );

    // (b) Continuous dirtying still force-emits within max-stale.
    //
    // HARNESS LIMITATION: the max-stale guard compares wall-clock
    // `std::time::Instant::now()` against `first_dirty_at`, and the GPUI
    // test executor's `advance_clock` only moves the *dispatcher* timer
    // (which drives the 500 ms debounce), NOT `Instant::now()`. So a
    // microsecond-fast burst can never accumulate 2 s of real staleness
    // and the force-emit branch can't be exercised by clock advancement.
    //
    // To assert the invariant deterministically we seed a throttle slot
    // whose `first_dirty_at` is already 2 s+ in the past (as a long
    // continuous stream would have left it), then fire one more
    // EntryUpdated(0): the handler must take the max-stale branch and emit
    // SYNCHRONOUSLY (no debounce wait), and clear the slot.
    *appended.borrow_mut() = 0;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _cx| {
            store.entry_update_throttles.insert(
                (session_id, 0),
                EntryUpdateThrottle {
                    first_dirty_at: std::time::Instant::now()
                        - std::time::Duration::from_millis(2_500),
                    _task: gpui::Task::ready(()),
                },
            );
        });
    });
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "max-stale breach must force a synchronous emit"
    );
    let still_throttled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .entry_update_throttles
            .contains_key(&(session_id, 0))
    });
    assert!(
        !still_throttled,
        "force-emit must clear the throttle slot so the next window starts fresh"
    );
}

/// Reproduction for the "phone never shows the LAST message of a turn until the
/// next send" bug. Drives the real turn-end acp event sequence — a streamed
/// assistant reply whose trailing bytes are flushed at end-of-turn, then
/// `Stopped` — and asserts that, from a cursor captured BEFORE the turn ended, a
/// `get_session_changes` poll (what the mobile runs on `agent_session_dirty`)
/// returns the FINAL entry with its complete text and that the session's
/// `change_seq` advanced past it.
///
/// The wire path the mobile depends on at turn end is: the `Stopped` event flips
/// the session Idle, which advances `change_seq` and emits a `dirty` carrying
/// that fresh seq; the mobile converges on it. This test pins that the final
/// entry IS visible to that poll — without the fix it was not, because the only
/// `change_seq` bump after the final flush rode the debounced
/// `SessionMessageAppended` whose timer was cancelled by the session being torn
/// down / never drained, so the `Stopped` dirty reported a seq that did not yet
/// cover the flushed tail.
#[gpui::test]
async fn final_streamed_message_is_visible_to_delta_poll_after_stop(cx: &mut TestAppContext) {
    use agent_client_protocol::schema as acp;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // A user message (entry 0) then an assistant reply (entry 1). The assistant
    // entry starts with a first chunk so subsequent text streams into it.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                acp::ContentBlock::Text(acp::TextContent::new("question".to_string())),
                cx,
            );
            t.push_assistant_content_block(
                acp::ContentBlock::Text(acp::TextContent::new("Answer: ".to_string())),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Capture the cursor the mobile would hold mid-turn (before the final
    // streamed tail). This stands in for `openSeq` on the phone.
    let cursor_before_tail = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let s = store.read(cx).session(session_id).expect("session");
        s.read(cx).change_seq
    });

    // Force the session into Running so the upcoming Stopped flips the
    // discriminant (Running -> Idle) and therefore emits the state change /
    // dirty the mobile converges on — exactly what a real turn looks like.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(
                session_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    };
                },
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Stream a long trailing tail into the assistant entry. It lands in the
    // streaming buffer and is revealed gradually — the final bytes only reach
    // the markdown when the buffer is flushed at end-of-turn.
    const TAIL: &str =
        "this is the final sentence of the assistant's reply that must reach the phone.";
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                acp::ContentBlock::Text(acp::TextContent::new(TAIL.to_string())),
                false,
                cx,
            );
        });
    });
    // Deliberately do NOT drain the reveal task fully — the tail is still
    // buffered. The turn-end flush + Stopped is what must surface it.

    // Now emit the exact end-of-turn sequence the run-turn completion code
    // produces (acp_thread.rs Ok path): cancel the reveal task, flush the
    // buffered tail into the markdown, signal the final EntryUpdated, then
    // Stopped. `cancel` flushes the streaming buffer for us.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            // Flush the buffered tail (mirrors `flush_streaming_text` at turn end).
            let _ = t.cancel(cx);
            let last = t.entries().len().saturating_sub(1);
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(last));
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                acp::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    // CRITICAL: WITHOUT advancing any debounce/reveal timers, the `dirty`
    // signal the editor pushed for the Stopped transition must already carry a
    // `current_seq` that covers the final entry. The mobile receives this dirty
    // and converges to it; if it does NOT cover the final entry, the phone's
    // convergence poll either early-returns (cursor already >= target) or fetches
    // a transcript that still lacks the flushed tail — the "last message missing
    // until next send" symptom. Reading the dirty payload here mirrors the exact
    // value `event_sources::build_session_dirty_payload` would have emitted on
    // the Stopped store event.
    let (immediate_dirty_seq, final_mod_seq) = cx.update(|cx| {
        let dirty = crate::event_sources::build_session_dirty_payload(session_id, cx);
        let seq = dirty
            .get("current_seq")
            .and_then(|v| v.as_u64())
            .expect("dirty payload carries current_seq");
        let store = SolutionAgentStore::global(cx);
        let s = store.read(cx).session(session_id).expect("session");
        let final_mod = s.read(cx).entries.last().expect("final entry").mod_seq;
        (seq, final_mod)
    });
    assert!(
        immediate_dirty_seq >= final_mod_seq,
        "the dirty pushed on the Stopped transition (current_seq={immediate_dirty_seq}) must already \
         cover the final entry's mod_seq ({final_mod_seq}) — without advancing any debounce timer; \
         otherwise the phone never converges to the final message until the next send"
    );

    // Drain any debounce / reveal timers so an honest "after everything settles"
    // state is observed (the bug is that the mobile must NOT need this).
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(2_500));
    cx.executor().run_until_parked();

    // The session must be Idle and its change_seq must have advanced past the
    // cursor the mobile held mid-turn.
    let (state_idle, change_seq_now, final_text) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let s = store.read(cx).session(session_id).expect("session");
        let s = s.read(cx);
        let idle = matches!(s.state, SessionState::Idle);
        let text = match &s.entries.last().expect("final entry").kind {
            crate::session_entry::SessionEntryKind::AssistantMessage { chunks } => chunks
                .iter()
                .map(|c| match c {
                    crate::session_entry::AssistantChunk::Message(m) => m.to_string(),
                    crate::session_entry::AssistantChunk::Thought(t) => t.to_string(),
                })
                .collect::<String>(),
            other => panic!("expected assistant final entry, got {other:?}"),
        };
        (idle, s.change_seq, text)
    });
    assert!(state_idle, "session must be Idle after Stopped");
    assert!(
        final_text.contains("final sentence"),
        "the flushed tail must be stored in the final entry: {final_text:?}"
    );
    assert!(
        change_seq_now > cursor_before_tail,
        "change_seq must advance past the mid-turn cursor ({change_seq_now} > {cursor_before_tail})"
    );

    // The decisive assertion: a delta poll from the mid-turn cursor (what the
    // mobile runs when it receives the `Stopped` dirty) must return the final
    // entry carrying the full flushed tail.
    use context_server::listener::McpServerTool as _;
    let delta = crate::mcp::GetSessionChangesTool
        .run(
            crate::mcp::GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: cursor_before_tail,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            &mut cx.to_async(),
        )
        .await
        .expect("get_session_changes")
        .structured_content;
    assert!(!delta.reset, "no epoch rotation expected");
    let final_index = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let s = store.read(cx).session(session_id).expect("session");
        s.read(cx).entries.len() - 1
    });
    let returned_final = delta
        .changed_entries
        .iter()
        .find(|e| e.index == final_index)
        .unwrap_or_else(|| {
            panic!(
                "delta poll from mid-turn cursor must include the final entry (index {final_index}); \
                 got indices {:?}",
                delta.changed_entries.iter().map(|e| e.index).collect::<Vec<_>>()
            )
        });
    assert!(
        returned_final.preview.contains("final sentence"),
        "the delta-returned final entry must carry the flushed tail: {:?}",
        returned_final.preview
    );
}

/// Regression for the "final message strands until next send" bug, isolating the
/// fragile path: the final entry's append signal rides ONLY the `EntryUpdated`
/// debounce, and `Stopped` does NOT change the state discriminant (so the
/// `mark_state_changed` dirty does not bail us out). The turn-completion handler
/// must flush the pending debounce SYNCHRONOUSLY on `Stopped`, emitting the
/// final entry's `SessionMessageAppended` (→ `agent_session_dirty`) immediately —
/// WITHOUT waiting out the 500 ms / 2 s debounce window.
///
/// Without the fix the only append emit is the still-armed debounce task, so
/// this asserts zero appends right after `Stopped` and the test fails.
#[gpui::test]
async fn stopped_flushes_pending_entry_update_debounce_immediately(cx: &mut TestAppContext) {
    use agent_client_protocol::schema as acp;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // One assistant entry to stream into.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                acp::ContentBlock::Text(acp::TextContent::new("partial".to_string())),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Subscribe to SessionMessageAppended for the assistant entry (index 0).
    let appended = Rc::new(std::cell::RefCell::new(0usize));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let appended = appended.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionMessageAppended(id, idx) = event
                && *id == session_id
                && *idx == 0
            {
                *appended.borrow_mut() += 1;
            }
        })
    });

    // Fire an EntryUpdated (streaming chunk). This arms the 500 ms debounce —
    // no append emit yet. Session stays Idle (never set Running), so the
    // upcoming Stopped will NOT change the state discriminant: the debounce
    // flush is the ONLY thing that can surface the final entry promptly.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        0,
        "debounced EntryUpdated must not emit before the quiet window"
    );

    // Turn ends. The Stopped handler must flush the pending debounce slot
    // synchronously → exactly one append emit on this tick, with NO clock
    // advance.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                acp::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "Stopped must flush the pending entry-update debounce immediately so the \
         final entry's append (and its agent_session_dirty) reaches the client \
         without waiting out the debounce window"
    );

    // The flushed slot must be gone so the debounce timer can't double-fire.
    let still_throttled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .entry_update_throttles
            .contains_key(&(session_id, 0))
    });
    assert!(
        !still_throttled,
        "the flushed throttle slot must be cleared on Stopped"
    );

    // And the timer firing later must NOT produce a second append.
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(2_500));
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "the cleared debounce timer must not double-emit after Stopped flushed it"
    );
}

/// Symmetric with `stopped_flushes_pending_entry_update_debounce_immediately`:
/// a turn that ends via `Error` must ALSO flush the pending entry-update
/// debounce synchronously, so the final entry's append (and its
/// `agent_session_dirty`) reaches the client without waiting out the 500 ms
/// window. Without the flush, a turn that errored while already `Errored`
/// (state discriminant unchanged → no state dirty) would leave the last
/// assistant bytes reachable only after the debounce timer — the tail-loss
/// class this fix closes.
#[gpui::test]
async fn errored_flushes_pending_entry_update_debounce_immediately(cx: &mut TestAppContext) {
    use agent_client_protocol::schema as acp;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                acp::ContentBlock::Text(acp::TextContent::new("partial".to_string())),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let appended = Rc::new(std::cell::RefCell::new(0usize));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let appended = appended.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionMessageAppended(id, idx) = event
                && *id == session_id
                && *idx == 0
            {
                *appended.borrow_mut() += 1;
            }
        })
    });

    // Arm the 500 ms debounce with a streaming EntryUpdated — no append yet.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(*appended.borrow(), 0, "debounced update must not emit yet");

    // Turn errors. The Error handler must flush the pending slot synchronously.
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Error);
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "Error must flush the pending entry-update debounce immediately, like Stopped"
    );

    let still_throttled = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .entry_update_throttles
            .contains_key(&(session_id, 0))
    });
    assert!(!still_throttled, "the flushed throttle slot must be cleared on Error");

    cx.executor()
        .advance_clock(std::time::Duration::from_millis(2_500));
    cx.executor().run_until_parked();
    assert_eq!(
        *appended.borrow(),
        1,
        "the cleared debounce timer must not double-emit after Error flushed it"
    );
}

/// Path to the `claude_native` mock binary (a bash script) used by the
/// integration tests in `crates/claude_native/tests/`. Reuses the same fixture
/// here so a Phase-2 store-routing test can stand up a real
/// `ClaudeNativeConnection` without a system-installed `claude`.
fn native_mock_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("claude_native")
        .join("tests")
        .join("fixtures")
        .join("mock_claude.sh")
}

/// All mid-turn sends (including those backed by a native `ClaudeNativeConnection`)
/// now route through `pending_messages` — the inject branch has been removed.
/// In-turn delivery will be restored via a pull-closure in a later task.
#[gpui::test]
async fn send_during_running_on_native_connection_routes_to_queue(cx: &mut TestAppContext) {
    use acp_thread::AgentConnection;
    use agent_client_protocol::schema as acp;
    use agent_servers::{AgentServer, AgentServerDelegate};
    use claude_native::{ClaudeNativeAgentServer, ClaudeNativeConnection};
    use project::AgentId;

    let mock_binary = native_mock_binary();
    if !mock_binary.exists() {
        panic!(
            "mock claude binary missing at {} — tests/fixtures/mock_claude.sh not bundled?",
            mock_binary.display()
        );
    }
    cx.executor().allow_parking();

    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("claude-native");

    let server = Rc::new(ClaudeNativeAgentServer::with_binary(
        AgentId::new("claude-native"),
        mock_binary,
        Vec::new(),
    ));

    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(agent_id.clone(), server.clone());
        });
    });

    let connection: Rc<dyn acp_thread::AgentConnection> = cx
        .update(|cx| {
            let store = project.read(cx).agent_server_store().clone();
            let delegate = AgentServerDelegate::new(store, None, None);
            AgentServer::connect(server.as_ref(), delegate, project.clone(), cx)
        })
        .await
        .expect("native connect");
    let native = connection
        .clone()
        .downcast::<ClaudeNativeConnection>()
        .expect("downcast to ClaudeNativeConnection");

    let work_dirs = util::path_list::PathList::new(&[std::env::temp_dir().as_path()]);
    let acp_thread = cx
        .update(|cx| Rc::clone(&native).new_session(project.clone(), work_dirs, cx))
        .await
        .expect("new_session");

    let acp_session_id = acp_thread.read_with(cx, |t, _| t.session_id().clone());

    let session_id = SolutionSessionId::new();
    cx.update(|cx| {
        let session = cx.new(|_| {
            let mut s = crate::model::SolutionSession::new_idle(
                session_id,
                solution_id.clone(),
                agent_id.clone(),
                acp_session_id.clone(),
            );
            s.title = SharedString::from("native-test");
            s.project = Some(project.clone());
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s
        });
        session.update(cx, |session, cx| {
            session.set_acp_thread(Some(acp_thread.clone()), cx);
        });
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _store_cx| {
            store.sessions.insert(session_id, session);
            store
                .by_solution
                .entry(solution_id.clone())
                .or_default()
                .push(session_id);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store
                .send_message_blocks(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "PURPLE_PINEAPPLE".to_string(),
                    ))],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            let s = session.read(cx);
            // All running sends go through pending_messages now — native is no exception.
            assert_eq!(
                s.pending_messages.len(),
                1,
                "native-backed Running send must queue into pending_messages"
            );
            let payload: String = s.pending_messages[0]
                .blocks
                .iter()
                .filter_map(|b| match b {
                    acp::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(
                payload.contains("PURPLE_PINEAPPLE"),
                "queued bundle must carry the sent text, got {payload:?}"
            );
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .update(cx, |store, cx| store.close_session(session_id, cx))
            .ok();
    });
    drop(acp_thread);
    drop(native);
    drop(server);
}

/// End-to-end of the pull side: after a native-backed session is wired through
/// `subscribe_to_session`, the connection holds a store pull-closure. Invoking
/// that closure (as the live pump does at each hook) must map the ACP session
/// id back to the `SolutionSessionId`, drain `pending_messages`, push a single
/// user entry onto the thread, and return the agent-facing text (no hint for a
/// mid-turn hook, hint prepended at end-of-turn).
#[gpui::test]
async fn registered_store_pull_drains_queue_and_returns_followup_text(cx: &mut TestAppContext) {
    use acp_thread::AgentConnection;
    use agent_client_protocol::schema as acp;
    use agent_servers::{AgentServer, AgentServerDelegate};
    use claude_native::ClaudeNativeConnection;
    use project::AgentId;

    let mock_binary = native_mock_binary();
    if !mock_binary.exists() {
        panic!(
            "mock claude binary missing at {} — tests/fixtures/mock_claude.sh not bundled?",
            mock_binary.display()
        );
    }
    cx.executor().allow_parking();

    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("claude-native");

    let server = Rc::new(claude_native::ClaudeNativeAgentServer::with_binary(
        AgentId::new("claude-native"),
        mock_binary,
        Vec::new(),
    ));

    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(agent_id.clone(), server.clone());
        });
    });

    let connection: Rc<dyn acp_thread::AgentConnection> = cx
        .update(|cx| {
            let store = project.read(cx).agent_server_store().clone();
            let delegate = AgentServerDelegate::new(store, None, None);
            AgentServer::connect(server.as_ref(), delegate, project.clone(), cx)
        })
        .await
        .expect("native connect");
    let native = connection
        .clone()
        .downcast::<ClaudeNativeConnection>()
        .expect("downcast to ClaudeNativeConnection");

    let work_dirs = util::path_list::PathList::new(&[std::env::temp_dir().as_path()]);
    let acp_thread = cx
        .update(|cx| Rc::clone(&native).new_session(project.clone(), work_dirs, cx))
        .await
        .expect("new_session");

    let acp_session_id = acp_thread.read_with(cx, |t, _| t.session_id().clone());

    // Insert the session AND wire it through `subscribe_to_session` (the single
    // attach choke point) so Part A's pull-registration runs.
    let session_id = SolutionSessionId::new();
    cx.update(|cx| {
        let session = cx.new(|_| {
            let mut s = crate::model::SolutionSession::new_idle(
                session_id,
                solution_id.clone(),
                agent_id.clone(),
                acp_session_id.clone(),
            );
            s.title = SharedString::from("native-pull-test");
            s.project = Some(project.clone());
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s
        });
        session.update(cx, |session, cx| {
            session.set_acp_thread(Some(acp_thread.clone()), cx);
        });
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.sessions.insert(session_id, session.clone());
            store
                .by_solution
                .entry(solution_id.clone())
                .or_default()
                .push(session_id);
            let sub = store.subscribe_to_session(session_id, acp_thread.clone(), cx);
            session.update(cx, |s, _| s._acp_subscription = Some(sub));
        });
    });

    // (1) Pull registered via `subscribe_to_session` ⇒ Part A ran.
    assert!(
        native.store_pull_registered_for_test(),
        "subscribe_to_session must register the native store pull"
    );

    // (2) Mid-turn send enqueues into pending_messages.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store
                .send_message_blocks(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "FOLLOWUP_XYZ".to_string(),
                    ))],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            assert_eq!(
                session.read(cx).pending_messages.len(),
                1,
                "running send must queue"
            );
        });
    });

    let entries_before = acp_thread.read_with(cx, |t, _| t.entries().len());

    // (3) Invoke the registered pull exactly as the pump would (mid-turn ⇒
    // is_end_of_turn = false). The closure runs `weak.update` itself, so it must
    // be called OUTSIDE any open store update.
    let mut async_cx = cx.to_async();
    // A subagent's hook (agent_id present) must NOT drain the main queue —
    // otherwise a running Agent Teams teammate swallows the follow-up. It
    // stays queued for the main agent's next hook.
    let sub_pull = native.invoke_store_pull_for_test(
        &acp_session_id,
        Some("sub-agent-1"),
        false,
        &mut async_cx,
    );
    assert!(
        sub_pull.is_none(),
        "a subagent hook must not drain the main agent's queue, got {sub_pull:?}"
    );
    // The main agent's hook (no agent_id) drains it.
    let pulled = native.invoke_store_pull_for_test(&acp_session_id, None, false, &mut async_cx);
    let pulled = pulled.expect("pull must return the queued follow-up text");
    assert!(
        pulled.contains("FOLLOWUP_XYZ"),
        "pulled text must carry the queued message, got {pulled:?}"
    );
    assert!(
        !pulled.contains(crate::store::queue::QUEUE_HINT_LINE),
        "mid-turn pull must NOT prepend the queue hint, got {pulled:?}"
    );

    // (4) Queue drained + the thread gained exactly one user entry ⇒ the
    // acp→solution id mapping ran and `take_pending_for_delivery` executed.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            assert_eq!(
                session.read(cx).pending_messages.len(),
                0,
                "pull must drain pending_messages"
            );
        });
    });
    let entries_after = acp_thread.read_with(cx, |t, _| t.entries().len());
    assert_eq!(
        entries_after,
        entries_before + 1,
        "delivery must push exactly one user entry onto the thread"
    );
    let last_is_user = acp_thread.read_with(cx, |t, _| {
        matches!(
            t.entries().last(),
            Some(acp_thread::AgentThreadEntry::UserMessage(_))
        )
    });
    assert!(last_is_user, "the appended entry must be a user message");

    // (5) End-of-turn pull prepends the hint.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store
                .send_message_blocks(
                    session_id,
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "FOLLOWUP_EOT".to_string(),
                    ))],
                    cx,
                )
                .detach_and_log_err(cx);
        });
    });
    let mut async_cx = cx.to_async();
    let pulled_eot = native
        .invoke_store_pull_for_test(&acp_session_id, None, true, &mut async_cx)
        .expect("end-of-turn pull must return text");
    assert!(
        pulled_eot.contains("FOLLOWUP_EOT"),
        "end-of-turn pull must carry the queued message, got {pulled_eot:?}"
    );
    assert!(
        pulled_eot.contains(crate::store::queue::QUEUE_HINT_LINE),
        "end-of-turn pull must prepend the queue hint, got {pulled_eot:?}"
    );

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .update(cx, |store, cx| store.close_session(session_id, cx))
            .ok();
    });
    drop(acp_thread);
    drop(native);
    drop(server);
}

// ---------------------------------------------------------------------------
// Etap 3: Subagent-tab lifecycle (`teammate_labels`). Since wire v5 the durable
// friendly label rides `teammate_labels`; a teammate's rendered pill + wire label
// come from `Stream.label` (enriched from that map at `rebuild_streams`).
// These exercise `SolutionAgentStore::apply_subagent_lifecycle` through the
// real `AcpThreadEvent::NewEntry` / `EntryUpdated` plumbing — by upserting
// `acp::ToolCall` shapes directly on a live `AcpThread` and asserting how
// the per-session map and the `SessionSubagentsChanged` event stream react.
// ---------------------------------------------------------------------------

/// Build an `acp::ToolCall` for a Task/Agent subagent dispatch with the
/// programmatic name carried in `_meta.tool_name` (the convention shared by
/// `claude_native::translate_assistant` and consumed by
/// `apply_subagent_lifecycle`). Optional `description` populates
/// `raw_input["description"]` so the label-fallback chain can be exercised.
fn make_task_tool_call(
    id: &str,
    tool_name: &str,
    status: agent_client_protocol::schema::ToolCallStatus,
    description: Option<&str>,
    subagent_type: Option<&str>,
) -> agent_client_protocol::schema::ToolCall {
    use agent_client_protocol::schema as acp;
    let mut raw_input = serde_json::Map::new();
    if let Some(d) = description {
        raw_input.insert("description".into(), serde_json::Value::String(d.into()));
    }
    if let Some(s) = subagent_type {
        raw_input.insert("subagent_type".into(), serde_json::Value::String(s.into()));
    }
    let mut call = acp::ToolCall::new(acp::ToolCallId::new(id.to_string()), tool_name.to_string())
        .kind(acp::ToolKind::Think)
        .status(status)
        .meta(Some(acp_thread::meta_with_tool_name(tool_name)));
    if !raw_input.is_empty() {
        call = call.raw_input(serde_json::Value::Object(raw_input));
    }
    call
}

/// Helper to count `SessionSubagentsChanged` events for a given session.
/// Returns the counter handle plus the subscription (which must be held in
/// scope for the lifetime of the test).
fn subscribe_subagents_changed(
    session_id: SolutionSessionId,
    cx: &mut TestAppContext,
) -> (Rc<std::cell::RefCell<usize>>, gpui::Subscription) {
    let counter = Rc::new(std::cell::RefCell::new(0usize));
    let subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionSubagentsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });
    (counter, subscription)
}

#[gpui::test]
async fn subagent_inprogress_task_registers_tab(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_task_1",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Loop agent 1"),
                    Some("general-purpose"),
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.teammate_labels.len(), 1, "one teammate label captured");
        let key = SharedString::from("toolu_task_1");
        let label = s.teammate_labels.get(&key).expect("label present");
        assert_eq!(label.as_ref(), "Loop agent 1");
    });
    assert_eq!(
        *changed_count.borrow(),
        1,
        "SessionSubagentsChanged emitted exactly once on first registration"
    );
}

#[gpui::test]
async fn subagent_terminal_status_removes_tab(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_task_2",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker A"),
                    None,
                ),
                cx,
            )
            .expect("upsert in progress");
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(*changed_count.borrow(), 1, "one add emit");

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_task_2",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::Completed,
                    Some("Worker A"),
                    None,
                ),
                cx,
            )
            .expect("upsert completed");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert!(
            s.teammate_labels.is_empty(),
            "label reclaimed on Completed (via close_stream)"
        );
    });
    assert_eq!(
        *changed_count.borrow(),
        2,
        "exactly two emits: add + remove"
    );
}

#[gpui::test]
async fn tool_completion_entry_updated_refreshes_silence_clock(cx: &mut TestAppContext) {
    // A long silent FOREGROUND command bumps `last_activity_at` once (the
    // tool-call `NewEntry` at start), then blocks claude for minutes while
    // streaming nothing — during which the stuck-session watchdog is held off
    // only by the in-progress-tool shield (TOOL_STUCK_SECS). When the command
    // finishes, its terminal-status transition arrives as an `EntryUpdated`.
    // That event MUST refresh `last_activity_at`; otherwise the instant the tool
    // leaves `InProgress` the watchdog sees `silent_secs >= STUCK_TURN_SECS` with
    // no live tool and falsely reconnects a healthy agent (observed 2026-07-01:
    // an `until grep …` poll loop hit claude's own 5-min Bash timeout and the
    // editor immediately "переподключил сессию").
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Tool starts (NewEntry) — bumps last_activity_at to "now".
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_slow_cmd",
                    "Bash",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("until grep -qE '^(ok|FAIL)' out"),
                    None,
                ),
                cx,
            )
            .expect("upsert in progress");
        });
    });
    cx.executor().run_until_parked();

    // Simulate the long silent run: freeze the clock 6 minutes in the past,
    // past STUCK_TURN_SECS (5 min).
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        session.update(cx, |s, _| {
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(360);
        });
    });

    // Command finishes → terminal-status EntryUpdated on the same tool id.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_slow_cmd",
                    "Bash",
                    agent_client_protocol::schema::ToolCallStatus::Completed,
                    Some("until grep -qE '^(ok|FAIL)' out"),
                    None,
                ),
                cx,
            )
            .expect("upsert completed");
        });
    });
    cx.executor().run_until_parked();

    // The completion must have reset the silence clock, so a subsequent watchdog
    // tick sees a live agent, not a wedged one.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let silent_secs = chrono::Utc::now()
            .signed_duration_since(session.read(cx).last_activity_at)
            .num_seconds();
        assert!(
            silent_secs < 60,
            "a tool-completion EntryUpdated must refresh last_activity_at \
             (was {silent_secs}s stale — watchdog would falsely reconnect)"
        );
    });
}

#[gpui::test]
async fn subagent_label_falls_back_to_subagent_type(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_long_abcd",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    None,
                    Some("general-purpose"),
                ),
                cx,
            )
            .expect("upsert");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let key = SharedString::from("toolu_long_abcd");
        let label = s.teammate_labels.get(&key).expect("label present");
        assert_eq!(
            label.as_ref(),
            "general-purpose#abcd",
            "fallback label is `subagent_type#<short-id>`"
        );
    });
}

#[gpui::test]
async fn subagent_label_defaults_to_agent_short_id(cx: &mut TestAppContext) {
    // No description, no subagent_type → final fallback: `Agent <short-id>`.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_xy12",
                    "Agent",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    None,
                    None,
                ),
                cx,
            )
            .expect("upsert");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let key = SharedString::from("toolu_xy12");
        let label = s.teammate_labels.get(&key).expect("label present");
        assert_eq!(label.as_ref(), "Agent xy12");
    });
}

#[gpui::test]
async fn non_task_tool_call_does_not_register_tab(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_bash_1",
                    "Bash",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("ignored"),
                    None,
                ),
                cx,
            )
            .expect("upsert bash");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert!(
            s.teammate_labels.is_empty(),
            "Bash is not a subagent — no label captured"
        );
    });
    assert_eq!(
        *changed_count.borrow(),
        0,
        "no SessionSubagentsChanged emission for non-subagent tools"
    );
}

#[gpui::test]
async fn subagent_registration_captures_all_labels(cx: &mut TestAppContext) {
    // Since wire v5 the pill ORDER lives in `streams` (an IndexMap keyed by
    // teammate first-appearance), not on `teammate_labels` (a plain map). This
    // test now just proves every teammate's label is captured with its right
    // value; stream order is covered by the demux / session_view tests.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    for (id, label) in [
        ("toolu_a", "First"),
        ("toolu_b", "Second"),
        ("toolu_c", "Third"),
    ] {
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.upsert_tool_call(
                    make_task_tool_call(
                        id,
                        "Task",
                        agent_client_protocol::schema::ToolCallStatus::InProgress,
                        Some(label),
                        None,
                    ),
                    cx,
                )
                .expect("upsert");
            });
        });
        cx.executor().run_until_parked();
    }

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.teammate_labels.len(), 3);
        for (id, label) in [
            ("toolu_a", "First"),
            ("toolu_b", "Second"),
            ("toolu_c", "Third"),
        ] {
            assert_eq!(
                s.teammate_labels
                    .get(&SharedString::from(id))
                    .map(|l| l.as_ref()),
                Some(label),
                "label for {id} captured"
            );
        }
    });
}

#[gpui::test]
async fn duplicate_inprogress_does_not_re_register(cx: &mut TestAppContext) {
    // A streaming Task's raw_input arrives over several EntryUpdated events
    // before the status flips off InProgress; each one re-enters
    // `apply_subagent_lifecycle`. The first must register the tab, every
    // subsequent observation must be a no-op (no double-insert, no
    // SessionSubagentsChanged spam).
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_dup",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Original"),
                    None,
                ),
                cx,
            )
            .expect("upsert initial");
        });
    });
    cx.executor().run_until_parked();

    // Simulate a second EntryUpdated for the same id, still InProgress
    // (e.g. raw_input streamed in more keys). Even if the label would now
    // be different, the tab is already there.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_dup",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Renamed"),
                    None,
                ),
                cx,
            )
            .expect("upsert again");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.teammate_labels.len(), 1, "single label, not doubled");
        let label = s
            .teammate_labels
            .get(&SharedString::from("toolu_dup"))
            .expect("label");
        // Label is locked-in at first observation — the "Renamed" update is
        // ignored to preserve a stable user-facing pill across the streaming
        // raw_input chunks.
        assert_eq!(label.as_ref(), "Original");
    });
    assert_eq!(
        *changed_count.borrow(),
        1,
        "duplicate InProgress must not re-emit SessionSubagentsChanged"
    );
}

#[gpui::test]
async fn subagent_failed_status_also_removes_tab(cx: &mut TestAppContext) {
    // Terminal-status coverage: Failed is just as final as Completed/Canceled.
    // A Task subagent that crashed mid-run still releases its tab.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_fail",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Doomed"),
                    None,
                ),
                cx,
            )
            .expect("upsert");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_fail",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::Failed,
                    Some("Doomed"),
                    None,
                ),
                cx,
            )
            .expect("upsert failed");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        assert!(session.read(cx).teammate_labels.is_empty());
    });
    assert_eq!(*changed_count.borrow(), 2, "add + remove on Failed");
}

/// Background-Agents-Strip Task 8: when an `Agent`-named ToolCall enters a
/// terminal status with a parseable `raw_output`, `apply_subagent_lifecycle`
/// registers a `BackgroundAgent` and emits `SessionBackgroundAgentsChanged`.
#[gpui::test]
async fn agent_terminal_with_parseable_raw_output_registers_background_agent(
    cx: &mut TestAppContext,
) {
    use agent_client_protocol::schema as acp;
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let bg_counter = Rc::new(std::cell::RefCell::new(0usize));
    let _bg_sub = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = bg_counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });

    // InProgress → Completed with raw_output carrying the managed-agent
    // announcement. Mirrors how claude_code emits the Agent tool call.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_agent_1".to_string()),
                "Agent".to_string(),
            )
            .kind(acp::ToolKind::Think)
            .status(acp::ToolCallStatus::InProgress)
            .meta(Some(acp_thread::meta_with_tool_name("Agent")));
            t.upsert_tool_call(call, cx).expect("upsert in-progress");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            let raw = serde_json::Value::String(
                "agentId: a30f92a688e431edc\noutput_file: /tmp/agent-a30f92a688e431edc.output"
                    .to_string(),
            );
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_agent_1".to_string()),
                "Agent".to_string(),
            )
            .kind(acp::ToolKind::Think)
            .status(acp::ToolCallStatus::Completed)
            .raw_output(raw)
            .meta(Some(acp_thread::meta_with_tool_name("Agent")));
            t.upsert_tool_call(call, cx).expect("upsert completed");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.background_agents.len(),
            1,
            "one background-agent registered on Agent-tool terminal"
        );
        assert_eq!(s.background_agent_order.len(), 1, "order vec parallel");
        let id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
        assert!(
            s.background_agents.contains_key(&id),
            "registration keyed on parsed agentId"
        );
        assert_eq!(s.background_agent_order[0], id);
    });
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "SessionBackgroundAgentsChanged emitted exactly once on registration"
    );

    // Idempotency: a duplicate terminal-status update with the same
    // raw_output must NOT re-register or re-emit. Drives the
    // `contains_key` guard in apply_subagent_lifecycle.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            let raw = serde_json::Value::String(
                "agentId: a30f92a688e431edc\noutput_file: /tmp/agent-a30f92a688e431edc.output"
                    .to_string(),
            );
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_agent_1".to_string()),
                "Agent".to_string(),
            )
            .kind(acp::ToolKind::Think)
            .status(acp::ToolCallStatus::Completed)
            .raw_output(raw)
            .meta(Some(acp_thread::meta_with_tool_name("Agent")));
            t.upsert_tool_call(call, cx).expect("upsert duplicate");
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "duplicate Agent-terminal update is a no-op (idempotent registration)"
    );
}

/// Build a `Bash` ToolCall carrying a `run_in_background` flag in
/// `raw_input` and the launch announcement in `raw_output`. Mirrors how
/// claude_code shapes a backgrounded `Bash` call.
fn make_bash_bg_tool_call(
    id: &str,
    command: &str,
    run_in_background: bool,
    raw_output: Option<&str>,
) -> agent_client_protocol::schema::ToolCall {
    use agent_client_protocol::schema as acp;
    let mut raw_input = serde_json::Map::new();
    raw_input.insert("command".into(), serde_json::Value::String(command.into()));
    raw_input.insert(
        "run_in_background".into(),
        serde_json::Value::Bool(run_in_background),
    );
    let mut call = acp::ToolCall::new(acp::ToolCallId::new(id.to_string()), "Bash".to_string())
        .kind(acp::ToolKind::Execute)
        .status(acp::ToolCallStatus::Completed)
        .meta(Some(acp_thread::meta_with_tool_name("Bash")))
        .raw_input(serde_json::Value::Object(raw_input));
    if let Some(out) = raw_output {
        call = call.raw_output(serde_json::Value::String(out.to_string()));
    }
    call
}

/// Background-Shells-Strip Tasks 7 + 9: a terminal `Bash` tool call with
/// `run_in_background=true` and a parseable launch announcement registers a
/// `BackgroundShell` (before the `is_task_like` gate, since `Bash` is not a
/// task-like tool) and emits `SessionBackgroundShellsChanged`.
#[gpui::test]
async fn bash_run_in_background_terminal_registers_background_shell(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let bg_counter = Rc::new(std::cell::RefCell::new(0usize));
    let _bg_sub = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = bg_counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionBackgroundShellsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });

    const ANNOUNCEMENT: &str = "Command running in background with ID: bvb4ful1z. Output is being written to: /tmp/claude-1000/-home-spk-proj/ses-x/tasks/bvb4ful1z.output. You will be notified when it completes.";

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_bash_bg_tool_call("toolu_bash_1", "sleep 60", true, Some(ANNOUNCEMENT)),
                cx,
            )
            .expect("upsert bash bg");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.background_shells.len(),
            1,
            "one background-shell registered on Bash(bg) terminal"
        );
        assert_eq!(s.background_shell_order.len(), 1, "order vec parallel");
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        assert!(
            s.background_shells.contains_key(&id),
            "registration keyed on parsed shell id"
        );
        assert_eq!(s.background_shell_order[0], id);
        let shell = s.background_shells.get(&id).expect("shell present");
        assert_eq!(
            shell.output_path,
            PathBuf::from("/tmp/claude-1000/-home-spk-proj/ses-x/tasks/bvb4ful1z.output"),
            "output_path parsed from announcement"
        );
        assert_eq!(shell.command.as_ref(), "sleep 60", "command label captured");
        assert_eq!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Running,
            "fresh shell is Running"
        );
    });
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "SessionBackgroundShellsChanged emitted exactly once on registration"
    );
}

/// Tasks 7 + 9: a `Bash` tool call WITHOUT `run_in_background` (false) must
/// NOT register a background shell — the `run_in_background == Some(true)`
/// guard rejects it.
#[gpui::test]
async fn bash_without_run_in_background_registers_no_shell(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    const ANNOUNCEMENT: &str = "Command running in background with ID: bvb4ful1z. Output is being written to: /tmp/claude-1000/proj/tasks/bvb4ful1z.output. You will be notified.";

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                // run_in_background = false, but with an announcement-shaped
                // raw_output — the guard must still reject it.
                make_bash_bg_tool_call("toolu_bash_2", "echo hi", false, Some(ANNOUNCEMENT)),
                cx,
            )
            .expect("upsert non-bg bash");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert!(
            s.background_shells.is_empty(),
            "no shell registered for a non-background Bash call"
        );
        assert!(s.background_shell_order.is_empty());
    });
}

/// Tasks 7 + 9: after registration, `refresh_background_shell_snapshot`
/// live-tails the on-disk `.output` file into `BackgroundShell::latest`.
/// Drives the full launch→register→tail pipeline against a real temp file.
#[gpui::test]
async fn refresh_background_shell_snapshot_tails_output_file(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let output_path = dir.path().join("tasks").join("bvb4ful1z.output");
    std::fs::create_dir_all(output_path.parent().expect("parent")).expect("mkdir tasks");
    std::fs::write(&output_path, b"line one\nline two\n").expect("write output");

    let announcement = format!(
        "Command running in background with ID: bvb4ful1z. Output is being written to: {}. You will be notified.",
        output_path.display()
    );

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_bash_bg_tool_call("toolu_bash_3", "tail -f log", true, Some(&announcement)),
                cx,
            )
            .expect("upsert bash bg");
        });
    });
    cx.executor().run_until_parked();

    // The inline refresh in the registration branch already tailed the file
    // (the announcement path is a real temp file). Assert the snapshot.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        let shell = s.background_shells.get(&id).expect("shell present");
        let latest = shell
            .latest
            .as_ref()
            .expect("inline refresh seeded a snapshot from the real file");
        assert!(
            latest.output_tail.contains("line one") && latest.output_tail.contains("line two"),
            "tail captured the file bytes, got: {:?}",
            latest.output_tail
        );
        assert_eq!(
            shell.last_offset, 18,
            "offset advanced past the 18 written bytes"
        );
    });
}

/// Test helper: register a `Running` background shell directly on a session,
/// bypassing the `Bash(bg)` launch-announcement parse path. Used by the Task 8
/// terminal-signal tests so they can assert the state transition in isolation.
fn register_background_shell(
    cx: &mut TestAppContext,
    session_id: SolutionSessionId,
    shell_id: &str,
) {
    let id = crate::background_shell::BackgroundShellId::new(shell_id.to_string());
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.update(cx, |s, _| {
            s.background_shells.insert(
                id.clone(),
                crate::background_shell::BackgroundShell {
                    id: id.clone(),
                    command: SharedString::from("sleep 60"),
                    output_path: PathBuf::from("/tmp/claude-1000/tasks")
                        .join(format!("{shell_id}.output")),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    state: crate::background_shell::ShellRuntimeState::Running,
                },
            );
            s.background_shell_order.push(id);
        });
    });
}

/// A background command COMPLETING must reset the session's silence clock.
/// While it runs, `has_live_background_work` keeps the supervisor quiet but
/// `last_activity_at` stays frozen at launch — so on completion the accrued
/// silence is already past the threshold and the judge would fire instantly,
/// racing the agent's own resume-to-read-the-result. Bumping the clock gives
/// the agent a fresh idle window to self-resume first.
#[gpui::test]
async fn background_shell_completion_resets_silence_clock(cx: &mut TestAppContext) {
    let (session_id, _acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "bg-done");

    // Freeze the clock 10 min back, as a long silent command would leave it.
    let stale = chrono::Utc::now() - chrono::Duration::seconds(600);
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store
                .session(session_id)
                .unwrap()
                .update(cx, |s, _| s.last_activity_at = stale);
            store.mark_background_shell_state(
                session_id,
                crate::background_shell::BackgroundShellId::new("bg-done".to_string()),
                crate::background_shell::ShellRuntimeState::Exited(Some(0)),
                cx,
            );
        });
    });

    let after = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .unwrap()
            .read(cx)
            .last_activity_at
    });
    assert!(
        after > stale,
        "a completed background command must reset the silence clock"
    );
    assert!(
        chrono::Utc::now().signed_duration_since(after).num_seconds() < 60,
        "last_activity must be bumped to ~now on completion, not left stale"
    );
}

/// Task 8: a terminal `KillShell` tool_call whose `raw_input` carries the
/// `shell_id` of a tracked background shell flips that shell to
/// `ShellRuntimeState::Killed` and emits `SessionBackgroundShellsChanged`.
#[gpui::test]
async fn kill_shell_terminal_marks_shell_killed(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "bvb4ful1z");

    let bg_counter = Rc::new(std::cell::RefCell::new(0usize));
    let _bg_sub = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = bg_counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionBackgroundShellsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            use agent_client_protocol::schema as acp;
            let mut raw_input = serde_json::Map::new();
            raw_input.insert(
                "shell_id".into(),
                serde_json::Value::String("bvb4ful1z".into()),
            );
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_kill_1".to_string()),
                "KillShell".to_string(),
            )
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .meta(Some(acp_thread::meta_with_tool_name("KillShell")))
            .raw_input(serde_json::Value::Object(raw_input));
            t.upsert_tool_call(call, cx).expect("upsert KillShell");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        let shell = s.background_shells.get(&id).expect("shell still tracked");
        assert_eq!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Killed,
            "KillShell tool_call flips the shell to Killed"
        );
    });
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "SessionBackgroundShellsChanged emitted exactly once on the kill"
    );
}

/// Task 8: a `<task-notification>` user-role message whose `<task-id>` matches
/// a tracked background shell flips it to `Exited(Some(code))`. Drives the
/// real NewEntry wiring: `push_user_content_block` appends a `UserMessage`
/// entry → `AcpThreadEvent::NewEntry` → `observe_task_notification`.
#[gpui::test]
async fn task_notification_user_message_marks_shell_exited(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "bvb4ful1z");

    const NOTIFICATION: &str = r#"<task-notification>
<task-id>bvb4ful1z</task-id>
<status>completed</status>
<summary>Background command "sleep 60" completed (exit code 0)</summary>
</task-notification>"#;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(NOTIFICATION.to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        let shell = s.background_shells.get(&id).expect("shell still tracked");
        assert_eq!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Exited(Some(0)),
            "task-notification user message flips the shell to Exited(0)"
        );
    });
}

/// Task 8: a `<task-notification>` for an UNTRACKED shell id is a no-op — no
/// stray shell is registered and no tracked shell's state changes.
#[gpui::test]
async fn task_notification_unknown_shell_is_noop(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "tracked123");

    const NOTIFICATION: &str = r#"<task-notification>
<task-id>unknown999</task-id>
<status>completed</status>
<summary>Background command "x" completed (exit code 0)</summary>
</task-notification>"#;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(NOTIFICATION.to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.background_shells.len(), 1, "no stray shell registered");
        let tracked = crate::background_shell::BackgroundShellId::new("tracked123");
        assert_eq!(
            s.background_shells
                .get(&tracked)
                .expect("tracked present")
                .state,
            crate::background_shell::ShellRuntimeState::Running,
            "an unrelated notification leaves the tracked shell Running"
        );
    });
}

/// Task 9: a Managed Agent whose `latest.stop_reason` is `Some(...)` is
/// removed from the session on the next `tick_background_agents` pass,
/// and a `SessionBackgroundAgentsChanged` event is emitted.
#[gpui::test]
async fn done_agent_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Done."),
                        stop_reason: Some(SharedString::from("end_turn")),
                    }),
                    last_offset: 0,
                    parent_tool_use_id: None,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_agents(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "done agent must be removed on tick"
        );
        assert!(
            session.read(cx).background_agent_order.is_empty(),
            "order vec must be pruned in lockstep"
        );
    });
}

/// Analogous to `background_shell_completion_resets_silence_clock`, for the
/// MANAGED-agent path: a background agent reaching a terminal `stop_reason`
/// must reset the session's silence clock, so the supervisor gives the parent a
/// full idle window to resume on its own before judging (rather than firing the
/// instant `has_live_background_work` flips false with silence already accrued).
#[gpui::test]
async fn background_agent_terminal_transition_resets_silence_clock(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");

    // A JSONL whose latest line is a terminal assistant stop.
    let dir = tempfile::tempdir().unwrap();
    let jsonl = dir.path().join("agent.jsonl");
    std::fs::write(
        &jsonl,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"bye"}],"stop_reason":"end_turn"}}"#,
    )
    .unwrap();

    let stale = chrono::Utc::now() - chrono::Duration::seconds(600);
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.last_activity_at = stale;
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: jsonl.clone().into(),
                    registered_at: chrono::Utc::now(),
                    // Currently NON-terminal — the refresh below observes the
                    // transition INTO terminal.
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Generating…"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                    parent_tool_use_id: None,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.refresh_background_agent_snapshot(session_id, bg_id.clone(), cx)
        });
    });

    let after = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .unwrap()
            .read(cx)
            .last_activity_at
    });
    assert!(after > stale, "terminal transition must reset the silence clock");
    assert!(
        chrono::Utc::now().signed_duration_since(after).num_seconds() < 60,
        "last_activity must be bumped to ~now, not left stale"
    );
}

/// Sub-task C (deferred #1): an async `Agent` teammate's demux `Teammate`
/// stream — which phase 3 deliberately kept live past the spawn tool-call's
/// terminal (that's only spawn-ack) — is auto-closed when the managed
/// background agent reaches its REAL terminal `stop_reason`, via the
/// `BackgroundAgent.parent_tool_use_id` → `StreamId::Teammate` mapping.
#[gpui::test]
async fn background_agent_terminal_closes_teammate_stream(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    let parent_toolu = SharedString::from("toolu_X");
    let teammate = crate::stream::StreamId::Teammate(parent_toolu.clone());

    let dir = tempfile::tempdir().unwrap();
    let jsonl = dir.path().join("agent.jsonl");
    std::fs::write(
        &jsonl,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"bye"}],"stop_reason":"end_turn"}}"#,
    )
    .unwrap();

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            // A parent-thread entry tagged with the teammate's parent tool_use
            // id → the demux produces a live `Teammate` stream.
            s.set_entries(
                vec![crate::session_entry::SessionEntry {
                    created_ms: 0,
                    mod_seq: 0,
                    subagent_id: Some(parent_toolu.clone()),
                    kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                        chunks: vec![crate::session_entry::AssistantChunk::Message(
                            "streaming".to_string(),
                        )],
                    },
                }],
                cx,
            );
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: jsonl.clone().into(),
                    registered_at: chrono::Utc::now(),
                    // Currently NON-terminal so the refresh observes the edge.
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Generating…"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                    parent_tool_use_id: Some(parent_toolu.clone()),
                },
            );
            s.background_agent_order.push(bg_id.clone());
            assert!(
                s.streams.contains_key(&teammate),
                "teammate stream is live before the agent finishes"
            );
        });
    });

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.refresh_background_agent_snapshot(session_id, bg_id.clone(), cx)
        });
    });

    cx.update(|cx| {
        let session = SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                !s.streams.contains_key(&teammate),
                "teammate stream must be closed on the agent's real terminal"
            );
            assert!(
                s.closed_streams.contains_key(&teammate),
                "close reason recorded in the overlay"
            );
        });
    });
}

/// Sub-task C negative case: a NON-terminal snapshot refresh must leave the
/// teammate's demux stream live (only the terminal `stop_reason` closes it).
#[gpui::test]
async fn background_agent_non_terminal_leaves_teammate_stream(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("b41a03b799f542fed");
    let parent_toolu = SharedString::from("toolu_Y");
    let teammate = crate::stream::StreamId::Teammate(parent_toolu.clone());

    let dir = tempfile::tempdir().unwrap();
    let jsonl = dir.path().join("agent.jsonl");
    // Latest line is a still-running assistant turn (no stop_reason).
    std::fs::write(
        &jsonl,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}"#,
    )
    .unwrap();

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![crate::session_entry::SessionEntry {
                    created_ms: 0,
                    mod_seq: 0,
                    subagent_id: Some(parent_toolu.clone()),
                    kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                        chunks: vec![crate::session_entry::AssistantChunk::Message(
                            "streaming".to_string(),
                        )],
                    },
                }],
                cx,
            );
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: jsonl.clone().into(),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    parent_tool_use_id: Some(parent_toolu.clone()),
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.refresh_background_agent_snapshot(session_id, bg_id.clone(), cx)
        });
    });

    cx.update(|cx| {
        let session = SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                s.streams.contains_key(&teammate),
                "a non-terminal refresh must keep the teammate stream live"
            );
        });
    });
}

// --- Mid-session SELECTIVE teammate-pill reconcile
// (`reconcile_finished_teammate_streams`) ---------------------------------
//
// These verify the busy-session leak fix: a finished-teammate pill must close
// the moment its completion is provable, WITHOUT any →Idle transition (a long
// busy session never fires the →Idle GC).

/// Build a parent spawn tool-call entry (on the Main stream — `subagent_id`
/// None) with the given owned-model `ToolStatus`.
fn reconcile_toolcall_entry(
    id: &str,
    tool_name: &str,
    status: crate::session_entry::ToolStatus,
) -> crate::session_entry::SessionEntry {
    use agent_client_protocol::schema as acp;
    crate::session_entry::SessionEntry {
        created_ms: 1_700_000_000_000,
        mod_seq: 1,
        subagent_id: None,
        kind: crate::session_entry::SessionEntryKind::ToolCall {
            id: id.to_string(),
            label_md: "spawn".into(),
            kind: acp::ToolKind::Think,
            status,
            content_md: Vec::new(),
            raw_input: None,
            raw_output: None,
            tool_name: Some(tool_name.to_string()),
            locations: Vec::new(),
            status_started_at: None,
        },
    }
}

/// Build a teammate body entry tagged with `toolu` — this is what the demux
/// turns into a `Teammate(toolu)` stream.
fn reconcile_tagged_body(toolu: &str) -> crate::session_entry::SessionEntry {
    crate::session_entry::SessionEntry {
        created_ms: 1_700_000_000_001,
        mod_seq: 2,
        subagent_id: Some(SharedString::from(toolu.to_string())),
        kind: crate::session_entry::SessionEntryKind::AssistantMessage {
            chunks: vec![crate::session_entry::AssistantChunk::Message(
                "teammate work".into(),
            )],
        },
    }
}

/// Rule 2: a NON-Idle (Running) session with an inline `Task` teammate whose
/// spawn tool-call is `Completed` has its `Teammate` stream closed by the
/// mid-session reconcile — no →Idle transition involved.
#[gpui::test]
async fn reconcile_closes_finished_inline_task_teammate_mid_session(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let toolu = "toolu_task_done";
    let teammate = crate::stream::StreamId::Teammate(SharedString::from(toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s.teammate_labels
                .insert(SharedString::from(toolu), SharedString::from("Worker"));
            s.set_entries(
                vec![
                    reconcile_toolcall_entry(
                        toolu,
                        "Task",
                        crate::session_entry::ToolStatus::Completed,
                    ),
                    reconcile_tagged_body(toolu),
                ],
                cx,
            );
            assert!(
                s.streams.contains_key(&teammate),
                "teammate stream is live before the reconcile"
            );
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                !s.streams.contains_key(&teammate),
                "a terminal inline Task teammate must be reconciled closed mid-session"
            );
            assert!(
                !matches!(s.state, SessionState::Idle),
                "guard: the session never went Idle"
            );
        });
    });
}

/// Do-NOT-close: an inline `Task` whose spawn tool-call is still `InProgress`
/// is genuinely live mid-session — the reconcile must leave it.
#[gpui::test]
async fn reconcile_keeps_live_inline_task_teammate(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let toolu = "toolu_task_live";
    let teammate = crate::stream::StreamId::Teammate(SharedString::from(toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s.set_entries(
                vec![
                    reconcile_toolcall_entry(
                        toolu,
                        "Task",
                        crate::session_entry::ToolStatus::InProgress,
                    ),
                    reconcile_tagged_body(toolu),
                ],
                cx,
            );
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                s.streams.contains_key(&teammate),
                "a live (InProgress) inline Task teammate must survive the reconcile"
            );
        });
    });
}

/// Rule 3 both ways: a fresh async `Agent` (recent snapshot, no stop_reason)
/// survives while a stale-mtime one — AND one with a terminal stop_reason — are
/// reconciled closed. A terminal async Agent's spawn tool-call being terminal is
/// spawn-ack and is NOT what closes it; the background_agent snapshot is.
#[gpui::test]
async fn reconcile_keeps_live_async_agent_and_closes_stale_one(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let live_toolu = "toolu_async_live";
    let stale_toolu = "toolu_async_stale";
    let stop_toolu = "toolu_async_stop";
    let live = crate::stream::StreamId::Teammate(SharedString::from(live_toolu));
    let stale = crate::stream::StreamId::Teammate(SharedString::from(stale_toolu));
    let stopped = crate::stream::StreamId::Teammate(SharedString::from(stop_toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            // Three async teammates, seeded via tagged body entries + their
            // background_agent registrations (async classification is by the
            // map, not by any tool-call entry — none is present here). The
            // spawn tool-calls being terminal must NOT be what drives the close.
            s.set_entries(
                vec![
                    reconcile_tagged_body(live_toolu),
                    reconcile_tagged_body(stale_toolu),
                    reconcile_tagged_body(stop_toolu),
                ],
                cx,
            );
            let register = |s: &mut crate::model::SolutionSession,
                            bg: &str,
                            parent: &str,
                            snap: crate::background_agent::BackgroundAgentSnapshot| {
                let bg_id = crate::background_agent::BackgroundAgentId::new(bg.to_string());
                s.background_agents.insert(
                    bg_id.clone(),
                    crate::background_agent::BackgroundAgent {
                        id: bg_id.clone(),
                        jsonl_path: "/nonexistent".into(),
                        registered_at: chrono::Utc::now(),
                        latest: Some(snap),
                        last_offset: 0,
                        parent_tool_use_id: Some(SharedString::from(parent.to_string())),
                    },
                );
                s.background_agent_order.push(bg_id);
            };
            register(
                s,
                "bg_live",
                live_toolu,
                crate::background_agent::BackgroundAgentSnapshot {
                    mtime: std::time::SystemTime::now(),
                    activity_label: SharedString::from("Generating…"),
                    stop_reason: None,
                },
            );
            register(
                s,
                "bg_stale",
                stale_toolu,
                crate::background_agent::BackgroundAgentSnapshot {
                    // Older than MANAGED_AGENT_STALE_TIMEOUT_SECS (120s).
                    mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(200),
                    activity_label: SharedString::from("Generating…"),
                    stop_reason: None,
                },
            );
            register(
                s,
                "bg_stop",
                stop_toolu,
                crate::background_agent::BackgroundAgentSnapshot {
                    mtime: std::time::SystemTime::now(),
                    activity_label: SharedString::from("done"),
                    stop_reason: Some(SharedString::from("end_turn")),
                },
            );
            assert!(s.streams.contains_key(&live));
            assert!(s.streams.contains_key(&stale));
            assert!(s.streams.contains_key(&stopped));
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                s.streams.contains_key(&live),
                "a fresh async agent (recent snapshot, no stop_reason) must survive"
            );
            assert!(
                !s.streams.contains_key(&stale),
                "an async agent stale beyond MANAGED_AGENT_STALE_TIMEOUT_SECS must be closed"
            );
            assert!(
                !s.streams.contains_key(&stopped),
                "an async agent with a terminal stop_reason must be closed"
            );
        });
    });
}

/// Snapshot-less async agent leak: an async `Agent` teammate whose JSONL never
/// produced a parseable snapshot (`latest == None`) has no mtime to age from.
/// Without the `registered_at` fallback its `Teammate` pill lingers forever
/// (the →Idle GC excludes async parents). A stale registration (>120s) must be
/// reconciled closed; a fresh one (<120s, still might snapshot) must survive.
#[gpui::test]
async fn reconcile_closes_snapshotless_stale_async_agent_but_keeps_fresh(
    cx: &mut TestAppContext,
) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let stale_toolu = "toolu_async_nosnap_stale";
    let fresh_toolu = "toolu_async_nosnap_fresh";
    let stale = crate::stream::StreamId::Teammate(SharedString::from(stale_toolu));
    let fresh = crate::stream::StreamId::Teammate(SharedString::from(fresh_toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s.set_entries(
                vec![
                    reconcile_tagged_body(stale_toolu),
                    reconcile_tagged_body(fresh_toolu),
                ],
                cx,
            );
            let register = |s: &mut crate::model::SolutionSession,
                            bg: &str,
                            parent: &str,
                            registered_at: chrono::DateTime<chrono::Utc>| {
                let bg_id = crate::background_agent::BackgroundAgentId::new(bg.to_string());
                s.background_agents.insert(
                    bg_id.clone(),
                    crate::background_agent::BackgroundAgent {
                        id: bg_id.clone(),
                        jsonl_path: "/nonexistent".into(),
                        registered_at,
                        // No parseable JSONL snapshot yet.
                        latest: None,
                        last_offset: 0,
                        parent_tool_use_id: Some(SharedString::from(parent.to_string())),
                    },
                );
                s.background_agent_order.push(bg_id);
            };
            // Registered > MANAGED_AGENT_STALE_TIMEOUT_SECS (120s) ago.
            register(
                s,
                "bg_nosnap_stale",
                stale_toolu,
                chrono::Utc::now() - chrono::Duration::seconds(200),
            );
            // Registered just now — still might snapshot.
            register(s, "bg_nosnap_fresh", fresh_toolu, chrono::Utc::now());
            assert!(s.streams.contains_key(&stale));
            assert!(s.streams.contains_key(&fresh));
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                !s.streams.contains_key(&stale),
                "a snapshot-less async agent stale beyond MANAGED_AGENT_STALE_TIMEOUT_SECS \
                 must age out via registered_at"
            );
            assert!(
                s.streams.contains_key(&fresh),
                "a snapshot-less async agent freshly registered must survive — the fallback \
                 must not reap live-but-not-yet-snapshotted agents"
            );
        });
    });
}

/// Rule 1: a teammate stream whose parent tool-call entry has vanished from the
/// thread (and which is not a registered async agent) is orphaned → closed.
#[gpui::test]
async fn reconcile_closes_teammate_whose_toolcall_vanished(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let toolu = "toolu_orphan";
    let teammate = crate::stream::StreamId::Teammate(SharedString::from(toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            // Only the tagged body entry — NO matching ToolCall entry and no
            // background_agent registration → the spawn tool-call is "gone".
            s.set_entries(vec![reconcile_tagged_body(toolu)], cx);
            assert!(
                s.streams.contains_key(&teammate),
                "orphan teammate stream exists before the reconcile"
            );
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                !s.streams.contains_key(&teammate),
                "a teammate whose spawn tool-call vanished must be reconciled closed"
            );
        });
    });
}

/// Task 9: an agent with a stale snapshot beyond
/// `MANAGED_AGENT_STALE_TIMEOUT + MANAGED_AGENT_DEAD_LINGER`
/// (V1 hardcoded: 120s + 300s = 420s) is removed on tick.
#[gpui::test]
async fn stale_agent_lingers_briefly_then_removed(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(500),
                        activity_label: SharedString::from("Bash: x"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                    parent_tool_use_id: None,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_agents(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "stale-beyond-linger agent must be removed on tick"
        );
    });
}

/// Task 9: a fresh (non-terminal, recent mtime) Managed Agent must NOT
/// be removed by `tick_background_agents` — only done + long-dead are
/// candidates. Guards against the tick over-pruning live work.
#[gpui::test]
async fn fresh_agent_survives_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Bash: x"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                    parent_tool_use_id: None,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_agents(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.contains_key(&bg_id),
            "fresh agent must survive a tick"
        );
    });
}

/// Helper: insert one background shell into a session's tracking maps.
fn insert_test_background_shell(
    cx: &mut TestAppContext,
    session_id: crate::model::SolutionSessionId,
    shell_id: &crate::background_shell::BackgroundShellId,
    state: crate::background_shell::ShellRuntimeState,
    latest: Option<crate::background_shell::BackgroundShellSnapshot>,
    registered_at: chrono::DateTime<chrono::Utc>,
) {
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_shells.insert(
                shell_id.clone(),
                crate::background_shell::BackgroundShell {
                    id: shell_id.clone(),
                    command: SharedString::from("sleep 1"),
                    output_path: "/nonexistent".into(),
                    registered_at,
                    latest,
                    last_offset: 0,
                    state,
                },
            );
            s.background_shell_order.push(shell_id.clone());
        });
    });
}

/// Task 10: an `Exited(Some(0))` background shell is reaped on the next
/// `tick_background_shells` pass (terminal-state arm).
#[gpui::test]
async fn exited_shell_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Exited(Some(0)),
        Some(crate::background_shell::BackgroundShellSnapshot {
            // Fresh mtime: proves it's the terminal state, not staleness,
            // driving the removal.
            mtime: std::time::SystemTime::now(),
            output_tail: SharedString::from("done"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "Exited shell must be removed on tick"
        );
        assert!(
            session.read(cx).background_shell_order.is_empty(),
            "order vec must be pruned in lockstep"
        );
    });
}

/// Task 10: a `Killed` background shell is reaped on tick (terminal-state arm).
#[gpui::test]
async fn killed_shell_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Killed,
        Some(crate::background_shell::BackgroundShellSnapshot {
            mtime: std::time::SystemTime::now(),
            output_tail: SharedString::from("killed"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "Killed shell must be removed on tick"
        );
    });
}

/// Task 10 — THE leak-prevention case: a still-`Running` shell whose
/// `latest.mtime` is older than stale+linger (420s) is reaped on tick. In the
/// current build a completed shell almost never flips to `Exited` (the
/// `<task-notification>` signal is dormant), so without this staleness arm the
/// finished shell would leak as a "Running" pill forever.
#[gpui::test]
async fn stale_running_shell_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        Some(crate::background_shell::BackgroundShellSnapshot {
            mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(10_000),
            output_tail: SharedString::from("...stalled output"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "stale-Running shell (mtime beyond stale+linger) must be reaped \
             on tick — else it leaks as a Running pill forever"
        );
    });
}

/// Task 10: a Running shell with NO snapshot but a stale `registered_at`
/// (zero output, long since launched) must still age out via the
/// registered_at fallback.
#[gpui::test]
async fn stale_running_shell_no_snapshot_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        None,
        chrono::Utc::now() - chrono::Duration::seconds(10_000),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "stale Running shell with no snapshot must age out via registered_at"
        );
    });
}

/// Task 10: a fresh `Running` shell (recent mtime, just registered) must NOT
/// be removed by `tick_background_shells`. Guards against over-pruning live
/// shells.
#[gpui::test]
async fn fresh_running_shell_survives_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        Some(crate::background_shell::BackgroundShellSnapshot {
            mtime: std::time::SystemTime::now(),
            output_tail: SharedString::from("running..."),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.contains_key(&shell_id),
            "fresh Running shell must survive a tick"
        );
    });
}

/// Pin the error-string set that `resume_session` treats as "session gone,
/// try the next cwd candidate (and ultimately mint a new ACP session)".
///
/// claude-code-acp returns `No conversation found with session ID: …` as the
/// MESSAGE of a JSON-RPC `-32603` (Internal error). Before the fix, our
/// predicate only matched `Resource not found` / `-32002` (the spec's
/// "missing resource" code) — so the message-only error fell through, the
/// `resume_session` for-loop broke after the first attempt, the
/// `solution.root` fallback never fired, and the user saw a raw
/// "No conversation found with session ID: …" snackbar on the next editor
/// restart even though the jsonl was sitting under sanitize(solution.root)
/// the whole time.
///
/// If you change the predicate, update both the match set and this test —
/// dropping a marker silently reintroduces the snackbar.
#[test]
fn is_session_gone_error_matches_known_markers() {
    use crate::store::is_session_gone_error;
    assert!(is_session_gone_error(
        "No conversation found with session ID: 877b9e1b-ae75-448e-bcef-906058b156df"
    ));
    assert!(is_session_gone_error("Resource not found"));
    assert!(is_session_gone_error(
        "RPC error -32002: session id no longer known"
    ));
    // Non-recoverable transport/auth/allow-list errors stay opaque so we
    // don't pointlessly retry against another cwd.
    assert!(!is_session_gone_error("connection refused"));
    assert!(!is_session_gone_error("authentication required"));
    assert!(!is_session_gone_error("permission denied: /home/spk/.spk"));
}

/// Task 13: hydrate-side reconcile drops SQLite rows whose JSONL file
/// no longer exists on disk — without this the strip would render dead
/// pills for runs whose worker dirs were wiped while the editor was
/// closed.
#[gpui::test]
async fn reconciliation_drops_rows_with_missing_jsonl(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![crate::db::BackgroundAgentRow {
        solution_session_id: session_id.to_string(),
        agent_id: "missing000000000000".into(),
        jsonl_path: "/nonexistent/path/missing.jsonl".into(),
        registered_at_ms: 0,
        last_seen_label: None,
        last_mtime_ms: None,
        stop_reason: None,
    }];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "row with missing JSONL file must not be registered"
        );
    });
}

/// Task 13: rows whose tail JSONL line carries a terminal `stop_reason`
/// are NOT re-registered — the worker already wrapped up while the
/// editor was closed, so re-adding it would resurrect a finished pill.
#[gpui::test]
async fn reconciliation_drops_done_rows(cx: &mut TestAppContext) {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("done.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"bye"}}],"stop_reason":"end_turn"}}}}"#
    )
    .unwrap();
    drop(f);

    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![crate::db::BackgroundAgentRow {
        solution_session_id: session_id.to_string(),
        agent_id: "done00000000000000".into(),
        jsonl_path: path.to_string_lossy().into_owned(),
        registered_at_ms: 0,
        last_seen_label: None,
        last_mtime_ms: None,
        stop_reason: None,
    }];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "row with terminal stop_reason must not be registered"
        );
    });
}

/// Cold-load decision: even an "alive-looking" row (file present, no terminal
/// stop_reason on the tail) is DROPPED, not re-registered — async `Agent`
/// subagents do not survive an editor restart, so the persisted row is stale
/// regardless of what its frozen JSONL tail shows. (Was `registers_alive_row`
/// before the drop-on-cold-load decision.)
#[gpui::test]
async fn reconciliation_drops_alive_row_too(cx: &mut TestAppContext) {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alive.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"hi"}}]}}}}"#
    )
    .unwrap();
    drop(f);

    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![crate::db::BackgroundAgentRow {
        solution_session_id: session_id.to_string(),
        agent_id: "alive0000000000000".into(),
        jsonl_path: path.to_string_lossy().into_owned(),
        registered_at_ms: 0,
        last_seen_label: None,
        last_mtime_ms: None,
        stop_reason: None,
    }];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        let s = session.read(cx);
        assert!(
            s.background_agents.is_empty(),
            "an alive-looking row must still be dropped on cold-load (agents don't survive restart)"
        );
    });
}

/// Cold-load decision: async `Agent` subagents do NOT survive an editor
/// restart, so their persisted `background_agents` rows are stale and must be
/// dropped — not restored — on cold-load. This seeds a row that under the OLD
/// behavior WOULD have been re-registered (file present, no terminal
/// `stop_reason` on the tail — i.e. the `reconciliation_registers_alive_row`
/// case) and asserts that after `reconcile_background_agents_for` NOTHING is
/// registered. Restoring it is what resurrected finished teammate pills after a
/// restart; with no re-registration there is no watcher, so the teammate's
/// stream stays a collapsed hydration orphan (no pill).
///
/// This test FAILS against the old restore behavior (which left
/// `background_agents` non-empty) and passes with the drop-all change.
#[gpui::test]
async fn cold_load_drops_all_background_agents(cx: &mut TestAppContext) {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alive.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    // Non-terminal tail line: no `stop_reason` → previously re-registered.
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"still working"}}]}}}}"#
    )
    .unwrap();
    drop(f);

    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![
        crate::db::BackgroundAgentRow {
            solution_session_id: session_id.to_string(),
            agent_id: "alive0000000000001".into(),
            jsonl_path: path.to_string_lossy().into_owned(),
            registered_at_ms: 0,
            last_seen_label: None,
            last_mtime_ms: None,
            stop_reason: None,
        },
        crate::db::BackgroundAgentRow {
            solution_session_id: session_id.to_string(),
            agent_id: "alive0000000000002".into(),
            jsonl_path: path.to_string_lossy().into_owned(),
            registered_at_ms: 0,
            last_seen_label: None,
            last_mtime_ms: None,
            stop_reason: None,
        },
    ];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        let s = session.read(cx);
        assert!(
            s.background_agents.is_empty(),
            "cold-load must register NO background agents (they don't survive a restart)"
        );
        assert!(
            s.background_agent_order.is_empty(),
            "no agent order entries either"
        );
        assert!(
            !s.streams
                .keys()
                .any(|id| matches!(id, crate::stream::StreamId::Teammate(_))),
            "no teammate stream pill should be present after cold-load reconcile"
        );
    });
}

/// Removes a directory subtree on drop — used to clean up the
/// `~/.claude/projects/<encoded-cwd>/` dir the live-scan test must create at
/// the real (home-derived) path the resolver computes.
struct CleanupDir(PathBuf);
impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// End-to-end: a Running shell flips to `Exited(Some(0))` when a realistic
/// single-line `<task-notification>` JSON `user` message is appended to the
/// parent session JSONL and `scan_parent_jsonl_for_completions` runs. Also
/// asserts the forward-only offset: a second scan with no new bytes is a
/// no-op (the shell stays exactly where the first scan left it).
#[gpui::test]
fn scan_parent_jsonl_flips_running_shell_to_exited(cx: &mut TestAppContext) {
    use crate::background_shell::{BackgroundShell, BackgroundShellId, ShellRuntimeState};

    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    // Unique cwd so the home-derived JSONL path never collides with a real
    // session or a parallel test run.
    let unique = format!(
        "/tmp/spk-scan-test-{}-{}",
        std::process::id(),
        SolutionSessionId::new()
    );
    let cwd = PathBuf::from(&unique);
    let acp_id = "ses-scan-xyz";

    let jsonl = super::parent_session_jsonl_for(&cwd, acp_id).expect("home_dir resolves in test");
    let project_dir = jsonl.parent().expect("jsonl has parent").to_path_buf();
    let _cleanup = CleanupDir(project_dir.clone());
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    // Pre-existing historical content the scan must skip past (offset lazy-init
    // to current EOF on first sight).
    std::fs::write(&jsonl, b"{\"type\":\"system\",\"old\":true}\n").expect("seed jsonl");

    let session_id = SolutionSessionId::new();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let entity = cx.new(|_| {
                let mut s = SolutionSession::new_idle(
                    session_id,
                    SolutionId("sol-scan".into()),
                    SharedString::from("claude-acp"),
                    agent_client_protocol::schema::SessionId::new(acp_id),
                );
                s.cwd = cwd.clone();
                let id = BackgroundShellId::new("bvb4ful1z");
                s.background_shells.insert(
                    id.clone(),
                    BackgroundShell {
                        id: id.clone(),
                        command: "sleep 60".into(),
                        output_path: PathBuf::from("/tmp/bvb4ful1z.output"),
                        registered_at: Utc::now(),
                        latest: None,
                        last_offset: 0,
                        state: ShellRuntimeState::Running,
                    },
                );
                s.background_shell_order.push(id);
                s
            });
            store.sessions.insert(session_id, entity);
            store
                .by_solution
                .entry(SolutionId("sol-scan".into()))
                .or_default()
                .push(session_id);
            // First scan: lazy-inits the offset to current EOF, flips nothing.
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        let shell = session
            .read(cx)
            .background_shells
            .get(&BackgroundShellId::new("bvb4ful1z"))
            .cloned()
            .unwrap();
        assert_eq!(
            shell.state,
            ShellRuntimeState::Running,
            "historical content must not flip the shell"
        );
    });

    // Append the realistic single-line completion notification.
    {
        use std::io::Write;
        let line = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<task-notification>\\n<task-id>bvb4ful1z</task-id>\\n<status>completed</status>\\n<summary>Background command \\\"sleep 60\\\" completed (exit code 0)</summary>\\n</task-notification>\"}]},\"uuid\":\"abc-123\"}\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .expect("reopen jsonl");
        f.write_all(line.as_bytes()).expect("append notification");
    }

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        let shell = session
            .read(cx)
            .background_shells
            .get(&BackgroundShellId::new("bvb4ful1z"))
            .cloned()
            .unwrap();
        assert_eq!(
            shell.state,
            ShellRuntimeState::Exited(Some(0)),
            "completion notification must flip Running -> Exited(0)"
        );
    });

    // Second scan, no new bytes: idempotent no-op (state unchanged).
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
        let session = store.read(cx).session(session_id).unwrap();
        let shell = session
            .read(cx)
            .background_shells
            .get(&BackgroundShellId::new("bvb4ful1z"))
            .cloned()
            .unwrap();
        assert_eq!(shell.state, ShellRuntimeState::Exited(Some(0)));
    });

    // Unknown-id notification flips nothing.
    {
        use std::io::Write;
        let line = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<task-notification>\\n<task-id>unknownid99</task-id>\\n<status>completed</status>\\n<summary>done (exit code 0)</summary>\\n</task-notification>\"}]}}\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .expect("reopen jsonl");
        f.write_all(line.as_bytes()).expect("append unknown");
    }
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
        let session = store.read(cx).session(session_id).unwrap();
        assert_eq!(session.read(cx).background_shells.len(), 1);
    });
}

// =====================================================================
// Create-implies-open: a freshly-created top-level session must be
// pinned into its solution's tab strip (tab_order set) so it surfaces
// on every open-set surface (desktop ConsolePanel + mobile workspace
// mirror). Regression coverage for the "session created on mobile is
// invisible everywhere / appears to vanish" bug — the root cause was
// `create_session` writing the row without ever opening it.
// =====================================================================

/// Read a session's `tab_order` out of the store.
fn tab_order_of(cx: &mut TestAppContext, id: SolutionSessionId) -> Option<i64> {
    cx.read(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(id)
            .expect("session exists")
            .read(cx)
            .tab_order
    })
}

#[gpui::test]
async fn create_session_pins_top_level_into_strip(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    assert_eq!(
        tab_order_of(cx, session_id),
        Some(0),
        "a freshly-created top-level session must be pinned into the strip (create implies open)"
    );
}

#[gpui::test]
async fn create_session_appends_subsequent_sessions_in_order(cx: &mut TestAppContext) {
    let (first, _thread, _tmp) = create_session_with_thread(cx).await;
    // A second top-level session in the SAME solution, via the store.
    let (solution_id, agent_id, project) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(first).expect("first session");
        let s = session.read(cx);
        (
            s.solution_id.clone(),
            s.agent_id.clone(),
            s.project.clone().expect("first session has project"),
        )
    });
    let second = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id, agent_id, project, cx)
            })
        })
        .await
        .expect("create second session");

    assert_eq!(tab_order_of(cx, first), Some(0), "first stays at index 0");
    assert_eq!(
        tab_order_of(cx, second),
        Some(1),
        "second top-level session appends after the first"
    );
}

#[gpui::test]
async fn create_child_session_is_not_pinned(cx: &mut TestAppContext) {
    let (parent, _thread, _tmp) = create_session_with_thread(cx).await;
    let (solution_id, agent_id, project) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(parent).expect("parent session");
        let s = session.read(cx);
        (
            s.solution_id.clone(),
            s.agent_id.clone(),
            s.project.clone().expect("parent has project"),
        )
    });
    let child = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session_with_parent(
                    solution_id,
                    agent_id,
                    project,
                    None,
                    Some(parent),
                    None,
                    None,
                    false,
                    false,
                    cx,
                )
            })
        })
        .await
        .expect("create child session");

    assert_eq!(tab_order_of(cx, parent), Some(0), "parent is pinned");
    assert_eq!(
        tab_order_of(cx, child),
        None,
        "a sub-agent (parent_session_id set) must NOT be pinned as a top-level tab"
    );
}

#[gpui::test]
async fn open_session_in_strip_is_idempotent(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    // Already pinned at create. A second open must not bump or duplicate
    // the order.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.open_session_in_strip(session_id, cx));
    });
    assert_eq!(
        tab_order_of(cx, session_id),
        Some(0),
        "re-opening an already-pinned session is a no-op"
    );
}

/// Idle-flush (queue drained on `Stopped`) must prepend the "not a reply"
/// hint so the agent knows the follow-up arrived after its last response.
#[gpui::test]
async fn idle_flush_prepends_not_a_reply_hint(cx: &mut TestAppContext) {
    let (session_id, thread, _tmp) = create_session_with_thread(cx).await;

    let entries_before = cx.update(|cx| thread.read(cx).entries().len());

    // Force Running so send_message takes the queueing branch.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.session(session_id).unwrap().update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            store
                .send_message(session_id, "LATE_FOLLOWUP".to_string(), cx)
                .detach_and_log_err(cx);
        });
    });

    // Confirm the message is queued, not yet sent.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                1,
                "message queued while Running"
            );
        });
    });

    // Emit Stopped(EndTurn) — the Stopped handler transitions to Idle and
    // flushes the queue as a new turn WITH the not-a-reply hint prepended.
    cx.update(|cx| {
        thread.update(cx, |_thread, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                agent_client_protocol::schema::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    // Queue must be drained.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            assert_eq!(
                store
                    .session(session_id)
                    .unwrap()
                    .read(cx)
                    .pending_messages
                    .len(),
                0,
                "queue flushed after Stopped"
            );
        });
    });

    // The flushed turn must have pushed a new UserMessage entry.  Its
    // `chunks` carry the raw blocks we sent — verify the hint and the
    // follow-up text are both present.
    let found = cx.update(|cx| {
        thread.read(cx).entries()[entries_before..].iter().any(|e| {
            if let acp_thread::AgentThreadEntry::UserMessage(msg) = e {
                let text: String = msg
                    .chunks
                    .iter()
                    .filter_map(|b| {
                        if let agent_client_protocol::schema::ContentBlock::Text(t) = b {
                            Some(t.text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                text.contains(crate::store::queue::QUEUE_HINT_LINE)
                    && text.contains("LATE_FOLLOWUP")
            } else {
                false
            }
        })
    });
    assert!(
        found,
        "idle-flush new turn must carry the not-a-reply hint and the follow-up text"
    );
}

#[test]
fn persisted_session_round_trips_models() {
    let p = PersistedSession {
        title: "t".into(),
        entries: vec![],
        entry_summaries: vec![],
        entries_v2: vec![],
        entry_created_ms: vec![],
        available_models: vec![claude_native::ModelInfo {
            value: "opus".into(),
            display_name: "Opus".into(),
            description: "".into(),
        }],
        desired_model: Some("opus".into()),
        desired_effort: Some("high".into()),
    };
    let bytes = serde_json::to_vec(&p).unwrap();
    let back: PersistedSession = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back.available_models.len(), 1);
    assert_eq!(back.available_models[0].value, "opus");
    assert_eq!(back.desired_model.as_deref(), Some("opus"));
    assert_eq!(back.desired_effort.as_deref(), Some("high"));
    // Old blobs without the new fields still decode (serde default).
    let old = serde_json::json!({"title":"t","entry_summaries":[]});
    let back2: PersistedSession = serde_json::from_value(old).unwrap();
    assert!(back2.available_models.is_empty());
    assert!(back2.desired_model.is_none());
    assert!(
        back2.desired_effort.is_none(),
        "old blobs without desired_effort decode to None via serde default"
    );
}

/// Cold path (no live `acp_thread`): `select_model` records `desired_model`
/// and `selected_model` reflects it, without any live connection. This is the
/// primary user scenario — picking a model on a cold tab before waking it.
#[gpui::test]
fn select_model_on_cold_session_records_desired(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let id = SolutionSessionId::new();
            let session = insert_cold_session(
                id,
                SolutionId("sol-a".into()),
                SharedString::from("claude-acp"),
                None,
                None,
                store,
                cx,
            );
            session.update(cx, |s, _| {
                s.cached_models = vec![
                    claude_native::ModelInfo {
                        value: "opus".into(),
                        display_name: "Opus".into(),
                        description: "".into(),
                    },
                    claude_native::ModelInfo {
                        value: "sonnet".into(),
                        display_name: "Sonnet".into(),
                        description: "".into(),
                    },
                ];
            });

            let models = store.session_models(id, cx);
            assert_eq!(models.len(), 2);
            assert_eq!(models[0].value, "opus");
            assert_eq!(models[1].value, "sonnet");

            assert!(store.selected_model(id, cx).is_none());

            store.select_model(id, "sonnet".into(), cx);

            assert_eq!(
                session.read(cx).desired_model.as_deref(),
                Some("sonnet"),
                "select_model must record desired_model on the cold session"
            );
            assert_eq!(
                store.selected_model(id, cx).as_deref(),
                Some("sonnet"),
                "selected_model must reflect the explicit desired_model"
            );
        });
    });
}

/// `new_chat_model_options` derives the model list + default selection for a
/// brand-new session from the pair's most-recently-active existing session:
/// its captured `cached_models` and chosen `desired_model`.
#[gpui::test]
fn new_chat_model_options_from_latest_session(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let sol = SolutionId("sol-a".into());
    let agent: AgentServerId = SharedString::from("claude-acp");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            // No sessions yet for the pair → empty list + None.
            let (models, selected) = store.new_chat_model_options(&sol, &agent, cx);
            assert!(models.is_empty());
            assert!(selected.is_none());

            let id = SolutionSessionId::new();
            let session =
                insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);
            session.update(cx, |s, _| {
                s.cached_models = vec![
                    claude_native::ModelInfo {
                        value: "opus".into(),
                        display_name: "Opus".into(),
                        description: "".into(),
                    },
                    claude_native::ModelInfo {
                        value: "sonnet".into(),
                        display_name: "Sonnet".into(),
                        description: "".into(),
                    },
                ];
                s.desired_model = Some("opus".into());
            });

            let (models, selected) = store.new_chat_model_options(&sol, &agent, cx);
            assert_eq!(models.len(), 2);
            assert_eq!(models[0].value, "opus");
            assert_eq!(models[1].value, "sonnet");
            assert_eq!(selected.as_deref(), Some("opus"));
        });
    });
}

/// A FRESH session (no turn yet → empty `cached_models`) must still offer a
/// model picker by falling back to the GLOBAL per-agent cache (`agent_models`),
/// which is filled by the first live capture / create-time probe of any session
/// of that agent. Mirrors the live capture by seeding `agent_models` directly.
#[gpui::test]
fn session_models_falls_back_to_global_agent_cache(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let sol = SolutionId("sol-a".into());
    let agent: AgentServerId = SharedString::from("claude-acp");

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            // Stand in for the first live capture: a sibling session of this
            // agent populated the global cache.
            store.agent_models.insert(
                agent.clone(),
                vec![
                    claude_native::ModelInfo {
                        value: "opus".into(),
                        display_name: "Opus".into(),
                        description: "".into(),
                    },
                    claude_native::ModelInfo {
                        value: "sonnet".into(),
                        display_name: "Sonnet".into(),
                        description: "".into(),
                    },
                ],
            );

            // A brand-new session with an empty per-session list.
            let fresh_id = SolutionSessionId::new();
            let fresh =
                insert_cold_session(fresh_id, sol.clone(), agent.clone(), None, None, store, cx);
            assert!(
                fresh.read(cx).cached_models.is_empty(),
                "precondition: fresh session has no per-session model list"
            );

            let models = store.session_models(fresh_id, cx);
            assert_eq!(
                models.len(),
                2,
                "fresh session must surface the global agent model list"
            );
            assert_eq!(models[0].value, "opus");
            assert_eq!(models[1].value, "sonnet");
        });
    });
}

/// Phase 2, Task 2: after a `NewEntry` event the session's `entries` list is
/// rebuilt and reflects the full cold+live entry count. Mirrors the harness
/// from `append_stamps_entry_created_ms_once_per_index`.
#[gpui::test]
async fn new_entry_rebuilds_session_entries(cx: &mut TestAppContext) {
    use crate::session_entry::SessionEntryKind;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Append a user entry → NewEntry fires → entries should be rebuilt.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Append an assistant entry → second NewEntry.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        // Cold prefix is empty for a fresh session; live has 2 entries → entries must be 2.
        assert_eq!(
            s.entries.len(),
            2,
            "entries must equal cold({}) + live(2) after two NewEntry events",
            s.live_base
        );
        // The last entry must be the assistant message we just appended.
        assert!(
            matches!(
                s.entries.last().unwrap().kind,
                SessionEntryKind::AssistantMessage { .. }
            ),
            "last entries element must be AssistantMessage, got {:?}",
            s.entries.last().unwrap().kind
        );
    });
}

/// Phase 2, Task 4: after two `NewEntry` events followed by an `EntryUpdated`
/// on the first entry, the first entry's `created_ms` must NOT change (no
/// restamp on in-place updates), and the second entry must still exist.
#[gpui::test]
async fn entry_updated_preserves_created_ms(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // First NewEntry: user message.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Second NewEntry: assistant message.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Capture the created_ms for entry 0 and verify both entries are present.
    let original_entry0_created_ms = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "two NewEntry events → two entries");
        assert!(
            s.entries[0].created_ms > 0,
            "entry 0 must have a real positive created_ms after NewEntry"
        );
        s.entries[0].created_ms
    });

    // EntryUpdated on entry 0 (the user message at local index 0 — emit directly).
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        // Both entries must still be present.
        assert_eq!(
            s.entries.len(),
            2,
            "EntryUpdated must not change entries count"
        );
        // Entry 0's created_ms must be unchanged (no restamp on update).
        assert_eq!(
            s.entries[0].created_ms, original_entry0_created_ms,
            "EntryUpdated must not restamp entry 0's created_ms"
        );
        // Entry 1 must still exist.
        assert!(
            s.entries[1].created_ms > 0,
            "entry 1 must retain its positive created_ms after update on entry 0"
        );
    });
}

/// Phase 3, Task 2: `mod_seq` is stamped on every live mutation.
/// After two `NewEntry` events, entry0.mod_seq==1 and entry1.mod_seq==2.
/// After `EntryUpdated(0)`, entry0.mod_seq==3 (advanced) and entry1.mod_seq==2
/// (unchanged). `entry0.created_ms` must be preserved across the update.
#[gpui::test]
async fn mod_seq_stamped_on_live_mutations(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // First NewEntry: user message.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Second NewEntry: assistant message.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // After two NewEntry events: entry0.mod_seq==1, entry1.mod_seq==2.
    let original_entry0_created_ms = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "two NewEntry events → two entries");
        // The first NewEntry flips Idle→Running through `mutate_state`, which now
        // consumes one `change_seq` for the `state_seq` watermark (decision 1:
        // entry mod_seq and section watermarks share one monotonic clock). So the
        // first entry stamps mod_seq 2 (seq 1 went to the state flip); the second
        // NewEntry finds the session already Running (no flip, no extra bump) and
        // stamps mod_seq 3.
        assert_eq!(
            s.entries[0].mod_seq, 2,
            "entry0.mod_seq must be 2 (seq 1 consumed by the Idle→Running state flip)"
        );
        assert_eq!(
            s.entries[1].mod_seq, 3,
            "entry1.mod_seq must be 3 after second NewEntry"
        );
        s.entries[0].created_ms
    });

    // EntryUpdated on entry 0 (local index 0).
    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::EntryUpdated(0));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.entries.len(),
            2,
            "EntryUpdated must not change entries count"
        );
        assert_eq!(
            s.entries[0].mod_seq, 4,
            "entry0.mod_seq must advance to 4 after EntryUpdated(0)"
        );
        assert_eq!(
            s.entries[1].mod_seq, 3,
            "entry1.mod_seq must remain 3 (unchanged by EntryUpdated on entry0)"
        );
        assert_eq!(
            s.entries[0].created_ms, original_entry0_created_ms,
            "EntryUpdated must not restamp entry0.created_ms"
        );
    });
}

/// Fix 4: after cold-restore of a NON-EMPTY prefix, attaching a live thread
/// sets `live_base` to the prefix length. A subsequent `NewEntry` must land
/// at `entries[live_base]` (AFTER the cold prefix) and leave the cold prefix
/// entries unchanged.
#[gpui::test]
async fn new_entry_after_cold_prefix_lands_at_live_base(cx: &mut TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Inject 2 fake cold SessionEntries and re-attach the same acp_thread so
    // live_base is recomputed from entries.len() = 2.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, cx| {
                s.entries = vec![
                    SessionEntry {
                        created_ms: 1_700_000_000_000,
                        mod_seq: 0,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "cold user".to_string(),
                            chunks: vec![],
                        },
                    },
                    SessionEntry {
                        created_ms: 1_700_000_001_000,
                        mod_seq: 0,
                        subagent_id: None,
                        kind: SessionEntryKind::AssistantMessage { chunks: vec![] },
                    },
                ];
                // Re-attach the same thread so live_base = entries.len() = 2.
                s.set_acp_thread(Some(acp_thread.clone()), cx);
            });
        });
    });
    cx.executor().run_until_parked();

    // Confirm live_base == 2 and is_cold() == false.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(s.live_base, 2, "live_base must equal cold prefix length");
        assert!(!s.is_cold(), "session has a live thread");
    });

    // Fire a NewEntry via push_user_content_block.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("live msg".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        // Total entries = 2 cold + 1 live.
        assert_eq!(
            s.entries.len(),
            3,
            "entries must be cold(2) + live(1) = 3, got {}",
            s.entries.len()
        );
        // Cold prefix entries must be unchanged.
        assert_eq!(
            s.entries[0].created_ms, 1_700_000_000_000,
            "cold entry[0] created_ms must be unchanged"
        );
        assert_eq!(
            s.entries[1].created_ms, 1_700_000_001_000,
            "cold entry[1] created_ms must be unchanged"
        );
        // New entry must be at index live_base (= 2) with a real positive timestamp.
        assert!(
            s.entries[2].created_ms > 0,
            "live entry at entries[2] must have a positive created_ms, got {}",
            s.entries[2].created_ms
        );
        assert!(
            matches!(s.entries[2].kind, SessionEntryKind::UserMessage { .. }),
            "live entry at entries[2] must be UserMessage, got {:?}",
            s.entries[2].kind
        );
    });
}

/// Phase 3, Task 3: cold-restored entries get ascending `mod_seq` (1-based)
/// and `change_seq` is re-seated so the first live `NewEntry` stamps the next
/// monotonic value.
///
/// Setup: create a session with a live thread, then replace its entries with 2
/// cold entries built by `rebuild_entries(base_seq=0)` and seed `change_seq`
/// via `init_change_seq_from_entries`.  Re-attach the live thread so the store
/// observes `NewEntry`.  Fire one user message and assert the new entry's
/// `mod_seq` == 3.
#[gpui::test]
async fn cold_restore_stamps_mod_seq_and_reseats_change_seq(cx: &mut TestAppContext) {
    use crate::cold_persistence::{
        PersistedAssistantChunk, PersistedAssistantMessage, PersistedEntryV2, PersistedUserMessage,
    };
    use crate::session_entry::SessionEntryKind;

    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Build 2 cold AgentThreadEntry values via the persisted → cold pipeline,
    // then call `rebuild_entries` with `base_seq = 0` to get stamped SessionEntries.
    let (cold_entries, created_ms) = cx.update(|cx| {
        crate::store::cold_entries_from_persisted(
            Some(crate::store::PersistedSession {
                title: "test".into(),
                entries: vec![],
                entry_summaries: vec![],
                entries_v2: vec![
                    PersistedEntryV2::User(PersistedUserMessage {
                        id: None,
                        content_md: "cold user".into(),
                        chunks: vec![],
                    }),
                    PersistedEntryV2::Assistant(PersistedAssistantMessage {
                        chunks: vec![PersistedAssistantChunk::Message("cold reply".into())],
                    }),
                ],
                entry_created_ms: vec![1_700_000_000_000, 1_700_000_001_000],
                available_models: vec![],
                desired_model: None,
                desired_effort: None,
            }),
            cx,
        )
    });

    // Inject the cold entries and re-seat change_seq, then re-attach the live
    // thread so the store's observe_task_notification hook sees subsequent NewEntry.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            session.update(cx, |s, cx| {
                let stamped =
                    crate::session_entry::rebuild_entries(&cold_entries, &[], &created_ms, 0, cx);
                s.set_entries(stamped, cx);
                s.init_change_seq_from_entries();
                // Re-attach the live thread so live_base = 2 and the store
                // resumes observing AcpThreadEvent notifications.
                s.set_acp_thread(Some(acp_thread.clone()), cx);
            });
        });
    });
    cx.executor().run_until_parked();

    // Assert: mod_seq stamped 1..=2 (N = 2); `init_change_seq_from_entries`
    // re-seats change_seq to max(mod_seq) then seeds the three section
    // watermarks above it (decision 3), so change_seq lands at N + 3 = 5 and the
    // watermarks are N+1, N+2, N+3, each strictly above the restored entries.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 2, "expected 2 cold entries");
        assert_eq!(s.entries[0].mod_seq, 1, "cold entry[0].mod_seq must be 1");
        assert_eq!(s.entries[1].mod_seq, 2, "cold entry[1].mod_seq must be 2");
        assert_eq!(
            s.change_seq, 5,
            "change_seq must be max(mod_seq)=2 + 3 watermark bumps after cold restore"
        );
        assert_eq!(s.queue_seq, 3, "queue_seq = N + 1");
        assert_eq!(s.subagents_seq, 4, "subagents_seq = N + 2");
        assert_eq!(s.state_seq, 5, "state_seq = N + 3");
        for w in [s.queue_seq, s.subagents_seq, s.state_seq] {
            assert!(w > 2, "section watermark {w} must be > max(mod_seq)=2");
        }
    });

    // Fire one live NewEntry. The session is Idle, so the NewEntry first flips
    // Idle→Running through `mutate_state` (consuming seq 6 for the `state_seq`
    // watermark — shared clock, decision 1), then stamps the entry at seq 7.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("live msg".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(s.entries.len(), 3, "entries must be cold(2) + live(1) = 3");
        assert_eq!(
            s.entries[2].mod_seq, 7,
            "first live NewEntry stamps mod_seq == 7 (3 restore watermark bumps + 1 \
             Idle→Running state-flip bump precede it)"
        );
        assert!(
            matches!(s.entries[2].kind, SessionEntryKind::UserMessage { .. }),
            "live entry must be UserMessage"
        );
    });
}

/// Phase 5, Task 5.1: enqueuing a follow-up onto a Running session emits
/// `SessionQueueChanged` and must move `queue_seq` to the freshly-allocated
/// `change_seq` (and to a value > 0). Only the queue watermark advances — the
/// enqueue path emits a bare `SessionStateChanged` for the live Queued bubble
/// WITHOUT a `state_seq` bump (it does not route through `mark_state_changed`),
/// so the state watermark is intentionally left untouched here.
#[gpui::test]
async fn enqueue_bumps_queue_watermark(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session exists");
            // Force Running so send_message_blocks takes the queueing branch
            // (the branch that emits SessionQueueChanged).
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            let queue_seq_before = session.read(cx).queue_seq;
            assert_eq!(queue_seq_before, 0, "fresh session starts with queue_seq 0");
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("follow-up".to_string()),
            )];
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(s.pending_messages.len(), 1, "message queued");
        assert!(s.queue_seq > 0, "queue_seq must advance off zero");
        assert_eq!(
            s.queue_seq, s.change_seq,
            "queue_seq must equal the freshly-allocated change_seq"
        );
    });
}

/// Phase 5, Task 5.1: registering a subagent tab emits `SessionSubagentsChanged`
/// and must move `subagents_seq` to the freshly-allocated `change_seq`.
#[gpui::test]
async fn subagent_spawn_bumps_subagents_watermark(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let subagents_seq_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .subagents_seq
    });
    assert_eq!(subagents_seq_before, 0, "fresh session starts at 0");

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_wm_1",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(s.teammate_labels.len(), 1, "one teammate label captured");
        assert!(s.subagents_seq > 0, "subagents_seq must advance off zero");
        assert_eq!(
            s.subagents_seq, s.change_seq,
            "subagents_seq must equal the freshly-allocated change_seq"
        );
    });
}

/// The `→Idle` subagent-strip GC in `mutate_state` clears a stranded strip and
/// MUST move `subagents_seq` (and emit `SessionSubagentsChanged`), exactly like
/// an explicit removal — otherwise the mobile delta (and the desktop strip
/// view) never learns the strip emptied and a finished subagent tab strands.
#[gpui::test]
async fn idle_transition_gc_bumps_subagents_watermark(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Strand a subagent pill: register an InProgress Task (non-terminal, so the
    // per-tool-call removal path never fires) and force the session Running.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_gc_1",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    let subagents_seq_after_spawn = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                // Seed a teammate-tagged entry so the demux produces a live
                // `Teammate` stream — since wire v5 the →Idle GC sources stranded
                // ids from `streams`, so a pill with no stream is not "stranded".
                s.entries = vec![SessionEntry {
                    created_ms: 1_700_000_000_000,
                    mod_seq: 1,
                    subagent_id: Some(SharedString::from("toolu_gc_1")),
                    kind: SessionEntryKind::AssistantMessage {
                        chunks: vec![AssistantChunk::Message("sub work".into())],
                    },
                }];
                s.rebuild_streams();
            });
            let seq = session.read(cx).subagents_seq;
            assert!(
                session.read(cx).teammate_labels.contains_key("toolu_gc_1"),
                "label captured"
            );
            assert!(
                session.read(cx).streams.contains_key(
                    &crate::stream::StreamId::Teammate(SharedString::from("toolu_gc_1"))
                ),
                "teammate stream stranded"
            );
            seq
        })
    });

    // Transition into Idle through `mutate_state` — the GC must fire.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert!(
            s.teammate_labels.is_empty()
                && !s.streams.contains_key(&crate::stream::StreamId::Teammate(
                    SharedString::from("toolu_gc_1")
                )),
            "→Idle GC must close the stranded stream + reclaim its label"
        );
        assert!(
            s.subagents_seq > subagents_seq_after_spawn,
            "the GC clear must advance subagents_seq ({} !> {})",
            s.subagents_seq,
            subagents_seq_after_spawn
        );
        assert_eq!(
            s.subagents_seq, s.change_seq,
            "subagents_seq must equal the freshly-allocated change_seq after GC"
        );
    });
}

/// Phase 6c regression: the `→Idle` subagent-strip GC sources stranded ids from
/// `session.streams` (the desktop snap-back `next_selection_after_change` also
/// watches `streams`), and MUST `close_stream` each stranded teammate — otherwise the stream
/// keeps re-demuxing Live from its still-tagged `entries` and the viewer strands on
/// a frozen, pill-less tab (the 14h-stuck-tab class this GC exists to prevent).
#[gpui::test]
async fn idle_transition_gc_closes_stranded_teammate_stream(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Strand an inline-Task pill (non-terminal, so the per-tool-call removal never
    // fires) exactly like `idle_transition_gc_bumps_subagents_watermark`.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_gc_2",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    let teammate = crate::stream::StreamId::Teammate(SharedString::from("toolu_gc_2"));
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                // Seed a teammate-tagged entry so the demux produces a live
                // `Teammate` stream (the pill alone doesn't create one).
                s.entries = vec![SessionEntry {
                    created_ms: 1_700_000_000_000,
                    mod_seq: 1,
                    subagent_id: Some(SharedString::from("toolu_gc_2")),
                    kind: SessionEntryKind::AssistantMessage {
                        chunks: vec![AssistantChunk::Message("sub work".into())],
                    },
                }];
                s.rebuild_streams();
            });
            let s = session.read(cx);
            assert!(
                s.teammate_labels.contains_key("toolu_gc_2"),
                "label captured"
            );
            assert!(
                s.streams.contains_key(&teammate),
                "teammate stream must exist before the GC"
            );
        });
    });

    // Transition into Idle through `mutate_state` — the GC must fire.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert!(
            s.teammate_labels.is_empty(),
            "→Idle GC must reclaim the stranded label"
        );
        assert!(
            !s.streams.contains_key(&teammate),
            "→Idle GC must close the stranded teammate stream (still-tagged rows notwithstanding)"
        );
        assert!(
            s.streams.contains_key(&crate::stream::StreamId::Main),
            "Main stream survives the GC"
        );
    });
}

/// The →Idle GC must EXCLUDE a live async `Agent` teammate — its stream outlives
/// the parent turn (it closes on the real `stop_reason`, not on →Idle). Regression
/// guard for the `async_parents` exclusion set (`store.rs`, sourced from
/// `background_agents[*].parent_tool_use_id`): closing it here would suppress a
/// still-streaming async teammate (decision #5) and drop its label.
#[gpui::test]
async fn idle_transition_gc_excludes_live_async_agent_teammate(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // An async `Agent` spawn tool-call reaches terminal (Completed) at spawn-ack
    // while the teammate keeps streaming — register it as an `Agent` (not `Task`)
    // so the terminal path leaves its stream open.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_async_gc",
                    "Agent",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Async worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert agent");
        });
    });
    cx.executor().run_until_parked();

    let teammate = crate::stream::StreamId::Teammate(SharedString::from("toolu_async_gc"));
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                // Live async teammate: a tagged entry (its demux stream) + a
                // `background_agents` registration whose `parent_tool_use_id` is
                // the teammate id — this is what marks it async-and-live for the
                // →Idle exclusion set.
                s.entries = vec![SessionEntry {
                    created_ms: 1_700_000_000_000,
                    mod_seq: 1,
                    subagent_id: Some(SharedString::from("toolu_async_gc")),
                    kind: SessionEntryKind::AssistantMessage {
                        chunks: vec![AssistantChunk::Message("async work in flight".into())],
                    },
                }];
                let bg_id = crate::background_agent::BackgroundAgentId::new("bg_async_gc");
                s.background_agents.insert(
                    bg_id.clone(),
                    crate::background_agent::BackgroundAgent {
                        id: bg_id.clone(),
                        jsonl_path: "/nonexistent".into(),
                        registered_at: chrono::Utc::now(),
                        // No terminal stop_reason → still running.
                        latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                            mtime: std::time::SystemTime::now(),
                            activity_label: SharedString::from("Bash: cargo test"),
                            stop_reason: None,
                        }),
                        last_offset: 0,
                        parent_tool_use_id: Some(SharedString::from("toolu_async_gc")),
                    },
                );
                s.background_agent_order.push(bg_id);
                s.rebuild_streams();
            });
            let s = session.read(cx);
            assert!(
                s.streams.contains_key(&teammate),
                "async teammate stream must exist before the GC"
            );
            assert!(
                s.teammate_labels.contains_key("toolu_async_gc"),
                "async teammate label captured at registration"
            );
        });
    });

    // →Idle: the GC fires, but the async teammate is excluded.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert!(
            s.streams.contains_key(&teammate),
            "→Idle GC must NOT close a live async agent's teammate stream"
        );
        assert!(
            s.teammate_labels.contains_key("toolu_async_gc"),
            "→Idle GC must NOT drop a live async agent's label"
        );
    });
}

/// The per-source-streams migration's stream-lifecycle (the →Idle strip GC +
/// `close_stream` + `rebuild_streams`) must NOT touch `last_activity_at` — it is
/// the supervisor's silence clock (`quiet_since_ms`), and a spurious bump on the
/// →Idle transition would push the judge's deadline forward every idle tick and
/// starve `should_fire` forever ("supervisor never fires" regression guard). The
/// GC runs INSIDE `mutate_state(Idle)`, which must leave the clock alone; only
/// the separate turn-completion path (a distinct statement) legitimately bumps it.
#[gpui::test]
async fn idle_transition_gc_does_not_bump_last_activity_at(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // A stranded inline-Task teammate (non-terminal, so the per-tool-call removal
    // never fires) — exactly the input the →Idle GC closes.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_gc_clock",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker"),
                    None,
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    // Pin the silence clock to a fixed instant in the past so any bump is visible.
    let t0 = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(1_700_000_000_000)
        .expect("valid timestamp");
    let teammate = crate::stream::StreamId::Teammate(SharedString::from("toolu_gc_clock"));
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
                s.entries = vec![SessionEntry {
                    created_ms: 1_700_000_000_000,
                    mod_seq: 1,
                    subagent_id: Some(SharedString::from("toolu_gc_clock")),
                    kind: SessionEntryKind::AssistantMessage {
                        chunks: vec![AssistantChunk::Message("sub work".into())],
                    },
                }];
                s.rebuild_streams();
                s.last_activity_at = t0;
            });
            assert!(
                session.read(cx).streams.contains_key(&teammate),
                "teammate stream must exist before the GC"
            );
        });
    });

    // →Idle: the GC (close_stream over stranded teammates + rebuild_streams) runs
    // inside this call.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.mutate_state(session_id, |state| *state = SessionState::Idle, cx);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert!(
            !s.streams.contains_key(&teammate),
            "→Idle GC must have closed the stranded teammate stream"
        );
        assert_eq!(
            s.last_activity_at, t0,
            "→Idle GC / close_stream / rebuild_streams must NOT bump last_activity_at \
             (it is the supervisor's silence clock — a bump here starves should_fire)"
        );
    });
}

/// Phase 5, Task 5.1: a discriminant-changing transition through `mutate_state`
/// (Idle → Running) emits `SessionStateChanged` and must move `state_seq` to the
/// freshly-allocated `change_seq`.
#[gpui::test]
async fn mutate_state_bumps_state_watermark(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    // Pin to Idle so the Idle → Running flip is a real discriminant change.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| s.state = SessionState::Idle);
            let state_seq_before = session.read(cx).state_seq;
            store.mutate_state(
                session_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    };
                },
                cx,
            );
            let s = session.read(cx);
            assert!(
                s.state_seq > state_seq_before,
                "state_seq must advance on a discriminant-changing transition"
            );
            assert_eq!(
                s.state_seq, s.change_seq,
                "state_seq must equal the freshly-allocated change_seq"
            );
        });
    });
}

/// Regression (Chat Supervisor port, C1/I1): a state change on an
/// `is_supervisor_ephemeral` session (the hidden judge/auditor) must NOT emit a
/// sequenced `workspace.session_state_changed` notification — that event is
/// forwarded to mobile by `remote_control`'s allow-list, so an unfiltered emit
/// would leak the invisible judge's Idle→Running→Idle churn on every supervisor
/// wake-up. Asserted via the coordinator's seq counter: `emit_sequenced` advances
/// it, the suppressed path does not. A non-ephemeral control session in the same
/// test confirms the guard is specific to the ephemeral flag (and that internal
/// bookkeeping — `state_seq` — still advances for the judge).
#[gpui::test]
async fn ephemeral_session_state_change_does_not_emit_workspace_event(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(editor_mcp::workspace_seq::install);

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            // Mark the session as the hidden ephemeral supervisor judge.
            session.update(cx, |s, _| {
                s.is_supervisor_ephemeral = true;
                s.state = SessionState::Idle;
            });

            let coord = editor_mcp::workspace_seq::WorkspaceEventCoordinator::global(cx);
            let seq_before = coord.current_seq();
            let state_seq_before = session.read(cx).state_seq;

            // Idle → Running is a real discriminant change, so the wire emit
            // would fire for a visible session.
            store.mutate_state(
                session_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    };
                },
                cx,
            );

            let coord = editor_mcp::workspace_seq::WorkspaceEventCoordinator::global(cx);
            assert_eq!(
                coord.current_seq(),
                seq_before,
                "ephemeral judge state change must NOT advance the workspace seq \
                 (no workspace.session_state_changed emit leaks to mobile)"
            );
            // Internal bookkeeping must still run: the judge's own state_seq
            // advances so `message_generator`'s Idle/Errored await still resolves.
            assert!(
                session.read(cx).state_seq > state_seq_before,
                "internal state_seq must still advance for the ephemeral session"
            );
        });
    });

    // Control: a NON-ephemeral session in the same store DOES emit, proving the
    // guard keys on `is_supervisor_ephemeral` and not on some unrelated path.
    let (visible_id, _thread2, _tmp2) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(visible_id).expect("session");
            session.update(cx, |s, _| s.state = SessionState::Idle);
            let coord = editor_mcp::workspace_seq::WorkspaceEventCoordinator::global(cx);
            let seq_before = coord.current_seq();
            store.mutate_state(
                visible_id,
                |state| {
                    *state = SessionState::Running {
                        started_at: std::time::Instant::now(),
                        notified: false,
                    };
                },
                cx,
            );
            let coord = editor_mcp::workspace_seq::WorkspaceEventCoordinator::global(cx);
            assert!(
                coord.current_seq() > seq_before,
                "a visible session's state change MUST advance the workspace seq"
            );
        });
    });
}

/// Phase 3, Task 4: `reset_context` (/clear) must increment `epoch` by exactly 1,
/// because it replaces the transcript wholesale. A plain `NewEntry` must NOT bump
/// `epoch` (only `change_seq` / `mod_seq` advance on live appends).
#[gpui::test]
async fn reset_context_bumps_epoch(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Record the epoch before any reset.
    let epoch_before = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .epoch
    });

    // Fire a plain NewEntry — must NOT bump epoch.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    let epoch_after_entry = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .epoch
    });
    assert_eq!(
        epoch_after_entry, epoch_before,
        "a plain NewEntry must not bump epoch (got {} → {})",
        epoch_before, epoch_after_entry
    );

    // Now call reset_context — epoch must advance by exactly 1.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    let epoch_after_reset = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store
            .read(cx)
            .session(session_id)
            .expect("session")
            .read(cx)
            .epoch
    });
    assert_eq!(
        epoch_after_reset,
        epoch_before + 1,
        "reset_context must bump epoch by exactly 1 (was {}, got {})",
        epoch_before,
        epoch_after_reset
    );
}

/// Phase 5, Task 5.1: `/clear` (`reset_context`) with a non-empty queue must bump
/// `epoch` AND move the queue watermark, because the queued bundle is dropped —
/// a paired mobile holding the stale Queued bubble needs the section re-sent
/// (now empty). Mirrors `reset_context_bumps_epoch` but seeds a pending message.
#[gpui::test]
async fn reset_context_with_queue_bumps_epoch_and_queue_watermark(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    // Queue a follow-up onto a Running session so reset_context has pending
    // bundles to drop (the `had_pending` branch that moves the queue watermark).
    let (epoch_before, queue_seq_before) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let session = store.session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            let blocks = vec![agent_client_protocol::schema::ContentBlock::Text(
                agent_client_protocol::schema::TextContent::new("queued".to_string()),
            )];
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
            let s = session.read(cx);
            assert_eq!(
                s.pending_messages.len(),
                1,
                "one bundle queued before /clear"
            );
            (s.epoch, s.queue_seq)
        })
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let s = session.read(cx);
        assert_eq!(
            s.epoch,
            epoch_before + 1,
            "reset_context must bump epoch by exactly 1"
        );
        assert!(
            s.pending_messages.is_empty(),
            "reset_context drops the queued bundle"
        );
        assert!(
            s.queue_seq > queue_seq_before,
            "reset_context must move the queue watermark when it drops a pending bundle \
             (was {queue_seq_before}, got {})",
            s.queue_seq
        );
    });
}

/// Phase 4 Task 3 (follow-up): `/clear` (reset_context) must not only wipe
/// in-memory entries and bump the in-memory epoch — it must also flush that
/// state to the DB so a cold-load after a /clear doesn't replay stale rows.
///
/// Uses the `reset_context` path (not the persist_all_rows fallback) because
/// the method is public and the test harness supports a live AcpThread session.
/// After the call, `db.load_entries` must be empty and `db.load_epoch` must be
/// strictly greater than the epoch that was written before the reset.
#[gpui::test]
async fn transcript_clear_resets_stale_rows_and_bumps_epoch(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    // Append two entries so the DB has rows to clear.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("world".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    // Sanity check: two rows must be persisted before the reset.
    let rows_before = db
        .load_entries(session_id)
        .await
        .expect("load entries before");
    assert_eq!(
        rows_before.len(),
        2,
        "two appends → two persisted rows before clear"
    );

    // Capture the epoch as written to the DB before the reset.
    let epoch_in_db_before = db
        .load_epoch(session_id)
        .await
        .expect("load epoch before")
        .unwrap_or(0);

    // Call reset_context — this clears entries, bumps in-memory epoch, then
    // calls persist_all_rows which deletes all rows and saves the new epoch.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");

    cx.executor().run_until_parked();

    // DB rows must be gone after the clear.
    let rows_after = db
        .load_entries(session_id)
        .await
        .expect("load entries after");
    assert_eq!(
        rows_after.len(),
        0,
        "reset_context must delete all persisted entry rows"
    );

    // DB epoch must have advanced so a cold-load sees the new epoch, not the
    // stale pre-clear one.
    let epoch_in_db_after = db
        .load_epoch(session_id)
        .await
        .expect("load epoch after")
        .unwrap_or(0);
    assert!(
        epoch_in_db_after > epoch_in_db_before,
        "reset_context must persist the bumped epoch to the DB (was {}, got {})",
        epoch_in_db_before,
        epoch_in_db_after
    );
}

/// Finding 2 regression guard: the fresh-entity branch of `resume_session`
/// (taken when the session is NOT already in `self.sessions`) must seed
/// `desired_model`, `desired_effort`, and `cached_models` from the
/// persisted `SolutionSessionMetadata`.
///
/// Before the fix, those three fields were simply never assigned in the
/// fresh-entity block, so a cold-resumed session lost its model selection
/// and the status-row dropdown would reset to the default on the next open.
///
/// Because driving the full `resume_session` code path requires a live
/// agent subprocess (the mock only supports `new_session`, not
/// `load_session`/`resume_session`), this test exercises the narrower
/// invariant directly: it constructs a `SolutionSession` entity using the
/// same pattern as the fixed fresh-entity block and asserts the three
/// fields are propagated from the metadata.
#[gpui::test]
fn resume_session_fresh_entity_copies_model_from_meta(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let session_id = crate::model::SolutionSessionId::new();
    let solution_id = SolutionId("sol-model-test".into());
    let now = Utc::now();

    let meta = crate::model::SolutionSessionMetadata {
        id: session_id,
        solution_id: solution_id.clone(),
        agent_id: SharedString::from("mock-agent"),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-model-test"),
        title: SharedString::from("model-test session"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: Some(12_345),
        context_count: 2,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: Some("claude-opus-4-5".to_string()),
        desired_effort: Some("high".to_string()),
        cached_models: vec![],
        tab_order: None,
    };

    // Build the entity exactly as the fixed fresh-entity branch does.
    let entity = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |_, cx| {
            cx.new(|_| {
                let mut s = SolutionSession::new_idle(
                    meta.id,
                    meta.solution_id.clone(),
                    meta.agent_id.clone(),
                    meta.acp_session_id.clone(),
                );
                s.title = meta.title.clone();
                s.created_at = meta.created_at;
                s.context_count = meta.context_count;
                s.cwd = meta.cwd.clone();
                s.cached_total_tokens = meta.total_tokens;
                s.parent_session_id = meta.parent_session_id;
                s.desired_model = meta.desired_model.clone();
                s.desired_effort = meta.desired_effort.clone();
                s.cached_models = meta.cached_models.clone();
                s
            })
        })
    });

    cx.update(|cx| {
        entity.read_with(cx, |s, _| {
            assert_eq!(
                s.desired_model.as_deref(),
                Some("claude-opus-4-5"),
                "desired_model must be seeded from meta in the fresh-entity branch"
            );
            assert_eq!(
                s.desired_effort.as_deref(),
                Some("high"),
                "desired_effort must be seeded from meta in the fresh-entity branch"
            );
            // cached_models is empty in this fixture — just assert the field exists
            // and is not corrupt.
            assert!(
                s.cached_models.is_empty(),
                "cached_models must round-trip from meta (empty vec expected here)"
            );
        });
    });
}

/// Verify that persisted supervisor states are merged into the store map as
/// soon as `set_persistence` is called — not lazily deferred to per-solution
/// hydration. This is the "restart" path: supervisor toggles written to the
/// DB in a previous run must be visible in the fresh store after init.
#[gpui::test]
async fn supervisor_states_loaded_at_persistence_init(cx: &mut gpui::TestAppContext) {
    // Open a DB and write a supervisor state row directly, simulating work
    // done in a previous session. The in-memory DB is keyed by thread name,
    // so a second `open` call below shares the same data.
    let session_id = crate::model::SolutionSessionId::new();
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let state = crate::supervisor::SupervisorState {
        session_id,
        enabled: true,
        custom_prompt: Some("reload me".into()),
        consecutive_continues: 0,
        backoff_attempt: 0,
        last_fired_at: None,
        next_eligible_ms: None,
        status: crate::supervisor::SupervisorStatus::Watching,
        trigger_count: 0,
        last_user_input_ms: None,
        judge_superseded: false,
        held_by_done: false,
        pending_nudge: None,
        wait_until_ms: None,
        watch_started_ms: None,
    };
    db.save_supervisor_state(state.clone())
        .await
        .expect("save_supervisor_state");

    // Create a fresh store (no prior in-memory state) and attach persistence.
    let registry = Arc::new(AdapterRegistry::new());
    let store: gpui::Entity<SolutionAgentStore> =
        cx.update(|cx| cx.new(|cx| SolutionAgentStore::new_in_app(registry, cx)));

    // Pre-condition: the fresh store has no supervisor state for our session.
    let pre = store.read_with(cx, |s, _| s.supervisor_state(session_id));
    assert!(
        pre.is_none(),
        "fresh store must not have supervisor state yet"
    );

    // Open the same shared in-memory DB and call set_persistence — this
    // must fire the one-time load.
    let db2 = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db again"));
    store.update(cx, |store, cx| {
        store.set_persistence(db2, cx);
    });

    // Pump the executor so the spawned load task runs to completion.
    cx.run_until_parked();

    // Post-condition: the loaded row is now in the map.
    let loaded = store
        .read_with(cx, |s, _| s.supervisor_state(session_id))
        .expect("supervisor state must be present after persistence init");
    assert!(loaded.enabled, "enabled must be restored from DB");
    assert_eq!(
        loaded.custom_prompt.as_deref(),
        Some("reload me"),
        "custom_prompt must be restored from DB"
    );
    assert_eq!(loaded.status, crate::supervisor::SupervisorStatus::Watching);
    // The restart/load path stamps a fresh watch baseline so an inherited idle
    // session doesn't fire a judge the instant the editor reopens.
    assert!(
        loaded.watch_started_ms.is_some(),
        "load must anchor the idle clock to process start"
    );
}

/// Regression for "reopened the editor and the observer fired on a parked
/// session by itself": after a restart the operator resumes each session by
/// hand — the supervisor must NOT auto-resume a session that was already idle
/// before the restart. A restored row carries a `watch_started_ms` baseline;
/// `tick_supervisor` fires only once the session produces genuinely-new
/// activity THIS process (its `last_activity_at` moves past the baseline, e.g.
/// a manual kick starts a turn). Three cases pin the whole gate.
#[gpui::test]
async fn restart_leaves_inherited_idle_until_fresh_activity(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let now_ms = chrono::Utc::now().timestamp_millis();

    // Case A — inherited idle (activity predates the watch baseline): the
    // session was parked before the restart, so it stays untouched even though
    // it is idle and silent well past the threshold.
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
        // Restart/load path: we started watching just now, AFTER the last activity.
        store
            .supervisor_states
            .get_mut(&id)
            .unwrap()
            .watch_started_ms = Some(now_ms);
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "a session parked before the restart must NOT auto-resume on reopen"
    );
    assert!(st.last_fired_at.is_none(), "no judge should have fired");

    // Case B — genuinely-new activity under our watch (a manual kick's turn
    // completed: last_activity is now AFTER the baseline, and silent past the
    // threshold): the normal idle-nudge cycle re-engages and fires.
    store.update(cx, |store, cx| {
        // Baseline sits 5 min back; the fresh activity landed 2 min ago.
        let st = store.supervisor_states.get_mut(&id).unwrap();
        st.watch_started_ms = Some(now_ms - 300_000);
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Judging,
        "once the session has fresh activity under our watch, the cycle re-engages"
    );

    // Case C — a FRESH in-session enable (no baseline) is always eligible: its
    // idle arose under our watch, so immediate-idle semantics are unchanged.
    let (store2, id2, _tmp2) = crate::store::test_support::seed_store_with_session(cx).await;
    store2.update(cx, |store, cx| {
        let session = store.session(id2).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id2, true, cx);
        assert!(
            store
                .supervisor_states
                .get(&id2)
                .unwrap()
                .watch_started_ms
                .is_none()
        );
        store.tick_supervisor(cx);
    });
    let st2 = store2
        .read_with(cx, |store, _| store.supervisor_state(id2))
        .unwrap();
    assert_eq!(
        st2.status,
        crate::supervisor::SupervisorStatus::Judging,
        "a fresh in-session enable fires on idle as before (no baseline gate)"
    );
}

#[gpui::test]
async fn toggle_supervision_persists_and_reads_back(cx: &mut gpui::TestAppContext) {
    let (store, session_id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;

    store.update(cx, |store, cx| {
        store.set_supervision_enabled(session_id, true, cx);
        store.set_supervisor_prompt(session_id, Some("verify FORK.md updated".into()), cx);
    });

    let st = store
        .read_with(cx, |store, _| store.supervisor_state(session_id))
        .unwrap();
    assert!(st.enabled);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);
    assert_eq!(st.custom_prompt.as_deref(), Some("verify FORK.md updated"));

    store.update(cx, |store, cx| {
        store.set_supervision_enabled(session_id, false, cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(session_id))
        .unwrap();
    assert!(!st.enabled);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Disabled);
}

#[gpui::test]
async fn tick_fires_judge_after_threshold(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    // Force the session Idle with last_activity 2 minutes ago.
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
    });

    store.update(cx, |store, cx| store.tick_supervisor(cx));

    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Judging);
    assert!(st.last_fired_at.is_some());

    // Second tick must NOT re-fire (already Judging).
    store.update(cx, |store, cx| {
        if let Some(s) = store.supervisor_states.get(&id) {
            assert_eq!(s.status, crate::supervisor::SupervisorStatus::Judging);
        }
        store.tick_supervisor(cx);
    });
    let st2 = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st2.last_fired_at, st.last_fired_at,
        "must not re-fire while judging"
    );
}

/// Manual user-stop parks the supervisor in `Held`: even an idle, long-silent
/// session never fires a judge while held, and the next human message re-arms it
/// to `Watching`. This is the "I stopped the agent myself; don't let the
/// observer drag it back to work until I say so" guarantee.
#[gpui::test]
async fn manual_stop_holds_supervisor_then_message_rearms(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
        // The user manually stops the agent.
        store.hold_supervisor(id, cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "manual stop parks the supervisor on hold"
    );
    assert!(
        st.enabled,
        "Held keeps supervision enabled (it's a pause, not a disable)"
    );

    // A tick while Held must NOT spawn a judge, despite the idle, silent session.
    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "a held supervisor never fires on the current dialog state"
    );

    // The user sends a new message → re-arm to Watching.
    store.update(cx, |store, cx| {
        store.reset_supervisor_continue_counter(id, cx)
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "the next human message re-arms the held supervisor"
    );
}

/// Live typing defers the watchdog: a fresh keystroke (`note_user_input`) makes
/// the supervisor treat the session as active, so an otherwise-fireable idle
/// session does NOT get a judge until the typing grace elapses. Prevents the
/// observer firing its own nudge while the user is mid-message.
#[gpui::test]
async fn typing_defers_supervisor_tick(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
        // The user is mid-message: a keystroke just landed.
        store.note_user_input(id);
    });

    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "a recent keystroke defers the supervisor tick — no judge mid-typing"
    );

    // Clear the typing marker (grace window elapsed) → the tick now fires.
    store.update(cx, |store, _| {
        if let Some(s) = store.supervisor_states.get_mut(&id) {
            s.last_user_input_ms = None;
        }
    });
    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Judging,
        "with no recent typing the idle, silent session fires a judge"
    );
}

/// Hold-on-typing: a judge that fired while the user was idle but FINISHED
/// after the user started composing must NOT drop its nudge into the middle of
/// the user's message. The verdict is accepted (the continue-counter bumps) but
/// its nudge is parked in `pending_nudge`; once the user goes quiet for the idle
/// window, `tick_supervisor` flushes it (without firing a fresh judge).
#[gpui::test]
async fn observer_nudge_held_while_typing_then_flushed(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
        // Simulate a judge that already fired and is mid-review.
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.status = SupervisorStatus::Judging;
            st.last_fired_at = Some(now);
        }
        store.judge_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                _task: Task::ready(()),
            },
        );
        // The user starts typing WHILE the judge is running.
        store.note_user_input(id);
        // The judge delivers a Continue verdict — but the user is mid-message.
        store.apply_verdict(
            id,
            VerdictAction::Continue,
            "keep going".into(),
            Some("Continue please.".into()),
            None,
            None,
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.consecutive_continues, 1,
        "the verdict is accepted (counter bumps) even though its nudge is held"
    );
    assert_eq!(
        st.pending_nudge.as_deref(),
        Some("Continue please."),
        "the nudge is parked, not delivered, while the user is typing"
    );

    // The user changes their mind and goes quiet past the idle window.
    store.update(cx, |store, cx| {
        let quiet = chrono::Utc::now().timestamp_millis()
            - (crate::supervisor::IDLE_THRESHOLD_SECS as i64 + 5) * 1000;
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.last_user_input_ms = Some(quiet);
        }
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.pending_nudge, None,
        "once the user is quiet for the idle window, the held nudge flushes"
    );
    assert_eq!(
        st.status,
        SupervisorStatus::Watching,
        "flushing a held nudge must not fire a fresh judge"
    );
}

/// Audit / interrupt: turning supervision OFF while a judge is mid-review tears
/// the judge down immediately, discards any held nudge, and a verdict that still
/// races out of the torn-down judge is dropped by the send-time gate (no nudge,
/// no counter bump). Previously disabling did NOT stop a running observer.
#[gpui::test]
async fn disabling_supervision_interrupts_running_judge(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.status = SupervisorStatus::Judging;
            st.last_fired_at = Some(now);
            st.pending_nudge = Some("held".into());
        }
        store.judge_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                _task: Task::ready(()),
            },
        );
        store.set_supervision_enabled(id, false, cx);
    });
    store.read_with(cx, |store, _| {
        assert!(
            !store.judge_sessions.contains_key(&id),
            "disabling supervision must tear down the in-flight judge"
        );
        let st = store.supervisor_state(id).unwrap();
        assert!(matches!(st.status, SupervisorStatus::Disabled));
        assert_eq!(st.pending_nudge, None, "disabling discards a held nudge");
    });
    // A verdict racing out of the torn-down judge must be dropped.
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            VerdictAction::Continue,
            "keep going".into(),
            Some("Continue.".into()),
            None,
            None,
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.consecutive_continues, 0,
        "a verdict from a disabled supervisor is dropped (no nudge, no counter bump)"
    );
}

/// Audit: a verdict that arrives after the user hit Stop (supervision parked in
/// `Held`) is dropped by the send-time gate. `hold_supervisor` already tore the
/// judge down; this proves the racing-verdict backstop (previously a verdict
/// racing in after a manual Stop still nudged the agent).
#[gpui::test]
async fn held_supervisor_drops_racing_verdict(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.status = SupervisorStatus::Judging;
            st.last_fired_at = Some(now);
        }
        store.judge_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                _task: Task::ready(()),
            },
        );
        store.hold_supervisor(id, cx);
    });
    store.read_with(cx, |store, _| {
        assert!(!store.judge_sessions.contains_key(&id));
        assert!(matches!(
            store.supervisor_state(id).unwrap().status,
            SupervisorStatus::Held
        ));
    });
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            VerdictAction::Continue,
            "keep going".into(),
            Some("Continue.".into()),
            None,
            None,
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.consecutive_continues, 0,
        "a verdict arriving after a manual Stop (Held) is dropped"
    );
}

/// The user sending their own message forgets a nudge that was parked waiting
/// for them to stop typing. The clear happens in the single user-send funnel
/// unconditionally, because the judge is already gone once a nudge is held.
#[gpui::test]
async fn user_send_discards_held_nudge(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.pending_nudge = Some("stale observer nudge".into());
        }
    });
    // The user sends their own reply through the single funnel (from_user).
    store.update(cx, |store, cx| {
        let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
            "my own reply".to_string(),
        ))];
        store
            .send_message_blocks_targeted(id, blocks, crate::model::QueueTarget::Main, true, cx)
            .detach();
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.pending_nudge, None,
        "the user's own message forgets the held observer nudge"
    );
}

/// Interrupt: changing the supervisor's instruction while a judge is mid-review
/// tears that judge down (its verdict is stale — it reviewed under the old
/// instruction) and returns to `Watching` so the next tick re-fires under the
/// new instruction. A verdict racing out of the old judge is then dropped.
#[gpui::test]
async fn changing_instruction_interrupts_running_judge(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(st) = store.supervisor_states.get_mut(&id) {
            st.status = SupervisorStatus::Judging;
            st.last_fired_at = Some(now);
        }
        store.judge_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                _task: Task::ready(()),
            },
        );
        store.set_supervisor_prompt(id, Some("review only the tests".into()), cx);
    });
    store.read_with(cx, |store, _| {
        assert!(
            !store.judge_sessions.contains_key(&id),
            "changing the instruction tears down the stale in-flight judge"
        );
        assert!(matches!(
            store.supervisor_state(id).unwrap().status,
            SupervisorStatus::Watching
        ));
    });
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            VerdictAction::Continue,
            "keep going".into(),
            Some("Continue.".into()),
            None,
            None,
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.consecutive_continues, 0,
        "a verdict from a judge interrupted by an instruction change is dropped"
    );
}

/// A session sitting idle OVER a running background command must not wake the
/// supervisor — the agent is legitimately waiting on that work, and hung
/// background commands are watched elsewhere. Once the shell exits, the idle
/// session fires normally.
#[gpui::test]
async fn background_command_suppresses_supervisor_tick(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::SupervisorStatus;
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
            let shell_id = crate::background_shell::BackgroundShellId::new("shell-1");
            s.background_shells.insert(
                shell_id.clone(),
                crate::background_shell::BackgroundShell {
                    id: shell_id,
                    command: "sleep 100".into(),
                    output_path: std::path::PathBuf::from("/tmp/x.output"),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    state: crate::background_shell::ShellRuntimeState::Running,
                },
            );
        });
        store.set_supervision_enabled(id, true, cx);
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        SupervisorStatus::Watching,
        "a running background command keeps the supervisor quiet"
    );

    // The shell finishes → the idle session now fires.
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            for shell in s.background_shells.values_mut() {
                shell.state = crate::background_shell::ShellRuntimeState::Exited(Some(0));
            }
        });
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        SupervisorStatus::Judging,
        "once the background command exits, the idle session fires a judge"
    );
}

/// A `wait` verdict is one-shot: the mechanism honors the full timeout without
/// re-judging (no fresh judge even though the session is silent past the idle
/// threshold), then wakes the agent itself when the deadline elapses.
#[gpui::test]
async fn wait_is_one_shot_no_rejudge_until_deadline(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::{SupervisorStatus, VerdictAction};
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(600);
        });
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status = SupervisorStatus::Judging;
        store.apply_verdict(
            id,
            VerdictAction::Wait,
            "waiting on the background build".into(),
            None,
            None,
            None,
            Some(300),
            cx,
        );
    });
    // Parked with a future deadline: a tick must NOT fire a fresh judge despite
    // the session being silent for 10 minutes.
    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        SupervisorStatus::Watching,
        "a parked wait must not re-fire a judge"
    );
    assert!(
        st.wait_until_ms.is_some(),
        "the wait deadline is still pending"
    );

    // Force the deadline into the past → the mechanism wakes the agent itself
    // and clears the wait; it does NOT spawn a judge (status stays Watching).
    store.update(cx, |store, cx| {
        store.supervisor_states.get_mut(&id).unwrap().wait_until_ms =
            Some(chrono::Utc::now().timestamp_millis() - 1_000);
        store.tick_supervisor(cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.wait_until_ms, None, "the elapsed wait is cleared");
    assert_eq!(
        st.status,
        SupervisorStatus::Watching,
        "waking the agent at the deadline does not spawn a judge"
    );
}

/// The status-icon firing counter: `trigger_count` increments each time the
/// supervisor fires a judge and resets to 0 on every enable/disable toggle.
#[gpui::test]
async fn trigger_count_increments_on_fire_and_resets_on_toggle(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        });
        store.set_supervision_enabled(id, true, cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.trigger_count, 0,
        "a fresh enable starts the counter at 0"
    );

    // One fire → count 1.
    store.update(cx, |store, cx| store.tick_supervisor(cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.trigger_count, 1, "firing a judge increments the counter");

    // Toggle off then on → counter cleared both times.
    store.update(cx, |store, cx| store.set_supervision_enabled(id, false, cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.trigger_count, 0, "disabling clears the counter");

    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.trigger_count, 0, "re-enabling keeps the counter at 0");
}

#[gpui::test]
async fn tick_sweeps_stuck_auditor(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));

    // Register a STALE auditor handle (spawned past the timeout) and a FRESH
    // one for an unrelated supervised id. The stale handle models an auditor
    // that errored / ended without calling `supervisor_audit_verdict` while the
    // supervised session sat in `Watching` — the judge-stuck path never sees it.
    let fresh_id = SolutionSessionId::new();
    let timeout_ms = (crate::supervisor::AUDITOR_TIMEOUT_SECS as i64) * 1000;
    let now = chrono::Utc::now().timestamp_millis();
    store.update(cx, |store, _| {
        store.auditor_sessions.insert(
            id,
            JudgeHandle {
                judge_id: None,
                started_ms: now - timeout_ms - 1_000,
                _task: Task::ready(()),
            },
        );
        store.auditor_sessions.insert(
            fresh_id,
            JudgeHandle {
                judge_id: None,
                started_ms: now,
                _task: Task::ready(()),
            },
        );
    });

    store.update(cx, |store, cx| store.tick_supervisor(cx));

    store.read_with(cx, |store, _| {
        assert!(
            !store.auditor_sessions.contains_key(&id),
            "stale auditor handle must be swept by the auditor-stuck sweep"
        );
        assert!(
            store.auditor_sessions.contains_key(&fresh_id),
            "a fresh auditor handle must survive the sweep"
        );
    });

    // The sweep is a pure cleanup — it must NOT pause/disable supervision
    // (an auditor failing is not the judge failing).
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert!(st.enabled, "auditor timeout must not disable supervision");
}

#[gpui::test]
async fn apply_continue_nudges_and_increments(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
    });
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Continue,
            "two items left".into(),
            None,
            None,
            Some(500),
            None,
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.consecutive_continues, 1);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);

    // Verdict was recorded to disk.
    let root = crate::store::test_support::session_solution_root(&store, id, cx);
    let dir = crate::supervisor::supervisor_dir(&root, id);
    assert_eq!(crate::supervisor::read_verdicts(&dir).len(), 1);
}

#[gpui::test]
async fn apply_wait_sleeps_without_nudging(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
    });
    let before = chrono::Utc::now().timestamp_millis();
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Wait,
            "agent is waiting on the background build".into(),
            None,
            None,
            None,
            Some(90),
            cx,
        );
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    // Wait must NOT count toward the consecutive-continue guard.
    assert_eq!(st.consecutive_continues, 0);
    // Stays Watching (the one-shot wait handler is gated on it).
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);
    // The one-shot wait commits a single wake deadline ~90s out (slack for
    // scheduling); the mechanism honors it in full without re-judging.
    let wake = st.wait_until_ms.expect("wait sets wait_until_ms");
    assert!(
        wake >= before + 80_000,
        "wait wake ~90s out: {wake} vs {before}"
    );
    assert!(wake <= before + 100_000, "wait wake within clamp: {wake}");

    // The wait verdict was recorded to disk.
    let root = crate::store::test_support::session_solution_root(&store, id, cx);
    let dir = crate::supervisor::supervisor_dir(&root, id);
    assert_eq!(crate::supervisor::read_verdicts(&dir).len(), 1);
}

#[gpui::test]
async fn apply_ask_agent_increments_and_records(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
    });
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::AskAgent,
            "unclear whether tests were actually run".into(),
            None,
            Some("Did you run the full test suite, and did it pass?".into()),
            None,
            None,
            cx,
        );
    });
    // ask_agent behaves like continue for the guard: counts toward the cap and
    // returns the session to Watching (it sent the question to the agent).
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.consecutive_continues, 1);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);

    // The verdict was logged with action == AskAgent.
    let root = crate::store::test_support::session_solution_root(&store, id, cx);
    let dir = crate::supervisor::supervisor_dir(&root, id);
    let recs = crate::supervisor::read_verdicts(&dir);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].action,
        Some(crate::supervisor::VerdictAction::AskAgent)
    );
}

#[gpui::test]
async fn fifteen_continues_force_ask(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));

    for _ in 0..15 {
        store.update(cx, |store, cx| {
            // A judge only ever fires while the session is idle (`should_fire`),
            // and `apply_verdict` now re-checks that at delivery. A real
            // continue cycle returns to idle between nudges; this test fires
            // them back-to-back with no live turn, so re-assert the idle
            // premise each round (the previous nudge left the session Running).
            if let Some(s) = store.session(id) {
                s.update(cx, |s, _| s.state = crate::model::SessionState::Idle);
            }
            store.supervisor_states.get_mut(&id).unwrap().status =
                crate::supervisor::SupervisorStatus::Judging;
            store.apply_verdict(
                id,
                crate::supervisor::VerdictAction::Continue,
                "still going".into(),
                None,
                None,
                None,
                None,
                cx,
            );
        });
    }

    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::WaitingUser);

    // User replies → counter resets, back to watching.
    store.update(cx, |store, cx| {
        store.reset_supervisor_continue_counter(id, cx)
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.consecutive_continues, 0);
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);
}

/// A session parked by the supervisor that resumes ON ITS OWN (the agent's
/// self-scheduled monitor fires fresh activity, not a human message) must
/// re-arm to `Watching` so the status stops hanging. This covers the two
/// supervisor-parked states — `WaitingUser` (after `ask`) and `Held` with
/// `held_by_done` (after `done`) — but a `Held` from a MANUAL user stop
/// (`held_by_done == false`) must NOT re-arm on self-activity.
#[gpui::test]
async fn self_resume_rearms_parked_supervisor(cx: &mut gpui::TestAppContext) {
    use crate::supervisor::SupervisorStatus;
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));

    // WaitingUser (ask escalation) → self-resume re-arms to Watching.
    store.update(cx, |store, cx| {
        store.supervisor_states.get_mut(&id).unwrap().status = SupervisorStatus::WaitingUser;
        store.rearm_supervisor_on_self_activity(id, cx);
    });
    assert_eq!(
        store
            .read_with(cx, |store, _| store.supervisor_state(id))
            .unwrap()
            .status,
        SupervisorStatus::Watching,
    );

    // Held from a `done` verdict (held_by_done) → self-resume re-arms to Watching
    // and clears the flag.
    store.update(cx, |store, cx| {
        {
            let st = store.supervisor_states.get_mut(&id).unwrap();
            st.status = SupervisorStatus::Held;
            st.held_by_done = true;
        }
        store.rearm_supervisor_on_self_activity(id, cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.status, SupervisorStatus::Watching);
    assert!(!st.held_by_done);

    // Held from a MANUAL user stop (held_by_done == false) → self-resume must NOT
    // re-arm; the session stays On hold until the user sends a message.
    store.update(cx, |store, cx| {
        {
            let st = store.supervisor_states.get_mut(&id).unwrap();
            st.status = SupervisorStatus::Held;
            st.held_by_done = false;
        }
        store.rearm_supervisor_on_self_activity(id, cx);
    });
    assert_eq!(
        store
            .read_with(cx, |store, _| store.supervisor_state(id))
            .unwrap()
            .status,
        SupervisorStatus::Held,
    );
}

/// Send-time session-state re-check: a judge fires only while the session is
/// idle, but its turn runs seconds→minutes and the agent can resume ON ITS
/// OWN in the meantime (a `Bash(run_in_background)` continuation lands as an
/// orphan result and flips the session back to `Running`). A `Continue`
/// verdict delivered against a now-`Running` session must be DROPPED — not
/// nudged/queued behind the live turn (the reported "supervisor reacted while
/// the agent was still alive and the message got queued").
#[gpui::test]
async fn continue_verdict_dropped_when_agent_already_running(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));

    store.update(cx, |store, cx| {
        // The judge fired while idle; by delivery the agent has resumed.
        if let Some(s) = store.session(id) {
            s.update(cx, |s, _| {
                s.state = crate::model::SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
        }
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Continue,
            "keep going".into(),
            None,
            None,
            None,
            None,
            cx,
        );
    });

    // The verdict is dropped: the continue counter never advanced (the drop
    // returns before the Continue arm), and no nudge was queued behind the
    // live turn.
    let st = store.read_with(cx, |store, _| store.supervisor_state(id)).unwrap();
    assert_eq!(
        st.consecutive_continues, 0,
        "a verdict delivered while the agent is Running must be dropped, not acted on"
    );
    let queued = store.read_with(cx, |store, cx| {
        store.session(id).unwrap().read(cx).pending_messages.len()
    });
    assert_eq!(queued, 0, "no spurious supervisor nudge queued behind the live turn");
    // And the drop must return the supervisor to `Watching` — NOT leave it
    // pinned in `Judging` with no live judge, which the stuck-watchdog would
    // later mistake for a crashed judge and charge a bogus backoff (audit #1).
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "a dropped verdict must un-pin the status from Judging back to Watching"
    );
}

/// Audit #3: a nudge parked for the "user stopped typing" flush must be DROPPED
/// (not delivered) if the session moved to a paused state (`Held` etc.) after
/// it was parked — otherwise a held nudge drags the agent back to work the user
/// explicitly stopped. Mirrors the wait-wake path's `Watching` gate.
#[gpui::test]
async fn pending_nudge_dropped_when_paused_before_flush(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        let session = store.session(id).unwrap();
        session.update(cx, |s, _| {
            s.state = crate::model::SessionState::Idle;
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(300);
        });
        // A nudge was held (user was typing when the judge finished), then the
        // user hit Stop → Held. `hold_supervisor` doesn't clear the parked nudge.
        let st = store.supervisor_states.get_mut(&id).unwrap();
        st.pending_nudge = Some("continue where you left off".into());
        st.status = crate::supervisor::SupervisorStatus::Held;
        store.tick_supervisor(cx);
    });

    let st = store.read_with(cx, |store, _| store.supervisor_state(id)).unwrap();
    assert!(
        st.pending_nudge.is_none(),
        "a parked nudge must be dropped once the session is paused (Held), not delivered"
    );
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "the pause state is preserved — the observer stays Held"
    );
    let queued = store.read_with(cx, |store, cx| {
        store.session(id).unwrap().read(cx).pending_messages.len()
    });
    assert_eq!(queued, 0, "the stale nudge must not be delivered/queued after a Stop");
}

#[gpui::test]
async fn audit_escalate_pauses_supervision(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.apply_audit_verdict(id, false, true, "supervisor is looping".into(), cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(st.status, crate::supervisor::SupervisorStatus::WaitingUser);

    // An Audit-kind record must be on disk.
    let root = crate::store::test_support::session_solution_root(&store, id, cx);
    let dir = crate::supervisor::supervisor_dir(&root, id);
    let recs = crate::supervisor::read_verdicts(&dir);
    assert!(
        recs.iter()
            .any(|r| matches!(r.kind, crate::supervisor::VerdictKind::Audit)),
        "expected an Audit-kind verdict record"
    );
    assert!(
        recs.iter()
            .any(|r| matches!(r.kind, crate::supervisor::VerdictKind::Audit)
                && r.audit_ok == Some(false)),
        "Audit record must carry audit_ok = Some(false)"
    );
}

#[gpui::test]
fn visible_session_count_excludes_live_auditors(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let sol = SolutionId("sol-a".into());
            let agent = SharedString::from("claude-acp");

            // Two ordinary user sessions + one auditor child session.
            let supervised = SolutionSessionId::new();
            let other = SolutionSessionId::new();
            let auditor = SolutionSessionId::new();
            for id in [supervised, other, auditor] {
                insert_cold_session(id, sol.clone(), agent.clone(), None, None, store, cx);
            }

            assert_eq!(store.visible_session_count(&sol), 3);

            // Register a live AUDITOR handle (separate map from judges); it
            // must be excluded from the badge count exactly like a judge.
            store.auditor_sessions.insert(
                supervised,
                JudgeHandle {
                    judge_id: Some(auditor),
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    _task: Task::ready(()),
                },
            );
            assert_eq!(store.visible_session_count(&sol), 2);
            assert!(store.live_supervisor_session_ids().contains(&auditor));

            // Auditor whose create hasn't resolved (judge_id None) excludes nothing.
            store.auditor_sessions.remove(&supervised);
            store.auditor_sessions.insert(
                supervised,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    _task: Task::ready(()),
                },
            );
            assert_eq!(store.visible_session_count(&sol), 3);
        });
    });
}

#[gpui::test]
async fn escalate_sets_marker_and_waiting(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.escalate_to_user(id, "Which API did you mean?".into(), cx);
    });
    let (status, q) = store.read_with(cx, |store, cx| {
        let st = store.supervisor_state(id).unwrap();
        let q = store
            .session(id)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone();
        (st.status, q)
    });
    assert_eq!(status, crate::supervisor::SupervisorStatus::WaitingUser);
    assert_eq!(q.as_deref(), Some("Which API did you mean?"));
}

/// Regression: a human reply into a supervised session that is paused in
/// `WaitingUser` (after an `ask`) must RESUME supervision (→ `Watching`) and
/// clear the question banner — but ONLY for a genuine user send (`from_user:
/// true`), never for a supervisor nudge (`from_user: false`). The desktop
/// compose row funnels through `send_message_blocks_targeted`, so the resume
/// must live there, not only in the MCP `send_message` path (the bug: a chat
/// reply left supervision stuck `WaitingUser` forever).
#[gpui::test]
async fn user_reply_resumes_waiting_user_supervision(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.escalate_to_user(id, "Restart the stand yourself?".into(), cx);
    });

    let msg = |text: &str| {
        vec![acp::ContentBlock::Text(acp::TextContent::new(
            text.to_string(),
        ))]
    };

    // A supervisor nudge (from_user: false) must NOT resume — still WaitingUser.
    store.update(cx, |store, cx| {
        store
            .send_message_blocks_targeted(
                id,
                msg("supervisor nudge"),
                crate::model::QueueTarget::Main,
                false,
                cx,
            )
            .detach();
    });
    let (status, q) = store.read_with(cx, |store, cx| {
        (
            store.supervisor_state(id).unwrap().status,
            store
                .session(id)
                .unwrap()
                .read(cx)
                .supervisor_question
                .clone(),
        )
    });
    assert_eq!(
        status,
        crate::supervisor::SupervisorStatus::WaitingUser,
        "a non-user send must not resume a WaitingUser pause"
    );
    assert!(q.is_some(), "question stays set after a non-user send");

    // A genuine user reply (from_user: true) resumes supervision + clears banner.
    store.update(cx, |store, cx| {
        store
            .send_message_blocks_targeted(
                id,
                msg("готово, перезапустил"),
                crate::model::QueueTarget::Main,
                true,
                cx,
            )
            .detach();
    });
    let (status, q) = store.read_with(cx, |store, cx| {
        (
            store.supervisor_state(id).unwrap().status,
            store
                .session(id)
                .unwrap()
                .read(cx)
                .supervisor_question
                .clone(),
        )
    });
    assert_eq!(
        status,
        crate::supervisor::SupervisorStatus::Watching,
        "a user reply must resume supervision to Watching"
    );
    assert!(q.is_none(), "user reply must clear the supervisor question");
}

/// A `Done` verdict parks supervision in `Held` (standby) WITHOUT disabling it —
/// the same pause the user's manual Stop uses. When the user then continues the
/// work with a new message, supervision re-arms to `Watching` (the task is
/// evidently not done anymore).
#[gpui::test]
async fn user_reply_rearms_supervision_after_done(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Done,
            "task complete".into(),
            None,
            None,
            None,
            None,
            cx,
        );
    });
    let st = store.read_with(cx, |store, _| store.supervisor_state(id).unwrap());
    assert!(
        st.enabled,
        "Done parks in Held, it does NOT disable supervision"
    );
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "Done parks the supervisor on hold"
    );

    // User continues the work — supervision must re-arm.
    store.update(cx, |store, cx| {
        store
            .send_message_blocks_targeted(
                id,
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "ещё не всё, продолжаем".to_string(),
                ))],
                crate::model::QueueTarget::Main,
                true,
                cx,
            )
            .detach();
    });
    let st = store.read_with(cx, |store, _| store.supervisor_state(id).unwrap());
    assert!(
        st.enabled,
        "a user reply after Done must re-enable supervision"
    );
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "re-armed supervision returns to Watching"
    );
}

/// Regression: a `Compact` verdict re-entered the store. `apply_verdict` runs
/// inside the MCP tool's `store.update(...)` lease, and the Compact arm called
/// `compact::start_compact_for_session`, which re-`read_with`s the global store
/// → `double_lease_panic` ("cannot read SolutionAgentStore while it is already
/// being updated"). Every supervisor "compact" verdict crashed the live editor.
/// The fix defers the compact past the update lease; this test drives the exact
/// path (apply_verdict(Compact) inside `store.update`, then pump deferred work)
/// and must NOT panic.
#[gpui::test]
async fn compact_verdict_does_not_reenter_store(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        // Pre-fix this call panicked synchronously inside the update.
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Compact,
            "context is large; compact".into(),
            None,
            None,
            None,
            None,
            cx,
        );
    });
    // The deferred compact runs here (post-lease). It may decline (no context to
    // compact on the seeded session) — that's fine; the point is no panic.
    cx.executor().run_until_parked();
    let st = store.read_with(cx, |store, _| store.supervisor_state(id).unwrap());
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Watching,
        "a Compact verdict leaves supervision Watching"
    );
}

#[gpui::test]
async fn done_verdict_clears_pending_question(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;

    // Enable supervision and escalate a question — the banner should be set.
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.escalate_to_user(id, "Q?".into(), cx);
    });
    let (status, q) = store.read_with(cx, |store, cx| {
        let st = store.supervisor_state(id).unwrap();
        let q = store
            .session(id)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone();
        (st.status, q)
    });
    assert_eq!(status, crate::supervisor::SupervisorStatus::WaitingUser);
    assert!(q.is_some(), "question must be set after escalate_to_user");

    // A Done verdict fires before the user replies — banner must be cleared.
    store.update(cx, |store, cx| {
        store.apply_verdict(
            id,
            crate::supervisor::VerdictAction::Done,
            "all done".into(),
            None,
            None,
            None,
            None,
            cx,
        );
    });
    let q_after = store.read_with(cx, |store, cx| {
        store
            .session(id)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone()
    });
    assert!(
        q_after.is_none(),
        "supervisor_question must be cleared after Done verdict"
    );

    // Verify the supervision state is parked on hold (Done → Held).
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Held,
        "a Done verdict parks supervision on hold"
    );

    // Also verify that set_supervision_enabled(false) clears a pending question.
    let (store2, id2, _tmp2) = crate::store::test_support::seed_store_with_session(cx).await;
    store2.update(cx, |store, cx| {
        store.set_supervision_enabled(id2, true, cx);
        store.escalate_to_user(id2, "still waiting?".into(), cx);
    });
    let q_before_disable = store2.read_with(cx, |store, cx| {
        store
            .session(id2)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone()
    });
    assert!(
        q_before_disable.is_some(),
        "question must be set before disable"
    );

    store2.update(cx, |store, cx| {
        store.set_supervision_enabled(id2, false, cx);
    });
    let q_after_disable = store2.read_with(cx, |store, cx| {
        store
            .session(id2)
            .unwrap()
            .read(cx)
            .supervisor_question
            .clone()
    });
    assert!(
        q_after_disable.is_none(),
        "supervisor_question must be cleared when supervision is disabled"
    );
}

#[gpui::test]
async fn quota_error_stops_immediately(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| {
        store.set_supervision_enabled(id, true, cx);
        store.supervisor_states.get_mut(&id).unwrap().status =
            crate::supervisor::SupervisorStatus::Judging;
        store.on_judge_failed(id, "Error: usage limit reached".into(), cx);
    });
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert!(!st.enabled);
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Stopped(crate::supervisor::StoppedReason::Quota)
    );
    // Quota never schedules a retry.
    assert_eq!(st.next_eligible_ms, None);
}

#[gpui::test]
async fn transient_error_advances_backoff_then_gives_up(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    store.update(cx, |store, cx| store.set_supervision_enabled(id, true, cx));
    // The spec requires all 8 delays to be waited: 1,1,2,3,5,10,30,60 minutes.
    // Failures 1..=8 must each leave status Watching with a retry gate; the 9th
    // failure exhausts the schedule and gives up (Stopped(ProviderError)).
    let mut prev_next_eligible: Option<i64> = None;
    for expected in 1..=9u32 {
        store.update(cx, |store, cx| {
            store.supervisor_states.get_mut(&id).unwrap().status =
                crate::supervisor::SupervisorStatus::Judging;
            store.on_judge_failed(id, "overloaded_error".into(), cx);
        });
        let st = store
            .read_with(cx, |store, _| store.supervisor_state(id))
            .unwrap();
        if expected <= 8 {
            assert_eq!(st.backoff_attempt, expected);
            assert_eq!(st.status, crate::supervisor::SupervisorStatus::Watching);
            // Each transient retry arms an eligibility gate in the future.
            let next = st.next_eligible_ms;
            assert!(
                next.is_some(),
                "attempt {expected} must schedule a retry window"
            );
            // Each successive delay must be >= the previous one (schedule is non-decreasing).
            if let (Some(prev), Some(cur)) = (prev_next_eligible, next) {
                assert!(
                    cur >= prev,
                    "attempt {expected}: next_eligible_ms should not decrease"
                );
            }
            // The 8th failure must schedule the 60-minute delay (last schedule entry).
            if expected == 8 {
                // 60 min = 3_600_000 ms; prev entry was 30 min = 1_800_000 ms.
                // Verify the 8th delay is strictly larger than the 7th.
                if let (Some(prev), Some(cur)) = (prev_next_eligible, next) {
                    assert!(
                        cur > prev,
                        "attempt 8 (60-min delay) must set a later gate than attempt 7 (30-min)"
                    );
                }
            }
            prev_next_eligible = next;
        }
    }
    // After the 9th transient failure the schedule is exhausted.
    let st = store
        .read_with(cx, |store, _| store.supervisor_state(id))
        .unwrap();
    assert!(!st.enabled);
    assert_eq!(
        st.status,
        crate::supervisor::SupervisorStatus::Stopped(
            crate::supervisor::StoppedReason::ProviderError
        )
    );
    // Giving up must clear the gate (no dangling retry).
    assert_eq!(st.next_eligible_ms, None);
}

/// Helper: does the session's live thread contain a UserMessage carrying the
/// reconnect continuation prompt?
fn thread_has_continuation(
    store: &SolutionAgentStore,
    session_id: SolutionSessionId,
    cx: &gpui::App,
) -> bool {
    let Some(thread) = store
        .session(session_id)
        .and_then(|s| s.read(cx).acp_thread().cloned())
    else {
        return false;
    };
    thread.read(cx).entries().iter().any(|e| {
        if let acp_thread::AgentThreadEntry::UserMessage(msg) = e {
            let text: String = msg
                .chunks
                .iter()
                .filter_map(|b| match b {
                    acp::ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            text.contains("продолжай работу с того места")
        } else {
            false
        }
    })
}

/// Helper: does any UserMessage in the session's live thread contain `needle`?
fn last_user_text_contains(
    store: &SolutionAgentStore,
    session_id: SolutionSessionId,
    needle: &str,
    cx: &gpui::App,
) -> bool {
    let Some(thread) = store
        .session(session_id)
        .and_then(|s| s.read(cx).acp_thread().cloned())
    else {
        return false;
    };
    thread.read(cx).entries().iter().any(|e| {
        if let acp_thread::AgentThreadEntry::UserMessage(msg) = e {
            let text: String = msg
                .chunks
                .iter()
                .filter_map(|b| match b {
                    acp::ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            text.contains(needle)
        } else {
            false
        }
    })
}

#[gpui::test]
async fn reconnect_continues_a_wedged_running_session(cx: &mut TestAppContext) {
    // #6: the watchdog only reconnects sessions wedged mid-turn, so once the
    // session is back the agent must be re-engaged with a continuation prompt
    // rather than parking at Idle. (Driven through the post-resume hook
    // directly: the mock backend can't load/resume a session, so we exercise
    // `maybe_send_reconnect_continuation` — the real send + gate — against a
    // live thread, the state the genuine resume path lands in.)
    //
    // COVERAGE BOUNDARY: this verifies the gate (`was_running` -> send) and the
    // actual prompt send, NOT the `reconnect_agent` call site or its
    // `was_running` capture — those need a resume-capable backend the mock
    // lacks. If `reconnect_agent` were changed to always pass `was_running:
    // false`, this test would still pass; that wiring is currently unguarded by
    // a test (a MockConnection load/resume impl would close the gap).
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.maybe_send_reconnect_continuation(
                session_id,
                /* was_running */ true,
                /* tail_unanswered_user */ false,
                cx,
            )
        });
    });
    cx.executor().run_until_parked();

    let continued = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            thread_has_continuation(store, session_id, cx)
        })
    });
    assert!(
        continued,
        "a continuation prompt must be sent after reconnecting a wedged (Running) session",
    );
}

#[gpui::test]
async fn reconnect_idle_session_sends_no_continuation(cx: &mut TestAppContext) {
    // The gate: a reconnect of an already-idle session (e.g. a manual MCP
    // reconnect) must NOT inject a spurious "carry on" prompt — there was no
    // in-flight work to continue.
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.maybe_send_reconnect_continuation(
                session_id,
                /* was_running */ false,
                /* tail_unanswered_user */ false,
                cx,
            )
        });
    });
    cx.executor().run_until_parked();

    let continued = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            thread_has_continuation(store, session_id, cx)
        })
    });
    assert!(
        !continued,
        "no continuation prompt for a reconnect of an already-idle session",
    );
}

/// The regression this session shipped: a wedge that happened on an UNANSWERED
/// user message must re-engage the fresh subprocess with the "you didn't answer
/// the user's last message" prompt, NOT the generic "carry on" — otherwise the
/// replayed human message is treated as already-handled and silently dropped.
#[gpui::test]
async fn reconnect_on_unanswered_user_message_points_at_it(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.maybe_send_reconnect_continuation(
                session_id,
                /* was_running */ true,
                /* tail_unanswered_user */ true,
                cx,
            )
        });
    });
    cx.executor().run_until_parked();

    let (has_unanswered_prompt, has_generic) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, cx| {
            let unanswered = last_user_text_contains(store, session_id, "НЕ считай его уже", cx);
            let generic = last_user_text_contains(store, session_id, "продолжай работу с того места", cx);
            (unanswered, generic)
        })
    });
    assert!(
        has_unanswered_prompt,
        "an unanswered-user wedge must send the 'you didn't answer the user' continuation",
    );
    assert!(
        !has_generic,
        "the generic 'carry on' prompt must NOT be used when the tail is an unanswered user message",
    );
}

#[test]
fn classify_done_reasoning_park_vs_completion() {
    use super::classify_done_reasoning;
    // Genuine completion: not a park, body unchanged.
    assert_eq!(
        classify_done_reasoning("Shipped everything, all tests green."),
        (false, "Shipped everything, all tests green."),
    );
    // Park: flagged, and the `PARK:` token is stripped from the body.
    assert_eq!(
        classify_done_reasoning("PARK: blocked on the operator's branch decision"),
        (true, "blocked on the operator's branch decision"),
    );
    // Leading whitespace before the token is tolerated.
    assert_eq!(
        classify_done_reasoning("   PARK: awaiting go-ahead"),
        (true, "awaiting go-ahead"),
    );
    // The token must be a PREFIX — a mid-sentence "PARK" (even with a colon) is
    // a completion, body untouched.
    assert_eq!(
        classify_done_reasoning("Completed the PARK: feature end to end."),
        (false, "Completed the PARK: feature end to end."),
    );
}

#[test]
fn tail_unanswered_user_detection() {
    use crate::session_entry::{
        AssistantChunk, SessionEntry, SessionEntryKind, SystemEntryLevel,
    };
    use agent_client_protocol::schema as acp;
    let ent = |kind| SessionEntry {
        created_ms: 0,
        mod_seq: 0,
        subagent_id: None,
        kind,
    };
    let user = |chunks: Vec<acp::ContentBlock>| SessionEntryKind::UserMessage {
        id: None,
        content_md: "hi".into(),
        chunks,
    };
    let plain_user = || user(vec![]);
    let nudge_user = || {
        user(vec![acp::ContentBlock::Text(
            acp::TextContent::new("nudge").meta(Some(acp_thread::meta_with_observer_nudge())),
        )])
    };
    let system = || SessionEntryKind::System {
        level: SystemEntryLevel::Info,
        text_md: "note".into(),
    };
    let assistant = || SessionEntryKind::AssistantMessage {
        chunks: vec![AssistantChunk::Message("ok".into())],
    };

    use super::tail_is_unanswered_user_message as tail;
    assert!(!tail(&[]), "empty transcript is not an unanswered-user tail");
    assert!(tail(&[ent(plain_user())]), "bare trailing user message");
    assert!(
        tail(&[ent(plain_user()), ent(system())]),
        "a trailing System note is skipped over",
    );
    assert!(
        !tail(&[ent(plain_user()), ent(assistant())]),
        "an assistant reply after the user message = answered",
    );
    assert!(
        !tail(&[ent(nudge_user())]),
        "an observer nudge is the supervisor's voice, not the human's",
    );
    assert!(
        !tail(&[ent(plain_user()), ent(nudge_user())]),
        "a nudge after the human message means the tail is not a bare unanswered human message",
    );
}

/// Arm a usage-limit / backoff resume gate on `session_id` exactly like
/// `on_judge_failed`'s Quota branch: `next_eligible_ms` in the future plus a
/// live `backoff_timers` wake task.
fn arm_resume_gate(store: &mut SolutionAgentStore, session_id: SolutionSessionId) {
    let future_ms = chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000;
    let st = store
        .supervisor_states
        .entry(session_id)
        .or_insert_with(|| crate::supervisor::SupervisorState::new(session_id));
    st.enabled = true;
    st.status = crate::supervisor::SupervisorStatus::Watching;
    st.next_eligible_ms = Some(future_ms);
    store.backoff_timers.insert(session_id, Task::ready(()));
}

#[gpui::test]
async fn successful_turn_clears_pending_usage_limit_resume_gate(cx: &mut TestAppContext) {
    // #7: a usage-limit auto-resume gate (`next_eligible_ms` + its
    // `backoff_timers` wake task) must be cancelled once the worker actually
    // responds — a successful turn (`Stopped`) proves the wall is gone.
    // Otherwise the session stays gated until the stale reset time and the
    // timer fires a redundant judge.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| arm_resume_gate(store, session_id));
    });

    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Stopped(
                acp::StopReason::EndTurn,
            ));
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, _| {
            let st = store.supervisor_states.get(&session_id).expect("state");
            assert_eq!(
                st.next_eligible_ms, None,
                "resume gate must be cleared on a successful turn",
            );
            assert!(
                !store.backoff_timers.contains_key(&session_id),
                "resume wake timer must be removed on a successful turn",
            );
        });
    });
}

#[gpui::test]
async fn rehit_error_keeps_pending_usage_limit_resume_gate(cx: &mut TestAppContext) {
    // #7 (the other half): a NEW user message that re-hits the wall surfaces as
    // `AcpThreadEvent::Error` (not `Stopped`), so the pending resume gate must
    // SURVIVE — we keep waiting for the reset.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| arm_resume_gate(store, session_id));
    });

    cx.update(|cx| {
        acp_thread.update(cx, |_t, cx| {
            cx.emit(acp_thread::AcpThreadEvent::Error);
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.read_with(cx, |store, _| {
            let st = store.supervisor_states.get(&session_id).expect("state");
            assert!(
                st.next_eligible_ms.is_some(),
                "resume gate must survive a re-hit (Error), so we keep waiting",
            );
            assert!(
                store.backoff_timers.contains_key(&session_id),
                "resume wake timer must survive a re-hit (Error)",
            );
        });
    });
}

/// Regression (ghost console tabs): internal one-shot AI helpers
/// (`message_generator::run_ephemeral_task`) go through
/// `create_ephemeral_session`, which must NOT pin the session into the tab
/// strip and must emit NO `SessionCreated` / `TabsChanged { opened }` — its
/// lifetime is so brief that the console panel's async tab-add could lose the
/// race to the synchronous tab-remove and strand an orphaned tab. A normal
/// `create_session` DOES pin and DOES emit; this test asserts the contrast.
#[gpui::test]
async fn ephemeral_session_is_not_pinned_and_emits_no_session_created(
    cx: &mut TestAppContext,
) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");

    let connect_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::new(connect_count.clone())),
            );
        });
    });

    let created = Rc::new(std::cell::RefCell::new(
        Vec::<crate::model::SolutionSessionId>::new(),
    ));
    let opened = Rc::new(std::cell::RefCell::new(
        Vec::<crate::model::SolutionSessionId>::new(),
    ));
    let closed = Rc::new(std::cell::RefCell::new(
        Vec::<crate::model::SolutionSessionId>::new(),
    ));
    let _subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let created = created.clone();
        let opened = opened.clone();
        let closed = closed.clone();
        cx.subscribe(&store, move |_store, event, _cx| match event {
            SolutionAgentStoreEvent::SessionCreated { id, .. } => {
                created.borrow_mut().push(*id);
            }
            SolutionAgentStoreEvent::TabsChanged {
                opened: opened_ids, ..
            } => {
                opened.borrow_mut().extend(opened_ids.iter().copied());
            }
            SolutionAgentStoreEvent::SessionClosed(id) => {
                closed.borrow_mut().push(*id);
            }
            _ => {}
        })
    });

    // Normal top-level session: pinned + SessionCreated + TabsChanged{opened}.
    let normal_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session(solution_id.clone(), agent_id.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("create_session");
    cx.executor().run_until_parked();

    // Ephemeral one-shot session: NOT pinned, no SessionCreated.
    let ephemeral_id = cx
        .update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_ephemeral_session(
                    solution_id.clone(),
                    agent_id.clone(),
                    project.clone(),
                    cx,
                )
            })
        })
        .await
        .expect("create_ephemeral_session");
    cx.executor().run_until_parked();

    let (normal_tab_order, ephemeral_tab_order) = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let store = store.read(cx);
        let normal = store
            .session(normal_id)
            .expect("normal session exists")
            .read(cx)
            .tab_order;
        let ephemeral = store
            .session(ephemeral_id)
            .expect("ephemeral session exists")
            .read(cx)
            .tab_order;
        (normal, ephemeral)
    });
    assert!(
        normal_tab_order.is_some(),
        "a normal top-level session must be pinned into the tab strip",
    );
    assert_eq!(
        ephemeral_tab_order, None,
        "an ephemeral one-shot session must never be pinned",
    );

    let created = created.borrow().clone();
    let opened = opened.borrow().clone();
    assert_eq!(
        created,
        vec![normal_id],
        "only the normal session may emit SessionCreated",
    );
    assert!(
        !created.contains(&ephemeral_id),
        "ephemeral session must emit no SessionCreated",
    );
    assert!(
        opened.contains(&normal_id),
        "normal session must appear in TabsChanged{{opened}}",
    );
    assert!(
        !opened.contains(&ephemeral_id),
        "ephemeral session must not appear in TabsChanged{{opened}}",
    );

    // Close side: closing a normal session emits `SessionClosed` (which
    // `finalize_session_teardown` also mirrors to `workspace.session_deleted`
    // via the same `was_ephemeral` gate); closing an ephemeral one-shot must
    // emit NOTHING, so a wire client never sees a close for a session it was
    // never told was created. Asserting the store `SessionClosed` event is the
    // testable proxy for the wire suppression — both share the `was_ephemeral`
    // gate, and the `WorkspaceEventCoordinator` isn't installed in this test.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.close_session(normal_id, cx).expect("close normal");
            store
                .close_session(ephemeral_id, cx)
                .expect("close ephemeral");
        });
    });
    cx.executor().run_until_parked();

    let closed = closed.borrow().clone();
    assert_eq!(
        closed,
        vec![normal_id],
        "only the normal session may emit SessionClosed; the ephemeral one is suppressed",
    );
    assert!(
        !closed.contains(&ephemeral_id),
        "ephemeral session must emit no SessionClosed",
    );
}
