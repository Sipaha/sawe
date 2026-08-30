//! Mock `AgentServer` / `AgentConnection` reachable from in-crate unit tests
//! AND from `tests/*_e2e_test.rs` integration tests via the `test-support`
//! feature. The unit tests in `store.rs` re-export these via
//! `crate::test_support::*`; integration tests (Phase 5.6) consume them via
//! `solution_agent::test_support::MockAgentServer`.
//!
//! Kept feature-gated so production builds never see them.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gpui::{App, AppContext, SharedString, Task};
use util::ResultExt as _;

/// AgentConnection mock that returns a real `AcpThread` from `new_session`
/// so `create_session` can complete without going through a real subprocess.
///
/// `prompt()` returns an error by default. Tests that want to control turn
/// timing can pass a `prompt_gate` receiver; `prompt()` then awaits the
/// gate before returning `Ok(EndTurn)`.
pub struct MockConnection {
    next_session: Cell<u64>,
    prompt_gate: parking_lot::Mutex<Option<async_channel::Receiver<()>>>,
    // Counts `cancel()` calls so tests can assert the store forwarded a stop
    // exactly once (and didn't double-forward on a repeated cancel).
    cancel_count: Arc<AtomicUsize>,
    // OPT-IN, default `false`. Several tests
    // (`reconnect_resume_failure_surfaces_after_retry`,
    // `restart_and_watchdog_respawn_report_differently`,
    // `restart_agent_keeps_the_session_and_its_history`) depend on the mock
    // refusing to resume, so this must stay off unless a test asks for it.
    supports_resume: bool,
}

impl MockConnection {
    /// Every option at once. The named constructors below are sugar over this,
    /// and `MockAgentServer::connect` builds through it directly — a
    /// priority-ordered `match` over the options silently DROPPED whichever one
    /// lost, which is how a server built with both a prompt gate and resume
    /// support quietly produced a connection that could not resume.
    pub fn configured(
        prompt_gate: Option<async_channel::Receiver<()>>,
        cancel_count: Option<Arc<AtomicUsize>>,
        supports_resume: bool,
    ) -> Self {
        Self {
            next_session: Cell::new(0),
            prompt_gate: parking_lot::Mutex::new(prompt_gate),
            cancel_count: cancel_count.unwrap_or_else(|| Arc::new(AtomicUsize::new(0))),
            supports_resume,
        }
    }

    pub fn new() -> Self {
        Self::configured(None, None, false)
    }

    /// A mock whose `resume_session` SUCCEEDS, re-attaching to the id it is
    /// handed. Needed to exercise `SolutionAgentStore::resume_session`'s
    /// fresh-entity branch, which is only reachable after a successful attach.
    pub fn with_resume_support() -> Self {
        Self::configured(None, None, true)
    }

    pub fn with_prompt_gate(gate: async_channel::Receiver<()>) -> Self {
        Self::configured(Some(gate), None, false)
    }

    pub fn with_cancel_count(cancel_count: Arc<AtomicUsize>) -> Self {
        Self::configured(None, Some(cancel_count), false)
    }
}

impl Default for MockConnection {
    fn default() -> Self {
        Self::new()
    }
}

impl acp_thread::AgentConnection for MockConnection {
    fn agent_id(&self) -> project::AgentId {
        project::AgentId::new("mock-agent")
    }
    fn telemetry_id(&self) -> SharedString {
        SharedString::from("mock")
    }
    fn new_session(
        self: Rc<Self>,
        project: gpui::Entity<project::Project>,
        work_dirs: util::path_list::PathList,
        cx: &mut App,
    ) -> Task<anyhow::Result<gpui::Entity<acp_thread::AcpThread>>> {
        let n = self.next_session.get();
        self.next_session.set(n + 1);
        let session_id = agent_client_protocol::schema::SessionId::new(format!("mock-{n}"));
        let action_log = cx.new(|_| action_log::ActionLog::new(project.clone()));
        let connection: Rc<dyn acp_thread::AgentConnection> = self;
        let thread = cx.new(|cx| {
            acp_thread::AcpThread::new(
                None,
                None,
                Some(work_dirs),
                connection,
                project,
                action_log,
                session_id,
                watch::Receiver::constant(agent_client_protocol::schema::PromptCapabilities::new()),
                cx,
            )
        });
        Task::ready(Ok(thread))
    }
    /// The mock supports the history-less reattach (`resume_session`), not the
    /// full replay (`load_session`), which is the same shape the native claude
    /// backend has: `--resume` re-attaches the subprocess but does NOT re-emit
    /// the transcript, which is exactly why `SolutionAgentStore::resume_session`
    /// has to reconstruct the cold prefix from the DB itself.
    fn supports_resume_session(&self) -> bool {
        self.supports_resume
    }
    fn resume_session(
        self: Rc<Self>,
        session_id: agent_client_protocol::schema::SessionId,
        project: gpui::Entity<project::Project>,
        work_dirs: util::path_list::PathList,
        _title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<anyhow::Result<gpui::Entity<acp_thread::AcpThread>>> {
        if !self.supports_resume {
            return Task::ready(Err(anyhow::anyhow!("Resuming sessions is not supported")));
        }
        // Re-attaches to the id it was handed, unlike `new_session`, which mints
        // `mock-<n>`. A test asserting that a resumed session kept its
        // `acp_session_id` depends on that.
        let action_log = cx.new(|_| action_log::ActionLog::new(project.clone()));
        let connection: Rc<dyn acp_thread::AgentConnection> = self;
        let thread = cx.new(|cx| {
            acp_thread::AcpThread::new(
                None,
                None,
                Some(work_dirs),
                connection,
                project,
                action_log,
                session_id,
                watch::Receiver::constant(agent_client_protocol::schema::PromptCapabilities::new()),
                cx,
            )
        });
        Task::ready(Ok(thread))
    }
    fn auth_methods(&self) -> &[agent_client_protocol::schema::AuthMethod] {
        &[]
    }
    fn authenticate(
        &self,
        _method: agent_client_protocol::schema::AuthMethodId,
        _cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        Task::ready(Ok(()))
    }
    fn prompt(
        &self,
        _user_message_id: acp_thread::UserMessageId,
        _params: agent_client_protocol::schema::PromptRequest,
        cx: &mut App,
    ) -> Task<anyhow::Result<agent_client_protocol::schema::PromptResponse>> {
        let gate = self.prompt_gate.lock().clone();
        match gate {
            None => Task::ready(Err(anyhow::anyhow!("not used in this test"))),
            // `gate.send(())` releases the prompt with `Ok(EndTurn)`;
            // dropping the sender without sending releases it with `Err(...)`.
            // The latter lets a test simulate "the in-flight turn errored
            // mid-flight" (e.g. for the rotation-race regression in
            // `send_message_blocks`).
            Some(gate) => cx.spawn(async move |_| match gate.recv().await {
                Ok(()) => Ok(agent_client_protocol::schema::PromptResponse::new(
                    agent_client_protocol::schema::StopReason::EndTurn,
                )),
                Err(_) => Err(anyhow::anyhow!("mock prompt failed (gate closed)")),
            }),
        }
    }
    fn cancel(&self, _session_id: &agent_client_protocol::schema::SessionId, _cx: &mut App) {
        self.cancel_count.fetch_add(1, Ordering::SeqCst);
    }
    fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
        self
    }
}

/// AgentServer mock whose `connect()` counts invocations and lazily
/// constructs a single shared `MockConnection` on first call. Used to
/// assert that the pool collapses parallel calls onto one spawn.
///
/// `MockConnection` is `Rc<...>` (and hence `!Send`), but `AgentServer`
/// is `Send`-bound — so we keep the `Rc` inside a `RefCell` that is
/// constructed only on the foreground thread inside `connect()`.
pub struct MockAgentServer {
    connect_count: Arc<AtomicUsize>,
    // Optional async gate to hold connect() pending until the test releases it.
    gate: parking_lot::Mutex<Option<async_channel::Receiver<()>>>,
    // Optional gate forwarded to the spawned `MockConnection::prompt`.
    prompt_gate: parking_lot::Mutex<Option<async_channel::Receiver<()>>>,
    // Optional cancel counter forwarded to the spawned `MockConnection` so a
    // test can assert how many times the store forwarded `cancel()`.
    cancel_count: Option<Arc<AtomicUsize>>,
    // Forwarded to the spawned `MockConnection`; see `MockConnection::supports_resume`.
    supports_resume: bool,
}

// SAFETY: We only ever touch `gate` from the foreground thread (its
// contents are `!Send` `Rc`s, but `async_channel::Receiver` itself is
// `Send`). The `Mutex` guards the option swap.
unsafe impl Send for MockAgentServer {}

impl MockAgentServer {
    /// Every option at once; the named constructors below are sugar over this.
    /// Same reason as [`MockConnection::configured`] — these options are not
    /// mutually exclusive, and expressing them as separate constructors is what
    /// made it possible to build a server that could not say "gate the prompt
    /// AND allow resume".
    pub fn configured(
        connect_count: Arc<AtomicUsize>,
        gate: Option<async_channel::Receiver<()>>,
        prompt_gate: Option<async_channel::Receiver<()>>,
        cancel_count: Option<Arc<AtomicUsize>>,
        supports_resume: bool,
    ) -> Self {
        Self {
            connect_count,
            gate: parking_lot::Mutex::new(gate),
            prompt_gate: parking_lot::Mutex::new(prompt_gate),
            cancel_count,
            supports_resume,
        }
    }

    pub fn new(connect_count: Arc<AtomicUsize>) -> Self {
        Self::configured(connect_count, None, None, None, false)
    }

    pub fn with_gate(connect_count: Arc<AtomicUsize>, gate: async_channel::Receiver<()>) -> Self {
        Self::configured(connect_count, Some(gate), None, None, false)
    }

    pub fn with_prompt_gate(
        connect_count: Arc<AtomicUsize>,
        prompt_gate: async_channel::Receiver<()>,
    ) -> Self {
        Self::configured(connect_count, None, Some(prompt_gate), None, false)
    }

    pub fn with_cancel_count(
        connect_count: Arc<AtomicUsize>,
        cancel_count: Arc<AtomicUsize>,
    ) -> Self {
        Self::configured(connect_count, None, None, Some(cancel_count), false)
    }

    /// Hands out a `MockConnection::with_resume_support`, so
    /// `SolutionAgentStore::resume_session` can actually attach.
    pub fn with_resume_support(connect_count: Arc<AtomicUsize>) -> Self {
        Self::configured(connect_count, None, None, None, true)
    }
}

impl agent_servers::AgentServer for MockAgentServer {
    fn logo(&self) -> ui::IconName {
        ui::IconName::Sparkle
    }
    fn agent_id(&self) -> project::AgentId {
        project::AgentId::new("mock-agent")
    }
    fn connect(
        &self,
        _delegate: agent_servers::AgentServerDelegate,
        _project: gpui::Entity<project::Project>,
        cx: &mut App,
    ) -> Task<anyhow::Result<Rc<dyn acp_thread::AgentConnection>>> {
        self.connect_count.fetch_add(1, Ordering::SeqCst);
        let gate = self.gate.lock().clone();
        let prompt_gate = self.prompt_gate.lock().clone();
        let cancel_count = self.cancel_count.clone();
        let supports_resume = self.supports_resume;
        cx.spawn(async move |_| {
            if let Some(gate) = gate {
                gate.recv().await.log_err();
            }
            // Every option forwarded, none of them mutually exclusive. This was
            // a priority `match` and it silently dropped the losers — a server
            // built `with_prompt_gate` AND resume support handed out a
            // connection that refused to resume, with no error anywhere.
            let connection: Rc<dyn acp_thread::AgentConnection> = Rc::new(
                MockConnection::configured(prompt_gate, cancel_count, supports_resume),
            );
            Ok(connection)
        })
    }
    fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
        self
    }
}
