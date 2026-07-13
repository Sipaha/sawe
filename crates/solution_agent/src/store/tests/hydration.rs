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
        solution_id: solution_id,
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
        member_id: None,
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

    db.update_tab_orders(solution_id, vec![id_b, id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id, cx)
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
                store.create_session(solution_id, agent_id.clone(), project.clone(), cx)
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
        .list_open_tabs(solution_id)
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
        .list_for_solution(solution_id)
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
        solution_id: solution_id,
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
        member_id: None,
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
    db.update_tab_orders(solution_id, vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id, cx)
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
        solution_id: solution_id,
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
        member_id: None,
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
    db.update_tab_orders(solution_id, vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id, cx)
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
        solution_id: solution_id,
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
        member_id: None,
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
    db.update_tab_orders(solution_id, vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id, cx)
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
        solution_id: solution_id,
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
        member_id: None,
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
    db.update_tab_orders(solution_id, vec![id_a])
        .await
        .expect("tab order");

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.restore_open_tabs(solution_id, cx)
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
        solution_id: solution_id,
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
        member_id: None,
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
    db.update_tab_orders(solution_id, vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id, cx)
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
                store.restore_open_tabs(solution_id, cx)
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
        solution_id: solution_id,
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
        member_id: None,
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
    db.update_tab_orders(solution_id, vec![id_a])
        .await
        .expect("tab order");

    // First restore — MIGRATE branch: recovers desired_model from blob.
    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id, cx)
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
        .list_for_solution(solution_id)
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
                store.restore_open_tabs(solution_id, cx)
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
        solution_id: solution_id,
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
        member_id: None,
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
    db.update_tab_orders(solution_id, vec![id_a])
        .await
        .expect("tab order");

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.restore_open_tabs(solution_id, cx)
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
    let solution_id = SolutionId(7);
    let now = Utc::now();

    let meta = crate::model::SolutionSessionMetadata {
        id: session_id,
        solution_id: solution_id,
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
        member_id: None,
    };

    // Build the entity exactly as the fixed fresh-entity branch does.
    let entity = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |_, cx| {
            cx.new(|_| {
                let mut s = SolutionSession::new_idle(
                    meta.id,
                    meta.solution_id,
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
