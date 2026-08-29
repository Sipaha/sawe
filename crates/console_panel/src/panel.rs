use anyhow::{Result, anyhow};
use collections::HashMap;
use futures::channel::oneshot;
use futures::future::join_all;
use gpui::{
    Action, Anchor, App, AppContext as _, AsyncApp, AsyncWindowContext, Context, DismissEvent,
    Entity, FocusHandle, Focusable, IntoElement, MouseButton, MouseDownEvent, Pixels, Point,
    Render, Subscription, Task, WeakEntity, Window, anchored, deferred,
};
use solution_agent::reopen_session_modal::{ReopenSessionModal, ReopenableSession};
use solution_agent::solution_band::SolutionBand;
use solution_agent::store::SolutionAgentStore;
use solutions::{MemberId, SolutionId, SolutionStore};
use std::path::PathBuf;
use task::{RevealStrategy, RevealTarget, Shell, SpawnInTerminal, TaskId};
use terminal::Terminal;
use terminal_view::TerminalView;
use terminal_view::terminal_panel::prepare_task_for_spawn;
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{Item, UtilityKind, Workspace, WorkspaceDb};

use crate::TerminalProvider;
use crate::actions::NewChat;

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
            return Some(sol.id);
        }
    }
    None
}

/// Resolve the concrete `Entity<ConsolePanel>` from the type-erased
/// `Workspace::solution_band_utility_item` slot `zed.rs` installs at
/// startup (phase 2a task 6). Replaces the old `workspace.panel::<ConsolePanel>(cx)`
/// dock lookup, which walked the docks and would find nothing now that the
/// panel isn't registered in one. `None` means either the panel-init task
/// (`initialize_panels`) hasn't finished yet, or this workspace has no
/// Solution band installed at all.
pub fn console_panel_for_workspace(workspace: &Workspace) -> Option<Entity<ConsolePanel>> {
    workspace
        .solution_band_utility_item(UtilityKind::Terminal)?
        .downcast::<ConsolePanel>()
        .ok()
}

/// Make the Solution band's utility section show `kind` (without stealing
/// focus) so whatever the caller just produced is actually on screen.
/// `console_panel` already depends on `solution_agent` (for
/// `SolutionAgentStore`), so unlike `SolutionBand` itself — which cannot
/// hold a typed `Entity<ConsolePanel>` without creating a crate cycle — this
/// direction is fine: downcast `Workspace::solution_band_item`'s `AnyView`
/// straight to the concrete `SolutionBand` type. No-op if the band isn't
/// installed (headless/test workspaces).
///
/// `kind` is a parameter, not `UtilityKind::Terminal` baked in, because
/// "reveal the section" and "reveal *the terminal*" are different intents
/// and only the caller knows which it means. Every caller in this file
/// passes `Terminal` and must keep doing so: selecting the kind is
/// load-bearing, not belt-and-braces. `utility_kind` is persisted per
/// Solution and the debugger writes it too (phase 2b task 5), so revealing
/// only `utility_visible` would pop the band open on whatever was last
/// shown — for a `RevealStrategy::Always` task that means focusing a
/// terminal the user cannot see.
fn reveal_utility_section(workspace: &Workspace, kind: UtilityKind, cx: &mut App) {
    let Some(band) = workspace
        .solution_band_item()
        .and_then(|item| item.downcast::<SolutionBand>().ok())
    else {
        return;
    };
    band.update(cx, |band, cx| {
        band.set_utility_kind(kind, cx);
        band.set_utility_visible(true, cx);
    });
}

/// Whether this workspace has a project to run a terminal in — the gate on
/// terminal creation ("+" menu state, `NewTerminal`). AI chats are
/// solution-scoped (spec 2026-08-26: a chat always roots at `solution.root`)
/// and never use this gate — only a terminal needs a directory to `cd` into.
///
/// For a **Solution** workspace the authoritative answer is its member list, not
/// its worktrees: `solutions_ui::open` opens an EMPTY solution with the solution
/// root as an *invisible* worktree (`OpenVisible::None`), and a naive
/// `project.worktrees()` check counts invisible worktrees too — so it would pass
/// for exactly the case this gate exists to block. `Solution::members` is the
/// single source of truth for "which projects are in this solution" (plan 1's
/// numeric `MemberId`s), so ask it directly.
///
/// A plain folder workspace (not a Solution) has no member list; there the
/// question really is "is a project directory open", which means a VISIBLE
/// worktree — an invisible one is a stray single file, not a project.
///
/// Takes `&Workspace` directly so it is safe to call from action handlers that
/// already hold the `Workspace` leased (reading the entity via `cx` there would
/// double-lease-panic).
pub fn workspace_has_project(workspace: &Workspace, cx: &App) -> bool {
    if let Some(solution_id) = active_solution_id_for_workspace(workspace, cx)
        && let Some(store) = SolutionStore::try_global(cx)
    {
        return store
            .read(cx)
            .solutions()
            .iter()
            .find(|solution| solution.id == solution_id)
            .is_some_and(|solution| !solution.members.is_empty());
    }
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .is_some()
}

/// Folder of the solution's *active* project — the one selected in the
/// project tab strip — falling back to the solution root when there is no
/// active member. Used as the `cwd` for new terminals started from the "+"
/// menu; a chat never uses this — it is solution-scoped and always roots at
/// `solution.root` (spec 2026-08-26), decided by the `NewChat` action
/// handler in `console_panel.rs`.
fn active_member_path(solution_id: SolutionId, cx: &App) -> Option<PathBuf> {
    let store = SolutionStore::try_global(cx)?;
    let store = store.read(cx);
    store.active_member_path(solution_id)
}

/// Which project a console tab belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabScope {
    /// The tab is bound to a member by placement — a terminal's cwd lives
    /// inside the member's folder.
    Member(MemberId),
    /// The tab sits at the solution root and belongs to no project.
    Root,
    /// The tab cannot be placed at all — a terminal opened by the bare
    /// `NewTerminal` keybinding with no cwd, or one restored with a NULL cwd.
    Unscoped,
}

/// Whether a tab is visible under the current active-member selection. A `None`
/// active member means "no filter". An `Unscoped` tab is always shown: a tab we
/// can't place must never silently vanish (that would leave an un-closeable
/// ghost tab in the strip).
pub(crate) fn tab_in_scope(scope: TabScope, active_member: Option<MemberId>) -> bool {
    match (scope, active_member) {
        (_, None) => true,
        (TabScope::Unscoped, _) => true,
        (TabScope::Member(member), Some(active)) => member == active,
        (TabScope::Root, Some(_)) => false,
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

/// Whether the end-of-restore reconciliation (`ConsolePanel::persist`) may run.
///
/// That reconciliation is destructive in both directions: it DELETEs+INSERTs
/// the whole `console_panel_state` row set for the workspace and re-derives
/// every session's `tab_order` from the tabs that actually made it into the
/// strip (`persist_tab_order` NULLs `tab_order` on every session of the
/// solution that is absent from the strip). Running it after a PARTIAL restore
/// therefore promotes a transient failure — a session that had not hydrated
/// yet, a solution the workspace could not resolve, a window that went away
/// mid-restore — into permanent data loss: the very next boot has no rows left
/// to restore from. One bad boot would make the tab set unrecoverable.
///
/// So reconcile only when every persisted row came back. A lossy restore keeps
/// the DB exactly as it was; the next successful boot (or the next real tab
/// mutation, which persists the user's actual intent) reconciles instead.
fn restore_may_reconcile(rows_persisted: usize, tabs_restored: usize) -> bool {
    tabs_restored >= rows_persisted
}

pub enum ConsoleTab {
    Terminal {
        view: Entity<TerminalView>,
        /// The directory the terminal was created in (the active member path
        /// at spawn time). Used to scope the tab to its owning member project.
        /// Fixed for the tab's life — unlike the terminal's *live* working
        /// directory, which wanders with `cd` and becomes unreadable when the
        /// foreground process is owned by another user (e.g. after `sudo su`,
        /// `/proc/<pid>/cwd` is denied), which would otherwise make the tab
        /// silently drop out of scope and vanish from the strip.
        origin_cwd: Option<PathBuf>,
    },
}

/// Stable per-tab identity used to remember the active tab for each member
/// project across active-member switches. Indices shift as tabs open/close,
/// so the per-member memory is keyed by content, not position.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ConsoleTabKey {
    Terminal(gpui::EntityId),
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
    terminal_provider: Entity<TerminalProvider>,
    focus_handle: FocusHandle,
    tab_context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    pending_terminals_to_add: usize,
    deferred_tasks: HashMap<TaskId, Task<()>>,
    assistant_enabled: bool,
    /// Last-active tab per member project, so switching back to a member
    /// restores the exact terminal the user last had open there — every key
    /// is a terminal tab now that chats left `ConsolePanel` for the Solution
    /// band (phase 2a task 5). In-memory only — on restart the panel falls
    /// back to each member's first tab.
    active_by_member: HashMap<PathBuf, ConsoleTabKey>,
    /// The member path the panel last rendered for; used to attribute the
    /// outgoing active tab to the correct member when the active member flips.
    last_member_path: Option<PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl ConsolePanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let terminal_provider = cx.new(|_| TerminalProvider::new(workspace.clone()));
        // Re-scope the visible tabs whenever the solution-wide active member
        // flips, so the strip + content swap to that project's own dialogs —
        // mirroring how Project Panel / Git Panel follow the active member.
        let member_change_sub = SolutionStore::try_global(cx).map(|store| {
            cx.subscribe(&store, |this, _store, event, cx| {
                if matches!(
                    event,
                    solutions::SolutionStoreEvent::ActiveMemberChanged { .. }
                ) {
                    this.on_active_member_changed(cx);
                }
            })
        });
        let subscriptions = member_change_sub.into_iter().collect();
        Self {
            workspace,
            tabs: Vec::new(),
            active_index: None,
            terminal_provider,
            focus_handle: cx.focus_handle(),
            tab_context_menu: None,
            pending_terminals_to_add: 0,
            deferred_tasks: HashMap::default(),
            assistant_enabled: false,
            active_by_member: HashMap::default(),
            last_member_path: None,
            _subscriptions: subscriptions,
        }
    }

    /// Loader. Constructs a fresh `ConsolePanel` and then restores any
    /// persisted terminal tabs from the workspace DB, re-spawning each at its
    /// stored CWD with a fresh shell (clean-start policy: state inside the
    /// shell is *not* restored). Chat tabs left `ConsolePanel` for the
    /// Solution band (phase 2a task 5); any persisted `console_panel_state`
    /// row of kind `chat` is legacy data from before that move and is
    /// skipped here (see [`restore_from_db`]) and purged by a one-shot DB
    /// migration (`crates/workspace/src/persistence.rs`).
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let panel = workspace.update_in(&mut cx, |workspace, _window, cx| {
            cx.new(|cx| Self::new(workspace.weak_handle(), cx))
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

        let terminal_provider: Entity<TerminalProvider> =
            panel.read_with(cx, |panel, _| panel.terminal_provider.clone());

        let mut active_index: Option<usize> = None;
        let rows_persisted = rows.len();
        let mut tabs_restored = 0usize;

        for (tab_index, kind, item_id, cwd, active) in rows {
            let spawned = match kind.as_str() {
                "terminal" => {
                    let cwd_path = cwd.as_ref().map(PathBuf::from);
                    let provider = terminal_provider.clone();
                    let task = cx.update(|window, cx| {
                        // `update` gives the closure `&mut TerminalProvider`,
                        // which sidesteps the `read(cx).method(cx)` borrow
                        // conflict on the outer `cx`.
                        provider.update(cx, |provider, cx| {
                            provider.new_tab(cwd_path.clone(), window, cx)
                        })
                    });
                    match task {
                        Ok(task) => match task.await {
                            Ok(view) => Some(ConsoleTab::Terminal {
                                view,
                                origin_cwd: cwd_path.clone(),
                            }),
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
                    // Chat tabs left `ConsolePanel` for the Solution band
                    // (phase 2a task 5) — this row is legacy data from
                    // before that move. The panel can no longer restore it;
                    // a one-shot DB migration purges these rows so they
                    // don't accumulate (`crates/workspace/src/persistence.rs`).
                    // The session itself is untouched in `solution_sessions`
                    // — only the panel's memory of "it was open" is gone.
                    log::info!(
                        "ConsolePanel restore: skipping legacy chat tab #{tab_index} \
                         (item_id={item_id:?}); chat tabs now live in the Solution band"
                    );
                    None
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
                tabs_restored += 1;
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
            // `hydrate_all_for_solution` — they were free to diverge once a desktop
            // user added a tab in a previous run (only ConsolePanel persisted
            // the new tab; tab_order stayed pointing at the previous set).
            // Calling persist here at end of restore harmonises them.
            //
            // ...but ONLY when the restore was complete — see
            // `restore_may_reconcile`.
            if restore_may_reconcile(rows_persisted, tabs_restored) {
                panel.persist(cx);
            } else {
                log::warn!(
                    "ConsolePanel restore: only {tabs_restored}/{rows_persisted} persisted tab(s) \
                     came back; skipping the end-of-restore reconciliation so the missing rows \
                     stay recoverable on the next boot"
                );
            }
        });

        Ok(())
    }

    /// Snapshot the current (terminal-only) tab list into `console_panel_state`.
    /// Chat tabs left `ConsolePanel` for the Solution band (phase 2a task 5),
    /// so this no longer needs to reconcile `SolutionSession.tab_order` — that
    /// field is now owned entirely by the store's own session-lifecycle calls
    /// (`create_session_with_cwd`'s create-implies-open pin, `close_session`).
    fn persist(&self, cx: &mut Context<Self>) {
        // Snapshot tab state synchronously — we only read TerminalView
        // entities here, never the Workspace. Workspace lookup for
        // `database_id` is deferred into the spawned task below so this
        // method is safe to call while a `Workspace::update` is in flight on
        // the outer borrow stack (action handlers, modal close paths, …).
        let active_index = self.active_index;
        let rows: Vec<(i64, String, String, Option<String>, bool)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let ConsoleTab::Terminal { origin_cwd, .. } = tab;
                // Persist the immutable `origin_cwd` (the owning member
                // path), NOT the live working directory: the shell process
                // doesn't survive restart, and restore re-spawns the
                // terminal in this cwd — reopening it in its member dir
                // keeps the tab in scope (a live cwd captured under `sudo
                // su` would be empty/unreadable and drop the restored tab
                // out of scope).
                let cwd = origin_cwd
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned());
                // The `item_id` column is informational for terminal rows;
                // restore only consults `cwd`. We use the cwd string (or an
                // empty marker) so the column stays human-readable in the DB.
                let item_id = cwd.clone().unwrap_or_default();
                (
                    ix as i64,
                    "terminal".to_string(),
                    item_id,
                    cwd,
                    active_index == Some(ix),
                )
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
}

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
            // Only render terminal tabs belonging to the active member
            // project; the rest stay live in `self.tabs` (absolute indices
            // keep activate/close/reorder valid) but are hidden until their
            // member is selected.
            if !scope_flags.get(ix).copied().unwrap_or(true) {
                continue;
            }
            let ConsoleTab::Terminal { view, .. } = tab;
            let (icon, title): (IconName, SharedString) =
                (IconName::Terminal, view.read(cx).tab_content_text(0, cx));
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
                        .on_click(cx.listener(move |this, _, _, cx| this.close_tab(ix, cx))),
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
        // New terminals open in the active project's folder (the project
        // selected in the project tab strip). New chats are solution-scoped
        // and always root at `solution.root` — handled entirely by the
        // `NewChat` action (`console_panel.rs`), not here.
        let active_path = active_solution_id.and_then(|id| active_member_path(id, cx));
        // A terminal needs a project directory to run in. An empty solution has
        // no member project, so grey out "New Terminal" (the action handlers
        // enforce the same rule for the keyboard path). A non-empty solution, or a
        // plain folder with a visible worktree, is allowed.
        let has_project = self
            .workspace
            .upgrade()
            .is_some_and(|ws| workspace_has_project(ws.read(cx), cx));
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
                    let active_path = active_path.clone();
                    let weak_self = weak_self.clone();
                    Some(ContextMenu::build(window, cx, move |menu, _, _| {
                        // New Terminal in the active project's folder. Disabled
                        // when there's no active project (empty / no solution) —
                        // there's nowhere to run it.
                        let menu = {
                            let weak_self = weak_self.clone();
                            let cwd = active_path.clone();
                            let label = if has_project {
                                "New Terminal"
                            } else {
                                "New Terminal (no project)"
                            };
                            menu.item(
                                ui::ContextMenuEntry::new(label)
                                    .disabled(!has_project)
                                    .handler(move |window, cx| {
                                        if let Some(panel) = weak_self.upgrade() {
                                            panel.update(cx, |panel, cx| {
                                                panel.add_terminal_tab(cwd.clone(), window, cx);
                                            });
                                        }
                                    }),
                            )
                        };
                        // New AI Chat: dispatches the same `NewChat` action the
                        // status-bar session tab strip's own "+" button uses
                        // (`solution_agent::session_tab_strip`), so chat
                        // creation has exactly one code path regardless of
                        // which "+" menu triggered it — two paths disagreeing
                        // on a new chat's cwd was the phase-1 Critical. The
                        // handler itself no-ops without an active solution;
                        // grey the entry out for the same reason here.
                        let menu = menu.action_disabled_when(
                            !has_active_solution,
                            "New AI Chat",
                            NewChat.boxed_clone(),
                        );
                        // Reopen a chat that was closed but still lives on disk.
                        // Solution-scoped like New AI Chat above — needs an
                        // active solution, not a member project.
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
        active_member_path(solution_id, cx)
    }

    /// A terminal is placed by its *immutable* creation-time `origin_cwd`
    /// (longest matching member wins; anything under the root but in no
    /// member is `Root`) — deliberately NOT the terminal's live working
    /// directory, which wanders with `cd` and goes unreadable under a
    /// foreign-user foreground process (`sudo su`), which would drop the tab
    /// out of scope and make it disappear from the strip.
    fn tab_scope(&self, tab: &ConsoleTab, cx: &App) -> TabScope {
        let ConsoleTab::Terminal { origin_cwd, .. } = tab;
        let Some(cwd) = origin_cwd.clone() else {
            return TabScope::Unscoped;
        };
        let Some(solution_id) = self.active_solution_id(cx) else {
            return TabScope::Unscoped;
        };
        let Some(store) = SolutionStore::try_global(cx) else {
            return TabScope::Unscoped;
        };
        let store = store.read(cx);
        let Ok(solution) = store.find_solution(solution_id) else {
            return TabScope::Unscoped;
        };
        match solution.member_for_path(&cwd) {
            Some(member) => TabScope::Member(member.id),
            None if cwd.starts_with(&solution.root) => TabScope::Root,
            None => TabScope::Unscoped,
        }
    }

    /// The solution-wide active member the tab strip is currently filtered by.
    fn active_member(&self, cx: &App) -> Option<MemberId> {
        let solution_id = self.active_solution_id(cx)?;
        let store = SolutionStore::try_global(cx)?;
        store.read(cx).active_member(solution_id)
    }

    /// Per-tab in-scope flags for the currently active member, in tab order.
    fn tab_scope_flags(&self, cx: &App) -> Vec<bool> {
        let active_member = self.active_member(cx);
        self.tabs
            .iter()
            .map(|tab| tab_in_scope(self.tab_scope(tab, cx), active_member))
            .collect()
    }

    /// Stable identity for a tab, used to remember the active tab per member
    /// across active-member switches.
    fn tab_key(tab: &ConsoleTab) -> ConsoleTabKey {
        let ConsoleTab::Terminal { view, .. } = tab;
        ConsoleTabKey::Terminal(view.entity_id())
    }

    /// Re-resolve `active_index` for the now-active member: remember the tab
    /// that was active for the previous member, then restore the new
    /// member's last-active tab (if it is still present and in scope),
    /// falling back to its first in-scope tab. Called when the solution-wide
    /// active member flips so the strip swaps to that project's own
    /// terminals. Every key here is a terminal tab now that chats left
    /// `ConsolePanel` for the Solution band (phase 2a task 5).
    fn on_active_member_changed(&mut self, cx: &mut Context<Self>) {
        // Stash the outgoing member's active tab so switching back restores
        // the exact terminal the user last had open.
        if let Some(prev) = self.last_member_path.take()
            && let Some(ix) = self.active_index
            && let Some(tab) = self.tabs.get(ix)
        {
            self.active_by_member.insert(prev, Self::tab_key(tab));
        }

        let member_path = self.active_member_path(cx);
        let flags = self.tab_scope_flags(cx);

        let remembered = member_path
            .as_ref()
            .and_then(|path| self.active_by_member.get(path).copied());
        let remembered_ix =
            remembered.and_then(|key| self.tabs.iter().position(|tab| Self::tab_key(tab) == key));

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
        let origin_cwd = cwd.clone();
        let task = self
            .terminal_provider
            .update(cx, |provider, cx| provider.new_tab(cwd, window, cx));
        cx.spawn(async move |this, cx| {
            let view = task.await?;
            this.update(cx, |this, cx| {
                this.tabs.push(ConsoleTab::Terminal { view, origin_cwd });
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
        // No project directory to run in (an empty solution has no member
        // project) → refuse both the center-pane and the console-panel spawn.
        if !workspace_has_project(workspace, cx) {
            return;
        }
        let center_pane = workspace.active_pane();
        let center_pane_has_focus = center_pane.focus_handle(cx).contains_focused(window, cx);
        let active_center_item_is_terminal = center_pane
            .read(cx)
            .active_item()
            .is_some_and(|item| item.downcast::<TerminalView>().is_some());

        if center_pane_has_focus && active_center_item_is_terminal {
            let working_directory = terminal_view::default_working_directory(workspace, cx);
            let local = action.local;
            terminal_view::terminal_panel::TerminalPanel::add_center_terminal(
                workspace,
                window,
                cx,
                move |project, cx| {
                    if local {
                        project.create_local_terminal(cx)
                    } else {
                        project.create_terminal_shell(working_directory, cx)
                    }
                },
            )
            .detach_and_log_err(cx);
            return;
        }

        let Some(console_panel) = console_panel_for_workspace(workspace) else {
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
        let origin_cwd = task.cwd.clone();
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
                        reveal_utility_section(workspace, UtilityKind::Terminal, cx);
                        if let Some(panel) = this.upgrade() {
                            panel.focus_handle(cx).focus(window, cx);
                        }
                    }
                    RevealStrategy::NoFocus => {
                        reveal_utility_section(workspace, UtilityKind::Terminal, cx);
                    }
                    RevealStrategy::Never => {}
                }
                view
            })?;
            this.update(cx, |this, cx| {
                this.tabs.push(ConsoleTab::Terminal {
                    view: terminal_view,
                    origin_cwd,
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
    ///
    /// **Must not be called while the `Workspace` entity is leased** — it reads
    /// its own `WeakEntity<Workspace>` below, and under a lease a read aborts
    /// the process exactly as an update does (`entity_map.rs:164`), while
    /// compiling clean and passing unit tests. So calling it from a
    /// `workspace.register_action` handler — or anything else holding
    /// `&mut Workspace` — kills the editor. Call it from an async task
    /// instead. Its only caller,
    /// `run_config_ui::run_controller::RunController`, does that; the method
    /// this one mirrors, `TerminalPanel::spawn_task`, carries the same
    /// constraint and its private `TerminalProvider::spawn` wrapper satisfies
    /// it the same way, from inside `window.spawn`.
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

    /// Inherits `spawn_task`'s no-active-`Workspace`-lease requirement. The
    /// `RevealTarget::Center` arm below is a second, independent violation of
    /// it — it *updates* the workspace synchronously, since
    /// `add_center_terminal` takes `&mut Workspace` — so removing the read in
    /// `spawn_task` would not make this path callable under a lease.
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
                    terminal_view::terminal_panel::TerminalPanel::add_center_terminal(
                        workspace,
                        window,
                        cx,
                        |project, cx| project.create_terminal_task(spawn_task, cx),
                    )
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
                                reveal_utility_section(workspace, UtilityKind::Terminal, cx);
                            });
                        }
                        this.focus_handle(cx).focus(window, cx);
                    })?;
                }
                RevealStrategy::NoFocus => {
                    this.update_in(cx, |this, _window, cx| {
                        this.activate_tab(existing_tab_index, cx);
                        if let Some(workspace) = this.workspace.upgrade() {
                            workspace.update(cx, |workspace, cx| {
                                reveal_utility_section(workspace, UtilityKind::Terminal, cx);
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
            .filter_map(|(index, tab)| {
                let ConsoleTab::Terminal { view, .. } = tab;
                let task_state = view.read(cx).terminal().read(cx).task()?;
                if task_state.spawned_task.full_label == label {
                    Some((index, view.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Mirrors `TerminalPanel::terminal_selections`: the non-empty selection
    /// text of every terminal tab.
    pub fn terminal_selections(&self, cx: &App) -> Vec<String> {
        self.tabs
            .iter()
            .filter_map(|tab| {
                let ConsoleTab::Terminal { view, .. } = tab;
                view.read(cx)
                    .terminal()
                    .read(cx)
                    .last_content
                    .selection_text
                    .clone()
                    .filter(|text| !text.is_empty())
            })
            .collect()
    }

    /// The currently-active terminal tab's view, if any.
    pub fn active_terminal_view(&self, _cx: &App) -> Option<Entity<TerminalView>> {
        let ix = self.active_index?;
        let ConsoleTab::Terminal { view, .. } = self.tabs.get(ix)?;
        Some(view.clone())
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

    fn show_tab_context_menu(
        &mut self,
        tab_index: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ConsoleTab::Terminal { view, .. }) = self.tabs.get(tab_index) else {
            return;
        };
        let view = view.clone();
        let weak = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
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
                            view.rename_terminal(&terminal_view::RenameTerminal, window, cx);
                        });
                    });
                }
            })
            .entry("Reveal CWD in Project Panel", None, move |window, cx| {
                if let Some(this) = weak_reveal.upgrade() {
                    this.update(cx, |this, cx| {
                        this.reveal_terminal_cwd(&view_reveal, window, cx);
                    });
                }
            })
        });
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

    /// Reopen-a-closed-chat flow. Hydrates the active solution's
    /// on-disk sessions, gathers the top-level ones that aren't currently
    /// pinned in the strip (closed tabs whose transcript survives), and
    /// opens a picker. Selecting a session re-pins it via
    /// `SolutionAgentStore::open_session_in_strip` — the same "open" path
    /// create and the wire RPC use — so the tab lands through the normal
    /// `TabsChanged` writer.
    ///
    /// Deliberately kept in `ConsolePanel`'s "+" popover even though chat
    /// tabs left the panel for the Solution band (phase 2a task 5): unlike
    /// `add_chat_tab`, this flow never builds a `ConsoleTab` — it only reads
    /// the store and opens a modal — so it has no replacement to migrate to
    /// and no reason to move. `Rename Session` / `Restart Agent`, which used
    /// to live on the (now-deleted) chat tab's right-click menu, have no
    /// such home any more; both remain reachable via the
    /// `solution_agent.{rename_session,restart_agent}` MCP tools but not
    /// from a desktop click path until a future task gives the status-bar
    /// `SessionTabStrip` (`solution_agent::session_tab_strip`) its own
    /// context menu.
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
        let closed = store.update(cx, |store, cx| store.list_closed_sessions(solution_id, cx));
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
        let ConsoleTab::Terminal { view, .. } = &self.tabs[ix];
        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(view.clone())
            .into_any_element()
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
    use crate::ToggleFocus;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use solution_agent::store::SolutionAgentStore;
    use workspace::Workspace;

    #[test]
    fn tab_in_scope_filters_by_active_member() {
        let a = MemberId(1);
        let b = MemberId(2);

        // No active member → the panel shows everything.
        assert!(tab_in_scope(TabScope::Member(a), None));
        assert!(tab_in_scope(TabScope::Root, None));
        assert!(tab_in_scope(TabScope::Unscoped, None));

        // A tab bound to the active member is in scope; one bound to a sibling
        // is not — even though both live under the same solution root, and even
        // if one of the folders was renamed out from under the tab's cwd.
        assert!(tab_in_scope(TabScope::Member(a), Some(a)));
        assert!(!tab_in_scope(TabScope::Member(b), Some(a)));

        // A solution-root tab is hidden while a member is selected (it is not
        // part of that project), matching the pre-existing prefix behaviour.
        assert!(!tab_in_scope(TabScope::Root, Some(a)));

        // A tab we cannot place must never silently vanish — hiding it would
        // leave an un-closeable ghost in the strip.
        assert!(tab_in_scope(TabScope::Unscoped, Some(a)));
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
        assert_eq!(
            effective_active_index(&[false, false, false], Some(1)),
            None
        );
        assert_eq!(effective_active_index(&[], None), None);
        // Stale index past the end → fall back to first in-scope.
        assert_eq!(effective_active_index(&[true, false], Some(9)), Some(0));
    }

    /// The end-of-restore reconciliation may only run on a COMPLETE restore.
    /// A lossy one must leave `console_panel_state` / `tab_order` untouched,
    /// otherwise a single bad boot (session not hydrated, no active solution,
    /// window gone mid-restore) permanently commits the loss.
    #[test]
    fn restore_reconciles_only_when_nothing_was_lost() {
        // Every persisted row came back → reconcile.
        assert!(restore_may_reconcile(3, 3));
        // Nothing persisted, nothing restored → vacuously complete.
        assert!(restore_may_reconcile(0, 0));

        // A skipped row (failed terminal spawn, legacy chat row, unknown
        // kind) or an aborted loop (window went away) → do NOT commit the
        // loss.
        assert!(!restore_may_reconcile(3, 2));
        assert!(!restore_may_reconcile(1, 0));
        // The pathological case the bug reproduced: every persisted tab was
        // a legacy chat row, none of which the panel can restore any more
        // (chat tabs left `ConsolePanel` for the Solution band).
        assert!(!restore_may_reconcile(5, 0));
    }

    /// `solution_agent::session_tab_strip`'s trailing `+` button dispatches
    /// this action *by name* (`cx.build_action("console_panel::NewChat", ..)`)
    /// rather than importing the type — `solution_agent` cannot depend on
    /// `console_panel` (the reverse dependency already exists). That string
    /// literal lives three files away from this one with nothing the
    /// compiler checks tying them together, so a rename of `NewChat` or of
    /// this crate's name would otherwise only surface as a `log::error!` at
    /// runtime — the `+` button would silently do nothing. This test can't
    /// see that literal directly (still no cross-crate dependency), but it
    /// pins the registered name so a rename fails HERE, in CI, instead of
    /// silently in the shipped `+` button: `cargo test -p console_panel` is
    /// exactly the kind of check a rename's author is expected to run.
    #[test]
    fn new_chat_action_matches_the_status_bar_strips_dispatch_string() {
        assert_eq!(NewChat.name(), "console_panel::NewChat");
    }

    /// Same pinning for the band's utility button group (phase 2b task 6),
    /// which builds this action by name for its Terminal button's tooltip
    /// keybinding. `solution_agent` cannot import the action type (cycle), so
    /// a rename would silently strip the keybinding from the tooltip rather
    /// than fail to compile — this test is what makes it fail loudly.
    #[test]
    fn toggle_focus_action_matches_the_utility_button_tooltip_lookup() {
        assert_eq!(
            solution_agent::utility_buttons::toggle_action_name(UtilityKind::Terminal),
            Some(crate::ToggleFocus.name())
        );
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

    /// Bootstrap a real `Workspace` + `SolutionAgentStore` + `ConsolePanel`.
    /// `ConsolePanel` is terminal-only (chat tabs left for the Solution band
    /// in phase 2a task 5); the `SolutionAgentStore` is still wired up here
    /// because it backs "Reopen Closed Chat…" in the "+" popover.
    async fn bootstrap_panel(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<Workspace>, Entity<ConsolePanel>) {
        bootstrap_panel_with_worktrees(cx, &["/root"]).await
    }

    /// Like [`bootstrap_panel`] but with an explicit worktree set — pass `&[]`
    /// to model an empty solution (no project directory).
    async fn bootstrap_panel_with_worktrees(
        cx: &mut TestAppContext,
        worktrees: &[&str],
    ) -> (gpui::WindowHandle<Workspace>, Entity<ConsolePanel>) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        for wt in worktrees {
            fs.insert_tree(*wt, serde_json::json!({})).await;
        }
        let paths: Vec<&std::path::Path> =
            worktrees.iter().map(|p| std::path::Path::new(p)).collect();
        let project = Project::test(fs, paths, cx).await;

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

        let window_handle = cx.add_window(|window, cx| Workspace::test_new(project, window, cx));

        let panel = window_handle
            .update(cx, |workspace, _window, cx| {
                cx.new(|cx| ConsolePanel::new(workspace.weak_handle(), cx))
            })
            .unwrap();

        (window_handle, panel)
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

    /// Regression guard for the Critical-1-fix regression (2026-08-26
    /// second-pass final review), now guarding `handle_new_chat`
    /// (`console_panel.rs`) — the direct `Workspace`-action successor to
    /// `ConsolePanel::add_chat_tab`, which is what this test used to drive
    /// before chat tabs left `ConsolePanel` for the Solution band (phase 2a
    /// task 5). `handle_new_chat` runs under `workspace.register_action`'s
    /// mutable `Workspace` lease; anything it does that re-acquires that
    /// SAME entity's lease through a weak handle (`self.workspace.upgrade()
    /// ?.read(cx)`, the exact shape `add_chat_tab`'s `active_member_path`
    /// call used to have) double-lease-panics. `WindowHandle::update`'s
    /// closure leases the root view (`Workspace`) the same way the real
    /// dispatch does, and the Solution/member setup below makes
    /// `active_solution_id_for_workspace` resolve to `Some`, so the call
    /// actually reaches the project-read + `SolutionAgentStore` call that
    /// would double-lease if it ever went back to reading `Workspace`
    /// through `self` instead of the `&mut Workspace` it's handed.
    #[gpui::test]
    async fn new_chat_action_does_not_double_lease_the_workspace(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

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

        // A Solution whose root the test workspace's worktree lives under,
        // so `active_solution_id_for_workspace` resolves to `Some` and
        // `handle_new_chat` runs past its first guard.
        let solution_root = cx.update(|cx| {
            let store = SolutionStore::for_test(std::path::PathBuf::from("/cfg.json"), cx);
            let root = store.update(cx, |store, cx| {
                let id = store.create_for_test_minimal("NewChatGuard", cx);
                store
                    .solutions()
                    .iter()
                    .find(|sol| sol.id == id)
                    .map(|sol| sol.root.clone())
                    .expect("just-created solution")
            });
            solutions::install_global_for_test(store, cx);
            root
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&solution_root, serde_json::json!({})).await;
        let project = Project::test(fs, [solution_root.as_path()], cx).await;
        let window_handle = cx.add_window(|window, cx| Workspace::test_new(project, window, cx));
        cx.run_until_parked();

        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_new_chat(workspace, &NewChat, window, cx);
            })
            .unwrap();
        // The async `create_session_with_cwd` task settles (successfully or
        // not) after this call returns — either way is fine. What this test
        // guards is that the synchronous part of `handle_new_chat` (reading
        // the active solution + project off `&mut Workspace`, then calling
        // into `SolutionAgentStore`) does not panic while the `Workspace` is
        // leased.
        cx.run_until_parked();
    }

    /// Bootstrap a `Workspace` with BOTH Solution-band slots filled — the
    /// `SolutionBand` in `solution_band_item` and a `ConsolePanel` under
    /// `UtilityKind::Terminal` in the `solution_band_utility_item` map —
    /// which is what `handle_toggle_focus` resolves at runtime.
    /// `worktree_under_solution` decides whether the
    /// workspace's worktree lives under the created Solution's root (so
    /// `SolutionBand::solution_id` resolves to `Some`) or in an unrelated
    /// plain folder (so it resolves to `None` and the band falls back to its
    /// window-local state). Returns the created Solution's id either way, so
    /// the caller can assert which of the two stores the toggle wrote to.
    async fn bootstrap_band_and_panel(
        cx: &mut TestAppContext,
        worktree_under_solution: bool,
    ) -> (
        gpui::WindowHandle<Workspace>,
        Entity<ConsolePanel>,
        SolutionId,
    ) {
        init_test(cx);

        let (solution_id, solution_root) = cx.update(|cx| {
            let store = SolutionStore::for_test(std::path::PathBuf::from("/cfg.json"), cx);
            let created = store.update(cx, |store, cx| {
                let id = store.create_for_test_minimal("ToggleFocusGuard", cx);
                let root = store
                    .solutions()
                    .iter()
                    .find(|sol| sol.id == id)
                    .map(|sol| sol.root.clone())
                    .expect("just-created solution");
                (id, root)
            });
            solutions::install_global_for_test(store, cx);
            created
        });

        let worktree = if worktree_under_solution {
            solution_root.clone()
        } else {
            std::path::PathBuf::from("/plain-folder")
        };

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&solution_root, serde_json::json!({})).await;
        if worktree != solution_root {
            fs.insert_tree(&worktree, serde_json::json!({})).await;
        }
        let project = Project::test(fs, [worktree.as_path()], cx).await;

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

        let window_handle = cx.add_window(|window, cx| Workspace::test_new(project, window, cx));
        let panel = window_handle
            .update(cx, |workspace, window, cx| {
                let panel = cx.new(|cx| ConsolePanel::new(workspace.weak_handle(), cx));
                let band = cx.new(|cx| {
                    SolutionBand::new(workspace.weak_handle(), workspace.project().clone(), cx)
                });
                workspace.set_solution_band_item(band.into(), window, cx);
                workspace.set_solution_band_utility_item(
                    UtilityKind::Terminal,
                    panel.clone().into(),
                    window,
                    cx,
                );
                panel
            })
            .unwrap();
        cx.run_until_parked();

        (window_handle, panel, solution_id)
    }

    fn band_of(
        window_handle: &gpui::WindowHandle<Workspace>,
        cx: &mut TestAppContext,
    ) -> Entity<SolutionBand> {
        window_handle
            .update(cx, |workspace, _window, _cx| {
                workspace
                    .solution_band_item()
                    .and_then(|item| item.downcast::<SolutionBand>().ok())
                    .expect("the band was installed by bootstrap_band_and_panel")
            })
            .unwrap()
    }

    /// Double-lease guard for `ctrl-\``, the sibling of
    /// `new_chat_action_does_not_double_lease_the_workspace` below.
    /// `handle_toggle_focus` runs under `workspace.register_action`'s mutable
    /// `Workspace` lease and now reaches all the way into
    /// `SolutionBand::toggle_utility_focus` → `utility_visible` →
    /// `solution_id`, which has to answer "which Solution is this?" before it
    /// can read or write the band's persisted geometry. Resolving that by
    /// upgrading the band's `WeakEntity<Workspace>` and reading it — the
    /// obvious implementation — re-acquires the SAME entity's lease and
    /// panics at runtime while compiling clean and passing every other unit
    /// test in this crate. The band therefore walks its `Entity<Project>`
    /// instead. `WindowHandle::update`'s closure leases the root view
    /// (`Workspace`) exactly the way the real action dispatch does, and the
    /// Solution set up here makes `solution_id` resolve to `Some`, so the
    /// call actually reaches the lookup that would double-lease.
    #[gpui::test]
    async fn toggle_focus_action_does_not_double_lease_the_workspace(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, _panel, solution_id) = bootstrap_band_and_panel(cx, true).await;
        let band = band_of(&window_handle, cx);

        assert!(
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "the utility section starts hidden"
        );

        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert!(
            band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "ctrl-` reveals the utility section"
        );
        cx.update(|cx| {
            assert!(
                SolutionAgentStore::global(cx)
                    .read(cx)
                    .band_state(solution_id)
                    .utility_visible,
                "the toggle must land on the OWNING Solution's band state, not the \
                 window-local fallback — otherwise the Solution lookup silently \
                 resolved to None and this test would no longer be exercising the \
                 double-lease path at all"
            );
        });
    }

    /// A plain-folder window that belongs to no Solution is a supported case
    /// in this fork, and `ctrl-\`` has to keep working there — that is the
    /// entire reason `SolutionBand` carries a window-local `BandState`
    /// fallback. Keyed on a `SolutionId` alone, this toggle would be a no-op.
    #[gpui::test]
    async fn toggle_focus_works_in_a_workspace_with_no_solution(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, _panel, solution_id) = bootstrap_band_and_panel(cx, false).await;
        let band = band_of(&window_handle, cx);

        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert!(
            band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "ctrl-` must reveal the terminal in a window with no Solution too"
        );
        cx.update(|cx| {
            assert!(
                !SolutionAgentStore::global(cx)
                    .read(cx)
                    .band_state(solution_id)
                    .utility_visible,
                "a Solution-less window must not write into some unrelated \
                 Solution's persisted band geometry"
            );
        });

        // Hiding again goes back through the same fallback rather than
        // sticking on `true` (the tri-state's second leg).
        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "a second ctrl-` on the focused section hides it again"
        );
    }

    /// Regression (phase 2b task 5 review, Critical 1): `utility_kind` is
    /// PERSISTED per Solution, and the debugger became its first non-terminal
    /// writer. If `ctrl-\`` only flipped `utility_visible`, a user who had
    /// opened the debugger and then closed the band would reopen it **on the
    /// debugger** while focus went to an unrendered `ConsolePanel` — leaving
    /// the terminal unreachable by its own keybinding for the rest of that
    /// Solution's life. The toggle must select `Terminal`, not merely reveal.
    #[gpui::test]
    async fn toggle_focus_selects_the_terminal_when_the_band_shows_another_kind(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let (window_handle, _panel, _solution_id) = bootstrap_band_and_panel(cx, true).await;
        let band = band_of(&window_handle, cx);

        // Stand in for the debugger having claimed the section.
        band.update(cx, |band, cx| {
            band.set_utility_kind(UtilityKind::Debug, cx);
            band.set_utility_visible(false, cx);
        });
        cx.run_until_parked();

        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(
            band.read_with(cx, |band, cx| band.utility_kind(cx)),
            UtilityKind::Terminal,
            "ctrl-` must point the utility section back at the terminal, not \
             reopen it on whatever the debugger left behind"
        );
        assert!(
            band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "and it must be visible"
        );
    }

    /// Same hazard on the non-focusing path: `RevealStrategy::Always` /
    /// `NoFocus` (task terminals, and the debugger's own DAP `runInTerminal`)
    /// go through `reveal_utility_section`, which must also select the kind —
    /// otherwise "reveal the terminal so the user can see the output" opens
    /// the band on the debugger instead.
    #[gpui::test]
    async fn revealing_a_task_terminal_selects_the_terminal_kind(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, _panel, _solution_id) = bootstrap_band_and_panel(cx, true).await;
        let band = band_of(&window_handle, cx);

        band.update(cx, |band, cx| {
            band.set_utility_kind(UtilityKind::Debug, cx);
            band.set_utility_visible(false, cx);
        });
        cx.run_until_parked();

        window_handle
            .update(cx, |workspace, _window, cx| {
                reveal_utility_section(workspace, UtilityKind::Terminal, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(
            band.read_with(cx, |band, cx| band.utility_kind(cx)),
            UtilityKind::Terminal,
            "a revealed task terminal must actually be the thing on screen"
        );
        assert!(band.read_with(cx, |band, cx| band.utility_visible(cx)));
    }

    /// Switching the band's utility content away from the occupant that holds
    /// focus does NOT strand focus on the panel that just stopped being
    /// rendered — and the band deliberately owns no focus logic to make that
    /// true. GPUI's `Window::draw` sees the rendered frame's focus path go
    /// empty (the focused handle is no longer in the dispatch tree) and fires
    /// the window's focus-lost listeners; `Workspace::new` registers one that
    /// re-focuses `Workspace::focus_handle`, which is `active_pane`'s handle.
    /// That is the same target `Workspace::focus_or_unfocus_panel` picked on
    /// the dock path this band replaced, so the transition already lands
    /// where the dock left it.
    ///
    /// Pinned because reading the band in isolation says the opposite —
    /// `activate_utility_kind` never touches focus, and `Window::focus`
    /// really does keep pointing at the departed handle for the rest of that
    /// frame — and the fix that reading suggests (a focus-tracking wrapper
    /// around the utility half so the band can release focus itself) would
    /// also start capturing focus on every click over an occupant that is not
    /// itself focusable, e.g. the git graph. Verified against a live editor
    /// too: typing into the band's terminal, clicking Git Graph, then typing
    /// again puts the characters in the centre pane's buffer.
    #[gpui::test]
    async fn switching_the_utility_kind_leaves_focus_on_the_centre_pane(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, _solution_id) = bootstrap_band_and_panel(cx, true).await;
        let band = band_of(&window_handle, cx);

        // `ctrl-\``: show the terminal occupant and focus it.
        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window_handle
            .update(cx, |_workspace, window, cx| {
                assert!(
                    panel.focus_handle(cx).is_focused(window),
                    "precondition: the band's terminal owns the window's focus"
                );
            })
            .unwrap();

        // What a click on the status bar's Git Graph button does. The kind has
        // no occupant registered in this test, so the half unmounts outright —
        // the worst case for the focus path.
        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::GitGraph, cx)
        });
        cx.run_until_parked();

        window_handle
            .update(cx, |workspace, window, cx| {
                assert!(
                    !panel.focus_handle(cx).is_focused(window),
                    "the terminal is gone from the frame; it must not still hold \
                     the window's focus"
                );
                assert!(
                    workspace.active_pane().focus_handle(cx).is_focused(window),
                    "and focus must have landed on the centre pane, so the next \
                     keystroke goes somewhere the user can see"
                );
            })
            .unwrap();
    }

    /// Regression: the empty-solution guard used to ask
    /// `project.worktrees()`, which COUNTS INVISIBLE worktrees — and
    /// `solutions_ui::open` opens an empty solution with its root as an
    /// *invisible* worktree (`OpenVisible::None`). So the guard passed for
    /// exactly the case it existed to block: "New Terminal" stayed enabled in
    /// a solution with no projects, with nowhere to `cd` into.
    ///
    /// The guard now asks the authoritative thing — the Solution's member list.
    #[gpui::test]
    async fn empty_solution_with_an_invisible_root_worktree_has_no_project(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (solution_id, solution_root) = cx.update(|cx| {
            let store = SolutionStore::for_test(std::path::PathBuf::from("/cfg.json"), cx);
            let out = store.update(cx, |store, cx| {
                let id = store.create_for_test_minimal("Empty", cx);
                let root = store
                    .solutions()
                    .iter()
                    .find(|sol| sol.id == id)
                    .map(|sol| sol.root.clone())
                    .expect("just-created solution");
                (id, root)
            });
            solutions::install_global_for_test(store, cx);
            out
        });
        let member_path = solution_root.join("proj");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&solution_root, serde_json::json!({"proj": {}}))
            .await;

        // A Project with NO visible worktree, plus the solution root as an
        // INVISIBLE one — precisely the shape `solutions_ui::open` builds for an
        // empty solution.
        let project = Project::test(fs, [] as [&std::path::Path; 0], cx).await;
        // Keep the handle alive: `WorktreeStore` holds worktrees weakly.
        let _invisible_worktree = project
            .update(cx, |project, cx| {
                project.create_worktree(&solution_root, false, cx)
            })
            .await
            .expect("invisible worktree");
        cx.run_until_parked();
        let window = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        cx.run_until_parked();

        window
            .update(cx, |workspace, _window, cx| {
                assert!(
                    workspace.project().read(cx).worktrees(cx).next().is_some(),
                    "precondition: the invisible root worktree is there — it is what the \
                     old `worktrees()` check tripped over"
                );
                assert!(
                    !workspace_has_project(workspace, cx),
                    "a Solution with zero members has no project to run a terminal in, \
                     however many invisible worktrees its workspace carries"
                );
            })
            .unwrap();

        // Give the Solution a member: the same workspace now hosts a project.
        cx.update(|cx| {
            let store = SolutionStore::global(cx);
            store.update(cx, |store, _| {
                store.test_add_member_with_path(solution_id, "proj", member_path.clone());
            });
        });
        window
            .update(cx, |workspace, _window, cx| {
                assert!(
                    workspace_has_project(workspace, cx),
                    "a Solution with a member project must allow a terminal"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    async fn workspace_has_project_gates_on_project_dirs(cx: &mut TestAppContext) {
        // The signal the terminal entry points (keyboard `NewTerminal`,
        // `handle_new_terminal`, and the "+" menu's disabled state) use to
        // block a terminal in an empty solution: no worktree => no project
        // directory => refuse; a worktree present => allow.
        cx.executor().allow_parking();

        let (empty_window, _empty_panel) = bootstrap_panel_with_worktrees(cx, &[]).await;
        empty_window
            .update(cx, |workspace, _window, cx| {
                assert!(
                    !workspace_has_project(workspace, cx),
                    "an empty solution (no worktrees) must report no project"
                );
            })
            .unwrap();

        let (window, _panel) = bootstrap_panel_with_worktrees(cx, &["/root"]).await;
        window
            .update(cx, |workspace, _window, cx| {
                assert!(
                    workspace_has_project(workspace, cx),
                    "a solution with a project worktree must report a project"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    async fn terminal_tab_scopes_by_origin_cwd(cx: &mut TestAppContext) {
        // A terminal must be scoped to the directory it was created in, not its
        // live working directory — the latter wanders with `cd` and goes
        // unreadable when the foreground process is another user's (`sudo su`),
        // which would drop the tab out of scope and make it vanish. Here we
        // assert the creation cwd is recorded as `origin_cwd`, which is what
        // `tab_scope` reads (see that function's `ConsoleTab::Terminal` arm).
        cx.executor().allow_parking();
        let (window_handle, panel) = bootstrap_panel(cx).await;

        let origin = std::env::temp_dir();
        window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |p, cx| {
                    p.add_terminal_tab(Some(origin.clone()), window, cx)
                });
            })
            .unwrap();
        cx.run_until_parked();

        panel.read_with(cx, |p, _cx| {
            assert_eq!(p.tabs.len(), 1);
            let ConsoleTab::Terminal { origin_cwd, .. } = &p.tabs[0];
            assert_eq!(
                origin_cwd.as_deref(),
                Some(origin.as_path()),
                "creation cwd must be recorded as origin_cwd"
            );
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
                .map(|t| {
                    let ConsoleTab::Terminal { view, .. } = t;
                    view.entity_id()
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
    async fn console_panel_for_workspace_finds_the_installed_panel(cx: &mut TestAppContext) {
        // `console_panel::NewTerminal` / `::NewChat` action handlers, plus
        // run-configuration output and the inline assistant, locate the panel
        // via `console_panel_for_workspace` (phase 2a task 6 — the panel is no
        // longer dock-registered, so `workspace.panel::<ConsolePanel>(cx)`
        // would find nothing). Verify the `zed.rs`-style install
        // (`set_solution_band_utility_item`) round-trips through that lookup,
        // so the action wiring isn't sabotaged at this seam. End-to-end action
        // dispatch needs a rendered workspace (GPUI attaches workspace
        // `register_action` handlers via the render div) — exercised live in
        // `docs/findings/2026-05-26-console-panel-shipped/`, not here.
        cx.executor().allow_parking();
        let (window_handle, panel) = bootstrap_panel(cx).await;

        window_handle
            .update(cx, |workspace, window, cx| {
                workspace.set_solution_band_utility_item(
                    UtilityKind::Terminal,
                    panel.clone().into(),
                    window,
                    cx,
                );
                assert!(
                    console_panel_for_workspace(workspace).is_some(),
                    "ConsolePanel should be retrievable via console_panel_for_workspace(workspace) \
                     after set_solution_band_utility_item"
                );
            })
            .unwrap();
    }
}
