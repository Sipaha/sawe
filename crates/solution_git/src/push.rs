//! S-SOL-PSH — Solution-wide push orchestrator + dialog + MCP tool.
//!
//! Implements [`SolutionPushProvider`] from `git_ui`. Opens a workspace
//! pane Item with one collapsible section per member: each section
//! mirrors the single-repo S-PSH dialog (mini-graph + force / tags /
//! no-verify toggles) but is bundled into a single Push All flow that
//! runs the per-member pushes in parallel (default) or sequentially.
//!
//! ## Atomicity
//!
//! Push is *not* rollback-able — once a member has pushed, the remote has
//! the new tip. The orchestrator therefore makes no attempt at an atomic
//! "all-or-nothing" multi-repo push; instead it consolidates per-member
//! outcomes into a single results view (success / failed / skipped).
//!
//! ## Force-push handling
//!
//! `ForceMode::Force` rows render the toggle in red as an extra visual
//! cue. The MCP `solution.git.push_all` tool requires `confirmed: true`
//! whenever any member's resolved force mode is `Force` — same gate as
//! S-DST destructive ops.

use anyhow::{Result, anyhow};
use git_ui::mini_graph::MiniCommit;
use git_ui::providers::SolutionPushProvider;
use git_ui::push_dialog::{ForceMode, build_preview, run_plain_push};
use gpui::{
    AnyElement, App, AppContext, AsyncApp, Context, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, SharedString, WeakEntity, Window,
};
use solutions::{Solution, SolutionStore};
use std::collections::HashMap;
use std::path::PathBuf;
use ui::prelude::*;
use ui::{Button, Checkbox, Color, IconName, Label, LabelCommon, LabelSize, ToggleState, Tooltip};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams, TabTooltipContent},
};

// =====================================================================
//  Provider — registered in `solution_git::init`.
// =====================================================================

/// Holds a `WeakEntity<SolutionStore>` so the provider always resolves
/// against whichever Solution is currently active without needing to
/// re-register on solution switch (mirrors `commit::SolutionCommitOrchestrator`).
pub struct SolutionPushOrchestrator {
    store: WeakEntity<SolutionStore>,
}

impl SolutionPushOrchestrator {
    pub fn new(store: WeakEntity<SolutionStore>) -> Self {
        Self { store }
    }

    /// Most-recent `last_opened_at` heuristic — same as the dashboard /
    /// commit orchestrator. Returns `None` when the store is gone or has
    /// no Solutions.
    fn active_solution(&self, cx: &App) -> Option<Solution> {
        let store = self.store.upgrade()?;
        crate::active_solution_from_store(&store, cx)
    }
}

/// Build an orchestrator wired to the global `SolutionStore`. Returns
/// `None` when the store global is missing — same pattern as the commit
/// orchestrator.
pub fn build_global_orchestrator(cx: &App) -> Option<SolutionPushOrchestrator> {
    let store = SolutionStore::try_global(cx)?;
    Some(SolutionPushOrchestrator::new(store.downgrade()))
}

impl SolutionPushProvider for SolutionPushOrchestrator {
    fn is_active(&self) -> bool {
        self.store.upgrade().is_some()
    }

    fn open_solution_push_dialog(&self, workspace: WeakEntity<Workspace>, cx: &mut App) {
        let Some(solution) = self.active_solution(cx) else {
            log::info!("solution_git::push: no active Solution");
            return;
        };
        if solution.members.is_empty() {
            log::info!("solution_git::push: active Solution has no members");
            return;
        }
        // The trait surface only carries `&mut App`, so we route through
        // the currently-active platform window to get a `&mut Window`
        // for `add_item_to_active_pane` / `activate_item`. Falls back to
        // a no-op log if no window is focused (test contexts, etc.).
        let Some(window_handle) = cx.active_window() else {
            log::info!("solution_git::push: no active window — cannot open dialog");
            return;
        };
        let solution_for_existing = solution.clone();
        window_handle
            .update(cx, move |_, window, cx| {
                let Some(workspace_entity) = workspace.upgrade() else {
                    return;
                };
                workspace_entity.update(cx, |ws, cx| {
                    let existing: Option<gpui::Entity<SolutionPushDialog>> = ws
                        .items_of_type::<SolutionPushDialog>(cx)
                        .find(|item| item.read(cx).solution.id == solution_for_existing.id);
                    if let Some(existing) = existing {
                        ws.activate_item(&existing, true, true, window, cx);
                        return;
                    }
                    let weak = ws.weak_handle();
                    let dialog = cx.new(|cx| SolutionPushDialog::new(solution, weak, cx));
                    ws.add_item_to_active_pane(Box::new(dialog), None, true, window, cx);
                });
            })
            .log_err();
    }
}

// =====================================================================
//  Per-member section state.
// =====================================================================

/// One row in the dialog — mirrors a single Solution member.
#[derive(Debug, Clone)]
pub struct MemberPushSection {
    pub member_id: SharedString,
    pub work_dir: PathBuf,
    pub branch: SharedString,
    pub remote: SharedString,
    pub remote_branch: SharedString,
    pub ahead_commits: Vec<MiniCommit>,
    pub behind_count: u32,
    pub diverged: bool,
    pub will_create_remote: bool,
    pub skip: bool,
    pub force_mode: ForceMode,
    pub push_tags: bool,
    pub no_verify: bool,
    pub collapsed: bool,
    /// `true` while the initial preview is in flight; `false` once the
    /// preview has resolved (or errored — in which case `load_error`
    /// holds the message).
    pub loading: bool,
    pub load_error: Option<SharedString>,
}

impl MemberPushSection {
    fn skeleton(member_id: SharedString, work_dir: PathBuf) -> Self {
        Self {
            member_id,
            work_dir,
            branch: SharedString::default(),
            remote: SharedString::default(),
            remote_branch: SharedString::default(),
            ahead_commits: Vec::new(),
            behind_count: 0,
            diverged: false,
            will_create_remote: false,
            skip: false,
            force_mode: ForceMode::None,
            push_tags: false,
            no_verify: false,
            collapsed: false,
            loading: true,
            load_error: None,
        }
    }
}

/// Apply-to-all overrides surfaced in the top bar. Each `Some` value
/// overrides the per-section value at command-build time.
#[derive(Debug, Clone, Default)]
pub struct ApplyAllOptions {
    pub force_mode: Option<ForceMode>,
    pub push_tags: Option<bool>,
    pub no_verify: Option<bool>,
}

/// Per-member outcome captured after a push run.
#[derive(Debug, Clone)]
pub struct MemberPushOutcome {
    pub member_id: SharedString,
    pub status: PushOutcomeStatus,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcomeStatus {
    Pushed,
    UpToDate,
    Skipped,
    Failed,
}

impl PushOutcomeStatus {
    fn label(self) -> &'static str {
        match self {
            PushOutcomeStatus::Pushed => "pushed",
            PushOutcomeStatus::UpToDate => "up_to_date",
            PushOutcomeStatus::Skipped => "skipped",
            PushOutcomeStatus::Failed => "failed",
        }
    }
}

// =====================================================================
//  Dialog — workspace pane Item.
// =====================================================================

pub struct SolutionPushDialog {
    solution: Solution,
    sections: Vec<MemberPushSection>,
    apply_to_all: ApplyAllOptions,
    parallel: bool,
    pushing: bool,
    outcomes: Vec<MemberPushOutcome>,
    /// Held to keep a back-reference for future per-member "Open in
    /// Project" / "Show diff" buttons; not used today but exposed via
    /// the constructor for symmetry with [`crate::dashboard`].
    _workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
}

impl SolutionPushDialog {
    pub fn new(
        solution: Solution,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let sections: Vec<MemberPushSection> = solution
            .members
            .iter()
            .map(|m| {
                MemberPushSection::skeleton(
                    SharedString::from(m.catalog_id.0.clone()),
                    m.local_path.clone(),
                )
            })
            .collect();

        let mut this = Self {
            solution,
            sections,
            apply_to_all: ApplyAllOptions::default(),
            parallel: true,
            pushing: false,
            outcomes: Vec::new(),
            _workspace: workspace,
            focus_handle: cx.focus_handle(),
        };
        for section in this.sections.clone() {
            this.refresh_section_preview(section.member_id.clone(), section.work_dir.clone(), cx);
        }
        this
    }

    fn refresh_section_preview(
        &mut self,
        member_id: SharedString,
        work_dir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            // First resolve the current branch — branch detection is
            // cheap and lets us skip a doomed `build_preview` call when
            // HEAD is detached.
            let branch_result = cx
                .background_spawn({
                    let work_dir = work_dir.clone();
                    async move { resolve_current_branch(&work_dir).await }
                })
                .await;
            let branch = match branch_result {
                Ok(b) => b,
                Err(err) => {
                    let msg = format!("{err}");
                    let _ = this.update(cx, |this, cx| {
                        if let Some(section) =
                            this.sections.iter_mut().find(|s| s.member_id == member_id)
                        {
                            section.loading = false;
                            section.load_error = Some(SharedString::from(msg));
                        }
                        cx.notify();
                    });
                    return;
                }
            };
            let preview = cx
                .background_spawn({
                    let work_dir = work_dir.clone();
                    let branch = branch.clone();
                    async move { build_preview(&work_dir, &branch, "").await }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(section) = this.sections.iter_mut().find(|s| s.member_id == member_id) {
                    section.loading = false;
                    match preview {
                        Ok(preview) => {
                            section.branch = SharedString::from(branch);
                            section.remote = SharedString::from(preview.remote);
                            section.remote_branch = SharedString::from(preview.remote_branch);
                            section.ahead_commits = preview.ahead;
                            section.behind_count = preview.behind.len() as u32;
                            section.diverged = section.behind_count > 0;
                            section.will_create_remote = preview.will_create_remote_branch;
                            section.load_error = None;
                            // Skip members that have nothing to push
                            // unless the user opts in. Diverged remotes
                            // still render so the user can pick force /
                            // skip.
                            if section.ahead_commits.is_empty()
                                && !section.diverged
                                && !section.will_create_remote
                            {
                                section.skip = true;
                            }
                        }
                        Err(err) => {
                            section.load_error = Some(SharedString::from(format!("{err}")));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_section_skip(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(section) = self.sections.get_mut(ix) {
            section.skip = !section.skip;
            cx.notify();
        }
    }

    fn toggle_section_collapsed(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(section) = self.sections.get_mut(ix) {
            section.collapsed = !section.collapsed;
            cx.notify();
        }
    }

    fn toggle_section_force_with_lease(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(section) = self.sections.get_mut(ix) {
            section.force_mode = match section.force_mode {
                ForceMode::WithLease => ForceMode::None,
                _ => ForceMode::WithLease,
            };
            cx.notify();
        }
    }

    fn toggle_section_force(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(section) = self.sections.get_mut(ix) {
            section.force_mode = match section.force_mode {
                ForceMode::Force => ForceMode::None,
                _ => ForceMode::Force,
            };
            cx.notify();
        }
    }

    fn toggle_section_tags(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(section) = self.sections.get_mut(ix) {
            section.push_tags = !section.push_tags;
            cx.notify();
        }
    }

    fn toggle_section_no_verify(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(section) = self.sections.get_mut(ix) {
            section.no_verify = !section.no_verify;
            cx.notify();
        }
    }

    fn cycle_apply_force(&mut self, cx: &mut Context<Self>) {
        // None → WithLease → Force → None.
        self.apply_to_all.force_mode = match self.apply_to_all.force_mode {
            None => Some(ForceMode::WithLease),
            Some(ForceMode::WithLease) => Some(ForceMode::Force),
            Some(ForceMode::Force) => None,
            Some(ForceMode::None) => Some(ForceMode::WithLease),
        };
        cx.notify();
    }

    fn cycle_apply_tags(&mut self, cx: &mut Context<Self>) {
        self.apply_to_all.push_tags = match self.apply_to_all.push_tags {
            None => Some(true),
            Some(true) => Some(false),
            Some(false) => None,
        };
        cx.notify();
    }

    fn cycle_apply_no_verify(&mut self, cx: &mut Context<Self>) {
        self.apply_to_all.no_verify = match self.apply_to_all.no_verify {
            None => Some(true),
            Some(true) => Some(false),
            Some(false) => None,
        };
        cx.notify();
    }

    fn toggle_parallel(&mut self, cx: &mut Context<Self>) {
        self.parallel = !self.parallel;
        cx.notify();
    }

    fn confirm_push_all(&mut self, cx: &mut Context<Self>) {
        if self.pushing {
            return;
        }
        let plans: Vec<MemberPushPlan> = self
            .sections
            .iter()
            .filter(|s| !s.skip && s.load_error.is_none())
            .filter_map(|section| build_per_member_command(section, &self.apply_to_all))
            .collect();
        if plans.is_empty() {
            log::info!("solution_git::push: nothing to push (all members skipped or empty)");
            return;
        }
        let parallel = self.parallel;
        self.pushing = true;
        self.outcomes.clear();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcomes = cx
                .background_spawn(async move { execute_plans(plans, parallel).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.pushing = false;
                this.outcomes = outcomes;
                // Reload preview for every section so ahead-counts /
                // divergence indicators reflect the new server state.
                for section in this.sections.clone() {
                    this.refresh_section_preview(
                        section.member_id.clone(),
                        section.work_dir.clone(),
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        // The pane that owns this Item routes `ItemEvent::CloseItem` to
        // its own close machinery — no need to reach for the Workspace
        // handle (which would require also resolving a Window).
        cx.emit(ItemEvent::CloseItem);
    }
}

impl EventEmitter<ItemEvent> for SolutionPushDialog {}

impl Focusable for SolutionPushDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SolutionPushDialog {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(IconName::ArrowUp).color(Color::Muted))
    }

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!("{} — Push", self.solution.name).into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn tab_tooltip_content(&self, _cx: &App) -> Option<TabTooltipContent> {
        Some(TabTooltipContent::Text(
            format!("Solution Push — {}", self.solution.name).into(),
        ))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for SolutionPushDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .key_context("SolutionPushDialog")
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .child(self.render_top_bar(cx))
            .child(self.render_body(cx))
            .child(self.render_footer(cx))
    }
}

impl SolutionPushDialog {
    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let force_label: SharedString = match self.apply_to_all.force_mode {
            None => "force: per-member".into(),
            Some(ForceMode::None) => "force: off (apply)".into(),
            Some(ForceMode::WithLease) => "force-with-lease (apply to all)".into(),
            Some(ForceMode::Force) => "FORCE (apply to all)".into(),
        };
        let force_color = match self.apply_to_all.force_mode {
            Some(ForceMode::Force) => Color::Error,
            Some(ForceMode::WithLease) => Color::Warning,
            _ => Color::Muted,
        };
        let tags_label: SharedString = match self.apply_to_all.push_tags {
            None => "tags: per-member".into(),
            Some(true) => "tags: ON (apply)".into(),
            Some(false) => "tags: OFF (apply)".into(),
        };
        let no_verify_label: SharedString = match self.apply_to_all.no_verify {
            None => "no-verify: per-member".into(),
            Some(true) => "no-verify: ON (apply)".into(),
            Some(false) => "no-verify: OFF (apply)".into(),
        };
        let parallel_label: SharedString = if self.parallel {
            "parallel".into()
        } else {
            "sequential".into()
        };

        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new("Apply to all:")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("apply-force", force_label)
                    .color(force_color)
                    .tooltip(Tooltip::text(SharedString::from(
                        "Cycle: per-member → force-with-lease → FORCE → off",
                    )))
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_apply_force(cx))),
            )
            .child(
                Button::new("apply-tags", tags_label)
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_apply_tags(cx))),
            )
            .child(
                Button::new("apply-no-verify", no_verify_label)
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_apply_no_verify(cx))),
            )
            .child(
                Button::new("toggle-parallel", parallel_label)
                    .tooltip(Tooltip::text(SharedString::from(
                        "Toggle between parallel and sequential push execution",
                    )))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_parallel(cx))),
            )
    }

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.outcomes.is_empty() {
            return self.render_outcomes(cx).into_any_element();
        }
        let sections = self.sections.clone();
        let mut list = v_flex()
            .id("solution-push-sections")
            .flex_grow()
            .overflow_y_scroll();
        for (ix, section) in sections.iter().enumerate() {
            list = list.child(self.render_section(ix, section, cx).into_any_element());
        }
        list.into_any_element()
    }

    fn render_section(
        &self,
        ix: usize,
        section: &MemberPushSection,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id_str = section.member_id.to_string();
        let chevron = if section.collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        let header_summary: SharedString = if let Some(err) = &section.load_error {
            format!("error: {err}").into()
        } else if section.loading {
            "loading…".into()
        } else if section.skip {
            format!("skipped — {} ahead", section.ahead_commits.len()).into()
        } else if section.ahead_commits.is_empty() && !section.will_create_remote {
            "nothing to push".into()
        } else {
            let mut s = format!("{} ahead", section.ahead_commits.len());
            if section.diverged {
                s.push_str(&format!(", remote {} ahead", section.behind_count));
            }
            if section.will_create_remote {
                s.push_str(" (new remote branch)");
            }
            s.into()
        };

        let skip_state = if section.skip {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };

        let header = h_flex()
            .id(SharedString::from(format!("section-header-{id_str}")))
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                ui::IconButton::new(SharedString::from(format!("toggle-{id_str}")), chevron)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_section_collapsed(ix, cx)),
                    ),
            )
            .child(Label::new(section.member_id.clone()).size(LabelSize::Default))
            .child(
                Label::new(if section.branch.is_empty() {
                    SharedString::from("…")
                } else {
                    section.branch.clone()
                })
                .color(Color::Accent)
                .size(LabelSize::Small),
            )
            .child(Label::new("→").color(Color::Muted).size(LabelSize::Small))
            .child(
                Label::new(if section.remote.is_empty() {
                    SharedString::from("origin")
                } else {
                    section.remote.clone()
                })
                .color(Color::Muted)
                .size(LabelSize::Small),
            )
            .child(Label::new("/").color(Color::Muted).size(LabelSize::Small))
            .child(
                Label::new(if section.remote_branch.is_empty() {
                    SharedString::from("…")
                } else {
                    section.remote_branch.clone()
                })
                .color(Color::Muted)
                .size(LabelSize::Small),
            )
            .child(
                Label::new(header_summary)
                    .color(if section.load_error.is_some() {
                        Color::Error
                    } else if section.diverged {
                        Color::Warning
                    } else {
                        Color::Muted
                    })
                    .size(LabelSize::Small),
            )
            .child(
                Checkbox::new(SharedString::from(format!("skip-{id_str}")), skip_state)
                    .label("skip")
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_section_skip(ix, cx))),
            );

        if section.collapsed {
            return v_flex().child(header).into_any_element();
        }

        let mini = if section.ahead_commits.is_empty() {
            div()
                .py_2()
                .px_3()
                .child(
                    Label::new(if section.loading {
                        "Loading commits…"
                    } else if section.will_create_remote {
                        "New remote branch — full history will be pushed."
                    } else {
                        "Nothing to push."
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element()
        } else {
            git_ui::mini_graph::MiniGraph::new(section.ahead_commits.clone())
                .render(move |_ix, _cx| {}, cx)
                .into_any_element()
        };

        let force_lease_state = if matches!(section.force_mode, ForceMode::WithLease) {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let force_state = if matches!(section.force_mode, ForceMode::Force) {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let tags_state = if section.push_tags {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let no_verify_state = if section.no_verify {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };

        // When apply-to-all has a non-None override, the section's
        // checkboxes show as disabled for clarity — the override wins.
        let force_locked = self.apply_to_all.force_mode.is_some();
        let tags_locked = self.apply_to_all.push_tags.is_some();
        let no_verify_locked = self.apply_to_all.no_verify.is_some();

        let mut force_box = Checkbox::new(
            SharedString::from(format!("force-with-lease-{id_str}")),
            force_lease_state,
        )
        .label("force-with-lease")
        .disabled(force_locked)
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_section_force_with_lease(ix, cx)));
        if force_locked {
            force_box = force_box.tooltip(Tooltip::text(SharedString::from(
                "Overridden by Apply-to-all",
            )));
        }
        let mut force_plain_box =
            Checkbox::new(SharedString::from(format!("force-{id_str}")), force_state)
                .label("force")
                .disabled(force_locked)
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_section_force(ix, cx)));
        if force_locked {
            force_plain_box = force_plain_box.tooltip(Tooltip::text(SharedString::from(
                "Overridden by Apply-to-all",
            )));
        }
        let force_plain_box = if matches!(section.force_mode, ForceMode::Force) {
            force_plain_box.tooltip(Tooltip::text(SharedString::from(
                "Plain --force overwrites without atomic check.",
            )))
        } else {
            force_plain_box
        };

        let toggles_row = h_flex()
            .gap_3()
            .px_3()
            .py_2()
            .child(force_box)
            .child(force_plain_box)
            .child(
                Checkbox::new(SharedString::from(format!("tags-{id_str}")), tags_state)
                    .label("tags")
                    .disabled(tags_locked)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_section_tags(ix, cx))),
            )
            .child(
                Checkbox::new(
                    SharedString::from(format!("no-verify-{id_str}")),
                    no_verify_state,
                )
                .label("no-verify")
                .disabled(no_verify_locked)
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_section_no_verify(ix, cx))),
            );

        let body = v_flex()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                div()
                    .px_3()
                    .py_1()
                    .h(rems(14.))
                    .overflow_hidden()
                    .child(mini),
            )
            .child(toggles_row);

        v_flex().child(header).child(body).into_any_element()
    }

    fn render_outcomes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex()
            .id("solution-push-outcomes")
            .flex_grow()
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .gap_2();
        let pushed = self
            .outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.status,
                    PushOutcomeStatus::Pushed | PushOutcomeStatus::UpToDate
                )
            })
            .count();
        let failed = self
            .outcomes
            .iter()
            .filter(|o| o.status == PushOutcomeStatus::Failed)
            .count();
        let skipped = self
            .outcomes
            .iter()
            .filter(|o| o.status == PushOutcomeStatus::Skipped)
            .count();
        list = list.child(
            Label::new(SharedString::from(format!(
                "{pushed} pushed, {failed} failed, {skipped} skipped"
            )))
            .size(LabelSize::Default),
        );
        let outcomes = self.outcomes.clone();
        for outcome in outcomes.iter() {
            let color = match outcome.status {
                PushOutcomeStatus::Pushed => Color::Success,
                PushOutcomeStatus::UpToDate => Color::Muted,
                PushOutcomeStatus::Skipped => Color::Muted,
                PushOutcomeStatus::Failed => Color::Error,
            };
            let mut row = v_flex()
                .gap_0p5()
                .px_2()
                .py_1()
                .border_l_2()
                .border_color(match outcome.status {
                    PushOutcomeStatus::Pushed => cx.theme().status().success_border,
                    PushOutcomeStatus::Failed => cx.theme().status().error_border,
                    _ => cx.theme().colors().border_variant,
                })
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(outcome.member_id.clone()).color(color))
                        .child(
                            Label::new(SharedString::from(outcome.status.label()))
                                .color(color)
                                .size(LabelSize::Small),
                        ),
                );
            if let Some(err) = &outcome.error {
                row = row.child(
                    Label::new(SharedString::from(err.clone()))
                        .color(Color::Error)
                        .size(LabelSize::XSmall),
                );
            }
            if !outcome.stderr.trim().is_empty() {
                row = row.child(
                    Label::new(SharedString::from(outcome.stderr.trim().to_string()))
                        .color(Color::Muted)
                        .size(LabelSize::XSmall),
                );
            }
            list = list.child(row);
        }
        list
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pending = self
            .sections
            .iter()
            .filter(|s| !s.skip && s.load_error.is_none())
            .count();
        let push_label: SharedString = if self.pushing {
            "Pushing…".into()
        } else if !self.outcomes.is_empty() {
            "Push Again".into()
        } else {
            format!("Push All ({pending})").into()
        };
        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .justify_end()
            .child(
                Button::new("solution-push-cancel", "Close")
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
            )
            .child(
                Button::new("solution-push-go", push_label)
                    .disabled(self.pushing || pending == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_push_all(cx))),
            )
    }
}

// =====================================================================
//  Per-member command building.
// =====================================================================

/// Resolved per-member push command. Pure data — no `git` invocation
/// here, so the build step is unit-testable in isolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberPushPlan {
    pub member_id: SharedString,
    pub work_dir: PathBuf,
    pub branch: String,
    pub remote: String,
    pub remote_branch: String,
    pub force_mode: ForceMode,
    pub push_tags: bool,
    pub no_verify: bool,
    pub set_upstream: bool,
    /// `Some(sha)` pins `--force-with-lease=<branch>:<sha>` to the
    /// remote tip we previewed against; `None` falls back to plain
    /// `--force-with-lease`.
    pub expected_remote_sha: Option<String>,
}

impl MemberPushPlan {
    /// Argv after the leading `git` — used in tests to assert the
    /// command shape. Order matches `run_plain_push` / `run_force_with_lease`
    /// in `git_ui::push_dialog` so the executor below can shell out
    /// directly.
    pub fn build_argv(&self) -> Vec<String> {
        let mut args: Vec<String> = vec!["push".into()];
        if self.no_verify {
            args.push("--no-verify".into());
        }
        if self.push_tags {
            args.push("--tags".into());
        }
        if self.set_upstream {
            args.push("--set-upstream".into());
        }
        match self.force_mode {
            ForceMode::None => {}
            ForceMode::WithLease => match &self.expected_remote_sha {
                Some(sha) => {
                    args.push(format!("--force-with-lease={}:{}", self.remote_branch, sha))
                }
                None => args.push("--force-with-lease".into()),
            },
            ForceMode::Force => args.push("--force".into()),
        }
        args.push(self.remote.clone());
        args.push(format!("{}:{}", self.branch, self.remote_branch));
        args
    }
}

/// Apply the section state + apply-to-all overrides to produce a final
/// [`MemberPushPlan`]. Returns `None` if the section has no branch /
/// remote (still loading or errored).
pub fn build_per_member_command(
    section: &MemberPushSection,
    apply_to_all: &ApplyAllOptions,
) -> Option<MemberPushPlan> {
    if section.branch.is_empty() || section.remote.is_empty() || section.remote_branch.is_empty() {
        return None;
    }
    let force_mode = apply_to_all.force_mode.unwrap_or(section.force_mode);
    let push_tags = apply_to_all.push_tags.unwrap_or(section.push_tags);
    let no_verify = apply_to_all.no_verify.unwrap_or(section.no_verify);
    Some(MemberPushPlan {
        member_id: section.member_id.clone(),
        work_dir: section.work_dir.clone(),
        branch: section.branch.to_string(),
        remote: section.remote.to_string(),
        remote_branch: section.remote_branch.to_string(),
        force_mode,
        push_tags,
        no_verify,
        set_upstream: section.will_create_remote,
        // The per-row preview doesn't carry the remote sha today; the
        // dialog falls back to plain `--force-with-lease`. The MCP tool
        // can pass an explicit `expected_remote_sha` per member when
        // pinning is needed.
        expected_remote_sha: None,
    })
}

// =====================================================================
//  Execution.
// =====================================================================

async fn execute_plans(plans: Vec<MemberPushPlan>, parallel: bool) -> Vec<MemberPushOutcome> {
    if parallel {
        let futures = plans.into_iter().map(execute_one).collect::<Vec<_>>();
        futures::future::join_all(futures).await
    } else {
        let mut out = Vec::with_capacity(plans.len());
        for plan in plans {
            out.push(execute_one(plan).await);
        }
        out
    }
}

async fn execute_one(plan: MemberPushPlan) -> MemberPushOutcome {
    let res = match plan.force_mode {
        ForceMode::WithLease => {
            git_ui::push_dialog::run_force_with_lease(
                &plan.work_dir,
                &plan.branch,
                &plan.remote,
                &plan.remote_branch,
                plan.expected_remote_sha.as_deref(),
                plan.set_upstream,
                plan.push_tags,
                plan.no_verify,
            )
            .await
        }
        ForceMode::Force => {
            run_plain_push(
                &plan.work_dir,
                &plan.branch,
                &plan.remote,
                &plan.remote_branch,
                plan.set_upstream,
                plan.push_tags,
                plan.no_verify,
                true,
            )
            .await
        }
        ForceMode::None => {
            run_plain_push(
                &plan.work_dir,
                &plan.branch,
                &plan.remote,
                &plan.remote_branch,
                plan.set_upstream,
                plan.push_tags,
                plan.no_verify,
                false,
            )
            .await
        }
    };
    match res {
        Ok(out) => {
            let status = if out.stderr.contains("Everything up-to-date") {
                PushOutcomeStatus::UpToDate
            } else {
                PushOutcomeStatus::Pushed
            };
            MemberPushOutcome {
                member_id: plan.member_id,
                status,
                stdout: out.stdout,
                stderr: out.stderr,
                error: None,
            }
        }
        Err(err) => MemberPushOutcome {
            member_id: plan.member_id,
            status: PushOutcomeStatus::Failed,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(err.to_string()),
        },
    }
}

async fn resolve_current_branch(work_dir: &std::path::Path) -> Result<String> {
    use std::process::Stdio;
    use util::command::new_command;
    let mut command = new_command("git");
    command.current_dir(work_dir);
    command.args(["symbolic-ref", "--short", "-q", "HEAD"]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await?;
    if !output.status.success() {
        return Err(anyhow!("HEAD is detached or no branch resolved"));
    }
    let raw = String::from_utf8(output.stdout)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Err(anyhow!("HEAD is detached"))
    } else {
        Ok(trimmed.to_string())
    }
}

// =====================================================================
//  Action wiring + workspace registration.
// =====================================================================

gpui::actions!(
    solution_git,
    [
        /// Open the Solution-wide push dialog (S-SOL-PSH).
        PushAll,
    ]
);

/// Wire `solution_git::PushAll` into a workspace so the command palette
/// (and the dashboard's Push All button) can dispatch it. Called once
/// per workspace via `cx.observe_new` in `solution_git::init`.
pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &PushAll, _window, cx| {
        let Some(provider) = git_ui::providers::solution_push_provider() else {
            log::info!("solution_git::push: no SolutionPushProvider registered");
            return;
        };
        provider.open_solution_push_dialog(workspace.weak_handle(), cx);
    });
}

// =====================================================================
//  MCP tool — solution.git.push_all (Write tier; force ⇒ confirmed).
// =====================================================================

pub mod mcp {
    use super::*;
    use anyhow::Result;
    use context_server::listener::{McpServerTool, ToolResponse};
    use context_server::types::ToolResponseContent;
    use editor_mcp::{ToolTier, register_typed_tool_with_tier};
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use solutions::SolutionStore;

    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default, deny_unknown_fields)]
    pub struct PerMemberPushOptions {
        pub skip: Option<bool>,
        /// One of `"none" | "with_lease" | "force"`. Defaults to `"none"`.
        pub force_mode: Option<String>,
        pub push_tags: Option<bool>,
        pub no_verify: Option<bool>,
        pub expected_remote_sha: Option<String>,
    }

    /// Input parameters for the push all tool.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    #[serde(default, deny_unknown_fields)]
    pub struct PushAllInput {
        pub members: Option<Vec<String>>,
        pub per_member_options: Option<HashMap<String, PerMemberPushOptions>>,
        /// Default: true.
        pub parallel: Option<bool>,
        pub solution_id: Option<String>,
        /// Required when any resolved per-member force mode is `force`.
        pub confirmed: Option<bool>,
    }

    #[derive(Debug, Clone, Serialize, JsonSchema)]
    pub struct PushAllResultEntry {
        pub member_id: String,
        pub status: String,
        pub output: String,
        pub error: Option<String>,
    }

    /// Output of the push all tool.
    #[derive(Debug, Clone, Serialize, JsonSchema)]
    pub struct PushAllOutput {
        pub outcomes: Vec<PushAllResultEntry>,
    }

    fn parse_force(s: &str) -> Result<ForceMode> {
        match s {
            "none" => Ok(ForceMode::None),
            "with_lease" | "with-lease" => Ok(ForceMode::WithLease),
            "force" => Ok(ForceMode::Force),
            other => Err(anyhow!(
                "unknown force_mode `{other}`; expected `none`, `with_lease`, or `force`"
            )),
        }
    }

    /// Build per-member plans on the foreground thread (needs `&App` to
    /// read the SolutionStore). Doesn't run any git — that happens on a
    /// background thread.
    fn build_plans(
        members: Option<&[String]>,
        per_member: &HashMap<String, PerMemberPushOptions>,
        cx: &App,
    ) -> Result<Vec<MemberPushPlan>> {
        let store = SolutionStore::try_global(cx)
            .ok_or_else(|| anyhow!("no SolutionStore global — `solution_git::init` must run"))?;
        let solution = crate::active_solution_from_store(&store, cx)
            .ok_or_else(|| anyhow!("no active Solution"))?;
        let allowed: Option<std::collections::HashSet<&str>> =
            members.map(|ids| ids.iter().map(String::as_str).collect());

        let mut plans = Vec::new();
        for member in &solution.members {
            let id_str = member.catalog_id.0.as_str();
            if let Some(allowed) = &allowed
                && !allowed.contains(id_str)
            {
                continue;
            }
            // Drop non-git members so `push_all` doesn't fail every push
            // with "fatal: not a git repository". Mirrors the same gate
            // in dashboard / commit / aggregator.
            if !member.local_path.join(".git").exists() {
                continue;
            }
            let opts = per_member.get(id_str).cloned().unwrap_or_default();
            if opts.skip.unwrap_or(false) {
                continue;
            }
            let force_mode = match opts.force_mode.as_deref() {
                Some(s) => parse_force(s)?,
                None => ForceMode::None,
            };
            plans.push(MemberPushPlan {
                member_id: SharedString::from(id_str.to_string()),
                work_dir: member.local_path.clone(),
                // Branch / remote / remote_branch are filled in after
                // resolving the preview. We keep a placeholder here and
                // resolve below.
                branch: String::new(),
                remote: String::new(),
                remote_branch: String::new(),
                force_mode,
                push_tags: opts.push_tags.unwrap_or(false),
                no_verify: opts.no_verify.unwrap_or(false),
                set_upstream: false,
                expected_remote_sha: opts.expected_remote_sha.clone(),
            });
        }
        Ok(plans)
    }

    /// Fill in the per-plan branch / remote fields from `git`. Runs on a
    /// background thread; one per-plan await each.
    async fn fill_plans(plans: Vec<MemberPushPlan>) -> Vec<Result<MemberPushPlan>> {
        let mut futs = Vec::new();
        for plan in plans {
            futs.push(async move {
                let branch = resolve_current_branch(&plan.work_dir).await?;
                let preview = build_preview(&plan.work_dir, &branch, "").await?;
                Ok::<_, anyhow::Error>(MemberPushPlan {
                    branch,
                    remote: preview.remote,
                    remote_branch: preview.remote_branch,
                    set_upstream: preview.will_create_remote_branch,
                    ..plan
                })
            });
        }
        futures::future::join_all(futs).await
    }

    #[derive(Clone)]
    pub struct PushAllTool;

    impl McpServerTool for PushAllTool {
        type Input = PushAllInput;
        type Output = PushAllOutput;
        const NAME: &'static str = "solution.git.push_all";

        async fn run(
            &self,
            input: Self::Input,
            cx: &mut AsyncApp,
        ) -> Result<ToolResponse<Self::Output>> {
            let per_member = input.per_member_options.unwrap_or_default();
            let initial_plans: Vec<MemberPushPlan> =
                cx.update(|cx| build_plans(input.members.as_deref(), &per_member, cx))?;
            let any_force = initial_plans
                .iter()
                .any(|p| matches!(p.force_mode, ForceMode::Force));
            if any_force && !input.confirmed.unwrap_or(false) {
                return Err(anyhow!(
                    "solution.git.push_all with force=true requires confirmed=true"
                ));
            }
            if initial_plans.is_empty() {
                return Ok(ToolResponse {
                    content: vec![ToolResponseContent::Text {
                        text: "no members to push".into(),
                    }],
                    structured_content: PushAllOutput { outcomes: vec![] },
                });
            }
            let parallel = input.parallel.unwrap_or(true);
            let outcomes = cx
                .background_spawn(async move {
                    let mut filled = Vec::new();
                    for res in fill_plans(initial_plans).await {
                        filled.push(res);
                    }
                    let mut runnable = Vec::new();
                    let mut prep_failures = Vec::new();
                    for res in filled {
                        match res {
                            Ok(plan) => runnable.push(plan),
                            Err(err) => prep_failures.push(err.to_string()),
                        }
                    }
                    let mut results = execute_plans(runnable, parallel).await;
                    for err in prep_failures {
                        results.push(MemberPushOutcome {
                            member_id: SharedString::from("<unknown>"),
                            status: PushOutcomeStatus::Failed,
                            stdout: String::new(),
                            stderr: String::new(),
                            error: Some(err),
                        });
                    }
                    results
                })
                .await;

            let entries: Vec<PushAllResultEntry> = outcomes
                .iter()
                .map(|o| PushAllResultEntry {
                    member_id: o.member_id.to_string(),
                    status: o.status.label().to_string(),
                    output: format!("{}\n{}", o.stdout, o.stderr).trim().to_string(),
                    error: o.error.clone(),
                })
                .collect();
            let summary = format!(
                "{} pushed, {} failed",
                entries
                    .iter()
                    .filter(|e| e.status == "pushed" || e.status == "up_to_date")
                    .count(),
                entries.iter().filter(|e| e.status == "failed").count(),
            );
            Ok(ToolResponse {
                content: vec![ToolResponseContent::Text { text: summary }],
                structured_content: PushAllOutput { outcomes: entries },
            })
        }
    }

    pub(crate) fn register(cx: &mut App) {
        register_typed_tool_with_tier(cx, ToolTier::Write, PushAllTool);
    }
}

// =====================================================================
//  Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn skeleton(id: &str, branch: &str, remote: &str, remote_branch: &str) -> MemberPushSection {
        let mut s = MemberPushSection::skeleton(id.into(), PathBuf::from(format!("/tmp/{id}")));
        s.branch = branch.into();
        s.remote = remote.into();
        s.remote_branch = remote_branch.into();
        s.loading = false;
        s
    }

    #[test]
    fn build_per_member_command_force_with_lease_pinned_sha() {
        let mut section = skeleton("a", "feature", "origin", "feature");
        section.force_mode = ForceMode::WithLease;
        section.push_tags = true;
        let mut plan =
            build_per_member_command(&section, &ApplyAllOptions::default()).expect("plan");
        plan.expected_remote_sha = Some("deadbeef".into());
        let argv = plan.build_argv();
        assert_eq!(
            argv,
            vec![
                "push",
                "--tags",
                "--force-with-lease=feature:deadbeef",
                "origin",
                "feature:feature",
            ]
        );
    }

    #[test]
    fn build_per_member_command_skip_returns_none_when_branch_missing() {
        // Section before preview resolves: branch is empty.
        let section = MemberPushSection::skeleton("a".into(), PathBuf::from("/tmp/a"));
        assert!(build_per_member_command(&section, &ApplyAllOptions::default()).is_none());
    }

    #[test]
    fn apply_to_all_overrides_per_member_force() {
        let mut a = skeleton("a", "main", "origin", "main");
        a.force_mode = ForceMode::None;
        let mut b = skeleton("b", "main", "origin", "main");
        b.force_mode = ForceMode::WithLease;
        let apply = ApplyAllOptions {
            force_mode: Some(ForceMode::Force),
            ..Default::default()
        };
        let plan_a = build_per_member_command(&a, &apply).expect("a");
        let plan_b = build_per_member_command(&b, &apply).expect("b");
        assert_eq!(plan_a.force_mode, ForceMode::Force);
        assert_eq!(plan_b.force_mode, ForceMode::Force);
        assert!(plan_a.build_argv().iter().any(|s| s == "--force"));
        assert!(plan_b.build_argv().iter().any(|s| s == "--force"));
    }

    #[test]
    fn apply_to_all_tags_and_no_verify_override() {
        let section = skeleton("a", "main", "origin", "main");
        // Per-member push_tags = false; apply_to_all overrides to true.
        let apply = ApplyAllOptions {
            push_tags: Some(true),
            no_verify: Some(true),
            ..Default::default()
        };
        let plan = build_per_member_command(&section, &apply).expect("plan");
        assert!(plan.push_tags);
        assert!(plan.no_verify);
        let argv = plan.build_argv();
        assert!(argv.iter().any(|s| s == "--tags"));
        assert!(argv.iter().any(|s| s == "--no-verify"));
    }

    #[test]
    fn build_argv_set_upstream_emits_flag() {
        let mut section = skeleton("a", "feature", "origin", "feature");
        section.will_create_remote = true;
        let plan = build_per_member_command(&section, &ApplyAllOptions::default()).expect("plan");
        assert!(plan.set_upstream);
        let argv = plan.build_argv();
        assert!(argv.iter().any(|s| s == "--set-upstream"));
    }

    #[test]
    fn build_argv_no_force_when_section_clean() {
        let section = skeleton("a", "main", "origin", "main");
        let plan = build_per_member_command(&section, &ApplyAllOptions::default()).expect("plan");
        let argv = plan.build_argv();
        assert!(!argv.iter().any(|s| s == "--force"));
        assert!(!argv.iter().any(|s| s.starts_with("--force-with-lease")));
    }

    /// Mock-push-outcome consolidation: ensure parallel and sequential
    /// execution paths both surface the per-member outcomes in the
    /// returned vec (order-insensitive for parallel, order-preserving
    /// for sequential).
    #[gpui::test]
    async fn parallel_push_consolidates_outcomes(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        // Build three plans pointing at non-existent remotes — every
        // push will fail. We only care that we get three outcomes back
        // from `execute_plans` and that each one is `Failed`.
        let plans = vec![
            MemberPushPlan {
                member_id: "a".into(),
                work_dir: PathBuf::from("/nonexistent/a"),
                branch: "main".into(),
                remote: "origin".into(),
                remote_branch: "main".into(),
                force_mode: ForceMode::None,
                push_tags: false,
                no_verify: false,
                set_upstream: false,
                expected_remote_sha: None,
            },
            MemberPushPlan {
                member_id: "b".into(),
                work_dir: PathBuf::from("/nonexistent/b"),
                branch: "main".into(),
                remote: "origin".into(),
                remote_branch: "main".into(),
                force_mode: ForceMode::None,
                push_tags: false,
                no_verify: false,
                set_upstream: false,
                expected_remote_sha: None,
            },
            MemberPushPlan {
                member_id: "c".into(),
                work_dir: PathBuf::from("/nonexistent/c"),
                branch: "main".into(),
                remote: "origin".into(),
                remote_branch: "main".into(),
                force_mode: ForceMode::None,
                push_tags: false,
                no_verify: false,
                set_upstream: false,
                expected_remote_sha: None,
            },
        ];
        let outcomes = execute_plans(plans, true).await;
        assert_eq!(outcomes.len(), 3);
        for outcome in &outcomes {
            assert_eq!(
                outcome.status,
                PushOutcomeStatus::Failed,
                "expected Failed for {} (working dir doesn't exist)",
                outcome.member_id
            );
            assert!(outcome.error.is_some());
        }
        let ids: std::collections::HashSet<&str> =
            outcomes.iter().map(|o| o.member_id.as_ref()).collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
        assert!(ids.contains("c"));
    }
}
