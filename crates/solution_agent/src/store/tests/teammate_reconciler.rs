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

/// Helper to count `SessionSubagentsChanged` events for a given session.
/// Returns the counter handle plus the subscription (which must be held in
/// scope for the lifetime of the test).
fn subscribe_subagents_changed(
    session_id: SolutionSessionId,
    cx: &mut TestAppContext,
) -> (Rc<std::cell::RefCell<usize>>, gpui::Subscription) {
    let counter = Rc::new(std::cell::RefCell::new(0usize));
    let subscription = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionSubagentsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });
    (counter, subscription)
}

#[gpui::test]
async fn subagent_inprogress_task_registers_tab(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_task_1",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Loop agent 1"),
                    Some("general-purpose"),
                ),
                cx,
            )
            .expect("upsert task");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.teammate_labels.len(), 1, "one teammate label captured");
        let key = SharedString::from("toolu_task_1");
        let label = s.teammate_labels.get(&key).expect("label present");
        assert_eq!(label.as_ref(), "Loop agent 1");
    });
    assert_eq!(
        *changed_count.borrow(),
        1,
        "SessionSubagentsChanged emitted exactly once on first registration"
    );
}

#[gpui::test]
async fn subagent_terminal_status_removes_tab(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_task_2",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Worker A"),
                    None,
                ),
                cx,
            )
            .expect("upsert in progress");
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(*changed_count.borrow(), 1, "one add emit");

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_task_2",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::Completed,
                    Some("Worker A"),
                    None,
                ),
                cx,
            )
            .expect("upsert completed");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert!(
            s.teammate_labels.is_empty(),
            "label reclaimed on Completed (via close_stream)"
        );
    });
    assert_eq!(
        *changed_count.borrow(),
        2,
        "exactly two emits: add + remove"
    );
}

#[gpui::test]
async fn tool_completion_entry_updated_refreshes_silence_clock(cx: &mut TestAppContext) {
    // A long silent FOREGROUND command bumps `last_activity_at` once (the
    // tool-call `NewEntry` at start), then blocks claude for minutes while
    // streaming nothing — during which the stuck-session watchdog is held off
    // only by the in-progress-tool shield (TOOL_STUCK_SECS). When the command
    // finishes, its terminal-status transition arrives as an `EntryUpdated`.
    // That event MUST refresh `last_activity_at`; otherwise the instant the tool
    // leaves `InProgress` the watchdog sees `silent_secs >= STUCK_TURN_SECS` with
    // no live tool and falsely reconnects a healthy agent (observed 2026-07-01:
    // an `until grep …` poll loop hit claude's own 5-min Bash timeout and the
    // editor immediately "переподключил сессию").
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    // Tool starts (NewEntry) — bumps last_activity_at to "now".
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_slow_cmd",
                    "Bash",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("until grep -qE '^(ok|FAIL)' out"),
                    None,
                ),
                cx,
            )
            .expect("upsert in progress");
        });
    });
    cx.executor().run_until_parked();

    // Simulate the long silent run: freeze the clock 6 minutes in the past,
    // past STUCK_TURN_SECS (5 min).
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        session.update(cx, |s, _| {
            s.last_activity_at = chrono::Utc::now() - chrono::Duration::seconds(360);
        });
    });

    // Command finishes → terminal-status EntryUpdated on the same tool id.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_slow_cmd",
                    "Bash",
                    agent_client_protocol::schema::ToolCallStatus::Completed,
                    Some("until grep -qE '^(ok|FAIL)' out"),
                    None,
                ),
                cx,
            )
            .expect("upsert completed");
        });
    });
    cx.executor().run_until_parked();

    // The completion must have reset the silence clock, so a subsequent watchdog
    // tick sees a live agent, not a wedged one.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session");
        let silent_secs = chrono::Utc::now()
            .signed_duration_since(session.read(cx).last_activity_at)
            .num_seconds();
        assert!(
            silent_secs < 60,
            "a tool-completion EntryUpdated must refresh last_activity_at \
             (was {silent_secs}s stale — watchdog would falsely reconnect)"
        );
    });
}

#[gpui::test]
async fn subagent_label_falls_back_to_subagent_type(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_long_abcd",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    None,
                    Some("general-purpose"),
                ),
                cx,
            )
            .expect("upsert");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let key = SharedString::from("toolu_long_abcd");
        let label = s.teammate_labels.get(&key).expect("label present");
        assert_eq!(
            label.as_ref(),
            "general-purpose#abcd",
            "fallback label is `subagent_type#<short-id>`"
        );
    });
}

#[gpui::test]
async fn subagent_label_defaults_to_agent_short_id(cx: &mut TestAppContext) {
    // No description, no subagent_type → final fallback: `Agent <short-id>`.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_xy12",
                    "Agent",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    None,
                    None,
                ),
                cx,
            )
            .expect("upsert");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let key = SharedString::from("toolu_xy12");
        let label = s.teammate_labels.get(&key).expect("label present");
        assert_eq!(label.as_ref(), "Agent xy12");
    });
}

#[gpui::test]
async fn non_task_tool_call_does_not_register_tab(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_bash_1",
                    "Bash",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("ignored"),
                    None,
                ),
                cx,
            )
            .expect("upsert bash");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert!(
            s.teammate_labels.is_empty(),
            "Bash is not a subagent — no label captured"
        );
    });
    assert_eq!(
        *changed_count.borrow(),
        0,
        "no SessionSubagentsChanged emission for non-subagent tools"
    );
}

#[gpui::test]
async fn subagent_registration_captures_all_labels(cx: &mut TestAppContext) {
    // Since wire v5 the pill ORDER lives in `streams` (an IndexMap keyed by
    // teammate first-appearance), not on `teammate_labels` (a plain map). This
    // test now just proves every teammate's label is captured with its right
    // value; stream order is covered by the demux / session_view tests.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    for (id, label) in [
        ("toolu_a", "First"),
        ("toolu_b", "Second"),
        ("toolu_c", "Third"),
    ] {
        cx.update(|cx| {
            acp_thread.update(cx, |t, cx| {
                t.upsert_tool_call(
                    make_task_tool_call(
                        id,
                        "Task",
                        agent_client_protocol::schema::ToolCallStatus::InProgress,
                        Some(label),
                        None,
                    ),
                    cx,
                )
                .expect("upsert");
            });
        });
        cx.executor().run_until_parked();
    }

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.teammate_labels.len(), 3);
        for (id, label) in [
            ("toolu_a", "First"),
            ("toolu_b", "Second"),
            ("toolu_c", "Third"),
        ] {
            assert_eq!(
                s.teammate_labels
                    .get(&SharedString::from(id))
                    .map(|l| l.as_ref()),
                Some(label),
                "label for {id} captured"
            );
        }
    });
}

#[gpui::test]
async fn duplicate_inprogress_does_not_re_register(cx: &mut TestAppContext) {
    // A streaming Task's raw_input arrives over several EntryUpdated events
    // before the status flips off InProgress; each one re-enters
    // `apply_subagent_lifecycle`. The first must register the tab, every
    // subsequent observation must be a no-op (no double-insert, no
    // SessionSubagentsChanged spam).
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_dup",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Original"),
                    None,
                ),
                cx,
            )
            .expect("upsert initial");
        });
    });
    cx.executor().run_until_parked();

    // Simulate a second EntryUpdated for the same id, still InProgress
    // (e.g. raw_input streamed in more keys). Even if the label would now
    // be different, the tab is already there.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_dup",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Renamed"),
                    None,
                ),
                cx,
            )
            .expect("upsert again");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.teammate_labels.len(), 1, "single label, not doubled");
        let label = s
            .teammate_labels
            .get(&SharedString::from("toolu_dup"))
            .expect("label");
        // Label is locked-in at first observation — the "Renamed" update is
        // ignored to preserve a stable user-facing pill across the streaming
        // raw_input chunks.
        assert_eq!(label.as_ref(), "Original");
    });
    assert_eq!(
        *changed_count.borrow(),
        1,
        "duplicate InProgress must not re-emit SessionSubagentsChanged"
    );
}

#[gpui::test]
async fn subagent_failed_status_also_removes_tab(cx: &mut TestAppContext) {
    // Terminal-status coverage: Failed is just as final as Completed/Canceled.
    // A Task subagent that crashed mid-run still releases its tab.
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    let (changed_count, _sub) = subscribe_subagents_changed(session_id, cx);

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_fail",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::InProgress,
                    Some("Doomed"),
                    None,
                ),
                cx,
            )
            .expect("upsert");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_task_tool_call(
                    "toolu_fail",
                    "Task",
                    agent_client_protocol::schema::ToolCallStatus::Failed,
                    Some("Doomed"),
                    None,
                ),
                cx,
            )
            .expect("upsert failed");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        assert!(session.read(cx).teammate_labels.is_empty());
    });
    assert_eq!(*changed_count.borrow(), 2, "add + remove on Failed");
}

/// Background-Agents-Strip Task 8: when an `Agent`-named ToolCall enters a
/// terminal status with a parseable `raw_output`, `apply_subagent_lifecycle`
/// registers a `BackgroundAgent` and emits `SessionBackgroundAgentsChanged`.
#[gpui::test]
async fn agent_terminal_with_parseable_raw_output_registers_background_agent(
    cx: &mut TestAppContext,
) {
    use agent_client_protocol::schema as acp;
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let bg_counter = Rc::new(std::cell::RefCell::new(0usize));
    let _bg_sub = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = bg_counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });

    // InProgress → Completed with raw_output carrying the managed-agent
    // announcement. Mirrors how claude_code emits the Agent tool call.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_agent_1".to_string()),
                "Agent".to_string(),
            )
            .kind(acp::ToolKind::Think)
            .status(acp::ToolCallStatus::InProgress)
            .meta(Some(acp_thread::meta_with_tool_name("Agent")));
            t.upsert_tool_call(call, cx).expect("upsert in-progress");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            let raw = serde_json::Value::String(
                "agentId: a30f92a688e431edc\noutput_file: /tmp/agent-a30f92a688e431edc.output"
                    .to_string(),
            );
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_agent_1".to_string()),
                "Agent".to_string(),
            )
            .kind(acp::ToolKind::Think)
            .status(acp::ToolCallStatus::Completed)
            .raw_output(raw)
            .meta(Some(acp_thread::meta_with_tool_name("Agent")));
            t.upsert_tool_call(call, cx).expect("upsert completed");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.background_agents.len(),
            1,
            "one background-agent registered on Agent-tool terminal"
        );
        assert_eq!(s.background_agent_order.len(), 1, "order vec parallel");
        let id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
        assert!(
            s.background_agents.contains_key(&id),
            "registration keyed on parsed agentId"
        );
        assert_eq!(s.background_agent_order[0], id);
    });
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "SessionBackgroundAgentsChanged emitted exactly once on registration"
    );

    // Idempotency: a duplicate terminal-status update with the same
    // raw_output must NOT re-register or re-emit. Drives the
    // `contains_key` guard in apply_subagent_lifecycle.
    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            let raw = serde_json::Value::String(
                "agentId: a30f92a688e431edc\noutput_file: /tmp/agent-a30f92a688e431edc.output"
                    .to_string(),
            );
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_agent_1".to_string()),
                "Agent".to_string(),
            )
            .kind(acp::ToolKind::Think)
            .status(acp::ToolCallStatus::Completed)
            .raw_output(raw)
            .meta(Some(acp_thread::meta_with_tool_name("Agent")));
            t.upsert_tool_call(call, cx).expect("upsert duplicate");
        });
    });
    cx.executor().run_until_parked();
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "duplicate Agent-terminal update is a no-op (idempotent registration)"
    );
}

/// Build a `Bash` ToolCall carrying a `run_in_background` flag in
/// `raw_input` and the launch announcement in `raw_output`. Mirrors how
/// claude_code shapes a backgrounded `Bash` call.
fn make_bash_bg_tool_call(
    id: &str,
    command: &str,
    run_in_background: bool,
    raw_output: Option<&str>,
) -> agent_client_protocol::schema::ToolCall {
    use agent_client_protocol::schema as acp;
    let mut raw_input = serde_json::Map::new();
    raw_input.insert("command".into(), serde_json::Value::String(command.into()));
    raw_input.insert(
        "run_in_background".into(),
        serde_json::Value::Bool(run_in_background),
    );
    let mut call = acp::ToolCall::new(acp::ToolCallId::new(id.to_string()), "Bash".to_string())
        .kind(acp::ToolKind::Execute)
        .status(acp::ToolCallStatus::Completed)
        .meta(Some(acp_thread::meta_with_tool_name("Bash")))
        .raw_input(serde_json::Value::Object(raw_input));
    if let Some(out) = raw_output {
        call = call.raw_output(serde_json::Value::String(out.to_string()));
    }
    call
}

/// Background-Shells-Strip Tasks 7 + 9: a terminal `Bash` tool call with
/// `run_in_background=true` and a parseable launch announcement registers a
/// `BackgroundShell` (before the `is_task_like` gate, since `Bash` is not a
/// task-like tool) and emits `SessionBackgroundShellsChanged`.
#[gpui::test]
async fn bash_run_in_background_terminal_registers_background_shell(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let bg_counter = Rc::new(std::cell::RefCell::new(0usize));
    let _bg_sub = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = bg_counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionBackgroundShellsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });

    const ANNOUNCEMENT: &str = "Command running in background with ID: bvb4ful1z. Output is being written to: /tmp/claude-1000/-home-spk-proj/ses-x/tasks/bvb4ful1z.output. You will be notified when it completes.";

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_bash_bg_tool_call("toolu_bash_1", "sleep 60", true, Some(ANNOUNCEMENT)),
                cx,
            )
            .expect("upsert bash bg");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(
            s.background_shells.len(),
            1,
            "one background-shell registered on Bash(bg) terminal"
        );
        assert_eq!(s.background_shell_order.len(), 1, "order vec parallel");
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        assert!(
            s.background_shells.contains_key(&id),
            "registration keyed on parsed shell id"
        );
        assert_eq!(s.background_shell_order[0], id);
        let shell = s.background_shells.get(&id).expect("shell present");
        assert_eq!(
            shell.output_path,
            PathBuf::from("/tmp/claude-1000/-home-spk-proj/ses-x/tasks/bvb4ful1z.output"),
            "output_path parsed from announcement"
        );
        assert_eq!(shell.command.as_ref(), "sleep 60", "command label captured");
        assert_eq!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Running,
            "fresh shell is Running"
        );
    });
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "SessionBackgroundShellsChanged emitted exactly once on registration"
    );
}

/// Tasks 7 + 9: a `Bash` tool call WITHOUT `run_in_background` (false) must
/// NOT register a background shell — the `run_in_background == Some(true)`
/// guard rejects it.
#[gpui::test]
async fn bash_without_run_in_background_registers_no_shell(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    const ANNOUNCEMENT: &str = "Command running in background with ID: bvb4ful1z. Output is being written to: /tmp/claude-1000/proj/tasks/bvb4ful1z.output. You will be notified.";

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                // run_in_background = false, but with an announcement-shaped
                // raw_output — the guard must still reject it.
                make_bash_bg_tool_call("toolu_bash_2", "echo hi", false, Some(ANNOUNCEMENT)),
                cx,
            )
            .expect("upsert non-bg bash");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert!(
            s.background_shells.is_empty(),
            "no shell registered for a non-background Bash call"
        );
        assert!(s.background_shell_order.is_empty());
    });
}

/// Tasks 7 + 9: after registration, `refresh_background_shell_snapshot`
/// live-tails the on-disk `.output` file into `BackgroundShell::latest`.
/// Drives the full launch→register→tail pipeline against a real temp file.
#[gpui::test]
async fn refresh_background_shell_snapshot_tails_output_file(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let output_path = dir.path().join("tasks").join("bvb4ful1z.output");
    std::fs::create_dir_all(output_path.parent().expect("parent")).expect("mkdir tasks");
    std::fs::write(&output_path, b"line one\nline two\n").expect("write output");

    let announcement = format!(
        "Command running in background with ID: bvb4ful1z. Output is being written to: {}. You will be notified.",
        output_path.display()
    );

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.upsert_tool_call(
                make_bash_bg_tool_call("toolu_bash_3", "tail -f log", true, Some(&announcement)),
                cx,
            )
            .expect("upsert bash bg");
        });
    });
    cx.executor().run_until_parked();

    // The inline refresh in the registration branch already tailed the file
    // (the announcement path is a real temp file). Assert the snapshot.
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        let shell = s.background_shells.get(&id).expect("shell present");
        let latest = shell
            .latest
            .as_ref()
            .expect("inline refresh seeded a snapshot from the real file");
        assert!(
            latest.output_tail.contains("line one") && latest.output_tail.contains("line two"),
            "tail captured the file bytes, got: {:?}",
            latest.output_tail
        );
        assert_eq!(
            shell.last_offset, 18,
            "offset advanced past the 18 written bytes"
        );
    });
}

/// Test helper: register a `Running` background shell directly on a session,
/// bypassing the `Bash(bg)` launch-announcement parse path. Used by the Task 8
/// terminal-signal tests so they can assert the state transition in isolation.
fn register_background_shell(
    cx: &mut TestAppContext,
    session_id: SolutionSessionId,
    shell_id: &str,
) {
    let id = crate::background_shell::BackgroundShellId::new(shell_id.to_string());
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        session.update(cx, |s, _| {
            s.background_shells.insert(
                id.clone(),
                crate::background_shell::BackgroundShell {
                    id: id.clone(),
                    command: SharedString::from("sleep 60"),
                    output_path: PathBuf::from("/tmp/claude-1000/tasks")
                        .join(format!("{shell_id}.output")),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    state: crate::background_shell::ShellRuntimeState::Running,
                },
            );
            s.background_shell_order.push(id);
        });
    });
}

/// A background command COMPLETING must reset the session's silence clock.
/// While it runs, `has_live_background_work` keeps the supervisor quiet but
/// `last_activity_at` stays frozen at launch — so on completion the accrued
/// silence is already past the threshold and the judge would fire instantly,
/// racing the agent's own resume-to-read-the-result. Bumping the clock gives
/// the agent a fresh idle window to self-resume first.
#[gpui::test]
async fn background_shell_completion_resets_silence_clock(cx: &mut TestAppContext) {
    let (session_id, _acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "bg-done");

    // Freeze the clock 10 min back, as a long silent command would leave it.
    let stale = chrono::Utc::now() - chrono::Duration::seconds(600);
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store
                .session(session_id)
                .unwrap()
                .update(cx, |s, _| s.last_activity_at = stale);
            store.mark_background_shell_state(
                session_id,
                crate::background_shell::BackgroundShellId::new("bg-done".to_string()),
                crate::background_shell::ShellRuntimeState::Exited(Some(0)),
                cx,
            );
        });
    });

    let after = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .unwrap()
            .read(cx)
            .last_activity_at
    });
    assert!(
        after > stale,
        "a completed background command must reset the silence clock"
    );
    assert!(
        chrono::Utc::now().signed_duration_since(after).num_seconds() < 60,
        "last_activity must be bumped to ~now on completion, not left stale"
    );
}

/// Task 8: a terminal `KillShell` tool_call whose `raw_input` carries the
/// `shell_id` of a tracked background shell flips that shell to
/// `ShellRuntimeState::Killed` and emits `SessionBackgroundShellsChanged`.
#[gpui::test]
async fn kill_shell_terminal_marks_shell_killed(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "bvb4ful1z");

    let bg_counter = Rc::new(std::cell::RefCell::new(0usize));
    let _bg_sub = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let counter = bg_counter.clone();
        cx.subscribe(&store, move |_store, event, _cx| {
            if let SolutionAgentStoreEvent::SessionBackgroundShellsChanged(id) = event
                && *id == session_id
            {
                *counter.borrow_mut() += 1;
            }
        })
    });

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            use agent_client_protocol::schema as acp;
            let mut raw_input = serde_json::Map::new();
            raw_input.insert(
                "shell_id".into(),
                serde_json::Value::String("bvb4ful1z".into()),
            );
            let call = acp::ToolCall::new(
                acp::ToolCallId::new("toolu_kill_1".to_string()),
                "KillShell".to_string(),
            )
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Completed)
            .meta(Some(acp_thread::meta_with_tool_name("KillShell")))
            .raw_input(serde_json::Value::Object(raw_input));
            t.upsert_tool_call(call, cx).expect("upsert KillShell");
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        let shell = s.background_shells.get(&id).expect("shell still tracked");
        assert_eq!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Killed,
            "KillShell tool_call flips the shell to Killed"
        );
    });
    assert_eq!(
        *bg_counter.borrow(),
        1,
        "SessionBackgroundShellsChanged emitted exactly once on the kill"
    );
}

/// Task 8: a `<task-notification>` user-role message whose `<task-id>` matches
/// a tracked background shell flips it to `Exited(Some(code))`. Drives the
/// real NewEntry wiring: `push_user_content_block` appends a `UserMessage`
/// entry → `AcpThreadEvent::NewEntry` → `observe_task_notification`.
#[gpui::test]
async fn task_notification_user_message_marks_shell_exited(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "bvb4ful1z");

    const NOTIFICATION: &str = r#"<task-notification>
<task-id>bvb4ful1z</task-id>
<status>completed</status>
<summary>Background command "sleep 60" completed (exit code 0)</summary>
</task-notification>"#;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(NOTIFICATION.to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        let id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
        let shell = s.background_shells.get(&id).expect("shell still tracked");
        assert_eq!(
            shell.state,
            crate::background_shell::ShellRuntimeState::Exited(Some(0)),
            "task-notification user message flips the shell to Exited(0)"
        );
    });
}

/// Task 8: a `<task-notification>` for an UNTRACKED shell id is a no-op — no
/// stray shell is registered and no tracked shell's state changes.
#[gpui::test]
async fn task_notification_unknown_shell_is_noop(cx: &mut TestAppContext) {
    let (session_id, acp_thread, _tmp) = create_session_with_thread(cx).await;
    register_background_shell(cx, session_id, "tracked123");

    const NOTIFICATION: &str = r#"<task-notification>
<task-id>unknown999</task-id>
<status>completed</status>
<summary>Background command "x" completed (exit code 0)</summary>
</task-notification>"#;

    cx.update(|cx| {
        acp_thread.update(cx, |t, cx| {
            t.push_user_content_block(
                Some(acp_thread::UserMessageId::new()),
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(NOTIFICATION.to_string()),
                ),
                cx,
            );
        });
    });
    cx.executor().run_until_parked();

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).expect("session exists");
        let s = session.read(cx);
        assert_eq!(s.background_shells.len(), 1, "no stray shell registered");
        let tracked = crate::background_shell::BackgroundShellId::new("tracked123");
        assert_eq!(
            s.background_shells
                .get(&tracked)
                .expect("tracked present")
                .state,
            crate::background_shell::ShellRuntimeState::Running,
            "an unrelated notification leaves the tracked shell Running"
        );
    });
}

/// Task 9: a Managed Agent whose `latest.stop_reason` is `Some(...)` is
/// removed from the session on the next `tick_background_agents` pass,
/// and a `SessionBackgroundAgentsChanged` event is emitted.
#[gpui::test]
async fn done_agent_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Done."),
                        stop_reason: Some(SharedString::from("end_turn")),
                    }),
                    last_offset: 0,
                    parent_tool_use_id: None,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_agents(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "done agent must be removed on tick"
        );
        assert!(
            session.read(cx).background_agent_order.is_empty(),
            "order vec must be pruned in lockstep"
        );
    });
}

/// Analogous to `background_shell_completion_resets_silence_clock`, for the
/// MANAGED-agent path: a background agent reaching a terminal `stop_reason`
/// must reset the session's silence clock, so the supervisor gives the parent a
/// full idle window to resume on its own before judging (rather than firing the
/// instant `has_live_background_work` flips false with silence already accrued).
#[gpui::test]
async fn background_agent_terminal_transition_resets_silence_clock(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");

    // A JSONL whose latest line is a terminal assistant stop.
    let dir = tempfile::tempdir().unwrap();
    let jsonl = dir.path().join("agent.jsonl");
    std::fs::write(
        &jsonl,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"bye"}],"stop_reason":"end_turn"}}"#,
    )
    .unwrap();

    let stale = chrono::Utc::now() - chrono::Duration::seconds(600);
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.last_activity_at = stale;
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: jsonl.clone().into(),
                    registered_at: chrono::Utc::now(),
                    // Currently NON-terminal — the refresh below observes the
                    // transition INTO terminal.
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Generating…"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                    parent_tool_use_id: None,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.refresh_background_agent_snapshot(session_id, bg_id.clone(), cx)
        });
    });

    let after = cx.update(|cx| {
        SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .unwrap()
            .read(cx)
            .last_activity_at
    });
    assert!(after > stale, "terminal transition must reset the silence clock");
    assert!(
        chrono::Utc::now().signed_duration_since(after).num_seconds() < 60,
        "last_activity must be bumped to ~now, not left stale"
    );
}

/// Sub-task C (deferred #1): an async `Agent` teammate's demux `Teammate`
/// stream — which phase 3 deliberately kept live past the spawn tool-call's
/// terminal (that's only spawn-ack) — is auto-closed when the managed
/// background agent reaches its REAL terminal `stop_reason`, via the
/// `BackgroundAgent.parent_tool_use_id` → `StreamId::Teammate` mapping.
#[gpui::test]
async fn background_agent_terminal_closes_teammate_stream(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    let parent_toolu = SharedString::from("toolu_X");
    let teammate = crate::stream::StreamId::Teammate(parent_toolu.clone());

    let dir = tempfile::tempdir().unwrap();
    let jsonl = dir.path().join("agent.jsonl");
    std::fs::write(
        &jsonl,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"bye"}],"stop_reason":"end_turn"}}"#,
    )
    .unwrap();

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            // A parent-thread entry tagged with the teammate's parent tool_use
            // id → the demux produces a live `Teammate` stream.
            s.set_entries(
                vec![crate::session_entry::SessionEntry {
                    created_ms: 0,
                    mod_seq: 0,
                    subagent_id: Some(parent_toolu.clone()),
                    kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                        chunks: vec![crate::session_entry::AssistantChunk::Message(
                            "streaming".to_string(),
                        )],
                    },
                }],
                cx,
            );
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: jsonl.clone().into(),
                    registered_at: chrono::Utc::now(),
                    // Currently NON-terminal so the refresh observes the edge.
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Generating…"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                    parent_tool_use_id: Some(parent_toolu.clone()),
                },
            );
            s.background_agent_order.push(bg_id.clone());
            assert!(
                s.streams.contains_key(&teammate),
                "teammate stream is live before the agent finishes"
            );
        });
    });

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.refresh_background_agent_snapshot(session_id, bg_id.clone(), cx)
        });
    });

    cx.update(|cx| {
        let session = SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                !s.streams.contains_key(&teammate),
                "teammate stream must be closed on the agent's real terminal"
            );
            assert!(
                s.closed_streams.contains_key(&teammate),
                "close reason recorded in the overlay"
            );
        });
    });
}

/// Sub-task C negative case: a NON-terminal snapshot refresh must leave the
/// teammate's demux stream live (only the terminal `stop_reason` closes it).
#[gpui::test]
async fn background_agent_non_terminal_leaves_teammate_stream(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("b41a03b799f542fed");
    let parent_toolu = SharedString::from("toolu_Y");
    let teammate = crate::stream::StreamId::Teammate(parent_toolu.clone());

    let dir = tempfile::tempdir().unwrap();
    let jsonl = dir.path().join("agent.jsonl");
    // Latest line is a still-running assistant turn (no stop_reason).
    std::fs::write(
        &jsonl,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}"#,
    )
    .unwrap();

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.set_entries(
                vec![crate::session_entry::SessionEntry {
                    created_ms: 0,
                    mod_seq: 0,
                    subagent_id: Some(parent_toolu.clone()),
                    kind: crate::session_entry::SessionEntryKind::AssistantMessage {
                        chunks: vec![crate::session_entry::AssistantChunk::Message(
                            "streaming".to_string(),
                        )],
                    },
                }],
                cx,
            );
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: jsonl.clone().into(),
                    registered_at: chrono::Utc::now(),
                    latest: None,
                    last_offset: 0,
                    parent_tool_use_id: Some(parent_toolu.clone()),
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });

    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.refresh_background_agent_snapshot(session_id, bg_id.clone(), cx)
        });
    });

    cx.update(|cx| {
        let session = SolutionAgentStore::global(cx)
            .read(cx)
            .session(session_id)
            .unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                s.streams.contains_key(&teammate),
                "a non-terminal refresh must keep the teammate stream live"
            );
        });
    });
}

// --- Mid-session SELECTIVE teammate-pill reconcile
// (`reconcile_finished_teammate_streams`) ---------------------------------
//
// These verify the busy-session leak fix: a finished-teammate pill must close
// the moment its completion is provable, WITHOUT any →Idle transition (a long
// busy session never fires the →Idle GC).

/// Build a parent spawn tool-call entry (on the Main stream — `subagent_id`
/// None) with the given owned-model `ToolStatus`.
fn reconcile_toolcall_entry(
    id: &str,
    tool_name: &str,
    status: crate::session_entry::ToolStatus,
) -> crate::session_entry::SessionEntry {
    use agent_client_protocol::schema as acp;
    crate::session_entry::SessionEntry {
        created_ms: 1_700_000_000_000,
        mod_seq: 1,
        subagent_id: None,
        kind: crate::session_entry::SessionEntryKind::ToolCall {
            id: id.to_string(),
            label_md: "spawn".into(),
            kind: acp::ToolKind::Think,
            status,
            content_md: Vec::new(),
            raw_input: None,
            raw_output: None,
            tool_name: Some(tool_name.to_string()),
            locations: Vec::new(),
            status_started_at: None,
        },
    }
}

/// Build a teammate body entry tagged with `toolu` — this is what the demux
/// turns into a `Teammate(toolu)` stream.
fn reconcile_tagged_body(toolu: &str) -> crate::session_entry::SessionEntry {
    crate::session_entry::SessionEntry {
        created_ms: 1_700_000_000_001,
        mod_seq: 2,
        subagent_id: Some(SharedString::from(toolu.to_string())),
        kind: crate::session_entry::SessionEntryKind::AssistantMessage {
            chunks: vec![crate::session_entry::AssistantChunk::Message(
                "teammate work".into(),
            )],
        },
    }
}

/// Rule 2: a NON-Idle (Running) session with an inline `Task` teammate whose
/// spawn tool-call is `Completed` has its `Teammate` stream closed by the
/// mid-session reconcile — no →Idle transition involved.
#[gpui::test]
async fn reconcile_closes_finished_inline_task_teammate_mid_session(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let toolu = "toolu_task_done";
    let teammate = crate::stream::StreamId::Teammate(SharedString::from(toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s.teammate_labels
                .insert(SharedString::from(toolu), SharedString::from("Worker"));
            s.set_entries(
                vec![
                    reconcile_toolcall_entry(
                        toolu,
                        "Task",
                        crate::session_entry::ToolStatus::Completed,
                    ),
                    reconcile_tagged_body(toolu),
                ],
                cx,
            );
            assert!(
                s.streams.contains_key(&teammate),
                "teammate stream is live before the reconcile"
            );
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                !s.streams.contains_key(&teammate),
                "a terminal inline Task teammate must be reconciled closed mid-session"
            );
            assert!(
                !matches!(s.state, SessionState::Idle),
                "guard: the session never went Idle"
            );
        });
    });
}

/// Do-NOT-close: an inline `Task` whose spawn tool-call is still `InProgress`
/// is genuinely live mid-session — the reconcile must leave it.
#[gpui::test]
async fn reconcile_keeps_live_inline_task_teammate(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let toolu = "toolu_task_live";
    let teammate = crate::stream::StreamId::Teammate(SharedString::from(toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s.set_entries(
                vec![
                    reconcile_toolcall_entry(
                        toolu,
                        "Task",
                        crate::session_entry::ToolStatus::InProgress,
                    ),
                    reconcile_tagged_body(toolu),
                ],
                cx,
            );
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                s.streams.contains_key(&teammate),
                "a live (InProgress) inline Task teammate must survive the reconcile"
            );
        });
    });
}

/// Rule 3 both ways: a fresh async `Agent` (recent snapshot, no stop_reason)
/// survives while a stale-mtime one — AND one with a terminal stop_reason — are
/// reconciled closed. A terminal async Agent's spawn tool-call being terminal is
/// spawn-ack and is NOT what closes it; the background_agent snapshot is.
#[gpui::test]
async fn reconcile_keeps_live_async_agent_and_closes_stale_one(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let live_toolu = "toolu_async_live";
    let stale_toolu = "toolu_async_stale";
    let stop_toolu = "toolu_async_stop";
    let live = crate::stream::StreamId::Teammate(SharedString::from(live_toolu));
    let stale = crate::stream::StreamId::Teammate(SharedString::from(stale_toolu));
    let stopped = crate::stream::StreamId::Teammate(SharedString::from(stop_toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            // Three async teammates, seeded via tagged body entries + their
            // background_agent registrations (async classification is by the
            // map, not by any tool-call entry — none is present here). The
            // spawn tool-calls being terminal must NOT be what drives the close.
            s.set_entries(
                vec![
                    reconcile_tagged_body(live_toolu),
                    reconcile_tagged_body(stale_toolu),
                    reconcile_tagged_body(stop_toolu),
                ],
                cx,
            );
            let register = |s: &mut crate::model::SolutionSession,
                            bg: &str,
                            parent: &str,
                            snap: crate::background_agent::BackgroundAgentSnapshot| {
                let bg_id = crate::background_agent::BackgroundAgentId::new(bg.to_string());
                s.background_agents.insert(
                    bg_id.clone(),
                    crate::background_agent::BackgroundAgent {
                        id: bg_id.clone(),
                        jsonl_path: "/nonexistent".into(),
                        registered_at: chrono::Utc::now(),
                        latest: Some(snap),
                        last_offset: 0,
                        parent_tool_use_id: Some(SharedString::from(parent.to_string())),
                    },
                );
                s.background_agent_order.push(bg_id);
            };
            register(
                s,
                "bg_live",
                live_toolu,
                crate::background_agent::BackgroundAgentSnapshot {
                    mtime: std::time::SystemTime::now(),
                    activity_label: SharedString::from("Generating…"),
                    stop_reason: None,
                },
            );
            register(
                s,
                "bg_stale",
                stale_toolu,
                crate::background_agent::BackgroundAgentSnapshot {
                    // Older than MANAGED_AGENT_STALE_TIMEOUT_SECS (120s).
                    mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(200),
                    activity_label: SharedString::from("Generating…"),
                    stop_reason: None,
                },
            );
            register(
                s,
                "bg_stop",
                stop_toolu,
                crate::background_agent::BackgroundAgentSnapshot {
                    mtime: std::time::SystemTime::now(),
                    activity_label: SharedString::from("done"),
                    stop_reason: Some(SharedString::from("end_turn")),
                },
            );
            assert!(s.streams.contains_key(&live));
            assert!(s.streams.contains_key(&stale));
            assert!(s.streams.contains_key(&stopped));
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                s.streams.contains_key(&live),
                "a fresh async agent (recent snapshot, no stop_reason) must survive"
            );
            assert!(
                !s.streams.contains_key(&stale),
                "an async agent stale beyond MANAGED_AGENT_STALE_TIMEOUT_SECS must be closed"
            );
            assert!(
                !s.streams.contains_key(&stopped),
                "an async agent with a terminal stop_reason must be closed"
            );
        });
    });
}

/// Snapshot-less async agent leak: an async `Agent` teammate whose JSONL never
/// produced a parseable snapshot (`latest == None`) has no mtime to age from.
/// Without the `registered_at` fallback its `Teammate` pill lingers forever
/// (the →Idle GC excludes async parents). A stale registration (>120s) must be
/// reconciled closed; a fresh one (<120s, still might snapshot) must survive.
#[gpui::test]
async fn reconcile_closes_snapshotless_stale_async_agent_but_keeps_fresh(
    cx: &mut TestAppContext,
) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let stale_toolu = "toolu_async_nosnap_stale";
    let fresh_toolu = "toolu_async_nosnap_fresh";
    let stale = crate::stream::StreamId::Teammate(SharedString::from(stale_toolu));
    let fresh = crate::stream::StreamId::Teammate(SharedString::from(fresh_toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            s.set_entries(
                vec![
                    reconcile_tagged_body(stale_toolu),
                    reconcile_tagged_body(fresh_toolu),
                ],
                cx,
            );
            let register = |s: &mut crate::model::SolutionSession,
                            bg: &str,
                            parent: &str,
                            registered_at: chrono::DateTime<chrono::Utc>| {
                let bg_id = crate::background_agent::BackgroundAgentId::new(bg.to_string());
                s.background_agents.insert(
                    bg_id.clone(),
                    crate::background_agent::BackgroundAgent {
                        id: bg_id.clone(),
                        jsonl_path: "/nonexistent".into(),
                        registered_at,
                        // No parseable JSONL snapshot yet.
                        latest: None,
                        last_offset: 0,
                        parent_tool_use_id: Some(SharedString::from(parent.to_string())),
                    },
                );
                s.background_agent_order.push(bg_id);
            };
            // Registered > MANAGED_AGENT_STALE_TIMEOUT_SECS (120s) ago.
            register(
                s,
                "bg_nosnap_stale",
                stale_toolu,
                chrono::Utc::now() - chrono::Duration::seconds(200),
            );
            // Registered just now — still might snapshot.
            register(s, "bg_nosnap_fresh", fresh_toolu, chrono::Utc::now());
            assert!(s.streams.contains_key(&stale));
            assert!(s.streams.contains_key(&fresh));
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                !s.streams.contains_key(&stale),
                "a snapshot-less async agent stale beyond MANAGED_AGENT_STALE_TIMEOUT_SECS \
                 must age out via registered_at"
            );
            assert!(
                s.streams.contains_key(&fresh),
                "a snapshot-less async agent freshly registered must survive — the fallback \
                 must not reap live-but-not-yet-snapshotted agents"
            );
        });
    });
}

/// Rule 1: a teammate stream whose parent tool-call entry has vanished from the
/// thread (and which is not a registered async agent) is orphaned → closed.
#[gpui::test]
async fn reconcile_closes_teammate_whose_toolcall_vanished(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let toolu = "toolu_orphan";
    let teammate = crate::stream::StreamId::Teammate(SharedString::from(toolu));

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.update(cx, |s, cx| {
            s.state = SessionState::Running {
                started_at: std::time::Instant::now(),
                notified: false,
            };
            // Only the tagged body entry — NO matching ToolCall entry and no
            // background_agent registration → the spawn tool-call is "gone".
            s.set_entries(vec![reconcile_tagged_body(toolu)], cx);
            assert!(
                s.streams.contains_key(&teammate),
                "orphan teammate stream exists before the reconcile"
            );
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.reconcile_finished_teammate_streams(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        session.read_with(cx, |s, _| {
            assert!(
                !s.streams.contains_key(&teammate),
                "a teammate whose spawn tool-call vanished must be reconciled closed"
            );
        });
    });
}

/// Task 9: an agent with a stale snapshot beyond
/// `MANAGED_AGENT_STALE_TIMEOUT + MANAGED_AGENT_DEAD_LINGER`
/// (V1 hardcoded: 120s + 300s = 420s) is removed on tick.
#[gpui::test]
async fn stale_agent_lingers_briefly_then_removed(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(500),
                        activity_label: SharedString::from("Bash: x"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                    parent_tool_use_id: None,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_agents(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "stale-beyond-linger agent must be removed on tick"
        );
    });
}

/// Task 9: a fresh (non-terminal, recent mtime) Managed Agent must NOT
/// be removed by `tick_background_agents` — only done + long-dead are
/// candidates. Guards against the tick over-pruning live work.
#[gpui::test]
async fn fresh_agent_survives_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let bg_id = crate::background_agent::BackgroundAgentId::new("a30f92a688e431edc");
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_agents.insert(
                bg_id.clone(),
                crate::background_agent::BackgroundAgent {
                    id: bg_id.clone(),
                    jsonl_path: "/nonexistent".into(),
                    registered_at: chrono::Utc::now(),
                    latest: Some(crate::background_agent::BackgroundAgentSnapshot {
                        mtime: std::time::SystemTime::now(),
                        activity_label: SharedString::from("Bash: x"),
                        stop_reason: None,
                    }),
                    last_offset: 0,
                    parent_tool_use_id: None,
                },
            );
            s.background_agent_order.push(bg_id.clone());
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_agents(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.contains_key(&bg_id),
            "fresh agent must survive a tick"
        );
    });
}

/// Helper: insert one background shell into a session's tracking maps.
fn insert_test_background_shell(
    cx: &mut TestAppContext,
    session_id: crate::model::SolutionSessionId,
    shell_id: &crate::background_shell::BackgroundShellId,
    state: crate::background_shell::ShellRuntimeState,
    latest: Option<crate::background_shell::BackgroundShellSnapshot>,
    registered_at: chrono::DateTime<chrono::Utc>,
) {
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        session.update(cx, |s, _| {
            s.background_shells.insert(
                shell_id.clone(),
                crate::background_shell::BackgroundShell {
                    id: shell_id.clone(),
                    command: SharedString::from("sleep 1"),
                    output_path: "/nonexistent".into(),
                    registered_at,
                    latest,
                    last_offset: 0,
                    state,
                },
            );
            s.background_shell_order.push(shell_id.clone());
        });
    });
}

/// Task 10: an `Exited(Some(0))` background shell is reaped on the next
/// `tick_background_shells` pass (terminal-state arm).
#[gpui::test]
async fn exited_shell_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Exited(Some(0)),
        Some(crate::background_shell::BackgroundShellSnapshot {
            // Fresh mtime: proves it's the terminal state, not staleness,
            // driving the removal.
            mtime: std::time::SystemTime::now(),
            output_tail: SharedString::from("done"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "Exited shell must be removed on tick"
        );
        assert!(
            session.read(cx).background_shell_order.is_empty(),
            "order vec must be pruned in lockstep"
        );
    });
}

/// Task 10: a `Killed` background shell is reaped on tick (terminal-state arm).
#[gpui::test]
async fn killed_shell_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Killed,
        Some(crate::background_shell::BackgroundShellSnapshot {
            mtime: std::time::SystemTime::now(),
            output_tail: SharedString::from("killed"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "Killed shell must be removed on tick"
        );
    });
}

/// Task 10 / #9 — the runaway backstop: a still-`Running` shell that never
/// completes and never prints is reaped even with a LIVE parent once its
/// `latest.mtime` crosses [`BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS`] (60min).
/// Here the mtime is 10,000s (≈2.8h) old — well past the cap — so it ages out.
#[gpui::test]
async fn stale_running_shell_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        Some(crate::background_shell::BackgroundShellSnapshot {
            mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(10_000),
            output_tail: SharedString::from("...stalled output"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "stale-Running shell past the live-parent hard cap must be reaped \
             on tick — else a runaway leaks as a Running pill forever"
        );
    });
}

/// Task 10 / #9: a Running shell with NO snapshot but a `registered_at` past the
/// live-parent hard cap (zero output, long since launched) still ages out via
/// the registered_at fallback.
#[gpui::test]
async fn stale_running_shell_no_snapshot_removed_on_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        None,
        chrono::Utc::now() - chrono::Duration::seconds(10_000),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "stale Running shell with no snapshot must age out via registered_at"
        );
    });
}

/// Task 10: a fresh `Running` shell (recent mtime, just registered) must NOT
/// be removed by `tick_background_shells`. Guards against over-pruning live
/// shells.
#[gpui::test]
async fn fresh_running_shell_survives_tick(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        Some(crate::background_shell::BackgroundShellSnapshot {
            mtime: std::time::SystemTime::now(),
            output_tail: SharedString::from("running..."),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.contains_key(&shell_id),
            "fresh Running shell must survive a tick"
        );
    });
}

/// #9: a still-`Running` shell whose parent agent is ALIVE (live `acp_thread`),
/// silent past the ordinary ~7min stale+linger but still under the live-parent
/// hard cap, must NOT be reaped — output-silence is not death (a long silent
/// build/`sleep`), and keeping it preserves the `has_live_background_work`
/// supervisor-suppression. Previously it was dropped at ~7min, losing the
/// suppression while the command was still running.
#[gpui::test]
async fn stale_running_shell_with_live_parent_survives_below_cap(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        Some(crate::background_shell::BackgroundShellSnapshot {
            // 600s: past STALE+DEAD_LINGER (420s) but well under the 60min cap.
            mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(600),
            output_tail: SharedString::from("...quiet build"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.contains_key(&shell_id),
            "a silent-but-Running shell with a live parent must survive past the \
             ~7min stale mark (its completion still arrives via the JSONL scan), \
             so the supervisor stays suppressed"
        );
    });
}

/// #9: the same silent-`Running` shell whose parent subprocess is GONE (a cold
/// session with no `acp_thread` — reconnect / crash / close) IS reaped at the
/// ordinary ~7min stale+linger: no completion notification can ever arrive for
/// an orphan, so the leak guard still applies.
#[gpui::test]
fn stale_running_shell_reaped_when_parent_gone(cx: &mut TestAppContext) {
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));
    let session_id = cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let id = SolutionSessionId::new();
            insert_cold_session(
                id,
                SolutionId("sol-a".into()),
                SharedString::from("claude-acp"),
                None,
                None,
                store,
                cx,
            );
            id
        })
    });
    let shell_id = crate::background_shell::BackgroundShellId::new("bvb4ful1z");
    insert_test_background_shell(
        cx,
        session_id,
        &shell_id,
        crate::background_shell::ShellRuntimeState::Running,
        Some(crate::background_shell::BackgroundShellSnapshot {
            // 600s: past STALE+DEAD_LINGER (420s), under the 60min live-parent cap
            // — but this parent has NO acp_thread, so the ordinary timeout applies.
            mtime: std::time::SystemTime::now() - std::time::Duration::from_secs(600),
            output_tail: SharedString::from("...orphaned"),
        }),
        chrono::Utc::now(),
    );
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| s.tick_background_shells(cx));
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_shells.is_empty(),
            "a stale Running shell whose parent subprocess is gone (no acp_thread) \
             is an orphan and must be reaped at the ordinary stale+linger timeout"
        );
    });
}

/// Pin the error-string set that `resume_session` treats as "session gone,
/// try the next cwd candidate (and ultimately mint a new ACP session)".
///
/// claude-code-acp returns `No conversation found with session ID: …` as the
/// MESSAGE of a JSON-RPC `-32603` (Internal error). Before the fix, our
/// predicate only matched `Resource not found` / `-32002` (the spec's
/// "missing resource" code) — so the message-only error fell through, the
/// `resume_session` for-loop broke after the first attempt, the
/// `solution.root` fallback never fired, and the user saw a raw
/// "No conversation found with session ID: …" snackbar on the next editor
/// restart even though the jsonl was sitting under sanitize(solution.root)
/// the whole time.
///
/// If you change the predicate, update both the match set and this test —
/// dropping a marker silently reintroduces the snackbar.
#[test]
fn is_session_gone_error_matches_known_markers() {
    use crate::store::is_session_gone_error;
    assert!(is_session_gone_error(
        "No conversation found with session ID: 877b9e1b-ae75-448e-bcef-906058b156df"
    ));
    assert!(is_session_gone_error("Resource not found"));
    assert!(is_session_gone_error(
        "RPC error -32002: session id no longer known"
    ));
    // Non-recoverable transport/auth/allow-list errors stay opaque so we
    // don't pointlessly retry against another cwd.
    assert!(!is_session_gone_error("connection refused"));
    assert!(!is_session_gone_error("authentication required"));
    assert!(!is_session_gone_error("permission denied: /home/spk/.spk"));
}

/// Task 13: hydrate-side reconcile drops SQLite rows whose JSONL file
/// no longer exists on disk — without this the strip would render dead
/// pills for runs whose worker dirs were wiped while the editor was
/// closed.
#[gpui::test]
async fn reconciliation_drops_rows_with_missing_jsonl(cx: &mut TestAppContext) {
    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![crate::db::BackgroundAgentRow {
        solution_session_id: session_id.to_string(),
        agent_id: "missing000000000000".into(),
        jsonl_path: "/nonexistent/path/missing.jsonl".into(),
        registered_at_ms: 0,
        last_seen_label: None,
        last_mtime_ms: None,
        stop_reason: None,
    }];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "row with missing JSONL file must not be registered"
        );
    });
}

/// Task 13: rows whose tail JSONL line carries a terminal `stop_reason`
/// are NOT re-registered — the worker already wrapped up while the
/// editor was closed, so re-adding it would resurrect a finished pill.
#[gpui::test]
async fn reconciliation_drops_done_rows(cx: &mut TestAppContext) {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("done.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"bye"}}],"stop_reason":"end_turn"}}}}"#
    )
    .unwrap();
    drop(f);

    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![crate::db::BackgroundAgentRow {
        solution_session_id: session_id.to_string(),
        agent_id: "done00000000000000".into(),
        jsonl_path: path.to_string_lossy().into_owned(),
        registered_at_ms: 0,
        last_seen_label: None,
        last_mtime_ms: None,
        stop_reason: None,
    }];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        assert!(
            session.read(cx).background_agents.is_empty(),
            "row with terminal stop_reason must not be registered"
        );
    });
}

/// Cold-load decision: even an "alive-looking" row (file present, no terminal
/// stop_reason on the tail) is DROPPED, not re-registered — async `Agent`
/// subagents do not survive an editor restart, so the persisted row is stale
/// regardless of what its frozen JSONL tail shows. (Was `registers_alive_row`
/// before the drop-on-cold-load decision.)
#[gpui::test]
async fn reconciliation_drops_alive_row_too(cx: &mut TestAppContext) {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alive.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"hi"}}]}}}}"#
    )
    .unwrap();
    drop(f);

    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![crate::db::BackgroundAgentRow {
        solution_session_id: session_id.to_string(),
        agent_id: "alive0000000000000".into(),
        jsonl_path: path.to_string_lossy().into_owned(),
        registered_at_ms: 0,
        last_seen_label: None,
        last_mtime_ms: None,
        stop_reason: None,
    }];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        let s = session.read(cx);
        assert!(
            s.background_agents.is_empty(),
            "an alive-looking row must still be dropped on cold-load (agents don't survive restart)"
        );
    });
}

/// Cold-load decision: async `Agent` subagents do NOT survive an editor
/// restart, so their persisted `background_agents` rows are stale and must be
/// dropped — not restored — on cold-load. This seeds a row that under the OLD
/// behavior WOULD have been re-registered (file present, no terminal
/// `stop_reason` on the tail — i.e. the `reconciliation_registers_alive_row`
/// case) and asserts that after `reconcile_background_agents_for` NOTHING is
/// registered. Restoring it is what resurrected finished teammate pills after a
/// restart; with no re-registration there is no watcher, so the teammate's
/// stream stays a collapsed hydration orphan (no pill).
///
/// This test FAILS against the old restore behavior (which left
/// `background_agents` non-empty) and passes with the drop-all change.
#[gpui::test]
async fn cold_load_drops_all_background_agents(cx: &mut TestAppContext) {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alive.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    // Non-terminal tail line: no `stop_reason` → previously re-registered.
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"still working"}}]}}}}"#
    )
    .unwrap();
    drop(f);

    let (session_id, _thread, _tmp) = create_session_with_thread(cx).await;
    let rows = vec![
        crate::db::BackgroundAgentRow {
            solution_session_id: session_id.to_string(),
            agent_id: "alive0000000000001".into(),
            jsonl_path: path.to_string_lossy().into_owned(),
            registered_at_ms: 0,
            last_seen_label: None,
            last_mtime_ms: None,
            stop_reason: None,
        },
        crate::db::BackgroundAgentRow {
            solution_session_id: session_id.to_string(),
            agent_id: "alive0000000000002".into(),
            jsonl_path: path.to_string_lossy().into_owned(),
            registered_at_ms: 0,
            last_seen_label: None,
            last_mtime_ms: None,
            stop_reason: None,
        },
    ];
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        s.update(cx, |s, cx| {
            s.reconcile_background_agents_for(session_id, rows, cx)
        });
    });
    cx.update(|cx| {
        let s = SolutionAgentStore::global(cx);
        let session = s.read(cx).session(session_id).unwrap();
        let s = session.read(cx);
        assert!(
            s.background_agents.is_empty(),
            "cold-load must register NO background agents (they don't survive a restart)"
        );
        assert!(
            s.background_agent_order.is_empty(),
            "no agent order entries either"
        );
        assert!(
            !s.streams
                .keys()
                .any(|id| matches!(id, crate::stream::StreamId::Teammate(_))),
            "no teammate stream pill should be present after cold-load reconcile"
        );
    });
}

/// Removes a directory subtree on drop — used to clean up the
/// `~/.claude/projects/<encoded-cwd>/` dir the live-scan test must create at
/// the real (home-derived) path the resolver computes.
struct CleanupDir(PathBuf);

impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// End-to-end: a Running shell flips to `Exited(Some(0))` when a realistic
/// single-line `<task-notification>` JSON `user` message is appended to the
/// parent session JSONL and `scan_parent_jsonl_for_completions` runs. Also
/// asserts the forward-only offset: a second scan with no new bytes is a
/// no-op (the shell stays exactly where the first scan left it).
#[gpui::test]
fn scan_parent_jsonl_flips_running_shell_to_exited(cx: &mut TestAppContext) {
    use crate::background_shell::{BackgroundShell, BackgroundShellId, ShellRuntimeState};

    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    // Unique cwd so the home-derived JSONL path never collides with a real
    // session or a parallel test run.
    let unique = format!(
        "/tmp/spk-scan-test-{}-{}",
        std::process::id(),
        SolutionSessionId::new()
    );
    let cwd = PathBuf::from(&unique);
    let acp_id = "ses-scan-xyz";

    let jsonl = crate::store::parent_session_jsonl_for(&cwd, acp_id).expect("home_dir resolves in test");
    let project_dir = jsonl.parent().expect("jsonl has parent").to_path_buf();
    let _cleanup = CleanupDir(project_dir.clone());
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    // Pre-existing historical content the scan must skip past (offset lazy-init
    // to current EOF on first sight).
    std::fs::write(&jsonl, b"{\"type\":\"system\",\"old\":true}\n").expect("seed jsonl");

    let session_id = SolutionSessionId::new();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            let entity = cx.new(|_| {
                let mut s = SolutionSession::new_idle(
                    session_id,
                    SolutionId("sol-scan".into()),
                    SharedString::from("claude-acp"),
                    agent_client_protocol::schema::SessionId::new(acp_id),
                );
                s.cwd = cwd.clone();
                let id = BackgroundShellId::new("bvb4ful1z");
                s.background_shells.insert(
                    id.clone(),
                    BackgroundShell {
                        id: id.clone(),
                        command: "sleep 60".into(),
                        output_path: PathBuf::from("/tmp/bvb4ful1z.output"),
                        registered_at: Utc::now(),
                        latest: None,
                        last_offset: 0,
                        state: ShellRuntimeState::Running,
                    },
                );
                s.background_shell_order.push(id);
                s
            });
            store.sessions.insert(session_id, entity);
            store
                .by_solution
                .entry(SolutionId("sol-scan".into()))
                .or_default()
                .push(session_id);
            // First scan: lazy-inits the offset to current EOF, flips nothing.
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        let shell = session
            .read(cx)
            .background_shells
            .get(&BackgroundShellId::new("bvb4ful1z"))
            .cloned()
            .unwrap();
        assert_eq!(
            shell.state,
            ShellRuntimeState::Running,
            "historical content must not flip the shell"
        );
    });

    // Append the realistic single-line completion notification.
    {
        use std::io::Write;
        let line = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<task-notification>\\n<task-id>bvb4ful1z</task-id>\\n<status>completed</status>\\n<summary>Background command \\\"sleep 60\\\" completed (exit code 0)</summary>\\n</task-notification>\"}]},\"uuid\":\"abc-123\"}\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .expect("reopen jsonl");
        f.write_all(line.as_bytes()).expect("append notification");
    }

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
    });

    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        let session = store.read(cx).session(session_id).unwrap();
        let shell = session
            .read(cx)
            .background_shells
            .get(&BackgroundShellId::new("bvb4ful1z"))
            .cloned()
            .unwrap();
        assert_eq!(
            shell.state,
            ShellRuntimeState::Exited(Some(0)),
            "completion notification must flip Running -> Exited(0)"
        );
    });

    // Second scan, no new bytes: idempotent no-op (state unchanged).
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
        let session = store.read(cx).session(session_id).unwrap();
        let shell = session
            .read(cx)
            .background_shells
            .get(&BackgroundShellId::new("bvb4ful1z"))
            .cloned()
            .unwrap();
        assert_eq!(shell.state, ShellRuntimeState::Exited(Some(0)));
    });

    // Unknown-id notification flips nothing.
    {
        use std::io::Write;
        let line = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<task-notification>\\n<task-id>unknownid99</task-id>\\n<status>completed</status>\\n<summary>done (exit code 0)</summary>\\n</task-notification>\"}]}}\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .expect("reopen jsonl");
        f.write_all(line.as_bytes()).expect("append unknown");
    }
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.scan_parent_jsonl_for_completions(session_id, cx);
        });
        let session = store.read(cx).session(session_id).unwrap();
        assert_eq!(session.read(cx).background_shells.len(), 1);
    });
}
