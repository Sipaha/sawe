use gpui::{
    App, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, WeakEntity,
    Window, div, px,
};
use project::Project;
use solutions_ui::project_tab_strip::ProjectTabStrip;
use ui::{PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*};
use workspace::{MultiWorkspace, Workspace};

/// SPK Editor fork: a full-width toolbar row mounted by `Workspace` directly
/// below the title bar (see `Workspace::project_toolbar_item`). It hosts the
/// per-solution `ProjectTabStrip` on the left and the relocated git-branch
/// widget + run-config strip on the right.
///
/// Lives in the `title_bar` crate because `workspace` cannot depend on
/// `solutions_ui`/`git_ui`/`run_config_ui` (they depend on `workspace` — a
/// cycle), while `title_bar` already depends on all of them.
pub struct ProjectToolbar {
    workspace: WeakEntity<Workspace>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    project: Entity<Project>,
    // Created lazily once both `workspace` and `multi_workspace` are
    // resolved (mirrors `TitleBar::ensure_solution_tab_strip`).
    project_tab_strip: Option<Entity<ProjectTabStrip>>,
    branch_popover_handle: PopoverMenuHandle<git_ui::branch_picker::BranchesPopup>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectToolbar {
    pub fn new(
        workspace: &Workspace,
        multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();

        let mut subscriptions = Vec::new();
        // Re-render when the active repository or its branch changes so the
        // relocated branch widget stays current.
        subscriptions.push(cx.subscribe(
            &git_store,
            move |_, _, event, cx| match event {
                project::git_store::GitStoreEvent::ActiveRepositoryChanged(_)
                | project::git_store::GitStoreEvent::RepositoryUpdated(_, _, true) => {
                    cx.notify();
                }
                _ => {}
            },
        ));
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.observe(&workspace_entity, |_, _, cx| cx.notify()));
        }
        // Re-render the branch widget when the solution-wide active project
        // changes so it follows the active member's repository.
        if let Some(store) = solutions::SolutionStore::try_global(cx) {
            subscriptions.push(cx.subscribe(&store, |_, _, event, cx| {
                if let solutions::SolutionStoreEvent::ActiveMemberChanged { .. } = event {
                    cx.notify();
                }
            }));
        }

        Self {
            workspace: workspace.weak_handle(),
            multi_workspace,
            project,
            project_tab_strip: None,
            branch_popover_handle: PopoverMenuHandle::default(),
            _subscriptions: subscriptions,
        }
    }

    pub fn toggle_branch_popover(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.branch_popover_handle.toggle(window, cx);
    }

    /// Build (or return the cached) `ProjectTabStrip` entity. Mirrors
    /// `TitleBar::ensure_solution_tab_strip`: the strip is created lazily
    /// because `multi_workspace` may arrive after construction.
    fn ensure_project_tab_strip(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ProjectTabStrip>> {
        if self.project_tab_strip.is_none() {
            let workspace = self.workspace.clone();
            let multi_workspace = self.multi_workspace.clone()?;
            let strip = cx.new(|cx| ProjectTabStrip::new(workspace, multi_workspace, cx));
            self.project_tab_strip = Some(strip);
        }
        self.project_tab_strip.clone()
    }

    /// Resolve the repository the branch widget should display. Prefers the
    /// solution-wide active member's repository (the repo whose
    /// `work_directory_abs_path` is under the active member's `local_path` —
    /// mirroring `git_panel::refresh_active_repository_for_selector`), and falls
    /// back to `project.active_repository(cx)` when there is no active solution,
    /// no active member, or no matching repo (so a plain non-solution project
    /// still shows its branch). Both the display and the popover menu use this
    /// single resolution.
    fn resolve_repository(
        project: &Entity<Project>,
        cx: &App,
    ) -> Option<Entity<project::git_store::Repository>> {
        if let Some(repo) = Self::active_member_repository(project, cx) {
            return Some(repo);
        }
        project.read(cx).active_repository(cx)
    }

    /// The repository of this toolbar's solution-wide active member, if any.
    fn active_member_repository(
        project: &Entity<Project>,
        cx: &App,
    ) -> Option<Entity<project::git_store::Repository>> {
        let store = solutions::SolutionStore::try_global(cx)?;
        let store = store.read(cx);
        let solution = project
            .read(cx)
            .worktrees(cx)
            .find_map(|worktree| store.solution_for_path(&worktree.read(cx).abs_path()))?;
        let catalog = store.active_member(&solution.id)?;
        let member = solution
            .members
            .iter()
            .find(|member| &member.catalog_id == catalog)?;
        project
            .read(cx)
            .repositories(cx)
            .values()
            .find(|repo| {
                repo.read(cx)
                    .work_directory_abs_path
                    .starts_with(&member.local_path)
            })
            .cloned()
    }

    fn render_branch_widget(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let repository = Self::resolve_repository(&self.project, cx)?;
        let snapshot = repository.read(cx);
        // Only the `behind` count is shown on the branch widget now; the
        // `ahead` (unpushed) count moved to the dedicated Push button.
        let (name, behind) = match &snapshot.branch {
            Some(branch) => {
                let behind = branch
                    .tracking_status()
                    .map(|s| s.behind)
                    .unwrap_or(0);
                (SharedString::from(branch.name().to_string()), behind)
            }
            None => {
                // Detached HEAD: show short commit SHA, no upstream tracking indicators.
                let sha = snapshot.head_commit.as_ref().map(|c| c.short_sha())?;
                (sha, 0)
            }
        };
        let workspace_weak = self.workspace.clone();
        let project = self.project.clone();
        Some(
            PopoverMenu::new("branch-widget")
                .with_handle(self.branch_popover_handle.clone())
                .trigger(
                    ui::ButtonLike::new("branch-widget-trigger")
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::GitBranch).size(IconSize::Small))
                                .child(Label::new(name).size(LabelSize::Small))
                                // The unpushed-commit count (`↑ahead`) now lives
                                // on the dedicated Push button (`render_push_button`),
                                // so it's intentionally not shown here anymore.
                                .when(behind > 0, |this| {
                                    this.child(
                                        Label::new(format!("↓{behind}"))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                })
                                .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                        )
                        .toggle_state(self.branch_popover_handle.is_deployed()),
                )
                .menu(move |window, cx| {
                    let workspace = workspace_weak.upgrade()?;
                    let repository = Self::resolve_repository(&project, cx);
                    let weak = workspace.downgrade();
                    Some(cx.new(|cx| {
                        git_ui::branch_picker::BranchesPopup::new(weak, repository, window, cx)
                    }))
                }),
        )
    }

    /// "Update Project" button — sits to the LEFT of the branch-widget
    /// dropdown. Fetches + pulls ONLY the active project's repo (dispatches
    /// `git::Fetch` then `git::Pull` — they route through the git panel's
    /// `active_repository`, which is scoped to the active member). Solution-
    /// wide "Update All Projects" was deliberately dropped: a fetch+pull that
    /// spans every member can leave half the repos in a conflicted state with
    /// no good way to resolve it from this surface. Only shown when the active
    /// project has a git repository (mirrors the branch widget's gating).
    fn render_update_button(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        Self::resolve_repository(&self.project, cx)?;
        Some(
            IconButton::new("update-project-trigger", IconName::ArrowCircle)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Update Project — fetch + pull"))
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(git::Fetch), cx);
                    window.dispatch_action(Box::new(git::Pull), cx);
                }),
        )
    }

    /// "Push" button — sits beside the Update button. Shown ONLY when the
    /// active project's branch has unpushed commits (`ahead > 0`); the count
    /// renders next to the arrow icon (this is the indicator that used to sit
    /// on the branch-widget dropdown). Click dispatches `git::Push`, scoped to
    /// the git panel's active repository.
    fn render_push_button(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let repository = Self::resolve_repository(&self.project, cx)?;
        let ahead = repository
            .read(cx)
            .branch
            .as_ref()
            .and_then(|branch| branch.tracking_status())
            .map(|status| status.ahead)
            .unwrap_or(0);
        if ahead == 0 {
            return None;
        }
        Some(
            ui::ButtonLike::new("push-trigger")
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(Icon::new(IconName::ArrowUp).size(IconSize::Small))
                        .child(Label::new(format!("{ahead}")).size(LabelSize::Small)),
                )
                .tooltip(Tooltip::text("Push unpushed commits"))
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(git::Push), cx);
                }),
        )
    }
}

impl Render for ProjectToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Late-bind `multi_workspace` if it was not available at construction
        // (mirrors `TitleBar::render`).
        if self.multi_workspace.is_none() {
            if let Some(mw) = self
                .workspace
                .upgrade()
                .and_then(|ws| ws.read(cx).multi_workspace().cloned())
            {
                self.multi_workspace = Some(mw);
            }
        }
        // Use the title-bar background (not the more saturated
        // `toolbar_background`) so this row reads as a continuation of the
        // title bar above it rather than a separate, prominent band.
        let toolbar_background = cx.theme().colors().title_bar_background;
        let border_color = cx.theme().colors().border;
        let project_tab_strip = self.ensure_project_tab_strip(cx);

        let run_config = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).run_config_strip().cloned());

        h_flex()
            .w_full()
            .h(px(30.))
            .items_center()
            .bg(toolbar_background)
            // Top border separates this row from the solution-tab row in the
            // title bar above — needed now that the two share a background
            // (without it the project tabs visually merge into the title bar).
            // The bottom border separates it from the body below.
            .border_t_1()
            .border_b_1()
            .border_color(border_color)
            .pl_2()
            // Inset so the first project tab lines up with the left edge of
            // the project panel below it (the activity strip + panel border).
            // `pl_2` (8px) + 32px = 40px from the body's left, matching where
            // the project tree content begins.
            .child(div().w(px(32.)))
            .when_some(project_tab_strip, |this, strip| this.child(strip))
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_1()
                    .children(self.render_update_button(cx).map(IntoElement::into_any_element))
                    .children(self.render_push_button(cx).map(IntoElement::into_any_element))
                    .children(
                        self.render_branch_widget(cx)
                            .map(IntoElement::into_any_element),
                    ),
            )
            .children(run_config)
            .pr_1p5()
    }
}
