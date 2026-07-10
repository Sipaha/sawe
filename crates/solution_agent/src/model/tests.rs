    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn resume_on_activity_clears_inactive_states_including_errored() {
        // Genuine non-system agent activity (a new entry / streaming update)
        // means the session is live again, so a latched `Errored` must clear —
        // otherwise the status row stays red "Error: agent error" while the
        // agent keeps streaming (bug #5). `Idle`/`AwaitingInput` clear too.
        for mut state in [
            SessionState::Errored("agent error".into()),
            SessionState::Idle,
            SessionState::AwaitingInput,
        ] {
            let before = state.short_label();
            assert!(
                state.resume_on_activity(),
                "{before} must resume on activity"
            );
            assert!(
                matches!(
                    state,
                    SessionState::Running {
                        notified: false,
                        ..
                    }
                ),
                "{before} -> Running, got {state:?}"
            );
        }

        // Already-active / cancelling states are left untouched (no spurious
        // reset of `notified`, no Stopping -> Running flip).
        let started = Instant::now();
        let mut running = SessionState::Running {
            started_at: started,
            notified: true,
        };
        assert!(!running.resume_on_activity());
        assert!(matches!(
            running,
            SessionState::Running { notified: true, .. }
        ));

        let mut stopping = SessionState::Stopping {
            started_at: started,
        };
        assert!(!stopping.resume_on_activity());
        assert!(matches!(stopping, SessionState::Stopping { .. }));
    }

    #[test]
    fn clear_error_on_activity_only_unlatches_errored() {
        // `clear_error_on_activity` is the narrower sibling for in-place
        // streaming updates (`EntryUpdated`): it clears a latched `Errored` but
        // must NOT resurrect a finished turn — an `Idle`/`AwaitingInput` session
        // can still receive a late streaming-reveal update after the turn's
        // `Stopped`, and flipping it to Running would wrongly show "Thinking…".
        let mut errored = SessionState::Errored("agent error".into());
        assert!(errored.clear_error_on_activity());
        assert!(matches!(
            errored,
            SessionState::Running {
                notified: false,
                ..
            }
        ));

        for mut state in [SessionState::Idle, SessionState::AwaitingInput] {
            let before = state.short_label();
            assert!(
                !state.clear_error_on_activity(),
                "{before} must be left untouched"
            );
            assert!(matches!(
                state,
                SessionState::Idle | SessionState::AwaitingInput
            ));
        }
    }

    fn build_session() -> SolutionSession {
        SolutionSession {
            id: SolutionSessionId::new(),
            solution_id: SolutionId("sol".into()),
            agent_id: SharedString::from("claude-acp"),
            acp_session_id: acp::SessionId::new("acp-mock"),
            acp_thread: None,
            title: SharedString::from("test"),
            created_at: Utc::now(),
            last_activity_at: Utc::now(),
            state: SessionState::Idle,
            cwd: PathBuf::new(),
            context_count: 1,
            project: None,
            _acp_subscription: None,
            pending_messages: VecDeque::new(),
            flush_after_cancel: false,
            live_base: 0,
            entries: Vec::new(),
            streams: {
                let mut streams = indexmap::IndexMap::new();
                streams.insert(crate::stream::StreamId::Main, crate::stream::Stream::main());
                streams
            },
            closed_streams: HashMap::new(),
            hydration_orphan_streams: std::collections::HashSet::new(),
            hydration_watermark: 0,
            persisted_main_seq: 0,
            hydrating: false,
            last_turn_duration: None,
            cached_total_tokens: None,
            cached_max_tokens: None,
            cached_models: Vec::new(),
            desired_model: None,
            desired_effort: None,
            parent_session_id: None,
            stopping_safety_net: None,
            teammate_labels: HashMap::new(),
            background_agents: HashMap::new(),
            background_agent_order: Vec::new(),
            background_shells: HashMap::new(),
            background_shell_order: Vec::new(),
            tab_order: None,
            change_seq: 0,
            epoch: 0,
            queue_seq: 0,
            subagents_seq: 0,
            state_seq: 0,
            supervisor_question: None,
            is_supervisor_ephemeral: false,
            is_ephemeral: false,
        }
    }

    /// `set_acp_thread` is the load-bearing contract that keeps
    /// `SolutionSessionView::_thread_subscription` from going stale when
    /// a session swaps its `AcpThread` (compact, `/clear`, cold→live).
    /// If anyone reverts to direct `s.acp_thread = ...` assignment
    /// inside a nested `update`, observers wired through `cx.observe`
    /// may be silently skipped — this test pins both signals so that
    /// regression is caught at unit-test time.
    #[gpui::test]
    fn set_acp_thread_emits_thread_replaced_and_notifies(cx: &mut TestAppContext) {
        let session = cx.update(|cx| cx.new(|_| build_session()));

        let emit_count = Arc::new(AtomicUsize::new(0));
        let observe_count = Arc::new(AtomicUsize::new(0));

        cx.update(|cx| {
            let emit = emit_count.clone();
            cx.subscribe(
                &session,
                move |_session: Entity<SolutionSession>, event: &SolutionSessionEvent, _cx| {
                    let SolutionSessionEvent::ThreadReplaced = event;
                    emit.fetch_add(1, Ordering::SeqCst);
                },
            )
            .detach();
            let observe = observe_count.clone();
            cx.observe(&session, move |_session: Entity<SolutionSession>, _cx| {
                observe.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
        });

        cx.run_until_parked();
        assert_eq!(emit_count.load(Ordering::SeqCst), 0);
        assert_eq!(observe_count.load(Ordering::SeqCst), 0);

        session.update(cx, |s, cx| s.set_acp_thread(None, cx));
        cx.run_until_parked();

        assert_eq!(
            emit_count.load(Ordering::SeqCst),
            1,
            "set_acp_thread must emit exactly one ThreadReplaced event"
        );
        assert_eq!(
            observe_count.load(Ordering::SeqCst),
            1,
            "set_acp_thread must wake cx.observe subscribers via cx.notify()"
        );
    }

    #[gpui::test]
    fn set_entries_stores_and_notifies(cx: &mut TestAppContext) {
        let session = cx.update(|cx| cx.new(|_| build_session()));
        let notified = std::rc::Rc::new(std::cell::Cell::new(false));
        let _sub = cx.update(|cx| {
            let n = notified.clone();
            cx.observe(&session, move |_, _| n.set(true))
        });
        session.update(cx, |s, cx| {
            assert!(s.entries.is_empty());
            s.set_entries(
                vec![SessionEntry {
                    created_ms: 0,
                    mod_seq: 0,
                    subagent_id: None,
                    kind: crate::session_entry::SessionEntryKind::UserMessage {
                        id: None,
                        content_md: "x".into(),
                        chunks: vec![],
                    },
                }],
                cx,
            );
        });
        cx.run_until_parked();
        assert!(notified.get());
        session.read_with(cx, |s, _| assert_eq!(s.entries.len(), 1));
    }

    #[gpui::test]
    fn streams_mirror_tracks_entries_via_set_entries(cx: &mut TestAppContext) {
        use crate::session_entry::{AssistantChunk, SessionEntryKind};
        use crate::stream::StreamId;
        fn msg(text: &str, sub: Option<&str>) -> SessionEntry {
            SessionEntry {
                created_ms: 0,
                mod_seq: 0,
                subagent_id: sub.map(SharedString::from),
                kind: SessionEntryKind::AssistantMessage {
                    chunks: vec![AssistantChunk::Message(text.to_string())],
                },
            }
        }
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            // A fresh session already carries a Main-only streams mirror.
            assert_eq!(s.streams.len(), 1);
            assert!(s.streams.contains_key(&StreamId::Main));
            s.set_entries(vec![msg("hi", None), msg("sub", Some("T1"))], cx);
            // Mirror now has Main + Teammate(T1), each with one entry.
            assert_eq!(s.streams.len(), 2);
            assert_eq!(s.streams[&StreamId::Main].entries.len(), 1);
            assert_eq!(
                s.streams[&StreamId::Teammate(SharedString::from("T1"))]
                    .entries
                    .len(),
                1
            );
        });
    }

    fn msg_tagged(text: &str, sub: Option<&str>) -> SessionEntry {
        use crate::session_entry::{AssistantChunk, SessionEntryKind};
        SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: sub.map(SharedString::from),
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text.to_string())],
            },
        }
    }

    #[gpui::test]
    fn close_stream_removes_teammate_and_survives_rebuild(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let t1 = StreamId::Teammate(SharedString::from("T1"));
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![msg_tagged("hi", None), msg_tagged("sub", Some("T1"))],
                cx,
            );
            assert!(s.streams.contains_key(&t1), "teammate stream present");
            s.close_stream(t1.clone(), SharedString::new_static("done"));
            assert!(!s.streams.contains_key(&t1), "closed → absent from mirror");
            // Entries are untouched, so a bare rebuild must NOT resurrect it.
            s.rebuild_streams();
            assert!(!s.streams.contains_key(&t1), "overlay survives rebuild");
            assert_eq!(s.entries.len(), 2, "tagged entries stay in entries");
        });
    }

    #[gpui::test]
    fn close_stream_refuses_main(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, _| {
            s.close_stream(StreamId::Main, SharedString::new_static("x"));
            assert!(s.streams.contains_key(&StreamId::Main), "Main stays live");
            assert!(s.closed_streams.is_empty(), "Main never enters overlay");
        });
    }

    #[gpui::test]
    fn closed_stream_does_not_block_a_different_teammate(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let t1 = StreamId::Teammate(SharedString::from("T1"));
        let t2 = StreamId::Teammate(SharedString::from("T2"));
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            s.set_entries(vec![msg_tagged("sub", Some("T1"))], cx);
            s.close_stream(t1.clone(), SharedString::new_static("done"));
            // A later demux (via set_entries) that also carries T2 keeps T1
            // closed (overlay) while T2 comes up fresh and live.
            s.set_entries(
                vec![msg_tagged("sub1", Some("T1")), msg_tagged("sub2", Some("T2"))],
                cx,
            );
            assert!(!s.streams.contains_key(&t1), "T1 stays closed");
            assert!(s.streams.contains_key(&t2), "T2 present");
            assert_eq!(
                s.streams[&t2].state,
                crate::stream::StreamState::Live,
                "T2 is live"
            );
        });
    }

    #[gpui::test]
    fn clear_closed_streams_reopens(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let t1 = StreamId::Teammate(SharedString::from("T1"));
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            s.set_entries(vec![msg_tagged("sub", Some("T1"))], cx);
            s.close_stream(t1.clone(), SharedString::new_static("done"));
            assert!(!s.streams.contains_key(&t1));
            s.clear_closed_streams();
            s.rebuild_streams();
            assert!(s.streams.contains_key(&t1), "cleared overlay → reopened");
        });
    }

    fn msg_seq(text: &str, sub: Option<&str>, mod_seq: u64) -> SessionEntry {
        use crate::session_entry::{AssistantChunk, SessionEntryKind};
        SessionEntry {
            created_ms: 0,
            mod_seq,
            subagent_id: sub.map(SharedString::from),
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text.to_string())],
            },
        }
    }

    // Sub-task A: per-stream `seq` = max `mod_seq` of the stream's entries,
    // recomputed on every full-replace `rebuild_streams` — nonzero once the
    // stream has a stamped entry, UNCHANGED while its entries+mod_seqs are, and
    // ADVANCED on any append / in-place re-stamp.
    #[gpui::test]
    fn stream_seq_allocated_kept_and_advanced_for_main(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            s.set_entries(vec![msg_seq("a", None, 1)], cx);
            let seq0 = s.streams[&StreamId::Main].seq;
            assert_eq!(seq0, 1, "seq is the stream's max entry mod_seq");

            // Same entries + same mod_seqs → max is unchanged → seq kept.
            s.set_entries(vec![msg_seq("a", None, 1)], cx);
            assert_eq!(
                s.streams[&StreamId::Main].seq, seq0,
                "unchanged entries must not bump seq"
            );

            // Append a Main entry with a higher mod_seq → max rises → seq advances.
            s.set_entries(vec![msg_seq("a", None, 1), msg_seq("b", None, 2)], cx);
            assert!(
                s.streams[&StreamId::Main].seq > seq0,
                "an appended entry with a higher mod_seq must bump the stream's seq"
            );
        });
    }

    // Sub-task A, decision #5: `push_coalesced` advances the coalesced entry's
    // mod_seq to the incoming max, so even though the merge keeps the stream one
    // entry long the stream's `seq` (= max entry mod_seq) still rises — a delta
    // keyed on it won't miss a coalesced-message update.
    #[gpui::test]
    fn stream_seq_advances_on_coalesce_merge_despite_single_entry(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            // Two consecutive Main assistant messages coalesce into ONE entry.
            s.set_entries(vec![msg_seq("one ", None, 1), msg_seq("two", None, 2)], cx);
            assert_eq!(
                s.streams[&StreamId::Main].entries.len(),
                1,
                "consecutive same-source assistant messages coalesce"
            );
            let seq_before = s.streams[&StreamId::Main].seq;
            assert_eq!(seq_before, 2, "seq is the coalesced entries' max mod_seq");

            // A THIRD assistant chunk coalesces too (stream stays one entry) but
            // its higher mod_seq is carried onto the coalesced entry by
            // `push_coalesced`, so the stream's max mod_seq rises.
            s.set_entries(
                vec![
                    msg_seq("one ", None, 1),
                    msg_seq("two ", None, 2),
                    msg_seq("three", None, 3),
                ],
                cx,
            );
            assert_eq!(
                s.streams[&StreamId::Main].entries.len(),
                1,
                "still one coalesced entry"
            );
            assert_eq!(
                s.streams[&StreamId::Main].seq, 3,
                "seq must advance on a coalesce-merge the frozen first-fragment mod_seq hides"
            );
        });
    }

    // Sub-task A: per-stream seqs are independent — changing one stream's
    // entries must not bump the other stream's seq.
    #[gpui::test]
    fn stream_seq_is_per_stream_independent(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let t1 = StreamId::Teammate(SharedString::from("T1"));
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![msg_seq("m", None, 1), msg_seq("t", Some("T1"), 2)],
                cx,
            );
            let main0 = s.streams[&StreamId::Main].seq;
            let t0 = s.streams[&t1].seq;

            // Change ONLY the teammate stream (append a tagged entry).
            s.set_entries(
                vec![
                    msg_seq("m", None, 1),
                    msg_seq("t", Some("T1"), 2),
                    msg_seq("t2", Some("T1"), 3),
                ],
                cx,
            );
            assert_eq!(s.streams[&StreamId::Main].seq, main0, "Main seq unchanged");
            assert!(s.streams[&t1].seq > t0, "teammate seq advanced");

            // Now change ONLY Main.
            let t_now = s.streams[&t1].seq;
            let main_now = s.streams[&StreamId::Main].seq;
            s.set_entries(
                vec![
                    msg_seq("m", None, 1),
                    msg_seq("m2", None, 4),
                    msg_seq("t", Some("T1"), 2),
                    msg_seq("t2", Some("T1"), 3),
                ],
                cx,
            );
            assert!(s.streams[&StreamId::Main].seq > main_now, "Main seq advanced");
            assert_eq!(s.streams[&t1].seq, t_now, "teammate seq unchanged");
        });
    }

    // Sub-task B: cold-load hydration collapses tagged rows to a Main-only view
    // and records the watermark boundary between the cold prefix and any
    // resume-streamed entries.
    #[gpui::test]
    fn hydrate_collapses_to_main_only_and_records_watermark(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let t1 = StreamId::Teammate(SharedString::from("T1"));
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![msg_tagged("main", None), msg_tagged("sub", Some("T1"))],
                cx,
            );
            assert!(s.streams.contains_key(&t1), "teammate present before hydrate");

            s.hydrate_streams_main_only();
            assert_eq!(s.streams.len(), 1, "only Main survives hydration");
            assert!(s.streams.contains_key(&StreamId::Main));
            assert!(!s.streams.contains_key(&t1), "teammate collapsed to Main-only");
            assert_eq!(
                s.hydration_watermark, 2,
                "watermark pins the cold-prefix boundary at entries.len()"
            );
        });
    }

    // Decision #16: the cold-load sites assign `entries` DIRECTLY (no
    // `set_entries`/`rebuild_streams` first), so `hydrate_streams_main_only`
    // must derive orphans from a demux of the freshly-assigned entries, not
    // from the still-stale `self.streams` mirror. This test reproduces that
    // exact site — a direct-`entries`-assign, then hydrate — and asserts the
    // teammate is recorded as an orphan AND suppressed from the rebuilt streams.
    #[gpui::test]
    fn hydrate_records_orphans_from_directly_assigned_entries(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let t1 = StreamId::Teammate(SharedString::from("T1"));
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, _cx| {
            // Mimic the cold-load path: assign `entries` directly, leaving
            // `self.streams` as the stale Main-only mirror (the pre-fix bug's
            // read source that recorded zero orphans).
            s.entries = vec![msg_tagged("main", None), msg_tagged("sub", Some("T1"))];

            s.hydrate_streams_main_only();

            assert!(
                s.hydration_orphan_streams.contains(&t1),
                "teammate from directly-assigned entries must be recorded as an orphan"
            );
            assert!(
                !s.streams.contains_key(&t1),
                "the cold-restored teammate must be suppressed from the rebuilt streams"
            );
            assert!(
                s.streams.contains_key(&StreamId::Main),
                "Main survives hydration"
            );
        });
    }

    // Sub-task B, THE REGRESSION this fix removes: a cold-restored finished
    // teammate's tagged rows re-demux to a Live stream on every rebuild, but the
    // hydration-orphan overlay must keep suppressing it when NO new activity has
    // streamed past the watermark. (The old `clear_closed_streams`-on-attach
    // guard reopened it into a permanent zombie tab.)
    #[gpui::test]
    fn hydration_orphan_stays_suppressed_without_new_activity(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let t1 = StreamId::Teammate(SharedString::from("T1"));
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![msg_tagged("main", None), msg_tagged("sub", Some("T1"))],
                cx,
            );
            s.hydrate_streams_main_only();
            assert!(!s.streams.contains_key(&t1));

            // A bare rebuild (no entry past the watermark) must NOT resurrect it.
            s.rebuild_streams();
            assert!(
                !s.streams.contains_key(&t1),
                "no post-watermark activity → orphan stays collapsed"
            );
        });
    }

    // Sub-task B: an orphan REOPENS when the resume streams a fresh tagged entry
    // for it at an index at/after the watermark.
    #[gpui::test]
    fn hydration_orphan_reopens_on_post_watermark_activity(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let t1 = StreamId::Teammate(SharedString::from("T1"));
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![msg_tagged("main", None), msg_tagged("sub", Some("T1"))],
                cx,
            );
            s.hydrate_streams_main_only();
            assert!(!s.streams.contains_key(&t1), "collapsed while cold");

            // A resume streams a new T1-tagged entry at index 2 (>= watermark).
            s.set_entries(
                vec![
                    msg_tagged("main", None),
                    msg_tagged("sub", Some("T1")),
                    msg_tagged("resumed", Some("T1")),
                ],
                cx,
            );
            assert!(
                s.streams.contains_key(&t1),
                "post-watermark tagged activity reopens the orphan"
            );
        });
    }

    // Sub-task B: a permanent Done-close (Task terminal / async-Agent stop_reason)
    // is NOT reopenable — post-watermark activity for a permanently-closed stream
    // must stay absent. This distinguishes the two overlays (the naive "reopen
    // any suppressed id with live activity" fix would wrongly resurrect it).
    #[gpui::test]
    fn permanent_done_close_not_reopened_by_post_watermark_activity(cx: &mut TestAppContext) {
        use crate::stream::StreamId;
        let t2 = StreamId::Teammate(SharedString::from("T2"));
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![msg_tagged("main", None), msg_tagged("sub", Some("T2"))],
                cx,
            );
            s.hydrate_streams_main_only();
            // A real completion signal Done-closes T2 (moves it out of the orphan
            // overlay into the permanent overlay).
            s.close_stream(t2.clone(), SharedString::new_static("done"));
            assert!(!s.streams.contains_key(&t2));
            assert!(
                !s.hydration_orphan_streams.contains(&t2),
                "Done-close drops the reopenable orphan record"
            );

            // Even fresh post-watermark activity must not resurrect it.
            s.set_entries(
                vec![
                    msg_tagged("main", None),
                    msg_tagged("sub", Some("T2")),
                    msg_tagged("more", Some("T2")),
                ],
                cx,
            );
            assert!(
                !s.streams.contains_key(&t2),
                "permanent Done-close outranks post-watermark activity"
            );
        });
    }

    /// Phase 2c render-flip: the desktop render sources the selected view's
    /// entries from `streams[selected_stream]`. This is the
    /// end-to-end proof of the two things the screenshot gate checks — Main
    /// EXCLUDES teammate entries (no blank/leaked rows), and the Task view
    /// shows ONLY that teammate — including the per-stream coalescing that
    /// makes two same-source assistant messages, split by an interleaved
    /// other-source entry in the flat list, reunite into one bubble.
    #[gpui::test]
    fn selected_view_streams_split_main_and_teammate(cx: &mut TestAppContext) {
        use crate::session_entry::{AssistantChunk, SessionEntryKind};
        use crate::stream::StreamId;
        fn assistant(text: &str, sub: Option<&str>) -> SessionEntry {
            SessionEntry {
                created_ms: 0,
                mod_seq: 0,
                subagent_id: sub.map(SharedString::from),
                kind: SessionEntryKind::AssistantMessage {
                    chunks: vec![AssistantChunk::Message(text.to_string())],
                },
            }
        }
        fn user(text: &str) -> SessionEntry {
            SessionEntry {
                created_ms: 0,
                mod_seq: 0,
                subagent_id: None,
                kind: SessionEntryKind::UserMessage {
                    id: None,
                    content_md: text.into(),
                    chunks: vec![],
                },
            }
        }
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            // Flat interleaved transcript: two Main assistant messages that are
            // NOT adjacent in the flat list (a teammate entry sits between
            // them), and two adjacent teammate messages.
            s.set_entries(
                vec![
                    user("hello"),                     // Main
                    assistant("hi there", None),       // Main
                    assistant("sub work 1", Some("toolu_1")), // teammate
                    assistant("back to main", None),   // Main — reunites with "hi there"
                    assistant("sub work 2", Some("toolu_1")), // teammate — reunites
                ],
                cx,
            );

            // Main view resolves to StreamId::Main: user + ONE coalesced
            // assistant (the two Main assistants merged across the interleaved
            // teammate entry). NO teammate entry leaks in.
            let main_id = StreamId::Main;
            let main = &s.streams[&main_id].entries;
            assert_eq!(main.len(), 2, "user + one coalesced Main assistant");
            assert!(
                main.iter().all(|e| e.subagent_id.is_none()),
                "Main must contain no teammate-tagged entries"
            );

            // Task(toolu_1) resolves to the Teammate stream: ONE coalesced
            // assistant, tagged, and nothing from Main.
            let task_id = StreamId::Teammate("toolu_1".into());
            assert_eq!(task_id, StreamId::Teammate("toolu_1".into()));
            let team = &s.streams[&task_id].entries;
            assert_eq!(team.len(), 1, "two teammate messages coalesced into one");
            assert_eq!(
                team[0].subagent_id.as_deref(),
                Some("toolu_1"),
                "coalesced entry keeps the teammate tag"
            );

            // A selected teammate id with no entries has no stream → the render
            // helper falls back to empty (renders "(no messages yet)").
            assert!(
                !s.streams
                    .contains_key(&StreamId::Teammate("toolu_absent".into()))
            );
        });
    }

    #[gpui::test]
    fn change_seq_is_monotonic_and_epoch_bumps(cx: &mut TestAppContext) {
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, _| {
            assert_eq!(s.change_seq, 0);
            assert_eq!(s.bump_change_seq(), 1);
            assert_eq!(s.bump_change_seq(), 2);
            assert_eq!(s.change_seq, 2);
            let e0 = s.epoch;
            s.bump_epoch();
            assert_eq!(s.epoch, e0 + 1);
        });
    }

    /// Cold restore must reseat `change_seq = max(mod_seq)` AND seed the three
    /// section watermarks strictly above it (decision 3): queue/subagents/state
    /// are ephemeral and must re-send on the first post-restart delta.
    #[gpui::test]
    fn init_change_seq_seeds_section_watermarks_above_max(cx: &mut TestAppContext) {
        let session = cx.update(|cx| cx.new(|_| build_session()));
        session.update(cx, |s, cx| {
            // Three restored entries stamped mod_seq 1..=3 (N = 3).
            let entries = (1..=3u64)
                .map(|mod_seq| SessionEntry {
                    created_ms: 0,
                    mod_seq,
                    subagent_id: None,
                    kind: crate::session_entry::SessionEntryKind::UserMessage {
                        id: None,
                        content_md: "x".into(),
                        chunks: vec![],
                    },
                })
                .collect::<Vec<_>>();
            s.set_entries(entries, cx);
            s.init_change_seq_from_entries();

            // change_seq advanced to N + 3, watermarks each distinct and > N.
            assert_eq!(s.change_seq, 6, "change_seq must be max(mod_seq) + 3");
            assert_eq!(s.queue_seq, 4, "queue_seq = N + 1");
            assert_eq!(s.subagents_seq, 5, "subagents_seq = N + 2");
            assert_eq!(s.state_seq, 6, "state_seq = N + 3");
            for w in [s.queue_seq, s.subagents_seq, s.state_seq] {
                assert!(w > 3, "watermark {w} must be strictly above max(mod_seq)=3");
            }
        });
    }

    // -----------------------------------------------------------------------
    // Phase 6d-A — background shells folded into `streams` as Shell tabs
    // -----------------------------------------------------------------------

    fn insert_running_shell(s: &mut SolutionSession, id: &str, tail: Option<&str>) {
        let shell_id = crate::background_shell::BackgroundShellId::new(id);
        s.background_shells.insert(
            shell_id.clone(),
            crate::background_shell::BackgroundShell {
                id: shell_id.clone(),
                command: SharedString::from("echo hi"),
                output_path: PathBuf::from("/tmp/x.output"),
                registered_at: Utc::now(),
                latest: tail.map(|t| crate::background_shell::BackgroundShellSnapshot {
                    mtime: std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_720_000_000),
                    output_tail: SharedString::from(t.to_string()),
                }),
                last_offset: 0,
                state: crate::background_shell::ShellRuntimeState::Running,
            },
        );
        s.background_shell_order.push(shell_id);
    }

    #[test]
    fn rebuild_streams_folds_a_running_shell_into_a_shell_stream() {
        use crate::stream::{StreamId, StreamKind, StreamState};
        let mut s = build_session();
        insert_running_shell(&mut s, "bvb4ful1z", Some("hello\n"));
        s.rebuild_streams();

        let sid = StreamId::Shell(crate::background_shell::BackgroundShellId::new("bvb4ful1z"));
        let stream = s.streams.get(&sid).expect("running shell yields a Shell stream");
        assert_eq!(stream.kind, StreamKind::Shell);
        assert_eq!(stream.state, StreamState::Live);
        assert_eq!(stream.entries.len(), 1, "one fenced-output entry");
        // Shell streams sort AFTER Main (IndexMap insertion order = Main first).
        let ids: Vec<&StreamId> = s.streams.keys().collect();
        assert_eq!(ids.first(), Some(&&StreamId::Main));
        assert_eq!(ids.last(), Some(&&sid));
        // Per-stream `seq` picked up from the entry's mtime-based mod_seq.
        assert_eq!(stream.seq, 1_720_000_000_000);
    }

    #[test]
    fn rebuild_streams_auto_closes_a_terminal_shell() {
        use crate::stream::StreamId;
        let mut s = build_session();
        insert_running_shell(&mut s, "bvb4ful1z", Some("hello\n"));
        s.rebuild_streams();
        let sid = StreamId::Shell(crate::background_shell::BackgroundShellId::new("bvb4ful1z"));
        assert!(s.streams.contains_key(&sid), "running → present");

        // Flip to a terminal state (as `mark_background_shell_state` would).
        if let Some(shell) = s.background_shells.get_mut(
            &crate::background_shell::BackgroundShellId::new("bvb4ful1z"),
        ) {
            shell.state = crate::background_shell::ShellRuntimeState::Exited(Some(0));
        }
        s.rebuild_streams();
        assert!(
            !s.streams.contains_key(&sid),
            "a terminal shell is skipped → its stream auto-closes"
        );
        // Main is untouched.
        assert!(s.streams.contains_key(&StreamId::Main));
    }

    #[test]
    fn rebuild_streams_shell_streams_survive_an_entries_driven_rebuild() {
        // The shell stream is DERIVED from `background_shells`, so a rebuild that
        // also demuxes fresh `entries` must not wipe it.
        use crate::stream::StreamId;
        let mut s = build_session();
        insert_running_shell(&mut s, "bvb4ful1z", Some("out\n"));
        s.entries = vec![SessionEntry {
            created_ms: 0,
            mod_seq: 1,
            subagent_id: None,
            kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                chunks: vec![crate::session_entry::AssistantChunk::Message("main".into())],
            },
        }];
        s.rebuild_streams();
        let sid = StreamId::Shell(crate::background_shell::BackgroundShellId::new("bvb4ful1z"));
        assert!(s.streams.contains_key(&sid), "shell survives an entries rebuild");
        assert!(!s.streams[&StreamId::Main].entries.is_empty(), "Main demux still ran");
    }
