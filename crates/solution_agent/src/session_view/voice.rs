use std::{collections::VecDeque, time::Duration};

use audio::voice::{CancellablePlayback, VoiceCapture, VoiceCaptureEvent};
use gpui::{App, Context, IntoElement, Task, Window, div};
use settings::Settings as _;
use ui::{IconButton, IconName, Tooltip, prelude::*};

use super::SolutionSessionView;
use crate::{
    actions::{StopSpeaking, ToggleReadAloud, ToggleVoiceConversation, ToggleVoiceRecording},
    agent_settings::SolutionAgentSettings,
    model::SessionState,
    session_entry::{AssistantChunk, SessionEntry, SessionEntryKind},
    store::SolutionAgentStore,
    voice_service::{VoiceService, speech_text},
};

#[derive(Default)]
pub(super) struct SessionVoice {
    capture: Option<VoiceCapture>,
    continuous: bool,
    finishing: bool,
    user_speaking: bool,
    read_aloud: bool,
    generation: u64,
    speech_generation: u64,
    pending_recordings: VecDeque<Vec<u8>>,
    pending_speech: VecDeque<String>,
    transcription: Option<Task<()>>,
    synthesis: Option<Task<()>>,
    playback: Option<CancellablePlayback>,
    poll: Option<Task<()>>,
    keepalive: Option<Task<()>>,
    unsupported_keepalive_url: Option<String>,
    reply_start: Option<usize>,
    status: &'static str,
}

impl SessionVoice {
    fn speaking(&self) -> bool {
        self.synthesis.is_some() || self.playback.is_some() || !self.pending_speech.is_empty()
    }

    fn cancel_speech(&mut self) {
        self.speech_generation = self.speech_generation.wrapping_add(1);
        self.synthesis.take();
        self.pending_speech.clear();
        self.playback.take();
    }

    fn capture_stopped(&mut self) {
        self.capture.take();
        self.keepalive.take();
        self.user_speaking = false;
        self.finishing = false;
        self.continuous = false;
        self.status = "";
    }

    fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.cancel_speech();
        self.capture_stopped();
        self.transcription.take();
        self.pending_recordings.clear();
        self.reply_start = None;
    }
}

fn reply_start_index(entries: &[std::sync::Arc<SessionEntry>], state: &SessionState) -> usize {
    if matches!(
        state,
        SessionState::Running { .. } | SessionState::Stopping { .. } | SessionState::AwaitingInput
    ) && entries.last().is_some_and(|entry| {
        entry.subagent_id.is_none()
            && matches!(entry.kind, SessionEntryKind::AssistantMessage { .. })
    }) {
        entries.len().saturating_sub(1)
    } else {
        entries.len()
    }
}

fn reply_text(entries: &[std::sync::Arc<SessionEntry>], start: usize) -> String {
    // Read the last assistant message of a completed turn. Earlier messages
    // are progress commentary; reasoning and tool output are never spoken.
    let chunks = entries.iter().skip(start).rev().find_map(|entry| {
        if entry.subagent_id.is_some() {
            return None;
        }
        match &entry.kind {
            SessionEntryKind::AssistantMessage { chunks } => Some(chunks),
            _ => None,
        }
    });
    let text = chunks
        .into_iter()
        .flatten()
        .filter_map(|chunk| match chunk {
            AssistantChunk::Message(message) => Some(message.as_str()),
            AssistantChunk::Thought(_) => None,
        })
        .collect::<String>();
    speech_text(&text)
}

fn speech_chunks(text: &str) -> VecDeque<String> {
    // Bound queued synthesis independently of the transcript. A coding agent can
    // return a whole generated document; speaking that must not allocate forever.
    let mut result = VecDeque::new();
    let mut chunk = String::new();
    let mut count = 0;
    for character in text.chars().take(32_000) {
        chunk.push(character);
        count += 1;
        if count >= 3_000 {
            count = 0;
            result.push_back(std::mem::take(&mut chunk));
        }
    }
    if !chunk.trim().is_empty() {
        result.push_back(chunk);
    }
    result
}

impl SolutionSessionView {
    fn voice_service(&self, cx: &App) -> anyhow::Result<VoiceService> {
        let settings = SolutionAgentSettings::get_global(cx).voice.clone();
        VoiceService::new(settings, cx.http_client())
    }

    fn voice_is_visible(&self, cx: &App) -> bool {
        let solution_id = self.session.read(cx).solution_id;
        let current_solution = self.workspace.upgrade().and_then(|workspace| {
            let project = workspace.read(cx).project();
            let store = solutions::SolutionStore::try_global(cx)?.read(cx);
            project.read(cx).worktrees(cx).find_map(|tree| {
                store
                    .solution_for_path(&tree.read(cx).abs_path())
                    .map(|solution| solution.id)
            })
        });
        current_solution == Some(solution_id)
            && SolutionAgentStore::try_global(cx).is_some_and(|store| {
                store.read(cx).visible_dialog_session(solution_id, cx) == Some(self.session_id)
            })
    }

    pub(super) fn stop_voice(&mut self, cx: &mut Context<Self>) {
        self.voice.cancel();
        self.voice.read_aloud = false;
        self.voice.poll.take();
        cx.notify();
    }

    pub(super) fn toggle_voice_recording_action(
        &mut self,
        _: &ToggleVoiceRecording,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_voice_recording(false, window, cx);
    }

    pub(super) fn toggle_voice_conversation_action(
        &mut self,
        _: &ToggleVoiceConversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_voice_recording(true, window, cx);
    }

    fn toggle_voice_recording(
        &mut self,
        continuous: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compose_disabled(cx) || self.expanded_window.is_some() {
            return;
        }
        if continuous && self.voice.capture.is_some() && !self.voice.continuous {
            self.voice.cancel();
        }
        if let Some(capture) = self.voice.capture.as_ref() {
            if self.voice.continuous || self.voice.finishing {
                self.stop_voice(cx);
            } else {
                capture.finish();
                self.voice.finishing = true;
                self.voice.status = "Finishing recording…";
                cx.notify();
            }
            return;
        }
        if let Err(error) = self.voice_service(cx) {
            self.show_toast(format!("Voice setup: {error}").into(), cx);
            return;
        }
        self.voice.cancel();
        let device = audio::AudioSettings::get_global(cx)
            .input_audio_device
            .clone();
        let echo = cx.default_global::<audio::Audio>().echo_canceller.clone();
        match VoiceCapture::start(device, continuous, echo) {
            Ok(capture) => {
                self.voice.capture = Some(capture);
                self.voice.continuous = continuous;
                self.voice.read_aloud |= continuous;
                self.voice.status = if continuous {
                    "Listening…"
                } else {
                    "Recording… click mic to send"
                };
                let session = self.session.read(cx);
                self.voice.reply_start = Some(reply_start_index(&session.entries, &session.state));
                self.ensure_voice_keepalive(cx);
                self.ensure_voice_poll(window, cx);
            }
            Err(error) => self.show_toast(format!("Could not open microphone: {error}").into(), cx),
        }
        cx.notify();
    }

    pub(super) fn toggle_read_aloud_action(
        &mut self,
        _: &ToggleReadAloud,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.voice.read_aloud {
            self.voice.read_aloud = false;
            self.voice.cancel_speech();
        } else {
            if let Err(error) = self.voice_service(cx) {
                self.show_toast(format!("Voice setup: {error}").into(), cx);
                return;
            }
            self.voice.read_aloud = true;
            let session = self.session.read(cx);
            self.voice.reply_start = Some(reply_start_index(&session.entries, &session.state));
            self.ensure_voice_poll(window, cx);
        }
        cx.notify();
    }

    pub(super) fn stop_speaking_action(
        &mut self,
        _: &StopSpeaking,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.voice.cancel_speech();
        cx.notify();
    }

    pub(super) fn voice_turn_stopped(
        &mut self,
        reason: &agent_client_protocol::schema::v1::StopReason,
        cx: &mut Context<Self>,
    ) {
        if !self.voice.read_aloud || !self.voice_is_visible(cx) {
            return;
        }
        let session = self.session.read(cx);
        let start = self.voice.reply_start.unwrap_or(session.entries.len());
        if *reason == agent_client_protocol::schema::v1::StopReason::EndTurn {
            let text = reply_text(&session.entries, start);
            let available = 12usize.saturating_sub(self.voice.pending_speech.len());
            self.voice
                .pending_speech
                .extend(speech_chunks(&text).into_iter().take(available));
        }
        self.voice.reply_start = Some(session.entries.len());
        cx.notify();
    }

    fn ensure_voice_poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.voice.poll.is_some() {
            return;
        }
        self.voice.poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
                let keep_running =
                    this.update_in(cx, |this, window, cx| this.poll_voice(window, cx));
                if !matches!(keep_running, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn ensure_voice_keepalive(&mut self, cx: &mut Context<Self>) {
        if !self.voice.continuous || self.voice.keepalive.is_some() {
            return;
        }
        let generation = self.voice.generation;
        self.voice.keepalive = Some(cx.spawn(async move |this, cx| {
            loop {
                // Resolve current settings each time, just as transcription does.
                // Never retain an old target after the user changes speech servers.
                let service = this.update(cx, |this, cx| {
                    if this.voice.generation != generation
                        || !this.voice.continuous
                        || !this.voice_is_visible(cx)
                        || this.compose_disabled(cx)
                        || this.expanded_window.is_some()
                    {
                        return None;
                    }
                    Some(this.voice_service(cx).ok().filter(|service| {
                        service.keepalive_url().is_some_and(|url| {
                            this.voice.unsupported_keepalive_url.as_deref() != Some(url.as_str())
                        })
                    }))
                });
                let Ok(Some(service)) = service else {
                    break;
                };
                if let Some(service) = service
                    && matches!(service.keepalive().await, Ok(false))
                {
                    this.update(cx, |this, _| {
                        if this.voice.generation == generation {
                            this.voice.unsupported_keepalive_url =
                                service.keepalive_url().map(|url| url.as_str().to_owned());
                        }
                    })
                    .ok();
                }
                // One bounded request at a time. Transient failures retry quietly
                // while active so restarting the local service can recover.
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
            }
        }));
    }

    fn poll_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.voice_is_visible(cx) || self.compose_disabled(cx) || self.expanded_window.is_some()
        {
            self.stop_voice(cx);
            return false;
        }
        let mut events = Vec::new();
        if let Some(capture) = &self.voice.capture {
            // Poll work is bounded even if an audio device produces a burst.
            for _ in 0..16 {
                match capture.try_recv() {
                    Ok(event) => events.push(event),
                    Err(_) => break,
                }
            }
        }
        for event in events {
            match event {
                VoiceCaptureEvent::SpeechStarted => {
                    self.voice.cancel_speech();
                    self.voice.user_speaking = true;
                    self.voice.status = "Recording…";
                    if let Some(store) = SolutionAgentStore::try_global(cx) {
                        store.update(cx, |store, cx| store.note_user_input(self.session_id, cx));
                    }
                }
                VoiceCaptureEvent::SpeechEnded => {
                    self.voice.user_speaking = false;
                    self.voice.status = if self.voice.continuous {
                        "Listening…"
                    } else {
                        "Recording… click mic to send"
                    };
                }
                VoiceCaptureEvent::Utterance(wav) => {
                    self.voice.user_speaking = false;
                    if self.voice.pending_recordings.len() < 3 {
                        self.voice.pending_recordings.push_back(wav);
                    } else {
                        self.stop_voice(cx);
                        self.show_toast(
                            "Voice input is arriving faster than recognition. Please record again."
                                .into(),
                            cx,
                        );
                        return false;
                    }
                }
                VoiceCaptureEvent::Error(error) => {
                    self.stop_voice(cx);
                    self.show_toast(format!("Microphone: {error}").into(), cx);
                    return false;
                }
                VoiceCaptureEvent::Stopped => {
                    self.voice.capture_stopped();
                }
            }
            cx.notify();
        }
        if self
            .voice
            .playback
            .as_ref()
            .is_some_and(|playback| playback.is_finished())
        {
            self.voice.playback.take();
            cx.notify();
        }
        self.start_next_transcription(window, cx);
        self.start_next_speech(cx);
        let active = self.voice.capture.is_some()
            || self.voice.transcription.is_some()
            || !self.voice.pending_recordings.is_empty()
            || self.voice.read_aloud
            || self.voice.speaking();
        if !active {
            self.voice.poll.take();
        }
        active
    }

    fn start_next_transcription(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.voice.transcription.is_some() {
            return;
        }
        let Some(wav) = self.voice.pending_recordings.pop_front() else {
            return;
        };
        let service = match self.voice_service(cx) {
            Ok(service) => service,
            Err(error) => {
                self.show_toast(format!("Voice setup: {error}").into(), cx);
                return;
            }
        };
        let generation = self.voice.generation;
        self.voice.status = "Transcribing…";
        self.voice.transcription = Some(cx.spawn_in(window, async move |this, cx| {
            let result = service.transcribe(wav).await;
            this.update_in(cx, |this, window, cx| {
                if this.voice.generation != generation {
                    return;
                }
                this.voice.transcription.take();
                this.voice.status = if this.voice.capture.is_some() {
                    "Listening…"
                } else {
                    ""
                };
                match result {
                    Ok(text) if !text.trim().is_empty() => {
                        this.deliver_voice_text(text, window, cx)
                    }
                    Ok(_) => this.show_toast("No speech recognized. Please try again.".into(), cx),
                    Err(error) => {
                        this.show_toast(format!("Speech recognition failed: {error}").into(), cx)
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn deliver_voice_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.voice_is_visible(cx) || self.compose_disabled(cx) || self.expanded_window.is_some()
        {
            return;
        }
        self.voice.cancel_speech();
        let draft = self.compose_editor.read(cx).text(cx);
        let may_send = draft.trim().is_empty() && self.pending_images.is_empty() && !self.resuming;
        let combined = if draft.is_empty() {
            text
        } else {
            format!("{draft}\n{text}")
        };
        self.compose_editor
            .update(cx, |editor, cx| editor.set_text(combined, window, cx));
        if may_send {
            self.submit_compose_now(window, cx);
        } else {
            self.show_toast("Recognized speech added to your draft.".into(), cx);
        }
    }

    fn start_next_speech(&mut self, cx: &mut Context<Self>) {
        if self.voice.user_speaking
            || self.voice.synthesis.is_some()
            || self.voice.playback.is_some()
            || self.voice.transcription.is_some()
            || !self.voice.pending_recordings.is_empty()
            || (self.voice.capture.is_some() && !self.voice.continuous)
        {
            return;
        }
        let Some(text) = self.voice.pending_speech.pop_front() else {
            return;
        };
        let service = match self.voice_service(cx) {
            Ok(service) => service,
            Err(error) => {
                self.voice.cancel_speech();
                self.show_toast(format!("Voice setup: {error}").into(), cx);
                return;
            }
        };
        let generation = self.voice.speech_generation;
        self.voice.synthesis = Some(cx.spawn(async move |this, cx| {
            let result = service.synthesize(text).await;
            this.update(cx, |this, cx| {
                if this.voice.speech_generation != generation {
                    return;
                }
                if !this.voice_is_visible(cx)
                    || this.compose_disabled(cx)
                    || this.expanded_window.is_some()
                {
                    this.stop_voice(cx);
                    return;
                }
                this.voice.synthesis.take();
                match result {
                    Ok(wav) => {
                        let device = audio::AudioSettings::get_global(cx)
                            .output_audio_device
                            .clone();
                        let playback = cx
                            .default_global::<audio::Audio>()
                            .speech_output_mixer(device)
                            .and_then(|mixer| CancellablePlayback::start_on_mixer(wav, mixer));
                        match playback {
                            Ok(playback) => this.voice.playback = Some(playback),
                            Err(error) => {
                                this.voice.cancel_speech();
                                this.show_toast(
                                    format!("Could not play speech: {error}").into(),
                                    cx,
                                );
                            }
                        }
                    }
                    Err(error) => {
                        this.voice.cancel_speech();
                        this.show_toast(format!("Speech generation failed: {error}").into(), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn render_voice_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let recording = self.voice.capture.is_some();
        h_flex().debug_selector(|| "voice-controls".into()).w_full().min_w_0().gap_1().flex_none()
            .child(IconButton::new("voice-record", if recording { IconName::MicMute } else { IconName::Mic })
                .icon_size(IconSize::Small).toggle_state(recording && !self.voice.continuous)
                .tooltip(Tooltip::text(if self.voice.continuous { "Stop listening" } else if recording { "Finish voice recording" } else { "Record a voice message" }))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_voice_recording(false, window, cx))))
            .child(ui::Button::new("voice-conversation", "Hands-free")
                .style(ui::ButtonStyle::Subtle).size(ui::ButtonSize::Compact)
                .toggle_state(self.voice.continuous)
                .tooltip(Tooltip::text("Listen and send after pauses. Speaking interrupts AI-generated playback, not the agent."))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_voice_recording(true, window, cx))))
            .child(IconButton::new("voice-read-aloud", if self.voice.read_aloud { IconName::AudioOn } else { IconName::AudioOff })
                .icon_size(IconSize::Small).toggle_state(self.voice.read_aloud)
                .tooltip(Tooltip::text("Read new replies aloud with an AI-generated voice"))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_read_aloud_action(&ToggleReadAloud, window, cx))))
            .when(self.voice.speaking(), |row| row.child(
                IconButton::new("voice-stop-speaking", IconName::Stop).icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Stop speaking; keep the agent running"))
                    .on_click(cx.listener(|this, _, window, cx| this.stop_speaking_action(&StopSpeaking, window, cx)))))
            .child(div().flex_1().min_w_0().overflow_hidden().child(ui::Label::new(self.voice.status).truncate().size(ui::LabelSize::Small).color(Color::Muted)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_entry::AssistantChunk;

    #[test]
    fn speech_chunks_preserve_unicode_and_fit_service_limit() {
        let source = "Привет, Sawe! ".repeat(800);
        let chunks = speech_chunks(&source);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.iter().cloned().collect::<String>(), source);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 3000));
    }

    #[test]
    fn reply_speech_skips_history_reasoning_tools_and_subagents() {
        let message = |chunks, subagent_id| {
            std::sync::Arc::new(SessionEntry {
                created_ms: 0,
                mod_seq: 0,
                subagent_id,
                kind: SessionEntryKind::AssistantMessage { chunks },
            })
        };
        let entries = vec![
            message(vec![AssistantChunk::Message("Old history".into())], None),
            message(
                vec![
                    AssistantChunk::Thought("Private reasoning".into()),
                    AssistantChunk::Message("New answer.".into()),
                ],
                None,
            ),
            message(
                vec![AssistantChunk::Message("Child output".into())],
                Some("child".into()),
            ),
        ];
        assert_eq!(reply_text(&entries, 1).trim(), "New answer.");
    }

    #[test]
    fn stopping_speech_invalidates_inflight_synthesis_and_clears_queue() {
        let mut voice = SessionVoice::default();
        voice.pending_speech.push_back("reply".into());
        let generation = voice.speech_generation;
        voice.cancel_speech();
        assert_ne!(voice.speech_generation, generation);
        assert!(!voice.speaking());
        assert!(voice.pending_speech.is_empty());
    }

    #[gpui::test]
    fn stopping_capture_or_voice_cancels_inflight_keepalive(cx: &mut gpui::TestAppContext) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct RequestGuard(Arc<AtomicBool>);
        impl Drop for RequestGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        for stop_all in [false, true] {
            let dropped = Arc::new(AtomicBool::new(false));
            let guard = RequestGuard(dropped.clone());
            let request = cx.executor().spawn(async move {
                let _guard = guard;
                futures::future::pending::<()>().await;
            });
            let mut voice = SessionVoice {
                continuous: true,
                keepalive: Some(request),
                ..Default::default()
            };
            cx.executor().run_until_parked();
            assert!(!dropped.load(Ordering::SeqCst));
            if stop_all {
                voice.cancel();
            } else {
                voice.capture_stopped();
            }
            cx.executor().run_until_parked();
            assert!(
                dropped.load(Ordering::SeqCst),
                "an in-flight request must be dropped"
            );
            assert!(voice.keepalive.is_none());
            assert!(!voice.continuous);
        }
    }
    async fn voice_fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::WindowHandle<SolutionSessionView>,
        gpui::WindowHandle<workspace::Workspace>,
        gpui::Entity<crate::model::SolutionSession>,
        tempfile::TempDir,
    ) {
        let (id, _thread, tmp) = crate::store::tests::create_session_with_thread(cx).await;
        cx.update(|cx| theme_settings::init(theme::LoadThemes::JustBase, cx));
        let session = cx.update(|cx| SolutionAgentStore::global(cx).read(cx).session(id).unwrap());
        let project = cx.update(|cx| session.read(cx).project.clone().unwrap());
        let workspace =
            cx.add_window(|window, cx| workspace::Workspace::test_new(project, window, cx));
        let weak = cx.update(|cx| workspace.root(cx).unwrap().downgrade());
        cx.update(|cx| {
            let solution = session.read(cx).solution_id;
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_active_dialog_session(solution, Some(id), cx)
            });
        });
        let view = cx.add_window(|window, cx| {
            SolutionSessionView::for_test(id, session.clone(), weak, window, cx)
        });
        (view, workspace, session, tmp)
    }

    fn assistant(text: &str) -> std::sync::Arc<SessionEntry> {
        std::sync::Arc::new(SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: None,
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text.into())],
            },
        })
    }

    #[gpui::test]
    async fn voice_preserves_typed_drafts_and_uses_the_running_message_queue(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, _workspace, session, _tmp) = voice_fixture(cx).await;
        view.update(cx, |view, window, cx| {
            view.compose_editor
                .update(cx, |editor, cx| editor.set_text("Typed draft", window, cx));
            view.deliver_voice_text("Spoken addition".into(), window, cx);
            assert_eq!(
                view.compose_editor.read(cx).text(cx),
                "Typed draft\nSpoken addition"
            );
            assert!(session.read(cx).pending_messages.is_empty());
            view.compose_editor
                .update(cx, |editor, cx| editor.clear(window, cx));
            session.update(cx, |session, _| {
                session.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                }
            });
            view.deliver_voice_text("Please also run tests".into(), window, cx);
            assert!(view.compose_editor.read(cx).text(cx).is_empty());
            assert!(!session.read(cx).pending_messages.is_empty());
            assert!(
                matches!(session.read(cx).state, SessionState::Running { .. }),
                "voice must not stop the agent"
            );
        })
        .unwrap();
    }

    #[gpui::test]
    async fn voice_speaks_each_completed_turn_without_observable_idle(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, _workspace, session, _tmp) = voice_fixture(cx).await;
        view.update(cx, |view, window, cx| {
            view.voice.read_aloud = true;
            view.voice.reply_start = Some(session.read(cx).entries.len());
            session.update(cx, |session, _| {
                session.entries.push(assistant("First answer."));
                session.state = SessionState::Running {
                    started_at: std::time::Instant::now(),
                    notified: false,
                };
            });
            view.voice_turn_stopped(&agent_client_protocol::schema::v1::StopReason::EndTurn, cx);
            session.update(cx, |session, _| {
                session.entries.push(assistant("Second answer."))
            });
            view.voice_turn_stopped(&agent_client_protocol::schema::v1::StopReason::EndTurn, cx);
            assert_eq!(
                view.voice
                    .pending_speech
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["First answer.", "Second answer."]
            );
            view.voice_turn_stopped(&agent_client_protocol::schema::v1::StopReason::EndTurn, cx);
            assert_eq!(
                view.voice.pending_speech.len(),
                2,
                "duplicate terminal event cannot replay history"
            );
            view.stop_speaking_action(&StopSpeaking, window, cx);
            assert!(view.voice.pending_speech.is_empty());
            assert!(matches!(
                session.read(cx).state,
                SessionState::Running { .. }
            ));
        })
        .unwrap();
    }

    #[gpui::test]
    async fn voice_drops_late_transcripts_when_dialog_is_hidden(cx: &mut gpui::TestAppContext) {
        let (view, _workspace, session, _tmp) = voice_fixture(cx).await;
        cx.update(|cx| {
            let solution = session.read(cx).solution_id;
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_active_dialog_session(solution, None, cx)
            });
        });
        view.update(cx, |view, window, cx| {
            view.deliver_voice_text("Must not be sent".into(), window, cx);
            assert!(view.compose_editor.read(cx).text(cx).is_empty());
            assert!(session.read(cx).pending_messages.is_empty());
        })
        .unwrap();
    }

    #[gpui::test]
    async fn hands_free_keepalive_task_stops_when_dialog_is_hidden(cx: &mut gpui::TestAppContext) {
        let (view, _workspace, session, _tmp) = voice_fixture(cx).await;
        // A remote target opts out without contacting any real voice service.
        cx.update(|cx| {
            let mut settings = SolutionAgentSettings::get_global(cx).clone();
            settings.voice.api_url = "https://speech.example/v1".into();
            SolutionAgentSettings::override_global(settings, cx);
        });
        view.update(cx, |view, _, cx| {
            view.ensure_voice_keepalive(cx);
            assert!(
                view.voice.keepalive.is_none(),
                "read-aloud or recording alone must not retain the worker"
            );
            view.voice.continuous = true;
            view.ensure_voice_keepalive(cx);
            assert!(view.voice.keepalive.is_some());
        })
        .unwrap();
        cx.executor().run_until_parked();
        cx.update(|cx| {
            let solution = session.read(cx).solution_id;
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_active_dialog_session(solution, None, cx)
            });
        });
        view.update(cx, |view, window, cx| {
            assert!(!view.poll_voice(window, cx));
            assert!(view.voice.keepalive.is_none());
            assert!(!view.voice.continuous);
        })
        .unwrap();
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.executor().run_until_parked();
        view.update(cx, |view, _, _| assert!(view.voice.keepalive.is_none()))
            .unwrap();
    }

    #[gpui::test]
    async fn voice_controls_paint_and_stop_only_appears_for_speech(cx: &mut gpui::TestAppContext) {
        let (view, _workspace, _session, _tmp) = voice_fixture(cx).await;
        let vcx = &mut gpui::VisualTestContext::from_window(view.into(), cx);
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("voice-controls").is_some());
        assert!(vcx.debug_bounds("ICON-Mic").is_some());
        assert!(vcx.debug_bounds("ICON-Stop").is_none());
        view.update(vcx, |view, _, cx| {
            view.voice.pending_speech.push_back("Queued speech".into());
            cx.notify();
        })
        .unwrap();
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("ICON-Stop").is_some());
        view.update(vcx, |view, window, cx| {
            view.stop_speaking_action(&StopSpeaking, window, cx)
        })
        .unwrap();
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("ICON-Stop").is_none());
    }
    #[gpui::test]
    async fn voice_reads_final_mirrored_text_from_actual_thread_completion(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, _workspace, session, _tmp) = voice_fixture(cx).await;
        view.update(cx, |view, _, cx| {
            view.voice.read_aloud = true;
            view.voice.reply_start = Some(session.read(cx).entries.len());
        })
        .unwrap();
        let thread = cx.update(|cx| session.read(cx).acp_thread().unwrap().clone());
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread
                    .handle_session_update(
                        agent_client_protocol::schema::v1::SessionUpdate::AgentMessageChunk(
                            agent_client_protocol::schema::v1::ContentChunk::new(
                                "Actual final reply.".into(),
                            ),
                        ),
                        cx,
                    )
                    .unwrap();
                thread.flush_end_of_turn_tail(cx);
                cx.emit(acp_thread::AcpThreadEvent::Stopped(
                    agent_client_protocol::schema::v1::StopReason::EndTurn,
                ));
            })
        });
        cx.executor().run_until_parked();
        view.update(cx, |view, _, _| {
            assert_eq!(
                view.voice
                    .pending_speech
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["Actual final reply."]
            );
        })
        .unwrap();
    }

    #[gpui::test]
    async fn voice_cannot_target_the_old_solution_after_its_worktree_is_removed(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, _workspace, session, _tmp) = voice_fixture(cx).await;
        view.update(cx, |view, _, cx| assert!(view.voice_is_visible(cx)))
            .unwrap();
        cx.update(|cx| {
            let project = session.read(cx).project.clone().unwrap();
            let tree = project.read(cx).worktrees(cx).next().unwrap().read(cx).id();
            project.update(cx, |project, cx| project.remove_worktree(tree, cx));
        });
        view.update(cx, |view, window, cx| {
            assert!(!view.voice_is_visible(cx));
            view.deliver_voice_text("Old solution must not receive this".into(), window, cx);
            assert!(view.compose_editor.read(cx).text(cx).is_empty());
        })
        .unwrap();
    }
}
