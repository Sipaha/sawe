//! "Compact context" workflow: dump the current session's running summary to handoff files, then continue in a fresh ACP session.

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Context, SharedString, TaskExt as _};
use solutions::SolutionStore;
use workspace::notifications::{NotificationId, simple_message_notification::MessageNotification};

use crate::model::{SessionState, SolutionSessionId};
use crate::session_view::SolutionSessionView;
use crate::status_row::DEFAULT_CONTEXT_WINDOW;
use crate::store::SolutionAgentStore;

/// Outcome of [`start_compact_for_session`] — distinguishes "we ran the
/// orchestration and the prompt is now queued on the agent" from "we
/// declined to compact and here's why". Errors out only when the
/// session id is unknown or the underlying filesystem refuses to create
/// the dump directory (the two cases that no client retry can fix
/// without operator intervention).
#[derive(Debug, Clone)]
pub(crate) struct StartCompactOutcome {
    pub queued: bool,
    /// Human-readable reason when `queued == false`. `None` when queued
    /// successfully — keeps the success path cheap on the wire.
    pub reason: Option<String>,
}

/// Shared orchestration: precondition gate → render the compact prompt
/// (creates the `<root>/.agents/<sid>/c<NN>/` dump dir as a side effect)
/// → enqueue the rendered prompt as a user message on the live
/// `AcpThread`. Driven by both the desktop status-row popover and the
/// `solution_agent.start_compact` MCP tool so the two surfaces share a
/// single notion of "is this session compactable right now".
///
/// The cold-session branch is intentionally NOT in here: queueing on a
/// `SolutionSessionView` requires `&mut Window`, which the MCP path
/// doesn't have. The desktop entry point handles cold separately via
/// `start_compact_from_cold` on the navigator.
pub(crate) fn start_compact_for_session(
    session_id: SolutionSessionId,
    cx: &mut App,
) -> Result<StartCompactOutcome> {
    let store = SolutionAgentStore::global(cx);
    let session_entity = store
        .read_with(cx, |s, _| s.session(session_id))
        .ok_or_else(|| anyhow!("unknown session {session_id}"))?;

    // Precondition: must be Idle. A Running/AwaitingInput session would
    // race with the in-flight turn (claude-acp queues prompts in
    // `pending_messages`, which would deliver the compact instructions
    // AFTER the active turn — possibly minutes later — and surprise
    // the user). Cold (sleeping) sessions ARE compactable here: they read
    // as `Idle`, and the `store.send_message` below wakes them windowless
    // via `send_message_blocks_with_wake` (the desktop UI's
    // `start_compact_from_cold` does the same with a `&mut Window`; the MCP
    // path doesn't need one). This is what lets a paired phone compact a
    // sleeping session in one tap.
    {
        let s = session_entity.read(cx);
        if !matches!(s.state, SessionState::Idle) {
            return Ok(StartCompactOutcome {
                queued: false,
                reason: Some(format!(
                    "session is busy ({:?}); wait for the current turn to finish",
                    s.state
                )),
            });
        }

        // Precondition: meaningful context to compact AND headroom to
        // dump the summary. Matches `status_row::render_status_row`'s
        // gate so MCP and the desktop UI agree on "compactable".
        let usage = s
            .acp_thread()
            .and_then(|thread| thread.read(cx).token_usage().cloned());
        let used = usage
            .as_ref()
            .map(|u| u.used_tokens)
            .or(s.cached_total_tokens)
            .unwrap_or(0);
        let max = usage
            .as_ref()
            .map(|u| u.max_tokens)
            .filter(|m| *m > 0)
            .or(s.cached_max_tokens)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let pct = if max == 0 {
            0.0
        } else {
            (used as f64 / max as f64).clamp(0.0, 1.0)
        };
        let remaining = max.saturating_sub(used);
        if pct < COMPACT_BUTTON_MIN_PCT {
            return Ok(StartCompactOutcome {
                queued: false,
                reason: Some(format!(
                    "conversation is short ({:.1}%); compact later",
                    pct * 100.0
                )),
            });
        }
        if remaining < COMPACT_HEADROOM_MIN_TOKENS {
            return Ok(StartCompactOutcome {
                queued: false,
                reason: Some(format!(
                    "only {} tokens of headroom left — start a fresh session manually",
                    remaining
                )),
            });
        }
    }

    let rendered = render_compact_prompt_inner(session_id, cx)?;
    store.update(cx, |store, cx| {
        store
            .send_message(session_id, rendered, cx)
            .detach_and_log_err(cx);
    });
    Ok(StartCompactOutcome {
        queued: true,
        reason: None,
    })
}

/// Render the compact-instruction template for `session_id` and create
/// the per-rotation dump directory. Free-function counterpart of the
/// navigator's `render_compact_prompt` — returns an `anyhow::Error` so
/// MCP callers get a structured error instead of a workspace toast.
pub(crate) fn render_compact_prompt_inner(
    session_id: SolutionSessionId,
    cx: &mut App,
) -> Result<String> {
    let store = SolutionAgentStore::global(cx);
    let session_entity = store
        .read_with(cx, |s, _| s.session(session_id))
        .ok_or_else(|| anyhow!("unknown session {session_id}"))?;
    let (solution_id, agent_id, started_at, context_count, used, max) = {
        let s = session_entity.read(cx);
        let context_count = s.context_count;
        // Live `token_usage` when hot, else fall back to `cached_total_tokens`
        // so a cold caller still gets a meaningful prompt header.
        let usage = s
            .acp_thread()
            .and_then(|thread| thread.read(cx).token_usage().cloned());
        let used = usage
            .as_ref()
            .map(|u| u.used_tokens)
            .or(s.cached_total_tokens)
            .unwrap_or(0);
        let max = usage
            .as_ref()
            .map(|u| u.max_tokens)
            .filter(|m| *m > 0)
            .or(s.cached_max_tokens)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        (
            s.solution_id.clone(),
            s.agent_id.clone(),
            s.created_at,
            context_count,
            used,
            max,
        )
    };

    let solution_root = SolutionStore::try_global(cx)
        .and_then(|store| {
            store.read_with(cx, |s, _| {
                s.solutions()
                    .iter()
                    .find(|sol| sol.id == solution_id)
                    .map(|sol| sol.root.clone())
            })
        })
        .ok_or_else(|| {
            anyhow!(
                "Compact failed: solution {:?} not registered",
                solution_id.0
            )
        })?;

    // `<root>/.agents/<sid>/c<count>/` — `c01`, `c02`, … so a
    // single `<sid>` directory groups every rotation of one
    // logical conversation. The leading `c` keeps the names from
    // accidentally colliding with the legacy timestamp scheme.
    let context_label = format!("c{context_count:02}");
    let compact_dir = solution_root
        .join(".agents")
        .join(session_id.to_string())
        .join(&context_label);
    std::fs::create_dir_all(&compact_dir).map_err(|err| {
        anyhow!(
            "Compact failed: cannot create {}: {err}",
            compact_dir.display()
        )
    })?;

    // Bound disk over a long multi-day session: each rotation writes a fresh
    // `cNN/` handoff dir (~15KB) and they are never otherwise deleted. Keep the
    // most recent `COMPACT_DIR_RETENTION` and prune older ones. Safe: resume
    // only ever uses the LATEST rotation's `continue.md`, and the `done`
    // aggregation reads recent `state.md` summaries (each is itself cumulative),
    // so older rotations are historical detail, not load-bearing.
    if let Some(session_dir) = compact_dir.parent() {
        prune_old_compact_dirs(session_dir, context_count);
    }

    let mut compact_dir_str = compact_dir.to_string_lossy().to_string();
    if !compact_dir_str.ends_with(std::path::MAIN_SEPARATOR) {
        compact_dir_str.push(std::path::MAIN_SEPARATOR);
    }

    // The per-solution MCP socket — `solution_agent.compact_session` is a
    // solution-scoped tool, so it lives ONLY on this socket, never on the
    // editor-global `~/.spk/sawe/config/mcp.sock`. The template hands the
    // agent the literal path so it can `nc -U` it directly instead of
    // guessing (or hitting "Tool not found" on the global socket). The
    // per-solution socket is bound for every OPEN Solution (see
    // `editor_mcp` solution-socket lifecycle driven off
    // `SolutionStoreEvent::Opened/Closed`), so it is present regardless of
    // which Solution is the foreground one.
    let solution_socket = editor_mcp::solution_socket_path(solution_id.0.as_str())
        .to_string_lossy()
        .into_owned();

    Ok(COMPACT_INSTRUCTIONS_TEMPLATE
        .replace("{{session_id}}", &session_id.to_string())
        .replace("{{compact_dir}}", &compact_dir_str)
        .replace("{{solution_socket}}", &solution_socket)
        .replace("{{solution_id}}", solution_id.0.as_str())
        .replace("{{agent_id}}", agent_id.as_ref())
        .replace("{{started_at_iso}}", &started_at.to_rfc3339())
        .replace("{{tokens_used}}", &used.to_string())
        .replace("{{tokens_max}}", &max.to_string()))
}

/// How many most-recent `cNN/` rotation handoff dirs to keep per session.
const COMPACT_DIR_RETENTION: u32 = 20;

/// Delete `cNN/` handoff dirs older than the retention window. `current` is the
/// rotation just created; keeps `cNN` where `NN > current - COMPACT_DIR_RETENTION`
/// and removes the rest. Best-effort — any IO error is ignored (these are
/// historical handoff snapshots, never load-bearing for resume). Only touches
/// children named exactly `c<digits>`, so sibling files/dirs (`supervisor/`,
/// `session-log.md`, `inbox/`, …) are never affected.
fn prune_old_compact_dirs(session_dir: &std::path::Path, current: u32) {
    let cutoff = current.saturating_sub(COMPACT_DIR_RETENTION);
    if cutoff == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(session_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(num) = name
            .to_str()
            .and_then(|n| n.strip_prefix('c'))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if num <= cutoff && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

impl SolutionSessionView {
    /// Renders the current compact-instruction template, creates the
    /// per-rotation handoff directory, and ships the rendered prompt as
    /// a regular user message. The agent then writes its summary files
    /// into that directory and (after we've handed it `compact_dir`)
    /// calls back via `solution_agent.compact_session`.
    pub(crate) fn start_compact(&self, cx: &mut Context<Self>) {
        let session_id = self.session_id();
        match start_compact_for_session(session_id, cx) {
            Ok(StartCompactOutcome { queued: true, .. }) => {}
            Ok(StartCompactOutcome {
                queued: false,
                reason: Some(reason),
            }) => {
                log::info!("solution_agent compact declined: {reason}");
            }
            Ok(StartCompactOutcome {
                queued: false,
                reason: None,
            }) => {}
            Err(err) => {
                self.toast_compact_error(SharedString::from(format!("Compact failed: {err}")), cx);
            }
        }
    }

    /// Render the compact-instruction template for the active session and
    /// create the `<root>/.agents/<sid>/c<NN>/` dump directory. Returns the
    /// rendered prompt body. Surfaces a workspace toast and returns `None`
    /// on the same failure modes the inline path used to handle (unknown
    /// solution, mkdir failure).
    pub(crate) fn render_compact_prompt(&self, cx: &mut Context<Self>) -> Option<String> {
        let session_id = self.session_id();
        match render_compact_prompt_inner(session_id, cx) {
            Ok(rendered) => Some(rendered),
            Err(err) => {
                self.toast_compact_error(SharedString::from(err.to_string()), cx);
                None
            }
        }
    }

    /// Cold-state compact: render the prompt now, queue it as
    /// `pending_send`, and kick off `start_resume`. The existing wake-flush
    /// hook (`flush_pending_send_if_ready`) dispatches the queued prompt
    /// the moment `acp_thread` becomes `Some`. Status badge sequence the
    /// user sees: `Sleeping → Resuming… → Thinking… → Idle`.
    ///
    /// No-ops if there's no rendered prompt (template render + mkdir
    /// already toasted the failure).
    pub(crate) fn start_compact_from_cold(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rendered) = self.render_compact_prompt(cx) else {
            return;
        };
        self.enqueue_text_pending_send_and_resume(rendered, window, cx);
    }

    fn toast_compact_error(&self, message: SharedString, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace_handle().upgrade() else {
            log::warn!("solution_agent toast (no workspace): {message}");
            return;
        };
        workspace.update(cx, |workspace, cx| {
            struct CompactFailed;
            workspace.show_notification(NotificationId::unique::<CompactFailed>(), cx, move |cx| {
                cx.new(|cx| MessageNotification::new(message, cx))
            });
        });
    }
}

/// Compact button activation threshold. Below this the conversation is
/// too short for a compact to be worth the round-trip.
pub(crate) const COMPACT_BUTTON_MIN_PCT: f64 = 0.10;

/// Threshold at which the compact button paints in warning colour.
/// Past this, the user should rotate before the model starts dropping
/// context off the back of the window.
pub(crate) const COMPACT_BUTTON_WARN_PCT: f64 = 0.50;

/// Minimum free tokens we require before allowing a compact: enough
/// for the instruction prompt (~3 k) and the agent's dump (state.md +
/// decisions.md + next.md + continue.md, typically ~10–20 k combined),
/// plus a buffer for tool-call traces. Below this, refuse the button —
/// a half-truncated compact loses more than just starting over does.
pub(crate) const COMPACT_HEADROOM_MIN_TOKENS: u64 = 30_000;

/// Markdown template fed to the agent on compact. `{{var}}` placeholders
/// are filled from session state at click time. Source-of-truth lives in
/// the resources file so the prose can be reviewed without recompiling.
const COMPACT_INSTRUCTIONS_TEMPLATE: &str =
    include_str!("../resources/compact_context_instructions.md");

/// First heading of the compact-instructions template. The conversation
/// renderer matches user messages against this to fold the (large,
/// agent-only) compact prompt into a one-line placeholder instead of
/// dumping the whole template into the chat the user has to scroll past.
/// `compaction_template_starts_with_heading` keeps this in lockstep with
/// the resource file so the match can never silently drift. If you change
/// the template's first line, change this too (and the mobile client's
/// copy in `SessionDetailScreen.kt`).
pub(crate) const COMPACT_PROMPT_HEADING: &str =
    "# Compact this session and prepare a clean handoff";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AdapterRegistry;
    use crate::model::SolutionSessionId;
    use gpui::{TestAppContext, VisualTestContext};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// The renderer (desktop `conversation_render::is_compaction_prompt_text`
    /// and the mobile `SessionDetailScreen.kt`) folds the compact prompt by
    /// matching its first heading against `COMPACT_PROMPT_HEADING`. If the
    /// template's opening line ever drifts from that constant, the fold
    /// silently stops working — assert they stay in lockstep.
    #[test]
    fn compaction_template_starts_with_heading() {
        assert!(
            COMPACT_INSTRUCTIONS_TEMPLATE
                .trim_start()
                .starts_with(COMPACT_PROMPT_HEADING),
            "compact template's first heading must match COMPACT_PROMPT_HEADING; \
             template starts with: {:?}",
            &COMPACT_INSTRUCTIONS_TEMPLATE[..COMPACT_INSTRUCTIONS_TEMPLATE.len().min(80)]
        );
    }

    /// Cold-compact orchestrator must:
    ///   1. Render the compact instructions prompt (template variables
    ///      replaced with cached cold-state values).
    ///   2. Queue it as a single-block `pending_send` on the view.
    ///   3. Set `resuming = true` so the badge flips to `Resuming…`.
    ///
    /// Assertions are checked synchronously — before `run_until_parked()` —
    /// so the spawned `resume_session` task never fires and we don't have
    /// to mock the full ACP handshake. The workspace entity is kept alive
    /// for the duration of the test so `start_resume`'s synchronous
    /// `workspace.upgrade()` check returns `Some` and does not clear
    /// `pending_send` / `resuming` before we can read them.
    #[test]
    fn prune_old_compact_dirs_keeps_recent_window() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path();
        for n in 1..=25u32 {
            std::fs::create_dir_all(session_dir.join(format!("c{n:02}"))).unwrap();
        }
        // Sibling non-`cNN` entries must survive untouched.
        std::fs::create_dir_all(session_dir.join("supervisor")).unwrap();
        std::fs::write(session_dir.join("session-log.md"), b"x").unwrap();

        prune_old_compact_dirs(session_dir, 25);

        // cutoff = 25 - 20 = 5 → delete c01..c05, keep c06..c25.
        for n in 1..=5u32 {
            assert!(
                !session_dir.join(format!("c{n:02}")).exists(),
                "c{n:02} should be pruned"
            );
        }
        for n in 6..=25u32 {
            assert!(
                session_dir.join(format!("c{n:02}")).exists(),
                "c{n:02} should be kept"
            );
        }
        assert!(session_dir.join("supervisor").exists());
        assert!(session_dir.join("session-log.md").exists());
    }

    #[gpui::test]
    async fn cold_compact_queues_prompt_and_kicks_resume(cx: &mut TestAppContext) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        let agent_id = gpui::SharedString::from("mock-agent");

        cx.update(|cx| {
            // `Workspace::new` calls `theme_settings::track_window_appearance`
            // which requires `GlobalSystemAppearance` to be initialized.
            theme_settings::init(theme::LoadThemes::JustBase, cx);

            let registry = Arc::new(AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, _| {
                store.register_agent_server(
                    agent_id.clone(),
                    Rc::new(crate::test_support::MockAgentServer::new(Arc::new(
                        AtomicUsize::new(0),
                    ))),
                );
            });
        });

        let session_id = SolutionSessionId::new();

        // Open a Workspace window so `start_resume` can synchronously
        // upgrade `self.workspace` — without a valid workspace entity,
        // `start_resume` immediately clears `pending_send` + `resuming`.
        let workspace_window =
            cx.add_window(|window, cx| workspace::Workspace::test_new(project.clone(), window, cx));

        // Obtain a weak handle to the workspace entity BEFORE creating
        // the `VisualTestContext` so we can call `workspace_window.root`
        // without a re-entrant `update_window` (which would deadlock
        // because `vcx.update` already holds the window lock).
        let workspace_weak = cx.update(|cx| {
            workspace_window
                .root(cx)
                .expect("workspace window is alive")
                .downgrade()
        });

        let mut vcx = VisualTestContext::from_window(*workspace_window, cx);

        let view_entity = vcx.update(|window, cx| {
            let store = SolutionAgentStore::global(cx);
            let session = store.update(cx, |store, cx| {
                crate::store::tests::insert_cold_session(
                    session_id,
                    solution_id.clone(),
                    agent_id.clone(),
                    Some(120_000),
                    Some(project.clone()),
                    store,
                    cx,
                )
            });

            cx.new(|cx| {
                crate::session_view::SolutionSessionView::for_test(
                    session_id,
                    session,
                    workspace_weak.clone(),
                    window,
                    cx,
                )
            })
        });

        vcx.update(|window, cx| {
            view_entity.update(cx, |view, cx| {
                view.start_compact_from_cold(window, cx);
            });
        });

        vcx.update(|_window, cx| {
            view_entity.read_with(cx, |view, _| {
                let pending = view
                    .pending_send_for_test()
                    .expect("pending_send populated after start_compact_from_cold");
                assert_eq!(pending.len(), 1, "exactly one content block");
                let agent_client_protocol::schema::ContentBlock::Text(text) = &pending[0] else {
                    panic!("expected text block, got {:?}", pending[0]);
                };
                assert!(
                    !text.text.contains("{{compact_dir}}"),
                    "template variable {{{{compact_dir}}}} must be resolved; got: {:?}",
                    &text.text[..text.text.len().min(200)]
                );
                assert!(
                    text.text.contains(session_id.as_str()),
                    "rendered prompt must contain session_id={session_id}",
                );
                assert!(view.is_resuming(), "resuming flag set after enqueue");
            });
        });
    }

    /// The MCP `start_compact` path (a paired phone tapping "Compact" on a
    /// sleeping session) must wake the session and queue the compact prompt
    /// rather than declining. `store.send_message` already wakes cold
    /// sessions windowless via `send_message_blocks_with_wake`, so the
    /// orchestrator no longer needs a `&mut Window` for the cold case.
    #[gpui::test]
    async fn cold_session_above_gate_queues_compact_via_mcp(cx: &mut TestAppContext) {
        let (solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;
        let agent_id = gpui::SharedString::from("mock-agent");

        cx.update(|cx| {
            let registry = Arc::new(AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, _| {
                store.register_agent_server(
                    agent_id.clone(),
                    Rc::new(crate::test_support::MockAgentServer::new(Arc::new(
                        AtomicUsize::new(0),
                    ))),
                );
            });
        });

        let session_id = SolutionSessionId::new();

        cx.update(|cx| {
            let store = SolutionAgentStore::global(cx);
            store.update(cx, |store, cx| {
                crate::store::tests::insert_cold_session(
                    session_id,
                    solution_id.clone(),
                    agent_id.clone(),
                    // 50% of the 1.0M default window → comfortably above the
                    // 10% COMPACT_BUTTON_MIN_PCT gate, with ample headroom.
                    Some(500_000),
                    Some(project.clone()),
                    store,
                    cx,
                );
            });
        });

        let outcome = cx
            .update(|cx| start_compact_for_session(session_id, cx))
            .expect("start_compact_for_session dispatches");

        assert!(
            outcome.queued,
            "cold session above the usage gate must queue a compact; got reason={:?}",
            outcome.reason
        );
    }
}
