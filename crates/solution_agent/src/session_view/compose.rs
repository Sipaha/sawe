//! Compose-row submission: the send / queue / interrupt paths, cold-tab
//! deferred send, slash-command validation, and the recalled-bundle restore.
//! Relocated verbatim from the view root as `impl SolutionSessionView`
//! methods; `self`/fields stay owned by the struct.

use agent_client_protocol::schema as acp;
use gpui::{App, Context, FollowMode, Focusable, SharedString, TaskExt as _, Window};

use super::{SolutionSessionView, retain_images_with_live_placeholder};
use crate::model::SessionState;
use crate::store::SolutionAgentStore;

impl SolutionSessionView {
    pub(super) fn submit_compose_action(
        &mut self,
        _: &menu::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `menu::Confirm` is the catch-all "Enter" action and bubbles up the
        // focus chain. If focus isn't actually in the compose editor (e.g.
        // user is in the find bar, or just clicked into the conversation
        // body and pressed Enter), do nothing — sending stale draft text
        // because something elsewhere generated a Confirm event would be a
        // destructive surprise. Send button click goes through
        // `submit_compose_now`, bypassing this guard.
        let compose_focus = self.compose_editor.read(cx).focus_handle(cx);
        if !compose_focus.is_focused(window) {
            return;
        }
        self.submit_compose_now(window, cx);
    }

    /// If a queued bundle was previously pulled into the compose editor
    /// via `recall_queued_message`, push it back into
    /// `pending_messages`, clear the compose box, and drop any pending
    /// images. Returns `true` when a restore happened so the caller can
    /// stop further Esc handling. No-op + `false` when there's nothing
    /// to restore.
    pub(super) fn restore_recalled_bundle(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(bundle) = self.recalled_bundle.take() else {
            return false;
        };
        let session_id = self.session_id;
        log::info!(
            target: "solution_agent::queue",
            "session={session_id} restored recalled bundle into pending_messages (Esc cancel-edit)",
        );
        self.session.update(cx, |session, _| {
            // Push to back — the queue conceptually has at most one
            // bundle (per `send_message_blocks` merge logic) and the
            // recalled bundle was the back element when popped.
            session.pending_messages.push_back(bundle);
        });
        self.compose_editor.update(cx, |e, cx| e.clear(window, cx));
        self.pending_images.clear();
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            // `SolutionSession.state` is unchanged here (only the queue moved) —
            // emit the bare `SessionStateChanged` for the desktop re-render
            // without bumping `state_seq`. The queue change is carried by
            // `mark_queue_changed` below.
            cx.emit(crate::store::SolutionAgentStoreEvent::SessionStateChanged(
                session_id,
            ));
            // The bundle just landed back in pending_messages —
            // broadcast so paired clients (mobile) re-render the
            // restored Queued bubble.
            store.mark_queue_changed(session_id, cx);
        });
        cx.notify();
        true
    }

    fn submit_compose_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compose_disabled(cx) {
            // Shell view: a read-only background-shell transcript, not a live
            // agent — there is nothing to send to. Silently drop — the UI also
            // hides the Send button, this guard catches keybinding paths.
            return;
        }
        let content = self.compose_editor.read(cx).text(cx);
        // Reconcile attachments against the text: an attachment whose
        // `[image #N]` placeholder the user deleted is a removed attachment and
        // must not be sent.
        retain_images_with_live_placeholder(&content, &mut self.pending_images);
        if content.trim().is_empty() && self.pending_images.is_empty() {
            return;
        }
        if self.resuming {
            // Already waiting for `resume_session` to attach the agent
            // — ignore extra Send presses so we don't fire multiple
            // resume tasks for the same cold session. Log so a "Send
            // looks broken / nothing happens" report has a breadcrumb
            // showing the press WAS received, just suppressed.
            let session_id = self.session_id;
            let images = self.pending_images.len();
            let chars = content.chars().count();
            log::info!(
                target: "solution_agent::queue",
                "session={session_id} submit_compose_now suppressed (resuming=true) — would-have-sent text_chars={chars} images={images}",
            );
            return;
        }
        // Submitting supersedes any recalled-edit draft — the modified
        // text is the new authoritative version, drop the original
        // bundle stash so a follow-up Esc doesn't push it back as a
        // duplicate.
        self.recalled_bundle = None;
        // Audit log: every Send/Queue press lands here. Followed by
        // a downstream `enqueued` / `flushing` / `dropped` line from
        // `queue.rs` / `handle_acp_event`, so a missing pair pinpoints
        // exactly where a message vanished.
        {
            let session_id = self.session_id;
            let state_label = match self.session.read(cx).state {
                SessionState::Running { .. } => "Running",
                SessionState::Stopping { .. } => "Stopping",
                SessionState::Idle => "Idle",
                SessionState::AwaitingInput => "AwaitingInput",
                SessionState::Errored(_) => "Errored",
            };
            let is_cold = self.session.read(cx).is_cold();
            log::info!(
                target: "solution_agent::queue",
                "session={session_id} submit_compose_now state={state_label} cold={is_cold} text_chars={} images={}",
                content.chars().count(),
                self.pending_images.len(),
            );
        }
        if self.session.read(cx).is_cold() {
            // Cold tab: defer the actual send until the agent
            // subprocess is running. Pre-flight slash-command
            // validation here too so a typo is caught before the
            // 3-4s resume wait.
            if let Some(rejection) = self.validate_slash_command(&content, cx) {
                self.show_toast(rejection, cx);
                return;
            }
            let mut blocks: Vec<acp::ContentBlock> = Vec::new();
            if !content.trim().is_empty() {
                blocks.push(acp::ContentBlock::Text(acp::TextContent::new(content)));
            }
            for image in std::mem::take(&mut self.pending_images) {
                blocks.push(acp::ContentBlock::Image(acp::ImageContent::new(
                    image.data_base64,
                    image.mime_type,
                )));
            }
            if blocks.is_empty() {
                return;
            }
            self.compose_editor.update(cx, |e, cx| e.clear(window, cx));
            self.pending_send = Some(blocks);
            self.resuming = true;
            self.start_resume(window, cx);
            cx.notify();
            return;
        }
        // Pre-flight slash-command validation so a typo'd `/clearr` doesn't
        // disappear silently into the agent (where it gets treated as a
        // plain prompt). Show a toast and bail; user fixes the typo and
        // resends. Commands without arguments that the agent advertises
        // pass through as text — claude-acp parses them server-side.
        if let Some(rejection) = self.validate_slash_command(&content, cx) {
            self.show_toast(rejection, cx);
            return;
        }
        // `/clear` is intercepted client-side and translated into a fresh
        // ACP session under the same SolutionSessionId. Forwarding it to
        // the agent would clear the SDK's internal context but leave our
        // local `AcpThread.entries` (and the rendered conversation) as-is;
        // rotating is agent-agnostic and gives a guaranteed-clean slate
        // including a reset usage meter. Pending images are dropped — the
        // user explicitly asked to wipe the conversation.
        if content.trim() == "/clear" {
            self.compose_editor.update(cx, |e, cx| e.clear(window, cx));
            self.pending_images.clear();
            let session_id = self.session_id;
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.reset_context(session_id, cx).detach_and_log_err(cx);
            });
            return;
        }
        self.compose_editor.update(cx, |e, cx| e.clear(window, cx));
        // Sending implies "I want to follow what happens next." Re-stick to
        // the bottom even if the user had scrolled up to read older context.
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();
        let session_id = self.session_id;
        // Route the follow-up: every tab routes to Main — teammate/shell tabs
        // are view-only since the per-source-streams fold.
        let target = crate::model::QueueTarget::Main;

        if self.pending_images.is_empty() {
            let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(content))];
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store
                    .send_message_blocks_targeted(session_id, blocks, target, true, cx)
                    .detach_and_log_err(cx);
            });
            return;
        }

        let images = std::mem::take(&mut self.pending_images);
        let mut blocks: Vec<acp::ContentBlock> = Vec::with_capacity(images.len() + 1);
        if !content.trim().is_empty() {
            blocks.push(acp::ContentBlock::Text(acp::TextContent::new(content)));
        }
        for image in images {
            blocks.push(acp::ContentBlock::Image(acp::ImageContent::new(
                image.data_base64,
                image.mime_type,
            )));
        }
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .send_message_blocks_targeted(session_id, blocks, target, true, cx)
                .detach_and_log_err(cx);
        });
    }

    /// Drop a single text block into `pending_send` and start the
    /// cold-resume handshake. Used by callers outside the compose path
    /// that need to drive the same "wake the agent, then send" flow
    /// without going through the editor. No images supported — the
    /// argument must be plain text.
    pub(crate) fn enqueue_text_pending_send_and_resume(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() {
            return;
        }
        if self.resuming {
            // Already mid-resume from an earlier Send — refuse to
            // double-fire. The compose-side suppression in
            // `submit_compose_now` does the same and logs; mirror that
            // here so a missing resume has a breadcrumb.
            let session_id = self.session_id;
            log::info!(
                target: "solution_agent::queue",
                "session={session_id} enqueue_text_pending_send_and_resume suppressed (resuming=true)",
            );
            return;
        }
        let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(text))];
        self.pending_send = Some(blocks);
        self.resuming = true;
        self.start_resume(window, cx);
        cx.notify();
    }

    /// Drain `pending_send` once the session has gone live (acp_thread
    /// attached). Called from the session-observe callback so the
    /// dispatch happens on the same tick the resume completes.
    pub(crate) fn flush_pending_send_if_ready(&mut self, cx: &mut Context<Self>) {
        let Some(blocks) = self.pending_send.take() else {
            return;
        };
        if self.session.read(cx).acp_thread().is_none() {
            // Resume hasn't attached the thread yet — keep waiting.
            self.pending_send = Some(blocks);
            return;
        }
        self.resuming = false;
        let session_id = self.session_id;
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            store
                .send_message_blocks(session_id, blocks, cx)
                .detach_and_log_err(cx);
        });
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// Queue whatever is in the compose box (if anything) and then
    /// interrupt the running turn so the agent picks up the queue
    /// immediately. Wired to the lightning-bolt "Send now" button that
    /// appears next to Stop while a turn is running and the user has
    /// queued follow-ups (or is about to queue one). On a session that
    /// is NOT running this falls through to a regular send so the
    /// button stays useful if the agent flips to Idle between render
    /// and click.
    pub(crate) fn submit_compose_and_interrupt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compose_disabled(cx) {
            // Shell view: see `submit_compose_now`. Skip both the send AND the
            // interrupt — interrupting the parent thread while the user is
            // viewing a read-only background-shell transcript would surprise them.
            return;
        }
        let was_running = matches!(self.session.read(cx).state, SessionState::Running { .. });
        let had_compose_input = !self.compose_editor.read(cx).is_empty(cx);
        if had_compose_input {
            self.submit_compose_now(window, cx);
        }
        if !was_running {
            return;
        }
        let session_id = self.session_id;
        SolutionAgentStore::global(cx).update(cx, |store, cx| {
            if let Err(err) = store.interrupt_and_flush_pending(session_id, cx) {
                log::warn!("solution_agent: interrupt_and_flush_pending failed: {err:#}");
            }
        });
    }

    /// Returns `Some(error_message)` if `text` starts with a `/command` form
    /// the agent did not advertise (or with a known command that requires
    /// an argument but none was given). `None` means the message is fine to
    /// send as-is. Bare `/` and any text not starting with `/` always pass.
    fn validate_slash_command(&self, text: &str, cx: &App) -> Option<SharedString> {
        let trimmed = text.trim_start();
        if !trimmed.starts_with('/') {
            return None;
        }
        let first_line = trimmed.lines().next().unwrap_or("");
        let after_slash = &first_line[1..];
        let (name, rest) = match after_slash.find(char::is_whitespace) {
            Some(idx) => (&after_slash[..idx], after_slash[idx..].trim()),
            None => (after_slash, ""),
        };
        if name.is_empty() {
            return None;
        }
        let commands = self
            .session
            .read(cx)
            .acp_thread()
            .map(|thread| thread.read(cx).available_commands().to_vec())
            .unwrap_or_default();
        let matched = commands.iter().find(|cmd| cmd.name == name);
        match matched {
            None => {
                let mut available = commands
                    .iter()
                    .map(|cmd| format!("/{}", cmd.name))
                    .collect::<Vec<_>>();
                available.sort();
                let suffix = if available.is_empty() {
                    "The agent has not advertised any commands.".to_string()
                } else {
                    format!("Available: {}", available.join(", "))
                };
                Some(format!("Unknown command /{name}. {suffix}").into())
            }
            Some(cmd) if cmd.input.is_some() && rest.is_empty() => {
                let hint = cmd
                    .input
                    .as_ref()
                    .and_then(|input| match input {
                        acp::AvailableCommandInput::Unstructured(payload) => {
                            Some(payload.hint.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                let detail = if hint.is_empty() {
                    String::new()
                } else {
                    format!(" ({hint})")
                };
                Some(format!("/{name} requires an argument{detail}.").into())
            }
            Some(_) => None,
        }
    }
}
