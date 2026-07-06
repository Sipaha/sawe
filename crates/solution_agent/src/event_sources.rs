//! MCP event-source wiring for `SolutionAgentStore`.
//!
//! Subscribes a long-lived coordinator entity to `SolutionAgentStoreEvent`s
//! emitted by the global store and republishes them as `editor/notification`
//! frames so external MCP clients (and Phase 5.6 e2e tests) can observe
//! session lifecycle changes without polling.
//!
//! Wired event kinds: `agent_session_created`, `agent_session_closed`,
//! `agent_session_state_changed`, `agent_session_title_changed`,
//! `agent_session_message_appended`, `agent_session_notification_sent`.
//! (The `agent_session_background_{shells,agents}_changed` wire forwards were
//! dropped in phase 6d-tail — shells/agents now ride `session.streams`, and the
//! in-process `SessionBackground{Shells,Agents}Changed` store events are still
//! consumed by the desktop GPUI subscriptions, just not republished to the wire.)

use gpui::{App, AppContext as _, Entity, Global, Subscription};
use serde_json::json;

use crate::mcp::truncate_preview;
use crate::model::SolutionSessionId;
use crate::notifier::NotifyKind;
use crate::store::{SolutionAgentStore, SolutionAgentStoreEvent};

pub struct EventSourceCoordinator {
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

struct GlobalEventSourceCoordinator(#[allow(dead_code)] Entity<EventSourceCoordinator>);
impl Global for GlobalEventSourceCoordinator {}

/// Install the coordinator as a global. Idempotent: a second call is a
/// no-op (useful in tests that re-enter `solution_agent::init`). When the
/// `SolutionAgentStore` global is not initialised, returns without wiring
/// anything — `solution_agent::init` is responsible for ordering store
/// init before this call.
pub fn install(cx: &mut App) {
    if cx.try_global::<GlobalEventSourceCoordinator>().is_some() {
        return;
    }
    let Some(store) = SolutionAgentStore::try_global(cx) else {
        return;
    };

    let coordinator = cx.new(|_| EventSourceCoordinator {
        subscriptions: Vec::new(),
    });
    coordinator.update(cx, |this, cx| {
        this.subscriptions
            .push(cx.subscribe(&store, |_this, _store, event, cx| {
                emit_event_notification(event, cx);
                // Coalesced "re-poll" signal: any change that advances a
                // session's `change_seq` also emits a content-free
                // `agent_session_dirty { session_id, current_seq }`. The mobile
                // polls `get_session_changes` to convergence on it, so a single
                // delivered dirty heals a transcript left short by lost per-entry
                // pokes (the "interrupted reply stays interrupted" bug). Pure
                // lifecycle/tab/notify events don't advance a transcript and
                // don't signal dirty.
                if let Some(id) = dirty_target_session(event) {
                    editor_mcp::emit_notification(
                        cx,
                        "agent_session_dirty",
                        build_session_dirty_payload(id, cx),
                    );
                }
            }));
    });

    cx.set_global(GlobalEventSourceCoordinator(coordinator));
}

/// The session whose transcript a store event advanced — i.e. the one a remote
/// client should re-poll. `None` for lifecycle/tab/notify events that don't
/// move a session's `change_seq`. Used to drive the `agent_session_dirty`
/// convergence signal.
fn dirty_target_session(
    event: &SolutionAgentStoreEvent,
) -> Option<crate::model::SolutionSessionId> {
    use SolutionAgentStoreEvent::*;
    match event {
        SessionStateChanged(id)
        | SessionTitleChanged(id)
        | SessionMessageAppended(id, _)
        | SessionQueueChanged(id)
        | SessionSubagentsChanged(id)
        | SessionBackgroundAgentsChanged(id)
        | SessionBackgroundShellsChanged(id) => Some(*id),
        SessionContextReset { id, .. } => Some(*id),
        SessionCreated { .. } | SessionClosed(_) | SessionNotified(..) | TabsChanged { .. } => None,
    }
}

/// Build the `agent_session_dirty` payload: the session id + its CURRENT
/// `change_seq` (read at emit time, so it reflects the latest change, not the
/// one that happened to trigger this emit — a higher seq is always safe, the
/// client converges to it). Falls back to seq 0 when the session is gone.
pub(crate) fn build_session_dirty_payload(
    session_id: SolutionSessionId,
    cx: &App,
) -> serde_json::Value {
    let current_seq = SolutionAgentStore::try_global(cx)
        .and_then(|store| {
            store.read_with(cx, |store, cx| {
                store.session(session_id).map(|s| s.read(cx).change_seq)
            })
        })
        .unwrap_or(0);
    json!({
        "session_id": session_id.to_string(),
        "current_seq": current_seq,
    })
}

/// Translate a single [`SolutionAgentStoreEvent`] into its MCP notification.
fn emit_event_notification(event: &SolutionAgentStoreEvent, cx: &mut App) {
    match event {
        SolutionAgentStoreEvent::SessionCreated {
            id,
            parent_session_id,
        } => {
            editor_mcp::emit_notification(
                cx,
                "agent_session_created",
                json!({
                    "session_id": id.to_string(),
                    // `null` (not omitted) for top-level sessions
                    // so the wire shape is self-documenting: a
                    // missing field looks like "old server"; an
                    // explicit null looks like "top-level".
                    "parent_session_id": parent_session_id.map(|p| p.to_string()),
                }),
            );
        }
        SolutionAgentStoreEvent::SessionClosed(id) => {
            editor_mcp::emit_notification(
                cx,
                "agent_session_closed",
                json!({ "session_id": id.to_string() }),
            );
        }
        SolutionAgentStoreEvent::SessionStateChanged(id) => {
            editor_mcp::emit_notification(
                cx,
                "agent_session_state_changed",
                json!({ "session_id": id.to_string() }),
            );
        }
        SolutionAgentStoreEvent::SessionTitleChanged(id) => {
            editor_mcp::emit_notification(
                cx,
                "agent_session_title_changed",
                json!({ "session_id": id.to_string() }),
            );
        }
        SolutionAgentStoreEvent::SessionMessageAppended(id, entry_index) => {
            let payload = build_message_appended_payload(*id, *entry_index, cx);
            editor_mcp::emit_notification(cx, "agent_session_message_appended", payload);
        }
        SolutionAgentStoreEvent::SessionQueueChanged(id) => {
            let payload = build_queue_changed_payload(*id, cx);
            editor_mcp::emit_notification(cx, "agent_session_queue_changed", payload);
        }
        SolutionAgentStoreEvent::SessionSubagentsChanged(id) => {
            let payload = build_active_subagents_changed_payload(*id, cx);
            editor_mcp::emit_notification(cx, "agent_session_active_subagents_changed", payload);
        }
        SolutionAgentStoreEvent::SessionContextReset { id, context_count } => {
            editor_mcp::emit_notification(
                cx,
                "agent_session_context_reset",
                json!({
                    "session_id": id.to_string(),
                    "context_count": context_count,
                }),
            );
        }
        SolutionAgentStoreEvent::SessionNotified(id, kind) => {
            let kind_str = match kind {
                NotifyKind::Completed => "completed",
                NotifyKind::AwaitingInput => "awaiting_input",
                NotifyKind::Errored => "errored",
            };
            editor_mcp::emit_notification(
                cx,
                "agent_session_notification_sent",
                json!({
                    "session_id": id.to_string(),
                    "kind": kind_str,
                }),
            );
        }
        // `TabsChanged` drives `ConsolePanel` tab synchronisation
        // via a separate per-panel subscriber; the workspace-
        // events coordinator doesn't need to forward it
        // (sequenced `workspace.session_{opened,closed}` already
        // ride out from `persist_tab_order` itself).
        //
        // `SessionBackground{Agents,Shells}Changed` are NOT forwarded to the
        // wire: the push kind is unadvertised (not in `SUPPORTED_EVENT_KINDS`),
        // mobile unsubscribed in 6d-B, and desktop reacts to the in-process
        // GPUI store event directly — so there is no wire consumer. The store
        // event itself still fires for those GPUI subscribers; this coordinator
        // just doesn't mirror it onto MCP. (The `agent_session_dirty`
        // convergence signal still covers these via `dirty_target_session`.)
        SolutionAgentStoreEvent::TabsChanged { .. }
        | SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(_)
        | SolutionAgentStoreEvent::SessionBackgroundShellsChanged(_) => {}
    }
}

/// Build the JSON payload for an `agent_session_message_appended`
/// notification. Pure function (no side effects) so unit tests can
/// assert wire shape without running an MCP server.
///
/// When the session is closed or its `acp_thread` is gone (race
/// between rotate / close and the queued notification), falls back
/// to a minimal payload with just `session_id` + `entry_index` so the
/// consumer can still bump its append counter and re-fetch.
pub(crate) fn build_message_appended_payload(
    session_id: crate::model::SolutionSessionId,
    entry_index: usize,
    cx: &App,
) -> serde_json::Value {
    let role_preview_csid_created_ms = SolutionAgentStore::try_global(cx).and_then(|store| {
        store.read_with(cx, |store, cx| {
            let session = store.session(session_id)?;
            let session_ref = session.read(cx);
            let created_ms = session_ref
                .entries
                .get(entry_index)
                .map(|e| e.created_ms)
                .filter(|&ms| ms > 0);
            let thread = session_ref.acp_thread()?;
            let thread_ref = thread.read(cx);
            let entry = thread_ref.entries().get(entry_index)?;
            let role = match entry {
                acp_thread::AgentThreadEntry::UserMessage(_) => "user",
                acp_thread::AgentThreadEntry::AssistantMessage(_) => "assistant",
                acp_thread::AgentThreadEntry::ToolCall(_) => "tool_call",
                acp_thread::AgentThreadEntry::CompletedPlan(_) => "plan",
                acp_thread::AgentThreadEntry::ContextCompaction(_) => "context_compaction",
                acp_thread::AgentThreadEntry::SystemNote(_) => "system",
            };
            let preview = truncate_preview(&entry.to_markdown(cx), 200);
            // Only user messages can carry originating-client send ids
            // (stamped on each content block's `_meta` by the client).
            // For other roles return an empty Vec; for users return
            // every distinct id we find — a single id for the common
            // one-shot send, multiple when the server-side queue merge
            // rolled N originating bundles into one ACP message (see
            // `client_send_ids_from_user_message`). Clients use the
            // list to pop every contributing optimistic bubble.
            let client_send_ids: Vec<i64> =
                if let acp_thread::AgentThreadEntry::UserMessage(message) = entry {
                    acp_thread::client_send_ids_from_user_message(message)
                } else {
                    Vec::new()
                };
            Some((role.to_string(), preview, client_send_ids, created_ms))
        })
    });
    let (role_preview_csid, created_ms) = match role_preview_csid_created_ms {
        Some((role, preview, csids, created_ms)) => (Some((role, preview, csids)), created_ms),
        None => (None, None),
    };
    let mut obj = match role_preview_csid {
        Some((role, preview, csids)) if !csids.is_empty() => json!({
            "session_id": session_id.to_string(),
            "entry_index": entry_index,
            "role": role,
            "preview": preview,
            // Back-compat alias for pre-R6h mobile builds that only
            // know the singular field. Always the FIRST csid so the
            // legacy "pop one" path keeps working.
            "client_send_id": csids[0],
            "client_send_ids": csids,
        }),
        Some((role, preview, _)) => json!({
            "session_id": session_id.to_string(),
            "entry_index": entry_index,
            "role": role,
            "preview": preview,
        }),
        None => json!({
            "session_id": session_id.to_string(),
            "entry_index": entry_index,
        }),
    };
    if let Some(ms) = created_ms {
        obj["created_ms"] = serde_json::json!(ms);
    }
    obj
}

/// Build the JSON payload for an `agent_session_queue_changed`
/// notification. Walks the session's `pending_messages` queue and
/// emits one descriptor per bundle:
///
///   - `csids`: every `spk_client_send_id` stamp across the bundle's
///     content blocks, in source order, deduplicated. Mobile pops
///     local optimistic bubbles whose csid lands in this set, then
///     renders the bundle as ONE Queued bubble — matching the
///     desktop's "single ghost bubble that grows" semantics for
///     bundles that absorbed multiple originating sends.
///   - `preview`: the markdown rendering the desktop would show
///     (queue marker stripped, image placeholders inline).
///   - `image_count`: how many image blocks the bundle carries, so
///     the mobile can render `[image #N]`-style affordances on the
///     queued bubble without decoding the blocks themselves
///     (chunks aren't shipped on this wire path).
///
/// `bundles: []` is the canonical "queue is empty" payload — the
/// mobile uses that to clear any synthetic Queued bubbles it was
/// rendering off a previous broadcast. Stable session-id + always-
/// present `bundles` array (never omitted) keeps the consumer's
/// decode path simple.
pub(crate) fn build_queue_changed_payload(
    session_id: crate::model::SolutionSessionId,
    cx: &App,
) -> serde_json::Value {
    let bundles: Vec<serde_json::Value> = SolutionAgentStore::try_global(cx)
        .and_then(|store| {
            store.read_with(cx, |store, cx| {
                let session = store.session(session_id)?;
                let session_ref = session.read(cx);
                let out: Vec<serde_json::Value> = session_ref
                    .pending_messages
                    .iter()
                    .map(|bundle| {
                        let csids = acp_thread::csids_from_blocks(&bundle.blocks);
                        let preview =
                            crate::conversation_render::pending_blocks_preview(&bundle.blocks, cx);
                        let image_count: usize = bundle
                            .blocks
                            .iter()
                            .filter(|b| {
                                matches!(b, agent_client_protocol::schema::ContentBlock::Image(_))
                            })
                            .count();
                        json!({
                            "csids": csids,
                            "preview": preview,
                            "image_count": image_count,
                        })
                    })
                    .collect();
                Some(out)
            })
        })
        .unwrap_or_default();
    json!({
        "session_id": session_id.to_string(),
        "bundles": bundles,
    })
}

/// Build the JSON payload for an `agent_session_active_subagents_changed`
/// notification. Walks the session's insertion-ordered subagent vec via
/// the shared `mcp::build_active_subagents_vec` helper so the wire shape
/// matches what `get_session` / `list_sessions` would have returned on a
/// cold fetch — clients can apply either path interchangeably.
///
/// When the session is gone (race between close + queued notification),
/// emits `active_subagents: []` so the consumer's "clear the strip"
/// handler still fires correctly.
pub(crate) fn build_active_subagents_changed_payload(
    session_id: crate::model::SolutionSessionId,
    cx: &App,
) -> serde_json::Value {
    let subagents: Vec<crate::mcp::SubagentDto> = SolutionAgentStore::try_global(cx)
        .and_then(|store| {
            store.read_with(cx, |store, cx| {
                let session = store.session(session_id)?;
                let session_ref = session.read(cx);
                Some(crate::mcp::build_active_subagents_vec(session_ref))
            })
        })
        .unwrap_or_default();
    json!({
        "session_id": session_id.to_string(),
        "active_subagents": subagents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AdapterRegistry;
    use gpui::TestAppContext;
    use std::sync::Arc;

    #[gpui::test]
    async fn install_is_idempotent(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let registry = Arc::new(AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
            install(cx);
            install(cx);
            assert!(cx.try_global::<GlobalEventSourceCoordinator>().is_some());
        });
    }

    #[gpui::test]
    async fn install_without_store_global_is_a_no_op(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
            assert!(cx.try_global::<GlobalEventSourceCoordinator>().is_none());
        });
    }

    #[gpui::test]
    async fn store_event_does_not_panic(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let registry = Arc::new(AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
            install(cx);
            let store = SolutionAgentStore::global(cx);
            // Emit an event via the store. No MCP server is connected — emit
            // is a no-op, but we exercise the subscription path end-to-end.
            store.update(cx, |_s, cx| {
                cx.emit(SolutionAgentStoreEvent::SessionCreated {
                    id: crate::model::SolutionSessionId::new(),
                    parent_session_id: None,
                });
            });
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn message_appended_payload_carries_index_role_and_preview(cx: &mut TestAppContext) {
        // Build a real session with one user entry, then call the pure
        // payload builder directly — emit is a no-op without a socket,
        // so this is the only way to observe the wire shape from a
        // unit test.
        let (session_id, _acp_thread, _tmp) =
            crate::store::tests::create_session_with_thread(cx).await;
        cx.update(|cx| {
            let thread = {
                let store = SolutionAgentStore::global(cx);
                store
                    .read(cx)
                    .session(session_id)
                    .and_then(|s| s.read(cx).acp_thread().cloned())
            }
            .expect("thread");
            thread.update(cx, |thread, cx| {
                let chunk = agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hi".to_string()),
                );
                thread.push_user_content_block(None, chunk, cx);
            });
        });
        cx.executor().run_until_parked();

        cx.update(|cx| {
            let payload = build_message_appended_payload(session_id, 0, cx);
            let obj = payload.as_object().expect("object");
            assert_eq!(
                obj.get("session_id").and_then(|v| v.as_str()),
                Some(session_id.to_string().as_str())
            );
            assert_eq!(obj.get("entry_index").and_then(|v| v.as_u64()), Some(0));
            assert_eq!(obj.get("role").and_then(|v| v.as_str()), Some("user"));
            let preview = obj
                .get("preview")
                .and_then(|v| v.as_str())
                .expect("preview");
            assert!(
                preview.contains("hi"),
                "preview should contain 'hi': {preview}"
            );
        });
    }

    #[gpui::test]
    async fn message_appended_payload_falls_back_when_thread_missing(cx: &mut TestAppContext) {
        let registry = Arc::new(AdapterRegistry::new());
        cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

        cx.update(|cx| {
            let payload =
                build_message_appended_payload(crate::model::SolutionSessionId::new(), 7, cx);
            let obj = payload.as_object().expect("object");
            assert_eq!(obj.get("entry_index").and_then(|v| v.as_u64()), Some(7));
            assert!(obj.get("role").is_none());
            assert!(obj.get("preview").is_none());
        });
    }

    #[test]
    fn dirty_target_is_transcript_events_only() {
        use crate::store::SolutionAgentStoreEvent::*;
        let sid = crate::model::SolutionSessionId::new();
        // Transcript-advancing events signal a re-poll.
        assert_eq!(
            dirty_target_session(&SessionMessageAppended(sid, 3)),
            Some(sid)
        );
        assert_eq!(dirty_target_session(&SessionStateChanged(sid)), Some(sid));
        assert_eq!(dirty_target_session(&SessionQueueChanged(sid)), Some(sid));
        assert_eq!(
            dirty_target_session(&SessionSubagentsChanged(sid)),
            Some(sid)
        );
        // Pure lifecycle events do NOT — nothing for a client to re-fetch.
        assert_eq!(dirty_target_session(&SessionClosed(sid)), None);
        assert_eq!(
            dirty_target_session(&SessionCreated {
                id: sid,
                parent_session_id: None,
            }),
            None
        );
    }

    #[gpui::test]
    async fn dirty_payload_carries_session_id_and_current_seq(cx: &mut TestAppContext) {
        let (session_id, _acp_thread, _tmp) =
            crate::store::tests::create_session_with_thread(cx).await;
        cx.update(|cx| {
            let thread = {
                let store = SolutionAgentStore::global(cx);
                store
                    .read(cx)
                    .session(session_id)
                    .and_then(|s| s.read(cx).acp_thread().cloned())
            }
            .expect("thread");
            thread.update(cx, |thread, cx| {
                let chunk = agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hi".to_string()),
                );
                thread.push_user_content_block(None, chunk, cx);
            });
        });
        cx.executor().run_until_parked();

        cx.update(|cx| {
            let payload = build_session_dirty_payload(session_id, cx);
            let obj = payload.as_object().expect("object");
            assert_eq!(
                obj.get("session_id").and_then(|v| v.as_str()),
                Some(session_id.to_string().as_str())
            );
            assert!(
                obj.get("current_seq").and_then(|v| v.as_u64()).is_some(),
                "current_seq must be a u64: {payload}"
            );
        });
    }

    /// Build a text block carrying an `spk_client_send_id` stamp on its
    /// `_meta`, mirroring what the mobile client sends.
    fn stamped_text(text: &str, csid: i64) -> agent_client_protocol::schema::ContentBlock {
        let mut block = agent_client_protocol::schema::TextContent::new(text.to_string());
        let mut meta = serde_json::Map::new();
        meta.insert(
            acp_thread::SPK_CLIENT_SEND_ID_META_KEY.to_string(),
            serde_json::json!(csid),
        );
        block.meta = Some(meta);
        agent_client_protocol::schema::ContentBlock::Text(block)
    }

    fn image_block() -> agent_client_protocol::schema::ContentBlock {
        agent_client_protocol::schema::ContentBlock::Image(
            agent_client_protocol::schema::ImageContent::new(
                "AAAA".to_string(),
                "image/png".to_string(),
            ),
        )
    }

    #[gpui::test]
    async fn queue_changed_payload_summarises_mixed_bundle(cx: &mut TestAppContext) {
        let (session_id, _acp_thread, _tmp) =
            crate::store::tests::create_session_with_thread(cx).await;

        // Seed a single bundle mixing text (with two distinct csids) and two
        // image blocks. `image_count` must count ONLY Image blocks.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.pending_messages.push_back(crate::model::PendingBundle {
                    target: crate::model::QueueTarget::Main,
                    blocks: vec![
                        stamped_text("hello world", 111),
                        image_block(),
                        stamped_text("more", 222),
                        image_block(),
                    ],
                });
            });
        });

        cx.update(|cx| {
            let payload = build_queue_changed_payload(session_id, cx);
            let obj = payload.as_object().expect("object");
            assert_eq!(
                obj.get("session_id").and_then(|v| v.as_str()),
                Some(session_id.to_string().as_str())
            );
            let bundles = obj
                .get("bundles")
                .and_then(|v| v.as_array())
                .expect("bundles");
            assert_eq!(bundles.len(), 1, "one seeded bundle → one descriptor");
            let bundle = bundles[0].as_object().expect("bundle object");

            let csids: Vec<i64> = bundle
                .get("csids")
                .and_then(|v| v.as_array())
                .expect("csids")
                .iter()
                .filter_map(|v| v.as_i64())
                .collect();
            assert_eq!(csids, vec![111, 222], "csids in first-seen order, deduped");

            let preview = bundle
                .get("preview")
                .and_then(|v| v.as_str())
                .expect("preview");
            assert!(
                preview.contains("hello world") && preview.contains("more"),
                "preview should carry both text blocks: {preview}"
            );

            assert_eq!(
                bundle.get("image_count").and_then(|v| v.as_u64()),
                Some(2),
                "image_count counts ONLY image blocks, not all blocks"
            );
        });
    }

    #[gpui::test]
    async fn queue_changed_payload_empty_queue_emits_empty_bundles(cx: &mut TestAppContext) {
        // Mobile relies on `bundles: []` to clear synthetic Queued bubbles.
        let (session_id, _acp_thread, _tmp) =
            crate::store::tests::create_session_with_thread(cx).await;

        cx.update(|cx| {
            let payload = build_queue_changed_payload(session_id, cx);
            let obj = payload.as_object().expect("object");
            let bundles = obj
                .get("bundles")
                .and_then(|v| v.as_array())
                .expect("bundles");
            assert!(
                bundles.is_empty(),
                "empty queue must emit an empty bundles array"
            );
        });
    }

    #[gpui::test]
    async fn message_appended_payload_includes_created_ms(cx: &mut TestAppContext) {
        let (session_id, _acp_thread, _tmp) =
            crate::store::tests::create_session_with_thread(cx).await;

        // Append a user entry; `run_until_parked` lets the store handle the
        // `AcpThreadEvent::NewEntry` and stamp `entries[0].created_ms`.
        cx.update(|cx| {
            let thread = {
                let store = SolutionAgentStore::global(cx);
                store
                    .read(cx)
                    .session(session_id)
                    .and_then(|s| s.read(cx).acp_thread().cloned())
            }
            .expect("thread");
            thread.update(cx, |thread, cx| {
                thread.push_user_content_block(
                    None,
                    agent_client_protocol::schema::ContentBlock::Text(
                        agent_client_protocol::schema::TextContent::new("hello".to_string()),
                    ),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();

        // Positive case: a real stamp must be surfaced as `created_ms > 0`.
        cx.update(|cx| {
            let payload = build_message_appended_payload(session_id, 0, cx);
            let obj = payload.as_object().expect("object");
            let created = obj.get("created_ms").and_then(|v| v.as_i64());
            assert!(
                created.is_some_and(|ms| ms > 0),
                "real stamp must be surfaced as created_ms > 0, got: {created:?}"
            );
        });

        // Absent case: when the index is beyond `entries` (no entry present),
        // the key must be omitted entirely.
        cx.update(|cx| {
            // Index 99 has no entry and no stamp.
            let payload = build_message_appended_payload(session_id, 99, cx);
            let obj = payload.as_object().expect("object");
            assert!(
                obj.get("created_ms").is_none(),
                "missing stamp must not emit created_ms key"
            );
        });

        // Sentinel case: manually set the stamp to NO_TIMESTAMP_MS and verify
        // the key is omitted.
        cx.update(|cx| {
            use crate::model::NO_TIMESTAMP_MS;
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session");
            session.update(cx, |s, _| {
                if let Some(e) = s.entries.get_mut(0) {
                    e.created_ms = NO_TIMESTAMP_MS;
                }
            });
        });
        cx.update(|cx| {
            let payload = build_message_appended_payload(session_id, 0, cx);
            let obj = payload.as_object().expect("object");
            assert!(
                obj.get("created_ms").is_none(),
                "sentinel NO_TIMESTAMP_MS must not emit created_ms key"
            );
        });
    }
}
