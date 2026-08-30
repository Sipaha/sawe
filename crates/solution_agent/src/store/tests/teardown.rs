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
            store.by_solution.entry(SolutionId(1)).or_default().push(id);

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
            store
                .teammate_watchers
                .arm_agent_watcher(id, Task::ready(()));
            store
                .teammate_watchers
                .arm_shell_watcher(id, Task::ready(()));
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

/// Regression guard for "closing a Solution wiped its AI chat tabs".
///
/// Closing a Solution's window is a COLD close: `cold_close_solution` evicts
/// the sessions from memory and releases the pooled subprocess, but it must
/// leave BOTH restore predicates intact — `closed_at IS NULL`
/// (`select_open_session_ids`) and `tab_order IS NOT NULL`
/// (`select_open_tabs`). The desktop title-bar "Close" used to loop
/// `close_session` (the permanent per-tab archive) instead, which stamped
/// `closed_at` AND cascaded `SessionClosed` -> `ChatProvider` ->
/// `ConsolePanel::persist` -> `persist_tab_order`, NULLing `tab_order` too, so
/// reopening the Solution showed an empty AI tab strip.
#[gpui::test]
async fn cold_close_solution_keeps_sessions_restorable(cx: &mut gpui::TestAppContext) {
    let (store, id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let (db, sol) = store.update(cx, |store, cx| {
        (
            store.persistence().expect("persistence"),
            store.session(id).expect("session").read(cx).solution_id,
        )
    });

    // `apply_tab_orders` UPDATEs an existing row, so the metadata row has to
    // exist before the session can be pinned into the strip.
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
    })
    .await
    .unwrap();
    store.update(cx, |store, cx| store.persist_tab_order(sol, vec![id], cx));
    cx.run_until_parked();
    assert_eq!(
        db.list_open_tabs(sol).await.unwrap(),
        vec![id],
        "precondition: the session must be pinned into the strip before the close"
    );

    store.update(cx, |store, cx| store.cold_close_solution(&sol, cx));
    cx.run_until_parked();

    assert!(
        store.update(cx, |store, _| store.session(id).is_none()),
        "cold close must evict the session from memory (a live entity means the \
         eviction, and with it the pool release, silently stopped running)"
    );
    assert!(
        db.closed_at(id).await.unwrap().is_none(),
        "cold close must NOT stamp closed_at — that is the permanent per-tab \
         archive and it makes select_open_session_ids skip the row on reopen"
    );
    assert_eq!(
        db.list_open_tabs(sol).await.unwrap(),
        vec![id],
        "cold close must NOT clear tab_order — select_open_tabs is what rebuilds \
         the AI tab strip when the Solution is reopened"
    );
    assert!(
        db.list_open_session_ids(sol).await.unwrap().contains(&id),
        "cold-closed session must still be hydratable by hydrate_all_for_solution"
    );
}

/// `cold_close_solution` must not rewrite a cold-hydrated session's persisted
/// entry rows.
///
/// WHAT THIS PROVES, AND SINCE WHEN. The assertion used to hold for two
/// independent reasons and could not tell them apart:
///
///   1. the liveness gate in `cold_close_solution` skips `persist_all_rows` for
///      a session with no `acp_thread`, and
///   2. `persist_all_rows`'s work was parked in an `entries_persist_chain` task
///      that `evict_session_runtime_maps` dropped later in the same synchronous
///      block, so the flush reached disk for NO session, live or cold.
///
/// (2) is gone: the cold close now evicts with `ChainDisposition::Drain`, so a
/// LIVE session's flush does run (see
/// `cold_close_solution_flushes_persist_chain_to_disk`). Only (1) stands between
/// this transcript and a rewrite, which is exactly what the test now pins: drop
/// the gate and it fails, because the failure mode is live — "closing a
/// Solution window truncates the transcript of every chat the editor had merely
/// restored from disk", on legacy pre-6b layouts where teammate-tagged rows
/// demux out of Main so `entries.len() > main_len`.
#[gpui::test]
async fn cold_close_solution_does_not_rewrite_cold_session_rows(cx: &mut gpui::TestAppContext) {
    let (store, seeded_id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let (db, sol) = store.update(cx, |store, cx| {
        (
            store.persistence().expect("persistence"),
            store
                .session(seeded_id)
                .expect("session")
                .read(cx)
                .solution_id,
        )
    });

    let cold_id = SolutionSessionId::new();
    store.update(cx, |store, cx| {
        insert_cold_session(
            cold_id,
            sol,
            SharedString::from("claude-acp"),
            None,
            None,
            store,
            cx,
        );
    });
    // Two rows on disk that the in-memory (cold, stream-less) entity does not
    // mirror — exactly the shape a legacy teammate-tagged transcript hydrates as.
    for idx in 0..2 {
        db.upsert_entry(
            cold_id,
            idx,
            1,
            1_700_000_000_000 + idx,
            None,
            vec![1, 2, 3],
        )
        .await
        .expect("seed row");
    }
    assert_eq!(
        db.load_entries(cold_id).await.expect("load").len(),
        2,
        "precondition: rows on disk"
    );

    store.update(cx, |store, cx| store.cold_close_solution(&sol, cx));
    cx.run_until_parked();

    assert_eq!(
        db.load_entries(cold_id)
            .await
            .expect("load after close")
            .len(),
        2,
        "cold close must not flush a session that was never resumed — \
         persist_all_rows' delete_entries_from(main_len) would truncate it"
    );
}

/// The soft close must FLUSH the session's queued entry-row writes, not cancel
/// them. `close_session` issues `persist_all_rows` and then tears down; the
/// teardown evicts `entries_persist_chain`, and because every chain link moves
/// the previous one into its own future, dropping that map entry used to cancel
/// the whole chain — so closing a chat tab silently discarded the transcript
/// tail. Permanently: `persist_all_rows` advances `persisted_main_seq` before
/// spawning, so no later persist re-picks those rows.
///
/// Three stale rows on disk, a two-entry Main stream in memory: an executed
/// flush rewrites idx 0..1 and `delete_entries_from(2)` trims idx 2. A cancelled
/// one leaves all three stale rows.
#[gpui::test]
async fn close_session_flushes_persist_chain_to_disk(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    for idx in 0..3 {
        db.upsert_entry(id, idx, 0, 1_700_000_000_000 + idx, None, b"stale".to_vec())
            .await
            .expect("seed stale row");
    }

    let message = |n: u64, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: text.into(),
            chunks: vec![],
        },
    };

    store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = vec![message(1, "alpha"), message(2, "bravo")];
            s.rebuild_streams();
            cx.notify();
        });
        // Two links deep, both issued before the executor is pumped: the second
        // link owns the first, so the whole chain has to survive the teardown.
        store.persist_main_stream(id, cx);
        store.close_session(id, cx).expect("close_session");
        assert!(
            store.entries_persist_chain.contains_key(&id),
            "the drained chain must STAY under its key — the map owns it (so it \
             runs at all) and a reopen that re-keys this id has to find it as \
             its `prev`"
        );
    });
    cx.run_until_parked();

    let rows = db.load_entries(id).await.expect("load rows");
    assert_eq!(
        rows.len(),
        2,
        "the flush must reach disk: idx 0..1 rewritten and the stale idx 2 \
         trimmed by delete_entries_from(main_len)"
    );
    let texts: Vec<String> = rows
        .iter()
        .map(
            |row| match crate::session_entry::kind_from_payload(&row.payload).expect("decode") {
                SessionEntryKind::UserMessage { content_md, .. } => content_md,
                other => panic!("unexpected persisted kind: {other:?}"),
            },
        )
        .collect();
    assert_eq!(
        texts,
        vec!["alpha".to_string(), "bravo".to_string()],
        "the persisted rows must carry the in-memory Main stream, not the stale payloads"
    );
}

/// The `close_session` twin of `cold_close_solution_does_not_rewrite_cold_session_rows`:
/// closing a tab the editor merely RESTORED from disk must write nothing.
///
/// Now that the soft close actually drains its chain, an ungated
/// `persist_all_rows` here would be a full rewrite of a transcript the user
/// never touched — and on a legacy pre-6b layout (teammate-tagged rows
/// interleaved into the flat index space, so `entries.len() > main_len`) the
/// rewrite's `delete_entries_from(main_len)` deletes the teammate rows. A cold
/// session cannot have changed since hydration, so the correct write count is
/// zero.
#[gpui::test]
async fn close_session_does_not_rewrite_a_cold_legacy_session(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};

    let (store, seeded_id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let (db, sol) = store.update(cx, |store, cx| {
        (
            store.persistence().expect("persistence"),
            store
                .session(seeded_id)
                .expect("session")
                .read(cx)
                .solution_id,
        )
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
    // LEGACY layout: Main "alpha"@0, teammate "noise"@1, Main "bravo"@2 — the
    // flat mirror is one row longer than the Main stream it demuxes to.
    let legacy = [
        asst(1, None, "alpha"),
        asst(2, Some("T1"), "noise"),
        user(3, "bravo"),
    ];

    let cold_id = SolutionSessionId::new();
    store.update(cx, |store, cx| {
        insert_cold_session(
            cold_id,
            sol,
            SharedString::from("claude-acp"),
            None,
            None,
            store,
            cx,
        );
    });
    for (idx, entry) in legacy.iter().enumerate() {
        db.upsert_entry(
            cold_id,
            idx as i64,
            entry.mod_seq as i64,
            entry.created_ms,
            entry.subagent_id.as_ref().map(|s| s.to_string()),
            entry.to_payload(),
        )
        .await
        .expect("seed legacy row");
    }
    store.update(cx, |store, cx| {
        let session = store.session(cold_id).expect("cold session");
        session.update(cx, |s, cx| {
            s.entries = legacy.to_vec();
            s.hydrate_streams_main_only();
            cx.notify();
        });
        assert_eq!(
            session.read(cx).streams[&crate::stream::StreamId::Main]
                .entries
                .len(),
            2,
            "precondition: Main is shorter than the flat mirror, so a flush would truncate"
        );
    });

    store.update(cx, |store, cx| {
        store.close_session(cold_id, cx).expect("close_session")
    });
    cx.run_until_parked();

    let rows = db.load_entries(cold_id).await.expect("load after close");
    assert_eq!(
        rows.len(),
        3,
        "closing a never-resumed tab must not flush — persist_all_rows' \
         delete_entries_from(main_len) would drop the teammate row"
    );
    assert!(
        rows.iter()
            .any(|row| row.subagent_id.as_deref() == Some("T1")),
        "the legacy teammate-tagged row must survive untouched"
    );
}

/// The anti-resurrection guard for the hard purge. `purge_session_hard` evicts
/// the runtime maps BEFORE it issues `db.purge_session`, and the two are
/// unordered background work over the same connection. If the purge handed the
/// persist chain off (`Drain`) instead of dropping it, a queued link could run
/// after the cascade DELETE and re-insert entry rows for a session that no
/// longer has a `solution_sessions` row — orphans no UI enumerates and no GC
/// reaps, forever. Hence `ChainDisposition::Abandon` on this path.
///
/// SCOPE, MEASURED. The guard is exact only for a chain that is ONE link deep,
/// which is what this test pins. Dropping the map entry does not cancel a
/// deeper chain promptly: every link is already queued on the executor when it
/// is spawned, and only the OUTERMOST link's handle lives in the map — the
/// inner handles live inside their successor's future and are not dropped until
/// that successor's runnable is run. So the innermost links keep running while
/// the cancellation walks inward one runnable at a time. Measured on this
/// fixture: a 2-link chain leaves 1 orphan row after the purge, an 8-link chain
/// leaves 5. That leak predates the disposition split and is not introduced by
/// it (it is the same `.remove()` drop as before); closing it needs the purge
/// DELETE sequenced after the abandoned writes, which is a change to the purge
/// ordering rather than to the disposition.
#[gpui::test]
async fn purge_session_hard_abandons_in_flight_persist_chain(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    let message = |n: u64, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: text.into(),
            chunks: vec![],
        },
    };

    store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = (1..=8).map(|n| message(n, "row")).collect();
            s.rebuild_streams();
            cx.notify();
        });
        // Issued before the executor is pumped, so the flush is genuinely
        // in flight when the purge tears the session down.
        store.persist_all_rows(id, cx);
        store.purge_session_hard(id, None, cx);
    });
    cx.run_until_parked();

    assert!(
        db.load_entries(id).await.expect("load").is_empty(),
        "a hard purge must abandon its queued entry-row writes — a surviving \
         link re-inserts rows for a session that no longer exists"
    );
}

/// `gc_orphan_members` purges only **live** sessions whose `cwd` is no longer
/// under any alive member path (nor the solution root). Sessions under a member
/// dir, or at the solution root, survive — and so does an orphan that was
/// restored from disk and never resumed.
///
/// The cold-orphan case is the regression guard that matters. `gc_orphan_members`
/// hard-purges (six DB tables plus `remove_dir_all(<root>/.agents/<sid>)`), and
/// it purges every orphan in the store on any `Changed`, not just ones under the
/// member that was removed. Once cold hydration started indexing `by_solution`,
/// dropping this gate would put a real user's whole backlog of
/// removed-member transcripts — ~18 of them in the maintainer's database — one
/// solution-open away from irreversible deletion.
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

    // A live `AcpThread` for the one session that is allowed to be purged.
    let fs = fs::FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(root.clone(), serde_json::json!({ ".keep": "" }))
        .await;
    let project = project::Project::test(fs, [root.as_path()], cx).await;
    let connection = Rc::new(MockConnection::new());
    let live_thread = cx
        .update(|cx| {
            use acp_thread::AgentConnection as _;
            Rc::clone(&connection).new_session(
                project.clone(),
                util::path_list::PathList::new(std::slice::from_ref(&root)),
                cx,
            )
        })
        .await
        .expect("new_session");

    let agent = SharedString::from("claude-acp");
    let under_member = SolutionSessionId::new();
    let at_root = SolutionSessionId::new();
    let live_orphan = SolutionSessionId::new();
    let cold_orphan = SolutionSessionId::new();

    store.update(cx, |store, cx| {
        for (sid, cwd) in [
            (under_member, member_path.join("sub")),
            (at_root, root.clone()),
            (live_orphan, root.join("removed-member")),
            (cold_orphan, root.join("also-removed-member")),
        ] {
            let session = insert_cold_session(sid, sol, agent.clone(), None, None, store, cx);
            session.update(cx, |s, _| s.cwd = cwd);
        }
        let live = store.session(live_orphan).expect("live orphan inserted");
        live.update(cx, |s, cx| s.set_acp_thread(Some(live_thread.clone()), cx));
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
            store.session(live_orphan).is_none(),
            "removed-member session with a live thread purged"
        );
        assert!(
            store.session(cold_orphan).is_some(),
            "a removed-member session restored from disk and never resumed must be \
             logged, NOT hard-purged — see gc_orphan_members' doc"
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
        s.session(seeded).expect("seeded").read(cx).solution_id
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

    store.update(cx, |store, cx| store.reap_stale_closed_sessions(sol, cx));
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
            store
                .teammate_watchers
                .arm_agent_watcher(id, Task::ready(()));
            store
                .teammate_watchers
                .arm_shell_watcher(id, Task::ready(()));
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
            .update(cx, |s, cx| {
                s.create_solution("Sol", solutions_root.clone(), cx)
            })
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
            .update(cx, |s, cx| {
                s.create_solution("Sol", solutions_root.clone(), cx)
            })
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

    let new_member_path =
        solution_store.read_with(cx, |s, _| s.solutions()[0].members[0].local_path.clone());
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

/// The `cold_close_solution` twin of `close_session_flushes_persist_chain_to_disk`,
/// and the higher-volume half of the pair: closing a Solution window flushes
/// EVERY live session at once, where a tab close flushes one.
///
/// Without it that half of the disposition split is untested — flipping this
/// path back to `ChainDisposition::Abandon` leaves the whole suite green while
/// silently restoring "closing a Solution window discards every open chat's
/// transcript tail".
///
/// Three stale rows on disk, a two-entry Main stream in memory: an executed
/// flush rewrites idx 0..1 and `delete_entries_from(2)` trims idx 2. A cancelled
/// one leaves all three stale rows.
#[gpui::test]
async fn cold_close_solution_flushes_persist_chain_to_disk(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    for idx in 0..3 {
        db.upsert_entry(id, idx, 0, 1_700_000_000_000 + idx, None, b"stale".to_vec())
            .await
            .expect("seed stale row");
    }

    let message = |n: u64, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: text.into(),
            chunks: vec![],
        },
    };

    store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = vec![message(1, "alpha"), message(2, "bravo")];
            s.rebuild_streams();
            cx.notify();
        });
        // Two links deep, both issued before the executor is pumped: the second
        // link owns the first, so the whole chain has to survive the close.
        store.persist_main_stream(id, cx);
        let solution_id = session.read(cx).solution_id;
        store.cold_close_solution(&solution_id, cx);
    });
    cx.run_until_parked();

    let texts: Vec<String> = db
        .load_entries(id)
        .await
        .expect("load rows")
        .iter()
        .map(
            |row| match crate::session_entry::kind_from_payload(&row.payload).expect("decode") {
                SessionEntryKind::UserMessage { content_md, .. } => content_md,
                other => panic!("unexpected persisted kind: {other:?}"),
            },
        )
        .collect();
    assert_eq!(
        texts,
        vec!["alpha".to_string(), "bravo".to_string()],
        "the cold close's flush must reach disk: idx 0..1 rewritten from the \
         in-memory Main stream and the stale idx 2 trimmed by \
         delete_entries_from(main_len)"
    );
    assert!(
        store.update(cx, |store, _| store.session(id).is_none()),
        "precondition: the cold close really did evict the session"
    );
}

/// Close→reopen must not produce two chains racing each other.
///
/// A drained chain is still running when the tab is reopened, and reopening
/// re-keys the SAME `SolutionSessionId` (`hydrate_all_for_solution` restores it
/// under its persisted id). If the close had freed the key — which is what
/// handing the chain off with `.detach()` does — the reopened session's first
/// persist would find no `prev` and build a second, independent chain. The two
/// are then unordered, and the close flush's trailing
/// `delete_entries_from(old_main_len)` can land AFTER the new chain's upsert and
/// delete the message the user just typed: the phase-6b keystone bug, back at
/// the close/reopen seam.
///
/// Three rows on disk and a three-entry transcript, closed and reopened with one
/// new message appended. Ordered: four rows. Unordered: three, with the new
/// message silently absent from disk and gone at the next cold load.
#[gpui::test]
async fn close_then_reopen_orders_the_new_chain_behind_the_drained_flush(
    cx: &mut gpui::TestAppContext,
) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    let message = |n: u64, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: text.into(),
            chunks: vec![],
        },
    };

    for idx in 0..3i64 {
        db.upsert_entry(
            id,
            idx,
            idx + 1,
            1_700_000_000_000 + idx,
            None,
            message(idx as u64 + 1, "old").to_payload(),
        )
        .await
        .expect("seed row");
    }

    let sol = store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = vec![message(1, "old"), message(2, "old"), message(3, "old")];
            s.rebuild_streams();
            cx.notify();
        });
        session.read(cx).solution_id
    });

    // One synchronous block: the executor is never pumped between the close and
    // the reopen, so the close's flush is unambiguously still in flight. That is
    // the window the ordering has to cover.
    store.update(cx, |store, cx| {
        store.close_session(id, cx).expect("close_session");
        insert_cold_session(
            id,
            sol,
            SharedString::from("claude-acp"),
            None,
            None,
            store,
            cx,
        );
        let session = store.session(id).expect("reopened session");
        session.update(cx, |s, cx| {
            s.entries = vec![
                message(1, "old"),
                message(2, "old"),
                message(3, "old"),
                message(4, "brand new"),
            ];
            s.rebuild_streams();
            // What hydration derives from the rows it read back: only the tail
            // entry is above the watermark, so the reopened session's first
            // persist writes exactly one row and then trims past it.
            s.persisted_main_seq = 3;
            cx.notify();
        });
        store.persist_main_stream(id, cx);
    });
    cx.run_until_parked();

    let texts: Vec<String> = db
        .load_entries(id)
        .await
        .expect("load rows")
        .iter()
        .map(
            |row| match crate::session_entry::kind_from_payload(&row.payload).expect("decode") {
                SessionEntryKind::UserMessage { content_md, .. } => content_md,
                other => panic!("unexpected persisted kind: {other:?}"),
            },
        )
        .collect();
    assert_eq!(
        texts,
        vec![
            "old".to_string(),
            "old".to_string(),
            "old".to_string(),
            "brand new".to_string(),
        ],
        "the reopened session's new message must survive — an unordered close \
         flush deletes it with delete_entries_from(3)"
    );
}

/// The same ordering, over a transcript long enough that the close flush cannot
/// have finished by the time the reopen runs.
///
/// The reopen is separated from the close by real background round trips — five
/// `load_entries` awaits standing in for `reopen_closed_session`'s own
/// `db.reopen_session().await` plus `hydrate_all_for_solution().await` — so the
/// ordering cannot be an artifact of everything happening inside one synchronous
/// block, and this stays a real user-visible sequence ("close a long chat,
/// reopen it, type") rather than a same-tick race. The 200 entries no longer
/// stretch the flush across 200 yields (`upsert_entries_and_trim` writes the row
/// set in one), but they still make its trailing trim a 200-row one — which is
/// the value that deletes the new message the moment the chain stops ordering
/// the two.
#[gpui::test]
async fn close_then_reopen_orders_a_long_flush_before_the_new_message(
    cx: &mut gpui::TestAppContext,
) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    const ENTRIES: u64 = 200;

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    let message = |n: u64, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: text.into(),
            chunks: vec![],
        },
    };

    for idx in 0..ENTRIES as i64 {
        db.upsert_entry(
            id,
            idx,
            idx + 1,
            1_700_000_000_000 + idx,
            None,
            b"seed".to_vec(),
        )
        .await
        .expect("seed row");
    }

    let sol = store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = (1..=ENTRIES).map(|n| message(n, "old")).collect();
            s.rebuild_streams();
            cx.notify();
        });
        session.read(cx).solution_id
    });

    store.update(cx, |store, cx| {
        store.close_session(id, cx).expect("close_session");
    });
    // Stand in for the reopen's own latency: `reopen_closed_session` awaits
    // `db.reopen_session` and then a whole `hydrate_all_for_solution` before the
    // reopened tab can persist anything.
    for _ in 0..5 {
        db.load_entries(id).await.expect("reopen-path read");
    }

    store.update(cx, |store, cx| {
        insert_cold_session(
            id,
            sol,
            SharedString::from("claude-acp"),
            None,
            None,
            store,
            cx,
        );
        let session = store.session(id).expect("reopened session");
        session.update(cx, |s, cx| {
            let mut entries: Vec<_> = (1..=ENTRIES).map(|n| message(n, "old")).collect();
            entries.push(message(ENTRIES + 1, "brand new"));
            s.entries = entries;
            s.rebuild_streams();
            s.persisted_main_seq = ENTRIES;
            cx.notify();
        });
        store.persist_main_stream(id, cx);
    });
    cx.run_until_parked();

    let rows = db.load_entries(id).await.expect("load rows");
    assert_eq!(
        rows.len() as u64,
        ENTRIES + 1,
        "the long close flush must be ordered BEFORE the reopened session's \
         append — otherwise its delete_entries_from({ENTRIES}) deletes the new \
         message"
    );
}

/// A chain that has already run must not stay keyed in `entries_persist_chain`
/// forever. Retiring it is what bounds the map: a soft close deliberately leaves
/// its chain behind (a reopen has to be able to order behind it), so without a
/// sweep the editor would accumulate one dead key per closed session for the
/// whole process lifetime.
///
/// Retiring only ever removes a SPENT chain, which is why it cannot reintroduce
/// the close→reopen inversion: a link that has finished orders nothing. Both
/// halves of that are asserted here, and the negative one is the load-bearing
/// one: a sweep that reclaimed a chain still in flight would cancel the very
/// close flush [`ChainDisposition::Drain`] exists to preserve, and would do it
/// silently.
#[gpui::test]
async fn a_finished_persist_chain_is_retired_from_the_map(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = vec![SessionEntry {
                created_ms: 1_700_000_000_000,
                mod_seq: 1,
                subagent_id: None,
                kind: SessionEntryKind::UserMessage {
                    id: None,
                    content_md: "alpha".into(),
                    chunks: vec![],
                },
            }];
            s.rebuild_streams();
            cx.notify();
        });
        store.close_session(id, cx).expect("close_session");
        // Still inside the synchronous block, so the flush is queued and has
        // not been polled: the sweep must leave it alone.
        store.retire_finished_persist_chains();
        assert!(
            store.entries_persist_chain.contains_key(&id),
            "a chain still in flight must survive the sweep — reclaiming it \
             here would cancel the close flush instead of draining it"
        );
    });
    cx.run_until_parked();

    store.update(cx, |store, _| {
        assert!(
            store
                .entries_persist_chain
                .get(&id)
                .is_some_and(|chain| chain.is_finished()),
            "the drained chain must have run to completion and said so"
        );
        store.retire_finished_persist_chains();
        assert!(
            !store.entries_persist_chain.contains_key(&id),
            "a spent chain must be reclaimed — otherwise the map grows one \
             entry per closed session for the process's lifetime"
        );
    });
}

/// A session soft-closed and THEN hard-purged must not resurrect its rows.
///
/// The soft close leaves a drained chain under the session's key on purpose, and
/// `purge_session_hard` cannot reach it through `teardown_session_runtime` —
/// the session is no longer hydrated, so that path early-returns. Without an
/// explicit abandon in that branch the surviving link writes entry rows whose
/// parent `solution_sessions` row the purge has just deleted: orphans no UI
/// enumerates and no GC reaps.
///
/// What is pinned is that the purge REACHES the retained chain, and that is
/// asserted directly — synchronously, off the map — so it holds at any chain
/// depth. The `load_entries` check behind it is the stronger claim and is exact
/// only for the ONE-link flush this fixture builds (8 entries, a single
/// `persist_all_rows`): dropping a deeper chain cancels it from the outside in,
/// so some rows still land. `purge_session_hard_abandons_in_flight_persist_chain`
/// carries the measured numbers (2 links leak 1 row, 8 leak 5); it is a
/// pre-existing purge-ordering gap, not something this branch can close.
#[gpui::test]
async fn purge_after_soft_close_abandons_the_drained_chain(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    let message = |n: u64| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: "row".into(),
            chunks: vec![],
        },
    };

    store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = (1..=8).map(message).collect();
            s.rebuild_streams();
            cx.notify();
        });
        // Close (chain retained, still in flight) and purge in the same
        // synchronous block, so the purge lands while the flush is queued.
        store.close_session(id, cx).expect("close_session");
        store.purge_session_hard(id, None, cx);
        // Depth-independent evidence that the not-hydrated branch abandoned the
        // chain at all: nothing else removes this key (the sweep only reclaims
        // spent chains, and this one has not been polled yet).
        assert!(
            !store.entries_persist_chain.contains_key(&id),
            "the purge must reach the retained chain of a session it can no \
             longer tear down through `teardown_session_runtime`"
        );
    });
    cx.run_until_parked();

    assert!(
        db.load_entries(id).await.expect("load").is_empty(),
        "the purge must abandon the soft close's retained chain — a surviving \
         link re-inserts rows for a session that no longer exists"
    );
}

/// The solution-level twin of `purge_after_soft_close_abandons_the_drained_chain`.
///
/// `purge_solution_fully` only iterates HYDRATED sessions, so a session the
/// solution soft-closed earlier is invisible to its per-session purge loop while
/// its drained chain is still keyed. `delete_for_solution` then sweeps that
/// session's rows — and a surviving link writes them straight back, this time
/// with no `solution_sessions` parent at all.
#[gpui::test]
async fn purge_solution_after_soft_close_abandons_the_drained_chain(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        let solution_id = session.read(cx).solution_id;
        session.update(cx, |s, cx| {
            s.entries = (1..=8)
                .map(|n| SessionEntry {
                    created_ms: 1_700_000_000_000 + n,
                    mod_seq: n as u64,
                    subagent_id: None,
                    kind: SessionEntryKind::UserMessage {
                        id: None,
                        content_md: "row".into(),
                        chunks: vec![],
                    },
                })
                .collect();
            s.rebuild_streams();
            cx.notify();
        });
        // The soft close drops the session from `by_solution`, so the purge's
        // per-session loop below never sees it — only the solution-wide abandon
        // can reach its retained chain.
        store.close_session(id, cx).expect("close_session");
        store.purge_solution_fully(solution_id, None, cx);
    });
    cx.run_until_parked();

    assert!(
        db.load_entries(id).await.expect("load").is_empty(),
        "a solution purge must abandon the retained chains of its already \
         soft-closed sessions"
    );
}

/// Fact 23 of the persist-chain plan, made measurable: a flush must cost a
/// CONSTANT number of executor turns, not one per row.
///
/// The per-row `upsert_entry` spent an `executor.spawn` and a connection-lock
/// acquisition on every row, so a flush's wall-clock width scaled with the
/// transcript — and every gap between two of those round trips was a point where
/// a reopen's `load_entries` could read a prefix of the flush and hydrate from
/// it. Counting turns rather than timing anything is what makes the shrink an
/// assertion instead of an anecdote: the number is deterministic, and under the
/// old shape a 200-entry flush cost ~50x what a 4-entry one did.
///
/// `tick()` runs at most one task, so the loop below is "drain the executor,
/// counting". Both sessions are flushed from the same quiesced store, so the
/// two counts differ only by the write path under test.
#[gpui::test]
async fn a_flush_costs_the_same_executor_turns_at_any_size(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    const SMALL: u64 = 4;
    const LARGE: u64 = 200;

    let (store, seeded_id, _tmp) = crate::store::test_support::seed_store_with_session(cx).await;
    let (db, sol) = store.update(cx, |store, cx| {
        (
            store.persistence().expect("persistence"),
            store
                .session(seeded_id)
                .expect("session")
                .read(cx)
                .solution_id,
        )
    });

    let message = |n: u64| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: format!("m{n}"),
            chunks: vec![],
        },
    };

    let small = SolutionSessionId::new();
    let large = SolutionSessionId::new();
    store.update(cx, |store, cx| {
        for (id, count) in [(small, SMALL), (large, LARGE)] {
            insert_cold_session(
                id,
                sol,
                SharedString::from("claude-acp"),
                None,
                None,
                store,
                cx,
            );
            let session = store.session(id).expect("session");
            session.update(cx, |s, cx| {
                s.entries = (1..=count).map(message).collect();
                s.rebuild_streams();
                cx.notify();
            });
        }
    });
    cx.run_until_parked();

    store.update(cx, |store, cx| store.persist_all_rows(small, cx));
    let mut small_turns = 0usize;
    while cx.executor().tick() {
        small_turns += 1;
    }

    store.update(cx, |store, cx| store.persist_all_rows(large, cx));
    let mut large_turns = 0usize;
    while cx.executor().tick() {
        large_turns += 1;
    }

    assert_eq!(
        db.load_entries_blocking(large).expect("read").len() as u64,
        LARGE,
        "precondition: the large flush actually wrote its rows"
    );
    assert_eq!(
        small_turns, large_turns,
        "a {LARGE}-row flush must cost the same executor turns as a {SMALL}-row \
         one ({small_turns} vs {large_turns}) — the whole row set plus its \
         trailing trim is one round trip"
    );
}

/// The same shrink stated as the property it buys, and the direct answer to the
/// plan's fact 22: a reader that samples the table between every executor turn
/// of a close flush must never catch it half-applied.
///
/// This is fact 22's reproduction with its timing made exhaustive rather than
/// lucky. Forty stale rows on disk, an eight-entry transcript in memory — the
/// SHRINKING direction, deliberately, because it is the one in which the flush's
/// trailing trim is observable: an intermediate state where the first eight rows
/// are fresh and thirty-two stale ones still trail them is exactly the "flat
/// mirror longer than Main" shape cold load reads as a legacy layout. The table
/// is read on the test's own thread after every single turn the executor takes,
/// and each sample is reduced to `(row count, how many rows are still stale)`.
///
/// Under the per-row writer those samples walked `(40, 40)`, `(40, 39)` … down
/// to `(40, 32)` before the trim dropped them to `(8, 0)`; batching the rows but
/// awaiting the trim separately still parks on `(40, 32)`. Now the whole flush
/// is one acquisition of the one lock every reader takes, so only its two
/// endpoints exist to be seen.
///
/// What this does NOT claim: the window is closed. A reopen whose `load_entries`
/// is ordered entirely BEFORE the flush still hydrates the pre-flush rows and
/// still trims the rest afterwards. What batching removes is the torn middle —
/// the reader now sees a self-consistent snapshot either way, and the interval
/// in which it can see the stale one shrank from N round trips to one.
#[gpui::test]
async fn a_reader_never_observes_a_half_written_close_flush(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    const STALE_ROWS: i64 = 40;
    const ENTRIES: u64 = 8;

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    for idx in 0..STALE_ROWS {
        db.upsert_entry(id, idx, 0, 1_700_000_000_000 + idx, None, b"stale".to_vec())
            .await
            .expect("seed stale row");
    }

    store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = (1..=ENTRIES)
                .map(|n| SessionEntry {
                    created_ms: 1_700_000_000_000 + n as i64,
                    mod_seq: n,
                    subagent_id: None,
                    kind: SessionEntryKind::UserMessage {
                        id: None,
                        content_md: format!("m{n}"),
                        chunks: vec![],
                    },
                })
                .collect();
            s.rebuild_streams();
            cx.notify();
        });
        store.close_session(id, cx).expect("close_session");
    });

    let mut observed = std::collections::BTreeSet::new();
    loop {
        let rows = db.load_entries_blocking(id).expect("sample");
        let stale = rows.iter().filter(|row| row.payload == b"stale").count();
        observed.insert((rows.len(), stale));
        if !cx.executor().tick() {
            break;
        }
    }

    assert_eq!(
        observed.into_iter().collect::<Vec<_>>(),
        vec![
            (ENTRIES as usize, 0),
            (STALE_ROWS as usize, STALE_ROWS as usize)
        ],
        "a reader sampling between every executor turn must only ever see the \
         row set from before the flush or the one after it — anything in \
         between is a torn read, and cold load turns it into a permanent \
         truncation"
    );
}

/// Quitting the editor must FLUSH the in-flight entry-row writes, not drop them
/// with the process. Before the app-quit hook there was none at all: the store
/// global died with the process and every queued link was cancelled — silently
/// and permanently, because the persist helpers advance `persisted_main_seq`
/// synchronously before they spawn, so nothing re-picks those rows.
///
/// The test is only meaningful because `TestScheduler` models GPUI's real quit
/// contract: `App::shutdown` blocks the main thread on the quit futures through
/// the FOREGROUND executor's session, and `TestScheduler::step` excludes
/// runnables whose session is blocked — exactly as the production main-thread
/// channel stops being drained. So the executor is deliberately NOT pumped
/// before `cx.quit()`: the chain has not started, and the only thing that can
/// finish it is the quit hook awaiting background work.
///
/// `set_block_on_ticks` pins the tick budget the scheduler draws for a timed
/// block (`1..=1000` by default, i.e. as few as one poll) so the drain is not
/// randomly cut short; production's budget is `gpui::SHUTDOWN_TIMEOUT` of real
/// wall clock, which a single batched write never approaches.
#[gpui::test]
async fn app_quit_flushes_the_in_flight_entry_row_chain(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    for idx in 0..3 {
        db.upsert_entry(id, idx, 0, 1_700_000_000_000 + idx, None, b"stale".to_vec())
            .await
            .expect("seed stale row");
    }

    let message = |n: u64, text: &str| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: text.into(),
            chunks: vec![],
        },
    };

    store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = vec![message(1, "alpha"), message(2, "bravo")];
            s.rebuild_streams();
            cx.notify();
        });
        // Two links, both issued while the executor is idle: the second owns the
        // first, so quitting has to drain the whole chain, not just its tail.
        store.persist_main_stream(id, cx);
        store.persist_all_rows(id, cx);
        assert!(
            store.entries_persist_chain.contains_key(&id),
            "fixture: the chain must still be queued when the app quits"
        );
    });

    cx.executor().set_block_on_ticks(usize::MAX..=usize::MAX);
    cx.quit();

    let rows = db.load_entries_blocking(id).expect("load rows");
    // A row the flush never reached still holds the seeded bytes, which do not
    // decode — surface them as themselves so a failure reads as the row set that
    // was found rather than as a serde panic.
    let texts: Vec<String> = rows
        .iter()
        .map(
            |row| match crate::session_entry::kind_from_payload(&row.payload) {
                Ok(SessionEntryKind::UserMessage { content_md, .. }) => content_md,
                Ok(other) => format!("{other:?}"),
                Err(_) => String::from_utf8_lossy(&row.payload).into_owned(),
            },
        )
        .collect();
    assert_eq!(
        texts,
        vec!["alpha".to_string(), "bravo".to_string()],
        "quitting must flush the queued chain: idx 0..1 rewritten from the \
         in-memory Main stream and the stale idx 2 trimmed"
    );
}

/// The quit hook takes the WHOLE `entries_persist_chain` map, which is only
/// disposition-correct because a hard purge has already REMOVED its chain
/// (`ChainDisposition::Abandon`) before it issues the cascade DELETE. If the
/// abandon ever degrades to "leave it keyed and just log", quitting resurrects
/// entry rows for a session that no longer has a `solution_sessions` row —
/// orphans no UI enumerates and no GC reaps.
///
/// Asserted on payloads rather than on an empty table, because the purge's own
/// DELETE is background work that may or may not land inside the quit window:
/// what must never appear is the abandoned flush's content. Deliberately no
/// synchronous precondition on the map either — that is already pinned by
/// `purge_after_soft_close_abandons_the_drained_chain`, and asserting it here
/// would swallow the failure this test exists to produce.
#[gpui::test]
async fn app_quit_does_not_resurrect_an_abandoned_persist_chain(cx: &mut gpui::TestAppContext) {
    use crate::session_entry::{SessionEntry, SessionEntryKind};

    let (id, _thread, _tmp) = create_session_with_thread(cx).await;
    let db = Arc::new(crate::db::SolutionAgentDb::open(cx.executor()).expect("open db"));
    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    store.update(cx, |store, cx| store.set_persistence(db.clone(), cx));

    for idx in 0..3 {
        db.upsert_entry(id, idx, 0, 1_700_000_000_000 + idx, None, b"stale".to_vec())
            .await
            .expect("seed stale row");
    }

    let message = |n: u64| SessionEntry {
        created_ms: 1_700_000_000_000 + n as i64,
        mod_seq: n,
        subagent_id: None,
        kind: SessionEntryKind::UserMessage {
            id: None,
            content_md: "resurrected".into(),
            chunks: vec![],
        },
    };

    store.update(cx, |store, cx| {
        let session = store.session(id).expect("session");
        session.update(cx, |s, cx| {
            s.entries = (1..=2).map(message).collect();
            s.rebuild_streams();
            cx.notify();
        });
        store.persist_main_stream(id, cx);
        // Soft close retains the chain under its key; the purge then has to
        // abandon it from the not-hydrated branch.
        store.close_session(id, cx).expect("close_session");
        store.purge_session_hard(id, None, cx);
    });

    cx.executor().set_block_on_ticks(usize::MAX..=usize::MAX);
    cx.quit();

    let rows = db.load_entries_blocking(id).expect("load rows");
    assert!(
        rows.iter().all(|row| row.payload == b"stale"),
        "quitting must not write the abandoned chain's rows for a session the \
         purge deleted; found {} row(s)",
        rows.len(),
    );
}
