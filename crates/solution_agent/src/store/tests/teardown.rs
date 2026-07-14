#![allow(unused_imports)]

use super::common::*;
use crate::adapter::AdapterRegistry;
use crate::model::SessionState;
use crate::store::*;
use crate::test_support::{MockAgentServer, MockConnection};
use chrono::Utc;
use gpui::{Entity, SharedString, TestAppContext};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
                    SolutionId(1),
                    SharedString::from("claude-acp"),
                    agent_client_protocol::schema::SessionId::new("acp-1"),
                );
                s.title = SharedString::from("test");
                s
            });
            store.sessions.insert(id, entity);
            store
                .by_solution
                .entry(SolutionId(1))
                .or_default()
                .push(id);

            assert_eq!(store.sessions_for(&SolutionId(1)).len(), 1);
            store.close_session(id, cx).expect("close_session");
            assert_eq!(store.sessions_for(&SolutionId(1)).len(), 0);
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
            let sol = SolutionId(1);
            let agent = SharedString::from("claude-acp");
            let id = SolutionSessionId::new();
            insert_cold_session(id, sol, agent, None, None, store, cx);

            // Populate every per-session runtime map for `id`.
            store
                .supervisor_states
                .insert(id, crate::supervisor::SupervisorState::new(id));
            store.teammate_watchers.arm_agent_watcher(id, Task::ready(()));
            store.teammate_watchers.arm_shell_watcher(id, Task::ready(()));
            store.backoff_timers.insert(id, Task::ready(()));
            store.teammate_watchers.set_scan_offset(id, 0);
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
                    nonce: String::new(),
                    _task: Task::ready(()),
                },
            );
            store.auditor_sessions.insert(
                id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
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
                !store.teammate_watchers.has_agent_watcher(id),
                "background_agent_watchers leaked"
            );
            assert!(
                !store.teammate_watchers.has_shell_watcher(id),
                "background_shell_watchers leaked"
            );
            assert!(
                !store.backoff_timers.contains_key(&id),
                "backoff_timers leaked"
            );
            assert!(
                !store.teammate_watchers.has_scan_offset(id),
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
    assert!(crate::store::push_and_evict_transcripts(&mut h, "a".into(), 3).is_empty());
    assert!(crate::store::push_and_evict_transcripts(&mut h, "b".into(), 3).is_empty());
    // The third abandoned id evicts the oldest ("a").
    assert_eq!(
        crate::store::push_and_evict_transcripts(&mut h, "c".into(), 3),
        vec!["a".to_string()]
    );
    assert_eq!(
        crate::store::push_and_evict_transcripts(&mut h, "d".into(), 3),
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
        let sol = store.session(id).unwrap().read(cx).solution_id;
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
        solution_id: sol,
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
        member_id: None,
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
        db.list_for_solution(sol)
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
        let sol = store.session(id).unwrap().read(cx).solution_id;
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
        solution_id: sol,
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
        member_id: None,
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
        store.purge_solution_fully(sol, Some(root.clone()), cx)
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
        db.list_for_solution(sol).await.unwrap().is_empty(),
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
        let sol = store.session(id).unwrap().read(cx).solution_id;
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
        solution_id: sol,
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
        member_id: None,
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
            s.test_add_member_with_path(sol, "kept", member_path.clone());
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
                insert_cold_session(sid, sol, agent.clone(), None, None, store, cx);
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
                sol,
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
        store.reap_stale_closed_sessions(sol, cx)
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
            let sol = SolutionId(1);
            let agent = SharedString::from("claude-acp");
            let id = SolutionSessionId::new();
            insert_cold_session(id, sol, agent, None, None, store, cx);

            store
                .supervisor_states
                .insert(id, crate::supervisor::SupervisorState::new(id));
            store.teammate_watchers.arm_agent_watcher(id, Task::ready(()));
            store.teammate_watchers.arm_shell_watcher(id, Task::ready(()));
            store.backoff_timers.insert(id, Task::ready(()));
            store.teammate_watchers.set_scan_offset(id, 0);
            store.judge_sessions.insert(
                id,
                JudgeHandle {
                    judge_id: None,
                    started_ms: chrono::Utc::now().timestamp_millis(),
                    nonce: String::new(),
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
                !store.teammate_watchers.has_agent_watcher(id),
                "background_agent_watchers leaked"
            );
            assert!(
                !store.teammate_watchers.has_shell_watcher(id),
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

#[test]
fn stale_archive_dirs_gates_on_count_then_age() {
    let now = Utc::now();
    let root = std::path::Path::new("/sol/root");
    let make = |n: usize, days_ago: i64| crate::model::SolutionSessionMetadata {
        id: crate::model::SolutionSessionId::new(),
        solution_id: SolutionId(10),
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
        member_id: None,
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

/// A rename that physically moves the solution root must NOT let
/// `gc_orphan_members` (fired on the `Changed` the rename emits) hard-purge the
/// solution's open sessions. At rename time the store already points at the new
/// root while every live session still holds its old `cwd`, so without the
/// `PathsMoved` cwd-rewrite each open session is a false orphan and is deleted.
/// Regression for docs/findings/2026-07-14-rename-purges-open-sessions.md.
#[gpui::test]
async fn rename_solution_folder_move_keeps_open_sessions(cx: &mut gpui::TestAppContext) {
    use solutions::SolutionStore;

    let registry = Arc::new(AdapterRegistry::new());
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("solutions.json");
    let solutions_root = dir.path().join("solutions");
    std::fs::create_dir_all(&solutions_root).unwrap();

    let (solution_store, sol, member_path) = cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        let solution_store = SolutionStore::for_test(cfg_path, cx);
        solutions::install_global_for_test(solution_store.clone(), cx);
        let sol = solution_store
            .update(cx, |s, cx| s.create_solution("Sol", solutions_root.clone(), cx))
            .expect("create_solution");
        let root = solution_store.read(cx).solutions()[0].root.clone();
        let member_path = root.join("member");
        solution_store.update(cx, |s, _| {
            s.test_add_member_with_path(sol, "member", member_path.clone());
        });
        (solution_store, sol, member_path)
    });

    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));

    let session_id = SolutionSessionId::new();
    store.update(cx, |store, cx| {
        let session = insert_cold_session(
            session_id,
            sol,
            SharedString::from("claude-acp"),
            None,
            None,
            store,
            cx,
        );
        session.update(cx, |s, _| s.cwd = member_path.join("sub"));
    });
    cx.run_until_parked();

    // Rename the solution — the folder slug changes, so the root is physically
    // moved and `PathsMoved` + `Changed` fire.
    solution_store
        .update(cx, |s, cx| s.rename_solution(sol, "Renamed", cx))
        .expect("rename_solution");
    cx.run_until_parked();

    let new_root = solution_store.read_with(cx, |s, _| s.solutions()[0].root.clone());
    store.update(cx, |store, cx| {
        let session = store.session(session_id);
        assert!(
            session.is_some(),
            "the open session must survive a folder-moving rename"
        );
        let cwd = session.unwrap().read(cx).cwd.clone();
        assert_eq!(
            cwd,
            new_root.join("member").join("sub"),
            "the session cwd must be rewritten to the new root"
        );
    });
}

/// The same protection for a **member** rename: `rename_member` physically moves
/// the member's subfolder and emits `PathsMoved` for that subtree, so sessions
/// whose cwd sits under the renamed member survive instead of being purged as
/// false orphans.
#[gpui::test]
async fn rename_member_folder_move_keeps_open_sessions(cx: &mut gpui::TestAppContext) {
    use solutions::SolutionStore;

    let registry = Arc::new(AdapterRegistry::new());
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("solutions.json");
    let solutions_root = dir.path().join("solutions");
    std::fs::create_dir_all(&solutions_root).unwrap();

    let (solution_store, sol, member_id, member_path) = cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        let solution_store = SolutionStore::for_test(cfg_path, cx);
        solutions::install_global_for_test(solution_store.clone(), cx);
        let sol = solution_store
            .update(cx, |s, cx| s.create_solution("Sol", solutions_root.clone(), cx))
            .expect("create_solution");
        let root = solution_store.read(cx).solutions()[0].root.clone();
        let member_path = root.join("member");
        // The member subdir must exist on disk — `rename_member` does a real
        // `rename(2)` of it (unlike `rename_solution`, which moves the root).
        std::fs::create_dir_all(&member_path).unwrap();
        let member_id = solution_store.update(cx, |s, _| {
            s.test_add_member_with_path(sol, "member", member_path.clone())
        });
        (solution_store, sol, member_id, member_path)
    });

    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));

    let session_id = SolutionSessionId::new();
    store.update(cx, |store, cx| {
        let session = insert_cold_session(
            session_id,
            sol,
            SharedString::from("claude-acp"),
            None,
            None,
            store,
            cx,
        );
        session.update(cx, |s, _| s.cwd = member_path.join("sub"));
    });
    cx.run_until_parked();

    solution_store
        .update(cx, |s, cx| s.rename_member(member_id, "renamed-member", cx))
        .expect("rename_member");
    cx.run_until_parked();

    let new_member_path = solution_store.read_with(cx, |s, _| {
        s.solutions()[0].members[0].local_path.clone()
    });
    store.update(cx, |store, cx| {
        let session = store.session(session_id);
        assert!(
            session.is_some(),
            "the open session must survive a member folder rename"
        );
        assert_eq!(
            session.unwrap().read(cx).cwd.clone(),
            new_member_path.join("sub"),
            "the session cwd must be rewritten to the new member path"
        );
    });
}

/// The cold (un-hydrated) half of the `PathsMoved` fix: sessions not currently
/// in memory get their persisted `cwd` rewritten in the DB, so a same-process
/// solution reopen re-hydrates a valid path instead of a stale one that the gc
/// would purge. Guards the SQLite `solution_id` bind (TEXT column vs numeric id).
#[gpui::test]
async fn rewrite_session_cwds_rewrites_cold_db_rows(cx: &mut TestAppContext) {
    let (store, seeded, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let (sol, db) = store.read_with(cx, |s, cx| {
        (
            s.session(seeded).expect("seeded").read(cx).solution_id,
            s.persistence().expect("persistence"),
        )
    });

    let old_root = PathBuf::from("/old/root");
    let new_root = PathBuf::from("/new/root");

    // Persist a cwd under the old prefix, then evict the session from memory so
    // only the cold DB-rewrite branch can reach it.
    store.update(cx, |store, cx| {
        store
            .session(seeded)
            .unwrap()
            .update(cx, |s, _| s.cwd = old_root.join("member").join("sub"));
        store.persist_session_row(seeded, cx);
    });
    cx.run_until_parked();

    store.update(cx, |store, cx| {
        store.sessions.remove(&seeded);
        store.by_solution.remove(&sol);
        store.rewrite_session_cwds_for_move(sol, &old_root, &new_root, cx);
    });
    cx.run_until_parked();

    let metas = db.list_for_solution(sol).await.expect("list_for_solution");
    let cwd = metas
        .iter()
        .find(|m| m.id == seeded)
        .expect("seeded row present")
        .cwd
        .clone();
    assert_eq!(
        cwd,
        new_root.join("member").join("sub"),
        "the cold session's persisted cwd must be rewritten to the new root"
    );
}
