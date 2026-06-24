//! Integration tests driving a fake `claude` binary (a bash script) through
//! [`claude_native::process::ClaudeProcess`]. These exercise the real spawn +
//! stdio async tasks; the protocol/translate units are tested in-crate.

use std::path::PathBuf;
use std::time::Duration;

use acp_thread::{
    AcpThread, AgentConnection as _, AgentThreadEntry, ToolCall, ToolCallStatus,
};
use agent_client_protocol::schema as acp;
use agent_servers::{AgentServer, AgentServerDelegate};
use claude_native::command::{ClaudeCommandSpec, SessionArg};
use claude_native::process::ClaudeProcess;
use claude_native::protocol::{InputMessage, OutputMessage, System};
use claude_native::{ClaudeNativeAgentServer, ClaudeNativeConnection};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{Entity, TestAppContext};
use project::{AgentId, FakeFs, Project};
use std::rc::Rc;
use util::path_list::PathList;

fn mock_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_claude.sh")
}

fn spec_for(binary: PathBuf, capture: Option<PathBuf>) -> ClaudeCommandSpec {
    let mut extra_env = Vec::new();
    if let Some(path) = capture {
        extra_env.push((
            "MOCK_CLAUDE_CAPTURE".to_string(),
            path.to_string_lossy().into_owned(),
        ));
    }
    ClaudeCommandSpec {
        binary,
        work_dir: std::env::temp_dir(),
        session: SessionArg::New("mock-session".into()),
        mcp_servers_json: r#"{"mcpServers":{}}"#.into(),
        append_system_prompt: None,
        extra_env,
        model: None,
    }
}

/// Pull the next message off `incoming`, failing the test if it does not arrive
/// before the deadline (so a wedged reader is a test failure, not a hang).
async fn recv_with_timeout(
    process: &mut ClaudeProcess,
    cx: &mut TestAppContext,
) -> Option<OutputMessage> {
    let timeout = cx.background_executor.timer(Duration::from_secs(10)).fuse();
    let next = process.incoming.next().fuse();
    futures::pin_mut!(timeout, next);
    futures::select! {
        message = next => message,
        _ = timeout => panic!("timed out waiting for output message"),
    }
}

#[gpui::test]
async fn reads_init_message(cx: &mut TestAppContext) {
    // Real subprocess stdio is driven by the executor's I/O reactor, which only
    // makes progress when the deterministic test executor is allowed to park.
    cx.executor().allow_parking();

    let spec = spec_for(mock_binary(), None);
    let mut process = cx
        .update(|cx| ClaudeProcess::spawn(spec, cx))
        .expect("spawn mock claude");

    process
        .outgoing
        .unbounded_send(InputMessage::user_text("hello"))
        .expect("send user message");

    let message = recv_with_timeout(&mut process, cx).await;
    match message {
        Some(OutputMessage::System(System::Init { session_id, .. })) => {
            assert_eq!(session_id, "mock-session");
        }
        other => panic!("expected init system message, got {other:?}"),
    }
}

/// Drain `incoming` until a message matching `predicate` arrives, failing on a
/// timeout so a missing message is a test failure rather than a hang.
async fn recv_until(
    process: &mut ClaudeProcess,
    cx: &mut TestAppContext,
    mut predicate: impl FnMut(&OutputMessage) -> bool,
) -> OutputMessage {
    loop {
        match recv_with_timeout(process, cx).await {
            Some(message) if predicate(&message) => return message,
            Some(_) => continue,
            None => panic!("incoming closed before matching message arrived"),
        }
    }
}

#[gpui::test]
async fn delivers_control_request_and_writes_response(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let capture = std::env::temp_dir().join(format!(
        "claude_native_capture_{}.ndjson",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&capture);

    let mut spec = spec_for(mock_binary(), Some(capture.clone()));
    spec.extra_env
        .push(("MOCK_CLAUDE_CONTROL".to_string(), "1".to_string()));

    let mut process = cx
        .update(|cx| ClaudeProcess::spawn(spec, cx))
        .expect("spawn mock claude");

    process
        .outgoing
        .unbounded_send(InputMessage::user_text("hello"))
        .expect("send user message");

    let request = recv_until(&mut process, cx, |message| {
        matches!(message, OutputMessage::ControlRequest(_))
    })
    .await;
    let request_id = match request {
        OutputMessage::ControlRequest(envelope) => envelope.request_id,
        other => panic!("expected control request, got {other:?}"),
    };

    process
        .send_control_response(&request_id, true)
        .expect("write control response");

    // The mock only emits `result` after it reads our control_response, so
    // waiting for `result` proves the response reached its stdin.
    recv_until(&mut process, cx, |message| {
        matches!(message, OutputMessage::Result(_))
    })
    .await;

    let captured = std::fs::read_to_string(&capture).expect("read capture");
    assert!(
        captured.contains(r#""type":"control_response""#)
            && captured.contains(r#""behavior":"allow""#),
        "captured stdin missing control_response: {captured}"
    );
    let _ = std::fs::remove_file(&capture);
}

#[gpui::test]
async fn closes_incoming_and_resolves_wait_on_exit(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let spec = spec_for(mock_binary(), None);
    let mut process = cx
        .update(|cx| ClaudeProcess::spawn(spec, cx))
        .expect("spawn mock claude");

    let exited = process.wait_status();

    // Closing the outgoing sender drops stdin; the mock loop hits EOF and exits.
    drop(process.outgoing.clone());
    process.outgoing.close_channel();

    // Reader must observe EOF and close `incoming`.
    while recv_with_timeout(&mut process, cx).await.is_some() {}

    let status = {
        let timeout = cx.background_executor.timer(Duration::from_secs(10)).fuse();
        let exited = exited.fuse();
        futures::pin_mut!(timeout, exited);
        futures::select! {
            status = exited => status,
            _ = timeout => panic!("timed out waiting for process exit"),
        }
    };
    assert!(
        status.is_some(),
        "wait_status resolved without an exit status"
    );
}

async fn init_test(cx: &mut TestAppContext) -> Entity<Project> {
    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
    });
    cx.executor().allow_parking();
    let fs = FakeFs::new(cx.executor());
    Project::test(fs, [], cx).await
}

/// Build a native connection wired to the mock `claude` script, plus the extra
/// env a scenario needs (capture path, control/no-result toggles).
async fn connect_mock(
    project: &Entity<Project>,
    extra_env: Vec<(String, String)>,
    cx: &mut TestAppContext,
) -> Rc<ClaudeNativeConnection> {
    let server =
        ClaudeNativeAgentServer::with_binary(AgentId::new("claude-acp"), mock_binary(), extra_env);
    let store = project.read_with(cx, |project, _| project.agent_server_store().clone());
    let delegate = AgentServerDelegate::new(store, None);
    let connection = cx
        .update(|cx| AgentServer::connect(&server, delegate, project.clone(), cx))
        .await
        .expect("connect native backend");
    connection
        .into_any()
        .downcast::<ClaudeNativeConnection>()
        .expect("native connection type")
}

async fn await_thread(
    task: gpui::Task<anyhow::Result<Entity<AcpThread>>>,
    cx: &mut TestAppContext,
) -> Entity<AcpThread> {
    let timeout = cx.background_executor.timer(Duration::from_secs(10)).fuse();
    let task = task.fuse();
    futures::pin_mut!(timeout, task);
    futures::select! {
        thread = task => thread.expect("session created"),
        _ = timeout => panic!("timed out creating session"),
    }
}

#[gpui::test]
async fn new_session_captures_init_session_id(cx: &mut TestAppContext) {
    let project = init_test(cx).await;
    let connection = connect_mock(&project, Vec::new(), cx).await;

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    // Session creation must NOT block on `init`: the real `claude` only emits
    // `init` after the first user turn, so `new_session` returns immediately
    // adopting the id it spawned with (regression test for the startup deadlock).
    let thread = await_thread(task, cx).await;

    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
    assert!(!session_id.0.is_empty(), "thread must have a session id");
    // The thread's id is the one the connection registered the live process
    // under — proving the adopted id is consistent end to end.
    assert!(
        connection
            .session_process_id_for_test(&session_id)
            .is_some(),
        "session must be registered under the thread's id"
    );
}

#[gpui::test]
async fn prompt_resolves_on_result_and_streams_text(cx: &mut TestAppContext) {
    let project = init_test(cx).await;
    let connection = connect_mock(&project, Vec::new(), cx).await;

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    let request = acp::PromptRequest::new(session_id, prompt);
    let prompt_task =
        cx.update(|cx| connection.prompt(acp_thread::UserMessageId::new(), request, cx));

    let response = {
        let timeout = cx.background_executor.timer(Duration::from_secs(10)).fuse();
        let prompt_task = prompt_task.fuse();
        futures::pin_mut!(timeout, prompt_task);
        futures::select! {
            response = prompt_task => response.expect("prompt resolved Ok"),
            _ = timeout => panic!("prompt did not resolve on result message"),
        }
    };
    assert_eq!(response.stop_reason, acp::StopReason::EndTurn);

    let markdown = thread.read_with(cx, |thread, cx| thread.to_markdown(cx));
    assert!(
        markdown.contains("Hi"),
        "streamed assistant text missing from thread: {markdown}"
    );
}

#[gpui::test]
async fn can_use_tool_is_auto_approved_without_prompt(cx: &mut TestAppContext) {
    let capture = std::env::temp_dir().join(format!(
        "claude_native_authz_capture_{}.ndjson",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&capture);

    let project = init_test(cx).await;
    let connection = connect_mock(
        &project,
        vec![
            ("MOCK_CLAUDE_CONTROL".to_string(), "1".to_string()),
            (
                "MOCK_CLAUDE_CAPTURE".to_string(),
                capture.to_string_lossy().into_owned(),
            ),
        ],
        cx,
    )
    .await;

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "run a command".to_string(),
    ))];
    let request = acp::PromptRequest::new(session_id, prompt);
    let prompt_task =
        cx.update(|cx| connection.prompt(acp_thread::UserMessageId::new(), request, cx));

    // The mock emits a `can_use_tool` control request. The connection
    // AUTO-APPROVES it (the fork's bypass stance — see
    // `connection::spawn_tool_authorization`): the thread must NEVER surface a
    // confirmation prompt, and the turn completes on its own without any user
    // interaction.
    let response = await_prompt(prompt_task, cx, Duration::from_secs(10)).await;
    assert_eq!(response.stop_reason, acp::StopReason::EndTurn);

    // No gate was ever shown — the auto-approve path skips
    // `request_tool_call_authorization` entirely.
    let gated = thread.read_with(cx, |thread, _| {
        thread.entries().iter().any(|entry| {
            matches!(
                entry,
                AgentThreadEntry::ToolCall(ToolCall {
                    status: ToolCallStatus::WaitingForConfirmation { .. },
                    ..
                })
            )
        })
    });
    assert!(
        !gated,
        "auto-approval must not surface a WaitingForConfirmation prompt"
    );

    // The connection still wrote `control_response{behavior:"allow"}` to
    // claude's stdin — that's what lets the mock finish the turn.
    let captured = std::fs::read_to_string(&capture).expect("read capture");
    assert!(
        captured.contains(r#""type":"control_response""#)
            && captured.contains(r#""behavior":"allow""#),
        "captured stdin missing auto-approve control_response: {captured}"
    );
    let _ = std::fs::remove_file(&capture);
}

/// Race a prompt task against a timer, returning the prompt's response if it
/// resolves first or panicking on timeout. Used by the cancel tests where the
/// turn must be force-resolved (clean interrupt or escalation kill+resume).
async fn await_prompt(
    prompt_task: gpui::Task<anyhow::Result<acp::PromptResponse>>,
    cx: &mut TestAppContext,
    timeout: Duration,
) -> acp::PromptResponse {
    let timer = cx.background_executor.timer(timeout).fuse();
    let prompt_task = prompt_task.fuse();
    futures::pin_mut!(timer, prompt_task);
    futures::select! {
        response = prompt_task => response.expect("prompt resolved Ok"),
        _ = timer => panic!("prompt did not resolve within {timeout:?}"),
    }
}

#[gpui::test]
async fn cancel_clean_interrupt_resolves_without_kill(cx: &mut TestAppContext) {
    let capture = std::env::temp_dir().join(format!(
        "claude_native_cancel_clean_{}.ndjson",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&capture);

    let project = init_test(cx).await;
    let connection = connect_mock(
        &project,
        vec![
            ("MOCK_CLAUDE_OBEY_INTERRUPT".to_string(), "1".to_string()),
            (
                "MOCK_CLAUDE_CAPTURE".to_string(),
                capture.to_string_lossy().into_owned(),
            ),
        ],
        cx,
    )
    .await;
    // A short escalation window so a regression (escalation firing despite the
    // clean interrupt) would surface, while the mock's `result(cancelled)` still
    // wins the race.
    connection.set_escalation_timeout_for_test(Duration::from_secs(30));

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    // Record the spawned process's pid so we can prove it was NOT replaced.
    let pid_before = connection.session_process_id_for_test(&session_id);
    assert!(pid_before.is_some(), "session should have a live process");

    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    let request = acp::PromptRequest::new(session_id.clone(), prompt);
    let prompt_task =
        cx.update(|cx| connection.prompt(acp_thread::UserMessageId::new(), request, cx));

    // Let the turn start streaming before cancelling.
    cx.run_until_parked();
    cx.update(|cx| connection.cancel(&session_id, cx));

    let response = await_prompt(prompt_task, cx, Duration::from_secs(10)).await;
    assert_eq!(response.stop_reason, acp::StopReason::Cancelled);

    // The interrupt must have been written to claude's stdin.
    let captured = std::fs::read_to_string(&capture).expect("read capture");
    assert!(
        captured.contains(r#""subtype":"interrupt""#),
        "captured stdin missing interrupt control_request: {captured}"
    );

    // The session must still be present with the SAME process (no kill+respawn).
    let pid_after = connection.session_process_id_for_test(&session_id);
    assert_eq!(
        pid_after, pid_before,
        "clean interrupt must not kill/replace the process"
    );
    let _ = std::fs::remove_file(&capture);
}

#[gpui::test]
async fn cancel_escalates_to_kill_and_resume(cx: &mut TestAppContext) {
    let project = init_test(cx).await;
    let connection = connect_mock(
        &project,
        vec![("MOCK_CLAUDE_IGNORE_INTERRUPT".to_string(), "1".to_string())],
        cx,
    )
    .await;
    // Tiny escalation window so the test does not wait the real 30s.
    connection.set_escalation_timeout_for_test(Duration::from_millis(50));

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    let pid_before = connection.session_process_id_for_test(&session_id);
    assert!(pid_before.is_some());

    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    let request = acp::PromptRequest::new(session_id.clone(), prompt);
    let prompt_task =
        cx.update(|cx| connection.prompt(acp_thread::UserMessageId::new(), request, cx));

    cx.run_until_parked();
    cx.update(|cx| connection.cancel(&session_id, cx));

    // The mock ignores the interrupt and never sends `result`; only the
    // escalation (kill + resume + force-resolve) can resolve the prompt.
    let response = await_prompt(prompt_task, cx, Duration::from_secs(10)).await;
    assert_eq!(response.stop_reason, acp::StopReason::Cancelled);

    // The prompt resolves the instant the escalation force-resolves the oneshot,
    // which is before the resume spawn + `init` completes. Poll until the new
    // process lands in the map (or fail on timeout).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let pid_after = loop {
        cx.run_until_parked();
        let pid = connection.session_process_id_for_test(&session_id);
        if pid.is_some() && pid != pid_before {
            break pid;
        }
        if std::time::Instant::now() >= deadline {
            panic!("escalation never respawned the process (pid still {pid:?})");
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    };
    // After escalation the session must still exist (resumed) but be backed by a
    // different process than the one we killed.
    assert!(
        pid_after.is_some(),
        "session must survive escalation (resumed)"
    );
    assert_ne!(
        pid_after, pid_before,
        "escalation must kill and respawn the process"
    );
}

#[gpui::test]
async fn repeated_cancel_does_not_double_escalate(cx: &mut TestAppContext) {
    let project = init_test(cx).await;
    let connection = connect_mock(
        &project,
        vec![("MOCK_CLAUDE_IGNORE_INTERRUPT".to_string(), "1".to_string())],
        cx,
    )
    .await;
    // Tiny escalation window so the test does not wait the real 30s.
    connection.set_escalation_timeout_for_test(Duration::from_millis(50));

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    let pid_before = connection.session_process_id_for_test(&session_id);
    assert!(pid_before.is_some());

    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    let request = acp::PromptRequest::new(session_id.clone(), prompt);
    let prompt_task =
        cx.update(|cx| connection.prompt(acp_thread::UserMessageId::new(), request, cx));

    cx.run_until_parked();

    // Cancel TWICE in quick succession, before the (50ms) escalation can fire.
    // A non-idempotent cancel would arm a second escalation, restarting the
    // clock and ultimately killing+resuming twice.
    cx.update(|cx| connection.cancel(&session_id, cx));
    cx.update(|cx| connection.cancel(&session_id, cx));

    // The double cancel must have armed exactly ONE escalation — this is the
    // direct idempotency assertion (a second arm restarts the 30s clock and
    // schedules a second kill+resume).
    assert_eq!(
        connection.escalations_armed_for_test(),
        1,
        "repeated cancel for one in-flight turn must arm a single escalation"
    );

    // The mock ignores the interrupt and never sends `result`; only the
    // escalation (kill + resume + force-resolve) can resolve the prompt.
    let response = await_prompt(prompt_task, cx, Duration::from_secs(10)).await;
    assert_eq!(response.stop_reason, acp::StopReason::Cancelled);

    // Poll until the (single) escalation respawns the process.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let pid_after = loop {
        cx.run_until_parked();
        let pid = connection.session_process_id_for_test(&session_id);
        if pid.is_some() && pid != pid_before {
            break pid;
        }
        if std::time::Instant::now() >= deadline {
            panic!("escalation never respawned the process (pid still {pid:?})");
        }
        cx.background_executor
            .timer(Duration::from_millis(20))
            .await;
    };
    assert_ne!(
        pid_after, pid_before,
        "escalation must kill and respawn the process once"
    );

    // The new process must be stable: a second escalation (from the double
    // cancel) would kill+resume again, changing the pid a second time. Advance
    // the clock well past another escalation window and assert the pid holds.
    cx.background_executor
        .timer(Duration::from_millis(200))
        .await;
    cx.run_until_parked();
    let pid_settled = connection.session_process_id_for_test(&session_id);
    assert_eq!(
        pid_settled, pid_after,
        "a repeated cancel must not arm a second escalation (pid changed again)"
    );
}

#[gpui::test]
async fn prompt_stays_pending_without_result(cx: &mut TestAppContext) {
    let project = init_test(cx).await;
    let connection = connect_mock(
        &project,
        vec![("MOCK_CLAUDE_NO_RESULT".to_string(), "1".to_string())],
        cx,
    )
    .await;

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    let request = acp::PromptRequest::new(session_id, prompt);
    let prompt_task =
        cx.update(|cx| connection.prompt(acp_thread::UserMessageId::new(), request, cx));

    // The mock streams text but never sends `result`; the prompt must remain
    // pending. Race it against a short timer and assert the timer wins.
    let timeout = cx.background_executor.timer(Duration::from_secs(2)).fuse();
    let prompt_task = prompt_task.fuse();
    futures::pin_mut!(timeout, prompt_task);
    futures::select! {
        _ = prompt_task => panic!("prompt resolved despite no result message (hang scenario)"),
        _ = timeout => {}
    }
}

/// On Linux a SIGKILL'd child that hasn't been reaped lingers as a zombie
/// (`/proc/<pid>/stat` state `Z`); a fully gone process has no `/proc` entry.
/// Either state means the process is no longer running.
fn process_is_killed(process_id: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{process_id}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .map(|state| state == "Z")
            .unwrap_or(true),
    }
}

#[gpui::test]
async fn hook_inject_round_trips_additional_context(cx: &mut TestAppContext) {
    let capture = std::env::temp_dir().join(format!(
        "claude_native_hook_inject_{}.ndjson",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&capture);

    let project = init_test(cx).await;
    let connection = connect_mock(
        &project,
        vec![
            ("MOCK_CLAUDE_HOOK_INJECT".to_string(), "1".to_string()),
            (
                "MOCK_CLAUDE_CAPTURE".to_string(),
                capture.to_string_lossy().into_owned(),
            ),
        ],
        cx,
    )
    .await;

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    // Buffer the follow-up BEFORE sending the prompt. The mock fires its hook
    // callback after the text delta; the pump will respond with our marker as
    // `additionalContext`, and the mock echoes it in the final `result.result`.
    connection.inject_user_message(&session_id, "MOCK_HOOK_MARKER".to_string());

    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    let request = acp::PromptRequest::new(session_id, prompt);
    let prompt_task =
        cx.update(|cx| connection.prompt(acp_thread::UserMessageId::new(), request, cx));

    let response = {
        let timeout = cx.background_executor.timer(Duration::from_secs(10)).fuse();
        let prompt_task = prompt_task.fuse();
        futures::pin_mut!(timeout, prompt_task);
        futures::select! {
            response = prompt_task => response.expect("prompt resolved Ok"),
            _ = timeout => panic!("prompt did not resolve after hook round trip"),
        }
    };
    assert_eq!(response.stop_reason, acp::StopReason::EndTurn);

    // The connection must have written an `initialize` (hooks registration) and
    // a `control_response` carrying our marker as `additionalContext`. The
    // `result.result` field (where the mock echoes the marker) is not surfaced
    // through `acp::PromptResponse`; the capture-side assertions below are the
    // end-to-end proof that our hook reply reached the mock with the marker.
    let captured = std::fs::read_to_string(&capture).expect("read capture");
    assert!(
        captured.contains(r#""subtype":"initialize""#),
        "captured stdin missing initialize control_request: {captured}"
    );
    assert!(
        captured.contains(r#""hookCallbackIds":["pti"]"#),
        "captured stdin missing PostToolUse hook callback id: {captured}"
    );
    assert!(
        captured.contains(r#""type":"control_response""#)
            && captured.contains("MOCK_HOOK_MARKER")
            && captured.contains("additionalContext"),
        "captured stdin missing hook control_response with additionalContext: {captured}"
    );
    let _ = std::fs::remove_file(&capture);
}

#[gpui::test]
async fn hook_pulls_from_registered_store_closure(cx: &mut TestAppContext) {
    let capture = std::env::temp_dir().join(format!(
        "claude_native_hook_store_pull_{}.ndjson",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&capture);

    let project = init_test(cx).await;
    let connection = connect_mock(
        &project,
        vec![
            ("MOCK_CLAUDE_HOOK_INJECT".to_string(), "1".to_string()),
            (
                "MOCK_CLAUDE_CAPTURE".to_string(),
                capture.to_string_lossy().into_owned(),
            ),
        ],
        cx,
    )
    .await;

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    // Register a store pull BEFORE sending the prompt — this is the production
    // path (instead of the test-only `inject_user_message`). The pump must
    // invoke this closure at the hook and ship its output back as
    // `additionalContext`, which the mock echoes in its final result.
    connection.set_store_pull(Rc::new(|_sid, _agent_id, _eot, _cx| {
        Some("STORE_PULL_MARKER".to_string())
    }));

    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    let request = acp::PromptRequest::new(session_id, prompt);
    let prompt_task =
        cx.update(|cx| connection.prompt(acp_thread::UserMessageId::new(), request, cx));

    let response = {
        let timeout = cx.background_executor.timer(Duration::from_secs(10)).fuse();
        let prompt_task = prompt_task.fuse();
        futures::pin_mut!(timeout, prompt_task);
        futures::select! {
            response = prompt_task => response.expect("prompt resolved Ok"),
            _ = timeout => panic!("prompt did not resolve after hook round trip"),
        }
    };
    assert_eq!(response.stop_reason, acp::StopReason::EndTurn);

    let captured = std::fs::read_to_string(&capture).expect("read capture");
    assert!(
        captured.contains(r#""type":"control_response""#)
            && captured.contains("STORE_PULL_MARKER")
            && captured.contains("additionalContext"),
        "captured stdin missing hook control_response carrying the store-pull marker: {captured}"
    );
    let _ = std::fs::remove_file(&capture);
}

#[gpui::test]
async fn subagent_tool_use_carries_parent_meta_through_pump(cx: &mut TestAppContext) {
    // Drives a fake subagent (`parent_tool_use_id != null`) assistant message
    // through the real update-pump and asserts the resulting ToolCall reaches
    // the AcpThread. The wire-level `_meta.claudeCode.parentToolUseId` stamp
    // itself is exercised by translate.rs unit tests — AcpThread's `ToolCall`
    // entry currently discards `acp::ToolCall.meta` (Etap 2 will surface it
    // as `subagent_id`), so observing the stamped value here would require
    // a temporary probe; this end-to-end test instead proves the stamping
    // path doesn't drop the subagent ToolCall.
    let project = init_test(cx).await;
    let connection = connect_mock(
        &project,
        vec![("MOCK_CLAUDE_SUBAGENT".to_string(), "1".to_string())],
        cx,
    )
    .await;

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "run a subagent".to_string(),
    ))];
    let request = acp::PromptRequest::new(session_id, prompt);
    let prompt_task =
        cx.update(|cx| connection.prompt(acp_thread::UserMessageId::new(), request, cx));

    let response = {
        let timeout = cx.background_executor.timer(Duration::from_secs(10)).fuse();
        let prompt_task = prompt_task.fuse();
        futures::pin_mut!(timeout, prompt_task);
        futures::select! {
            response = prompt_task => response.expect("prompt resolved Ok"),
            _ = timeout => panic!("prompt did not resolve on result message"),
        }
    };
    assert_eq!(response.stop_reason, acp::StopReason::EndTurn);

    let found_subagent_tool_call = thread.read_with(cx, |thread, _| {
        thread.entries().iter().any(|entry| {
            matches!(
                entry,
                AgentThreadEntry::ToolCall(ToolCall { id, .. })
                    if id.0.as_ref() == "toolu_child_abc"
            )
        })
    });
    assert!(
        found_subagent_tool_call,
        "subagent tool_use must surface as a ToolCall entry on the thread"
    );
}

#[gpui::test]
async fn close_session_kills_process_and_removes_session(cx: &mut TestAppContext) {
    let project = init_test(cx).await;
    let connection = connect_mock(&project, Vec::new(), cx).await;

    let task = cx.update(|cx| {
        Rc::clone(&connection).new_session(
            project.clone(),
            PathList::new(&[std::env::temp_dir().as_path()]),
            cx,
        )
    });
    let thread = await_thread(task, cx).await;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());

    let process_id = connection
        .session_process_id_for_test(&session_id)
        .expect("live session has a process id");

    let close = cx.update(|cx| Rc::clone(&connection).close_session(&session_id, cx));
    close.await.expect("close_session ok");

    // Session is removed from the map.
    assert_eq!(
        connection.session_process_id_for_test(&session_id),
        None,
        "session must be removed after close"
    );

    // The backing process is killed (gone or zombie). Poll briefly: kill is
    // delivered synchronously but the OS may take a beat to tear it down.
    let deadline = cx.background_executor.timer(Duration::from_secs(10)).fuse();
    futures::pin_mut!(deadline);
    loop {
        if process_is_killed(process_id) {
            break;
        }
        let tick = cx
            .background_executor
            .timer(Duration::from_millis(20))
            .fuse();
        futures::pin_mut!(tick);
        futures::select! {
            _ = tick => continue,
            _ = deadline => panic!("process {process_id} still running after close"),
        }
    }
}
