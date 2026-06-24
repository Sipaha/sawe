use anyhow::{Result, anyhow};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, Subscription, Task,
    WeakEntity, Window,
};
use solution_agent::session_view::SolutionSessionView;
use solution_agent::store::{SolutionAgentStore, SolutionAgentStoreEvent};
use solution_agent::{AgentServerId, SolutionSession, SolutionSessionId};
use solutions::SolutionId;
use workspace::Workspace;

/// Spawns AI-chat session views on behalf of `ConsolePanel`. Also forwards
/// "session created" events from the store so the panel can open a tab
/// when a session was created outside the panel UI (e.g., via MCP).
pub struct ChatProvider {
    workspace: WeakEntity<Workspace>,
    store: Entity<SolutionAgentStore>,
    _subscriptions: Vec<Subscription>,
}

pub enum ChatProviderEvent {
    SessionCreatedExternally(SolutionSessionId),
    /// A `persist_tab_order` mutation outside the local panel changed
    /// the set of sessions whose `tab_order IS NOT NULL` for
    /// `solution_id`. `ConsolePanel` reacts to add the tabs in `opened`
    /// (when they belong to its active solution) and close any local
    /// tab whose session id is in `closed`.
    ///
    /// Primary driver is the wire-side
    /// `workspace.{open,close}_session` RPCs from the mobile client;
    /// without this seam mobile-side strip changes only updated the
    /// `tab_order` field and the wire notification, leaving the
    /// desktop strip stale.
    TabsChanged {
        solution_id: SolutionId,
        opened: Vec<SolutionSessionId>,
        closed: Vec<SolutionSessionId>,
    },
    /// A session was removed from the store (destructive
    /// `solution_agent.delete_session`). `ConsolePanel` closes its tab
    /// if it had one.
    SessionRemoved(SolutionSessionId),
}

impl EventEmitter<ChatProviderEvent> for ChatProvider {}

impl ChatProvider {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        store: Entity<SolutionAgentStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&store, |this, _store, event, cx| {
            match event {
                SolutionAgentStoreEvent::SessionCreated { id, .. } => {
                    cx.emit(ChatProviderEvent::SessionCreatedExternally(*id));
                }
                SolutionAgentStoreEvent::TabsChanged {
                    solution_id,
                    opened,
                    closed,
                } => {
                    if opened.is_empty() && closed.is_empty() {
                        // Reorder-only — current `ConsolePanel` doesn't
                        // mirror the order itself (it preserves user
                        // arrangement); skip the emit so observers don't
                        // get woken up for nothing.
                        return;
                    }
                    cx.emit(ChatProviderEvent::TabsChanged {
                        solution_id: solution_id.clone(),
                        opened: opened.clone(),
                        closed: closed.clone(),
                    });
                }
                SolutionAgentStoreEvent::SessionClosed(id) => {
                    cx.emit(ChatProviderEvent::SessionRemoved(*id));
                }
                _ => {}
            }
            let _ = this;
        });
        Self {
            workspace,
            store,
            _subscriptions: vec![subscription],
        }
    }

    /// Create a new session under `solution_id` using `agent_id`, then build
    /// a view for it. Pass `None` for `cwd` to use the solution root.
    pub fn new_tab(
        &self,
        solution_id: SolutionId,
        agent_id: AgentServerId,
        cwd: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<(SolutionSessionId, Entity<SolutionSessionView>)>> {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        window.spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let project = cx.update(|_window, cx| {
                workspace
                    .upgrade()
                    .ok_or_else(|| anyhow!("workspace dropped"))
                    .map(|ws| ws.read(cx).project().clone())
            })??;

            let session_id = store
                .update(cx, |s, cx| {
                    s.create_session_with_cwd(solution_id, agent_id, project, cwd, None, None, cx)
                })
                .await?;

            let session: Entity<SolutionSession> = cx.update(|_window, cx| {
                store.read(cx).session(session_id).ok_or_else(|| {
                    anyhow!("session {session_id:?} missing from store immediately after creation")
                })
            })??;

            let view = workspace.update_in(cx, |_ws, window, cx| {
                cx.new(|cx| {
                    SolutionSessionView::new(session_id, session, workspace.clone(), window, cx)
                })
            })?;

            Ok((session_id, view))
        })
    }

    /// Look up an existing session by id and build a view for it (no new
    /// session is created). Used by `ConsolePanel::load` when restoring a
    /// saved tab.
    pub fn new_tab_from_existing(
        &self,
        session_id: SolutionSessionId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<SolutionSessionView>>> {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        window.spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let session: Entity<SolutionSession> = cx.update(|_window, cx| {
                store
                    .read(cx)
                    .session(session_id)
                    .ok_or_else(|| anyhow!("session {session_id:?} not in store"))
            })??;

            let view = workspace.update_in(cx, |_ws, window, cx| {
                cx.new(|cx| {
                    SolutionSessionView::new(session_id, session, workspace.clone(), window, cx)
                })
            })?;

            Ok(view)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use solution_agent::claude_adapter::CLAUDE_ACP_AGENT_ID;
    use solution_agent::store::SolutionAgentStore;
    use solution_agent::test_support::MockAgentServer;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use workspace::Workspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    /// Bootstrap: one Solution + Project + SolutionAgentStore with a MockAgentServer.
    /// Returns `(solution_id, _tmpdir, project)`. Hold `_tmpdir` for the test's lifetime.
    async fn setup(
        cx: &mut TestAppContext,
    ) -> (
        solutions::SolutionId,
        tempfile::TempDir,
        gpui::Entity<project::Project>,
    ) {
        init_test(cx);
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("solutions.json");
        let solutions_root = dir.path().join("solutions");
        std::fs::create_dir_all(&solutions_root).expect("solutions root");

        let store = cx.update(|cx| {
            let sol_store = solutions::SolutionStore::for_test(cfg_path, cx);
            solutions::install_global_for_test(sol_store.clone(), cx);
            sol_store
        });
        let solution_id = store
            .update(cx, |s, cx| {
                s.create_solution("Sol", solutions_root.clone(), cx)
            })
            .expect("create_solution");
        let solution_root: std::path::PathBuf = store.read_with(cx, |s, _| {
            s.solutions()
                .iter()
                .find(|s| s.id == solution_id)
                .map(|s| s.root.clone())
                .expect("solution exists")
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(solution_root.clone(), serde_json::json!({ ".keep": "" }))
            .await;
        let project = Project::test(fs, [solution_root.as_path()], cx).await;

        let connect_count = Arc::new(AtomicUsize::new(0));
        cx.update(|cx| {
            let registry = Arc::new(solution_agent::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
            let agent_store = SolutionAgentStore::global(cx);
            agent_store.update(cx, |s, _| {
                s.register_agent_server(
                    gpui::SharedString::from(CLAUDE_ACP_AGENT_ID),
                    Rc::new(MockAgentServer::new(connect_count)),
                );
            });
        });

        (solution_id, dir, project)
    }

    // Ignored: `SolutionSessionView::new` constructs an `editor::Editor`
    // internally, which requires a fully-initialised editor/language/theme
    // stack. The `MockAgentServer` is sufficient to avoid spawning a real
    // subprocess, but wiring up the editor stack in a unit test is
    // disproportionately complex for a provider shim. Revisit in B8 when
    // ConsolePanel drives chat creation via UI actions (an integration-level
    // test would initialise the full stack naturally).
    #[gpui::test]
    #[ignore]
    async fn new_tab_creates_session_and_view(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (solution_id, _tmp, project) = setup(cx).await;

        let store = cx.read(|cx| SolutionAgentStore::global(cx));

        let window_handle = cx.add_window(|window, cx| Workspace::test_new(project, window, cx));

        let task = window_handle
            .update(cx, |workspace, window, cx| {
                // Build a plain ChatProvider value (not an entity) so new_tab can
                // take (&self, &mut Window, &mut App) without a borrow conflict.
                // The subscription will not fire in this test — that's fine since
                // the test only exercises the session-creation + view-build path.
                let provider = ChatProvider {
                    workspace: workspace.weak_handle(),
                    store: store.clone(),
                    _subscriptions: vec![],
                };
                provider.new_tab(
                    solution_id,
                    gpui::SharedString::from(CLAUDE_ACP_AGENT_ID),
                    None,
                    window,
                    cx,
                )
            })
            .unwrap();

        let result = task.await;
        assert!(result.is_ok(), "new_tab should succeed: {:?}", result.err());
        let (session_id, _view) = result.unwrap();
        cx.read(|cx| {
            let store = SolutionAgentStore::global(cx);
            assert!(
                store.read(cx).session(session_id).is_some(),
                "session should be registered in store"
            );
        });
    }
}
