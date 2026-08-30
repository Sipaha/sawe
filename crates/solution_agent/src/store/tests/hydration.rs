#![allow(unused_imports)]

use super::common::*;
use crate::adapter::AdapterRegistry;
use crate::model::SessionState;
use crate::store::*;
use crate::test_support::{MockAgentServer, MockConnection, PromptGate};
use chrono::Utc;
use gpui::{Entity, SharedString, TestAppContext};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[gpui::test]
async fn hydrate_all_hydrates_cold_sessions(cx: &mut TestAppContext) {
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
                store.hydrate_all_for_solution(solution_id, cx)
            })
        })
        .await
        .expect("restore");
    // `hydrate_all_for_solution` returns the ids it hydrated in the order the
    // metadata query yielded them, NOT in tab order — the tab-order contract
    // lives in `by_solution`, asserted through `sessions_for` below.
    assert_eq!(ordered.len(), 2);
    assert!(ordered.contains(&id_a) && ordered.contains(&id_b));

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
            // `sessions_for` is what the tab strip reads; insertion order
            // into `by_solution` must match the `tab_order ASC` returned by
            // the DB so the strip ends up identical to what the user closed
            // last time.
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
/// `select_open_tabs` / hydration, so the session was never
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
    // be durable, so a restart's hydration (which queries
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
/// the v2 reconstruction was inlined in the tab-restore path only; the
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
                store.hydrate_all_for_solution(solution_id, cx)
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

/// A tool call persisted mid-flight (its turn ended without terminalising it —
/// e.g. a synchronous `Agent` whose turn was cut short) must NOT rehydrate as
/// `InProgress`. Every restored row is cold-prefix history that nothing can
/// transition again, so an `InProgress` row would render a live-ticking
/// "running Xm Ys" badge forever against an agent that no longer exists (the
/// close-tab → reopen → stuck-plaque bug). Terminal statuses pass through
/// untouched.
#[test]
fn entries_from_rows_terminalizes_stranded_tool_calls() {
    let tool_call = |status: crate::session_entry::ToolStatus| {
        crate::session_entry::SessionEntryKind::ToolCall {
            id: "toolu_1".into(),
            label_md: "Agent".into(),
            kind: agent_client_protocol::schema::ToolKind::Think,
            status,
            content_md: vec![],
            raw_input: None,
            raw_output: None,
            tool_name: Some("Agent".into()),
            locations: vec![],
            status_started_at: Some(1_700_000_000_000),
        }
    };
    let row = |idx: i64, kind: &crate::session_entry::SessionEntryKind| crate::db::EntryRow {
        idx,
        mod_seq: idx,
        created_ms: 1_700_000_000_000 + idx,
        subagent_id: None,
        payload: serde_json::to_vec(kind).unwrap(),
    };
    let in_progress = tool_call(crate::session_entry::ToolStatus::InProgress);
    let pending = tool_call(crate::session_entry::ToolStatus::Pending);
    let awaiting = tool_call(crate::session_entry::ToolStatus::WaitingForConfirmation);
    let completed = tool_call(crate::session_entry::ToolStatus::Completed);
    let failed = tool_call(crate::session_entry::ToolStatus::Failed);

    let entries = crate::store::entries_from_rows(vec![
        row(0, &in_progress),
        row(1, &pending),
        row(2, &awaiting),
        row(3, &completed),
        row(4, &failed),
    ]);

    let status_of = |i: usize| match &entries[i].kind {
        crate::session_entry::SessionEntryKind::ToolCall { status, .. } => status.clone(),
        other => panic!("expected ToolCall at {i}, got {other:?}"),
    };
    assert_eq!(entries.len(), 5);
    for i in 0..3 {
        assert_eq!(
            status_of(i),
            crate::session_entry::ToolStatus::Canceled,
            "non-terminal row {i} must rehydrate as Canceled, not keep ticking",
        );
    }
    assert_eq!(status_of(3), crate::session_entry::ToolStatus::Completed);
    assert_eq!(status_of(4), crate::session_entry::ToolStatus::Failed);
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
                store.hydrate_all_for_solution(solution_id, cx)
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
                store.hydrate_all_for_solution(solution_id, cx)
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
            store.hydrate_all_for_solution(solution_id, cx)
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
                store.hydrate_all_for_solution(solution_id, cx)
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
                store.hydrate_all_for_solution(solution_id, cx)
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
/// A legacy blob that fails to decode must NOT be migrated, and the desktop
/// restore must not fail over it.
///
/// The trap is that migration is what makes the corruption permanent. On a
/// decode failure the entity is built with an empty transcript, so a `migrating`
/// flag derived from "rows absent, not wiped" alone flushes ZERO rows and bumps
/// the epoch — and "no rows + epoch > 0" is exactly what `is_wiped_row_native`
/// reads as a deliberate `/clear`. The still-intact bytes on disk would then be
/// suppressed by every read path forever after, so a blob that a later build (or
/// a hand repair) could decode is instead thrown away by the first restore that
/// could not.
///
/// The restore itself keeps going: the row still has a title and a tab, and one
/// unreadable transcript must not cost the user every other session in the
/// Solution. The MCP read path takes the opposite decision (it errors) — see
/// `mcp::tests::get_session_refuses_to_serve_an_undecodable_blob_as_empty`.
#[gpui::test]
async fn an_undecodable_blob_is_not_migrated_away(cx: &mut TestAppContext) {
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

    let id = crate::model::SolutionSessionId::new();
    let now = Utc::now();
    db.save_metadata(crate::model::SolutionSessionMetadata {
        id,
        solution_id,
        agent_id: SharedString::from("claude-acp"),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("corrupt session"),
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
    })
    .await
    .expect("meta");

    // Truncated JSON — the realistic corruption (a partial write), not random
    // bytes. Intact prefix, so a repair is plausible; that is the whole point.
    let intact = serde_json::to_vec(&PersistedSession {
        title: "corrupt session".into(),
        entry_summaries: vec!["a line the user still wants".to_string()],
        ..Default::default()
    })
    .expect("encode blob");
    let mut truncated = intact.clone();
    truncated.truncate(intact.len() / 2);
    assert!(
        serde_json::from_slice::<PersistedSession>(&truncated).is_err(),
        "fixture must actually fail to decode, or this test proves nothing"
    );
    db.save_blob(id, truncated).await.expect("blob");
    assert_eq!(
        db.load_epoch(id).await.expect("load epoch"),
        None,
        "precondition: an un-migrated session's epoch column is NULL"
    );

    let ordered = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.hydrate_all_for_solution(solution_id, cx)
            })
        })
        .await
        .expect("a corrupt transcript must not fail the whole restore");
    assert_eq!(
        ordered,
        vec![id],
        "the session still exists — it has a metadata row and a tab"
    );
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.session(id).expect("restored").read_with(cx, |s, _| {
                assert!(s.entries.is_empty(), "nothing could be decoded");
                assert_eq!(
                    s.epoch, 0,
                    "the migration bump must not fire for a blob that was never read"
                );
            });
        });
    });
    // The migration, if one were scheduled, is spawned + detached.
    cx.run_until_parked();

    assert_eq!(
        db.load_epoch(id).await.expect("load epoch after"),
        None,
        "no epoch may be written: with zero rows it would read as a deliberate wipe \
         and permanently suppress the blob"
    );
    assert!(
        db.load_entries(id).await.expect("load rows").is_empty(),
        "nothing to migrate, so nothing may be written"
    );
    assert!(
        db.load_blob(id).await.expect("load blob").is_some(),
        "the bytes must be left on disk — they are the only copy of the transcript"
    );

    // What all of that buys: a repair still works. Restore the intact bytes and
    // cold-load again; with the epoch bumped to 1 above, this session would be
    // `is_wiped_row_native` and the blob would never be looked at again.
    db.save_blob(id, intact).await.expect("repair blob");
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.sessions.remove(&id);
            store.by_solution.remove(&solution_id);
        });
    });
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.hydrate_all_for_solution(solution_id, cx)
        })
    })
    .await
    .expect("restore 2");
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(id)
                .expect("re-restored")
                .read_with(cx, |s, _| {
                    assert_eq!(
                        s.entries.len(),
                        1,
                        "the repaired blob must come back — the failed restore must \
                         not have marked the session as wiped"
                    );
                });
        });
    });
}

/// The production hydration path must seed the model/effort a cold tab's status
/// row renders. It used to leave all three at their defaults, which was
/// invisible while cold tabs were unreachable and self-healed on first use;
/// the Solution band reopens directly onto a cold session, so the blank label
/// is now on screen at every restart.
#[gpui::test]
async fn hydrate_all_restores_model_and_effort(cx: &mut TestAppContext) {
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

    let id = crate::model::SolutionSessionId::new();
    let now = Utc::now();
    let meta = crate::model::SolutionSessionMetadata {
        id,
        solution_id,
        agent_id: SharedString::from("claude-acp"),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-a"),
        title: SharedString::from("cold"),
        created_at: now,
        last_activity_at: now,
        preview: None,
        total_tokens: None,
        context_count: 1,
        cwd: PathBuf::new(),
        parent_session_id: None,
        desired_model: Some("opus".into()),
        desired_effort: Some("high".into()),
        cached_models: vec![claude_native::ModelInfo {
            value: "opus".into(),
            display_name: "Opus".into(),
            description: String::new(),
        }],
        tab_order: None,
    };
    db.save_metadata(meta).await.expect("meta");
    // Rows, not a blob: the branch every non-legacy session takes.
    db.upsert_entry(id, 0, 1, 1_700_000_000_000, None, b"{}".to_vec())
        .await
        .expect("row");
    db.update_tab_orders(solution_id, vec![id])
        .await
        .expect("tab order");

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.hydrate_all_for_solution(solution_id, cx)
        })
    })
    .await
    .expect("hydrate");
    cx.run_until_parked();

    cx.update(|cx| {
        SolutionAgentStore::global(cx).read_with(cx, |store, cx| {
            let session = store.session(id).expect("hydrated");
            session.read_with(cx, |s, _| {
                assert_eq!(s.desired_model.as_deref(), Some("opus"));
                assert_eq!(s.desired_effort.as_deref(), Some("high"));
                assert_eq!(
                    s.cached_models
                        .iter()
                        .map(|model| model.value.as_str())
                        .collect::<Vec<_>>(),
                    vec!["opus"],
                    "the model picker's options must survive too, or the cold tab \
                     offers an empty list until the first live capture"
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
                store.hydrate_all_for_solution(solution_id, cx)
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
    let metas = db.list_for_solution(solution_id).await.expect("list metas");
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
                store.hydrate_all_for_solution(solution_id, cx)
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
                store.hydrate_all_for_solution(solution_id, cx)
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
        migrated_text.contains("user said hello") && migrated_text.contains("assistant replied hi"),
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
async fn legacy_teammate_tagged_rows_realign_to_main_local_on_cold_load(cx: &mut TestAppContext) {
    use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let asst = |n: u64, subagent: Option<&str>, text: &str| {
        std::sync::Arc::new(SessionEntry {
            created_ms: 1_700_000_000_000 + n as i64,
            mod_seq: n,
            subagent_id: subagent.map(SharedString::from),
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text.into())],
            },
        })
    };
    let user = |n: u64, text: &str| {
        std::sync::Arc::new(SessionEntry {
            created_ms: 1_700_000_000_000 + n as i64,
            mod_seq: n,
            subagent_id: None,
            kind: SessionEntryKind::UserMessage {
                id: None,
                content_md: text.into(),
                chunks: vec![],
            },
        })
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
        .map(
            |r| match crate::session_entry::kind_from_payload(&r.payload).expect("decode") {
                SessionEntryKind::AssistantMessage { chunks } => chunks
                    .iter()
                    .filter_map(|c| match c {
                        AssistantChunk::Message(m) => Some(m.clone()),
                        _ => None,
                    })
                    .collect(),
                SessionEntryKind::UserMessage { content_md, .. } => content_md,
                _ => String::new(),
            },
        )
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

/// `/clear` has to wipe BOTH persisted representations of a transcript, or a
/// wiped conversation comes back.
///
/// The population this covers is not the shrinking "never migrated" one — it is
/// any session old enough to have been written by a pre-Phase-4 build, migrated
/// to entry rows since, and then `/clear`ed:
///
/// 1. `reset_context` truncates the entry rows (`upsert_entries_and_trim(id, [],
///    0)`) and bumps the epoch. Nothing used to clear `acp_thread_blob` —
///    `save_blob` has no production caller at all, so the blob is retained for
///    the life of the row.
/// 2. `build_cold_session` consults the blob exactly when a session has NO entry
///    rows, which is permanently the state a `/clear` leaves behind.
/// 3. So the next desktop restore (and the cold `get_session` read, pinned
///    separately in `mcp::tests`) decoded the retained blob and served the
///    PRE-clear transcript.
///
/// Drives the real sequence end to end: rows + a legacy blob, `/clear`, evict
/// the solution, restore it from disk.
#[gpui::test]
async fn clear_wipes_the_legacy_blob_so_a_restore_cannot_replay_it(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let solution_id = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .expect("session exists")
            .read(cx)
            .solution_id
    });

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(
                        "the secret the user wants gone".to_string(),
                    ),
                ),
                false,
                cx,
            );
        });
    });
    cx.run_until_parked();

    // The shape a migrated pre-Phase-4 session really has on disk: entry rows
    // AND the original blob, which `hydrate_all_for_solution` deliberately keeps
    // as the model/effort fallback (pinned by
    // `legacy_v1_entry_summaries_survive_cold_load`).
    let legacy_blob = serde_json::to_vec(&PersistedSession {
        title: "migrated session".into(),
        entry_summaries: vec!["the secret the user wants gone".to_string()],
        ..Default::default()
    })
    .expect("encode blob");
    db.save_blob(session_id, legacy_blob)
        .await
        .expect("save blob");
    assert!(
        !db.load_entries(session_id)
            .await
            .expect("load rows before")
            .is_empty(),
        "fixture must be row-native before the clear"
    );

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| store.reset_context(session_id, cx))
    })
    .await
    .expect("reset_context");
    cx.run_until_parked();

    assert!(
        db.load_entries(session_id)
            .await
            .expect("load rows after")
            .is_empty(),
        "/clear must delete every entry row"
    );
    assert!(
        db.load_blob(session_id)
            .await
            .expect("load blob after")
            .is_none(),
        "/clear must drop the legacy blob too — with zero rows it is what every \
         read path falls back to, so keeping it hands the wiped transcript back"
    );
    let epoch_after_clear = db
        .load_epoch(session_id)
        .await
        .expect("load epoch after")
        .unwrap_or(0);
    assert!(
        epoch_after_clear > 0,
        "/clear must persist the bumped epoch (got {epoch_after_clear})"
    );

    // Desktop restore path: drop the whole solution from memory, then hydrate it
    // back off disk exactly as `SolutionStoreEvent::Opened` does.
    cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .update(cx, |store, cx| store.cold_close_solution(&solution_id, cx));
    });
    cx.run_until_parked();
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            assert!(
                store.session(session_id).is_none(),
                "cold_close_solution must evict the session so the restore is a real cold load"
            );
        });
    });

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.hydrate_all_for_solution(solution_id, cx)
        })
    })
    .await
    .expect("restore");

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let restored = store.session(session_id).expect("session restored");
            restored.read_with(cx, |s, _| {
                assert!(
                    s.entries.is_empty(),
                    "a restored /clear'ed session must be empty, not replaying the \
                     pre-clear blob; got {:?}",
                    s.entries.len()
                );
                assert_eq!(
                    s.epoch, epoch_after_clear as u64,
                    "the restore must serve the PERSISTED epoch — the legacy branch \
                     advertises 1, which a client cached at a higher epoch reads as a reset"
                );
            });
        });
    });
}

/// `/compact` (`rotate_context`) is the same wipe as `/clear` for this purpose:
/// it clears `entries`, bumps the epoch, and rewrites the rows wholesale, which
/// on a session whose new thread has not produced its summary yet leaves ZERO
/// rows — the state in which every read path falls back to `acp_thread_blob`.
/// It carries the retained blob into that state for exactly the same reason, so
/// it goes through the same `persist_context_wipe`.
///
/// Only the DB side is asserted here: the restore that would replay the blob is
/// identical to the one pinned by
/// `clear_wipes_the_legacy_blob_so_a_restore_cannot_replay_it`, since after
/// either wipe the persisted shape is the same (no rows, bumped epoch).
#[gpui::test]
async fn compact_wipes_the_legacy_blob_like_clear_does(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(
                        "the secret the user wants gone".to_string(),
                    ),
                ),
                false,
                cx,
            );
        });
    });
    cx.run_until_parked();

    let legacy_blob = serde_json::to_vec(&PersistedSession {
        title: "migrated session".into(),
        entry_summaries: vec!["the secret the user wants gone".to_string()],
        ..Default::default()
    })
    .expect("encode blob");
    db.save_blob(session_id, legacy_blob)
        .await
        .expect("save blob");

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| store.rotate_context(session_id, cx))
    })
    .await
    .expect("rotate_context");
    cx.run_until_parked();

    assert!(
        db.load_entries(session_id)
            .await
            .expect("load rows after")
            .is_empty(),
        "/compact must delete the pre-rotation entry rows"
    );
    assert!(
        db.load_blob(session_id)
            .await
            .expect("load blob after")
            .is_none(),
        "/compact must drop the legacy blob too, or the next cold load replays \
         the pre-rotation transcript it just archived"
    );
}

/// The FOURTH leak surface, and the most user-visible one:
/// [`SolutionAgentStore::resume_session`] does NOT go through
/// `build_cold_session` — its fresh-entity branch open-codes its own copy of
/// the rows-empty→blob fallback. Guarding only `hydrate_all_for_solution` and
/// the MCP cold read left this one open.
///
/// The path a user actually walks: a migrated session is `/clear`ed by a build
/// that kept the blob, the tab is closed (so the session leaves
/// `store.sessions`), and it is reopened from History. That reopen is a
/// `resume_session`, which took the legacy branch, decoded the retained blob,
/// repainted the erased conversation, and dropped the epoch from N to 1.
#[gpui::test]
async fn resume_of_a_wiped_session_does_not_repaint_the_blob(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            // Resume support is opt-in on the mock; the fresh-entity branch is
            // only reachable after a successful ACP attach.
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_resume_support(Arc::new(
                    AtomicUsize::new(0),
                ))),
            );
        });
    });

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let session_id = crate::model::SolutionSessionId::new();
    let now = Utc::now();
    let meta = crate::model::SolutionSessionMetadata {
        id: session_id,
        solution_id,
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-wiped"),
        title: SharedString::from("cleared session"),
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
    // The metadata row must exist FIRST: `save_blob` / `save_epoch` are
    // `UPDATE .. WHERE id = ?` and would silently no-op without it, which would
    // make this test vacuous — it would pass on an unguarded build too.
    db.save_metadata(meta.clone()).await.expect("save metadata");

    let blob = serde_json::to_vec(&PersistedSession {
        title: "cleared session".into(),
        entry_summaries: vec!["the secret the user wants gone".to_string()],
        ..Default::default()
    })
    .expect("encode blob");
    db.save_blob(session_id, blob).await.expect("save blob");
    db.save_epoch(session_id, 3).await.expect("save epoch");

    // Non-vacuity: the fixture really is the broken shape — blob present, zero
    // rows, epoch set — and the session really is absent from the store, so
    // `resume_session` must take the fresh-entity branch.
    assert!(
        db.load_blob(session_id).await.expect("load blob").is_some(),
        "fixture must actually have a blob on disk"
    );
    assert!(
        db.load_entries(session_id)
            .await
            .expect("load rows")
            .is_empty(),
        "fixture must have zero entry rows"
    );
    cx.update(|cx| {
        assert!(
            SolutionAgentStore::global(cx)
                .read(cx)
                .session(session_id)
                .is_none(),
            "the session must not be in memory, or resume takes the hot path"
        );
    });

    let resumed = cx
        .update(|cx| {
            SolutionAgentStore::global(cx)
                .update(cx, |store, cx| store.resume_session(meta, project, cx))
        })
        .await
        .expect("resume_session");
    assert_eq!(resumed, session_id);
    cx.run_until_parked();

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let session = store.session(session_id).expect("resumed session");
            session.read_with(cx, |s, _| {
                assert!(
                    s.entries.is_empty(),
                    "reopening a /clear'ed tab from History must not repaint the \
                     pre-clear blob; got {} entries",
                    s.entries.len()
                );
                assert_eq!(
                    s.epoch, 3,
                    "the resume must serve the PERSISTED epoch — the legacy branch \
                     bumps a fresh entity to 1, i.e. BACKWARDS from 3"
                );
            });
        });
    });
}

/// The same guard on the same path, for the OTHER thing that leaves a session
/// with no rows and a blob: a blob that does not decode.
///
/// `resume_session` derived `migrating` from "no rows and not wiped" alone, so a
/// failed decode still bumped the epoch and flushed zero rows — and "no rows +
/// epoch > 0" is what `is_wiped_row_native` reads as a deliberate `/clear`.
/// Reopening a corrupt tab from History therefore CONVERTED it into a wiped one,
/// and the intact bytes still sitting in the column became unreachable for
/// every later read. The reopen has to be lossless: it shows the user an empty
/// conversation, but it leaves the row exactly as it found it.
#[gpui::test]
async fn resume_of_an_undecodable_blob_leaves_the_row_recoverable(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_resume_support(Arc::new(
                    AtomicUsize::new(0),
                ))),
            );
        });
    });

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let session_id = crate::model::SolutionSessionId::new();
    let now = Utc::now();
    let meta = crate::model::SolutionSessionMetadata {
        id: session_id,
        solution_id,
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-corrupt"),
        title: SharedString::from("corrupt session"),
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
    // `save_blob` is an `UPDATE .. WHERE id = ?`; without the row first it
    // no-ops and the fixture silently loses its blob.
    db.save_metadata(meta.clone()).await.expect("save metadata");

    let intact = serde_json::to_vec(&PersistedSession {
        title: "corrupt session".into(),
        entry_summaries: vec!["a line the user still wants".to_string()],
        ..Default::default()
    })
    .expect("encode blob");
    let mut truncated = intact.clone();
    truncated.truncate(intact.len() / 2);
    assert!(
        serde_json::from_slice::<PersistedSession>(&truncated).is_err(),
        "fixture must actually fail to decode, or this test proves nothing"
    );
    db.save_blob(session_id, truncated)
        .await
        .expect("save blob");
    assert_eq!(
        db.load_epoch(session_id).await.expect("load epoch"),
        None,
        "fixture is un-migrated: the epoch column is NULL, so nothing but this \
         reopen can set it"
    );

    let resumed = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.resume_session(meta.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("a corrupt transcript must not fail the reopen");
    assert_eq!(resumed, session_id);
    cx.run_until_parked();

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(session_id)
                .expect("resumed session")
                .read_with(cx, |s, _| {
                    assert!(s.entries.is_empty(), "nothing could be decoded");
                    assert_eq!(
                        s.epoch, 0,
                        "the migration bump must not fire for a blob that was never read"
                    );
                });
        });
    });
    assert_eq!(
        db.load_epoch(session_id).await.expect("load epoch after"),
        None,
        "no epoch may be written: with zero rows it would read as a deliberate \
         wipe and permanently suppress the blob"
    );
    assert!(
        db.load_blob(session_id).await.expect("load blob").is_some(),
        "the bytes must be left on disk — they are the only copy of the transcript"
    );

    // What that buys: repair the bytes, close the tab, reopen it — the
    // transcript comes back. It cannot if this reopen marked the row as wiped.
    db.save_blob(session_id, intact).await.expect("repair blob");
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.sessions.remove(&session_id);
            store.by_solution.remove(&solution_id);
        });
    });
    cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .update(cx, |store, cx| store.resume_session(meta, project, cx))
    })
    .await
    .expect("second resume");
    cx.run_until_parked();
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(session_id)
                .expect("re-resumed session")
                .read_with(cx, |s, _| {
                    assert_eq!(
                        s.entries.len(),
                        1,
                        "the repaired blob must come back — the failed reopen must \
                         not have marked the session as wiped"
                    );
                });
        });
    });
}

/// F2, the destructive one: a transient entry-ROW read failure on reopen must
/// not delete the transcript.
///
/// The shape is a session BORN row-native — rows, no blob, `epoch` NULL. When
/// `load_entries` errors, `resume_session` used to take the failure as "no
/// rows": `is_wiped_row_native(true, 0)` is false and there is no blob to
/// decode, so `migrating` was true, and `persist_all_rows` flushed zero rows
/// with `trim_from_idx = 0` — a `delete_entries_from_idx(id, 0)` over every row
/// in the table. One sqlite hiccup during a History reopen and the conversation
/// was gone, with nothing left to recover it from. Unlike the blob cases, this
/// one does not merely hide the transcript; it removes it.
#[gpui::test]
async fn a_failed_row_read_on_reopen_does_not_delete_the_rows(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_resume_support(Arc::new(
                    AtomicUsize::new(0),
                ))),
            );
        });
    });

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let session_id = crate::model::SolutionSessionId::new();
    let now = Utc::now();
    let meta = crate::model::SolutionSessionMetadata {
        id: session_id,
        solution_id,
        agent_id: agent_id.clone(),
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-rows"),
        title: SharedString::from("row-native session"),
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
    db.save_metadata(meta.clone()).await.expect("save metadata");
    for idx in 0..3i64 {
        let entry = crate::session_entry::SessionEntry {
            created_ms: 1_700_000_000_000 + idx,
            mod_seq: (idx + 1) as u64,
            subagent_id: None,
            kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                chunks: vec![crate::session_entry::AssistantChunk::Message(
                    format!("line {idx}").into(),
                )],
            },
        };
        db.upsert_entry(
            session_id,
            idx,
            entry.mod_seq as i64,
            entry.created_ms,
            None,
            entry.to_payload(),
        )
        .await
        .expect("upsert entry");
    }
    // Born row-native: rows on disk, NO blob, epoch never written. That
    // combination is what makes the failure destructive rather than merely
    // misleading — with `epoch` NULL the wiped-session guard does not fire.
    assert_eq!(
        db.load_entries(session_id).await.expect("load rows").len(),
        3,
        "fixture must have rows to lose"
    );
    assert!(
        db.load_blob(session_id).await.expect("load blob").is_none(),
        "fixture must have no blob: the blob path is a different arm"
    );
    assert_eq!(
        db.load_epoch(session_id).await.expect("load epoch"),
        None,
        "fixture's epoch must be NULL, or `is_wiped_row_native` short-circuits \
         the branch under test"
    );

    db.fail_next_entry_load();
    let resumed = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.resume_session(meta.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("a failed row read must not fail the reopen");
    assert_eq!(resumed, session_id);
    cx.run_until_parked();

    // NON-VACUITY: the injected failure must actually have been what this reopen
    // saw. A reopen that read the rows successfully would show three entries
    // here, and would then satisfy every assertion below without ever exercising
    // the branch under test.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(session_id)
                .expect("resumed session")
                .read_with(cx, |s, _| {
                    assert!(
                        s.entries.is_empty(),
                        "the reopen must have hit the injected read failure; got {} \
                         entries, which means it read the rows",
                        s.entries.len()
                    );
                });
        });
    });

    assert_eq!(
        db.load_entries(session_id)
            .await
            .expect("load rows after")
            .len(),
        3,
        "a transient read failure must leave every row on disk — the migration \
         it used to trigger trims from index 0 and deletes them all"
    );
    assert_eq!(
        db.load_epoch(session_id).await.expect("load epoch after"),
        None,
        "and it must not record the failure as a generation bump"
    );

    // The reopen is lossless, not merely non-destructive: close the tab and
    // reopen it with a healthy read, and the transcript is all there.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.sessions.remove(&session_id);
            store.by_solution.remove(&solution_id);
        });
    });
    cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .update(cx, |store, cx| store.resume_session(meta, project, cx))
    })
    .await
    .expect("second resume");
    cx.run_until_parked();
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(session_id)
                .expect("re-resumed session")
                .read_with(cx, |s, _| {
                    assert_eq!(
                        s.entries.len(),
                        3,
                        "every entry must come back on the retry"
                    );
                });
        });
    });
}

/// F1: a transient BLOB read failure is the same defect as an undecodable blob,
/// and was left unjoined to the flag that guards it.
///
/// `load_blob` erroring produced `None`, which is byte-identical to "this
/// session has no blob" — so `migrating` was true, `persist_all_rows` wrote zero
/// rows and bumped the epoch, and "no rows + epoch > 0" is `is_wiped_row_native`'s
/// definition of a `/clear`. One transient sqlite error during a History reopen
/// permanently converted a legacy session into a wiped one, with the intact
/// bytes still sitting in the column and nothing ever looking at them again.
#[gpui::test]
async fn a_failed_blob_read_on_reopen_does_not_wipe_the_session(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_resume_support(Arc::new(
                    AtomicUsize::new(0),
                ))),
            );
        });
    });

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let session_id = crate::model::SolutionSessionId::new();
    let now = Utc::now();
    let meta = crate::model::SolutionSessionMetadata {
        id: session_id,
        solution_id,
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
    db.save_metadata(meta.clone()).await.expect("save metadata");
    db.save_blob(
        session_id,
        serde_json::to_vec(&PersistedSession {
            title: "legacy session".into(),
            entry_summaries: vec!["a line the user still wants".to_string()],
            ..Default::default()
        })
        .expect("encode blob"),
    )
    .await
    .expect("save blob");
    assert_eq!(
        db.load_epoch(session_id).await.expect("load epoch"),
        None,
        "fixture is un-migrated, so only this reopen can set the epoch"
    );

    db.fail_next_blob_load();
    let resumed = cx
        .update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.resume_session(meta.clone(), project.clone(), cx)
            })
        })
        .await
        .expect("a failed blob read must not fail the reopen");
    assert_eq!(resumed, session_id);
    cx.run_until_parked();

    // NON-VACUITY: same as its sibling — a reopen that read the blob would show
    // the transcript here, and would pass everything below without touching the
    // branch under test.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(session_id)
                .expect("resumed session")
                .read_with(cx, |s, _| {
                    assert!(
                        s.entries.is_empty(),
                        "the reopen must have hit the injected read failure; got {} \
                         entries, which means it read the blob",
                        s.entries.len()
                    );
                });
        });
    });

    assert_eq!(
        db.load_epoch(session_id).await.expect("load epoch after"),
        None,
        "a failed READ must not be recorded as a generation the user asked for"
    );
    assert!(
        db.load_blob(session_id).await.expect("load blob").is_some(),
        "the bytes must still be there"
    );

    // And the next reopen — with the read succeeding — serves the transcript,
    // which it cannot do if the failed one marked the row as wiped.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.sessions.remove(&session_id);
            store.by_solution.remove(&solution_id);
        });
    });
    cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .update(cx, |store, cx| store.resume_session(meta, project, cx))
    })
    .await
    .expect("second resume");
    cx.run_until_parked();
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(session_id)
                .expect("re-resumed session")
                .read_with(cx, |s, _| {
                    assert_eq!(
                        s.entries.len(),
                        1,
                        "the transcript must come back on the retry"
                    );
                });
        });
    });
}

/// The other side of the same predicate on the resume path: a genuinely
/// un-migrated blob-only session (`epoch` never written, so NULL → 0) must
/// still have its transcript restored when the user reopens it from History.
/// Without this, `resume_of_a_wiped_session_does_not_repaint_the_blob` would be
/// satisfied by simply never reading the blob again.
#[gpui::test]
async fn resume_of_a_legacy_blob_session_still_restores_it(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::with_resume_support(Arc::new(
                    AtomicUsize::new(0),
                ))),
            );
        });
    });

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    let session_id = crate::model::SolutionSessionId::new();
    let now = Utc::now();
    let meta = crate::model::SolutionSessionMetadata {
        id: session_id,
        solution_id,
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
    db.save_metadata(meta.clone()).await.expect("save metadata");

    let blob = serde_json::to_vec(&PersistedSession {
        title: "legacy session".into(),
        entry_summaries: vec!["a line the user still wants".to_string()],
        ..Default::default()
    })
    .expect("encode blob");
    db.save_blob(session_id, blob).await.expect("save blob");
    // Deliberately NO `save_epoch`: an un-migrated row's `epoch` is NULL.
    assert_eq!(
        db.load_epoch(session_id).await.expect("load epoch"),
        None,
        "an un-migrated session's epoch column must be NULL for this fixture"
    );

    cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .update(cx, |store, cx| store.resume_session(meta, project, cx))
    })
    .await
    .expect("resume_session");
    cx.run_until_parked();

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            let session = store.session(session_id).expect("resumed session");
            session.read_with(cx, |s, _| {
                assert_eq!(
                    s.entries.len(),
                    1,
                    "a never-migrated legacy transcript must still be restored on reopen"
                );
                assert_eq!(s.epoch, 1, "the legacy branch bumps 0 -> 1");
            });
        });
    });
}

/// The epoch must never outrun the row write it describes.
///
/// Both persist paths used to `.log_err()` the entry write and then save the
/// epoch unconditionally. That is benign on its own — a stale generation — but
/// `is_wiped_row_native` makes it load-bearing: "no rows + `epoch > 0`" is
/// precisely what that predicate reads as "wiped, do not consult the blob". A
/// failed write (disk full, I/O) on hydration's legacy→rows migration would
/// therefore persist `epoch = 1` for a session whose rows never landed, and the
/// guard would then suppress its genuinely un-migrated blob FOREVER — turning a
/// transient I/O error into permanent invisibility of a real transcript.
#[gpui::test]
async fn a_failed_row_write_must_not_advance_the_epoch(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hello".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.run_until_parked();

    let epoch_before = db
        .load_epoch(session_id)
        .await
        .expect("load epoch before")
        .unwrap_or(0);
    assert_eq!(
        epoch_before, 0,
        "a session that has never been wiped sits at epoch 0 — which is the \
         value `is_wiped_row_native` depends on staying put"
    );

    db.break_entry_writes_for_test()
        .expect("drop the entries table");

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(session_id)
                .expect("session")
                .update(cx, |s, _| s.bump_epoch());
            store.persist_all_rows(session_id, cx);
        });
    });
    cx.run_until_parked();

    assert_eq!(
        db.load_epoch(session_id)
            .await
            .expect("load epoch after")
            .unwrap_or(0),
        epoch_before,
        "the epoch must not advance past a row write that failed — doing so \
         manufactures the 'no rows + epoch > 0' shape that suppresses the blob"
    );

    // The incremental sibling carries the identical contract: it also writes the
    // epoch after its `upsert_entries_and_trim`, and its trim runs even when the
    // delta is empty, so a broken table fails it too.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(session_id)
                .expect("session")
                .update(cx, |s, _| s.bump_epoch());
            store.persist_main_stream(session_id, cx);
        });
    });
    cx.run_until_parked();

    assert_eq!(
        db.load_epoch(session_id)
            .await
            .expect("load epoch after incremental")
            .unwrap_or(0),
        epoch_before,
        "persist_main_stream must apply the same rule as persist_all_rows"
    );
}

/// The other half of "a failed write must not lie about what landed": the
/// WATERMARK.
///
/// Both persist paths advance `persisted_main_seq` synchronously, in event
/// order, before the detached write runs — deliberately, since that is what
/// stops a burst of ingest events each re-capturing rows an earlier link
/// already owns. But the advance used to happen whether or not the write
/// succeeded, so a failed flush left the watermark claiming rows that are not
/// on disk, and the NEXT flush's `mod_seq > watermark` delta skipped them
/// permanently.
///
/// That is ordinary data loss on its own. It matters more now: the surviving
/// flush still saves its epoch, so for a legacy session mid-migration the disk
/// ends up at zero rows + `epoch > 0` + a retained blob — exactly the shape
/// `is_wiped_row_native` treats as authoritative, which would suppress a real
/// transcript forever.
#[gpui::test]
async fn a_failed_row_write_makes_the_next_flush_re_cover_every_row(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    // Alternating roles so `Stream::push_coalesced` cannot merge them: three
    // distinct Main entries, one per flush.
    let push_assistant = |text: &'static str, cx: &mut TestAppContext| {
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.push_assistant_content_block(
                    agent_client_protocol::schema::ContentBlock::Text(
                        agent_client_protocol::schema::TextContent::new(text.to_string()),
                    ),
                    false,
                    cx,
                );
            });
        });
        cx.run_until_parked();
    };
    let push_user = |text: &'static str, cx: &mut TestAppContext| {
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.push_user_content_block(
                    Some(acp_thread::UserMessageId::new()),
                    agent_client_protocol::schema::ContentBlock::Text(
                        agent_client_protocol::schema::TextContent::new(text.to_string()),
                    ),
                    cx,
                );
            });
        });
        cx.run_until_parked();
    };

    push_assistant("alpha", cx);
    assert_eq!(
        db.load_entries(session_id)
            .await
            .expect("load rows after alpha")
            .len(),
        1,
        "the first flush must land before the failure is introduced"
    );

    // A transient I/O failure swallows the SECOND flush. The watermark still
    // advances past it — that is the optimistic advance the rollback exists for.
    db.break_entry_writes_for_test().expect("break writes");
    push_user("bravo", cx);
    db.restore_entry_writes_for_test().expect("restore writes");

    // The next flush consumes the failure flag, resets the watermark, and
    // therefore re-covers `bravo` instead of writing only `charlie` over a hole.
    push_assistant("charlie", cx);

    let rows = db
        .load_entries(session_id)
        .await
        .expect("load rows after recovery");
    let texts: Vec<String> = rows
        .iter()
        .map(|row| {
            format!(
                "{:?}",
                crate::session_entry::kind_from_payload(&row.payload).expect("decode payload")
            )
        })
        .collect();
    assert_eq!(
        rows.len(),
        3,
        "the recovered flush must re-cover every Main row, not just its own \
         delta over the hole the failed write left; got {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("bravo")),
        "the entry whose write failed must be back on disk; got {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("alpha")) && texts.iter().any(|t| t.contains("charlie")),
        "and the flushes either side of it must be intact; got {texts:?}"
    );
}

/// `MockAgentServer`'s options are not mutually exclusive, and `connect` used to
/// choose between them with a priority `match` that silently dropped the losers
/// — a server built with a prompt gate AND resume support handed out a
/// connection that refused to resume, with no error anywhere. Latent then (no
/// caller combined them); pinned now, because the next caller to combine them
/// would have debugged a resume failure that had nothing to do with resume.
#[gpui::test]
async fn mock_agent_server_composes_a_prompt_gate_with_resume_support(cx: &mut TestAppContext) {
    let (solution_id, _tmp, project) = setup_solution_and_project(cx).await;
    let agent_id = SharedString::from("mock-agent");
    let (_prompt_tx, prompt_rx) = async_channel::unbounded::<()>();
    cx.update(|cx| {
        let registry = Arc::new(AdapterRegistry::new());
        SolutionAgentStore::init_global(cx, registry);
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            store.register_agent_server(
                agent_id.clone(),
                Rc::new(MockAgentServer::configured(
                    Arc::new(AtomicUsize::new(0)),
                    None,
                    Some(PromptGate(prompt_rx)),
                    None,
                    true,
                )),
            );
        });
    });

    let now = Utc::now();
    let meta = crate::model::SolutionSessionMetadata {
        id: crate::model::SolutionSessionId::new(),
        solution_id,
        agent_id,
        acp_session_id: agent_client_protocol::schema::SessionId::new("acp-combined"),
        title: SharedString::from("combined options"),
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

    // Fails with "does not support loading or resuming sessions" if `connect`
    // drops `supports_resume` because a prompt gate was also configured.
    cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .update(cx, |store, cx| store.resume_session(meta, project, cx))
    })
    .await
    .expect("a prompt gate must not disable resume support");
}

/// The watermark rollback closes the failure window only for flushes captured
/// AFTER the failure is visible. The flag is set on the background and consumed
/// on the foreground, so a flush captured in between inherits the lie:
///
///   1. `persist_main_stream` P1 advances the watermark and spawns T1;
///   2. still in the same foreground burst, P2 captures its plan — the flag is
///      clear, so it gets no rollback — and spawns T2 chained behind T1;
///   3. T1 fails and sets the flag; T2 then succeeds with its pre-captured
///      delta and, unguarded, saved the epoch.
///
/// If P2's delta is empty — reachable through the `EntriesRemoved` rewind, or a
/// delta whose only row `drop_empty_payload_rows` discards — and the table was
/// empty because T1 was the legacy→rows migration flush, the disk lands on zero
/// rows + `epoch > 0` + a retained blob: the state `is_wiped_row_native` treats
/// as authoritative, now load-bearing for four reconstruction paths. So both
/// tasks also decline the epoch when the flag is set, which chain ordering
/// (`prev.await`) guarantees they observe.
///
/// The predecessor's failure is injected directly rather than through
/// `break_entry_writes_for_test`: the window needs T1 to FAIL while T2 SUCCEEDS
/// against the same table, and there is no deterministic point between two
/// chained background tasks at which a test could repair it. Setting the flag
/// after the plan capture reproduces the only state the gate can observe, and
/// with the same happens-before edge — the flag is written before the task runs.
#[gpui::test]
async fn a_flush_captured_before_a_predecessor_failed_does_not_advance_the_epoch(
    cx: &mut TestAppContext,
) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let executor = cx.executor();
    let db = Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.set_persistence(db.clone(), cx);
        });
    });

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_assistant_content_block(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("alpha".to_string()),
                ),
                false,
                cx,
            );
        });
    });
    cx.run_until_parked();

    let epoch_before = db
        .load_epoch(session_id)
        .await
        .expect("load epoch before")
        .unwrap_or(0);
    assert_eq!(epoch_before, 0, "a never-wiped session sits at epoch 0");

    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .session(session_id)
                .expect("session")
                .update(cx, |s, _| s.bump_epoch());
            // Captures its plan against a CLEAR flag: the watermark is already at
            // the tail, so this flush's delta is empty and its write will succeed
            // no matter what a predecessor did to the table.
            store.persist_main_stream(session_id, cx);
            // Only now does the chained predecessor fail.
            store
                .entry_write_failed
                .get(&session_id)
                .expect("a persist creates the flag")
                .store(true, std::sync::atomic::Ordering::Release);
        });
    });
    cx.run_until_parked();

    assert_eq!(
        db.load_epoch(session_id)
            .await
            .expect("load epoch after")
            .unwrap_or(0),
        epoch_before,
        "a flush whose own write succeeded must still decline to advance the \
         epoch over a table a chained predecessor left short"
    );

    // The task must READ the flag, never consume it. Consuming it would satisfy
    // every epoch assertion here and still disable the repair: the foreground is
    // the only place that rolls the watermark back, so a task that swallowed the
    // flag would leave the short rows un-re-covered forever.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, _| {
            assert!(
                store
                    .entry_write_failed
                    .get(&session_id)
                    .expect("flag still tracked")
                    .load(std::sync::atomic::Ordering::Acquire),
                "the declining task must leave the flag set for the foreground to \
                 consume — clearing it here skips the watermark rollback"
            );
        });
    });

    // The same gate on the full-flush sibling, which has its own copy of it.
    // Re-arm the predecessor failure against a plan captured while it was clear.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .entry_write_failed
                .get(&session_id)
                .expect("flag exists")
                .store(false, std::sync::atomic::Ordering::Release);
            store.persist_all_rows(session_id, cx);
            store
                .entry_write_failed
                .get(&session_id)
                .expect("flag exists")
                .store(true, std::sync::atomic::Ordering::Release);
        });
    });
    cx.run_until_parked();

    assert_eq!(
        db.load_epoch(session_id)
            .await
            .expect("load epoch after full flush")
            .unwrap_or(0),
        epoch_before,
        "persist_all_rows carries the same gate as persist_main_stream — a full \
         flush must decline the epoch too when a predecessor left the table short"
    );

    // …and the decline is a one-flush lag, not a permanent stall: the next
    // foreground persist consumes the flag, re-covers every row, and saves the
    // epoch it withheld.
    cx.update(|cx| {
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store.persist_main_stream(session_id, cx);
        });
    });
    cx.run_until_parked();

    assert_eq!(
        db.load_epoch(session_id)
            .await
            .expect("load epoch after recovery")
            .unwrap_or(0),
        1,
        "the withheld epoch must land on the next persist, once the flag has \
         been consumed and the rows re-covered"
    );
}
