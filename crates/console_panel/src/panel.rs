use anyhow::{Result, anyhow};
use collections::HashMap;
use futures::channel::oneshot;
use futures::future::join_all;
use gpui::{
    Action, Anchor, App, AppContext as _, AsyncApp, AsyncWindowContext, Context, DismissEvent,
    Entity, FocusHandle, Focusable, IntoElement, MouseButton, MouseDownEvent, Pixels, Point,
    Render, Subscription, Task, WeakEntity, Window, anchored, deferred,
};
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

/// Build the console panel's `+` menu. A free function rather than an inline
/// closure inside [`ConsolePanel::render_plus_popover`] so a paint test can
/// render exactly the menu the popover renders and read its `MENU_ITEM-*`
/// debug selectors — the entries themselves are what regressed (the panel
/// used to offer "New AI Chat" and "Reopen Closed Chat…" here, duplicating /
/// misplacing affordances that belong to the status-bar session tab strip),
/// and asserting a predicate instead of the paint would not have caught it.
///
/// AI-session entries deliberately do NOT belong here: chat creation lives on
/// `solution_agent::session_tab_strip`'s own `+`, which is where AI sessions
/// now live. This menu is terminal/task only.
fn build_plus_menu(
    panel: WeakEntity<ConsolePanel>,
    has_project: bool,
    active_path: Option<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _, _| {
        // New Terminal in the active project's folder. Disabled when there's
        // no active project (empty / no solution) — there's nowhere to run it.
        let label = if has_project {
            "New Terminal"
        } else {
            "New Terminal (no project)"
        };
        menu.item(
            ui::ContextMenuEntry::new(label)
                .disabled(!has_project)
                .handler(move |window, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |panel, cx| {
                            panel.add_terminal_tab(active_path.clone(), window, cx);
                        });
                    }
                }),
        )
        .separator()
        .action("Spawn Task…", zed_actions::Spawn::modal().boxed_clone())
    })
}

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
    /// Whether the Solution band was last seen handing its utility half to
    /// this panel. Edge memory for [`Self::on_band_state_changed`] — the
    /// band's stand-in for upstream's `Panel::set_active` flag.
    band_showed_this_panel: bool,
    /// True while [`Self::load`]'s detached [`Self::restore_from_db`] is still
    /// running, i.e. while `tabs.is_empty()` does not yet mean "this panel has
    /// no terminal". Read only by [`Self::autostart_terminal_if_empty`].
    restore_in_flight: bool,
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
            band_showed_this_panel: false,
            restore_in_flight: false,
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
        // Arm the auto-start edge before the restore below is spawned, and
        // mark that restore in flight, so an edge that arrives while
        // persisted tabs are still on their way in cannot read the
        // momentarily-empty `tabs` as "this panel has no terminal" and spawn
        // a duplicate shell.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.restore_in_flight = true;
            panel.observe_band_state(window, cx);
        })?;

        {
            let workspace = workspace.clone();
            let panel = panel.clone();
            cx.spawn(async move |cx: &mut AsyncWindowContext| {
                Self::restore_from_db(workspace, panel.clone(), cx)
                    .await
                    .log_err();
                panel
                    .update_in(cx, |panel, window, cx| {
                        panel.restore_in_flight = false;
                        // The one level-triggered check in the whole feature,
                        // and it is the boot case rather than a shortcut:
                        // `observe_band_state` above recorded the band's
                        // state before any tab could exist, so a window that
                        // boots with the terminal half ALREADY open never
                        // produces a hidden→visible edge to react to.
                        panel.autostart_terminal_if_empty(window, cx);
                    })
                    .log_err();
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
        // Focus that stopped on the panel's own handle is a mis-aimed focus:
        // the handle belongs to a container with no key handling, so the
        // keystroke after `ctrl-\`` used to go nowhere. Hand it down to the
        // terminal the strip is showing.
        if self.focus_handle.is_focused(window) {
            self.focus_active_terminal(window, cx);
        }

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
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.close_tab(ix, window, cx)),
                        ),
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
        // New terminals open in the active project's folder (the project
        // selected in the project tab strip).
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
                    Some(build_plus_menu(
                        weak_self.clone(),
                        has_project,
                        active_path.clone(),
                        window,
                        cx,
                    ))
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
        // Counted for the same reason `add_terminal_task` counts it: between
        // this call and the spawned view landing, `self.tabs` is still empty,
        // and `autostart_terminal_if_empty` reads exactly that pair to decide
        // whether the panel already has a terminal (upstream's
        // `TerminalPanel::has_no_terminals`). Without the counter, two band
        // edges inside one spawn's round trip start two shells.
        self.pending_terminals_to_add += 1;
        cx.spawn(async move |this, cx| {
            let view = task.await;
            this.update(cx, |this, cx| {
                this.pending_terminals_to_add = this.pending_terminals_to_add.saturating_sub(1);
                if let Ok(view) = &view {
                    this.tabs.push(ConsoleTab::Terminal {
                        view: view.clone(),
                        origin_cwd,
                    });
                    this.active_index = Some(this.tabs.len() - 1);
                    cx.notify();
                    this.persist(cx);
                }
            })?;
            view.map(|_| ())
        })
        .detach_and_log_err(cx);
    }

    /// The `SolutionBand` installed in this panel's window, if any. `None` in
    /// a headless or test workspace that never installed one.
    fn solution_band(&self, cx: &App) -> Option<Entity<SolutionBand>> {
        let workspace = self.workspace.upgrade()?;
        workspace
            .read(cx)
            .solution_band_item()?
            .downcast::<SolutionBand>()
            .ok()
    }

    /// Whether the Solution band is currently handing its utility half to
    /// THIS panel: the half is visible and its content kind is `Terminal`.
    ///
    /// Read from wherever the band actually keeps that state, which depends
    /// on the window. A Solution window's band writes through to
    /// `SolutionAgentStore` (that is what makes the geometry persist per
    /// Solution); a plain-folder window has no Solution to key a store row
    /// on, so `SolutionBand` keeps the identical `BandState` in its own
    /// `local_state` field. Asking the band directly in that case is not a
    /// second source of truth — it is the only one that window has, and
    /// `ctrl-\`` there deserves the same non-empty terminal half.
    fn band_shows_this_panel(&self, cx: &App) -> bool {
        let state = match self.active_solution_id(cx) {
            Some(solution_id) => {
                let Some(store) = SolutionAgentStore::try_global(cx) else {
                    return false;
                };
                store.read(cx).band_state(solution_id)
            }
            None => {
                let Some(band) = self.solution_band(cx) else {
                    return false;
                };
                band.read(cx).band_state(cx)
            }
        };
        state.utility_visible && state.utility_kind == UtilityKind::Terminal
    }

    /// Arm the auto-start edge.
    ///
    /// Upstream spawns a shell into an empty terminal dock from
    /// `TerminalPanel::set_active` (`terminal_view::terminal_panel`), on the
    /// `false → true` edge of the `workspace::Panel` activity flag.
    /// `ConsolePanel` deliberately implements no `Panel` — it is a Solution
    /// band occupant, not a dock panel — so nothing ever calls `set_active`
    /// and the behaviour was silently dropped in the port. The band's
    /// equivalent flag is `SolutionAgentStore::band_state`, and its
    /// equivalent edge source is `BandStateChanged`, which covers all three
    /// ways the half opens: `ctrl-\``, the status-bar Terminal button, and
    /// the hydration that restores a window whose band was already open.
    ///
    /// Two sources are armed, not one, because `BandStateChanged` describes
    /// only Solution windows — see the comment on the observation below.
    ///
    /// Called from [`Self::load`] rather than from `new` because the
    /// subscription needs a `Window` (constructing a `TerminalView` does) and
    /// `load` already runs inside a `workspace.update_in` that has one, while
    /// `new`'s signature stays untouched for this crate's — and
    /// `run_config_ui`'s — test constructors.
    pub fn observe_band_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = SolutionAgentStore::try_global(cx) else {
            // Not fatal — a window with no agent store has no band state to
            // watch — but it silently costs this panel its auto-start for the
            // window's whole life, so it must not be invisible. In the app the
            // store is installed before any panel loads; reaching this in a
            // real session means the install order regressed.
            log::warn!(
                "console panel: no SolutionAgentStore global; the band's \
                 terminal auto-start is disabled for this window"
            );
            return;
        };
        // Deferred, not read inline: `band_shows_this_panel` resolves the
        // active Solution by reading the `Workspace` through this panel's weak
        // handle, and every caller that can arm this — `load`'s
        // `workspace.update_in`, a test's `WindowHandle::update` — may hold
        // that same entity leased. A read under a lease aborts the process
        // (`entity_map.rs:164`), so take the snapshot once the lease is gone.
        cx.defer_in(window, |this, window, cx| {
            this.band_showed_this_panel = this.band_shows_this_panel(cx);
            // Second edge source, for the window `BandStateChanged` cannot
            // describe. A plain-folder window has no Solution, so
            // `SolutionBand::set_utility_visible` takes its `None` arm: it
            // writes `local_state` and calls `cx.notify()`, and the store —
            // and therefore the event above — never hears about it. That
            // notify is the honest edge source for such a window, because the
            // band entity IS that window's band state. Observing it runs the
            // very same check, so no second code path decides anything.
            //
            // Both sources are live in a Solution window (the band re-notifies
            // when it sees `BandStateChanged`), and that is harmless:
            // `on_band_state_changed` is edge-guarded by
            // `band_showed_this_panel`, so whichever arrives first takes the
            // false → true edge and the other sees none. Observing rather than
            // checking in `render` keeps the "never level-triggered" property
            // that stops the panel resurrecting the terminal a user just
            // closed — a notify with `shows == showed` does nothing.
            //
            // Resolved here rather than at the top of this method because
            // finding the band means reading the `Workspace`, and every caller
            // that can arm this holds that entity leased.
            if let Some(band) = this.solution_band(cx) {
                let subscription = cx.observe_in(&band, window, |this, _band, window, cx| {
                    this.on_band_state_changed(window, cx);
                });
                this._subscriptions.push(subscription);
            }
        });
        let subscription = cx.subscribe_in(&store, window, |this, _store, event, window, cx| {
            // Not filtered on the event's `solution_id`: the state is
            // re-derived for THIS panel's Solution either way, so another
            // Solution's band change reproduces the same answer and yields no
            // edge.
            if matches!(
                event,
                solution_agent::store::SolutionAgentStoreEvent::BandStateChanged { .. }
            ) {
                this.on_band_state_changed(window, cx);
            }
        });
        self._subscriptions.push(subscription);
    }

    fn on_band_state_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shows = self.band_shows_this_panel(cx);
        let showed = std::mem::replace(&mut self.band_showed_this_panel, shows);
        if shows && !showed {
            self.autostart_terminal_if_empty(window, cx);
        }
    }

    /// Start a shell so the band's terminal half is never handed to the user
    /// empty — «в панели терминала должна сразу сессия запускаться если её
    /// нету».
    ///
    /// Edge-driven only ([`Self::on_band_state_changed`], plus the one
    /// boot-time call at the end of restore); never called from `render`. A
    /// render-time check would be level-triggered, and the panel stays
    /// mounted after the user closes its last tab, so it would instantly
    /// respawn the terminal the user just deliberately closed.
    ///
    /// The guards:
    /// * `band_shows_this_panel` — the panel is only auto-populated when the
    ///   user can actually see it. Redundant on the edge path (the edge is
    ///   defined by it) and load-bearing on the boot path, which is a level
    ///   check: without it, every workspace whose console panel restores no
    ///   tabs starts a shell nobody asked for and nobody can see. Caught by
    ///   `debugger_ui`'s suite, which spawned real PTYs and lost its
    ///   determinism.
    /// * `tabs.is_empty() && pending_terminals_to_add == 0` is upstream's
    ///   `TerminalPanel::has_no_terminals`; the counter is what makes two
    ///   edges arriving inside one spawn's round trip idempotent.
    /// * `workspace_has_project` is this fork's addition: an empty Solution
    ///   has nowhere to `cd` into, which is exactly why the "+" menu greys
    ///   "New Terminal" out there. Auto-start must not do what the menu
    ///   forbids.
    /// * `restore_in_flight` — see the field.
    ///
    /// Focus is deliberately untouched: this only appends a tab. When the
    /// half was opened by `ctrl-\`` focus is already on the panel's own
    /// handle and `render`'s redirect hands it down to the new terminal;
    /// when it was opened by the status-bar button focus stays wherever the
    /// user left it.
    fn autostart_terminal_if_empty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.restore_in_flight || !self.tabs.is_empty() || self.pending_terminals_to_add > 0 {
            return;
        }
        if !self.band_shows_this_panel(cx) {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if !workspace_has_project(workspace.read(cx), cx) {
            return;
        }
        // The same cwd the "+" menu's "New Terminal" entry uses.
        let cwd = self.active_member_path(cx);
        self.add_terminal_tab(cwd, window, cx);
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
            // The body is an inner block so that the decrement below is
            // sequenced AFTER it on every exit, `?` bailouts included (remote
            // project, a failed `create_terminal_task`, a window that went
            // away mid-spawn). Decrementing at the end of the happy path only
            // — as this did — leaks the counter on those three paths, and the
            // counter is half of `autostart_terminal_if_empty`'s "does this
            // panel already have a terminal on the way" answer: one leaked
            // increment silently disables the band's terminal auto-start for
            // the rest of the window's life. Ordering matters the other way
            // too, which is why this is not a guard that fires first: while
            // the body runs, `tabs` is still empty and the counter is the only
            // thing standing between a second band edge and a second shell.
            let result: Result<WeakEntity<Terminal>> = {
                let this = &this;
                let cx = &mut *cx;
                async move {
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
                        cx.notify();
                        this.persist(cx);
                    })?;
                    Ok(terminal.downgrade())
                }
                .await
            };
            // An `Err` here means the panel itself is gone, so there is no
            // counter left to correct — not a failure to report.
            this.update(cx, |this, _cx| {
                this.pending_terminals_to_add = this.pending_terminals_to_add.saturating_sub(1);
            })
            .ok();
            result
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
            menu.entry("Close", None, move |window, cx| {
                if let Some(this) = weak_close.upgrade() {
                    this.update(cx, |this, cx| this.close_tab(tab_index, window, cx));
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

    /// Hand focus that landed on the panel itself to the terminal the strip
    /// is actually showing, so `ctrl-\`` (and every other caller that focuses
    /// this panel: `RevealStrategy::Always` task terminals, a click on the tab
    /// strip) leaves the caret where the user can type.
    ///
    /// Driven from `render` — a state check re-evaluated every frame — and
    /// not from the `cx.on_focus_in` subscription `DebugPanel` uses, because
    /// `Window::on_focus_in` is EDGE-triggered: `FocusEvent::is_focus_in` is
    /// `!previous_focus_path.contains(id) && current_focus_path.contains(id)`.
    /// While the terminal holds focus, the panel's handle is its ANCESTOR and
    /// is therefore already in the previous path, so moving focus from the
    /// terminal *up* to the panel root — which is exactly what a click on the
    /// tab strip does — never crosses that edge. No listener would run, and
    /// focus would strand on a container with no key handling, reproducing
    /// the very bug this exists to prevent. A state-based redirect catches
    /// that transition; an edge-triggered subscription cannot.
    ///
    /// The constructor's signature is not the obstacle, despite appearances:
    /// `ConsolePanel::load` already sits inside a `workspace.update_in` that
    /// hands it a `&mut Window` it discards, so a subscription installed in
    /// `new` could be armed long before the first `ctrl-\``. Do not "restore"
    /// the subscription on that basis — the tab-strip case above is what
    /// rules it out, and it regresses silently (a `ctrl-\`` test still passes,
    /// because that path *is* a genuine focus-in).
    ///
    /// This must stay a redirect rather than the shorter-looking fix of
    /// returning the terminal's handle from [`Focusable::focus_handle`]: the
    /// band's `toggle_utility_focus` tri-state asks
    /// `focus_handle.contains_focused`, which is only true of an ANCESTOR of
    /// the focused handle — handing out the terminal's own handle would make
    /// the "visible but unfocused" and "visible and focused" legs
    /// indistinguishable.
    ///
    /// The index has to come from `effective_active_index`, the same
    /// scope-filtered choice `render_active_tab` paints: `active_index` alone
    /// can point at a tab belonging to another member, whose view is not in
    /// the frame at all, and focusing a handle outside the rendered dispatch
    /// tree strands focus (`Workspace`'s focus-lost listener then yanks it to
    /// the centre pane).
    fn focus_active_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scope_flags = self.tab_scope_flags(cx);
        let Some(index) = effective_active_index(&scope_flags, self.active_index) else {
            return;
        };
        let Some(ConsoleTab::Terminal { view, .. }) = self.tabs.get(index) else {
            return;
        };
        view.focus_handle(cx).focus(window, cx);
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

    /// Closing the tab that holds focus has to re-home focus explicitly. The
    /// closed `TerminalView`'s handle leaves the dispatch tree, so the
    /// window's focus points at a dead id, [`focus_active_terminal`]'s
    /// `is_focused` guard is false and the redirect cannot fire — and
    /// `Workspace`'s focus-lost listener then yanks focus out to the centre
    /// pane, so the user's next keystroke edits a buffer instead of a
    /// console.
    ///
    /// Focus goes to the panel's own handle rather than straight to the
    /// surviving terminal so the redirect stays the single place that decides
    /// *which* tab receives focus (it applies the same scope filter
    /// `render_active_tab` paints with). When the closed tab was the last
    /// one there is nothing to redirect to, and focus deliberately rests on
    /// the panel root: it is still in the frame (the tab strip and its "new
    /// terminal" button survive an empty panel) and still carries the
    /// `ConsolePanel` key context, so a stray keystroke is absorbed by the
    /// console instead of silently editing whatever buffer the centre pane
    /// happens to show.
    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ConsoleTab::Terminal { view, .. }) = self.tabs.get(index) else {
            return;
        };
        let closed_tab_held_focus = view.focus_handle(cx).contains_focused(window, cx);
        self.tabs.remove(index);
        if closed_tab_held_focus {
            self.focus_handle.focus(window, cx);
        }
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
    use crate::actions::NewChat;
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

    /// Window root that paints exactly the `ContextMenu`
    /// [`ConsolePanel::render_plus_popover`] builds, so `debug_bounds` can
    /// read its `MENU_ITEM-*` entries. The popover itself is not driven here
    /// (its trigger needs a laid-out panel inside a dock); the menu is the
    /// thing under test and `build_plus_menu` is the single source both use.
    struct PlusMenuPaintHarness {
        menu: Entity<ContextMenu>,
    }

    impl Render for PlusMenuPaintHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().child(self.menu.clone())
        }
    }

    /// The console panel's `+` is terminals and tasks only. "New AI Chat" was
    /// pure duplication of the status-bar session strip's own `+`, and
    /// "Reopen Closed Chat…" moved onto that strip with the sessions it
    /// recovers (`solution_agent::session_tab_strip::build_plus_menu`).
    /// Asserted on the painted tree rather than on the builder's return
    /// value: an entry that stops painting for some other reason is the same
    /// bug to the user.
    #[gpui::test]
    async fn the_plus_menu_offers_terminals_and_tasks_but_no_ai_sessions(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (_window_handle, panel) = bootstrap_panel(cx).await;
        let weak_panel = panel.downgrade();

        let (_harness, cx) = cx.add_window_view(|window, cx| PlusMenuPaintHarness {
            menu: build_plus_menu(weak_panel, true, None, window, cx),
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("MENU_ITEM-New Terminal").is_some(),
            "the `+` menu must still create a terminal"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Spawn Task…").is_some(),
            "the `+` menu must still spawn a task"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-New AI Chat").is_none(),
            "AI chat creation belongs to the status-bar session tab strip's `+`"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Reopen Closed Chat…").is_none(),
            "reopening a closed chat moved to the status-bar session tab strip's `+`"
        );
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
                // `ConsolePanel::load` arms this in production; without it the
                // band's terminal half would never auto-start a shell here and
                // the tests below would pass against a panel that is not the
                // one the app ships.
                panel.update(cx, |panel, cx| panel.observe_band_state(window, cx));
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

    /// Give the bootstrapped Solution a member project rooted at the
    /// Solution's own root, so `workspace_has_project` — the auto-start's
    /// gate, and the "+" menu's — is satisfied.
    ///
    /// `create_for_test_minimal` builds a MEMBERLESS Solution, which is why
    /// the band tests that don't call this never auto-start a terminal: they
    /// model a Solution with nowhere to `cd` into.
    fn give_the_solution_a_member(cx: &mut TestAppContext, solution_id: SolutionId) {
        cx.update(|cx| {
            SolutionStore::global(cx).update(cx, |store, _| {
                let root = store
                    .find_solution(solution_id)
                    .expect("bootstrapped solution")
                    .root
                    .clone();
                store.test_add_member_with_path(solution_id, "proj", root);
            });
        });
    }

    /// The reported defect: opening the band's terminal half while it holds
    /// no terminal left the user staring at an empty panel — «в панели
    /// терминала должна сразу сессия запускаться если её нету».
    ///
    /// Upstream spawns one on the `false → true` edge of
    /// `TerminalPanel::set_active`; `ConsolePanel` implements no
    /// `workspace::Panel` (it is a band occupant), so nothing called
    /// `set_active` and the behaviour was lost in the port. The edge is
    /// re-derived from `SolutionAgentStore`'s band state instead.
    ///
    /// Driven through `activate_utility_kind` — what the status-bar Terminal
    /// button does — rather than `handle_toggle_focus`, so the focus
    /// assertion cannot pass by accident: this path never focuses the panel,
    /// so anything that moves focus off the centre pane came from the
    /// auto-start itself.
    #[gpui::test]
    async fn showing_an_empty_terminal_half_starts_a_shell(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, solution_id) = bootstrap_band_and_panel(cx, true).await;
        give_the_solution_a_member(cx, solution_id);
        let band = band_of(&window_handle, cx);

        panel.read_with(cx, |panel, _| {
            assert!(
                panel.tabs.is_empty(),
                "precondition: the panel starts with no terminal"
            );
        });
        assert!(
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "precondition: the band's utility half starts hidden"
        );

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::Terminal, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.tabs.len(),
                1,
                "showing the terminal half with nothing in it must start a shell"
            );
            assert_eq!(
                panel.active_index,
                Some(0),
                "and the auto-started shell must be the active tab, or the \
                 panel renders empty anyway"
            );
        });
        window_handle
            .update(cx, |workspace, window, cx| {
                assert!(
                    workspace.active_pane().focus_handle(cx).is_focused(window),
                    "the auto-start must not yank focus off the centre pane: \
                     the user clicked a status-bar button, they did not ask to \
                     start typing in a shell"
                );
                assert!(
                    !panel.focus_handle(cx).contains_focused(window, cx),
                    "and focus must not have moved into the console either"
                );
            })
            .unwrap();
    }

    /// The other side of the auto-start: a terminal half that already holds a
    /// terminal must be handed back untouched, however many times it is
    /// hidden and shown. Without this, every `ctrl-\`` would pile up another
    /// shell.
    #[gpui::test]
    async fn showing_a_populated_terminal_half_starts_nothing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, solution_id) = bootstrap_band_and_panel(cx, true).await;
        give_the_solution_a_member(cx, solution_id);
        let band = band_of(&window_handle, cx);

        window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| panel.add_terminal_tab(None, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.tabs.len(), 1, "precondition: one terminal is open");
        });

        for cycle in 0..2 {
            band.update(cx, |band, cx| {
                band.activate_utility_kind(UtilityKind::Terminal, cx)
            });
            cx.run_until_parked();
            assert!(
                band.read_with(cx, |band, cx| band.utility_visible(cx)),
                "cycle {cycle}: the half is showing"
            );
            panel.read_with(cx, |panel, _| {
                assert_eq!(
                    panel.tabs.len(),
                    1,
                    "cycle {cycle}: showing a half that already has a terminal \
                     must not start another one"
                );
            });
            // Toggle it back off; `activate_utility_kind` on the already-shown
            // kind hides the half, which re-arms the edge for the next pass.
            band.update(cx, |band, cx| {
                band.activate_utility_kind(UtilityKind::Terminal, cx)
            });
            cx.run_until_parked();
        }
    }

    /// The boot path — the call [`ConsolePanel::load`] makes once its restore
    /// settles — is the one LEVEL-triggered entry point, because a window that
    /// boots with the terminal half already open never produces a
    /// hidden→visible edge to react to. Level-triggered means it has to ask
    /// whether the half is actually open: an early draft did not, so EVERY
    /// workspace whose console panel restored no tabs started an invisible
    /// shell at startup. `debugger_ui`'s suite is what caught it — six of its
    /// tests lost determinism to the real PTYs that appeared.
    ///
    /// Deferred rather than called inline for the same reason
    /// `observe_band_state` defers its snapshot: this reads the `Workspace`
    /// through the panel's weak handle, and `WindowHandle::update` holds that
    /// entity leased. Production reaches it from an `AsyncWindowContext`,
    /// where nothing is leased.
    #[gpui::test]
    async fn the_boot_check_starts_nothing_while_the_half_is_hidden(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, solution_id) = bootstrap_band_and_panel(cx, true).await;
        give_the_solution_a_member(cx, solution_id);
        let band = band_of(&window_handle, cx);
        assert!(
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "precondition: the band's utility half is hidden"
        );

        window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |_panel, cx| {
                    cx.defer_in(window, |panel, window, cx| {
                        panel.autostart_terminal_if_empty(window, cx);
                    });
                });
            })
            .unwrap();
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(
                panel.tabs.is_empty(),
                "a console panel the user cannot see must not be handed a shell"
            );
        });
    }

    /// Two edges arriving inside one spawn's round trip must still produce
    /// one shell. `add_terminal_tab` only appends to `self.tabs` once its
    /// async `new_tab` resolves, so between the first edge and that
    /// resolution `tabs.is_empty()` is still true — which is exactly why
    /// `pending_terminals_to_add` is part of the "has no terminal" answer
    /// (upstream's `TerminalPanel::has_no_terminals` reads the same pair).
    ///
    /// Deliberately no `run_until_parked` between the toggles: parking is
    /// what would let the first spawn land and hide the bug.
    #[gpui::test]
    async fn a_second_edge_mid_spawn_does_not_start_a_second_shell(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, solution_id) = bootstrap_band_and_panel(cx, true).await;
        give_the_solution_a_member(cx, solution_id);
        let band = band_of(&window_handle, cx);

        for _ in 0..3 {
            // show, hide, show — two rising edges, no parking in between.
            band.update(cx, |band, cx| {
                band.activate_utility_kind(UtilityKind::Terminal, cx)
            });
        }
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.pending_terminals_to_add, 1,
                "precondition: the first edge's spawn is still in flight, so \
                 `tabs` alone cannot answer \"does this panel have a terminal\""
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.tabs.len(),
                1,
                "a second edge while the first shell is still spawning must not \
                 start another one"
            );
        });
    }

    /// A `SpawnInTerminal` whose command is an absolute path to no program,
    /// so `Project::create_terminal_task` fails instead of handing back a
    /// terminal: `spawn_task.command` becomes the PTY's program, and the
    /// exec of a nonexistent one fails in `TerminalBuilder::new`.
    ///
    /// A nonexistent `cwd` was tried first and does NOT work — the terminal
    /// falls back to another directory rather than erroring — so this must
    /// stay keyed on the program, and the `outcome.is_err()` precondition
    /// below is what stops a future change to that fallback from quietly
    /// turning this into a happy-path test.
    fn a_task_that_cannot_spawn() -> SpawnInTerminal {
        SpawnInTerminal {
            id: TaskId("console-panel-unspawnable".to_string()),
            label: "unspawnable".to_string(),
            full_label: "unspawnable".to_string(),
            command: Some(
                "/nonexistent-console-panel-program-for-the-failed-spawn-test".to_string(),
            ),
            ..SpawnInTerminal::default()
        }
    }

    /// `pending_terminals_to_add` is not a statistic — it is half of
    /// `autostart_terminal_if_empty`'s "does this panel already have a
    /// terminal on the way" answer. So an increment that leaks on an error
    /// path does not merely skew a number: it permanently convinces the panel
    /// that a shell is coming, and the band's terminal half is handed to the
    /// user empty for the rest of the window's life, with no log line to say
    /// why. The three ways `add_terminal_task` can bail (remote project,
    /// failed `create_terminal_task`, window gone mid-spawn) all used to skip
    /// the decrement, because it sat at the end of the happy path.
    ///
    /// Asserted through the user-visible consequence — a later band edge must
    /// still start a shell — and not only on the counter, because the counter
    /// is an implementation detail and the auto-start is the promise.
    #[gpui::test]
    async fn a_failed_task_spawn_does_not_leak_the_pending_counter(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, solution_id) = bootstrap_band_and_panel(cx, true).await;
        give_the_solution_a_member(cx, solution_id);
        let band = band_of(&window_handle, cx);

        let spawn = window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.add_terminal_task(
                        a_task_that_cannot_spawn(),
                        RevealStrategy::Never,
                        window,
                        cx,
                    )
                })
            })
            .unwrap();
        let outcome = spawn.await;
        cx.run_until_parked();

        assert!(
            outcome.is_err(),
            "precondition: the task spawn has to actually fail, or this test \
             is exercising the happy path it was written to avoid"
        );
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.tabs.is_empty(),
                "precondition: a failed spawn adds no tab"
            );
            assert_eq!(
                panel.pending_terminals_to_add, 0,
                "the increment taken before the spawn must come back on the \
                 error path too"
            );
        });

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::Terminal, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.tabs.len(),
                1,
                "a leaked counter reads as \"a terminal is already on the \
                 way\" forever, so opening the empty terminal half after a \
                 failed task run would silently start nothing"
            );
        });
    }

    /// A window that belongs to no Solution keeps its band state in
    /// `SolutionBand::local_state` rather than in `SolutionAgentStore`, so it
    /// produces no `BandStateChanged` — and until the band entity itself was
    /// observed, `ctrl-\`` there opened the terminal half empty, which is the
    /// exact defect the auto-start exists to fix, just in the other kind of
    /// window. The band's `cx.notify()` is that window's honest edge source;
    /// this pins that it is wired to the same edge check.
    #[gpui::test]
    async fn showing_an_empty_terminal_half_starts_a_shell_without_a_solution(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        // `false`: the workspace's worktree lives OUTSIDE any Solution root,
        // so `active_solution_id` is `None` and the band falls back to
        // `local_state` — the case under test. No `give_the_solution_a_member`
        // either: with no Solution, `workspace_has_project` is satisfied by
        // the window's own visible worktree.
        let (window_handle, panel, _solution_id) = bootstrap_band_and_panel(cx, false).await;
        let band = band_of(&window_handle, cx);

        panel.read_with(cx, |panel, cx| {
            assert!(
                panel.active_solution_id(cx).is_none(),
                "precondition: this window belongs to no Solution, or the \
                 store path would answer and the fallback would go untested"
            );
            assert!(
                panel.tabs.is_empty(),
                "precondition: the panel starts with no terminal"
            );
        });

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::Terminal, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.tabs.len(),
                1,
                "a plain-folder window's terminal half must not be handed over \
                 empty either"
            );
            assert_eq!(panel.active_index, Some(0));
        });

        // The other half of the edge: the fallback must be edge-triggered too,
        // not level-triggered on every band notify, or hiding and re-showing
        // would pile shells up in exactly the window that has no store row to
        // deduplicate against.
        for _ in 0..2 {
            band.update(cx, |band, cx| {
                band.activate_utility_kind(UtilityKind::Terminal, cx)
            });
            cx.run_until_parked();
        }
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.tabs.len(),
                1,
                "hiding and re-showing a half that already holds a terminal \
                 must not start another one"
            );
        });
    }

    /// Why the auto-start is edge-driven and not a check in `render`: the
    /// panel stays mounted after its last tab closes, so a level-triggered
    /// check would instantly resurrect the terminal the user just closed and
    /// the close button would look broken.
    #[gpui::test]
    async fn closing_the_last_terminal_does_not_respawn_one(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, solution_id) = bootstrap_band_and_panel(cx, true).await;
        give_the_solution_a_member(cx, solution_id);
        let band = band_of(&window_handle, cx);

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::Terminal, cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.tabs.len(),
                1,
                "precondition: the half auto-started one"
            );
        });

        window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| panel.close_tab(0, window, cx));
            })
            .unwrap();
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(
                panel.tabs.is_empty(),
                "closing the last terminal must leave the panel empty — the \
                 half is still visible, so a level-triggered auto-start would \
                 have put a new shell right back"
            );
            assert_eq!(panel.active_index, None);
        });
    }

    /// The auto-start obeys the same empty-Solution gate as the "+" menu's
    /// "New Terminal" entry: a Solution with no member project has nowhere to
    /// `cd` into, so showing its terminal half must start nothing rather than
    /// spawn a shell in an arbitrary directory.
    #[gpui::test]
    async fn the_auto_start_respects_the_empty_solution_gate(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, _solution_id) = bootstrap_band_and_panel(cx, true).await;
        let band = band_of(&window_handle, cx);
        window_handle
            .update(cx, |workspace, _window, cx| {
                assert!(
                    !workspace_has_project(workspace, cx),
                    "precondition: the bootstrapped Solution has no members"
                );
            })
            .unwrap();

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::Terminal, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert!(
                panel.tabs.is_empty(),
                "a Solution with no project must not get an auto-started shell"
            );
        });
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

    /// Same hazard on the spawn path: a task terminal with
    /// `RevealStrategy::Always` (which reveals AND focuses) or
    /// `RevealStrategy::NoFocus` (reveals only) goes through
    /// `reveal_utility_section`, which must also select the kind — otherwise
    /// "reveal the terminal so the user can see the output" opens the band on
    /// the debugger instead. The debugger's own DAP `runInTerminal` is NOT on
    /// this path: `debugger_ui::session::running::handle_run_in_terminal`
    /// builds its own `TerminalView` and installs it via
    /// `ensure_pane_item(DebuggerPaneItem::Terminal)`, so it creates no
    /// `ConsoleTab`. It reveals through the mirror-image helper on the
    /// debugger's side — `debugger_panel::reveal_debug_panel`, selecting
    /// `UtilityKind::Debug` — so the two never fight over the section.
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

    /// `ctrl-\`` has to leave the caret in the terminal, not on the panel's
    /// own handle — which is a container with no key handling, so every
    /// keystroke after the hotkey went nowhere until the user clicked into
    /// the terminal.
    ///
    /// The last assertion is the one that speaks about keystrokes rather than
    /// about focus bookkeeping: `Window::available_actions` is computed from
    /// the dispatch path of the focused node in the RENDERED frame, so a
    /// terminal action showing up there means a key event dispatched now
    /// would traverse the terminal's element. With focus resting on the
    /// panel root (the bug) that path stops above the terminal and no
    /// terminal action is reachable.
    #[gpui::test]
    async fn toggle_focus_lands_in_the_active_terminal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, _solution_id) = bootstrap_band_and_panel(cx, true).await;
        let band = band_of(&window_handle, cx);

        window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| panel.add_terminal_tab(None, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        let terminal = panel
            .read_with(cx, |panel, cx| panel.active_terminal_view(cx))
            .expect("the panel has one terminal tab");

        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        window_handle
            .update(cx, |_workspace, window, cx| {
                assert!(
                    terminal.focus_handle(cx).is_focused(window),
                    "ctrl-` must focus the terminal itself, not the panel around it"
                );
                // Non-regression guard only: this stays true whether or not
                // the redirect ran (without it focus rests ON the panel's own
                // handle, which `contains_focused` also reports). It does NOT
                // prove the band's tri-state survives the redirect — the
                // second `handle_toggle_focus` at the end of this test does.
                assert!(
                    panel.focus_handle(cx).contains_focused(window, cx),
                    "and the panel must still count as focused, or the band's \
                     tri-state loses its 'visible and focused' leg"
                );
                // `terminal::RerunTask` is registered on `TerminalView`'s own
                // element and nowhere else in the tree, so reachability is a
                // true discriminator. `Window::is_action_available` walks only
                // the dispatch path to the focused node; `available_actions`
                // would also union in every GLOBAL action listener, and a
                // `starts_with("terminal")` prefix over that set is vacuous —
                // `terminal_panel::{Toggle,ToggleFocus}` are registered on
                // `Workspace` itself and their names match that prefix.
                assert!(
                    window.is_action_available(&terminal_view::RerunTask, cx),
                    "a keystroke dispatched now must travel through the \
                     terminal's element"
                );
            })
            .unwrap();

        // Tri-state, third leg: the section is visible AND focused, so the
        // next ctrl-` hides it. That decision reads `contains_focused` on the
        // panel's handle, which the redirect above has to keep true.
        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            !band.read_with(cx, |band, cx| band.utility_visible(cx)),
            "a second ctrl-` on the focused terminal still hides the section"
        );
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
                // The panel is still empty here, and deliberately so: showing
                // the terminal half now auto-starts a shell
                // (`showing_an_empty_terminal_half_starts_a_shell`), but
                // `bootstrap_band_and_panel` builds a MEMBERLESS Solution, so
                // the auto-start's empty-Solution gate blocks it — the same
                // gate that greys out the "+" menu's "New Terminal". Asserted
                // rather than assumed, because the whole point of this test is
                // that focus starts on the panel's own handle: with a tab
                // present the render redirect in `focus_active_terminal` would
                // hand focus to the terminal instead, and this would silently
                // become a duplicate of
                // `switching_the_utility_kind_away_from_a_focused_terminal_
                // lands_on_the_centre_pane` below. Both must reach the centre
                // pane; the empty panel is the harsher case, because there is
                // no descendant to blur first.
                assert!(
                    panel.read(cx).tabs.is_empty(),
                    "precondition: no member project, so no auto-started shell \
                     and nothing for the render redirect to aim at"
                );
                assert!(
                    panel.focus_handle(cx).is_focused(window),
                    "precondition: the band's (empty) console panel owns the \
                     window's focus"
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

    /// The realistic companion to
    /// `switching_the_utility_kind_leaves_focus_on_the_centre_pane`: with a
    /// terminal tab present, `ctrl-\`` lands focus on the TERMINAL, so the
    /// handle that leaves the frame when the band switches kind is a
    /// descendant of the panel rather than the panel's own. The focus-lost
    /// path has to reach the centre pane from there too — and the sibling
    /// test above cannot show that, because the render redirect it relies on
    /// early-returns when there is no tab to redirect to.
    #[gpui::test]
    async fn switching_the_utility_kind_away_from_a_focused_terminal_lands_on_the_centre_pane(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let (window_handle, panel, _solution_id) = bootstrap_band_and_panel(cx, true).await;
        let band = band_of(&window_handle, cx);

        window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| panel.add_terminal_tab(None, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        let terminal = panel
            .read_with(cx, |panel, cx| panel.active_terminal_view(cx))
            .expect("the panel has one terminal tab");

        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window_handle
            .update(cx, |_workspace, window, cx| {
                assert!(
                    terminal.focus_handle(cx).is_focused(window),
                    "precondition: the redirect put focus on the terminal itself"
                );
            })
            .unwrap();

        band.update(cx, |band, cx| {
            band.activate_utility_kind(UtilityKind::GitGraph, cx)
        });
        cx.run_until_parked();

        window_handle
            .update(cx, |workspace, window, cx| {
                assert!(
                    !terminal.focus_handle(cx).is_focused(window),
                    "the terminal is gone from the frame; it must not still \
                     hold the window's focus"
                );
                assert!(
                    workspace.active_pane().focus_handle(cx).is_focused(window),
                    "and focus must have landed on the centre pane, so the \
                     next keystroke goes somewhere the user can see"
                );
            })
            .unwrap();
    }

    /// Closing the tab that holds focus must not eject the user to the centre
    /// pane. The hazard predates the render redirect but the redirect makes it
    /// the DEFAULT path: `ctrl-\`` used to leave focus on the panel root,
    /// where closing a tab moved nothing, and now it really does put focus on
    /// the terminal. That handle leaves the dispatch tree when the tab
    /// closes, so `Workspace`'s focus-lost listener pulls focus to
    /// `active_pane` unless `close_tab` re-homes it first — and the redirect
    /// cannot repair it, because its `is_focused` guard is false once focus
    /// points at a dead id.
    #[gpui::test]
    async fn closing_the_focused_tab_keeps_focus_in_the_console(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (window_handle, panel, _solution_id) = bootstrap_band_and_panel(cx, true).await;

        for _ in 0..3 {
            window_handle
                .update(cx, |_workspace, window, cx| {
                    panel.update(cx, |panel, cx| panel.add_terminal_tab(None, window, cx));
                })
                .unwrap();
            cx.run_until_parked();
        }
        let terminals = panel.read_with(cx, |panel, _| {
            panel
                .tabs
                .iter()
                .map(|ConsoleTab::Terminal { view, .. }| view.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(terminals.len(), 3);

        window_handle
            .update(cx, |workspace, window, cx| {
                crate::handle_toggle_focus(workspace, &ToggleFocus, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window_handle
            .update(cx, |_workspace, window, cx| {
                assert!(
                    terminals[2].focus_handle(cx).is_focused(window),
                    "precondition: ctrl-` focuses the active (last-added) terminal"
                );
            })
            .unwrap();

        window_handle
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |panel, cx| panel.close_tab(2, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        window_handle
            .update(cx, |workspace, window, cx| {
                assert!(
                    terminals[1].focus_handle(cx).is_focused(window),
                    "closing the focused tab must hand focus to the terminal \
                     that takes its place"
                );
                assert!(
                    !workspace.active_pane().focus_handle(cx).is_focused(window),
                    "and must not eject the user to the centre-pane editor"
                );
            })
            .unwrap();

        // The last tab is the interesting edge: nothing is left to redirect
        // to, and focus still must not leave the console.
        for index in (0..2).rev() {
            window_handle
                .update(cx, |_workspace, window, cx| {
                    panel.update(cx, |panel, cx| panel.close_tab(index, window, cx));
                })
                .unwrap();
            cx.run_until_parked();
        }
        window_handle
            .update(cx, |workspace, window, cx| {
                assert!(panel.read(cx).tabs.is_empty(), "all three tabs closed");
                assert!(
                    panel.focus_handle(cx).is_focused(window),
                    "with no terminal left, focus rests on the panel root — \
                     still rendered, still carrying the ConsolePanel key \
                     context, so a stray keystroke is absorbed by the console"
                );
                assert!(
                    !workspace.active_pane().focus_handle(cx).is_focused(window),
                    "closing the last terminal must not eject focus to the \
                     centre-pane editor either"
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
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |p, cx| {
                    p.activate_tab(1, cx);
                    assert_eq!(p.tabs.len(), 3);
                    assert_eq!(p.active_index, Some(1));
                    p.close_tab(1, window, cx);
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
            .update(cx, |_workspace, window, cx| {
                panel.update(cx, |p, cx| {
                    assert_eq!(p.tabs.len(), 1);
                    p.close_tab(0, window, cx);
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
