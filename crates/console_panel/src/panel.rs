use anyhow::{Result, anyhow};
use collections::HashMap;
use futures::channel::oneshot;
use futures::future::join_all;
use gpui::{
    Action, Anchor, App, AppContext as _, AsyncApp, AsyncWindowContext, Context, DismissEvent,
    Entity, EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton, MouseDownEvent, Pixels,
    Point, Render, Subscription, Task, WeakEntity, Window, anchored, deferred,
};
use project::Project;
use settings::Settings as _;
use solution_agent::SolutionSessionId;
use solution_agent::claude_adapter::CLAUDE_ACP_AGENT_ID;
use solution_agent::rename_session_modal::RenameSessionModal;
use solution_agent::reopen_session_modal::{ReopenSessionModal, ReopenableSession};
use solution_agent::session_view::SolutionSessionView;
use solution_agent::store::SolutionAgentStore;
use solutions::{SolutionId, SolutionStore};
use std::path::PathBuf;
use task::{RevealStrategy, RevealTarget, Shell, SpawnInTerminal, TaskId};
use terminal::Terminal;
use terminal_view::TerminalView;
use terminal_view::terminal_panel::prepare_task_for_spawn;
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Item, Workspace, WorkspaceDb,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::actions::{NewChat, ToggleFocus};
use crate::{ChatProvider, ChatProviderEvent, ConsolePanelSettings, TerminalProvider};

const CONSOLE_PANEL_KEY: &str = "ConsolePanel";

/// Resolve the active solution for a workspace by walking its worktrees and
/// matching against the global `SolutionStore`. Mirrors
/// `solutions_ui::window_helpers::active_solution_in_workspace` (kept local
/// here to avoid pulling `solutions_ui` as a dep for one helper). Callers
/// must hold the Workspace as a plain reference, NOT through `cx.read(...)`
/// on its `Entity<Workspace>` — re-reading the workspace while a
/// `workspace.register_action` handler holds `&mut Workspace` triggers
/// GPUI's double-lease panic.
pub fn active_solution_id_for_workspace(workspace: &Workspace, cx: &App) -> Option<SolutionId> {
    let store = SolutionStore::try_global(cx)?;
    let store = store.read(cx);
    let project = workspace.project().read(cx);
    for worktree in project.worktrees(cx) {
        let abs_path = worktree.read(cx).abs_path();
        if let Some(sol) = store.solution_for_path(abs_path.as_ref()) {
            return Some(sol.id.clone());
        }
    }
    None
}

/// Folder of the solution's *active* project — the one selected in the
/// project tab strip — falling back to the solution root when there is no
/// active member. Used as the `cwd` for new terminals / AI chats started
/// from the "+" menu (one project per solution drives both surfaces).
fn active_member_path(solution_id: &SolutionId, cx: &App) -> Option<PathBuf> {
    let store = SolutionStore::try_global(cx)?;
    let store = store.read(cx);
    let solution = store.solutions().iter().find(|s| &s.id == solution_id)?;
    if let Some(catalog) = store.active_member(solution_id)
        && let Some(member) = solution.members.iter().find(|m| &m.catalog_id == catalog)
    {
        return Some(member.local_path.clone());
    }
    Some(solution.root.clone())
}

/// Whether a tab whose working directory is `tab_cwd` belongs to the
/// project rooted at `active_member_path`. Mirrors `project_panel`'s
/// worktree filter (`abs_path().starts_with(active_member_path)`): a `None`
/// member path means "no active-member filter" so every tab is shown;
/// otherwise the tab is in scope iff its cwd lives inside the member root.
fn tab_cwd_in_scope(tab_cwd: Option<&std::path::Path>, active_member_path: Option<&std::path::Path>) -> bool {
    match active_member_path {
        None => true,
        Some(member) => tab_cwd.is_some_and(|cwd| cwd.starts_with(member)),
    }
}

/// Resolve which tab the panel should render as active given each tab's
/// in-scope flag and the stored `active_index`. The stored active tab wins
/// when it is in scope; otherwise the first in-scope tab is used; `None`
/// when no tab is in scope. Keeps the highlighted strip tab and the
/// rendered content in agreement even when the stored active tab belongs to
/// a different member than the one currently selected.
fn effective_active_index(in_scope: &[bool], active_index: Option<usize>) -> Option<usize> {
    if let Some(ix) = active_index
        && in_scope.get(ix).copied().unwrap_or(false)
    {
        return Some(ix);
    }
    in_scope.iter().position(|&visible| visible)
}

pub enum ConsoleTab {
    Terminal {
        view: Entity<TerminalView>,
    },
    Chat {
        view: Entity<SolutionSessionView>,
        session_id: SolutionSessionId,
    },
}

/// Stable per-tab identity used to remember the active tab for each member
/// project across active-member switches. Indices shift as tabs open/close,
/// so the per-member memory is keyed by content, not position.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ConsoleTabKey {
    Terminal(gpui::EntityId),
    Chat(SolutionSessionId),
}

/// Drag payload for reordering console tabs. The bespoke tab strip
/// doesn't use a `workspace::Pane` (whose tab bar gets DnD for free), so
/// the reorder affordance lost in the panel merge is re-implemented here
/// directly on the strip elements. Carries the source `ix` (consumed by
/// the drop target's [`ConsolePanel::reorder_tab`]) plus the icon/title
/// so the drag preview looks like the tab being dragged.
#[derive(Clone)]
struct DraggedConsoleTab {
    ix: usize,
    icon: IconName,
    title: SharedString,
}

impl Render for DraggedConsoleTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h_8()
            .items_center()
            .gap_1p5()
            .px_3()
            .bg(cx.theme().colors().tab_active_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(Icon::new(self.icon).size(IconSize::Small))
            .child(
                Label::new(self.title.clone())
                    .size(LabelSize::Default)
                    .line_height_style(LineHeightStyle::UiLabel),
            )
    }
}

pub struct ConsolePanel {
    workspace: WeakEntity<Workspace>,
    tabs: Vec<ConsoleTab>,
    active_index: Option<usize>,
    dock_position: DockPosition,
    terminal_provider: Entity<TerminalProvider>,
    chat_provider: Entity<ChatProvider>,
    focus_handle: FocusHandle,
    tab_context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    pending_terminals_to_add: usize,
    deferred_tasks: HashMap<TaskId, Task<()>>,
    assistant_enabled: bool,
    /// Session whose chat tab should be activated once it lands in the
    /// strip. Set by [`add_chat_tab_with_cwd`] when the local user creates
    /// a chat. Because chat tabs now have a single writer
    /// ([`apply_external_tab_changes`], driven by the store's
    /// create-implies-open pin), the creating code can't push-and-activate
    /// the tab directly — it records the id here and whichever of the two
    /// orderings wins (the tab landing vs. the create future resolving)
    /// performs the activation and clears this.
    chat_tab_to_activate: Option<SolutionSessionId>,
    /// Last-active tab per member project, so switching back to a member
    /// restores the exact dialog the user last had open there. In-memory
    /// only — on restart the panel falls back to each member's first tab.
    active_by_member: HashMap<PathBuf, ConsoleTabKey>,
    /// The member path the panel last rendered for; used to attribute the
    /// outgoing active tab to the correct member when the active member flips.
    last_member_path: Option<PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl ConsolePanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        store: Entity<SolutionAgentStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = ConsolePanelSettings::get_global(cx).clone();
        let terminal_provider = cx.new(|_| TerminalProvider::new(workspace.clone()));
        let chat_provider = cx.new(|cx| ChatProvider::new(workspace.clone(), store, cx));
        // Subscribe to external store mutations (mobile-side wire RPCs
        // driving the same store) so the desktop strip mirrors them in
        // real time. The handler filters by this panel's active
        // solution_id so a foreign-solution mutation doesn't open
        // ghost tabs in unrelated workspaces.
        let chat_event_sub = cx.subscribe_in(
            &chat_provider,
            window,
            |this, _provider, event, window, cx| match event {
                ChatProviderEvent::TabsChanged {
                    solution_id,
                    opened,
                    closed,
                } => this.apply_external_tab_changes(
                    solution_id.clone(),
                    opened.clone(),
                    closed.clone(),
                    window,
                    cx,
                ),
                ChatProviderEvent::SessionRemoved(id) => {
                    this.close_chat_tab_by_session_id(*id, cx);
                }
                ChatProviderEvent::SessionCreatedExternally(_) => {
                    // No-op: creates without an `open_session` follow-up
                    // don't pin the session in the strip; the user has to
                    // explicitly open it. Matches desktop's "new session"
                    // path, which calls open_session after create_session.
                }
            },
        );
        // Re-scope the visible tabs whenever the solution-wide active member
        // flips, so the strip + content swap to that project's own dialogs —
        // mirroring how Project Panel / Git Panel follow the active member.
        let member_change_sub = SolutionStore::try_global(cx).map(|store| {
            cx.subscribe(&store, |this, _store, event, cx| {
                if matches!(event, solutions::SolutionStoreEvent::ActiveMemberChanged { .. }) {
                    this.on_active_member_changed(cx);
                }
            })
        });
        let mut subscriptions = vec![chat_event_sub];
        subscriptions.extend(member_change_sub);
        Self {
            workspace,
            tabs: Vec::new(),
            active_index: None,
            dock_position: settings.default_position,
            terminal_provider,
            chat_provider,
            focus_handle: cx.focus_handle(),
            tab_context_menu: None,
            pending_terminals_to_add: 0,
            deferred_tasks: HashMap::default(),
            assistant_enabled: false,
            chat_tab_to_activate: None,
            active_by_member: HashMap::default(),
            last_member_path: None,
            _subscriptions: subscriptions,
        }
    }

    /// Loader. Constructs a fresh `ConsolePanel` and then restores any
    /// persisted tabs from the workspace DB. Terminal tabs are re-spawned at
    /// their stored CWD with a fresh shell (clean-start policy: state inside
    /// the shell is *not* restored). Chat tabs are reattached to existing
    /// sessions in `SolutionAgentStore`; rows whose session is no longer in
    /// the store are skipped with a warning.
    pub fn dock_position(&self) -> DockPosition {
        self.dock_position
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        // The store is only available once `SolutionAgentStore::init_global`
        // has run; in production that is guaranteed before any workspace
        // boots. Tests that don't init the store can't load the panel either,
        // which matches TerminalPanel's old behaviour for solution_agent.
        let store = workspace.update(&mut cx, |_, cx| SolutionAgentStore::global(cx))?;
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace.weak_handle(), store, window, cx))
        })?;

        // Restore persisted tabs in the BACKGROUND. `load` is awaited by
        // `initialize_panels` before the panel is added to the dock, so any
        // work done here delays the panel's dock icon AND its content from
        // appearing at all. `restore_from_db` hydrates each chat tab's
        // session transcript off disk — seconds of work for a busy
        // Solution — which used to leave the whole panel (icon included)
        // invisible until it finished. Detaching it lets `load` return
        // immediately: the empty panel + icon paint at once and tabs fill
        // in as their sessions hydrate. Best-effort: a restore failure must
        // not take the panel down, so errors are logged, not propagated.
        {
            let workspace = workspace.clone();
            let panel = panel.clone();
            cx.spawn(async move |cx: &mut AsyncWindowContext| {
                Self::restore_from_db(workspace, panel, cx).await.log_err();
            })
            .detach();
        }

        Ok(panel)
    }

    /// Reads persisted rows from the DB and re-spawns each tab on the panel.
    /// Split out from `load` so the error-propagation path stays linear and
    /// the caller can `.log_err()` a single future.
    async fn restore_from_db(
        workspace: WeakEntity<Workspace>,
        panel: Entity<Self>,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        let workspace_id = workspace
            .read_with(cx, |ws, _| ws.database_id())?
            .ok_or_else(|| anyhow!("workspace has no database_id; nothing to restore"))?;

        let rows = cx
            .update(|_, cx| WorkspaceDb::global(cx).console_panel_tabs(workspace_id))?
            .unwrap_or_else(|err| {
                log::warn!(
                    "ConsolePanel: failed to read console_panel_tabs(workspace_id={workspace_id:?}): {err:#}; \
                     starting with no restored tabs"
                );
                Vec::new()
            });

        if rows.is_empty() {
            return Ok(());
        }

        // If any persisted row is a chat tab, hydrate the active solution's
        // sessions from disk so `ChatProvider::new_tab_from_existing` can find
        // them. Without this the session lives in DB but not in the in-memory
        // store, so chat-tab restore silently skips with a "session no longer
        // exists" warning. The store filters out `closed_at != null` rows
        // internally, so explicitly-closed sessions still don't come back.
        //
        // We use the LAZY path (`hydrate_open_tabs_lazy`): it materialises
        // empty placeholder entities for every open chat tab fast, then loads
        // each transcript blob in the background — the active tab first
        // (passed as `priority` so it paints with content, not a spinner),
        // the rest detached. Only the placeholder pass is awaited here, so
        // `new_tab_from_existing` finds every session id while the heavy blob
        // loads are still in flight; tabs whose blob hasn't landed render a
        // loading spinner until it does.
        let has_chat_rows = rows.iter().any(|(_, kind, _, _, _)| kind == "chat");
        if has_chat_rows {
            let solution_id = workspace
                .read_with(cx, |ws, cx| active_solution_id_for_workspace(ws, cx))
                .ok()
                .flatten();
            if let Some(solution_id) = solution_id {
                // The active chat tab (active==1) is the one the user will see
                // first; prioritise loading its transcript so it doesn't flash
                // a spinner.
                let priority = rows
                    .iter()
                    .find(|(_, kind, _, _, active)| kind == "chat" && *active)
                    .and_then(|(_, _, item_id, _, _)| SolutionSessionId::parse(item_id).ok());
                let hydrate = cx.update(|_, cx| {
                    SolutionAgentStore::global(cx).update(cx, |store, cx| {
                        store.hydrate_open_tabs_lazy(solution_id, priority, cx)
                    })
                });
                if let Ok(task) = hydrate {
                    task.await.log_err();
                }
            }
        }

        let (terminal_provider, chat_provider): (Entity<TerminalProvider>, Entity<ChatProvider>) =
            panel.read_with(cx, |panel, _| {
                (panel.terminal_provider.clone(), panel.chat_provider.clone())
            });

        let mut active_index: Option<usize> = None;

        for (tab_index, kind, item_id, cwd, active) in rows {
            let spawned = match kind.as_str() {
                "terminal" => {
                    let cwd_path = cwd.as_ref().map(PathBuf::from);
                    let provider = terminal_provider.clone();
                    let task = cx.update(|window, cx| {
                        // `update` gives the closure `&mut TerminalProvider`,
                        // which sidesteps the `read(cx).method(cx)` borrow
                        // conflict on the outer `cx`.
                        provider.update(cx, |provider, cx| provider.new_tab(cwd_path, window, cx))
                    });
                    match task {
                        Ok(task) => match task.await {
                            Ok(view) => Some(ConsoleTab::Terminal { view }),
                            Err(err) => {
                                log::warn!(
                                    "ConsolePanel restore: terminal tab #{tab_index} at cwd={cwd:?} \
                                     failed to spawn: {err:#}; skipping row"
                                );
                                None
                            }
                        },
                        Err(err) => {
                            log::warn!(
                                "ConsolePanel restore: terminal tab #{tab_index} could not be \
                                 scheduled (window gone?): {err:#}; aborting restore"
                            );
                            break;
                        }
                    }
                }
                "chat" => {
                    let session_id = match SolutionSessionId::parse(&item_id) {
                        Ok(id) => id,
                        Err(err) => {
                            log::warn!(
                                "ConsolePanel restore: chat tab #{tab_index} has invalid item_id \
                                 {item_id:?}: {err:#}; skipping row"
                            );
                            continue;
                        }
                    };
                    // Skip rows whose session is no longer in the store
                    // before spending an entity construction on them.
                    let session_exists = cx
                        .update(|_, cx| {
                            SolutionAgentStore::global(cx)
                                .read(cx)
                                .session(session_id)
                                .is_some()
                        })
                        .unwrap_or(false);
                    if !session_exists {
                        log::warn!(
                            "ConsolePanel restore: chat tab #{tab_index} references session \
                             {session_id} that no longer exists; skipping row"
                        );
                        continue;
                    }
                    let provider = chat_provider.clone();
                    let task = cx.update(|window, cx| {
                        provider.update(cx, |provider, cx| {
                            provider.new_tab_from_existing(session_id, window, cx)
                        })
                    });
                    match task {
                        Ok(task) => match task.await {
                            Ok(view) => Some(ConsoleTab::Chat { view, session_id }),
                            Err(err) => {
                                log::warn!(
                                    "ConsolePanel restore: chat tab #{tab_index} session={session_id} \
                                     failed to reattach: {err:#}; skipping row"
                                );
                                None
                            }
                        },
                        Err(err) => {
                            log::warn!(
                                "ConsolePanel restore: chat tab #{tab_index} could not be \
                                 scheduled (window gone?): {err:#}; aborting restore"
                            );
                            break;
                        }
                    }
                }
                other => {
                    log::warn!(
                        "ConsolePanel restore: row #{tab_index} has unknown kind={other:?}; \
                         skipping (table CHECK constraint should make this impossible)"
                    );
                    None
                }
            };

            if let Some(tab) = spawned {
                let new_index = panel.update(cx, |panel, cx| {
                    panel.tabs.push(tab);
                    let new_index = panel.tabs.len() - 1;
                    cx.notify();
                    new_index
                });
                if active {
                    active_index = Some(new_index);
                }
            }
        }

        panel.update(cx, |panel, cx| {
            if let Some(ix) = active_index {
                panel.active_index = Some(ix);
            } else if !panel.tabs.is_empty() {
                // No row claimed active=1 (e.g. partial restore lost the
                // active row). Default to the last tab so the panel isn't
                // blank when the dock opens.
                panel.active_index = Some(panel.tabs.len() - 1);
            }
            cx.notify();
            // Reconcile SolutionSession.tab_order against the restored panel
            // strip. Without this, boot leaves two sources of truth: this
            // panel's persisted tabs vs. the tab_order column hydrated by
            // restore_open_tabs — they were free to diverge once a desktop
            // user added a tab in a previous run (only ConsolePanel persisted
            // the new tab; tab_order stayed pointing at the previous set).
            // Calling persist here at end of restore harmonises them.
            panel.persist(cx);
        });

        Ok(())
    }

    /// Snapshot the current tab list into `console_panel_state` AND reconcile
    /// the global `SolutionSession.tab_order` field so `workspace.snapshot`
    /// (the mobile WorkspaceScreen feed) returns the same set of chat tabs
    /// the user actually sees on the desktop strip. Without that reconciliation
    /// the two stores diverge: `console_panel_state` tracks every panel
    /// mutation, but `tab_order` was only being touched by the mobile-side
    /// `workspace.open_session` / `close_session` RPCs and by the boot-time
    /// `restore_open_tabs` DB hydration — so a desktop user opening a new
    /// console here left mobile seeing a stale set of sessions from whatever
    /// `tab_order` happened to be persisted last.
    fn persist(&self, cx: &mut Context<Self>) {
        // Reconcile tab_order on every persist (add / close / reorder / restore).
        // `persist_tab_order` already emits the seq-ed `workspace.session_opened`
        // / `workspace.session_closed` deltas, so the mobile client picks up
        // the new strip without a manual snapshot refresh.
        self.sync_chat_tab_order(cx);

        // Snapshot tab state synchronously — we only read TerminalView /
        // SolutionSession entities here, never the Workspace. Workspace lookup
        // for `database_id` is deferred into the spawned task below so this
        // method is safe to call while a `Workspace::update` is in flight on
        // the outer borrow stack (action handlers, modal close paths, …).
        let active_index = self.active_index;
        let rows: Vec<(i64, String, String, Option<String>, bool)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let (kind, item_id, cwd) = match tab {
                    ConsoleTab::Terminal { view } => {
                        let cwd = view
                            .read(cx)
                            .terminal()
                            .read(cx)
                            .working_directory()
                            .map(|p| p.to_string_lossy().into_owned());
                        // For terminal rows the `item_id` is informational;
                        // restore only consults `cwd`. We use the cwd string
                        // (or an empty marker) so the column stays
                        // human-readable in the DB.
                        let item_id = cwd.clone().unwrap_or_default();
                        ("terminal".to_string(), item_id, cwd)
                    }
                    ConsoleTab::Chat { session_id, .. } => {
                        ("chat".to_string(), session_id.to_string(), None)
                    }
                };
                (ix as i64, kind, item_id, cwd, active_index == Some(ix))
            })
            .collect();

        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            let lookup = cx.update(|cx| {
                let workspace = workspace.upgrade()?;
                let workspace_id = workspace.read(cx).database_id()?;
                Some((WorkspaceDb::global(cx), workspace_id))
            });
            let Some((db, workspace_id)) = lookup else {
                return;
            };
            db.save_console_panel_tabs(workspace_id, rows)
                .await
                .log_err();
        })
        .detach();
    }

    /// Project the in-memory chat-tab order onto `SolutionSession.tab_order`
    /// per solution. Terminal tabs are ignored — only chat tabs map onto
    /// `solution_agent` sessions. Called from [`persist`] so every tab
    /// mutation (add / close / reorder / restore) keeps the field aligned
    /// with what the panel actually shows.
    fn sync_chat_tab_order(&self, cx: &mut Context<Self>) {
        let Some(store) = SolutionAgentStore::try_global(cx) else {
            return;
        };
        let chat_ids: Vec<SolutionSessionId> = self
            .tabs
            .iter()
            .filter_map(|tab| match tab {
                ConsoleTab::Chat { session_id, .. } => Some(*session_id),
                ConsoleTab::Terminal { .. } => None,
            })
            .collect();

        // Bucket chat session ids by solution_id, preserving tab-strip order
        // within each bucket. The workspace is typically a single Solution but
        // the model doesn't enforce that — group defensively so a cross-
        // solution panel layout (rare / future) doesn't silently truncate
        // any solution's tab strip to the first one we encounter.
        //
        // `persist_tab_order` clears tab_order on every session of the given
        // solution that isn't in `ordered_ids`. Solutions absent from the
        // panel are intentionally NOT touched — those tabs live elsewhere
        // (other workspaces / future mobile-only strips) and we don't want
        // to clobber their state.
        let mut per_solution: std::collections::HashMap<SolutionId, Vec<SolutionSessionId>> =
            std::collections::HashMap::new();
        {
            let store_ref = store.read(cx);
            for session_id in &chat_ids {
                let Some(entity) = store_ref.session(*session_id) else {
                    continue;
                };
                let solution_id = entity.read(cx).solution_id.clone();
                per_solution
                    .entry(solution_id)
                    .or_default()
                    .push(*session_id);
            }
        }

        if per_solution.is_empty() {
            return;
        }
        store.update(cx, |store, cx| {
            for (solution_id, ordered_ids) in per_solution {
                store.persist_tab_order(solution_id, ordered_ids, cx);
            }
        });
    }
}

impl EventEmitter<PanelEvent> for ConsolePanel {}

impl Focusable for ConsolePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConsolePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep the per-member active-tab memory current from this safe
        // (un-leased) context: record which tab is active for the member we
        // are about to render, so `on_active_member_changed` can stash it
        // under the right member when the active member next flips.
        let member_path = self.active_member_path(cx);
        let scope_flags = self.tab_scope_flags(cx);
        if let Some(path) = member_path.clone()
            && let Some(ix) = effective_active_index(&scope_flags, self.active_index)
            && let Some(tab) = self.tabs.get(ix)
        {
            self.active_by_member.insert(path, Self::tab_key(tab));
        }
        self.last_member_path = member_path;

        let menu_overlay = self.tab_context_menu.as_ref().map(|(menu, position, _)| {
            deferred(
                anchored()
                    .position(*position)
                    .anchor(Anchor::TopLeft)
                    .child(menu.clone()),
            )
            .with_priority(1)
        });
        v_flex()
            .size_full()
            .key_context("ConsolePanel")
            .track_focus(&self.focus_handle)
            .child(self.render_tab_strip(window, cx))
            .child(self.render_active_tab(window, cx))
            .children(menu_overlay)
    }
}

impl ConsolePanel {
    fn render_tab_strip(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scope_flags = self.tab_scope_flags(cx);
        let active = effective_active_index(&scope_flags, self.active_index);
        let mut strip = div()
            .id("console-tab-strip")
            .flex()
            .flex_none()
            .items_stretch()
            .h_9()
            .bg(cx.theme().colors().tab_bar_background)
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .overflow_x_scroll();
        for (ix, tab) in self.tabs.iter().enumerate() {
            // Only render tabs belonging to the active member project; the
            // rest stay live in `self.tabs` (absolute indices keep
            // activate/close/reorder valid) but are hidden until their
            // member is selected.
            if !scope_flags.get(ix).copied().unwrap_or(true) {
                continue;
            }
            let (icon, title): (IconName, SharedString) = match tab {
                ConsoleTab::Terminal { view } => {
                    (IconName::Terminal, view.read(cx).tab_content_text(0, cx))
                }
                ConsoleTab::Chat {
                    view: _,
                    session_id,
                } => {
                    let title = SolutionAgentStore::global(cx)
                        .read_with(cx, |s, _| s.session(*session_id))
                        .map(|entity| entity.read(cx).title.clone())
                        .unwrap_or_else(|| SharedString::from(session_id.to_string()));
                    (IconName::Sparkle, title)
                }
            };
            let is_active = active == Some(ix);
            let bg = if is_active {
                cx.theme().colors().tab_active_background
            } else {
                cx.theme().colors().tab_inactive_background
            };
            let tab_el = div()
                .id(("console-tab", ix))
                .flex()
                .flex_none()
                .items_center()
                .h_full()
                .gap_1p5()
                .px_3()
                .min_w(gpui::px(140.0))
                .max_w(gpui::px(220.0))
                .bg(bg)
                .border_r_1()
                .border_color(cx.theme().colors().border_variant)
                .child(Icon::new(icon).size(IconSize::Small))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .h_full()
                        .child(
                            // NB: no `LineHeightStyle::UiLabel` here. UiLabel
                            // pins line-height to 1.0×font-size (no leading),
                            // and `.truncate()` adds `overflow: hidden`, so
                            // descenders (g, y, …) got clipped at the tab's
                            // bottom edge. The default line-height leaves room.
                            Label::new(title.clone())
                                .size(LabelSize::Default)
                                .truncate(),
                        ),
                )
                .child(
                    IconButton::new(("console-close", ix), IconName::Close)
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener(move |this, _, _, cx| this.close_tab_at(ix, cx))),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.activate_tab(ix, cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        let position = ev.position;
                        this.show_tab_context_menu(ix, position, window, cx);
                    }),
                )
                // Drag-and-drop reorder (restored from the pre-merge
                // Pane-backed tab bar). `on_drag` starts the gesture past
                // GPUI's movement threshold, so the left-click activate
                // above still fires for a plain click.
                .on_drag(
                    DraggedConsoleTab {
                        ix,
                        icon,
                        title: title.clone(),
                    },
                    |dragged, _offset, _window, cx| cx.new(|_| dragged.clone()),
                )
                .drag_over::<DraggedConsoleTab>(|style, _dragged, _window, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(
                    cx.listener(move |this, dragged: &DraggedConsoleTab, _window, cx| {
                        this.reorder_tab(dragged.ix, ix, cx);
                    }),
                );
            strip = strip.child(tab_el);
        }
        strip.child(self.render_plus_popover(cx))
    }

    fn render_plus_popover(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_solution_id = self.active_solution_id(cx);
        let has_active_solution = active_solution_id.is_some();
        // New terminals and new chats both open in the active project's
        // folder (the project selected in the project tab strip). Model and
        // effort are no longer chosen here — they're picked in the status bar
        // after the chat is created, before the first message is sent.
        let active_path = active_solution_id
            .as_ref()
            .and_then(|id| active_member_path(id, cx));
        // Read the project handle here, in render context, where nothing is
        // leased — `add_chat_tab_with_cwd` no longer reads it from the
        // Workspace entity (see its doc comment: the action path holds the
        // Workspace mutably).
        let project = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().clone());
        let weak_self = cx.weak_entity();

        let plus_container = div()
            .flex()
            .flex_none()
            .items_center()
            .h_full()
            .px_1p5()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant);

        plus_container.child(
            PopoverMenu::new("console-panel-plus")
                .trigger_with_tooltip(
                    IconButton::new("console-plus", IconName::Plus).icon_size(IconSize::Small),
                    Tooltip::text("New…"),
                )
                .anchor(Anchor::TopLeft)
                .menu(move |window, cx| {
                    let active_solution_id = active_solution_id.clone();
                    let active_path = active_path.clone();
                    let project = project.clone();
                    let weak_self = weak_self.clone();
                    Some(ContextMenu::build(window, cx, move |menu, _, _| {
                        // New Terminal in the active project's folder (falls
                        // back to terminal settings when there's no active
                        // solution, i.e. `active_path` is `None`).
                        let menu = {
                            let weak_self = weak_self.clone();
                            let cwd = active_path.clone();
                            menu.entry("New Terminal", None, move |window, cx| {
                                if let Some(panel) = weak_self.upgrade() {
                                    panel.update(cx, |panel, cx| {
                                        panel.add_terminal_tab(cwd.clone(), window, cx);
                                    });
                                }
                            })
                        };
                        // New AI Chat in the active project's folder.
                        let menu = if let (Some(solution_id), Some(project)) =
                            (active_solution_id.clone(), project.clone())
                        {
                            let weak_self = weak_self.clone();
                            let cwd = active_path.clone();
                            menu.entry("New AI Chat", None, move |window, cx| {
                                if let Some(panel) = weak_self.upgrade() {
                                    panel.update(cx, |panel, cx| {
                                        panel.add_chat_tab_with_cwd(
                                            solution_id.clone(),
                                            project.clone(),
                                            cwd.clone(),
                                            window,
                                            cx,
                                        );
                                    });
                                }
                            })
                        } else {
                            menu.action_disabled_when(
                                true,
                                "New AI Chat (no active solution)",
                                NewChat.boxed_clone(),
                            )
                        };
                        // Reopen a chat that was closed but still lives on
                        // disk. Disabled when there's no active solution.
                        let menu = {
                            let weak_self = weak_self.clone();
                            menu.item(
                                ui::ContextMenuEntry::new("Reopen Closed Chat…")
                                    .disabled(!has_active_solution)
                                    .handler(move |window, cx| {
                                        if let Some(panel) = weak_self.upgrade() {
                                            panel.update(cx, |panel, cx| {
                                                panel.open_reopen_session_modal(window, cx);
                                            });
                                        }
                                    }),
                            )
                        };
                        menu.separator()
                            .action("Spawn Task…", zed_actions::Spawn::modal().boxed_clone())
                    }))
                }),
        )
    }

    fn active_solution_id(&self, cx: &App) -> Option<SolutionId> {
        let workspace = self.workspace.upgrade()?;
        let workspace = workspace.read(cx);
        active_solution_id_for_workspace(workspace, cx)
    }

    /// Root path of the panel's active member project — the project selected
    /// in the project tab strip. `None` when no solution hosts the panel's
    /// worktrees or no active member is recorded, in which case the panel
    /// shows every tab (no per-member filter). Mirrors
    /// `project_panel::ProjectPanel::active_member_path`.
    fn active_member_path(&self, cx: &App) -> Option<PathBuf> {
        let solution_id = self.active_solution_id(cx)?;
        active_member_path(&solution_id, cx)
    }

    /// Working directory a tab is anchored to, used to decide which member
    /// project owns it. Chat tabs use the session's immutable `cwd` (set to
    /// the active member path at creation, the same value `claude-acp` keys
    /// its transcript bucket on); terminal tabs use their live working
    /// directory.
    fn tab_cwd(&self, tab: &ConsoleTab, cx: &App) -> Option<PathBuf> {
        match tab {
            ConsoleTab::Terminal { view } => {
                view.read(cx).terminal().read(cx).working_directory()
            }
            ConsoleTab::Chat { session_id, .. } => SolutionAgentStore::try_global(cx)
                .and_then(|store| store.read(cx).session(*session_id))
                .map(|session| session.read(cx).cwd.clone()),
        }
    }

    /// Per-tab in-scope flags for the currently active member, in tab order.
    fn tab_scope_flags(&self, cx: &App) -> Vec<bool> {
        let member_path = self.active_member_path(cx);
        self.tabs
            .iter()
            .map(|tab| {
                tab_cwd_in_scope(self.tab_cwd(tab, cx).as_deref(), member_path.as_deref())
            })
            .collect()
    }

    /// Stable identity for a tab, used to remember the active tab per member
    /// across active-member switches.
    fn tab_key(tab: &ConsoleTab) -> ConsoleTabKey {
        match tab {
            ConsoleTab::Terminal { view } => ConsoleTabKey::Terminal(view.entity_id()),
            ConsoleTab::Chat { session_id, .. } => ConsoleTabKey::Chat(*session_id),
        }
    }

    /// Re-resolve `active_index` for the now-active member: remember the tab
    /// that was active for the previous member, then restore the new
    /// member's last-active tab (if it is still present and in scope),
    /// falling back to its first in-scope tab. Called when the solution-wide
    /// active member flips so the strip and content swap to that project's
    /// own dialogs.
    fn on_active_member_changed(&mut self, cx: &mut Context<Self>) {
        // Stash the outgoing member's active tab so switching back restores
        // the exact dialog the user last had open.
        if let Some(prev) = self.last_member_path.take()
            && let Some(ix) = self.active_index
            && let Some(tab) = self.tabs.get(ix)
        {
            self.active_by_member
                .insert(prev, Self::tab_key(tab));
        }

        let member_path = self.active_member_path(cx);
        let flags = self.tab_scope_flags(cx);

        let remembered = member_path
            .as_ref()
            .and_then(|path| self.active_by_member.get(path).copied());
        let remembered_ix = remembered.and_then(|key| {
            self.tabs.iter().position(|tab| Self::tab_key(tab) == key)
        });

        self.active_index = match remembered_ix {
            Some(ix) if flags.get(ix).copied().unwrap_or(false) => Some(ix),
            _ => effective_active_index(&flags, self.active_index),
        };
        self.last_member_path = member_path;
        cx.notify();
    }

    pub fn add_terminal_tab(
        &mut self,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = self
            .terminal_provider
            .update(cx, |provider, cx| provider.new_tab(cwd, window, cx));
        cx.spawn(async move |this, cx| {
            let view = task.await?;
            this.update(cx, |this, cx| {
                this.tabs.push(ConsoleTab::Terminal { view });
                this.active_index = Some(this.tabs.len() - 1);
                cx.notify();
                this.persist(cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Handler for `workspace::NewTerminal`. Decides whether to add a terminal
    /// to the workspace's center pane (when the center is already showing a
    /// terminal) or to the ConsolePanel itself. Mirrors `TerminalPanel::new_terminal`.
    pub fn handle_new_terminal(
        workspace: &mut Workspace,
        action: &workspace::NewTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let center_pane = workspace.active_pane();
        let center_pane_has_focus = center_pane.focus_handle(cx).contains_focused(window, cx);
        let active_center_item_is_terminal = center_pane
            .read(cx)
            .active_item()
            .is_some_and(|item| item.downcast::<TerminalView>().is_some());

        if center_pane_has_focus && active_center_item_is_terminal {
            let working_directory = terminal_view::default_working_directory(workspace, cx);
            let local = action.local;
            terminal_view::terminal_panel::TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
                if local {
                    project.create_local_terminal(cx)
                } else {
                    project.create_terminal_shell(working_directory, cx)
                }
            })
            .detach_and_log_err(cx);
            return;
        }

        let Some(console_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        let working_directory = terminal_view::default_working_directory(workspace, cx);
        console_panel.update(cx, |panel, cx| {
            panel.add_terminal_tab(working_directory, window, cx);
        });
    }

    /// Spawn a task into a fresh terminal tab. Used both as the public entry
    /// point for `RevealTarget::Dock` task runs and as the new-tab branch of
    /// `spawn_task` below.
    pub fn add_terminal_task(
        &mut self,
        task: SpawnInTerminal,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let workspace = self.workspace.clone();
        self.pending_terminals_to_add += 1;
        cx.spawn_in(window, async move |this, cx| {
            let project = workspace.read_with(cx, |workspace, cx| {
                if !workspace.project().read(cx).supports_terminal(cx) {
                    Err(anyhow!("terminal not yet supported for remote projects"))
                } else {
                    Ok(workspace.project().clone())
                }
            })??;
            let terminal = project
                .update(cx, |project, cx| project.create_terminal_task(task, cx))
                .await?;
            let terminal_view = workspace.update_in(cx, |workspace, window, cx| {
                let view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.weak_handle(),
                        workspace.database_id(),
                        workspace.project().downgrade(),
                        window,
                        cx,
                    )
                });
                match reveal_strategy {
                    RevealStrategy::Always => {
                        workspace.focus_panel::<Self>(window, cx);
                    }
                    RevealStrategy::NoFocus => {
                        workspace.open_panel::<Self>(window, cx);
                    }
                    RevealStrategy::Never => {}
                }
                view
            })?;
            this.update(cx, |this, cx| {
                this.tabs.push(ConsoleTab::Terminal {
                    view: terminal_view,
                });
                this.active_index = Some(this.tabs.len() - 1);
                this.pending_terminals_to_add = this.pending_terminals_to_add.saturating_sub(1);
                cx.notify();
                this.persist(cx);
            })?;
            Ok(terminal.downgrade())
        })
    }

    /// Spawn or rerun a task. Mirrors `TerminalPanel::spawn_task` but uses
    /// `self.tabs` as the registry of existing terminals instead of a Pane.
    pub fn spawn_task(
        &mut self,
        task: &SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(Err(anyhow!("failed to read workspace")));
        };

        let project = workspace.read(cx).project().read(cx);

        if project.is_via_collab() {
            return Task::ready(Err(anyhow!("cannot spawn tasks as a guest")));
        }

        let remote_client = project.remote_client();
        let is_windows = project.path_style(cx).is_windows();
        let remote_shell = remote_client
            .as_ref()
            .and_then(|remote_client| remote_client.read(cx).shell());

        let shell = if let Some(remote_shell) = remote_shell
            && task.shell == Shell::System
        {
            Shell::Program(remote_shell)
        } else {
            task.shell.clone()
        };

        let task = prepare_task_for_spawn(task, &shell, is_windows);

        if task.allow_concurrent_runs && task.use_new_terminal {
            return self.spawn_in_new_terminal(task, window, cx);
        }

        let mut terminals_for_task = self.terminals_for_task(&task.full_label, cx);
        let Some(existing) = terminals_for_task.pop() else {
            return self.spawn_in_new_terminal(task, window, cx);
        };

        let (existing_tab_index, existing_terminal_view) = existing;
        if task.allow_concurrent_runs {
            return self.replace_terminal(
                task,
                existing_tab_index,
                existing_terminal_view,
                window,
                cx,
            );
        }

        let (tx, rx) = oneshot::channel::<Result<WeakEntity<Terminal>>>();

        self.deferred_tasks.insert(
            task.id.clone(),
            cx.spawn_in(window, async move |console_panel, cx| {
                wait_for_terminals_tasks(terminals_for_task, cx).await;
                let new_task = console_panel.update_in(cx, |console_panel, window, cx| {
                    if task.use_new_terminal {
                        console_panel.spawn_in_new_terminal(task, window, cx)
                    } else {
                        console_panel.replace_terminal(
                            task,
                            existing_tab_index,
                            existing_terminal_view,
                            window,
                            cx,
                        )
                    }
                });
                if let Ok(new_task) = new_task {
                    tx.send(new_task.await).ok();
                }
            }),
        );

        cx.spawn(async move |_, _| rx.await?)
    }

    fn spawn_in_new_terminal(
        &mut self,
        spawn_task: SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let reveal = spawn_task.reveal;
        let reveal_target = spawn_task.reveal_target;
        match reveal_target {
            RevealTarget::Center => self
                .workspace
                .update(cx, |workspace, cx| {
                    terminal_view::terminal_panel::TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_task(spawn_task, cx)
                    })
                })
                .unwrap_or_else(|e| Task::ready(Err(e))),
            RevealTarget::Dock => self.add_terminal_task(spawn_task, reveal, window, cx),
        }
    }

    fn replace_terminal(
        &self,
        spawn_task: SpawnInTerminal,
        existing_tab_index: usize,
        terminal_to_replace: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let reveal = spawn_task.reveal;
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let new_terminal = project
                .update(cx, |project, cx| {
                    project.create_terminal_task(spawn_task, cx)
                })
                .await?;
            terminal_to_replace.update_in(cx, |terminal_to_replace, window, cx| {
                terminal_to_replace.set_terminal(new_terminal.clone(), window, cx);
            })?;

            match reveal {
                RevealStrategy::Always => {
                    this.update_in(cx, |this, window, cx| {
                        this.activate_tab(existing_tab_index, cx);
                        if let Some(workspace) = this.workspace.upgrade() {
                            workspace.update(cx, |workspace, cx| {
                                workspace.focus_panel::<Self>(window, cx);
                            });
                        }
                    })?;
                }
                RevealStrategy::NoFocus => {
                    this.update_in(cx, |this, window, cx| {
                        this.activate_tab(existing_tab_index, cx);
                        if let Some(workspace) = this.workspace.upgrade() {
                            workspace.update(cx, |workspace, cx| {
                                workspace.open_panel::<Self>(window, cx);
                            });
                        }
                    })?;
                }
                RevealStrategy::Never => {}
            }

            Ok(new_terminal.downgrade())
        })
    }

    fn terminals_for_task(&self, label: &str, cx: &App) -> Vec<(usize, Entity<TerminalView>)> {
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| match tab {
                ConsoleTab::Terminal { view } => {
                    let task_state = view.read(cx).terminal().read(cx).task()?;
                    if task_state.spawned_task.full_label == label {
                        Some((index, view.clone()))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect()
    }

    /// Mirrors `TerminalPanel::terminal_selections`: the non-empty selection
    /// text of every terminal tab.
    pub fn terminal_selections(&self, cx: &App) -> Vec<String> {
        self.tabs
            .iter()
            .filter_map(|tab| match tab {
                ConsoleTab::Terminal { view } => view
                    .read(cx)
                    .terminal()
                    .read(cx)
                    .last_content
                    .selection_text
                    .clone()
                    .filter(|text| !text.is_empty()),
                _ => None,
            })
            .collect()
    }

    /// The currently-active terminal tab's view, if any.
    pub fn active_terminal_view(&self, _cx: &App) -> Option<Entity<TerminalView>> {
        let ix = self.active_index?;
        match self.tabs.get(ix)? {
            ConsoleTab::Terminal { view } => Some(view.clone()),
            _ => None,
        }
    }

    pub fn assistant_enabled(&self) -> bool {
        self.assistant_enabled
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn set_assistant_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.assistant_enabled != enabled {
            self.assistant_enabled = enabled;
            cx.notify();
        }
    }

    pub fn add_chat_tab(
        &mut self,
        solution_id: SolutionId,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Scope the new chat to the active project's folder, exactly like the
        // "+" → "New AI Chat" menu entry. Passing `None` here would default
        // the session cwd to the solution root, so the keyboard `NewChat`
        // action (and any other caller of this convenience wrapper) would land
        // the agent in "ROOT" instead of the selected project.
        let cwd = active_member_path(&solution_id, cx);
        self.add_chat_tab_with_cwd(solution_id, project, cwd, window, cx);
    }

    /// Create a chat session (create-implies-open) under `solution_id` rooted
    /// at `cwd`. `project` is passed in rather than read from `self.workspace`
    /// because callers may already hold the `Workspace` mutably (the `NewChat`
    /// workspace-action handler does): re-leasing the `Workspace` entity here
    /// via `workspace.read(cx)` would `double_lease_panic`. Callers that own a
    /// `&mut Workspace` pass `workspace.project().clone()`; the render-time
    /// menu path reads it while nothing is leased.
    pub fn add_chat_tab_with_cwd(
        &mut self,
        solution_id: SolutionId,
        project: Entity<Project>,
        cwd: Option<PathBuf>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Create-implies-open: `create_session_with_cwd` pins the new
        // session into the strip itself (sets tab_order + emits the
        // `TabsChanged` fan-out), so the actual tab is built and pushed by
        // [`apply_external_tab_changes`] — the single chat-tab writer. We
        // only need to remember which session to activate once its tab
        // lands. This is the same path a mobile-driven create takes, so the
        // two surfaces can't diverge.
        // Model and effort are chosen in the status bar after the chat is
        // created (and applied before the first message starts the session),
        // so the create call no longer carries them.
        let store = SolutionAgentStore::global(cx);
        let task = store.update(cx, |store, cx| {
            store.create_session_with_cwd(
                solution_id,
                SharedString::from(CLAUDE_ACP_AGENT_ID),
                project,
                cwd,
                None,
                None,
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let session_id = task.await?;
            this.update(cx, |this, cx| {
                this.chat_tab_to_activate = Some(session_id);
                // The tab may already have landed (the create-time pin's
                // `TabsChanged` can fire before this future resolves) — if
                // so, activate it now; otherwise the add path activates it.
                this.activate_chat_tab_if_present(session_id, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// If a chat tab for `session_id` is present in the strip, make it the
    /// active tab (and clear [`chat_tab_to_activate`] if it was the pending
    /// one). No-op when the tab hasn't landed yet.
    fn activate_chat_tab_if_present(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tabs.iter().position(
            |tab| matches!(tab, ConsoleTab::Chat { session_id: sid, .. } if *sid == session_id),
        ) else {
            return;
        };
        self.active_index = Some(ix);
        if self.chat_tab_to_activate == Some(session_id) {
            self.chat_tab_to_activate = None;
        }
        cx.notify();
        self.persist(cx);
    }

    /// Ensure a chat tab exists for `session_id` and make it the active tab.
    /// Backs the [`crate::ShowSession`] action (MCP-driven UI verification).
    ///
    /// Unlike [`add_chat_tab_with_cwd`](Self::add_chat_tab_with_cwd), which
    /// leans on the create-time `TabsChanged` fan-out to build the tab, this
    /// spawns the view directly. That matters because a session pinned
    /// out-of-band (e.g. the `workspace.open_session` RPC) may never produce
    /// a desktop tab through the event path, so relying on it would leave the
    /// session unreachable from the strip. Spawning here is idempotent: the
    /// `has_chat_tab_for` guard (both before the await and inside it) dedupes
    /// against a tab a parallel handler may have added meanwhile.
    pub fn show_session(
        &mut self,
        session_id: SolutionSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_chat_tab_for(session_id) {
            self.activate_chat_tab_if_present(session_id, cx);
            return;
        }
        let task = self.chat_provider.update(cx, |provider, cx| {
            provider.new_tab_from_existing(session_id, window, cx)
        });
        cx.spawn(async move |this, cx| {
            let view = task.await.log_err()?;
            this.update(cx, |this, cx| {
                if this.has_chat_tab_for(session_id) {
                    this.activate_chat_tab_if_present(session_id, cx);
                    return;
                }
                this.tabs.push(ConsoleTab::Chat { view, session_id });
                this.active_index = Some(this.tabs.len() - 1);
                cx.notify();
                this.persist(cx);
            })
            .log_err();
            Some(())
        })
        .detach();
    }

    fn show_tab_context_menu(
        &mut self,
        tab_index: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(tab_index) else {
            return;
        };
        let weak = cx.weak_entity();
        let menu = match tab {
            ConsoleTab::Terminal { view } => {
                let view = view.clone();
                ContextMenu::build(window, cx, |menu, _, _| {
                    let weak_close = weak.clone();
                    let weak_rename = weak.clone();
                    let weak_reveal = weak.clone();
                    let view_rename = view.clone();
                    let view_reveal = view;
                    menu.entry("Close", None, move |_, cx| {
                        if let Some(this) = weak_close.upgrade() {
                            this.update(cx, |this, cx| this.close_tab(tab_index, cx));
                        }
                    })
                    .entry("Rename Tab", None, move |window, cx| {
                        if let Some(this) = weak_rename.upgrade() {
                            this.update(cx, |_, cx| {
                                view_rename.update(cx, |view, cx| {
                                    view.rename_terminal(
                                        &terminal_view::RenameTerminal,
                                        window,
                                        cx,
                                    );
                                });
                            });
                        }
                    })
                    .entry(
                        "Reveal CWD in Project Panel",
                        None,
                        move |window, cx| {
                            if let Some(this) = weak_reveal.upgrade() {
                                this.update(cx, |this, cx| {
                                    this.reveal_terminal_cwd(&view_reveal, window, cx);
                                });
                            }
                        },
                    )
                })
            }
            ConsoleTab::Chat { session_id, .. } => {
                let session_id = *session_id;
                ContextMenu::build(window, cx, |menu, _, _| {
                    let weak_close = weak.clone();
                    let weak_rename = weak.clone();
                    let weak_restart = weak.clone();
                    menu.entry("Close", None, move |_, cx| {
                        if let Some(this) = weak_close.upgrade() {
                            this.update(cx, |this, cx| this.close_tab(tab_index, cx));
                        }
                    })
                    .entry("Rename Session", None, move |window, cx| {
                        if let Some(this) = weak_rename.upgrade() {
                            this.update(cx, |this, cx| {
                                this.open_rename_session_modal(session_id, window, cx);
                            });
                        }
                    })
                    .entry("Restart Agent", None, move |_, cx| {
                        if let Some(this) = weak_restart.upgrade() {
                            this.update(cx, |_, cx| {
                                let store = SolutionAgentStore::global(cx);
                                store
                                    .update(cx, |store, cx| store.restart_agent(session_id, cx))
                                    .detach_and_log_err(cx);
                            });
                        }
                    })
                })
            }
        };
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.tab_context_menu.take();
            cx.notify();
        });
        window.focus(&menu.focus_handle(cx), cx);
        self.tab_context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn reveal_terminal_cwd(
        &self,
        view: &Entity<TerminalView>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(cwd) = view.read(cx).terminal().read(cx).working_directory() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let Some((worktree, rel_path)) = project.read(cx).find_worktree(&cwd, cx) else {
            return;
        };
        let Some(entry_id) = worktree.read(cx).entry_for_path(&rel_path).map(|e| e.id) else {
            return;
        };
        project.update(cx, |_project, cx| {
            cx.emit(project::Event::RevealInProjectPanel(entry_id));
        });
    }

    fn open_rename_session_modal(
        &self,
        session_id: SolutionSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let current_title = SolutionAgentStore::global(cx)
            .read_with(cx, |s, _| s.session(session_id))
            .map(|entity| entity.read(cx).title.to_string())
            .unwrap_or_default();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                RenameSessionModal::new(session_id, current_title, window, cx)
            });
        });
    }

    /// Reopen-a-closed-chat flow. Hydrates the active solution's
    /// on-disk sessions, gathers the top-level ones that aren't currently
    /// pinned in the strip (closed tabs whose transcript survives), and
    /// opens a picker. Selecting a session re-pins it via
    /// `SolutionAgentStore::open_session_in_strip` — the same "open" path
    /// create and the wire RPC use — so the tab lands through the normal
    /// `TabsChanged` writer.
    fn open_reopen_session_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(solution_id) = self.active_solution_id(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        // Closed sessions live only on disk (close_session evicts them from
        // memory), so the picker reads them straight from the DB. The query
        // already returns top-level closed rows ordered most-recently-active
        // first, each carrying the token total + last-activity time the rows
        // display.
        let store = SolutionAgentStore::global(cx);
        let closed = store.update(cx, |store, cx| {
            store.list_closed_sessions(solution_id.clone(), cx)
        });
        cx.spawn_in(window, async move |_this, cx| {
            let metas = closed.await.log_err().unwrap_or_default();
            let sessions: Vec<ReopenableSession> =
                metas.iter().map(ReopenableSession::from_metadata).collect();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        ReopenSessionModal::new(sessions, window, cx)
                    });
                })
                .log_err();
        })
        .detach();
    }

    fn render_active_tab(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let scope_flags = self.tab_scope_flags(cx);
        let Some(ix) = effective_active_index(&scope_flags, self.active_index) else {
            return div().flex_1().min_h_0().into_any_element();
        };
        match &self.tabs[ix] {
            ConsoleTab::Terminal { view } => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            ConsoleTab::Chat { view, .. } => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
        }
    }

    fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        // Per-member active-tab memory is recorded in `render` / handled in
        // `on_active_member_changed`, both of which read the workspace from a
        // safe (un-leased) context. Reading it here would double-lease when
        // `activate_tab` runs inside a `Workspace::update` (e.g. a workspace
        // action handler or a test driving the panel through the window).
        if index < self.tabs.len() {
            self.active_index = Some(index);
            cx.notify();
            self.persist(cx);
        }
    }

    /// Move the tab at `from` so it lands at the position currently held by
    /// the tab at `to` (drag-and-drop reorder). The active tab follows its
    /// content across the move, then the new order is persisted (which also
    /// re-syncs `tab_order` for the mobile mirror via [`persist`]).
    fn reorder_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        // `to` indexes the original array; after removing `from` it is still
        // a valid insertion index because `to <= len - 1 == tabs.len()` now.
        self.tabs.insert(to, tab);
        self.active_index = self.active_index.map(|active| {
            if active == from {
                to
            } else {
                let mid = if active > from { active - 1 } else { active };
                if mid >= to { mid + 1 } else { mid }
            }
        });
        cx.notify();
        self.persist(cx);
    }

    /// Close button dispatch. A terminal tab is just dropped from the strip
    /// ([`close_tab`]). A chat tab is fully closed via the store
    /// ([`SolutionAgentStore::close_session`]): the transcript is flushed,
    /// the session is evicted + marked `closed_at` in the DB (so it surfaces
    /// in "Reopen Closed Chat"), and the resulting `SessionClosed` →
    /// `ChatProviderEvent::SessionRemoved` round-trip removes the tab here.
    fn close_tab_at(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.tabs.get(index) {
            Some(ConsoleTab::Chat { session_id, .. }) => {
                let id = *session_id;
                SolutionAgentStore::global(cx)
                    .update(cx, |store, cx| store.close_session(id, cx))
                    .log_err();
            }
            Some(ConsoleTab::Terminal { .. }) => self.close_tab(index, cx),
            None => {}
        }
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        self.active_index = if self.tabs.is_empty() {
            None
        } else {
            match self.active_index {
                Some(i) if i > index => Some(i - 1),
                Some(i) if i == index => Some(i.min(self.tabs.len() - 1)),
                other => other,
            }
        };
        cx.notify();
        self.persist(cx);
    }

    /// React to an external `persist_tab_order` mutation (mobile wire RPC,
    /// most commonly): close any local tab whose session is in `closed`,
    /// then spawn a tab for each session in `opened` that isn't already
    /// represented. Scoped to this panel's active solution — events for
    /// foreign solutions are ignored.
    fn apply_external_tab_changes(
        &mut self,
        solution_id: SolutionId,
        opened: Vec<SolutionSessionId>,
        closed: Vec<SolutionSessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let active_solution =
            workspace.read_with(cx, |ws, cx| active_solution_id_for_workspace(ws, cx));
        if active_solution.as_ref() != Some(&solution_id) {
            return;
        }
        for id in closed {
            self.close_chat_tab_by_session_id(id, cx);
        }
        for id in opened {
            if self.has_chat_tab_for(id) {
                continue;
            }
            // Spawn the tab. `new_tab_from_existing` returns a Task that
            // resolves once the SessionSessionView is wired up.
            let task = self.chat_provider.update(cx, |provider, cx| {
                provider.new_tab_from_existing(id, window, cx)
            });
            cx.spawn(async move |this, cx| {
                let view = task.await.log_err()?;
                this.update(cx, |this, cx| {
                    if this.has_chat_tab_for(id) {
                        // Race: a parallel handler already added the
                        // tab while we awaited the view. Drop ours.
                        return;
                    }
                    this.tabs.push(ConsoleTab::Chat {
                        view,
                        session_id: id,
                    });
                    let new_index = this.tabs.len() - 1;
                    // Activate when this is the session the local user just
                    // created (create-implies-open), or when the strip had
                    // no active tab yet. A remotely-created session that the
                    // desktop user didn't ask for lands without stealing the
                    // active tab.
                    if this.chat_tab_to_activate == Some(id) {
                        this.active_index = Some(new_index);
                        this.chat_tab_to_activate = None;
                    } else if this.active_index.is_none() {
                        this.active_index = Some(new_index);
                    }
                    cx.notify();
                    this.persist(cx);
                })
                .log_err();
                Some(())
            })
            .detach();
        }
    }

    /// Returns true when one of [`self.tabs`] is a Chat tab for
    /// [`session_id`]. Used by the external-mutation path to dedupe
    /// against tabs the user (or a previous handler) already opened.
    fn has_chat_tab_for(&self, session_id: SolutionSessionId) -> bool {
        self.tabs.iter().any(
            |tab| matches!(tab, ConsoleTab::Chat { session_id: sid, .. } if *sid == session_id),
        )
    }

    /// Close the Chat tab (if any) hosting [`session_id`]. No-op when
    /// no such tab is open. Driven by external store mutations: the
    /// wire-side `workspace.close_session` RPC and the destructive
    /// `solution_agent.delete_session` path both surface here.
    fn close_chat_tab_by_session_id(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        let index = self.tabs.iter().position(
            |tab| matches!(tab, ConsoleTab::Chat { session_id: sid, .. } if *sid == session_id),
        );
        if let Some(index) = index {
            self.close_tab(index, cx);
        }
    }
}

impl Panel for ConsolePanel {
    fn persistent_name() -> &'static str {
        CONSOLE_PANEL_KEY
    }

    fn panel_key() -> &'static str {
        CONSOLE_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.dock_position
    }

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dock_position = position;
        cx.notify();
        // Persisting to settings is a B-followup task.
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = ConsolePanelSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        if ConsolePanelSettings::get_global(cx).button_visible {
            Some(IconName::Console)
        } else {
            None
        }
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Toggle Console")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        2
    }
}

async fn wait_for_terminals_tasks(
    terminals_for_task: Vec<(usize, Entity<TerminalView>)>,
    cx: &mut AsyncApp,
) {
    let pending_tasks = terminals_for_task.iter().map(|(_, terminal)| {
        terminal.update(cx, |terminal_view, cx| {
            terminal_view
                .terminal()
                .update(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
        })
    });
    join_all(pending_tasks).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use solution_agent::store::SolutionAgentStore;
    use std::path::Path;
    use workspace::Workspace;

    #[test]
    fn tab_cwd_in_scope_filters_by_active_member() {
        let member_a = Path::new("/sol/member-a");
        let member_b = Path::new("/sol/member-b");

        // No active member → every tab is in scope (mirrors project_panel,
        // which shows all worktrees when no member is selected).
        assert!(tab_cwd_in_scope(Some(Path::new("/sol/member-a/sub")), None));
        assert!(tab_cwd_in_scope(None, None));

        // With an active member, a tab is in scope iff its cwd lives under
        // that member's root.
        assert!(tab_cwd_in_scope(Some(member_a), Some(member_a)));
        assert!(tab_cwd_in_scope(
            Some(Path::new("/sol/member-a/nested/dir")),
            Some(member_a)
        ));
        assert!(!tab_cwd_in_scope(Some(member_b), Some(member_a)));

        // A tab whose cwd is the solution root (no member) is hidden while a
        // member is active, and a tab with no recorded cwd never matches a
        // concrete member.
        assert!(!tab_cwd_in_scope(Some(Path::new("/sol")), Some(member_a)));
        assert!(!tab_cwd_in_scope(None, Some(member_a)));
    }

    #[test]
    fn effective_active_index_prefers_in_scope_active() {
        // Stored active tab is in scope → it stays active.
        assert_eq!(
            effective_active_index(&[false, true, true], Some(1)),
            Some(1)
        );
        // Stored active tab is out of scope → fall back to first in-scope tab.
        assert_eq!(
            effective_active_index(&[false, true, true], Some(0)),
            Some(1)
        );
        // No stored active → first in-scope tab.
        assert_eq!(effective_active_index(&[false, false, true], None), Some(2));
        // Nothing in scope → no active tab.
        assert_eq!(effective_active_index(&[false, false, false], Some(1)), None);
        assert_eq!(effective_active_index(&[], None), None);
        // Stale index past the end → fall back to first in-scope.
        assert_eq!(effective_active_index(&[true, false], Some(9)), Some(0));
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            terminal_view::init(cx);
            crate::init(cx);
        });
    }

    /// Bootstrap a real `Workspace` + `SolutionAgentStore` + `ConsolePanel`
    /// for terminal-tab tests. Chat-tab tests would additionally need the
    /// editor / language / font stack (`SolutionSessionView::new` embeds a
    /// real `editor::Editor`) — covered by the MCP e2e probe at runtime,
    /// not by these unit tests.
    async fn bootstrap_panel(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<Workspace>, Entity<ConsolePanel>) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", serde_json::json!({})).await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;

        let connect_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        cx.update(|cx| {
            let registry = std::sync::Arc::new(solution_agent::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
            let agent_store = SolutionAgentStore::global(cx);
            agent_store.update(cx, |s, _| {
                s.register_agent_server(
                    gpui::SharedString::from(solution_agent::claude_adapter::CLAUDE_ACP_AGENT_ID),
                    std::rc::Rc::new(solution_agent::test_support::MockAgentServer::new(
                        connect_count,
                    )),
                );
            });
        });

        let store = cx.read(|cx| SolutionAgentStore::global(cx));

        let window_handle = cx.add_window(|window, cx| Workspace::test_new(project, window, cx));

        let panel = window_handle
            .update(cx, |workspace, window, cx| {
                cx.new(|cx| ConsolePanel::new(workspace.weak_handle(), store, window, cx))
            })
            .unwrap();

        (window_handle, panel)
    }

    #[gpui::test]
    async fn defaults_to_bottom_position(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel) = bootstrap_panel(cx).await;

        window_handle
            .update(cx, |_workspace, window, cx| {
                assert_eq!(
                    panel.read(cx).position(window, cx),
                    DockPosition::Bottom,
                    "default position should be Bottom per ConsolePanelSettings defaults"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    async fn add_terminal_tab_appends_and_activates(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel) = bootstrap_panel(cx).await;

        window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |p, cx| p.add_terminal_tab(None, window, cx));
            })
            .unwrap();
        cx.run_until_parked();

        panel.read_with(cx, |p, _| {
            assert_eq!(p.tabs.len(), 1, "one tab after one NewTerminal");
            assert!(matches!(p.tabs[0], ConsoleTab::Terminal { .. }));
            assert_eq!(p.active_index, Some(0));
        });
    }

    #[gpui::test]
    async fn close_active_tab_moves_active_to_neighbor(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel) = bootstrap_panel(cx).await;

        // Spawn three terminal tabs.
        for _ in 0..3 {
            window_handle
                .update(cx, |_workspace, window, cx| {
                    panel.update(cx, |p, cx| p.add_terminal_tab(None, window, cx));
                })
                .unwrap();
            cx.run_until_parked();
        }

        // Activate the middle tab and close it. The active index should land
        // on the tab that shifted down from index 2 → 1.
        window_handle
            .update(cx, |_workspace, _window, cx| {
                panel.update(cx, |p, cx| {
                    p.activate_tab(1, cx);
                    assert_eq!(p.tabs.len(), 3);
                    assert_eq!(p.active_index, Some(1));
                    p.close_tab(1, cx);
                });
            })
            .unwrap();

        panel.read_with(cx, |p, _| {
            assert_eq!(p.tabs.len(), 2);
            assert_eq!(
                p.active_index,
                Some(1),
                "active_index should clamp to the new last tab (was 1 with 3 tabs; 1 with 2 tabs)"
            );
        });
    }

    #[gpui::test]
    async fn reorder_tab_moves_tab_and_tracks_active(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel) = bootstrap_panel(cx).await;

        // Four terminal tabs: indices 0,1,2,3.
        for _ in 0..4 {
            window_handle
                .update(cx, |_workspace, window, cx| {
                    panel.update(cx, |p, cx| p.add_terminal_tab(None, window, cx));
                })
                .unwrap();
            cx.run_until_parked();
        }

        // Capture per-tab entity ids so we can assert ordering after the move.
        let ids = |p: &ConsolePanel| -> Vec<gpui::EntityId> {
            p.tabs
                .iter()
                .map(|t| match t {
                    ConsoleTab::Terminal { view } => view.entity_id(),
                    ConsoleTab::Chat { view, .. } => view.entity_id(),
                })
                .collect()
        };

        let before = panel.read_with(cx, |p, _| ids(p));

        // Activate tab 2, then drag tab 0 onto position 2.
        window_handle
            .update(cx, |_workspace, _window, cx| {
                panel.update(cx, |p, cx| {
                    p.activate_tab(2, cx);
                    p.reorder_tab(0, 2, cx);
                });
            })
            .unwrap();

        panel.read_with(cx, |p, _| {
            let after = ids(p);
            // [0,1,2,3] with 0 moved to index 2 → [1,2,0,3].
            assert_eq!(
                after,
                vec![before[1], before[2], before[0], before[3]],
                "dragged tab lands at the target index, others shift"
            );
            // The active tab (originally index 2 = before[2]) is now at index 1.
            assert_eq!(
                p.active_index,
                Some(1),
                "active follows its content across the reorder"
            );
        });
    }

    #[gpui::test]
    async fn close_last_tab_clears_active(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel) = bootstrap_panel(cx).await;

        window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |p, cx| p.add_terminal_tab(None, window, cx));
            })
            .unwrap();
        cx.run_until_parked();

        window_handle
            .update(cx, |_workspace, _window, cx| {
                panel.update(cx, |p, cx| {
                    assert_eq!(p.tabs.len(), 1);
                    p.close_tab(0, cx);
                });
            })
            .unwrap();

        panel.read_with(cx, |p, _| {
            assert!(
                p.tabs.is_empty(),
                "tabs should be empty after closing the last one"
            );
            assert_eq!(p.active_index, None);
        });
    }

    #[gpui::test]
    async fn add_panel_registers_for_workspace_lookup(cx: &mut TestAppContext) {
        // `console_panel::NewTerminal` / `::NewChat` action handlers locate the
        // panel via `workspace.panel::<ConsolePanel>(cx)`. Verify that the
        // workspace can in fact retrieve the panel after `add_panel`, so the
        // action wiring isn't sabotaged at this seam. End-to-end action
        // dispatch needs a rendered workspace (GPUI attaches workspace
        // `register_action` handlers via the render div) — exercised live in
        // `docs/findings/2026-05-26-console-panel-shipped/`, not here.
        cx.executor().allow_parking();
        let (window_handle, panel) = bootstrap_panel(cx).await;

        window_handle
            .update(cx, |workspace, window, cx| {
                workspace.add_panel(panel.clone(), window, cx);
                assert!(
                    workspace.panel::<ConsolePanel>(cx).is_some(),
                    "ConsolePanel should be retrievable via workspace.panel::<ConsolePanel>(cx) after add_panel"
                );
            })
            .unwrap();
    }

}
