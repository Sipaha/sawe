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
        solution_id: SolutionId(10),
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
        pending_stop: std::collections::HashSet::new(),
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
            vec![
                msg_tagged("sub1", Some("T1")),
                msg_tagged("sub2", Some("T2")),
            ],
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

#[test]
fn clear_closed_streams_drops_buffered_pending_stop() {
    let mut session = build_session();
    session
        .pending_stop
        .insert(crate::background_agent::BackgroundAgentId::new(
            "a30f92a688e431edc",
        ));
    session.clear_closed_streams();
    assert!(
        session.pending_stop.is_empty(),
        "a context reset drops a buffered stop for an agent that never registered"
    );
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
            s.streams[&StreamId::Main].seq,
            seq0,
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
            s.streams[&StreamId::Main].seq,
            3,
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
        s.set_entries(vec![msg_seq("m", None, 1), msg_seq("t", Some("T1"), 2)], cx);
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
        assert!(
            s.streams[&StreamId::Main].seq > main_now,
            "Main seq advanced"
        );
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
        assert!(
            s.streams.contains_key(&t1),
            "teammate present before hydrate"
        );

        s.hydrate_streams_main_only();
        assert_eq!(s.streams.len(), 1, "only Main survives hydration");
        assert!(s.streams.contains_key(&StreamId::Main));
        assert!(
            !s.streams.contains_key(&t1),
            "teammate collapsed to Main-only"
        );
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
        s.entries = vec![
            Arc::new(msg_tagged("main", None)),
            Arc::new(msg_tagged("sub", Some("T1"))),
        ];

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
                user("hello"),                            // Main
                assistant("hi there", None),              // Main
                assistant("sub work 1", Some("toolu_1")), // teammate
                assistant("back to main", None),          // Main — reunites with "hi there"
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
            .map(|mod_seq| {
                std::sync::Arc::new(SessionEntry {
                    created_ms: 0,
                    mod_seq,
                    subagent_id: None,
                    kind: crate::session_entry::SessionEntryKind::UserMessage {
                        id: None,
                        content_md: "x".into(),
                        chunks: vec![],
                    },
                })
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
    let stream = s
        .streams
        .get(&sid)
        .expect("running shell yields a Shell stream");
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

// -----------------------------------------------------------------------
// Phase 6d-B follow-up — detached background AGENTS folded into `streams`
// as Teammate tabs (they have no `subagent_id`-tagged parent-thread entries
// for `demux` to build a stream from).
// -----------------------------------------------------------------------

fn insert_agent(
    s: &mut SolutionSession,
    agent_id: &str,
    parent_toolu: &str,
    stop_reason: Option<&str>,
) {
    let id = crate::background_agent::BackgroundAgentId::new(agent_id);
    s.background_agents.insert(
        id.clone(),
        crate::background_agent::BackgroundAgent {
            id: id.clone(),
            jsonl_path: PathBuf::from("/tmp/a.output"),
            registered_at: Utc::now(),
            latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_720_000_000),
                activity_label: SharedString::from("Bash: cargo test"),
                stop_reason: stop_reason.map(SharedString::from),
                usage_limited: false,
            }),
            last_offset: 0,
            parent_tool_use_id: Some(SharedString::from(parent_toolu)),
            // Distinctive change_seq-axis stamp — the fold entry's mod_seq
            // (and thus stream.seq) must ride this, NOT the mtime.
            latest_seq: 7,
            killed: false,
        },
    );
    s.background_agent_order.push(id);
}

#[test]
fn rebuild_streams_folds_a_live_background_agent_into_a_teammate_stream() {
    use crate::stream::{StreamId, StreamKind, StreamState};
    let mut s = build_session();
    insert_agent(&mut s, "ade80a6e3fce0efbe", "toolu_parent1", None);
    s.rebuild_streams();

    let sid = StreamId::Teammate(SharedString::from("toolu_parent1"));
    let stream = s
        .streams
        .get(&sid)
        .expect("a live detached background agent yields a Teammate stream/pill");
    assert_eq!(stream.kind, StreamKind::Teammate);
    assert_eq!(stream.state, StreamState::Live);
    assert_eq!(stream.entries.len(), 1, "one activity-snapshot entry");
    assert_eq!(
        stream.seq, 7,
        "stream.seq rides the change_seq-axis latest_seq, not the mtime"
    );
}

#[test]
fn rebuild_streams_skips_a_terminal_background_agent() {
    use crate::stream::StreamId;
    let mut s = build_session();
    insert_agent(
        &mut s,
        "ade80a6e3fce0efbe",
        "toolu_parent1",
        Some("end_turn"),
    );
    s.rebuild_streams();

    let sid = StreamId::Teammate(SharedString::from("toolu_parent1"));
    assert!(
        s.streams.get(&sid).is_none(),
        "a terminal (stop_reason) agent is not folded — tick_background_agents drops it"
    );
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
    if let Some(shell) =
        s.background_shells
            .get_mut(&crate::background_shell::BackgroundShellId::new(
                "bvb4ful1z",
            ))
    {
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
    s.entries = vec![Arc::new(SessionEntry {
        created_ms: 0,
        mod_seq: 1,
        subagent_id: None,
        kind: crate::session_entry::SessionEntryKind::AssistantMessage {
            chunks: vec![crate::session_entry::AssistantChunk::Message("main".into())],
        },
    })];
    s.rebuild_streams();
    let sid = StreamId::Shell(crate::background_shell::BackgroundShellId::new("bvb4ful1z"));
    assert!(
        s.streams.contains_key(&sid),
        "shell survives an entries rebuild"
    );
    assert!(
        !s.streams[&StreamId::Main].entries.is_empty(),
        "Main demux still ran"
    );
}

// ---------------------------------------------------------------------------
// Measurement harness (task-rebuild-streams). `#[ignore]`d: it is a timing
// probe, not an assertion. Run with
//
//   cargo test -p solution_agent --lib \
//     --config 'profile.dev.package.solution_agent.opt-level=3' \
//     bench_rebuild -- --ignored --nocapture
//
// The `--config` override raises ONLY this crate (`serde_json`, where a large
// share of the copied bytes live, is already at opt-level 3 in
// `[profile.dev.package]`). Do NOT reach for `--release` here: it rebuilds the
// whole dependency tree into the shared `target/`, which this repo's build
// conventions rule out for agent-driven checks.
// ---------------------------------------------------------------------------

/// Build a synthetic transcript whose *shape* matches the maintainer's largest
/// real session as measured in FORK.md #105/#107: 1,520 entry rows / 5.3 MB of
/// source payload bytes. Deterministic (a tiny xorshift), no I/O, no database.
#[cfg(test)]
pub(crate) fn synthetic_transcript(entry_count: usize, images: usize) -> Vec<Arc<SessionEntry>> {
    use crate::session_entry::{AssistantChunk, SessionEntryKind, ToolStatus};
    let mut rng: u64 = 0x5eed_1234_9876_4321;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let filler = |n: usize, seed: u64| -> String {
        // Real payloads are prose/code, not one repeated byte: build a string of
        // `n` bytes out of a rotating word list so the allocator and the memcpy
        // see a realistic size distribution.
        const WORDS: [&str; 8] = [
            "the ",
            "quick ",
            "brown ",
            "fox_jumps ",
            "over(the) ",
            "lazy=dog; ",
            "0x1f3b ",
            "\u{2014}nbsp ",
        ];
        let mut out = String::with_capacity(n + 16);
        let mut i = seed as usize;
        while out.len() < n {
            out.push_str(WORDS[i % WORDS.len()]);
            i += 1;
        }
        out
    };
    let mut entries = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let r = next();
        let bucket = r % 10;
        let kind = if i < images {
            // Pasted screenshot: a retained base64 payload on a user message.
            SessionEntryKind::UserMessage {
                id: Some(format!("um-{i}")),
                content_md: filler(200, r),
                chunks: vec![
                    acp::ContentBlock::Text(acp::TextContent::new(filler(200, r))),
                    acp::ContentBlock::Image(acp::ImageContent::new(
                        filler(180_000, r),
                        "image/png".to_string(),
                    )),
                ],
            }
        } else if bucket < 4 {
            SessionEntryKind::AssistantMessage {
                chunks: (0..(1 + (r as usize % 3)))
                    .map(|c| {
                        if c % 3 == 2 {
                            AssistantChunk::Thought(filler(600, r + c as u64))
                        } else {
                            AssistantChunk::Message(filler(1_400, r + c as u64))
                        }
                    })
                    .collect(),
            }
        } else if bucket < 8 {
            SessionEntryKind::ToolCall {
                id: format!("toolu_{i:08x}"),
                label_md: filler(80, r),
                kind: acp::ToolKind::Execute,
                status: ToolStatus::Completed,
                content_md: vec![filler(1_200, r), filler(900, r + 1)],
                raw_input: Some(serde_json::json!({
                    "command": filler(300, r),
                    "description": filler(120, r),
                    "nested": {"a": [1, 2, 3], "b": filler(200, r)},
                })),
                raw_output: Some(serde_json::json!({
                    "stdout": filler(1_500, r),
                    "stderr": "",
                    "meta": {"exit": 0, "lines": [filler(120, r), filler(120, r + 1)]},
                })),
                tool_name: Some("Bash".to_string()),
                locations: Vec::new(),
                status_started_at: Some(1_700_000_000_000),
            }
        } else if bucket < 9 {
            SessionEntryKind::UserMessage {
                id: Some(format!("um-{i}")),
                content_md: filler(900, r),
                chunks: vec![acp::ContentBlock::Text(acp::TextContent::new(filler(
                    900, r,
                )))],
            }
        } else {
            SessionEntryKind::System {
                level: crate::session_entry::SystemEntryLevel::Info,
                text_md: filler(400, r),
            }
        };
        entries.push(Arc::new(SessionEntry {
            created_ms: 1_700_000_000_000 + i as i64,
            mod_seq: i as u64 + 1,
            // ~1.4% of entries carry a teammate tag (1/10 x 1/7), over 5 runs.
            subagent_id: if bucket == 9 && i % 7 == 0 {
                Some(SharedString::from(format!("toolu_team{}", i % 5)))
            } else {
                None
            },
            kind,
        }));
    }
    entries
}

#[cfg(test)]
fn approx_payload_bytes(entries: &[Arc<SessionEntry>]) -> usize {
    entries
        .iter()
        .map(|e| serde_json::to_vec(e).map(|v| v.len()).unwrap_or(0))
        .sum()
}

#[test]
#[ignore = "timing probe, not an assertion"]
fn bench_rebuild_streams() {
    for n in [100usize, 500, 1_000, 1_520] {
        let entries = synthetic_transcript(n, if n >= 1_000 { 4 } else { 1 });
        let bytes = approx_payload_bytes(&entries);
        let mut s = build_session();
        s.entries = entries;
        // Warm.
        s.rebuild_streams();
        let iterations = 20;
        let clones_before = crate::session_entry::deep_clone_census::taken();
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            s.rebuild_streams();
        }
        let per = start.elapsed() / iterations;
        let clones =
            (crate::session_entry::deep_clone_census::taken() - clones_before) / iterations as u64;
        println!(
            "rebuild_streams n={n:>5} payload={:>8.2} MB  ->  {:>9.3} ms/call  ({:>6.1} calls per 16.6ms frame)  deep entry clones/call: {clones}",
            bytes as f64 / 1_048_576.0,
            per.as_secs_f64() * 1000.0,
            16.6 / (per.as_secs_f64() * 1000.0).max(0.000_001),
        );
    }
}

// ---------------------------------------------------------------------------
// The `streams` mirror shares its entries with `session.entries` behind `Arc`
// (task-rebuild-streams). Two things have to stay true, and neither is visible
// to a type checker:
//   1. `demux` still groups/coalesces/stamps EXACTLY as an owned implementation
//      would — pinned against an independently-written reference below;
//   2. a rebuild never writes through the sharing into `session.entries`, and is
//      therefore idempotent. Before `Arc::make_mut` guarded the coalesce merge,
//      a second rebuild would have appended the same chunks to the shared entry
//      again.
// ---------------------------------------------------------------------------

/// Deliberately naive, fully-owned reference demux: a `Vec` of buckets found by
/// linear search, deep-cloning every entry. Written out separately (no
/// `IndexMap`, no `Arc`, no `push_coalesced`) so it cannot share a bug with the
/// implementation it is checking.
#[cfg(test)]
fn reference_demux(
    entries: &[Arc<SessionEntry>],
) -> Vec<(
    crate::stream::StreamId,
    SharedString,
    u64,
    Vec<SessionEntry>,
)> {
    use crate::session_entry::SessionEntryKind;
    use crate::stream::StreamId;
    let mut buckets: Vec<(StreamId, SharedString, Vec<SessionEntry>)> =
        vec![(StreamId::Main, SharedString::new_static("Main"), Vec::new())];
    for entry in entries {
        let (id, label) = match &entry.subagent_id {
            None => (StreamId::Main, SharedString::new_static("Main")),
            Some(toolu) => (StreamId::Teammate(toolu.clone()), toolu.clone()),
        };
        let position = match buckets.iter().position(|(key, _, _)| *key == id) {
            Some(position) => position,
            None => {
                buckets.push((id, label, Vec::new()));
                buckets.len() - 1
            }
        };
        let bucket = &mut buckets[position].2;
        let incoming: SessionEntry = (**entry).clone();
        let merged = match (bucket.last_mut(), &incoming.kind) {
            (Some(last), SessionEntryKind::AssistantMessage { chunks: fresh }) => {
                match &mut last.kind {
                    SessionEntryKind::AssistantMessage { chunks } => {
                        chunks.extend(fresh.iter().cloned());
                        last.mod_seq = last.mod_seq.max(incoming.mod_seq);
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if !merged {
            bucket.push(incoming);
        }
    }
    buckets
        .into_iter()
        .map(|(id, label, entries)| {
            let seq = entries.iter().map(|e| e.mod_seq).max().unwrap_or(0);
            (id, label, seq, entries)
        })
        .collect()
}

/// A tiny deterministic PRNG so a failing case is reproducible from its seed.
#[cfg(test)]
struct Xorshift(u64);

#[cfg(test)]
impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

#[cfg(test)]
fn random_entry(rng: &mut Xorshift, mod_seq: u64) -> Arc<SessionEntry> {
    use crate::session_entry::{AssistantChunk, SessionEntryKind, SystemEntryLevel, ToolStatus};
    // Four teammate tags plus `None`, so runs of same-source assistant messages
    // (the coalescing case) and interleavings that BREAK a run both occur.
    let subagent_id = match rng.below(5) {
        0 => Some(SharedString::from("toolu_a")),
        1 => Some(SharedString::from("toolu_b")),
        2 => Some(SharedString::from("toolu_c")),
        _ => None,
    };
    // Weighted toward AssistantMessage: that is the only coalescing kind, and a
    // transcript of mostly non-mergeable entries would exercise nothing.
    let kind = match rng.below(10) {
        0..=5 => SessionEntryKind::AssistantMessage {
            chunks: (0..=rng.below(3))
                .map(|c| {
                    if c % 2 == 0 {
                        AssistantChunk::Message(format!("m{}", rng.next() % 1000))
                    } else {
                        AssistantChunk::Thought(format!("t{}", rng.next() % 1000))
                    }
                })
                .collect(),
        },
        6 | 7 => SessionEntryKind::ToolCall {
            id: format!("toolu_{}", rng.next() % 1000),
            label_md: "Bash".to_string(),
            kind: acp::ToolKind::Execute,
            status: ToolStatus::InProgress,
            content_md: vec![format!("out{}", rng.next() % 100)],
            raw_input: Some(serde_json::json!({"cmd": rng.next() % 100})),
            raw_output: None,
            tool_name: Some("Bash".to_string()),
            locations: Vec::new(),
            status_started_at: None,
        },
        8 => SessionEntryKind::UserMessage {
            id: None,
            content_md: format!("u{}", rng.next() % 1000),
            chunks: vec![acp::ContentBlock::Text(acp::TextContent::new(format!(
                "u{}",
                rng.next() % 1000
            )))],
        },
        _ => SessionEntryKind::System {
            level: SystemEntryLevel::Info,
            text_md: format!("s{}", rng.next() % 1000),
        },
    };
    Arc::new(SessionEntry {
        created_ms: 1_700_000_000_000 + mod_seq as i64,
        mod_seq,
        subagent_id,
        kind,
    })
}

#[cfg(test)]
fn assert_mirror_matches_reference(session: &SolutionSession, seed: u64, step: &str) {
    let expected = reference_demux(&session.entries);
    let actual: Vec<_> = session
        .streams
        .iter()
        .map(|(id, stream)| {
            (
                id.clone(),
                stream.label.clone(),
                stream.seq,
                stream
                    .entries
                    .iter()
                    .map(|entry| (**entry).clone())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        actual.len(),
        expected.len(),
        "seed {seed} / {step}: stream COUNT diverged from the owned reference"
    );
    for (index, (expected, actual)) in expected.iter().zip(actual.iter()).enumerate() {
        assert_eq!(
            actual.0, expected.0,
            "seed {seed} / {step}: stream #{index} id/order diverged"
        );
        assert_eq!(
            actual.1, expected.1,
            "seed {seed} / {step}: stream #{index} label diverged"
        );
        assert_eq!(
            actual.2, expected.2,
            "seed {seed} / {step}: stream #{index} seq diverged"
        );
        assert_eq!(
            actual.3, expected.3,
            "seed {seed} / {step}: stream #{index} entries diverged"
        );
    }
}

/// How many coalesce merges the mirror performed: for each stream, the number
/// of flat entries ROUTED to it minus the number of entries it ended up with.
/// Computed from the flat transcript, independently of `push_coalesced`, so a
/// merge that stops happening shows up here as a zero rather than as silence.
#[cfg(test)]
fn merges_performed(session: &SolutionSession) -> usize {
    use crate::stream::StreamId;
    let mut routed: HashMap<StreamId, usize> = HashMap::new();
    for entry in &session.entries {
        let id = match &entry.subagent_id {
            None => StreamId::Main,
            Some(toolu) => StreamId::Teammate(toolu.clone()),
        };
        *routed.entry(id).or_default() += 1;
    }
    session
        .streams
        .iter()
        .map(|(id, stream)| {
            routed
                .get(id)
                .copied()
                .unwrap_or(0)
                .saturating_sub(stream.entries.len())
        })
        .sum()
}

#[test]
fn shared_mirror_demuxes_identically_to_an_owned_reference() {
    // Property: over randomised transcripts — coalescing assistant runs,
    // teammate-tagged entries, and repeated in-place updates — the shared
    // mirror is byte-identical to the owned reference demux, the flat entries
    // are never written through the sharing, and a rebuild is idempotent.
    // Anti-vacuity: `push_coalesced`'s merge is the ONLY write into the mirror
    // and the entire reason `Arc::make_mut` is there, so the property test is
    // worthless if the generator drifts into never producing an adjacent
    // same-stream assistant pair. Count the merges that actually happen — a
    // "some stream is shorter than the flat list" proxy is satisfied by a bare
    // teammate split and would stay green through exactly that drift.
    let mut merges_seen = 0usize;
    let mut seeds_with_a_merge = 0usize;
    let mut checked_a_teammate = false;
    for seed in 1..=200u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut session = build_session();
        let count = rng.below(24);
        session.entries = (0..count)
            .map(|i| random_entry(&mut rng, i as u64 + 1))
            .collect();
        let flat_before: Vec<SessionEntry> =
            session.entries.iter().map(|e| (**e).clone()).collect();

        session.rebuild_streams();
        assert_mirror_matches_reference(&session, seed, "initial");
        let merges_here = merges_performed(&session);
        merges_seen += merges_here;
        seeds_with_a_merge += usize::from(merges_here > 0);
        checked_a_teammate |= session.streams.len() > 1;
        // The flat transcript is the source of truth and a rebuild must never
        // write through the sharing into it. Compared BY VALUE, and here in
        // particular: this is the point at which the first merge has just run.
        assert_eq!(
            session
                .entries
                .iter()
                .map(|e| (**e).clone())
                .collect::<Vec<_>>(),
            flat_before,
            "seed {seed}: the first rebuild wrote through into the flat transcript"
        );

        // Idempotence: rebuilding without touching `entries` must be a no-op.
        // Pre-`Arc::make_mut`, a merge wrote into the shared original, so a
        // second rebuild re-appended the same chunks.
        session.rebuild_streams();
        assert_mirror_matches_reference(&session, seed, "second rebuild");
        assert_eq!(
            session
                .entries
                .iter()
                .map(|e| (**e).clone())
                .collect::<Vec<_>>(),
            flat_before,
            "seed {seed}: the second rebuild wrote through into the flat transcript"
        );

        // In-place updates — the `EntryUpdated` shape — replacing an entry and
        // advancing its `mod_seq`, exactly as the store's arm does.
        let mut next_seq = count as u64 + 1;
        for update in 0..5 {
            if session.entries.is_empty() {
                break;
            }
            let index = rng.below(session.entries.len());
            next_seq += 1;
            session.entries[index] = random_entry(&mut rng, next_seq);
            // Snapshot AFTER the edit and BEFORE the rebuild, so the comparison
            // below is about what the rebuild did, not about what the edit did.
            let flat_expected: Vec<SessionEntry> =
                session.entries.iter().map(|e| (**e).clone()).collect();
            session.rebuild_streams();
            assert_mirror_matches_reference(&session, seed, &format!("update {update}"));
            merges_seen += merges_performed(&session);
            assert_eq!(
                session
                    .entries
                    .iter()
                    .map(|e| (**e).clone())
                    .collect::<Vec<_>>(),
                flat_expected,
                "seed {seed} / update {update}: the rebuild wrote through into the flat transcript"
            );
        }
    }
    assert!(
        seeds_with_a_merge >= 40 && merges_seen >= 100,
        "the generator produced only {merges_seen} coalesce merges over {seeds_with_a_merge} of \
         200 seeds — `push_coalesced`'s merge, the mirror's only write, is barely exercised and \
         the property test is close to vacuous"
    );
    assert!(
        checked_a_teammate,
        "the generator never produced a teammate stream — the property test would be vacuous"
    );
}

#[test]
fn rebuild_never_writes_through_the_shared_entries() {
    // The sharpest form of the aliasing guard: a coalescing run, rebuilt twice,
    // must leave the flat entries byte-identical AND must not grow the merged
    // stream entry's chunk list on the second pass.
    use crate::session_entry::SessionEntryKind;
    let mut s = build_session();
    s.entries = vec![
        Arc::new(msg_tagged("first ", None)),
        Arc::new(msg_tagged("interleaved", Some("T1"))),
        Arc::new(msg_tagged("second", None)),
    ];
    let flat_before: Vec<SessionEntry> = s.entries.iter().map(|e| (**e).clone()).collect();
    s.rebuild_streams();
    let chunks_after_first = match &s.streams[&crate::stream::StreamId::Main].entries[0].kind {
        SessionEntryKind::AssistantMessage { chunks } => chunks.len(),
        other => panic!("expected a coalesced AssistantMessage, got {other:?}"),
    };
    assert_eq!(
        chunks_after_first, 2,
        "the two Main fragments must coalesce"
    );
    s.rebuild_streams();
    let chunks_after_second = match &s.streams[&crate::stream::StreamId::Main].entries[0].kind {
        SessionEntryKind::AssistantMessage { chunks } => chunks.len(),
        other => panic!("expected a coalesced AssistantMessage, got {other:?}"),
    };
    assert_eq!(
        chunks_after_second, 2,
        "a second rebuild re-grew the merged entry — the merge wrote through the sharing"
    );
    let flat_after: Vec<SessionEntry> = s.entries.iter().map(|e| (**e).clone()).collect();
    assert_eq!(
        flat_after, flat_before,
        "the flat transcript must be untouched by a rebuild"
    );
}

#[test]
fn rebuild_streams_work_does_not_scale_with_transcript_length() {
    // Fails without the shared mirror: `demux` used to deep-clone EVERY entry,
    // so the census below read `entries.len()` and quadrupled with the
    // transcript. Deterministic — a clone census, not a timing bound — so there
    // is nothing here to be flaky about.
    use crate::session_entry::deep_clone_census;

    // A transcript with NO adjacent same-stream assistant pair coalesces
    // nothing, so a correct rebuild deep-copies exactly zero entries.
    let uniform = |count: usize| -> Vec<Arc<SessionEntry>> {
        (0..count)
            .map(|i| {
                Arc::new(if i % 2 == 0 {
                    msg_seq("assistant", None, i as u64 + 1)
                } else {
                    SessionEntry {
                        created_ms: 0,
                        mod_seq: i as u64 + 1,
                        subagent_id: None,
                        kind: crate::session_entry::SessionEntryKind::System {
                            level: crate::session_entry::SystemEntryLevel::Info,
                            text_md: "boundary".to_string(),
                        },
                    }
                })
            })
            .collect()
    };

    let mut clones_at = Vec::new();
    for count in [64usize, 1024] {
        let mut s = build_session();
        s.entries = uniform(count);
        s.rebuild_streams();
        let before = deep_clone_census::taken();
        s.rebuild_streams();
        clones_at.push((count, deep_clone_census::taken() - before));
    }
    assert_eq!(
        clones_at,
        vec![(64usize, 0u64), (1024usize, 0u64)],
        "a rebuild over a non-coalescing transcript must deep-copy nothing, at any length"
    );

    // With coalescing, the cost is the number of merge GROUPS (one forked head
    // each), not the number of entries — so it stays flat as the transcript
    // grows around a fixed number of groups.
    let with_groups = |tail: usize| -> Vec<Arc<SessionEntry>> {
        let mut entries = vec![
            Arc::new(msg_seq("run a1", None, 1)),
            Arc::new(msg_seq("run a2", None, 2)),
            Arc::new(msg_seq("t1", Some("T1"), 3)),
            Arc::new(msg_seq("t2", Some("T1"), 4)),
        ];
        for i in 0..tail {
            entries.push(Arc::new(SessionEntry {
                created_ms: 0,
                mod_seq: 5 + i as u64,
                subagent_id: None,
                kind: crate::session_entry::SessionEntryKind::System {
                    level: crate::session_entry::SystemEntryLevel::Info,
                    text_md: "filler".to_string(),
                },
            }));
        }
        entries
    };
    let mut group_clones = Vec::new();
    for tail in [8usize, 2048] {
        let mut s = build_session();
        s.entries = with_groups(tail);
        s.rebuild_streams();
        let before = deep_clone_census::taken();
        s.rebuild_streams();
        group_clones.push((tail, deep_clone_census::taken() - before));
    }
    assert_eq!(
        group_clones,
        vec![(8usize, 2u64), (2048usize, 2u64)],
        "a rebuild must fork exactly one head per coalesced run (2 runs here), \
         independent of transcript length"
    );
}

// ---------------------------------------------------------------------------
// Property test #2 — `rebuild_streams`'s DECORATION, over randomised sessions.
//
// `shared_mirror_demuxes_identically_to_an_owned_reference` above pins
// `demux` ≡ reference and NOTHING else: its `build_session()` leaves
// `closed_streams`, `hydration_orphan_streams`, `background_shells` and
// `background_agents` empty, so every step `rebuild_streams` runs *after* the
// demux is a no-op on all 200 of those seeds. This second property drives the
// same generator machinery with all four populated.
//
// DESIGN CHOICE — reference, not invariants. The decoration is a
// set-and-order transform (which ids survive, where a folded id lands, what
// label / state / seq it carries), and two of its steps — the never-clobber
// rule and the `closed_streams` age-out — are "…and nothing else happened"
// properties. A total comparison catches a stream that should NOT be there but
// IS, and an order shift; a hand-listed invariant set only catches the
// invariants someone thought to write down, and would additionally have had no
// expected value to compare `entries` / `label` / `seq` against at all. The
// drift risk is mitigated, not removed: `reference_decorated_mirror` shares no
// code with `rebuild_streams` (an ordered `Vec` of shapes with an explicit
// upsert, not an `IndexMap` mutated in place), and every decoration step has a
// production mutation that this test is confirmed to catch.
//
// What it does NOT cover:
//   * the fold *bodies*. `BackgroundShell::stream_entry` /
//     `BackgroundAgent::stream_entry` are called by the reference too, so this
//     test pins WHICH snapshot is folded and WHERE, not how it renders — that
//     is `background_shell.rs` / `background_agent.rs`'s own unit tests.
//   * the two `Utc::now()` reads. They are bracketed (see
//     `assert_decorated_mirror`), not injected, so a change to the "observed X
//     ago" bucketing is invisible here.
//   * `closed_streams` keyed by a non-`Teammate` id. `close_stream` is only
//     ever called with `StreamId::Teammate` in production, so generating a
//     closed `Shell` id would pin behaviour outside the contract.
//   * anything `cx`-shaped: notifications, persistence, the wire encoders.
// ---------------------------------------------------------------------------

/// A `Stream` flattened to plain comparable data — every field the mirror
/// exposes, including the three (`kind`, `state`, `source`) that
/// `assert_mirror_matches_reference` does not look at because the plain demux
/// never varies them. The decoration does: the background-agent fold re-states
/// an existing teammate stream and stamps `FileTail` on a fresh one.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
struct StreamShape {
    id: crate::stream::StreamId,
    kind: crate::stream::StreamKind,
    label: SharedString,
    state: crate::stream::StreamState,
    source: crate::stream::StreamSource,
    seq: u64,
    entries: Vec<SessionEntry>,
}

#[cfg(test)]
fn mirror_shape(session: &SolutionSession) -> Vec<StreamShape> {
    session
        .streams
        .iter()
        .map(|(id, stream)| StreamShape {
            id: id.clone(),
            kind: stream.kind,
            label: stream.label.clone(),
            state: stream.state.clone(),
            source: stream.source.clone(),
            seq: stream.seq,
            entries: stream
                .entries
                .iter()
                .map(|entry| (**entry).clone())
                .collect(),
        })
        .collect()
}

/// Teammate ids re-tagged by an entry at or after the hydration watermark —
/// the set that reopens a suppressed orphan.
#[cfg(test)]
fn streamed_anew(session: &SolutionSession) -> std::collections::HashSet<crate::stream::StreamId> {
    session
        .entries
        .iter()
        .skip(session.hydration_watermark)
        .filter_map(|entry| entry.subagent_id.clone())
        .map(crate::stream::StreamId::Teammate)
        .collect()
}

/// Owned reference for the WHOLE mirror: `reference_demux` plus every
/// decoration step, modelled independently of `rebuild_streams`.
#[cfg(test)]
fn reference_decorated_mirror(
    session: &SolutionSession,
    now: chrono::DateTime<Utc>,
) -> Vec<StreamShape> {
    use crate::stream::{StreamId, StreamKind, StreamSource, StreamState};

    let mut shapes: Vec<StreamShape> = reference_demux(&session.entries)
        .into_iter()
        .map(|(id, label, _seq, entries)| StreamShape {
            kind: match id {
                StreamId::Main => StreamKind::Main,
                StreamId::Teammate(_) => StreamKind::Teammate,
                StreamId::Shell(_) => StreamKind::Shell,
            },
            id,
            label,
            state: StreamState::Live,
            source: StreamSource::ParentThreadDemux,
            seq: 0,
            entries,
        })
        .collect();

    // Closed streams drop out permanently.
    shapes.retain(|shape| !session.closed_streams.contains_key(&shape.id));

    // Hydration orphans drop out unless a post-watermark entry re-tags them.
    let reopened = streamed_anew(session);
    shapes.retain(|shape| {
        !session.hydration_orphan_streams.contains(&shape.id) || reopened.contains(&shape.id)
    });

    // Running background shells fold in, in `background_shell_order`, after
    // Main + teammates. Modelled as an upsert because the production fold is an
    // `IndexMap::insert`: a repeated id replaces the value and keeps its slot.
    for id in &session.background_shell_order {
        let Some(shell) = session.background_shells.get(id) else {
            continue;
        };
        if !matches!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Running
        ) {
            continue;
        }
        let shape = StreamShape {
            id: StreamId::Shell(id.clone()),
            kind: StreamKind::Shell,
            label: shell.stream_label(),
            state: StreamState::Live,
            source: StreamSource::FileTail(shell.output_path.clone()),
            seq: 0,
            entries: vec![shell.stream_entry(now)],
        };
        match shapes.iter_mut().find(|existing| existing.id == shape.id) {
            Some(existing) => *existing = shape,
            None => shapes.push(shape),
        }
    }

    // Live background agents fold in as teammate streams.
    for id in &session.background_agent_order {
        let Some(agent) = session.background_agents.get(id) else {
            continue;
        };
        if !agent.renders_stream() {
            continue;
        }
        let Some(parent_toolu) = agent.parent_tool_use_id.clone() else {
            continue;
        };
        let key = StreamId::Teammate(parent_toolu.clone());
        if session.closed_streams.contains_key(&key) {
            continue;
        }
        match shapes.iter_mut().find(|existing| existing.id == key) {
            // Never clobber: only the liveness is the agent's to state.
            Some(existing) => existing.state = agent.stream_state(),
            None => shapes.push(StreamShape {
                id: key,
                kind: StreamKind::Teammate,
                label: parent_toolu,
                state: agent.stream_state(),
                source: StreamSource::FileTail(agent.jsonl_path.clone()),
                seq: 0,
                entries: vec![agent.stream_entry(now)],
            }),
        }
    }

    // Teammate label enrichment, falling back to the raw toolu.
    for shape in shapes.iter_mut() {
        if let StreamId::Teammate(toolu) = &shape.id {
            shape.label = session
                .teammate_labels
                .get(toolu)
                .cloned()
                .unwrap_or_else(|| toolu.clone());
        }
    }

    // Per-stream seq = the stream's high-water mark on the change_seq axis.
    for shape in shapes.iter_mut() {
        shape.seq = shape
            .entries
            .iter()
            .map(|entry| entry.mod_seq)
            .max()
            .unwrap_or(0);
    }

    shapes
}

/// Compare the mirror against the owned reference. `rebuild_streams` reads
/// `Utc::now()` internally for the shell / agent fold bodies, so the reference
/// is BRACKETED: `before` and `after` straddle the rebuild, the folded snapshot
/// mtimes are old enough that the "observed X ago" formatter is at day
/// granularity, and therefore one of the two brackets is exact. No tolerance is
/// introduced — the comparison stays byte-for-byte.
///
/// Returns how many streams it compared, which the caller accumulates and
/// asserts a floor on. That is not decoration: `-> usize` is what stops a
/// future edit (or a mutation-testing probe) from turning this into an
/// early-returning no-op that leaves the property green — a bare `return;`
/// then fails to compile rather than passing silently.
#[cfg(test)]
#[must_use]
fn assert_decorated_mirror(
    session: &SolutionSession,
    before: chrono::DateTime<Utc>,
    after: chrono::DateTime<Utc>,
    seed: u64,
    step: &str,
) -> usize {
    let actual = mirror_shape(session);
    let expected = {
        let at_after = reference_decorated_mirror(session, after);
        if actual == at_after {
            at_after
        } else {
            reference_decorated_mirror(session, before)
        }
    };
    let actual_ids: Vec<_> = actual.iter().map(|shape| shape.id.clone()).collect();
    let expected_ids: Vec<_> = expected.iter().map(|shape| shape.id.clone()).collect();
    assert_eq!(
        actual_ids, expected_ids,
        "seed {seed} / {step}: the decorated mirror's stream ids/order diverged \
         from the owned reference"
    );
    for (index, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            actual.label, expected.label,
            "seed {seed} / {step}: stream #{index} ({:?}) label diverged",
            actual.id
        );
        assert_eq!(
            actual.kind, expected.kind,
            "seed {seed} / {step}: stream #{index} ({:?}) kind diverged",
            actual.id
        );
        assert_eq!(
            actual.state, expected.state,
            "seed {seed} / {step}: stream #{index} ({:?}) state diverged",
            actual.id
        );
        assert_eq!(
            actual.source, expected.source,
            "seed {seed} / {step}: stream #{index} ({:?}) source diverged",
            actual.id
        );
        assert_eq!(
            actual.seq, expected.seq,
            "seed {seed} / {step}: stream #{index} ({:?}) seq diverged",
            actual.id
        );
        assert_eq!(
            actual.entries, expected.entries,
            "seed {seed} / {step}: stream #{index} ({:?}) entries diverged",
            actual.id
        );
    }
    actual.len()
}

/// Anti-vacuity ledger. Every count is derived from the session's own state
/// (plus `reference_demux`), NEVER from `session.streams` — so a mutation that
/// breaks the decoration cannot also fake the evidence that the decoration was
/// exercised, and a generator re-weight that stops producing one of these
/// shapes fails the threshold instead of going quiet.
#[cfg(test)]
#[derive(Default, Debug)]
struct DecorationCensus {
    closed_removed: usize,
    closed_removed_with_entries: usize,
    orphans_suppressed: usize,
    orphans_reopened: usize,
    shells_folded: usize,
    shells_terminal_skipped: usize,
    shells_without_output: usize,
    agents_folded_fresh: usize,
    agents_folded_over_suppressed: usize,
    agents_clobber_guarded: usize,
    agents_aged_out: usize,
    agents_terminal_skipped: usize,
    labels_enriched: usize,
    labels_fallback: usize,
    seeds_visibly_decorated: usize,
    /// Streams the reference comparison actually walked. Not a property of the
    /// decoration — a liveness check on `assert_decorated_mirror` itself.
    streams_compared: usize,
}

#[cfg(test)]
impl DecorationCensus {
    fn observe(&mut self, session: &SolutionSession) {
        use crate::stream::StreamId;
        let demuxed = reference_demux(&session.entries);
        let reopened = streamed_anew(session);

        let mut surviving: Vec<StreamId> = Vec::new();
        let mut suppressed: Vec<StreamId> = Vec::new();
        for (id, _, _, entries) in &demuxed {
            if session.closed_streams.contains_key(id) {
                self.closed_removed += 1;
                if !entries.is_empty() {
                    self.closed_removed_with_entries += 1;
                }
                suppressed.push(id.clone());
                continue;
            }
            if session.hydration_orphan_streams.contains(id) {
                if reopened.contains(id) {
                    self.orphans_reopened += 1;
                } else {
                    self.orphans_suppressed += 1;
                    suppressed.push(id.clone());
                    continue;
                }
            }
            surviving.push(id.clone());
        }

        for id in &session.background_shell_order {
            let Some(shell) = session.background_shells.get(id) else {
                continue;
            };
            if matches!(
                shell.state,
                crate::background_shell::ShellRuntimeState::Running
            ) {
                self.shells_folded += 1;
                if shell.latest.is_none() {
                    self.shells_without_output += 1;
                }
            } else {
                self.shells_terminal_skipped += 1;
            }
        }

        for id in &session.background_agent_order {
            let Some(agent) = session.background_agents.get(id) else {
                continue;
            };
            if !agent.renders_stream() {
                self.agents_terminal_skipped += 1;
                continue;
            }
            let Some(parent_toolu) = agent.parent_tool_use_id.clone() else {
                continue;
            };
            let key = StreamId::Teammate(parent_toolu);
            if session.closed_streams.contains_key(&key) {
                self.agents_aged_out += 1;
                continue;
            }
            if surviving.contains(&key) {
                self.agents_clobber_guarded += 1;
            } else {
                self.agents_folded_fresh += 1;
                if suppressed.contains(&key) {
                    self.agents_folded_over_suppressed += 1;
                }
                surviving.push(key);
            }
        }

        for id in &surviving {
            if let StreamId::Teammate(toolu) = id {
                if session.teammate_labels.contains_key(toolu) {
                    self.labels_enriched += 1;
                } else {
                    self.labels_fallback += 1;
                }
            }
        }

        // Did the decoration change the mirror AT ALL on this seed? Without
        // this, a generator that produced only inert decoration state would
        // still satisfy every counter above while comparing two plain demuxes.
        let decorated: Vec<StreamId> = surviving;
        let bare: Vec<StreamId> = demuxed.iter().map(|(id, _, _, _)| id.clone()).collect();
        let shell_folds = session
            .background_shell_order
            .iter()
            .filter(|id| {
                session.background_shells.get(*id).is_some_and(|shell| {
                    matches!(
                        shell.state,
                        crate::background_shell::ShellRuntimeState::Running
                    )
                })
            })
            .count();
        if decorated != bare || shell_folds > 0 {
            self.seeds_visibly_decorated += 1;
        }
    }
}

/// Populate a session with a randomised transcript AND randomised decoration
/// state. The toolu pool is shared with `random_entry`, so a generated
/// `closed_streams` / orphan / agent-parent id genuinely collides with a demux
/// stream most of the time; `toolu_d` never appears in a transcript, so an
/// agent parented on it exercises the fresh-fold path.
#[cfg(test)]
fn decorate_random_session(rng: &mut Xorshift, session: &mut SolutionSession) {
    use crate::stream::StreamId;
    const TOOLUS: [&str; 4] = ["toolu_a", "toolu_b", "toolu_c", "toolu_d"];

    let count = rng.below(18);
    session.entries = (0..count)
        .map(|index| random_entry(rng, index as u64 + 1))
        .collect();

    // Half the time exactly `entries.len()` — what `hydrate_streams_main_only`
    // actually stamps, so every orphan is purely cold and stays collapsed.
    // Otherwise anywhere in 0..=len, so an orphan's entries straddle the
    // boundary and it reopens. Both sides of the watermark matter: the
    // suppression and the reopen are different branches.
    session.hydration_watermark = if rng.below(2) == 0 {
        count
    } else {
        rng.below(count + 1)
    };

    for toolu in TOOLUS {
        if rng.below(3) == 0 {
            session
                .hydration_orphan_streams
                .insert(StreamId::Teammate(SharedString::from(toolu)));
        }
    }
    for toolu in TOOLUS {
        if rng.below(4) == 0 {
            let id = StreamId::Teammate(SharedString::from(toolu));
            // `close_stream`'s own invariant: a permanent Done-close drops the
            // reopenable orphan record AND the durable label, so a session
            // carrying both for one id is unreachable — don't generate one.
            session.hydration_orphan_streams.remove(&id);
            session
                .closed_streams
                .insert(id, SharedString::new_static("done"));
        }
    }
    for toolu in TOOLUS {
        let id = StreamId::Teammate(SharedString::from(toolu));
        if !session.closed_streams.contains_key(&id) && rng.below(2) == 0 {
            session.teammate_labels.insert(
                SharedString::from(toolu),
                SharedString::from(format!("Task: {toolu}")),
            );
        }
    }

    for index in 0..rng.below(3) {
        let id = crate::background_shell::BackgroundShellId::new(format!("sh{index}"));
        let state = match rng.below(5) {
            0 => crate::background_shell::ShellRuntimeState::Exited(Some(0)),
            1 => crate::background_shell::ShellRuntimeState::Killed,
            _ => crate::background_shell::ShellRuntimeState::Running,
        };
        let latest = if rng.below(5) == 0 {
            None
        } else {
            Some(crate::background_shell::BackgroundShellSnapshot {
                // Distinct per shell — and old enough that the "observed X ago"
                // formatter is at day granularity, which is what makes the
                // bracketed reference above exact. A fold that picks the WRONG
                // shell's snapshot shows up as a diverged `seq`.
                mtime: std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_600_000_000 + index as u64 * 86_400),
                output_tail: SharedString::from(format!("out{index}\n")),
            })
        };
        session.background_shells.insert(
            id.clone(),
            crate::background_shell::BackgroundShell {
                id: id.clone(),
                command: SharedString::from(format!("cargo test {index}")),
                output_path: PathBuf::from(format!("/tmp/sh{index}.output")),
                registered_at: Utc::now(),
                latest,
                last_offset: 0,
                state,
            },
        );
        session.background_shell_order.push(id);
    }

    for index in 0..rng.below(3) {
        let id = crate::background_agent::BackgroundAgentId::new(format!("agent{index}"));
        let parent_tool_use_id = if rng.below(6) == 0 {
            None
        } else {
            Some(SharedString::from(TOOLUS[rng.below(4)]))
        };
        let killed = rng.below(6) == 0;
        let latest = if rng.below(6) == 0 {
            None
        } else {
            Some(crate::background_agent::BackgroundAgentSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_600_000_000 + index as u64 * 86_400),
                activity_label: SharedString::from(format!("Bash: step {index}")),
                stop_reason: if rng.below(4) == 0 {
                    Some(SharedString::from("end_turn"))
                } else {
                    None
                },
                usage_limited: rng.below(8) == 0,
            })
        };
        session.background_agents.insert(
            id.clone(),
            crate::background_agent::BackgroundAgent {
                id: id.clone(),
                jsonl_path: PathBuf::from(format!("/tmp/agent{index}.jsonl")),
                registered_at: Utc::now(),
                latest,
                last_offset: 0,
                parent_tool_use_id,
                // Distinct per agent, on the change_seq axis — a fold that
                // stamps the wrong agent's snapshot diverges on `seq`.
                latest_seq: 900 + index as u64,
                killed,
            },
        );
        session.background_agent_order.push(id);
    }
}

#[test]
fn decorated_mirror_matches_an_owned_reference() {
    let mut census = DecorationCensus::default();
    for seed in 1..=300u64 {
        let mut rng =
            Xorshift((seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03) | 1);
        let mut session = build_session();
        decorate_random_session(&mut rng, &mut session);
        census.observe(&session);
        let flat_before: Vec<SessionEntry> = session
            .entries
            .iter()
            .map(|entry| (**entry).clone())
            .collect();

        // Twice: the decoration must be idempotent too. The shell / agent folds
        // are DERIVED, so a rebuild that also re-demuxes fresh entries must
        // neither wipe them nor accumulate a second copy.
        for pass in ["first", "second"] {
            let before = Utc::now();
            session.rebuild_streams();
            let after = Utc::now();
            census.streams_compared += assert_decorated_mirror(&session, before, after, seed, pass);
            assert_eq!(
                session
                    .entries
                    .iter()
                    .map(|entry| (**entry).clone())
                    .collect::<Vec<_>>(),
                flat_before,
                "seed {seed} / {pass} rebuild: wrote through into the flat transcript"
            );
        }

        // A live in-place update (the `EntryUpdated` shape) on top of the
        // decoration: the folds must survive it and the suppressions must not
        // be re-evaluated into a different answer by the fresh demux.
        if !session.entries.is_empty() {
            let index = rng.below(session.entries.len());
            session.entries[index] = random_entry(&mut rng, count_bound(&session) + 1);
            let flat_expected: Vec<SessionEntry> = session
                .entries
                .iter()
                .map(|entry| (**entry).clone())
                .collect();
            let before = Utc::now();
            session.rebuild_streams();
            let after = Utc::now();
            census.streams_compared +=
                assert_decorated_mirror(&session, before, after, seed, "after update");
            assert_eq!(
                session
                    .entries
                    .iter()
                    .map(|entry| (**entry).clone())
                    .collect::<Vec<_>>(),
                flat_expected,
                "seed {seed} / after update: wrote through into the flat transcript"
            );
        }
    }

    // Anti-vacuity. Thresholds are ~half the counts the committed generator
    // actually produces, so an incidental re-weight has room to move while a
    // category that stops being generated at all trips immediately. Each of
    // these is a decoration branch: if its count is 0 the corresponding step is
    // untested no matter what this test is called.
    for (count, floor, what) in [
        (
            census.closed_removed,
            85usize,
            "closed streams removed from the mirror",
        ),
        (
            census.closed_removed_with_entries,
            85,
            "closed streams that still had entries",
        ),
        (
            census.orphans_suppressed,
            50,
            "hydration orphans left collapsed",
        ),
        (
            census.orphans_reopened,
            30,
            "hydration orphans reopened by post-watermark activity",
        ),
        (census.shells_folded, 80, "running shells folded in"),
        (
            census.shells_terminal_skipped,
            60,
            "terminal shells auto-closed by the fold",
        ),
        (
            census.shells_without_output,
            18,
            "running shells with no snapshot",
        ),
        (
            census.agents_folded_fresh,
            30,
            "agents folded as a fresh stream",
        ),
        (
            census.agents_folded_over_suppressed,
            7,
            "agents folded over a suppressed demux stream",
        ),
        (
            census.agents_clobber_guarded,
            35,
            "agents that hit the never-clobber rule",
        ),
        (
            census.agents_aged_out,
            30,
            "agents skipped by the closed-streams age-out",
        ),
        (
            census.agents_terminal_skipped,
            15,
            "terminal agents skipped by the fold",
        ),
        (
            census.labels_enriched,
            110,
            "teammate labels enriched from the map",
        ),
        (
            census.labels_fallback,
            115,
            "teammate labels falling back to the toolu",
        ),
        (
            census.seeds_visibly_decorated,
            150,
            "seeds where the decoration actually changed the mirror",
        ),
        (
            census.streams_compared,
            1_500,
            "streams the reference comparison actually walked",
        ),
    ] {
        assert!(
            count >= floor,
            "the generator produced only {count} of `{what}` over 300 seeds \
             (floor {floor}) — that decoration step is barely exercised and this \
             property test is close to vacuous for it. Full census: {census:?}"
        );
    }
}

/// Highest `mod_seq` the generator has handed out for this session, so an
/// in-place update can advance past it the way the store's `EntryUpdated` arm
/// does.
#[cfg(test)]
fn count_bound(session: &SolutionSession) -> u64 {
    session
        .entries
        .iter()
        .map(|entry| entry.mod_seq)
        .max()
        .unwrap_or(0)
}
