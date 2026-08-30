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
        SessionCreated { .. }
        | SessionClosed(_)
        | SessionNotified(..)
        | TabsChanged { .. }
        | ActiveDialogSessionChanged { .. }
        | BandStateChanged { .. } => None,
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
        //
        // `ActiveDialogSessionChanged` and `BandStateChanged` are
        // desktop-local UI state (which session the Solution band shows, and
        // the band's own geometry) with no mobile-client analogue — same
        // treatment as the background-agents/-shells events above.
        SolutionAgentStoreEvent::TabsChanged { .. }
        | SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(_)
        | SolutionAgentStoreEvent::SessionBackgroundShellsChanged(_)
        | SolutionAgentStoreEvent::ActiveDialogSessionChanged { .. }
        | SolutionAgentStoreEvent::BandStateChanged { .. } => {}
    }
}

/// Build the JSON payload for an `agent_session_message_appended`
/// notification. Pure function (no side effects) so unit tests can
/// assert wire shape without running an MCP server.
///
/// `flat_entry_index` is the position in `session.entries` — the flat,
/// un-coalesced ingest mirror the store's event arms address. **The payload's
/// `entry_index` is NOT that number.** It is the STREAM-LOCAL position, in the
/// same space `get_session` / `get_session_changes` / `get_session_entry`
/// serve, and it ships next to the `stream_id` it belongs to. That is
/// load-bearing rather than tidy: the mobile client chains this notification
/// straight into `get_session_entry(entry_index)` (R-5f diff streaming) and
/// caches with `newTotalCount = entry.index + 1` against a stream-local
/// `total_count`, so a flat index either lands on an already-rendered entry or
/// — once the two RPCs agree — falls off the end of a shorter stream with an
/// `entry_index_out_of_range`. `[main user, teammate assistant, main user]` is
/// enough to produce it.
///
/// The stream-local position is computed by demultiplexing the transcript
/// PREFIX that ends at this entry and taking that stream's resulting length.
/// Re-using `stream::demux` is deliberate: the coalescing rule lives in
/// `push_coalesced` alone and a re-derivation here would be a second copy of it
/// to keep in sync. It is emphatically NOT a `mod_seq` search — an in-place
/// `EntryUpdated` re-stamps `mod_seq`, so it is not monotonic across a stream.
///
/// Every field is read off `session.entries` / `session.streams`, never off the
/// live `AcpThread`. The old code mixed the two and indexed the thread with a
/// GLOBAL index, so on a resumed session (`live_base > 0`) the `role` and
/// `preview` described a different entry than `created_ms` did.
///
/// Falls back to a minimal `session_id`-only payload when the session is gone,
/// the index is out of range, or the entry belongs to a stream that is no
/// longer in the mirror (an auto-closed teammate): in every one of those cases
/// there is no stream-local index that would resolve, and shipping the flat one
/// is precisely the bug above. A consumer must treat `entry_index` as optional
/// and re-poll when it is absent.
pub(crate) fn build_message_appended_payload(
    session_id: crate::model::SolutionSessionId,
    flat_entry_index: usize,
    cx: &App,
) -> serde_json::Value {
    let resolved = SolutionAgentStore::try_global(cx).and_then(|store| {
        store.read_with(cx, |store, cx| {
            let session = store.session(session_id)?;
            let session_ref = session.read(cx);
            let flat_entry = session_ref.entries.get(flat_entry_index)?;
            let stream_id = match &flat_entry.subagent_id {
                None => crate::stream::StreamId::Main,
                Some(toolu) => crate::stream::StreamId::Teammate(toolu.clone()),
            };
            let prefix = crate::stream::demux(&session_ref.entries[..=flat_entry_index]);
            // `- 1`: the prefix ends AT this entry, so the stream's length after
            // demuxing it is one past this entry's own position. `demux` always
            // inserts Main and creates a teammate stream on first sight of its
            // tag, so the lookup can only miss if `entries` changed under us.
            let index = prefix.get(&stream_id)?.entries.len().checked_sub(1)?;
            // Describe the FULLY coalesced entry from the live mirror — byte-for
            // -byte the one `get_session_entry { index, stream_id }` will serve
            // when the client chains this notification into it. The prefix demux
            // above answers WHERE, not WHAT: its last entry is missing any
            // fragments that arrived after this one.
            let entry = session_ref.streams.get(&stream_id)?.entries.get(index)?;
            let role = crate::mcp::entry_role(&entry.kind);
            let preview =
                truncate_preview(&crate::mcp::session_entry_to_markdown(&entry.kind), 200);
            // Only user messages can carry originating-client send ids (stamped
            // on each content block's `_meta` by the client); other roles get an
            // empty Vec. Read through the same `csids_from_blocks` every other
            // surface uses, off the retained `acp::ContentBlock`s, so this no
            // longer needs the live thread at all.
            let client_send_ids = match &entry.kind {
                crate::session_entry::SessionEntryKind::UserMessage { chunks, .. } => {
                    acp_thread::csids_from_blocks(chunks)
                }
                _ => Vec::new(),
            };
            let created_ms = (entry.created_ms > 0).then_some(entry.created_ms);
            Some((
                crate::mcp::StreamIdDto::from_model(&stream_id),
                index,
                role,
                preview,
                client_send_ids,
                created_ms,
            ))
        })
    });
    let Some((stream_id, index, role, preview, client_send_ids, created_ms)) = resolved else {
        return json!({ "session_id": session_id.to_string() });
    };
    let mut obj = json!({
        "session_id": session_id.to_string(),
        "entry_index": index,
        "stream_id": stream_id,
        "role": role,
        "preview": preview,
    });
    if let Some(first) = client_send_ids.first() {
        // Back-compat alias for pre-R6h mobile builds that only know the
        // singular field. Always the FIRST csid so the legacy "pop one" path
        // keeps working.
        obj["client_send_id"] = json!(first);
        obj["client_send_ids"] = json!(client_send_ids);
    }
    if let Some(ms) = created_ms {
        obj["created_ms"] = json!(ms);
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
/// notification. Since wire v5 this is a lean `{session_id}`-only dirty-poke:
/// a teammate's friendly label now rides `StreamDto.label`, so the consumer
/// just re-polls `streams` on a teammate register/deregister rather than
/// applying a subagent list off this notification.
pub(crate) fn build_active_subagents_changed_payload(
    session_id: crate::model::SolutionSessionId,
    _cx: &App,
) -> serde_json::Value {
    json!({
        "session_id": session_id.to_string(),
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

    /// The fallback payload is `session_id` ALONE. It used to echo the flat
    /// index back, which is the one thing it must not do now that the client
    /// chains `entry_index` into `get_session_entry`: an unresolvable entry has
    /// no stream-local index, and shipping the flat one is the bug this whole
    /// change is about.
    #[gpui::test]
    async fn message_appended_payload_falls_back_when_session_missing(cx: &mut TestAppContext) {
        let registry = Arc::new(AdapterRegistry::new());
        cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

        cx.update(|cx| {
            let payload =
                build_message_appended_payload(crate::model::SolutionSessionId::new(), 7, cx);
            let obj = payload.as_object().expect("object");
            assert_eq!(
                obj.get("session_id").and_then(|v| v.as_str()).is_some(),
                true
            );
            assert!(
                obj.get("entry_index").is_none(),
                "no flat index may leak into the fallback; got {obj:?}"
            );
            assert!(obj.get("stream_id").is_none());
            assert!(obj.get("role").is_none());
            assert!(obj.get("preview").is_none());
        });
    }

    // =============================================================
    // The notification's `entry_index` is STREAM-LOCAL, and chains
    // into `get_session_entry` — the mobile client's R-5f diff-
    // streaming path (`docs/plans/2026-05-17-remote-control-R5f-
    // client-rich-rendering.md`).
    // =============================================================

    fn assistant_entry(text: &str, toolu: Option<&str>) -> crate::session_entry::SessionEntry {
        crate::session_entry::SessionEntry {
            created_ms: 1_700_000_000_000,
            mod_seq: 0,
            subagent_id: toolu.map(|t| gpui::SharedString::from(t.to_string())),
            kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                chunks: vec![crate::session_entry::AssistantChunk::Message(
                    text.to_string(),
                )],
            },
        }
    }

    fn user_entry(text: &str) -> crate::session_entry::SessionEntry {
        crate::session_entry::SessionEntry {
            created_ms: 1_700_000_000_000,
            mod_seq: 0,
            subagent_id: None,
            kind: crate::session_entry::SessionEntryKind::UserMessage {
                id: None,
                content_md: text.to_string(),
                chunks: vec![],
            },
        }
    }

    fn set_transcript(
        session_id: crate::model::SolutionSessionId,
        cx: &mut TestAppContext,
        entries: Vec<crate::session_entry::SessionEntry>,
    ) {
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.entries = entries.into_iter().map(std::sync::Arc::new).collect();
                s.rebuild_streams();
            });
        });
    }

    /// Follow the notification the way the deployed client does: take its
    /// `stream_id` + `entry_index` verbatim and call `get_session_entry` with
    /// them. Returns the served entry's `(index, markdown)`.
    async fn chase_notification(
        cx: &mut TestAppContext,
        session_id: crate::model::SolutionSessionId,
        payload: &serde_json::Value,
    ) -> (usize, String) {
        use context_server::listener::McpServerTool;
        let stream_id: crate::mcp::StreamIdDto =
            serde_json::from_value(payload["stream_id"].clone()).expect("stream_id round-trips");
        let index = payload["entry_index"].as_u64().expect("entry_index") as usize;
        let entry = crate::mcp::GetSessionEntryTool
            .run(
                crate::mcp::GetSessionEntryParams {
                    session_id: session_id.to_string(),
                    index,
                    stream_id: Some(stream_id),
                    include_images: false,
                },
                &mut cx.to_async(),
            )
            .await
            .unwrap_or_else(|err| {
                panic!("the client chases entry_index {index} into get_session_entry: {err:#}")
            })
            .structured_content
            .entry;
        (
            entry.index,
            entry.markdown.expect("single-entry always has markdown"),
        )
    }

    /// `[main user, teammate assistant, main user]` — an ordinary session that
    /// dispatched one teammate. The third append is flat index 2 while Main
    /// holds 2 entries, so the flat index the payload used to carry now falls
    /// off the end of the stream the client renders.
    #[gpui::test]
    async fn message_appended_payload_is_stream_local_for_a_teammate_transcript(
        cx: &mut TestAppContext,
    ) {
        let (session_id, _thread, _tmp) = crate::store::tests::create_session_with_thread(cx).await;
        set_transcript(
            session_id,
            cx,
            vec![
                user_entry("main question"),
                assistant_entry("teammate work", Some("toolu_note_1")),
                user_entry("main follow-up"),
            ],
        );

        let payloads: Vec<serde_json::Value> = cx.update(|cx| {
            (0..3)
                .map(|flat| build_message_appended_payload(session_id, flat, cx))
                .collect()
        });

        assert_eq!(payloads[0]["entry_index"].as_u64(), Some(0));
        assert_eq!(
            payloads[0]["stream_id"],
            serde_json::json!({"type": "main"})
        );
        assert_eq!(payloads[0]["role"].as_str(), Some("user"));

        assert_eq!(
            payloads[1]["entry_index"].as_u64(),
            Some(0),
            "a teammate's first entry is index 0 of ITS stream, not 1 of the flat mirror"
        );
        assert_eq!(
            payloads[1]["stream_id"],
            serde_json::json!({"type": "teammate", "toolu": "toolu_note_1"})
        );
        assert_eq!(payloads[1]["role"].as_str(), Some("assistant"));

        assert_eq!(
            payloads[2]["entry_index"].as_u64(),
            Some(1),
            "the third append is Main index 1 — the flat index 2 is off the end of Main"
        );
        assert_eq!(
            payloads[2]["stream_id"],
            serde_json::json!({"type": "main"})
        );

        // And the whole point: every one of those chases cleanly into the RPC
        // the client calls next, landing on the entry the payload previewed.
        for (flat, expect) in [
            (0usize, "main question"),
            (1, "teammate work"),
            (2, "main follow-up"),
        ] {
            let payload = payloads[flat].clone();
            let (index, markdown) = chase_notification(cx, session_id, &payload).await;
            assert_eq!(index, payload["entry_index"].as_u64().unwrap() as usize);
            assert!(
                markdown.contains(expect),
                "flat {flat} previewed {:?} but get_session_entry served {markdown:?}",
                payload["preview"]
            );
            let preview = payload["preview"].as_str().expect("preview");
            assert!(
                markdown.starts_with(preview.trim_end_matches('\u{2026}')),
                "the payload's preview must be a prefix of the entry it points at; \
                 preview={preview:?} markdown={markdown:?}"
            );
        }
    }

    /// Two consecutive assistant fragments are ONE stream entry, so the second
    /// fragment's append must re-advertise the SAME index (with the merged
    /// preview), not a new one.
    #[gpui::test]
    async fn message_appended_payload_reports_the_coalesced_entry(cx: &mut TestAppContext) {
        let (session_id, _thread, _tmp) = crate::store::tests::create_session_with_thread(cx).await;
        // Distinct stamps: `push_coalesced` keeps the FIRST fragment's
        // `created_ms` on the merged entry, so the payload must report that one
        // for BOTH fragments' appends — the same value `get_session` serves.
        let mut fragment_b = assistant_entry("fragment b", None);
        fragment_b.created_ms = 1_700_000_009_000;
        set_transcript(
            session_id,
            cx,
            vec![
                assistant_entry("fragment a", None),
                fragment_b,
                user_entry("a question"),
            ],
        );

        let payloads: Vec<serde_json::Value> = cx.update(|cx| {
            (0..3)
                .map(|flat| build_message_appended_payload(session_id, flat, cx))
                .collect()
        });

        assert_eq!(payloads[0]["entry_index"].as_u64(), Some(0));
        assert_eq!(
            payloads[1]["entry_index"].as_u64(),
            Some(0),
            "the second fragment merged into the first entry — same index"
        );
        for flat in [0usize, 1] {
            let preview = payloads[flat]["preview"].as_str().expect("preview");
            assert!(
                preview.contains("fragment a") && preview.contains("fragment b"),
                "the preview describes the fully coalesced entry; got {preview:?}"
            );
        }
        assert_eq!(
            payloads[2]["entry_index"].as_u64(),
            Some(1),
            "the user message is index 1 of Main, not 2"
        );
        for flat in [0usize, 1] {
            assert_eq!(
                payloads[flat]["created_ms"].as_i64(),
                Some(1_700_000_000_000),
                "the merged entry keeps the FIRST fragment's stamp; flat {flat} got {:?}",
                payloads[flat]["created_ms"]
            );
        }

        let payload = payloads[1].clone();
        let (index, markdown) = chase_notification(cx, session_id, &payload).await;
        assert_eq!(index, 0);
        assert!(markdown.contains("fragment a") && markdown.contains("fragment b"));
    }

    /// `client_send_id(s)` used to be read off the live `AcpThread`'s
    /// `UserMessage`; they now come from the stored entry's retained
    /// `acp::ContentBlock`s through the same `csids_from_blocks` every other
    /// surface uses. The client pops its optimistic bubbles off these, so
    /// losing them in the rewrite would have been silent.
    #[gpui::test]
    async fn message_appended_payload_carries_client_send_ids(cx: &mut TestAppContext) {
        let (session_id, _thread, _tmp) = crate::store::tests::create_session_with_thread(cx).await;
        let mut stamped = user_entry("from the phone");
        if let crate::session_entry::SessionEntryKind::UserMessage { chunks, .. } =
            &mut stamped.kind
        {
            *chunks = vec![
                stamped_text("from the phone", 42),
                stamped_text("and again", 43),
            ];
        }
        set_transcript(
            session_id,
            cx,
            vec![stamped, user_entry("from the desktop")],
        );

        let stamped_payload = cx.update(|cx| build_message_appended_payload(session_id, 0, cx));
        assert_eq!(stamped_payload["client_send_id"].as_i64(), Some(42));
        assert_eq!(
            stamped_payload["client_send_ids"],
            serde_json::json!([42, 43])
        );

        let plain = cx.update(|cx| build_message_appended_payload(session_id, 1, cx));
        assert!(
            plain["client_send_id"].is_null() && plain["client_send_ids"].is_null(),
            "an unstamped message carries neither key; got {plain:?}"
        );
    }

    /// A resumed session offsets the live thread's local indices by
    /// `live_base`. The payload is built entirely from `session.entries` /
    /// `session.streams` now, so the cold prefix is described correctly — the
    /// old code indexed `acp_thread.entries()` with the GLOBAL index and
    /// described a different entry, or none at all.
    #[gpui::test]
    async fn message_appended_payload_is_correct_across_a_live_base_offset(
        cx: &mut TestAppContext,
    ) {
        let (session_id, acp_thread, _tmp) =
            crate::store::tests::create_session_with_thread(cx).await;
        // Two cold entries, then re-attach the thread so `live_base` = 2.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session");
            session.update(cx, |s, cx| {
                s.entries = vec![
                    std::sync::Arc::new(user_entry("cold user")),
                    std::sync::Arc::new(assistant_entry("cold assistant", None)),
                ];
                s.rebuild_streams();
                s.set_acp_thread(Some(acp_thread.clone()), cx);
            });
        });
        cx.executor().run_until_parked();
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session");
            assert_eq!(session.read(cx).live_base, 2, "cold prefix length");
        });

        // One live append lands at global index 2 / local index 0.
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                thread.push_user_content_block(
                    None,
                    agent_client_protocol::schema::ContentBlock::Text(
                        agent_client_protocol::schema::TextContent::new("live user".to_string()),
                    ),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();

        let cold = cx.update(|cx| build_message_appended_payload(session_id, 1, cx));
        assert_eq!(
            cold["role"].as_str(),
            Some("assistant"),
            "flat 1 is the cold assistant; the live thread's entry 1 does not exist"
        );
        assert!(
            cold["preview"]
                .as_str()
                .is_some_and(|p| p.contains("cold assistant")),
            "got {:?}",
            cold["preview"]
        );
        assert_eq!(cold["entry_index"].as_u64(), Some(1));

        let live = cx.update(|cx| build_message_appended_payload(session_id, 2, cx));
        assert_eq!(live["role"].as_str(), Some("user"));
        assert!(
            live["preview"]
                .as_str()
                .is_some_and(|p| p.contains("live user")),
            "got {:?}",
            live["preview"]
        );
        assert_eq!(live["entry_index"].as_u64(), Some(2));

        let (index, markdown) = chase_notification(cx, session_id, &live).await;
        assert_eq!(index, 2);
        assert!(markdown.contains("live user"));
    }

    /// An entry whose teammate stream has been auto-closed sits in NO stream,
    /// so there is no stream-local index to advertise: the payload degrades to
    /// the `session_id`-only form rather than inventing one.
    #[gpui::test]
    async fn message_appended_payload_omits_the_index_for_a_streamless_entry(
        cx: &mut TestAppContext,
    ) {
        let (session_id, _thread, _tmp) = crate::store::tests::create_session_with_thread(cx).await;
        set_transcript(
            session_id,
            cx,
            vec![
                user_entry("main question"),
                assistant_entry("teammate work", Some("toolu_closed_1")),
            ],
        );
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session");
            session.update(cx, |s, _| {
                s.close_stream(
                    crate::stream::StreamId::Teammate(gpui::SharedString::from("toolu_closed_1")),
                    gpui::SharedString::from("done"),
                );
            });
        });

        let payload = cx.update(|cx| build_message_appended_payload(session_id, 1, cx));
        assert!(
            payload["entry_index"].is_null() && payload["stream_id"].is_null(),
            "a streamless entry advertises no index; got {payload:?}"
        );
        assert!(payload["session_id"].as_str().is_some());
        // Main is unaffected.
        let main = cx.update(|cx| build_message_appended_payload(session_id, 0, cx));
        assert_eq!(main["entry_index"].as_u64(), Some(0));
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
                    std::sync::Arc::make_mut(e).created_ms = NO_TIMESTAMP_MS;
                }
                // The payload is built from the coalesced `streams` mirror now,
                // so a direct `entries` poke has to refresh it — production
                // never mutates `entries` without a rebuild.
                s.rebuild_streams();
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
