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
            store.model_catalog.set_models(
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
