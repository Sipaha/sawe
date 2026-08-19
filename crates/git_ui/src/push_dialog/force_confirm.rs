//! S-SOL-PRT force-push confirmation — the panel that stands between a
//! force push and the remote history it would destroy.
//!
//! Lifted out of [`super`] verbatim: the gate that reads the policy
//! decision, the confirmation state machine (pending → offered /
//! refused), the git query behind it, and the render. Its only coupling
//! back to the dialog is [`PushDialog::start_push`], which
//! [`PushDialog::confirm_force_push`] re-enters once the user has agreed
//! — everything else here is reachable only from
//! [`PushDialog::confirm_push`]'s gate.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{AnyElement, AppContext, SharedString, Task, div};
use ui::{
    Button, ButtonStyle, Clickable, Color, Context, Headline, HeadlineSize, Icon, IconName,
    IconSize, IntoElement, Label, LabelCommon, LabelSize, ParentElement, Styled, TintColor, h_flex,
    prelude::*, v_flex,
};

use crate::mini_graph::MiniCommit;

use super::{ForceMode, PushDialog, list_commits_in_range, run_git_void};

impl PushDialog {
    /// Put the S-SOL-PRT confirmation up in place of the dialog body and
    /// start reading what the force-push would destroy. The countdown and
    /// the git query are tasks owned by the confirmation state, so
    /// cancelling it drops both.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_force_push_confirm(
        &mut self,
        title: String,
        detail: String,
        confirm_label: String,
        work_dir: PathBuf,
        branch: String,
        remote: String,
        remote_branch: String,
        cx: &mut Context<Self>,
    ) {
        let load = cx.spawn({
            let work_dir = work_dir.clone();
            let branch = branch.clone();
            let remote = remote.clone();
            let remote_branch = remote_branch.clone();
            async move |this, cx| {
                let overwritten = cx
                    .background_spawn(async move {
                        overwritten_commits(&work_dir, &branch, &remote, &remote_branch).await
                    })
                    .await;
                this.update(cx, |this, cx| this.resolve_overwritten(overwritten, cx))
                    .ok();
            }
        });
        self.force_confirm = Some(ForcePushConfirm {
            title: SharedString::from(title),
            detail: SharedString::from(detail),
            confirm_label: SharedString::from(confirm_label),
            work_dir,
            branch,
            remote,
            remote_branch,
            state: ForcePushConfirmState::Pending,
            _load: load,
        });
        cx.notify();
    }

    /// Turn the overwrite list into the confirmation's final state. This
    /// is where the force push is allowed or refused: an empty list makes
    /// it pointless and an unreadable one makes it blind, and neither is
    /// something a countdown or a warning label should be asked to
    /// protect the user from.
    fn resolve_overwritten(&mut self, overwritten: OverwrittenCommits, cx: &mut Context<Self>) {
        let Some(confirm) = self.force_confirm.as_ref() else {
            return;
        };
        // One press, one read, one answer: a resolution that arrives
        // after the confirmation has already settled is stale by
        // definition, and must not restart a countdown the user is part
        // way through — or, worse, reopen a push that was refused.
        if !matches!(confirm.state, ForcePushConfirmState::Pending) {
            return;
        }
        let state = match overwritten {
            OverwrittenCommits::Unknown { why } => {
                log::warn!(
                    "PushDialog: refusing force-push to {}/{}: {why}",
                    confirm.remote,
                    confirm.remote_branch
                );
                ForcePushConfirmState::Refused(ForcePushRefusal::Undeterminable { why })
            }
            OverwrittenCommits::Known(commits) if commits.is_empty() => {
                log::info!(
                    "PushDialog: refusing force-push to {}/{}: nothing to overwrite",
                    confirm.remote,
                    confirm.remote_branch
                );
                ForcePushConfirmState::Refused(ForcePushRefusal::NothingToOverwrite)
            }
            OverwrittenCommits::Known(commits) => ForcePushConfirmState::Offered {
                commits,
                countdown: FORCE_PUSH_CONFIRM_DELAY_SECS,
                _countdown: Self::spawn_confirm_countdown(cx),
            },
        };
        if let Some(confirm) = self.force_confirm.as_mut() {
            confirm.state = state;
        }
        cx.notify();
    }

    /// Ticks the reflex-click window down once a second. Only ever
    /// started for an offered push, so a refusal never counts down to
    /// something the user cannot have.
    fn spawn_confirm_countdown(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            for _ in 0..FORCE_PUSH_CONFIRM_DELAY_SECS {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let still_counting = this
                    .update(cx, |this, cx| {
                        let Some(ForcePushConfirmState::Offered { countdown, .. }) = this
                            .force_confirm
                            .as_mut()
                            .map(|confirm| &mut confirm.state)
                        else {
                            return false;
                        };
                        *countdown = countdown.saturating_sub(1);
                        // The countdown lives in the retained scene, so
                        // this notify — raised from a task, never from a
                        // draw phase, where it would be discarded — is
                        // the only thing that repaints the label.
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !still_counting {
                    return;
                }
            }
        })
    }

    /// Back out of a pending force-push confirmation. Dropping the state
    /// cancels its countdown and its git query along with it.
    pub(super) fn cancel_force_push_confirm(&mut self, cx: &mut Context<Self>) {
        if self.force_confirm.take().is_some() {
            log::info!("PushDialog: force-push declined at the branch-protection confirmation");
            cx.notify();
        }
    }

    /// The confirmation's destructive button. Pushes exactly the target
    /// that was gated, and only once the confirmation has armed.
    fn confirm_force_push(&mut self, cx: &mut Context<Self>) {
        if !self
            .force_confirm
            .as_ref()
            .is_some_and(ForcePushConfirm::is_armed)
        {
            return;
        }
        let Some(confirm) = self.force_confirm.take() else {
            return;
        };
        log::info!(
            "PushDialog: force-push confirmed for {}:{} on {}",
            confirm.branch,
            confirm.remote_branch,
            confirm.remote
        );
        self.start_push(
            confirm.work_dir,
            confirm.branch,
            confirm.remote,
            confirm.remote_branch,
            cx,
        );
    }

    /// The bespoke force-push confirmation: what is about to be run, the
    /// server-side commits it would overwrite, and — only when there are
    /// any — a confirm button that stays dead for
    /// [`FORCE_PUSH_CONFIRM_DELAY_SECS`] so a reflex click aimed at
    /// "Push" cannot land on it. When the push is refused there is no
    /// confirm button at all, dead or otherwise.
    pub(super) fn render_force_push_confirm(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(confirm) = self.force_confirm.as_ref() else {
            return div().into_any_element();
        };
        let tracking = format!("{}/{}", confirm.remote, confirm.remote_branch);

        let mut body = v_flex().gap_1();
        let mut confirm_button = None;
        match &confirm.state {
            ForcePushConfirmState::Pending => {
                body = body.child(
                    Label::new(format!("Reading what {tracking} holds…"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            ForcePushConfirmState::Refused(refusal) => {
                let (headline, advice) =
                    refusal_lines(refusal, &confirm.branch, &tracking, &confirm.remote);
                body = body
                    .child(
                        Label::new(headline)
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(advice)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    );
            }
            ForcePushConfirmState::Offered {
                commits, countdown, ..
            } => {
                let plural = if commits.len() == 1 { "" } else { "s" };
                body = body.child(
                    Label::new(format!(
                        "{} commit{plural} on {tracking} will be overwritten:",
                        commits.len()
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Error),
                );
                let mut rows = v_flex()
                    .id("push-dialog-force-confirm-overwritten")
                    .max_h(rems(12.))
                    .overflow_y_scroll();
                for (ix, commit) in commits.iter().take(MAX_OVERWRITTEN_ROWS).enumerate() {
                    rows = rows.child(render_overwritten_row(ix, commit));
                }
                body = body.child(rows);
                if commits.len() > MAX_OVERWRITTEN_ROWS {
                    body = body.child(
                        Label::new(format!(
                            "…and {} more.",
                            commits.len() - MAX_OVERWRITTEN_ROWS
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    );
                }
                let label: SharedString = if *countdown > 0 {
                    format!("{} ({countdown})", confirm.confirm_label).into()
                } else {
                    confirm.confirm_label.clone()
                };
                confirm_button = Some(
                    Button::new("push-dialog-force-confirm", label)
                        .style(ButtonStyle::Tinted(TintColor::Error))
                        .disabled(*countdown > 0)
                        .on_click(cx.listener(|this, _, _window, cx| this.confirm_force_push(cx))),
                );
            }
        }

        v_flex()
            .key_context("PushDialog")
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(64.))
            .max_h(rems(40.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(Headline::new(confirm.title.clone()).size(HeadlineSize::Small)),
            )
            .child(
                Label::new(confirm.detail.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(body)
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("push-dialog-force-confirm-cancel", "Cancel").on_click(
                            cx.listener(|this, _, _window, cx| this.cancel_force_push_confirm(cx)),
                        ),
                    )
                    .when_some(confirm_button, |this, button| this.child(button)),
            )
            .into_any_element()
    }
}

/// What the S-SOL-PRT policy says about the force-push the user just
/// pressed Push on. Kept as a pure function over the [`Decision`] so the
/// tier→UI mapping is testable: the live policy snapshot lives in a
/// process-global cache owned by `solutions` that a `git_ui` test can
/// neither install nor isolate from its neighbours.
///
/// [`Decision`]: solutions::branch_protection::Decision
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ForcePushGate {
    /// Nothing to add — run the push.
    Proceed,
    /// `Forbidden`: lock the force toggle with this reason, push nothing.
    Locked { reason: SharedString },
    /// `RequiresConfirmation`: ask first, push only if the user agrees.
    Confirm {
        title: String,
        detail: String,
        confirm_label: String,
    },
}

pub(super) fn force_push_gate(
    decision: &solutions::branch_protection::Decision,
    force_mode: ForceMode,
    branch: &str,
    remote: &str,
    remote_branch: &str,
) -> ForcePushGate {
    use solutions::branch_protection::Decision;

    let force_flag = match force_mode {
        // Not a force push, so this gate has no say: a plain push is
        // `Allowed` by policy and must stay friction-free.
        ForceMode::None => return ForcePushGate::Proceed,
        // Both force postures run the leased form now — see `run_push_cli`.
        ForceMode::WithLease | ForceMode::Force => "--force-with-lease",
    };
    let reason = match decision {
        Decision::Allowed => return ForcePushGate::Proceed,
        Decision::Forbidden { reason } => {
            return ForcePushGate::Locked {
                reason: SharedString::from(reason.clone()),
            };
        }
        // Shown whole: the policy writes these for a plain confirm/cancel
        // prompt, and `branch_protection`'s own
        // `confirmation_reasons_never_ask_to_type_the_branch_name` pins
        // that at the source. Re-trimming here could only eat a future
        // reason's second clause.
        Decision::RequiresConfirmation { reason } => reason.as_str(),
    };
    ForcePushGate::Confirm {
        title: format!("Force-push “{branch}” to “{remote}”?"),
        detail: format!(
            "Branch protection: {reason}. Runs git push {force_flag} {remote} \
             {branch}:{remote_branch}, which overwrites {remote}/{remote_branch} with your \
             local history for everyone using this remote; commits that exist only there \
             cannot be restored from here."
        ),
        confirm_label: format!("Force-push to {remote}"),
    }
}

/// Seconds the confirmation's destructive button stays dead after it
/// appears. The dialog's Push button sits where this one does, so
/// without the delay a double-click on Push lands the second press on
/// "Force-push" — the exact accident the confirmation exists to stop.
const FORCE_PUSH_CONFIRM_DELAY_SECS: u8 = 5;

/// Rows rendered before the list is elided. A force-push that drops
/// hundreds of server-side commits is answered by the count, not by
/// scrolling all of them.
const MAX_OVERWRITTEN_ROWS: usize = 25;

/// Live state of the force-push confirmation. Owns its countdown and its
/// git query, so dropping it (Cancel / Escape) cancels both.
pub(super) struct ForcePushConfirm {
    title: SharedString,
    detail: SharedString,
    confirm_label: SharedString,
    /// The gated push target, captured at the press boundary so what
    /// finally runs is what the user was shown — not whatever the
    /// remote-branch editor says by the time they answer.
    work_dir: PathBuf,
    branch: String,
    remote: String,
    remote_branch: String,
    state: ForcePushConfirmState,
    _load: Task<()>,
}

/// Where a confirmation is in its life: reading the remote, offering the
/// push, or refusing it outright.
enum ForcePushConfirmState {
    /// Reading what `<remote>/<remote_branch>` holds. No confirm control
    /// exists yet and no countdown runs — there is nothing to count down
    /// to until we know whether the push is offerable at all.
    Pending,
    /// The remote holds work this branch does not: the only state in
    /// which force-pushing means anything, and so the only one with a
    /// countdown and a confirm button.
    Offered {
        commits: Vec<MiniCommit>,
        /// Seconds left before the confirm button arms.
        countdown: u8,
        _countdown: Task<()>,
    },
    /// Refused. The confirm control is not merely disabled, it is absent.
    Refused(ForcePushRefusal),
}

/// Why a force push was refused outright. Both cases are decided from
/// the overwrite list read at the press boundary, so neither can be
/// slipped past by a dialog that has sat open while the world moved.
enum ForcePushRefusal {
    /// The remote holds nothing this branch is missing. Forcing would do
    /// exactly what an ordinary push does, so the flag can only matter
    /// if this reading is wrong — i.e. it can only do harm.
    NothingToOverwrite,
    /// What the server holds could not be read. Refusing beats warning:
    /// a confirmation the user cannot make an informed answer to is not
    /// a safeguard, it is a rubber stamp.
    Undeterminable { why: SharedString },
}

impl ForcePushConfirm {
    /// The destructive button arms only in [`ForcePushConfirmState::Offered`],
    /// and only once the reflex-click window has passed.
    fn is_armed(&self) -> bool {
        matches!(
            self.state,
            ForcePushConfirmState::Offered { countdown: 0, .. }
        )
    }
}

/// The two lines a refusal shows: what is wrong, and what to do instead.
/// Split out of the render so the wording is unit-testable and so the
/// two cases cannot drift into saying the same thing.
fn refusal_lines(
    refusal: &ForcePushRefusal,
    branch: &str,
    tracking: &str,
    remote: &str,
) -> (String, String) {
    match refusal {
        ForcePushRefusal::NothingToOverwrite => (
            format!("Nothing on {tracking} to overwrite — a force push here is pointless."),
            format!(
                "“{branch}” already contains everything {tracking} holds, so forcing can only \
                 differ from an ordinary push if this reading is wrong. Turn force-with-lease \
                 off and press Push; if nothing is ahead either, there is nothing to push."
            ),
        ),
        // The missing-ref case is *why* this refusal exists, and it
        // covers both "never fetched" and "the branch does not exist on
        // the remote yet" — see `overwritten_commits`. A fetch can only
        // help the first, so the second needs the same way out the
        // pointless case gets: a brand-new remote branch is created by an
        // ordinary push, and forcing adds nothing to a create. Both
        // sentences are offered rather than picking one from
        // `PushPreview::will_create_remote_branch`, which is a snapshot
        // taken before the press — letting a stale or offline preview
        // choose the wording is how the user gets told the one thing that
        // cannot work.
        ForcePushRefusal::Undeterminable { why } => (
            format!("Cannot tell what {tracking} would lose: {why}."),
            format!(
                "Refusing to force-push blind — this dialog will not overwrite history it \
                 could not read. Fetch {remote}, then press Push again. If {tracking} does not \
                 exist on the server yet, no fetch can create it: turn force-with-lease off and \
                 press Push, which is all it takes to create a branch there."
            ),
        ),
    }
}

/// What `<remote>/<remote_branch>` holds that the local branch does not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OverwrittenCommits {
    /// Read successfully. Empty means the remote-tracking ref holds
    /// nothing the local branch is missing, as of the last fetch.
    Known(Vec<MiniCommit>),
    /// Could not be determined. Refused rather than warned about: an
    /// empty list would read as "nothing will be lost", and a warning the
    /// user cannot check is not a safeguard.
    Unknown { why: SharedString },
}

/// The commits a force-push would drop on the server: reachable from
/// `<remote>/<remote_branch>` but not from `branch`.
///
/// Deliberately re-derived instead of read off [`PushPreview::behind`]:
/// the preview's ranges are built with `--no-merges` (right for "what am
/// I about to send", wrong for "what am I about to destroy" — a merge
/// commit lost is still work lost), and a preview refresh that failed
/// leaves the previous, possibly empty, vec in place.
async fn overwritten_commits(
    work_dir: &Path,
    branch: &str,
    remote: &str,
    remote_branch: &str,
) -> OverwrittenCommits {
    let tracking = format!("{remote}/{remote_branch}");
    // No tracking ref is ambiguous — a brand-new remote branch looks
    // exactly like one this clone has never fetched — so it counts as
    // undeterminable, not as "nothing there". `--force-with-lease`
    // refuses on the same evidence.
    if let Err(err) = run_git_void(work_dir, &["rev-parse", "--verify", "--quiet", &tracking]).await
    {
        log::info!("PushDialog: no {tracking} ref to compare against: {err:#}");
        return OverwrittenCommits::Unknown {
            why: SharedString::from(format!(
                "this clone has no {tracking} ref, so what the server holds cannot be read \
                 from here — fetch {remote} first"
            )),
        };
    }
    match list_commits_in_range(work_dir, &format!("{branch}..{tracking}"), false).await {
        Ok(commits) => OverwrittenCommits::Known(commits),
        Err(err) => {
            log::warn!("PushDialog: listing {branch}..{tracking} failed: {err:#}");
            OverwrittenCommits::Unknown {
                why: SharedString::from(format!("git log {branch}..{tracking} failed: {err:#}")),
            }
        }
    }
}

/// One overwritten-commit row. Same shape as the dialog's own commit
/// rows (`mini_graph::render_row`) — subject on top, metadata muted
/// underneath — plus the author, since "who loses this work" is the
/// question this list exists to answer.
fn render_overwritten_row(ix: usize, commit: &MiniCommit) -> AnyElement {
    v_flex()
        .id(SharedString::from(format!(
            "push-dialog-overwritten-row-{ix}"
        )))
        .px_2()
        .py_1()
        .gap_0p5()
        .child(
            Label::new(commit.subject.clone())
                .size(LabelSize::Small)
                .truncate(),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Label::new(commit.short_sha())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(crate::mini_graph::format_relative(
                        commit.committer_date_unix,
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    Label::new(commit.author_email.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::Editor;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use menu::Cancel;
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::MultiWorkspace;

    use crate::push_dialog::tests::boot_repo;
    use crate::push_dialog::{PushPreview, build_preview};

    fn requires_confirmation(reason: &str) -> solutions::branch_protection::Decision {
        solutions::branch_protection::Decision::RequiresConfirmation {
            reason: reason.to_string(),
        }
    }

    /// A plain push is `Allowed` by policy and must stay friction-free —
    /// the gate has nothing to say about it even when the branch is one
    /// the policy guards.
    #[test]
    fn a_plain_push_is_never_gated() {
        assert_eq!(
            force_push_gate(
                &requires_confirmation(
                    "'main' is protected — confirm force-push by typing the branch name"
                ),
                ForceMode::None,
                "main",
                "origin",
                "main",
            ),
            ForcePushGate::Proceed
        );
    }

    #[test]
    fn an_allowed_force_push_is_never_gated() {
        assert_eq!(
            force_push_gate(
                &solutions::branch_protection::Decision::Allowed,
                ForceMode::WithLease,
                "main",
                "origin",
                "main",
            ),
            ForcePushGate::Proceed
        );
    }

    /// `Forbidden` keeps doing exactly what it did before the
    /// confirmation existed: lock the toggle with the policy's reason and
    /// push nothing. It must never be downgraded into a question.
    #[test]
    fn a_forbidden_force_push_locks_instead_of_asking() {
        let gate = force_push_gate(
            &solutions::branch_protection::Decision::Forbidden {
                reason: "force-push to protected branch 'main' is forbidden".to_string(),
            },
            ForceMode::WithLease,
            "main",
            "origin",
            "main",
        );
        assert_eq!(
            gate,
            ForcePushGate::Locked {
                reason: SharedString::from("force-push to protected branch 'main' is forbidden"),
            }
        );
    }

    #[test]
    fn a_confirmable_force_push_names_the_branch_remote_and_command() {
        let gate = force_push_gate(
            &requires_confirmation("force-push to 'wip' rewrites remote history"),
            ForceMode::WithLease,
            "wip",
            "origin",
            "wip-remote",
        );
        let ForcePushGate::Confirm {
            title,
            detail,
            confirm_label,
        } = gate
        else {
            panic!("the middle tier must ask, got: {gate:?}");
        };
        assert_eq!(title, "Force-push “wip” to “origin”?");
        assert_eq!(confirm_label, "Force-push to origin");
        assert!(
            detail.contains("git push --force-with-lease origin wip:wip-remote"),
            "the confirmation must name the exact command, got: {detail}"
        );
        assert!(
            detail.contains("force-push to 'wip' rewrites remote history"),
            "the policy's own reason must survive into the prompt, got: {detail}"
        );
    }

    /// The legacy bare-`--force` posture is upgraded, so it must never
    /// advertise a flag the runner will not use.
    #[test]
    fn a_legacy_force_posture_is_described_as_leased() {
        let gate = force_push_gate(
            &requires_confirmation("force-push to 'wip' rewrites remote history"),
            ForceMode::Force,
            "wip",
            "origin",
            "wip",
        );
        let ForcePushGate::Confirm { detail, .. } = gate else {
            panic!("the middle tier must ask, got: {gate:?}");
        };
        assert!(detail.contains("--force-with-lease"), "got: {detail}");
        assert!(
            !detail.contains("git push --force "),
            "a bare --force must not be advertised: {detail}"
        );
    }

    /// The gate shows the policy's reason whole, second clause and all.
    /// It used to trim a "confirm … by typing the branch name" tail
    /// written for a type-the-name modal this fork does not have, but
    /// `solutions::branch_protection` no longer writes one and its
    /// `confirmation_reasons_never_ask_to_type_the_branch_name` pins that
    /// at the source — so a trimmer here could only silently eat a real
    /// second clause like this one.
    #[test]
    fn the_confirmation_shows_the_policy_reason_whole() {
        let gate = force_push_gate(
            &requires_confirmation("'main' is protected — ask the release owner"),
            ForceMode::WithLease,
            "main",
            "origin",
            "main",
        );
        let ForcePushGate::Confirm { detail, .. } = gate else {
            panic!("the middle tier must ask, got: {gate:?}");
        };
        assert!(
            detail.contains("'main' is protected — ask the release owner"),
            "the policy's whole reason must survive into the panel, got: {detail}"
        );
    }

    fn confirm_with(state: ForcePushConfirmState) -> ForcePushConfirm {
        ForcePushConfirm {
            title: "Force-push “main” to “origin”?".into(),
            detail: "".into(),
            confirm_label: "Force-push to origin".into(),
            work_dir: PathBuf::from("/dir"),
            branch: "main".into(),
            remote: "origin".into(),
            remote_branch: "main".into(),
            state,
            _load: Task::ready(()),
        }
    }

    fn offered(countdown: u8) -> ForcePushConfirmState {
        ForcePushConfirmState::Offered {
            commits: vec![MiniCommit {
                sha: "deadbeefcafe".into(),
                subject: "server work".into(),
                author_email: "them@example.com".into(),
                committer_date_unix: 1_700_000_000,
            }],
            countdown,
            _countdown: Task::ready(()),
        }
    }

    /// Updated from the earlier version of this test, which let an
    /// undeterminable list arm the button once the countdown ran out.
    /// Neither refusal state is armable at all now.
    #[test]
    fn the_confirm_control_arms_only_for_an_offered_push() {
        assert!(
            !confirm_with(offered(1)).is_armed(),
            "the reflex-click window must gate the button"
        );
        assert!(confirm_with(offered(0)).is_armed());
        assert!(
            !confirm_with(ForcePushConfirmState::Pending).is_armed(),
            "confirming before the overwrite list resolves is confirming blind"
        );
        assert!(
            !confirm_with(ForcePushConfirmState::Refused(
                ForcePushRefusal::NothingToOverwrite
            ))
            .is_armed(),
            "a force push that overwrites nothing must be unreachable, not merely slow"
        );
        assert!(
            !confirm_with(ForcePushConfirmState::Refused(
                ForcePushRefusal::Undeterminable {
                    why: "no tracking ref".into()
                }
            ))
            .is_armed(),
            "a force push we cannot describe must be unreachable, not merely warned about"
        );
    }

    /// The two refusals must not read alike: one says "you do not need
    /// this", the other says "we could not look".
    #[test]
    fn the_two_refusals_say_different_things() {
        let (pointless_head, pointless_advice) = refusal_lines(
            &ForcePushRefusal::NothingToOverwrite,
            "main",
            "origin/main",
            "origin",
        );
        assert!(
            pointless_head.contains("origin/main") && pointless_head.contains("pointless"),
            "got: {pointless_head}"
        );
        assert!(
            pointless_advice.contains("force-with-lease") && pointless_advice.contains("Push"),
            "the empty case must point at the ordinary push: {pointless_advice}"
        );
        assert!(
            pointless_advice.contains("nothing to push"),
            "and at the case where there is nothing to send at all: {pointless_advice}"
        );

        let (blind_head, blind_advice) = refusal_lines(
            &ForcePushRefusal::Undeterminable {
                why: "this clone has no origin/main ref".into(),
            },
            "main",
            "origin/main",
            "origin",
        );
        assert!(
            blind_head.contains("Cannot tell") && blind_head.contains("no origin/main ref"),
            "got: {blind_head}"
        );
        assert!(
            blind_advice.contains("Fetch origin"),
            "the unknown case must send the user to fetch: {blind_advice}"
        );
        assert_ne!(pointless_head, blind_head);
        assert_ne!(pointless_advice, blind_advice);
    }

    /// "Fetch, then press Push again" is a dead end for the case that
    /// produces this refusal most often: a branch that does not exist on
    /// the remote yet has no tracking ref for any fetch to create, so the
    /// advice has to name the move that does work.
    #[test]
    fn the_blind_refusal_offers_a_way_out_for_a_brand_new_branch() {
        let (_, advice) = refusal_lines(
            &ForcePushRefusal::Undeterminable {
                why: "this clone has no origin/feature ref".into(),
            },
            "feature",
            "origin/feature",
            "origin",
        );
        assert!(
            advice.contains("does not exist on the server yet"),
            "the new-branch case must be named: {advice}"
        );
        assert!(
            advice.contains("turn force-with-lease off"),
            "and must be given the move that actually creates it: {advice}"
        );
    }

    fn init_ui_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// A dialog on a fake project, built field-by-field rather than
    /// through [`PushDialog::open`]: the press boundary is what is under
    /// test, and `open`'s async preview refresh would drag a real `git`
    /// invocation into every assertion.
    async fn init_push_dialog(
        force_mode: ForceMode,
        cx: &mut TestAppContext,
    ) -> (Entity<PushDialog>, VisualTestContext) {
        init_ui_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                ".git": {},
                "file.txt": "hello".to_string()
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let repository = cx
            .read(|cx| project.read(cx).active_repository(cx))
            .expect("fake project should expose a repository");
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");

        let dialog = window_handle
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    let editor = cx.new(|cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_text("main", window, cx);
                        editor
                    });
                    PushDialog {
                        workspace: workspace.downgrade(),
                        repository: repository.clone(),
                        work_dir: PathBuf::from(path!("/dir")),
                        branch: "main".into(),
                        remote: "origin".into(),
                        remote_branch_editor: editor,
                        preview: PushPreview::default(),
                        selected_commit: None,
                        selected_files: Vec::new(),
                        force_mode,
                        push_tags: false,
                        no_verify: false,
                        pull_rebase_first: false,
                        force_locked_reason: None,
                        force_confirm: None,
                        pushing: false,
                        refreshing: false,
                        failure: None,
                        notice: None,
                        remediation: None,
                        focus_handle: cx.focus_handle(),
                    }
                })
            })
            .expect("window should be open");

        (
            dialog,
            VisualTestContext::from_window(window_handle.into(), cx),
        )
    }

    #[gpui::test]
    async fn pressing_push_on_a_force_push_opens_the_confirmation(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });

        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog
                .force_confirm
                .as_ref()
                .expect("a force push must be confirmed before it runs");
            assert!(
                confirm.title.contains("main") && confirm.title.contains("origin"),
                "the confirmation must name the branch and the remote, got: {}",
                confirm.title
            );
            assert!(
                confirm
                    .detail
                    .contains("git push --force-with-lease origin main:main"),
                "got: {}",
                confirm.detail
            );
            assert!(
                !dialog.pushing,
                "nothing may be pushed until the user answers"
            );
        });
    }

    #[gpui::test]
    async fn a_plain_push_opens_no_confirmation(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::None, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });

        dialog.update(&mut cx, |dialog, _| {
            assert!(
                dialog.force_confirm.is_none(),
                "a plain push must stay friction-free"
            );
            assert!(dialog.pushing, "it should have gone straight to the push");
        });
    }

    #[gpui::test]
    async fn cancelling_the_confirmation_aborts_the_push(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });
        dialog.update_in(&mut cx, |dialog, window, cx| {
            // Escape, the same path the Cancel button takes.
            dialog.cancel(&Cancel, window, cx)
        });

        dialog.update(&mut cx, |dialog, _| {
            assert!(dialog.force_confirm.is_none(), "the confirmation is gone");
            assert!(!dialog.pushing, "cancelling must not push");
        });
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, _| {
            assert!(
                !dialog.pushing,
                "and it must not push once the timers drain either"
            );
        });
    }

    fn server_commit(subject: &str) -> MiniCommit {
        MiniCommit {
            sha: "0badc0ffee11".into(),
            subject: subject.into(),
            author_email: "them@example.com".into(),
            committer_date_unix: 1_700_000_000,
        }
    }

    /// The confirm control sits where the Push button was, so a
    /// double-click on Push would otherwise land its second press on the
    /// force-push. The countdown is the guard.
    ///
    /// Drives the overwrite list in by hand rather than waiting on the
    /// real `git` read `open_force_push_confirm` starts: this is the
    /// state machine's test, and `resolve_overwritten` is the same entry
    /// point the load task uses.
    #[gpui::test]
    async fn the_confirm_control_stays_dead_for_the_countdown(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });
        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("confirmation is up");
            assert!(
                matches!(confirm.state, ForcePushConfirmState::Pending),
                "it must not offer anything before it knows what is at stake"
            );
            assert!(!confirm.is_armed());
        });

        dialog.update(&mut cx, |dialog, cx| {
            dialog.resolve_overwritten(
                OverwrittenCommits::Known(vec![server_commit("server work")]),
                cx,
            )
        });
        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("confirmation is up");
            let ForcePushConfirmState::Offered { countdown, .. } = confirm.state else {
                panic!("a non-empty overwrite list must offer the push");
            };
            assert_eq!(countdown, FORCE_PUSH_CONFIRM_DELAY_SECS);
            assert!(!confirm.is_armed(), "it must not arm on arrival");
        });

        // The reflex click.
        dialog.update(&mut cx, |dialog, cx| dialog.confirm_force_push(cx));
        dialog.update(&mut cx, |dialog, _| {
            assert!(
                dialog.force_confirm.is_some(),
                "an early press must not dismiss the confirmation"
            );
            assert!(!dialog.pushing, "and it must not start the push");
        });

        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, _| {
            assert_eq!(
                dialog.force_confirm.as_ref().and_then(|c| match c.state {
                    ForcePushConfirmState::Offered { countdown, .. } => Some(countdown),
                    _ => None,
                }),
                Some(FORCE_PUSH_CONFIRM_DELAY_SECS - 1),
                "the countdown must tick on the executor clock"
            );
        });

        cx.executor()
            .advance_clock(Duration::from_secs(FORCE_PUSH_CONFIRM_DELAY_SECS as u64));
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("still up");
            assert!(confirm.is_armed(), "the wait must end and the button arm");
        });
    }

    /// Nothing on the remote to overwrite: the force push is pointless,
    /// so it is refused outright rather than offered after a wait.
    #[gpui::test]
    async fn an_empty_overwrite_list_refuses_the_force_push(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });
        dialog.update(&mut cx, |dialog, cx| {
            dialog.resolve_overwritten(OverwrittenCommits::Known(Vec::new()), cx)
        });

        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("confirmation is up");
            assert!(
                matches!(
                    confirm.state,
                    ForcePushConfirmState::Refused(ForcePushRefusal::NothingToOverwrite)
                ),
                "an empty overwrite list must refuse, not offer"
            );
            assert!(!confirm.is_armed());
        });

        // No countdown may be running: waiting cannot turn a refusal into
        // an offer.
        cx.executor().advance_clock(Duration::from_secs(
            FORCE_PUSH_CONFIRM_DELAY_SECS as u64 * 4,
        ));
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, cx| {
            let confirm = dialog.force_confirm.as_ref().expect("still up");
            assert!(!confirm.is_armed(), "no wait may arm a refusal");
            dialog.confirm_force_push(cx);
        });
        dialog.update(&mut cx, |dialog, _| {
            assert!(!dialog.pushing, "a refused force push must never run");
            assert!(
                dialog.force_confirm.is_some(),
                "and must not silently close"
            );
        });
    }

    /// The list could not be read: refused outright — a confirmation the
    /// user cannot answer informedly is not a safeguard.
    #[gpui::test]
    async fn an_undeterminable_overwrite_list_refuses_the_force_push(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (dialog, mut cx) = init_push_dialog(ForceMode::WithLease, cx).await;

        dialog.update_in(&mut cx, |dialog, window, cx| {
            dialog.confirm_push(window, cx)
        });
        dialog.update(&mut cx, |dialog, cx| {
            dialog.resolve_overwritten(
                OverwrittenCommits::Unknown {
                    why: "this clone has no origin/main ref".into(),
                },
                cx,
            )
        });

        dialog.update(&mut cx, |dialog, _| {
            let confirm = dialog.force_confirm.as_ref().expect("confirmation is up");
            let ForcePushConfirmState::Refused(ForcePushRefusal::Undeterminable { why }) =
                &confirm.state
            else {
                panic!("an unreadable overwrite list must refuse, not offer");
            };
            assert!(why.contains("origin/main"), "got: {why}");
            assert!(!confirm.is_armed());
        });

        cx.executor().advance_clock(Duration::from_secs(
            FORCE_PUSH_CONFIRM_DELAY_SECS as u64 * 4,
        ));
        cx.run_until_parked();
        dialog.update(&mut cx, |dialog, cx| {
            assert!(
                !dialog.force_confirm.as_ref().expect("still up").is_armed(),
                "no wait may arm a refusal"
            );
            dialog.confirm_force_push(cx);
        });
        dialog.update(&mut cx, |dialog, _| {
            assert!(!dialog.pushing, "a blind force push must never run");
        });
    }

    /// The list the confirmation shows is the server-side work a force
    /// push drops — and unlike the dialog's ahead/behind preview it must
    /// include merge commits, which `--no-merges` hides.
    #[gpui::test]
    async fn overwritten_commits_lists_server_side_work_including_merges(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let (_tmp, local, remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));
        let other = local
            .parent()
            .expect("parent of local exists")
            .join("other-merger");
        std::fs::create_dir_all(&other).expect("mkdir");
        run_git_void(&other, &["clone", remote.to_str().unwrap_or_default(), "."])
            .await
            .expect("clone");
        run_git_void(&other, &["config", "user.email", "test@example.com"])
            .await
            .expect("config email");
        run_git_void(&other, &["config", "user.name", "Test"])
            .await
            .expect("config name");
        run_git_void(&other, &["checkout", "-b", "side"])
            .await
            .expect("checkout side");
        std::fs::write(other.join("side.txt"), "s").expect("write side");
        run_git_void(&other, &["add", "side.txt"])
            .await
            .expect("add");
        run_git_void(&other, &["commit", "-m", "side work"])
            .await
            .expect("commit side");
        run_git_void(&other, &["checkout", "main"])
            .await
            .expect("checkout main");
        run_git_void(&other, &["merge", "--no-ff", "side", "-m", "Merge side"])
            .await
            .expect("merge");
        run_git_void(&other, &["push", "origin", "main"])
            .await
            .expect("push");

        // Our own commit, so the local branch is not simply behind.
        std::fs::write(local.join("local.txt"), "x").expect("write local");
        run_git_void(&local, &["add", "local.txt"])
            .await
            .expect("add");
        run_git_void(&local, &["commit", "-m", "local commit"])
            .await
            .expect("commit");
        run_git_void(&local, &["fetch", "origin"])
            .await
            .expect("fetch");

        let OverwrittenCommits::Known(commits) =
            overwritten_commits(&local, "main", "origin", "main").await
        else {
            panic!("a readable tracking ref must produce a list");
        };
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert!(
            subjects.contains(&"side work"),
            "the commits only on the server must be listed, got: {subjects:?}"
        );
        assert!(
            subjects.contains(&"Merge side"),
            "a merge commit lost is still work lost, got: {subjects:?}"
        );
        assert!(
            !subjects.contains(&"local commit"),
            "our own commits are not what the push overwrites, got: {subjects:?}"
        );
        assert!(
            commits.iter().all(|c| !c.author_email.is_empty()
                && c.committer_date_unix > 0
                && !c.subject.is_empty()),
            "each row needs a subject, a date and an author: {commits:?}"
        );

        // This is exactly why the list is re-derived rather than read off
        // the preview: the preview drops merges.
        let preview = build_preview(&local, "main", "")
            .await
            .unwrap_or_else(|e| panic!("preview: {e}"));
        let behind: Vec<&str> = preview.behind.iter().map(|c| c.subject.as_str()).collect();
        assert!(
            !behind.contains(&"Merge side"),
            "guard: if the preview ever starts including merges this test is moot"
        );
        for subject in &behind {
            assert!(
                subjects.contains(subject),
                "the overwrite list must be a superset of the behind set, missing {subject}"
            );
        }
    }

    /// A tracking ref this clone has never fetched is indistinguishable
    /// from a brand-new remote branch, so it must read as "cannot tell",
    /// never as an empty (reassuring) list.
    #[gpui::test]
    async fn overwritten_commits_treats_an_unreadable_ref_as_dangerous(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let (_tmp, local, _remote) = boot_repo().await.unwrap_or_else(|e| panic!("boot: {e}"));

        let outcome = overwritten_commits(&local, "main", "origin", "never-fetched").await;
        let OverwrittenCommits::Unknown { why } = outcome else {
            panic!("an unreadable ref must not resolve to a list: {outcome:?}");
        };
        assert!(
            why.contains("origin/never-fetched") && why.contains("fetch"),
            "the reason must say what could not be read and what to do: {why}"
        );
    }
}
