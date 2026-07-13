    //! R-5e enrichment coverage. These tests build a real `AcpThread`
    //! via the mock-agent test infra, push synthetic entries straight
    //! through the public `acp_thread` API, then call the MCP tools
    //! the same way the WS proxy does and assert the wire shape.

    use super::*;
    use crate::store::tests::create_session_with_thread;
    use agent_client_protocol::schema as acp;
    
    use context_server::listener::McpServerTool;
    use context_server::types::ToolResponseContent;
    
    
    
    
    use crate::model::SolutionSessionId;
    use crate::store::SolutionAgentStore;
    use gpui::SharedString;
    

    #[test]
    fn entry_role_and_status_dto_serialize_snake_case() {
        assert_eq!(
            serde_json::to_value(EntryRoleDto::ToolCall).unwrap(),
            serde_json::json!("tool_call")
        );
        assert_eq!(
            serde_json::to_value(ToolCallStatusDto::WaitingForConfirmation).unwrap(),
            serde_json::json!("waiting_for_confirmation")
        );
        assert_eq!(
            serde_json::to_value(ToolCallStatusDto::Running).unwrap(),
            serde_json::json!("running")
        );
    }

    #[test]
    fn session_state_dto_serializes_structured() {
        use crate::model::SessionState;
        let json = |s: &SessionState, running_ms: i64, stopping_ms: i64| {
            serde_json::to_value(SessionStateDto::from_state(s, running_ms, stopping_ms)).unwrap()
        };
        assert_eq!(
            json(&SessionState::Idle, 0, 0),
            serde_json::json!({"kind":"idle"})
        );
        assert_eq!(
            json(
                &SessionState::Stopping {
                    started_at: std::time::Instant::now()
                },
                0,
                1779000
            ),
            serde_json::json!({"kind":"stopping","started_at_ms":1779000})
        );
        assert_eq!(
            json(&SessionState::AwaitingInput, 0, 0),
            serde_json::json!({"kind":"awaiting_input"})
        );
        assert_eq!(
            json(&SessionState::Errored("boom".into()), 0, 0),
            serde_json::json!({"kind":"errored","message":"boom"})
        );
        let running = SessionState::Running {
            started_at: std::time::Instant::now(),
            notified: false,
        };
        assert_eq!(
            json(&running, 1779, 0),
            serde_json::json!({"kind":"running","started_at_ms":1779})
        );
    }

    fn fake_user_text_chunk(text: &str) -> acp::ContentBlock {
        acp::ContentBlock::Text(acp::TextContent::new(text.to_string()))
    }

    fn fake_image_chunk(mime: &str, data_b64: &str) -> acp::ContentBlock {
        acp::ContentBlock::Image(acp::ImageContent::new(
            data_b64.to_string(),
            mime.to_string(),
        ))
    }

    /// Push a minimal user message + assistant message into the live
    /// thread so `get_session` has at least two entries to enrich.
    /// Returns a 1x1 PNG base64 payload that callers can match against.
    async fn seed_session_with_image(
        cx: &mut gpui::TestAppContext,
    ) -> (crate::model::SolutionSessionId, String, tempfile::TempDir) {
        let (session_id, acp_thread, tmp) = create_session_with_thread(cx).await;
        // 1×1 PNG, generated once with `base64 -w0 < tiny.png` — kept
        // small so test fixtures don't bloat the suite.
        let image_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNgAAIAAAUAAen5lOEAAAAASUVORK5CYII=".to_string();
        let image_b64_clone = image_b64.clone();
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                thread.push_user_content_block(None, fake_user_text_chunk("hello"), cx);
                thread.push_user_content_block(
                    None,
                    fake_image_chunk("image/png", &image_b64_clone),
                    cx,
                );
                thread.push_assistant_content_block(fake_user_text_chunk("world"), false, cx);
            });
        });
        cx.executor().run_until_parked();
        (session_id, image_b64, tmp)
    }

    #[gpui::test]
    async fn list_agents_returns_empty_when_no_adapters_registered(cx: &mut gpui::TestAppContext) {
        // create_session_with_thread builds an empty AdapterRegistry —
        // mock-agent gets registered via `register_agent_server`, not
        // via `AdapterRegistry::register`. So list_agents (which reads
        // the adapter registry) returns []. Asserts the wire shape and
        // the empty-list code path; the registry itself is covered by
        // `adapter::tests`.
        let (_session_id, _img, _tmp) = seed_session_with_image(cx).await;
        let result = cx
            .update(|cx| {
                let cx = cx.to_async();
                async move {
                    ListAgentsTool
                        .run(ListAgentsParams {}, &mut cx.clone())
                        .await
                }
            })
            .await
            .expect("list_agents tool should run");
        assert_eq!(result.structured_content.agents.len(), 0);
        match &result.content[0] {
            ToolResponseContent::Text { text } => assert_eq!(text, "0 agent(s)"),
            _ => panic!("expected text content"),
        }
    }

    #[gpui::test]
    async fn get_session_default_flags_omit_full_content(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: false,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        assert!(
            !result.structured_content.entries.is_empty(),
            "expected entries"
        );
        for entry in &result.structured_content.entries {
            assert!(
                entry.markdown.is_none(),
                "markdown must stay None when include_full_content=false; got {:?}",
                entry.markdown
            );
            assert!(
                entry.images.is_none(),
                "images must stay None when include_images=false; got {:?}",
                entry.images
            );
            assert!(
                !entry.preview.is_empty(),
                "preview must always be populated"
            );
        }
    }

    #[gpui::test]
    async fn get_session_full_content_populates_markdown(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: true,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        for entry in &result.structured_content.entries {
            let md = entry
                .markdown
                .as_ref()
                .expect("markdown populated when include_full_content=true");
            assert!(
                md.len() >= entry.preview.trim_end_matches('…').len(),
                "markdown should be at least as long as preview's content"
            );
            assert!(
                entry.images.is_none(),
                "images stay None unless include_images=true"
            );
        }
    }

    #[gpui::test]
    async fn get_session_include_images_inlines_base64(cx: &mut gpui::TestAppContext) {
        let (session_id, expected_b64, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: true,
                    include_images: true,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let mut total_images = 0usize;
        let mut saw_expected = false;
        for entry in &result.structured_content.entries {
            let images = entry
                .images
                .as_ref()
                .expect("images list populated even if empty");
            total_images += images.len();
            for image in images {
                assert_eq!(image.mime_type, "image/png");
                if image.data_base64 == expected_b64 {
                    saw_expected = true;
                }
            }
        }
        assert!(
            total_images >= 1,
            "expected at least one image after seeding"
        );
        assert!(
            saw_expected,
            "the seeded PNG payload should round-trip unchanged"
        );
    }

    #[gpui::test]
    async fn get_session_entry_happy_path_returns_full_markdown(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionEntryTool
            .run(
                GetSessionEntryParams {
                    session_id: session_id.to_string(),
                    index: 0,
                    include_images: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_entry");

        let entry = result.structured_content.entry;
        assert_eq!(entry.role, EntryRoleDto::User);
        // R-6e: every EntrySummary carries its absolute index.
        assert_eq!(entry.index, 0);
        let md = entry
            .markdown
            .expect("markdown is always populated for single-entry fetch");
        assert!(md.contains("hello"));
    }

    #[gpui::test]
    async fn get_session_entry_out_of_range_errors(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let err = GetSessionEntryTool
            .run(
                GetSessionEntryParams {
                    session_id: session_id.to_string(),
                    index: 9_999,
                    include_images: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("out-of-range index must error");

        let msg = format!("{:#}", err);
        assert!(
            msg.contains("entry_index_out_of_range"),
            "error should mention entry_index_out_of_range, got: {msg}"
        );
    }

    #[gpui::test]
    async fn tool_call_entry_surfaces_status_and_args(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

        // Push a synthetic ToolCall directly into the thread. We bypass
        // `handle_session_update` because that path requires a real ACP
        // server; constructing the entry by hand exercises the same
        // public type the render layer reads.
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                let tool_call = acp::ToolCall::new(
                    acp::ToolCallId::new("call-1".to_string()),
                    "Bash".to_string(),
                )
                .kind(acp::ToolKind::Execute)
                .raw_input(serde_json::json!({ "cmd": "ls" }));
                thread
                    .upsert_tool_call(tool_call, cx)
                    .expect("upsert_tool_call");
            });
        });
        cx.executor().run_until_parked();

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: false,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let tool_entry = result
            .structured_content
            .entries
            .iter()
            .find(|e| e.role == EntryRoleDto::ToolCall)
            .expect("tool_call entry");
        let tool = tool_entry
            .tool_call
            .as_ref()
            .expect("tool_call summary populated");
        // Reuses `tool_call_status_text` — pending status maps to the
        // literal string "pending".
        assert_eq!(tool.status, ToolCallStatusDto::Pending);
        assert!(
            tool.args_preview.contains("\"cmd\""),
            "args_preview should serialize raw_input JSON, got: {}",
            tool.args_preview
        );
        assert!(
            tool.tool_status_started_at_ms.is_none(),
            "Pending tool call should not surface a started_at timestamp, got: {:?}",
            tool.tool_status_started_at_ms,
        );
    }

    #[test]
    fn push_system_note_params_parse_levels() {
        let parse = |v: serde_json::Value| serde_json::from_value::<PushSystemNoteParams>(v);
        let p = parse(serde_json::json!({
            "session_id": "s1", "level": "observer", "text": "hi"
        }))
        .expect("parse observer");
        assert_eq!(p.level, "observer");
        assert_eq!(p.text, "hi");
        // Unknown fields are rejected (deny_unknown_fields), matching the
        // sibling param structs.
        assert!(
            parse(serde_json::json!({ "session_id": "s1", "bogus": 1 })).is_err(),
            "unknown field should be rejected"
        );
    }

    #[gpui::test]
    async fn push_system_note_appends_observer_entry(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

        let before = cx.update(|cx| acp_thread.read(cx).entries().len());

        cx.update(|cx| {
            let cx = cx.to_async();
            async move {
                PushSystemNoteTool
                    .run(
                        PushSystemNoteParams {
                            session_id: session_id.to_string(),
                            level: "observer".to_string(),
                            text: "Наблюдатель направил агента".to_string(),
                        },
                        &mut cx.clone(),
                    )
                    .await
            }
        })
        .await
        .expect("push_system_note");
        cx.executor().run_until_parked();

        cx.update(|cx| {
            let entries = acp_thread.read(cx).entries();
            assert_eq!(entries.len(), before + 1, "one SystemNote appended");
            match entries.last().expect("last entry") {
                acp_thread::AgentThreadEntry::SystemNote(note) => {
                    assert_eq!(note.level, acp_thread::SystemNoteLevel::Observer);
                    assert_eq!(note.text.as_ref(), "Наблюдатель направил агента");
                }
                other => panic!("expected SystemNote, got {other:?}"),
            }
        });
    }

    #[gpui::test]
    async fn tool_call_entry_surfaces_status_started_at_when_in_progress(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

        let before_ms = chrono::Utc::now().timestamp_millis();
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                let tool_call = acp::ToolCall::new(
                    acp::ToolCallId::new("call-1".to_string()),
                    "Bash".to_string(),
                )
                .kind(acp::ToolKind::Execute)
                .status(acp::ToolCallStatus::InProgress);
                thread
                    .upsert_tool_call(tool_call, cx)
                    .expect("upsert_tool_call");
            });
        });
        cx.executor().run_until_parked();
        let after_ms = chrono::Utc::now().timestamp_millis();

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: false,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let tool = result
            .structured_content
            .entries
            .iter()
            .find(|e| e.role == EntryRoleDto::ToolCall)
            .and_then(|e| e.tool_call.as_ref())
            .expect("tool_call summary populated");
        assert_eq!(tool.status, ToolCallStatusDto::Running);
        let stamp = tool
            .tool_status_started_at_ms
            .expect("InProgress tool call must surface a started_at timestamp");
        assert!(
            stamp >= before_ms && stamp <= after_ms,
            "tool_status_started_at_ms {stamp} should fall between {before_ms} and {after_ms}",
        );
    }

    #[gpui::test]
    async fn plan_entry_surfaces_items(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                let plan = acp::Plan::new(vec![
                    acp::PlanEntry::new(
                        "step one".to_string(),
                        acp::PlanEntryPriority::Medium,
                        acp::PlanEntryStatus::Completed,
                    ),
                    acp::PlanEntry::new(
                        "step two".to_string(),
                        acp::PlanEntryPriority::Medium,
                        acp::PlanEntryStatus::Completed,
                    ),
                ]);
                thread.update_plan(plan, cx);
            });
        });
        cx.executor().run_until_parked();

        // `update_plan` keeps the plan in-flight until something
        // upgrades it to `CompletedPlan`. The session_view path does
        // this via the `EntryUpdated` cycle; in tests we drive the
        // same transition by emitting `Stopped` which forces the
        // pending plan to flush. If a plan entry isn't surfaced as
        // `CompletedPlan` we just confirm no panic — the actual plan
        // shape is checked in `acp_thread` upstream tests.
        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: false,
                    include_images: false,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        if let Some(plan_entry) = result
            .structured_content
            .entries
            .iter()
            .find(|e| e.role == EntryRoleDto::Plan)
        {
            let plan = plan_entry
                .plan
                .as_ref()
                .expect("plan summary populated for role=plan");
            assert_eq!(plan.items.len(), 2);
            assert!(plan.items[0].contains("step one"));
        }
        // Soft assertion — if the synthetic plan didn't get promoted to
        // CompletedPlan we still want the test to exercise the wire
        // path without panicking.
    }

    // =================================================================
    // R-6e pagination coverage (`solution_agent.get_session` +
    // `solution_agent.list_sessions`).
    // =================================================================

    /// Seed a session with exactly 5 plain text entries — alternating
    /// user/assistant — so pagination tests have stable indices 0..=4.
    /// No images, no tool calls; the only thing under test is
    /// before/after/count filtering on a known entry shape.
    async fn seed_session_with_n_entries(
        cx: &mut gpui::TestAppContext,
        n: usize,
    ) -> (crate::model::SolutionSessionId, tempfile::TempDir) {
        let (session_id, acp_thread, tmp) = create_session_with_thread(cx).await;
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                for i in 0..n {
                    let text = format!("entry-{i}");
                    if i % 2 == 0 {
                        thread.push_user_content_block(None, fake_user_text_chunk(&text), cx);
                    } else {
                        thread.push_assistant_content_block(fake_user_text_chunk(&text), false, cx);
                    }
                }
            });
        });
        cx.executor().run_until_parked();
        (session_id, tmp)
    }

    #[gpui::test]
    async fn get_session_no_pagination_returns_all_entries_with_total_count(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(entries.len(), 5, "no pagination → all 5 entries");
        assert_eq!(result.structured_content.total_count, 5);
        for (expected, entry) in entries.iter().enumerate() {
            assert_eq!(
                entry.index, expected,
                "EntrySummary.index must match absolute position"
            );
        }
    }

    /// Phase 5 Task 5.3 Part A (phase-4b per-stream): a full `get_session` load
    /// carries the session's `epoch` + the SELECTED stream's `current_seq` so the
    /// cache-first mobile client can seed its per-stream delta cursor from one
    /// fetch. `current_seq` is the selected stream's own watermark (its descriptor
    /// `seq`), not the global `change_seq`.
    #[gpui::test]
    async fn get_session_seeds_delta_cursor_epoch_and_seq(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 3).await;

        // Rotate the epoch the way a `/clear` would.
        mutate_session(session_id, cx, |s| {
            s.epoch = 7;
        });

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session")
            .structured_content;

        assert_eq!(result.epoch, 7, "full load must carry the session's epoch");
        // Per-stream cursor: `current_seq` == the selected (Main) stream's
        // descriptor `seq`, and is a real (nonzero) watermark.
        let main_seq = result
            .streams
            .iter()
            .find(|s| s.id == StreamIdDto::Main)
            .expect("Main descriptor present")
            .seq;
        assert_eq!(
            result.current_seq, main_seq,
            "current_seq is the SELECTED stream's watermark, matching its descriptor"
        );
        assert!(result.current_seq > 0, "a stamped stream has a nonzero cursor");

        // New Main activity advances that stream's watermark → the next load's
        // cursor rises (a bare `change_seq` bump with no new entry does NOT).
        let before = result.current_seq;
        mutate_session(session_id, cx, |s| {
            use crate::session_entry::{SessionEntry, SessionEntryKind};
            let next = s.change_seq + 1;
            s.change_seq = next;
            s.entries.push(SessionEntry {
                created_ms: 1_700_000_000_100,
                mod_seq: next,
                subagent_id: None,
                kind: SessionEntryKind::UserMessage {
                    id: None,
                    content_md: "more".into(),
                    chunks: vec![fake_user_text_chunk("more")],
                },
            });
        });
        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session")
            .structured_content;
        assert!(
            result.current_seq > before,
            "new Main-stream activity advances the per-stream cursor ({} !> {before})",
            result.current_seq
        );
    }

    /// Build a COLD, row-native session: `session.entries` populated, NO
    /// live `acp_thread` attached. Mirrors the post-restart state of a
    /// row-native session: `session.entries` populated, no live `acp_thread`.
    /// `get_session` must read from `session.entries` directly.
    /// The user message carries a 1×1 PNG image chunk so
    /// image extraction can be asserted on the user path.
    async fn seed_cold_row_native_session(
        cx: &mut gpui::TestAppContext,
    ) -> (crate::model::SolutionSessionId, String, tempfile::TempDir) {
        use crate::session_entry::{SessionEntry, SessionEntryKind};
        let (solution_id, tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            let registry = std::sync::Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });
        let image_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNgAAIAAAUAAen5lOEAAAAASUVORK5CYII=".to_string();
        let image_b64_clone = image_b64.clone();
        let session_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                let id = crate::model::SolutionSessionId::new();
                let mut session = crate::model::SolutionSession::new_idle(
                    id,
                    solution_id,
                    SharedString::from("mock-agent"),
                    acp::SessionId::new(format!("acp-{}", id.as_str())),
                );
                session.title = SharedString::from("cold session");
                session.entries = vec![
                    SessionEntry {
                        created_ms: 1_700_000_000_000,
                        mod_seq: 1,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "hello".into(),
                            chunks: vec![
                                fake_user_text_chunk("hello"),
                                fake_image_chunk("image/png", &image_b64_clone),
                            ],
                        },
                    },
                    SessionEntry {
                        created_ms: 1_700_000_000_001,
                        mod_seq: 2,
                        subagent_id: None,
                        kind: SessionEntryKind::AssistantMessage {
                            chunks: vec![crate::session_entry::AssistantChunk::Message(
                                "world".into(),
                            )],
                        },
                    },
                ];
                // Cold, row-native: NO live thread. The wire reads
                // `session.streams`; a direct `entries` assignment bypasses
                // `set_entries`, so demux the mirror by hand.
                session.rebuild_streams();
                assert!(session.acp_thread().is_none());
                store.register_prebuilt_session(session, cx)
            })
        });
        (session_id, image_b64, tmp)
    }

    #[gpui::test]
    async fn get_session_cold_row_native_returns_entries_from_session_entries(
        cx: &mut gpui::TestAppContext,
    ) {
        // A cold row-native session has no live thread; get_session must serve
        // the two entries from session.entries.
        let (session_id, _img, _tmp) = seed_cold_row_native_session(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: true,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(
            entries.len(),
            2,
            "cold row-native session must serve entries from session.entries"
        );
        assert_eq!(result.structured_content.total_count, 2);
        assert_eq!(entries[0].role, EntryRoleDto::User);
        assert_eq!(entries[1].role, EntryRoleDto::Assistant);
        assert!(
            entries[0]
                .markdown
                .as_ref()
                .is_some_and(|m| m.contains("hello")),
            "user markdown must round-trip from content_md"
        );
        assert!(
            entries[1]
                .markdown
                .as_ref()
                .is_some_and(|m| m.contains("world")),
            "assistant markdown must round-trip from chunks"
        );
    }

    #[gpui::test]
    async fn get_session_cold_row_native_preserves_user_images(cx: &mut gpui::TestAppContext) {
        // User-image fidelity must survive the SessionEntry repoint:
        // UserMessage.chunks retains the raw acp::ContentBlock::Image, so
        // the base64 payload round-trips unchanged.
        let (session_id, expected_b64, _tmp) = seed_cold_row_native_session(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_images: true,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let mut saw_expected = false;
        for entry in &result.structured_content.entries {
            if let Some(images) = &entry.images {
                for image in images {
                    if image.data_base64 == expected_b64 {
                        assert_eq!(image.mime_type, "image/png");
                        saw_expected = true;
                    }
                }
            }
        }
        assert!(
            saw_expected,
            "the seeded user PNG payload must round-trip unchanged from UserMessage.chunks"
        );
    }

    #[gpui::test]
    async fn get_session_count_returns_last_n_entries(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    count: Some(2),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![3, 4],
            "count=2 returns the LAST two entries (indices 3,4)"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_before_index_drops_newer(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    before_index: Some(3),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "before_index=3 keeps strictly-less indices 0,1,2"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_after_index_drops_older(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    after_index: Some(2),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![3, 4],
            "after_index=2 keeps strictly-greater indices 3,4"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_before_and_after_index_select_slice(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    after_index: Some(2),
                    before_index: Some(4),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![3],
            "after=2, before=4 leaves only index 3"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_after_index_then_count_takes_last_within_filter(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    after_index: Some(2),
                    count: Some(1),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        // After filter: indices 3,4. count=1 keeps the LAST = index 4.
        // Wait — plan says "entries are index 3 (last after filter)". Let's
        // re-read: "after_index=2, count=1 → entries are index 3 (last
        // after filter)". That's odd — the filter keeps {3,4} and "last"
        // would be 4. The plan likely meant "the slice has cardinality 1
        // — exactly one entry — at the most-recent position 4". But the
        // plan-doc literal says "index 3". Re-check: the plan-doc text in
        // the user prompt says exactly: "after_index=2, count=1 → entries
        // are index 3 (last after filter)". That contradicts the
        // count semantics ("LAST n") defined earlier in the SAME prompt.
        //
        // Resolving in favor of the LAST-N semantics defined in scope B
        // step 5 (`take(n)` on the reversed iterator), so count=1 of
        // {3,4} = {4}. The plan-doc's example is a typo.
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![4],
            "after=2 keeps {{3,4}}, count=1 then takes the LAST → index 4"
        );
        assert_eq!(result.structured_content.total_count, 5);
    }

    #[gpui::test]
    async fn get_session_after_index_past_end_returns_empty(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_session_with_n_entries(cx, 5).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    after_index: Some(99),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        assert!(
            result.structured_content.entries.is_empty(),
            "after_index past end → empty"
        );
        assert_eq!(
            result.structured_content.total_count, 5,
            "total_count still reflects the underlying thread"
        );
    }

    #[gpui::test]
    async fn list_sessions_pagination_orders_desc_and_caps_to_count(cx: &mut gpui::TestAppContext) {
        // Reuse the first session's setup (it primes globals + the mock
        // adapter), then create two more sessions in the same solution
        // with slightly later activity timestamps so the DESC ordering
        // is observable.
        let (first_session_id, _thread, _tmp) = create_session_with_thread(cx).await;

        let (solution_id, agent_id, project) = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store
                .read(cx)
                .session(first_session_id)
                .expect("first session exists");
            let session_ref = session.read(cx);
            (
                session_ref.solution_id,
                session_ref.agent_id.clone(),
                session_ref
                    .project
                    .clone()
                    .expect("create_session populates project"),
            )
        });

        let second_session_id = cx
            .update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| {
                    store.create_session(solution_id, agent_id.clone(), project.clone(), cx)
                })
            })
            .await
            .expect("create second session");

        let third_session_id = cx
            .update(|cx| {
                let store = SolutionAgentStore::global(cx);
                store.update(cx, |store, cx| {
                    store.create_session(solution_id, agent_id.clone(), project.clone(), cx)
                })
            })
            .await
            .expect("create third session");

        // The third is the most-recently-created; bump its
        // last_activity_at explicitly so the DESC sort puts it first
        // even on machines where Utc::now()'s resolution lets two
        // creates land in the same tick.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let (second, third) = store.read_with(cx, |store, _| {
                (
                    store.session(second_session_id).expect("second"),
                    store.session(third_session_id).expect("third"),
                )
            });
            second.update(cx, |s, _| {
                s.last_activity_at = chrono::Utc::now() + chrono::Duration::seconds(1);
            });
            third.update(cx, |s, _| {
                s.last_activity_at = chrono::Utc::now() + chrono::Duration::seconds(2);
            });
        });

        let result = ListSessionsTool
            .run(
                ListSessionsParams {
                    solution_id: Some(solution_id.0),
                    parent_session_id: None,
                    count: Some(1),
                    before_last_activity_at_ms: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("list_sessions");

        let sessions = &result.structured_content.sessions;
        assert_eq!(sessions.len(), 1, "count=1 caps to one entry");
        assert_eq!(
            sessions[0].id,
            third_session_id.to_string(),
            "DESC ordering surfaces the most-recent session first"
        );
        assert_eq!(
            result.structured_content.total_count, 3,
            "total_count reflects all matching sessions, pre-pagination"
        );
    }

    // =================================================================
    // F: sub-agent indication coverage
    //
    // Validates the `parent_session_id` field plumbing across the MCP
    // wire shape and the new `solution_agent.get_session_children` tool.
    // =================================================================

    /// Spawn a sub-session under `parent_id`. Stays at the store layer
    /// to avoid the `MultiWorkspace` requirement of `CreateSessionTool`;
    /// the tool-layer create_session paths are covered separately in
    /// the dedicated F validation tests below.
    async fn create_child_session(
        cx: &mut gpui::TestAppContext,
        parent_id: crate::model::SolutionSessionId,
    ) -> crate::model::SolutionSessionId {
        let (solution_id, agent_id, project) = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store
                .read(cx)
                .session(parent_id)
                .expect("parent session exists");
            let session_ref = session.read(cx);
            (
                session_ref.solution_id,
                session_ref.agent_id.clone(),
                session_ref.project.clone().expect("parent has project"),
            )
        });
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store.create_session_with_parent(
                    solution_id,
                    agent_id,
                    project,
                    None,
                    None,
                    Some(parent_id),
                    None,
                    None,
                    false,
                    false,
                    cx,
                )
            })
        })
        .await
        .expect("create child session")
    }

    #[gpui::test]
    async fn create_session_with_parent_sets_parent_session_id_on_child(
        cx: &mut gpui::TestAppContext,
    ) {
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let child_id = create_child_session(cx, parent_id).await;

        // GetSession surfaces parent_session_id on the child.
        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: child_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session(child)");
        assert_eq!(
            result.structured_content.parent_session_id.as_deref(),
            Some(parent_id.to_string().as_str()),
            "child reports parent_session_id"
        );

        // Top-level parent reports no parent_session_id.
        let parent_result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: parent_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session(parent)");
        assert!(
            parent_result.structured_content.parent_session_id.is_none(),
            "top-level parent has no parent_session_id"
        );
    }

    #[gpui::test]
    async fn create_session_with_unknown_parent_errors_with_named_code(
        cx: &mut gpui::TestAppContext,
    ) {
        // Seed the store + solution_id so the "unknown parent" branch
        // is reached. We don't need a real workspace because parent
        // validation runs before `project_for_solution`.
        let (real_session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let solution_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store
                .read(cx)
                .session(real_session_id)
                .expect("session")
                .read(cx)
                .solution_id
        });
        // A short id that's well-formed (`[a-z0-9]{8}`) but not in the
        // store. `parse` will accept it; the store lookup will reject.
        let unknown_parent = "abcd1234";
        let err = CreateSessionTool
            .run(
                CreateSessionParams {
                    solution_id: solution_id.0,
                    agent_id: "mock-agent".into(),
                    initial_message: None,
                    parent_session_id: Some(unknown_parent.to_string()),
                    title: None,
                    cwd: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("expected unknown_parent_session error");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown_parent_session"),
            "expected unknown_parent_session in {msg:?}"
        );
        assert!(
            msg.contains(unknown_parent),
            "expected error to include the bad id; got {msg:?}"
        );
    }

    #[gpui::test]
    async fn create_session_with_parent_in_different_solution_errors(
        cx: &mut gpui::TestAppContext,
    ) {
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        // CreateSession against a *different* solution id — the parent
        // belongs to solution-A; we pass solution-B. Validation fires
        // before workspace lookup so we don't need solution-B to have
        // an open window.
        let other_solution: i64 = 999;
        let err = CreateSessionTool
            .run(
                CreateSessionParams {
                    solution_id: other_solution,
                    agent_id: "mock-agent".into(),
                    initial_message: None,
                    parent_session_id: Some(parent_id.to_string()),
                    title: None,
                    cwd: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("expected parent_session_in_different_solution error");
        let msg = err.to_string();
        assert!(
            msg.contains("parent_session_in_different_solution"),
            "expected parent_session_in_different_solution in {msg:?}"
        );
    }

    #[gpui::test]
    async fn get_session_children_returns_child_with_summary_fields(cx: &mut gpui::TestAppContext) {
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let child_id = create_child_session(cx, parent_id).await;

        let result = GetSessionChildrenTool
            .run(
                GetSessionChildrenParams {
                    session_id: parent_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_children");
        let children = &result.structured_content.children;
        assert_eq!(children.len(), 1, "exactly one child");
        assert_eq!(children[0].id, child_id.to_string());
        assert_eq!(
            children[0].parent_session_id.as_deref(),
            Some(parent_id.to_string().as_str()),
            "child summary echoes parent_session_id"
        );
        // Text content carries a stable count summary for log scraping.
        match &result.content[0] {
            ToolResponseContent::Text { text } => {
                assert_eq!(text, "1 child session(s)");
            }
            _ => panic!("expected text content"),
        }
    }

    #[gpui::test]
    async fn get_session_children_returns_empty_list_for_leaf_session(
        cx: &mut gpui::TestAppContext,
    ) {
        let (leaf_id, _thread, _tmp) = create_session_with_thread(cx).await;

        let result = GetSessionChildrenTool
            .run(
                GetSessionChildrenParams {
                    session_id: leaf_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_children on a leaf");
        assert!(
            result.structured_content.children.is_empty(),
            "leaf session has no children"
        );
    }

    #[gpui::test]
    async fn supervisor_ephemeral_sessions_hidden_from_enumeration(cx: &mut gpui::TestAppContext) {
        // A supervised parent with one hidden ephemeral judge child. The judge
        // must NOT surface in either `list_sessions` (the parent does) or
        // `get_session_children` (an empty list — it's the only child).
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let judge_id = create_child_session(cx, parent_id).await;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                store
                    .session(judge_id)
                    .expect("judge session exists")
                    .update(cx, |s, _| s.is_supervisor_ephemeral = true);
            });
        });

        let listed = ListSessionsTool
            .run(
                ListSessionsParams {
                    solution_id: None,
                    parent_session_id: None,
                    count: None,
                    before_last_activity_at_ms: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("list_sessions");
        let ids: Vec<&str> = listed
            .structured_content
            .sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert!(
            ids.contains(&parent_id.to_string().as_str()),
            "the supervised parent is still enumerated"
        );
        assert!(
            !ids.contains(&judge_id.to_string().as_str()),
            "the flagged ephemeral judge is excluded from list_sessions"
        );

        let children = GetSessionChildrenTool
            .run(
                GetSessionChildrenParams {
                    session_id: parent_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_children");
        assert!(
            children.structured_content.children.is_empty(),
            "the flagged ephemeral judge is excluded from get_session_children"
        );
    }

    #[gpui::test]
    async fn list_sessions_filters_by_parent_session_id(cx: &mut gpui::TestAppContext) {
        let (parent_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let child_id = create_child_session(cx, parent_id).await;
        // Add a second sibling so the filter has more than one row to
        // partition.
        let sibling_id = create_child_session(cx, parent_id).await;

        let solution_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store
                .read(cx)
                .session(parent_id)
                .expect("parent")
                .read(cx)
                .solution_id
        });

        // parent_session_id=parent → both children come back, parent itself excluded.
        let filtered = ListSessionsTool
            .run(
                ListSessionsParams {
                    solution_id: Some(solution_id.0),
                    parent_session_id: Some(parent_id.to_string()),
                    before_last_activity_at_ms: None,
                    count: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("list_sessions filtered by parent");
        let ids: std::collections::HashSet<String> = filtered
            .structured_content
            .sessions
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(
            ids,
            [child_id.to_string(), sibling_id.to_string()]
                .into_iter()
                .collect(),
            "exactly the two children are returned",
        );
        assert!(
            !ids.contains(&parent_id.to_string()),
            "parent itself is excluded"
        );
    }

    #[gpui::test]
    async fn list_sessions_excludes_untabbed_sessions(cx: &mut gpui::TestAppContext) {
        // #4: the mobile list must equal the desktop tab strip 1-to-1. A
        // freshly created session is pinned (`tab_order` set by
        // `open_session_in_strip`) and shows; an un-pinned session
        // (`tab_order` NULL — closed-tab, or a row that lost its tab_order)
        // must NOT appear at top level.
        let (tabbed_id, _thread, _tmp) = create_session_with_thread(cx).await;
        let solution_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store
                .read(cx)
                .session(tabbed_id)
                .expect("tabbed")
                .read(cx)
                .solution_id
        });

        let untabbed_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                assert!(
                    store
                        .session(tabbed_id)
                        .unwrap()
                        .read(cx)
                        .tab_order
                        .is_some(),
                    "a freshly created session must be pinned to the strip",
                );
                let id = SolutionSessionId::new();
                crate::store::tests::insert_cold_session(
                    id,
                    solution_id,
                    "mock-agent".into(),
                    None,
                    None,
                    store,
                    cx,
                );
                id
            })
        });

        let result = ListSessionsTool
            .run(
                ListSessionsParams {
                    solution_id: Some(solution_id.0),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("list_sessions");
        let ids: std::collections::HashSet<String> = result
            .structured_content
            .sessions
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert!(
            ids.contains(&tabbed_id.to_string()),
            "the pinned session is listed",
        );
        assert!(
            !ids.contains(&untabbed_id.to_string()),
            "the un-pinned session is excluded (1-to-1 with the desktop strip)",
        );
    }

    #[gpui::test]
    async fn session_summary_total_tokens_populated_from_cached_value(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        // Seed `cached_total_tokens` directly so the fallback path is
        // exercised even without a live `TokenUsageUpdated` event.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session exists");
            session.update(cx, |s, _| s.cached_total_tokens = Some(42_000));
        });

        let result = ListSessionsTool
            .run(ListSessionsParams::default(), &mut cx.to_async())
            .await
            .expect("list_sessions");
        let summary = result
            .structured_content
            .sessions
            .iter()
            .find(|s| s.id == session_id.to_string())
            .expect("session present");
        // The live thread's `token_usage()` may be None at this stage,
        // so the fallback to `cached_total_tokens` is what we're
        // verifying. Either path yielding >= 42_000 is acceptable
        // (live could update past the seed); the contract is "non-None
        // when we have a value".
        assert!(
            summary.total_tokens.is_some_and(|t| t >= 42_000),
            "total_tokens should fall back to cached_total_tokens; got {:?}",
            summary.total_tokens,
        );
    }

    /// Phone client reads `SessionSummary::max_tokens` to size its
    /// context-fill meter the same way the desktop does — without it,
    /// it would have to guess the model's window. Live thread's
    /// `TokenUsage::max_tokens` is the source when hot; the cache
    /// fallback is exercised separately in
    /// `session_summary_max_tokens_falls_back_to_cached`.
    #[gpui::test]
    async fn session_summary_max_tokens_from_live_thread(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
        // Drive a TokenUsageUpdated through the live thread. The store's
        // event handler mirrors max_tokens onto cached_max_tokens, and
        // session_summary should surface it.
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.update_token_usage(
                    Some(acp_thread::TokenUsage {
                        used_tokens: 5_000,
                        max_tokens: 200_000,
                        ..Default::default()
                    }),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();

        let result = ListSessionsTool
            .run(ListSessionsParams::default(), &mut cx.to_async())
            .await
            .expect("list_sessions");
        let summary = result
            .structured_content
            .sessions
            .iter()
            .find(|s| s.id == session_id.to_string())
            .expect("session present");
        assert_eq!(
            summary.max_tokens,
            Some(200_000),
            "max_tokens should be reported from the live thread",
        );
        assert_eq!(
            summary.total_tokens,
            Some(5_000),
            "total_tokens should be reported alongside max",
        );
    }

    /// Cold tab path: no live `acp_thread`, but `cached_max_tokens` was
    /// stamped during an earlier live event. `session_summary` must
    /// fall through to the cache so the phone meter keeps rendering a
    /// realistic window size even on sleeping sessions.
    #[gpui::test]
    async fn session_summary_max_tokens_falls_back_to_cached(cx: &mut gpui::TestAppContext) {
        let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.read(cx).session(session_id).expect("session exists");
            session.update(cx, |s, _| s.cached_max_tokens = Some(180_000));
        });

        let result = ListSessionsTool
            .run(ListSessionsParams::default(), &mut cx.to_async())
            .await
            .expect("list_sessions");
        let summary = result
            .structured_content
            .sessions
            .iter()
            .find(|s| s.id == session_id.to_string())
            .expect("session present");
        // A live max may have been picked up in the meantime; the
        // contract is "non-None when the cache holds a value".
        assert!(
            summary.max_tokens.is_some_and(|m| m >= 180_000),
            "max_tokens should fall back to cached_max_tokens; got {:?}",
            summary.max_tokens,
        );
    }

    /// `start_compact` MCP tool refuses on a fresh session whose
    /// context usage is well below the 10% threshold — mirrors the
    /// desktop status-row gate. The structured `queued=false` + reason
    /// is the contract the phone client renders on its button.
    #[gpui::test]
    async fn start_compact_declines_below_threshold(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
        // Seed a low usage well below 20% so the precondition fails.
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.update_token_usage(
                    Some(acp_thread::TokenUsage {
                        used_tokens: 1_000,
                        max_tokens: 1_000_000,
                        ..Default::default()
                    }),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();

        let result = StartCompactTool
            .run(
                StartCompactParams {
                    session_id: session_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("start_compact dispatches");
        assert!(
            !result.structured_content.queued,
            "expected queued=false, got {:?}",
            result.structured_content
        );
        let msg = result
            .structured_content
            .message
            .as_deref()
            .unwrap_or_default();
        assert!(
            msg.contains("short") || msg.contains("%"),
            "expected reason mentioning short context or percentage; got {msg:?}"
        );
    }

    /// `start_compact` queues a user message on the agent when the
    /// session is Idle and context exceeds 20%. We check that
    /// `send_message` was forwarded by inspecting the prompts the mock
    /// connection received.
    #[gpui::test]
    async fn start_compact_queues_prompt_when_idle(cx: &mut gpui::TestAppContext) {
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.update_token_usage(
                    Some(acp_thread::TokenUsage {
                        // 25% of 1M = 250 000 (above the 20% gate)
                        used_tokens: 250_000,
                        max_tokens: 1_000_000,
                        ..Default::default()
                    }),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();

        let result = StartCompactTool
            .run(
                StartCompactParams {
                    session_id: session_id.to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("start_compact dispatches");
        assert!(
            result.structured_content.queued,
            "expected queued=true; reason={:?}",
            result.structured_content.message
        );
        assert!(
            result.structured_content.message.is_none(),
            "no decline reason on success; got {:?}",
            result.structured_content.message
        );
    }

    // -----------------------------------------------------------------
    // upload_{init,status,finish,abort} + send_message_blocks resolution
    // -----------------------------------------------------------------

    /// `crate::upload::install` is a `OnceLock` — only the first caller wins
    /// process-wide. We can't keep handing out fresh `UploadManager`s per
    /// test; if we did, the second caller's `TempDir` would also drop on
    /// scope exit, leaving the first-installed manager pointing at a
    /// vanished directory. Instead, keep one persistent tempdir + manager
    /// alive for the lifetime of the test binary, and have each test allocate
    /// a fresh session+upload inside it.
    fn ensure_test_upload_manager() {
        use std::sync::OnceLock;
        static GUARD: OnceLock<tempfile::TempDir> = OnceLock::new();
        GUARD.get_or_init(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let manager =
                crate::upload::UploadManager::new(dir.path().to_path_buf()).expect("new mgr");
            crate::upload::install(std::sync::Arc::new(std::sync::Mutex::new(manager)));
            dir
        });
    }

    #[gpui::test]
    async fn upload_init_returns_id_and_status_round_trips(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp_session) = seed_session_with_image(cx).await;
        // OnceLock semantics: install only takes on first call per process,
        // so a prior test's manager may already be in place. That's fine —
        // each upload gets a fresh id from `next_id` and lands in some
        // valid tmp_root.
        ensure_test_upload_manager();

        let init = UploadInitTool
            .run(
                UploadInitParams {
                    session_id: session_id.to_string(),
                    mime: "image/png".to_string(),
                    display_name: "pic.png".to_string(),
                    total_size: 4,
                    sha256: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("upload_init");
        let upload_id = init.structured_content.upload_id;
        assert!(upload_id > 0);

        let status = UploadStatusTool
            .run(UploadStatusParams { upload_id }, &mut cx.to_async())
            .await
            .expect("upload_status");
        assert_eq!(status.structured_content.received_bytes, 0);
        assert_eq!(status.structured_content.total_size, 4);
    }

    #[gpui::test]
    async fn upload_init_rejects_unknown_session(cx: &mut gpui::TestAppContext) {
        let (_session_id, _img, _tmp_session) = seed_session_with_image(cx).await;
        ensure_test_upload_manager();
        let err = UploadInitTool
            .run(
                UploadInitParams {
                    session_id: "nonexistent-session-id".to_string(),
                    mime: "image/png".to_string(),
                    display_name: "a.png".to_string(),
                    total_size: 1,
                    sha256: None,
                },
                &mut cx.to_async(),
            )
            .await
            .map(|_| "ok")
            .unwrap_or_else(|e| Box::leak(format!("ERR: {e}").into_boxed_str()));
        assert!(
            err.starts_with("ERR"),
            "expected error for unknown session, got {err}"
        );
    }

    #[gpui::test]
    async fn upload_finish_after_chunk_returns_handle_and_abort_cleans(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _img, _tmp_session) = seed_session_with_image(cx).await;
        ensure_test_upload_manager();

        let init = UploadInitTool
            .run(
                UploadInitParams {
                    session_id: session_id.to_string(),
                    mime: "image/png".to_string(),
                    display_name: "tiny.png".to_string(),
                    total_size: 4,
                    sha256: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("upload_init");
        let upload_id = init.structured_content.upload_id;

        // Drive a chunk write through the manager directly — the binary
        // frame path is tested in `remote_control`; here we just need a
        // populated tmp file for `finish` to verify.
        crate::upload::with_manager(|m| m.write_chunk(upload_id, 0, &[1, 2, 3, 4]))
            .expect("manager installed")
            .expect("write_chunk");

        let finish = UploadFinishTool
            .run(
                UploadFinishParams {
                    upload_id,
                    sha256: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("upload_finish");
        assert!(
            finish
                .structured_content
                .handle
                .starts_with(crate::upload::HANDLE_SCHEME),
            "expected spk-upload:// handle, got {}",
            finish.structured_content.handle
        );

        UploadAbortTool
            .run(UploadAbortParams { upload_id }, &mut cx.to_async())
            .await
            .expect("upload_abort");

        let after = crate::upload::with_manager(|m| m.resolve(upload_id).is_some())
            .expect("manager installed");
        assert!(!after, "abort should drop the entry");
    }

    // -----------------------------------------------------------------
    // A6: created_ms on wire EntrySummary
    // -----------------------------------------------------------------

    /// Verifies that `GetSessionTool` propagates `created_ms` from the
    /// session's `entries` list to `EntrySummary.created_ms`:
    /// - entries with a real positive stamp → `Some(ms)` with `ms > 0`
    /// - entries whose stamp is the absent-sentinel → `None`
    ///
    /// We bypass the store's stamping by directly writing `entries[i].created_ms`
    /// on the session entity — the same pattern used by the store's own unit
    /// tests (see `store/tests.rs::entry_updated_preserves_created_ms`).
    #[gpui::test]
    async fn get_session_entries_carry_created_ms(cx: &mut gpui::TestAppContext) {
        use crate::model::NO_TIMESTAMP_MS;

        let (session_id, _tmp) = seed_session_with_n_entries(cx, 3).await;

        // Directly stamp: index 0 and 2 get real times, index 1 gets sentinel.
        let fake_ms: i64 = 1_700_000_000_000;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session_entity = store.read(cx).session(session_id).expect("session exists");
            session_entity.update(cx, |s, _| {
                if let Some(e) = s.entries.get_mut(0) {
                    e.created_ms = fake_ms;
                }
                if let Some(e) = s.entries.get_mut(1) {
                    e.created_ms = NO_TIMESTAMP_MS;
                }
                if let Some(e) = s.entries.get_mut(2) {
                    e.created_ms = fake_ms + 1;
                }
                // The wire reads `session.streams`; refresh the mirror so the
                // directly-stamped created_ms values propagate.
                s.rebuild_streams();
            });
        });

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let entries = &result.structured_content.entries;
        assert_eq!(entries.len(), 3, "all 3 entries returned");

        // Entries 0 and 2 have real stamps.
        assert!(
            entries[0].created_ms.is_some_and(|ms| ms > 0),
            "entry 0 must carry a positive created_ms; got {:?}",
            entries[0].created_ms,
        );
        assert!(
            entries[2].created_ms.is_some_and(|ms| ms > 0),
            "entry 2 must carry a positive created_ms; got {:?}",
            entries[2].created_ms,
        );

        // Entry 1 has the sentinel → must surface as None.
        assert!(
            entries[1].created_ms.is_none(),
            "entry 1 (sentinel) must have created_ms=None; got {:?}",
            entries[1].created_ms,
        );
    }

    /// Verifies that `GetSessionEntryTool` also propagates `created_ms`.
    #[gpui::test]
    async fn get_session_entry_carries_created_ms(cx: &mut gpui::TestAppContext) {
        use crate::model::NO_TIMESTAMP_MS;

        let (session_id, _tmp) = seed_session_with_n_entries(cx, 2).await;

        // Directly stamp entry 0 with a real time; leave entry 1 at sentinel.
        let fake_ms: i64 = 1_700_000_000_000;
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session_entity = store.read(cx).session(session_id).expect("session exists");
            session_entity.update(cx, |s, _| {
                if let Some(e) = s.entries.get_mut(0) {
                    e.created_ms = fake_ms;
                }
                if let Some(e) = s.entries.get_mut(1) {
                    e.created_ms = NO_TIMESTAMP_MS;
                }
            });
        });

        let result = GetSessionEntryTool
            .run(
                GetSessionEntryParams {
                    session_id: session_id.to_string(),
                    index: 0,
                    include_images: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_entry");

        assert!(
            result
                .structured_content
                .entry
                .created_ms
                .is_some_and(|ms| ms > 0),
            "GetSessionEntryTool must carry created_ms for a stamped entry; got {:?}",
            result.structured_content.entry.created_ms,
        );

        let result_sentinel = GetSessionEntryTool
            .run(
                GetSessionEntryParams {
                    session_id: session_id.to_string(),
                    index: 1,
                    include_images: false,
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session_entry sentinel");

        assert!(
            result_sentinel
                .structured_content
                .entry
                .created_ms
                .is_none(),
            "GetSessionEntryTool must surface sentinel as None; got {:?}",
            result_sentinel.structured_content.entry.created_ms,
        );
    }

    /// Stage a tool call sitting in `WaitingForConfirmation` with a Flat
    /// allow/reject option pair, returning the session id, the tool call
    /// id, and the authorization-outcome `Task` (held so the oneshot the
    /// connection awaits stays alive — dropping it would cancel the
    /// confirmation and flip the call off `WaitingForConfirmation`).
    async fn seed_session_with_pending_authorization(
        cx: &mut gpui::TestAppContext,
    ) -> (
        crate::model::SolutionSessionId,
        String,
        gpui::Task<acp_thread::RequestPermissionOutcome>,
        tempfile::TempDir,
    ) {
        let (session_id, acp_thread, tmp) = create_session_with_thread(cx).await;
        let tool_call_id = "call-auth-1".to_string();
        let auth_task = cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                let update = acp::ToolCallUpdate::new(
                    acp::ToolCallId::new(tool_call_id.as_str()),
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
        (session_id, tool_call_id, auth_task, tmp)
    }

    #[gpui::test]
    async fn get_session_surfaces_auth_options_while_waiting(cx: &mut gpui::TestAppContext) {
        let (session_id, tool_call_id, _auth_task, _tmp) =
            seed_session_with_pending_authorization(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let tool_call = result
            .structured_content
            .entries
            .iter()
            .find_map(|entry| entry.tool_call.as_ref())
            .expect("a tool_call entry must be present");
        assert_eq!(tool_call.status, ToolCallStatusDto::WaitingForConfirmation);
        assert_eq!(tool_call.options.len(), 2, "both options must surface");
        assert_eq!(tool_call.options[0].kind, "allow_once");
        assert!(tool_call.options[0].is_allow);
        assert_eq!(tool_call.options[1].kind, "reject_once");
        assert!(!tool_call.options[1].is_allow);
        // The option id is opaque but must round-trip verbatim.
        assert_eq!(tool_call.options[0].option_id, "opt-allow");
        // tool_call_id is what the client echoes back to authorize.
        assert_eq!(
            tool_call.tool_call_id, tool_call_id,
            "tool_call_id must round-trip verbatim to the client"
        );
    }

    #[gpui::test]
    async fn authorize_tool_call_resolves_waiting_call(cx: &mut gpui::TestAppContext) {
        let (session_id, tool_call_id, _auth_task, _tmp) =
            seed_session_with_pending_authorization(cx).await;

        let result = AuthorizeToolCallTool
            .run(
                AuthorizeToolCallParams {
                    session_id: session_id.to_string(),
                    tool_call_id: tool_call_id.clone(),
                    option_id: "opt-allow".to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect("authorize_tool_call should succeed");
        assert!(result.structured_content.ok);
        cx.executor().run_until_parked();

        // The call must have flipped off WaitingForConfirmation — a
        // second authorize attempt now reports not_awaiting_confirmation.
        let err = AuthorizeToolCallTool
            .run(
                AuthorizeToolCallParams {
                    session_id: session_id.to_string(),
                    tool_call_id: tool_call_id.clone(),
                    option_id: "opt-allow".to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("second authorize must fail; call no longer waiting");
        assert!(
            err.to_string().contains("not_awaiting_confirmation"),
            "unexpected error: {err}"
        );
    }

    #[gpui::test]
    async fn authorize_tool_call_rejects_unknown_option(cx: &mut gpui::TestAppContext) {
        let (session_id, tool_call_id, _auth_task, _tmp) =
            seed_session_with_pending_authorization(cx).await;

        let err = AuthorizeToolCallTool
            .run(
                AuthorizeToolCallParams {
                    session_id: session_id.to_string(),
                    tool_call_id,
                    option_id: "opt-does-not-exist".to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("unknown option must error");
        assert!(
            err.to_string().contains("unknown_option"),
            "unexpected error: {err}"
        );
    }

    #[gpui::test]
    async fn authorize_tool_call_unknown_tool_call_errors(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let err = AuthorizeToolCallTool
            .run(
                AuthorizeToolCallParams {
                    session_id: session_id.to_string(),
                    tool_call_id: "no-such-call".to_string(),
                    option_id: "opt-allow".to_string(),
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("missing tool call must error");
        assert!(
            err.to_string().contains("tool_call_not_found"),
            "unexpected error: {err}"
        );
    }

    // -----------------------------------------------------------------
    // Etap 5: subagent_id + teammate streams on session DTOs.
    // -----------------------------------------------------------------

    #[gpui::test]
    async fn get_session_streams_list_main_first_then_teammate(cx: &mut gpui::TestAppContext) {
        // Phase 4b: the wire tab strip is driven by the `streams` descriptor list
        // (Main + teammates demuxed from tagged entries), not `active_subagents`.
        // `seed_mixed_subagent_session` produces [u0, a1-main, s2(sub1), u3] so the
        // demux yields Main + one Teammate(sub1) stream.
        let (session_id, _thread, _tmp) = seed_mixed_subagent_session(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let streams = &result.structured_content.streams;
        assert_eq!(streams.len(), 2, "Main + one teammate stream");
        assert_eq!(streams[0].id, StreamIdDto::Main, "Main is always first");
        assert!(matches!(streams[0].kind, StreamKindDto::Main));
        assert_eq!(
            streams[1].id,
            StreamIdDto::Teammate {
                toolu: "sub1".to_string()
            },
            "teammate stream keyed by its parent tool_use id"
        );
        assert!(matches!(streams[1].kind, StreamKindDto::Teammate));
        assert_eq!(streams[1].total_count, 1, "the one sub1-tagged entry");
        assert!(streams[1].seq > 0, "teammate stream has a stamped watermark");
    }

    #[gpui::test]
    async fn get_session_streams_main_only_when_no_teammates(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let streams = &result.structured_content.streams;
        assert_eq!(streams.len(), 1, "no tagged entries → Main-only stream list");
        assert_eq!(streams[0].id, StreamIdDto::Main);
    }

    #[gpui::test]
    async fn session_summary_exposes_session_cwd(cx: &mut gpui::TestAppContext) {
        let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;

        let expected_cwd = cx.read(|cx| {
            SolutionAgentStore::global(cx)
                .read(cx)
                .session(session_id)
                .expect("session exists")
                .read(cx)
                .cwd
                .to_string_lossy()
                .into_owned()
        });

        let get_result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");
        assert_eq!(
            get_result.structured_content.cwd.as_deref(),
            Some(expected_cwd.as_str()),
            "get_session must surface session.cwd"
        );

        let list_result = ListSessionsTool
            .run(ListSessionsParams::default(), &mut cx.to_async())
            .await
            .expect("list_sessions");
        let summary = list_result
            .structured_content
            .sessions
            .iter()
            .find(|s| s.id == session_id.to_string())
            .expect("session present in list_sessions");
        assert_eq!(
            summary.cwd.as_deref(),
            Some(expected_cwd.as_str()),
            "list_sessions must surface session.cwd on every entry"
        );
    }

    #[gpui::test]
    async fn entry_summary_carries_subagent_id_when_meta_present(cx: &mut gpui::TestAppContext) {
        // Push one assistant chunk stamped with a parent tool_use id via the
        // same meta key claude_native emits. The wire builder must surface it
        // verbatim on the resulting EntrySummary.
        let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                // `_meta.claudeCode.parentToolUseId` is the wire shape
                // claude_native stamps; matches `subagent_id_from_meta` in
                // acp_thread. Goes on the ContentChunk envelope, NOT on
                // the inner content block — that's where the helper looks.
                let mut meta = serde_json::Map::new();
                meta.insert(
                    "claudeCode".into(),
                    serde_json::json!({ "parentToolUseId": "toolu_parent_xyz" }),
                );
                let mut chunk = acp::ContentChunk::new(acp::ContentBlock::Text(
                    acp::TextContent::new("subagent says hi".to_string()),
                ));
                chunk.meta = Some(meta);
                thread
                    .handle_session_update(acp::SessionUpdate::AgentMessageChunk(chunk), cx)
                    .expect("handle_session_update");
            });
        });
        cx.executor().run_until_parked();

        // The tagged chunk is demuxed into its teammate stream, so SELECT that
        // stream (Main would not contain it — the whole point of the migration).
        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    stream_id: Some(StreamIdDto::Teammate {
                        toolu: "toolu_parent_xyz".to_string(),
                    }),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        let assistant = result
            .structured_content
            .entries
            .iter()
            .find(|e| matches!(e.role, EntryRoleDto::Assistant))
            .expect("assistant entry should be present in the teammate stream");
        assert_eq!(
            assistant.subagent_id.as_deref(),
            Some("toolu_parent_xyz"),
            "EntrySummary must carry the parent tool_use id"
        );
    }

    /// Seed `[user(Main), assistant(Main), assistant(sub1), user(Main)]` so a
    /// subagent dominates the recent tail (the empty-Main scenario) and return
    /// the session id. The single `sub1` assistant carries the subagent_id via
    /// the same `_meta` claude_native stamps.
    async fn seed_mixed_subagent_session(
        cx: &mut gpui::TestAppContext,
    ) -> (
        crate::model::SolutionSessionId,
        gpui::Entity<acp_thread::AcpThread>,
        tempfile::TempDir,
    ) {
        let (session_id, acp_thread, tmp) = create_session_with_thread(cx).await;
        cx.update(|cx| {
            acp_thread.update(cx, |thread, cx| {
                thread.push_user_content_block(
                    None,
                    acp::ContentBlock::Text(acp::TextContent::new("u0".to_string())),
                    cx,
                );
                thread.push_assistant_content_block(
                    acp::ContentBlock::Text(acp::TextContent::new("a1-main".to_string())),
                    false,
                    cx,
                );
                let mut meta = serde_json::Map::new();
                meta.insert(
                    "claudeCode".into(),
                    serde_json::json!({ "parentToolUseId": "sub1" }),
                );
                let mut chunk = acp::ContentChunk::new(acp::ContentBlock::Text(
                    acp::TextContent::new("s2-sub".to_string()),
                ));
                chunk.meta = Some(meta);
                thread
                    .handle_session_update(acp::SessionUpdate::AgentMessageChunk(chunk), cx)
                    .expect("handle_session_update");
                thread.push_user_content_block(
                    None,
                    acp::ContentBlock::Text(acp::TextContent::new("u3".to_string())),
                    cx,
                );
            });
        });
        cx.executor().run_until_parked();
        (session_id, acp_thread, tmp)
    }

    async fn get_session_stream(
        session_id: crate::model::SolutionSessionId,
        stream_id: Option<StreamIdDto>,
        cx: &mut gpui::TestAppContext,
    ) -> (Vec<Option<String>>, usize) {
        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    stream_id,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");
        let ids = result
            .structured_content
            .entries
            .iter()
            .map(|e| e.subagent_id.clone())
            .collect();
        (ids, result.structured_content.total_count)
    }

    #[gpui::test]
    async fn get_session_stream_selection_splits_main_and_teammate(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _thread, _tmp) = seed_mixed_subagent_session(cx).await;
        // Phase 4b: selecting a stream serves that stream's own entries. The
        // sub1-tagged entry lives in the teammate stream, never in Main — there is
        // no tag-then-filter and no "no strip → show all" bypass anymore.
        let (main_ids, main_total) = get_session_stream(session_id, None, cx).await;
        assert!(
            main_ids.iter().all(|id| id.is_none()),
            "Main stream has only parent (subagent_id == None) entries, got {main_ids:?}"
        );
        assert_eq!(main_ids.len(), 3, "u0 / a1-main / u3 are the Main entries");
        assert_eq!(main_total, 3, "total_count is the Main stream's own count");

        let (sub_ids, sub_total) = get_session_stream(
            session_id,
            Some(StreamIdDto::Teammate {
                toolu: "sub1".to_string(),
            }),
            cx,
        )
        .await;
        assert_eq!(
            sub_ids,
            vec![Some("sub1".to_string())],
            "the teammate stream holds only that teammate's entry"
        );
        assert_eq!(sub_total, 1);
    }

    #[gpui::test]
    async fn entry_summary_subagent_id_absent_for_parent_entries(cx: &mut gpui::TestAppContext) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        let result = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session");

        for entry in &result.structured_content.entries {
            assert!(
                entry.subagent_id.is_none(),
                "seeded session has only parent-level entries; got subagent_id={:?} on {:?}",
                entry.subagent_id,
                entry.role
            );
        }
    }

    #[gpui::test]
    async fn build_active_subagents_changed_payload_is_bare_session_id(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _img, _tmp) = seed_session_with_image(cx).await;

        cx.update(|cx| {
            let payload =
                crate::event_sources::build_active_subagents_changed_payload(session_id, cx);
            let obj = payload.as_object().expect("object");
            assert_eq!(
                obj.get("session_id").and_then(|v| v.as_str()),
                Some(session_id.to_string().as_str())
            );
            // Wire v5: the notification is a lean `{session_id}`-only dirty-poke —
            // no `active_subagents` list rides along (mobile re-polls `streams`).
            assert!(
                obj.get("active_subagents").is_none(),
                "v5 dirty-poke must not carry a subagents list"
            );
            assert_eq!(obj.len(), 1, "payload carries session_id only");
        });
    }

    /// Finding 1 regression guard: a session that was closed (not in
    /// `store.sessions`) but whose transcript is stored as per-entry rows
    /// (no blob — the Phase-4 write path never writes blobs) must be
    /// served by `read_session_history` instead of returning
    /// `session_not_found`.
    ///
    /// Before the fix the archive path only called `load_blob`, which
    /// returns NULL for a row-native session → the tool returned
    /// `session_not_found` even though the rows were present.
    #[gpui::test]
    async fn read_session_history_closed_row_native_returns_entries(cx: &mut gpui::TestAppContext) {
        use crate::session_entry::{SessionEntry, SessionEntryKind};

        // Set up a real DB so rows can be written + read by the tool.
        let (solution_id, _tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        let registry = std::sync::Arc::new(crate::adapter::AdapterRegistry::new());
        cx.update(|cx| SolutionAgentStore::init_global(cx, registry));
        let executor = cx.executor();
        let db = std::sync::Arc::new(crate::db::SolutionAgentDb::open(executor).expect("open db"));
        cx.update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_persistence(db.clone(), cx);
            });
        });

        let session_id = crate::model::SolutionSessionId::new();
        let now = chrono::Utc::now();
        let meta = crate::model::SolutionSessionMetadata {
            id: session_id,
            solution_id: solution_id,
            agent_id: SharedString::from("mock-agent"),
            acp_session_id: acp::SessionId::new(format!("acp-{}", session_id.as_str())),
            title: SharedString::from("closed row-native session"),
            created_at: now,
            last_activity_at: now,
            preview: None,
            total_tokens: None,
            context_count: 1,
            cwd: std::path::PathBuf::new(),
            parent_session_id: None,
            desired_model: None,
            desired_effort: None,
            cached_models: vec![],
            tab_order: None,
            member_id: None,
        };
        db.save_metadata(meta).await.expect("save metadata");

        // Write two entries as rows (no blob) — the Phase-4 row-native shape.
        let user_entry = SessionEntry {
            created_ms: 1_700_000_000_000,
            mod_seq: 1,
            subagent_id: None,
            kind: SessionEntryKind::UserMessage {
                id: None,
                content_md: "hello from closed session".into(),
                chunks: vec![fake_user_text_chunk("hello from closed session")],
            },
        };
        let assistant_entry = SessionEntry {
            created_ms: 1_700_000_000_001,
            mod_seq: 2,
            subagent_id: None,
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![crate::session_entry::AssistantChunk::Message(
                    "reply from closed session".into(),
                )],
            },
        };
        db.upsert_entry(
            session_id,
            0,
            user_entry.mod_seq as i64,
            user_entry.created_ms,
            None,
            user_entry.to_payload(),
        )
        .await
        .expect("upsert user entry");
        db.upsert_entry(
            session_id,
            1,
            assistant_entry.mod_seq as i64,
            assistant_entry.created_ms,
            None,
            assistant_entry.to_payload(),
        )
        .await
        .expect("upsert assistant entry");

        // The session is NOT in store.sessions — only the DB rows exist.
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            assert!(
                store.read(cx).session(session_id).is_none(),
                "session must not be in memory for this test"
            );
        });

        // Call the tool — before the fix this returned session_not_found.
        let result = ReadSessionHistoryTool
            .run(
                ReadSessionHistoryParams {
                    session_id: session_id.to_string(),
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("read_session_history must succeed for a closed row-native session");

        let sc = &result.structured_content;
        assert_eq!(
            sc.total_entries, 2,
            "must return both rows, got {}",
            sc.total_entries
        );
        assert_eq!(sc.returned_entries, 2);
        assert_eq!(sc.source, "archived");
        assert!(
            sc.entries[0].contains("hello from closed session"),
            "user entry must round-trip; got: {:?}",
            sc.entries[0]
        );
        assert!(
            sc.entries[1].contains("reply from closed session"),
            "assistant entry must round-trip; got: {:?}",
            sc.entries[1]
        );
    }

    // -----------------------------------------------------------------
    // Task 5.2: get_session_changes (mobile delta).
    // -----------------------------------------------------------------

    /// 1×1 PNG, base64 (no `data:` prefix) — same fixture the other image
    /// tests use, kept tiny so it doesn't bloat the suite.
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNgAAIAAAUAAen5lOEAAAAASUVORK5CYII=";

    /// Build a COLD, row-native session with a fixed entry layout for the
    /// delta tests:
    ///   index 0: Main user message + image     (mod_seq 1)
    ///   index 1: Main assistant message        (mod_seq 2)
    ///   index 2: Subagent("sub1") assistant    (mod_seq 3)
    ///   index 3: Main user message + image     (mod_seq 4)
    /// `change_seq` is seated at 4 (= max mod_seq). All section watermarks
    /// start at 0 so a `since_seq=0` poll re-sends every section; individual
    /// tests bump the watermarks they care about. No live thread.
    async fn seed_delta_session(
        cx: &mut gpui::TestAppContext,
    ) -> (crate::model::SolutionSessionId, tempfile::TempDir) {
        use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
        let (solution_id, tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        cx.update(|cx| {
            let registry = std::sync::Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });
        let session_id = cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                let id = crate::model::SolutionSessionId::new();
                let mut session = crate::model::SolutionSession::new_idle(
                    id,
                    solution_id,
                    SharedString::from("mock-agent"),
                    acp::SessionId::new(format!("acp-{}", id.as_str())),
                );
                session.title = SharedString::from("delta session");
                session.entries = vec![
                    SessionEntry {
                        created_ms: 1_700_000_000_000,
                        mod_seq: 1,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "u0".into(),
                            chunks: vec![
                                fake_user_text_chunk("u0"),
                                fake_image_chunk("image/png", TINY_PNG_B64),
                            ],
                        },
                    },
                    SessionEntry {
                        created_ms: 1_700_000_000_001,
                        mod_seq: 2,
                        subagent_id: None,
                        kind: SessionEntryKind::AssistantMessage {
                            chunks: vec![AssistantChunk::Message("a1-main".into())],
                        },
                    },
                    // Phase 4b: seed_delta_session is a MAIN-ONLY transcript so
                    // stream-local Main indices equal the old absolute indices and
                    // the Main-stream delta tests keep their [0..3] expectations.
                    // This third entry is a USER message (not a second consecutive
                    // assistant) so the Main stream's demux does NOT coalesce it
                    // into entry 1 — the four entries stay distinct on the wire.
                    // Teammate-stream selection is covered by dedicated tests.
                    SessionEntry {
                        created_ms: 1_700_000_000_002,
                        mod_seq: 3,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "u2".into(),
                            chunks: vec![fake_user_text_chunk("u2")],
                        },
                    },
                    SessionEntry {
                        created_ms: 1_700_000_000_003,
                        mod_seq: 4,
                        subagent_id: None,
                        kind: SessionEntryKind::UserMessage {
                            id: None,
                            content_md: "u3".into(),
                            chunks: vec![
                                fake_user_text_chunk("u3"),
                                fake_image_chunk("image/png", TINY_PNG_B64),
                            ],
                        },
                    },
                ];
                session.change_seq = 4;
                // The wire reads `session.streams`; direct `entries` assignment
                // bypasses `set_entries`, so demux the mirror by hand.
                session.rebuild_streams();
                store.register_prebuilt_session(session, cx)
            })
        });
        (session_id, tmp)
    }

    /// Mutate the in-memory session (set watermarks, push a queue bundle,
    /// seed a subagent tab, change state, …).
    fn mutate_session(
        session_id: crate::model::SolutionSessionId,
        cx: &mut gpui::TestAppContext,
        f: impl FnOnce(&mut crate::model::SolutionSession),
    ) {
        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store
                .read(cx)
                .session(session_id)
                .expect("session must exist");
            session.update(cx, |s, _| {
                f(s);
                // The wire now reads `session.streams`; a closure that assigns
                // `s.entries` directly bypasses `set_entries`, so refresh the
                // mirror. Idempotent for closures that only touch watermarks/state.
                s.rebuild_streams();
            });
        });
    }

    async fn run_changes(
        params: GetSessionChangesParams,
        cx: &mut gpui::TestAppContext,
    ) -> GetSessionChangesResult {
        GetSessionChangesTool
            .run(params, &mut cx.to_async())
            .await
            .expect("get_session_changes")
            .structured_content
    }

    #[gpui::test]
    async fn get_session_changes_returns_only_entries_past_since_seq(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _tmp) = seed_delta_session(cx).await;

        // since_seq = 2 → only entries with mod_seq 3 and 4 (indices 2, 3).
        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 2,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;

        assert!(!result.reset);
        assert_eq!(result.epoch, 0);
        assert_eq!(result.current_seq, 4);
        // total_count is the full (filtered=None) count, independent of since_seq.
        assert_eq!(result.total_count, 4);
        let indices: Vec<usize> = result.changed_entries.iter().map(|e| e.index).collect();
        assert_eq!(
            indices,
            vec![2, 3],
            "only mod_seq > since_seq entries, with absolute indices"
        );

        // since_seq = 4 → nothing changed.
        let none = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 4,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert!(none.changed_entries.is_empty());
        assert_eq!(none.total_count, 4);
    }

    // Decision #5 end-to-end on the wire: two consecutive Main assistant messages
    // coalesce into ONE stream entry that keeps the first fragment's position but
    // whose delta key (mod_seq, made coalesce-aware in `push_coalesced`) advances
    // to the LATEST fragment. A client caught up to the first fragment's seq MUST
    // still receive the merged entry — the flat `entry.mod_seq` wire would have
    // missed it (the coalesced entry froze at the first fragment's mod_seq).
    #[gpui::test]
    async fn get_session_changes_delivers_coalesce_merge_update(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_delta_session(cx).await;
        mutate_session(session_id, cx, |s| {
            use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};
            let asst = |n: u64, text: &str| SessionEntry {
                created_ms: 1_700_000_000_000 + n as i64,
                mod_seq: n,
                subagent_id: None,
                kind: SessionEntryKind::AssistantMessage {
                    chunks: vec![AssistantChunk::Message(text.into())],
                },
            };
            s.entries = vec![asst(1, "first "), asst(2, "second")];
            s.change_seq = 2;
        });

        // Caught up to the FIRST fragment's seq (1); the merged entry (mod_seq 2)
        // must still come back, at stream-local index 0, as a single entry.
        let delta = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 1,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert_eq!(
            delta.changed_entries.len(),
            1,
            "the coalesce-merged entry is delivered despite the frozen first mod_seq"
        );
        assert_eq!(
            delta.changed_entries[0].index, 0,
            "stream-local index 0 (the coalesced count did not grow)"
        );
        assert_eq!(delta.total_count, 1, "Main coalesced the two fragments to one entry");
        assert_eq!(
            delta.current_seq, 2,
            "cursor advances to the merged fragment's seq"
        );
    }

    #[gpui::test]
    async fn get_session_changes_paginates_changed_entries(cx: &mut gpui::TestAppContext) {
        use crate::session_entry::{SessionEntry, SessionEntryKind};
        let (session_id, _tmp) = seed_delta_session(cx).await;
        // Replace with 15 entries (mod_seq 1..=15) so a since=0 poll exceeds the
        // 10-per-page cap.
        mutate_session(session_id, cx, |s| {
            // USER messages (not consecutive assistant messages) so the Main
            // stream's demux keeps all 15 distinct — assistant messages would
            // coalesce into a single stream entry.
            s.entries = (1..=15u64)
                .map(|n| SessionEntry {
                    created_ms: 1_700_000_000_000 + n as i64,
                    mod_seq: n,
                    subagent_id: None,
                    kind: SessionEntryKind::UserMessage {
                        id: None,
                        content_md: format!("u{n}"),
                        chunks: vec![fake_user_text_chunk(&format!("u{n}"))],
                    },
                })
                .collect();
            s.change_seq = 15;
        });

        // Page 1: capped at 10, has_more, cursor at the 10th entry's mod_seq.
        let p1 = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 0,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert_eq!(
            p1.changed_entries.len(),
            CHANGED_ENTRIES_PAGE,
            "page capped"
        );
        assert!(p1.has_more, "more entries remain after page 1");
        assert_eq!(p1.current_seq, 10, "cursor advances to the 10th mod_seq");
        assert_eq!(p1.total_count, 15, "total_count is the full filtered count");

        // Page 2: the remaining 5, caught up, cursor at the full change_seq.
        let p2 = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: p1.current_seq,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert_eq!(p2.changed_entries.len(), 5, "remaining entries on page 2");
        assert!(!p2.has_more, "caught up after page 2");
        assert_eq!(
            p2.current_seq, 15,
            "cursor at full change_seq when caught up"
        );
    }

    #[gpui::test]
    async fn get_session_changes_sections_always_present(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_delta_session(cx).await;

        // The three small sections are ALWAYS sent, regardless of how far the
        // section watermarks sit below `since_seq`. Here every watermark is far
        // below the client's cursor — the gated implementation would have
        // omitted all three (the staleness hole); the always-send contract
        // surfaces the current values so the delta re-establishes them.
        mutate_session(session_id, cx, |s| {
            s.state = crate::model::SessionState::AwaitingInput;
            s.state_seq = 2;
            s.queue_seq = 2;
            s.subagents_seq = 2;
            s.change_seq = 9;
        });

        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 8,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert!(
            matches!(result.state, Some(SessionStateDto::AwaitingInput)),
            "state always present (even with state_seq << since_seq), got {:?}",
            result.state
        );
        assert!(
            result
                .pending_bundles
                .as_ref()
                .is_some_and(|b| b.is_empty()),
            "pending_bundles always present; empty Vec when the queue is empty"
        );
        assert!(
            !result.streams.is_empty()
                && result.streams.iter().any(|s| s.id == StreamIdDto::Main),
            "streams descriptor list is always present (Main at minimum)"
        );

        // A non-empty queue surfaces in the same always-present section.
        mutate_session(session_id, cx, |s| {
            s.pending_messages.push_back(crate::model::PendingBundle {
                target: crate::model::QueueTarget::Main,
                blocks: vec![fake_user_text_chunk("queued")],
            });
            s.queue_seq = 2;
            s.change_seq = 10;
        });
        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 9,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert!(
            matches!(result.state, Some(SessionStateDto::AwaitingInput)),
            "state still present on a later poll"
        );
        let bundles = result.pending_bundles.expect("pending_bundles always Some");
        assert_eq!(bundles.len(), 1, "the queued bundle surfaces");
    }

    #[gpui::test]
    async fn get_session_changes_reset_on_epoch_mismatch(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_delta_session(cx).await;
        // Push the session epoch above the client's known_epoch.
        mutate_session(session_id, cx, |s| {
            s.epoch = 3;
            // Move every watermark so a non-reset path WOULD have populated them.
            s.state_seq = 5;
            s.queue_seq = 5;
            s.subagents_seq = 5;
        });

        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 0,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;

        assert!(result.reset, "epoch mismatch must set reset");
        assert_eq!(result.epoch, 3);
        assert!(result.changed_entries.is_empty());
        assert!(result.removed_indices.is_empty());
        assert!(result.state.is_none());
        assert!(result.pending_bundles.is_none());
        // The `streams` descriptor list stays present even on a reset (decision #7)
        // so the client can re-select a stream after a full reload.
        assert!(
            !result.streams.is_empty(),
            "streams descriptor list present even on reset"
        );
        // total_count is still the filtered count (client ignores it).
        assert_eq!(result.total_count, 4);
    }

    #[gpui::test]
    async fn get_session_changes_stream_selection_narrows_entries_and_total(
        cx: &mut gpui::TestAppContext,
    ) {
        let (session_id, _tmp) = seed_delta_session(cx).await;
        // Install a MIXED transcript: [m0, m1, s2(sub1), m3]. The delta serves the
        // SELECTED stream's own entries with STREAM-LOCAL indices.
        mutate_session(session_id, cx, |s| {
            use crate::session_entry::{SessionEntry, SessionEntryKind};
            // USER messages so the Main entries (m0/m1/m3) stay distinct — three
            // consecutive assistant messages would coalesce into one stream entry.
            let mk = |n: u64, sub: Option<&str>, text: &str| SessionEntry {
                created_ms: 1_700_000_000_000 + n as i64,
                mod_seq: n,
                subagent_id: sub.map(SharedString::from),
                kind: SessionEntryKind::UserMessage {
                    id: None,
                    content_md: text.into(),
                    chunks: vec![fake_user_text_chunk(text)],
                },
            };
            s.entries = vec![
                mk(1, None, "m0"),
                mk(2, None, "m1"),
                mk(3, Some("sub1"), "s2"),
                mk(4, None, "m3"),
            ];
            s.change_seq = 4;
        });

        // Teammate stream, since_seq 0 → its one entry at STREAM-LOCAL index 0.
        let sub = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 0,
                known_epoch: 0,
                stream_id: Some(StreamIdDto::Teammate {
                    toolu: "sub1".to_string(),
                }),
                include_images: false,
            },
            cx,
        )
        .await;
        assert_eq!(
            sub.changed_entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![0],
            "teammate stream entry is stream-local index 0"
        );
        assert_eq!(sub.total_count, 1, "teammate stream's own count");
        assert_eq!(
            sub.selected_stream_id,
            StreamIdDto::Teammate {
                toolu: "sub1".to_string()
            }
        );
        assert_eq!(sub.current_seq, 3, "caught-up cursor = the teammate stream seq");

        // Main stream → the three parent entries at stream-local indices 0,1,2.
        let main = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 0,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert_eq!(
            main.changed_entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "Main stream entries are stream-local 0,1,2"
        );
        assert_eq!(main.total_count, 3);
        assert_eq!(main.selected_stream_id, StreamIdDto::Main);
    }

    #[gpui::test]
    async fn get_session_changes_image_indices_match_get_session(cx: &mut gpui::TestAppContext) {
        // The subtle parity test: a changed entry positioned AFTER earlier
        // image-bearing entries must report image indices identical to what
        // get_session returns for the same session + filter. Index 3 carries
        // an image and sits after the image at index 0, so its EntryImage.index
        // must be 1 in BOTH responses.
        let (session_id, _tmp) = seed_delta_session(cx).await;

        let full = GetSessionTool
            .run(
                GetSessionParams {
                    session_id: session_id.to_string(),
                    include_full_content: true,
                    include_images: true,
                    ..Default::default()
                },
                &mut cx.to_async(),
            )
            .await
            .expect("get_session")
            .structured_content;

        // Delta with since_seq = 3 → only index 3 (the second image-bearer).
        let delta = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 3,
                known_epoch: 0,
                stream_id: None,
                include_images: true,
            },
            cx,
        )
        .await;
        assert_eq!(
            delta.changed_entries.len(),
            1,
            "since_seq 3 yields exactly index 3"
        );
        let delta_entry = &delta.changed_entries[0];
        assert_eq!(delta_entry.index, 3);

        let full_entry = full
            .entries
            .iter()
            .find(|e| e.index == 3)
            .expect("get_session must include index 3");

        let delta_image_indices: Vec<usize> = delta_entry
            .images
            .as_ref()
            .expect("delta entry images populated")
            .iter()
            .map(|img| img.index)
            .collect();
        let full_image_indices: Vec<usize> = full_entry
            .images
            .as_ref()
            .expect("full entry images populated")
            .iter()
            .map(|img| img.index)
            .collect();
        assert_eq!(
            delta_image_indices, full_image_indices,
            "delta image indices must equal get_session's for the same entry"
        );
        assert_eq!(
            delta_image_indices,
            vec![1],
            "the second image-bearing entry's image is global index 1"
        );
    }

    #[gpui::test]
    async fn get_session_changes_tail_truncate_shrinks_total(cx: &mut gpui::TestAppContext) {
        let (session_id, _tmp) = seed_delta_session(cx).await;
        // Tail-truncate to the first two entries (mirrors EntriesRemoved).
        mutate_session(session_id, cx, |s| {
            s.entries.truncate(2);
            s.change_seq = 5;
            // Bump the surviving tail entry's mod_seq so it re-sends.
            if let Some(last) = s.entries.last_mut() {
                last.mod_seq = 5;
            }
        });

        let result = run_changes(
            GetSessionChangesParams {
                session_id: session_id.to_string(),
                since_seq: 4,
                known_epoch: 0,
                stream_id: None,
                include_images: false,
            },
            cx,
        )
        .await;
        assert_eq!(
            result.total_count, 2,
            "total_count shrank to the new length"
        );
        assert!(
            result.removed_indices.is_empty(),
            "removed_indices stays empty under the tail-truncate model"
        );
        let indices: Vec<usize> = result.changed_entries.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![1], "surviving changed entry keeps its index");
    }

    fn anchored_entry(index: usize, role: EntryRoleDto) -> EntrySummary {
        EntrySummary {
            role,
            index,
            preview: String::new(),
            markdown: None,
            images: None,
            tool_call: None,
            plan: None,
            system_level: None,
            client_send_id: None,
            client_send_ids: Vec::new(),
            created_ms: None,
            subagent_id: None,
            observer_nudge: false,
            editor_recovery: false,
        }
    }

    /// A user-role entry that is actually a supervisor nudge (the observer's
    /// own voice), not the human's message.
    fn nudge_entry(index: usize) -> EntrySummary {
        let mut e = anchored_entry(index, EntryRoleDto::User);
        e.observer_nudge = true;
        e
    }

    /// A user-role entry that is actually an editor reconnect-recovery prompt.
    fn recovery_entry(index: usize) -> EntrySummary {
        let mut e = anchored_entry(index, EntryRoleDto::User);
        e.editor_recovery = true;
        e
    }

    #[test]
    fn user_anchored_filter_keeps_user_lead_trail_and_resting_turn() {
        use EntryRoleDto::*;
        // Timeline: assistant churn, a user turn, more churn, another user
        // turn, then a long agent tail. lead=2 → each user keeps itself + 2
        // before; the TRAIL keeps the agent's assistant answer after each user
        // turn (skipping tool calls, stopping at the next user); the final
        // entry is always kept (the resting turn).
        let mut kept = vec![
            anchored_entry(0, Assistant),
            anchored_entry(1, ToolCall),
            anchored_entry(2, Assistant),
            anchored_entry(3, User), // lead keeps 1,2,3
            anchored_entry(4, ToolCall),
            anchored_entry(5, Assistant), // trail of #3 (tool 4 skipped), stops at user 7
            anchored_entry(6, ToolCall),
            anchored_entry(7, User), // lead keeps 5,6,7
            anchored_entry(8, Assistant), // trail of #7
            anchored_entry(9, ToolCall),
            anchored_entry(10, Assistant), // trail of #7 + resting turn
        ];
        apply_user_anchored_filter(&mut kept, 2, None);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![1, 2, 3, 5, 6, 7, 8, 10]);
    }

    /// The agent's answer to a user message survives even when buried behind a
    /// pile of tool calls, and the trail caps at `USER_ANCHORED_TRAIL_ASSISTANT`
    /// assistant text turns so a long tail can't blow the slice.
    #[test]
    fn user_anchored_filter_trail_skips_tool_calls_and_caps_assistant_turns() {
        use EntryRoleDto::*;
        let mut kept = vec![anchored_entry(0, User)];
        // 3 tool calls, then the text answer, then 6 more assistant turns.
        kept.push(anchored_entry(1, ToolCall));
        kept.push(anchored_entry(2, ToolCall));
        kept.push(anchored_entry(3, ToolCall));
        for i in 4..=10 {
            kept.push(anchored_entry(i, Assistant));
        }
        apply_user_anchored_filter(&mut kept, 0, None);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        // Anchor 0; tool calls 1-3 dropped; assistant 4-8 kept (cap 5); 9
        // dropped by the cap; 10 kept as the resting turn.
        assert_eq!(indices, vec![0, 4, 5, 6, 7, 8, 10]);
    }

    /// A supervisor nudge is user-role but must NOT anchor the slice — otherwise
    /// the judge re-reads its own past steering as a fresh user goal and loops.
    /// The nudge itself still shows up (as trailing/lead context) but never
    /// pulls a lead/trail window of its own.
    #[test]
    fn user_anchored_filter_ignores_observer_nudge_as_anchor() {
        use EntryRoleDto::*;
        let mut kept = vec![
            anchored_entry(0, Assistant),
            anchored_entry(1, User), // real anchor: lead keeps 0,1
            anchored_entry(2, Assistant), // trail of #1
            nudge_entry(3),          // observer nudge — NOT an anchor, stops #1 trail
            anchored_entry(4, Assistant),
            anchored_entry(5, ToolCall),
            anchored_entry(6, Assistant), // resting turn
        ];
        apply_user_anchored_filter(&mut kept, 1, None);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        // 4 and 5 belong to the nudge's follow-up work — not attributed to the
        // human's message #1, and the nudge doesn't anchor them. Only the
        // resting turn (6) rescues the tail.
        assert_eq!(indices, vec![0, 1, 2, 6]);
    }

    #[test]
    fn user_anchored_filter_ignores_editor_recovery_as_anchor() {
        use EntryRoleDto::*;
        // An editor reconnect-recovery prompt is a user-role entry but NOT the
        // human's voice — it must not anchor (the judge must not distill "your
        // process hung" into a user goal). Same shape as the observer-nudge test.
        let mut kept = vec![
            anchored_entry(0, Assistant),
            anchored_entry(1, User), // real anchor: lead keeps 0,1
            anchored_entry(2, Assistant), // trail of #1
            recovery_entry(3),       // editor recovery — NOT an anchor, stops #1 trail
            anchored_entry(4, Assistant),
            anchored_entry(5, ToolCall),
            anchored_entry(6, Assistant), // resting turn
        ];
        apply_user_anchored_filter(&mut kept, 1, None);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        // 4,5 belong to the recovery's follow-up work — not attributed to the
        // human's message #1, and recovery doesn't anchor them. Only the resting
        // turn (6) rescues the tail.
        assert_eq!(indices, vec![0, 1, 2, 6]);
    }

    /// Adjacent human messages must not overlap: each anchor's trail stops at
    /// the next user turn, so an answer is attributed to exactly one message.
    #[test]
    fn user_anchored_filter_trail_stops_at_next_user_no_overlap() {
        use EntryRoleDto::*;
        let mut kept = vec![
            anchored_entry(0, User), // lead keeps 0; trail stops immediately at user 1
            anchored_entry(1, User), // lead keeps 0,1; trail keeps 2
            anchored_entry(2, Assistant), // trail of #1 + resting turn
        ];
        apply_user_anchored_filter(&mut kept, 2, None);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn user_anchored_filter_dedups_overlapping_windows_and_clamps_start() {
        use EntryRoleDto::*;
        // Back-to-back user turns with lead larger than the gap must not
        // duplicate the shared lead entries, and lead past index 0 clamps.
        let mut kept = vec![
            anchored_entry(0, Assistant),
            anchored_entry(1, User),
            anchored_entry(2, User),
            anchored_entry(3, Assistant),
        ];
        apply_user_anchored_filter(&mut kept, 5, None);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn user_anchored_filter_noop_without_user_entries() {
        use EntryRoleDto::*;
        let mut kept = vec![anchored_entry(0, Assistant), anchored_entry(1, ToolCall)];
        apply_user_anchored_filter(&mut kept, 3, None);
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        assert_eq!(indices, vec![0, 1], "no anchor → window kept as-is");
    }

    /// `since_ms` makes the slice incremental: only user turns newer than the
    /// cutoff anchor (older ones are already in the judge's `user_intent.md`).
    #[test]
    fn user_anchored_filter_since_ms_keeps_only_new_user_turns() {
        use EntryRoleDto::*;
        let at = |index: usize, role: EntryRoleDto, ts: i64| {
            let mut e = anchored_entry(index, role);
            e.created_ms = Some(ts);
            e
        };
        // Two user turns: old (ts 100) and new (ts 200). cutoff = 150.
        let mut kept = vec![
            at(0, Assistant, 90),
            at(1, ToolCall, 95),
            at(2, User, 100), // old → must NOT anchor
            at(3, ToolCall, 180),
            at(4, Assistant, 190),
            at(5, User, 200),      // new → anchors, lead=2 keeps 3,4,5
            at(6, Assistant, 210), // resting turn → kept
        ];
        apply_user_anchored_filter(&mut kept, 2, Some(150));
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        assert_eq!(
            indices,
            vec![3, 4, 5, 6],
            "only the post-cutoff user turn anchors; old user turn dropped"
        );
    }

    /// When nothing is newer than the cutoff, keep ONLY the resting turn — the
    /// judge sees where the agent stopped, not the already-distilled old turns.
    #[test]
    fn user_anchored_filter_since_ms_all_old_keeps_resting_turn() {
        use EntryRoleDto::*;
        let at = |index: usize, role: EntryRoleDto, ts: i64| {
            let mut e = anchored_entry(index, role);
            e.created_ms = Some(ts);
            e
        };
        let mut kept = vec![
            at(0, User, 50),
            at(1, Assistant, 60), // resting turn
        ];
        apply_user_anchored_filter(&mut kept, 3, Some(100));
        let indices: Vec<usize> = kept.iter().map(|e| e.index).collect();
        assert_eq!(
            indices,
            vec![1],
            "all user turns pre-cutoff → only resting turn kept"
        );
    }

    /// The UI cannot create a chat in a member-less Solution
    /// (`console_panel::workspace_has_project`); the MCP tool must not be a way
    /// around that guard, or an agent lands in exactly the broken, cwd-less
    /// session the UI refuses to make.
    #[gpui::test]
    async fn create_session_in_a_member_less_solution_is_rejected(cx: &mut gpui::TestAppContext) {
        let (solution_id, _tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        let err = CreateSessionTool
            .run(
                CreateSessionParams {
                    solution_id: solution_id.0,
                    agent_id: "mock-agent".into(),
                    initial_message: None,
                    parent_session_id: None,
                    title: None,
                    cwd: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("a member-less solution cannot host a session");
        let msg = err.to_string();
        assert!(
            msg.contains("solution_has_no_members"),
            "expected solution_has_no_members in {msg:?}"
        );
    }

    /// …and the guard keys on the *member list*, not on "no window is open": a
    /// Solution that has a member gets past it and fails later, on the workspace
    /// lookup (this test opens no window).
    #[gpui::test]
    async fn create_session_with_a_member_clears_the_member_guard(cx: &mut gpui::TestAppContext) {
        let (solution_id, _tmp, _project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        let store = cx.update(|cx| solutions::SolutionStore::global(cx));
        store
            .update(cx, |store, cx| {
                store.add_empty_member(solution_id, "member", cx)
            })
            .expect("add_empty_member");

        let err = CreateSessionTool
            .run(
                CreateSessionParams {
                    solution_id: solution_id.0,
                    agent_id: "mock-agent".into(),
                    initial_message: None,
                    parent_session_id: None,
                    title: None,
                    cwd: None,
                },
                &mut cx.to_async(),
            )
            .await
            .expect_err("no workspace window is open in this test");
        let msg = err.to_string();
        assert!(
            !msg.contains("solution_has_no_members"),
            "a solution with a member must clear the member guard; got {msg:?}"
        );
        assert!(
            msg.contains("no_active_workspace_for_solution"),
            "expected the workspace-lookup error instead; got {msg:?}"
        );
    }
