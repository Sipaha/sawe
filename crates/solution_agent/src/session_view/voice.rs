use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use audio::voice::{CancellablePlayback, VoiceCapture, VoiceCaptureEvent};
use gpui::{App, Context, IntoElement, Task, WeakEntity, Window, div};
use settings::Settings as _;
use ui::{IconButton, IconName, Tooltip, prelude::*};

use super::SolutionSessionView;
use crate::{
    actions::{StopSpeaking, ToggleReadAloud, ToggleVoiceConversation, ToggleVoiceRecording},
    agent_settings::SolutionAgentSettings,
    model::{SessionState, SolutionSessionId},
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
    reply_spoken: String,
    reply_observed: String,
    reply_changed_at: Option<Instant>,
    reply_finished: usize,
    awaiting_user: bool,
    status: &'static str,
}

#[derive(Default)]
struct HandsFreeOwner(Option<(SolutionSessionId, WeakEntity<SolutionSessionView>)>);

impl gpui::Global for HandsFreeOwner {}

impl SessionVoice {
    fn speaking(&self) -> bool {
        self.synthesis.is_some() || self.playback.is_some() || !self.pending_speech.is_empty()
    }

    fn cancel_speech(&mut self) {
        self.speech_generation = self.speech_generation.wrapping_add(1);
        self.synthesis.take();
        self.pending_speech.clear();
        self.playback.take();
        self.awaiting_user = true;
    }

    fn reset_reply_cursor(&mut self, start: usize) {
        self.reply_start = Some(start);
        self.reply_spoken.clear();
        self.reply_observed.clear();
        self.reply_changed_at = None;
        self.reply_finished = start;
        self.awaiting_user = false;
    }

    fn advance_reply_cursor(&mut self, start: usize) {
        self.reply_start = Some(start);
        self.reply_spoken.clear();
        self.reply_observed.clear();
        self.reply_changed_at = None;
    }

    fn queue_replies(&mut self, entries: &[std::sync::Arc<SessionEntry>], now: Instant) {
        let Some(start) = self.reply_start else {
            return;
        };
        if start > entries.len() {
            self.reset_reply_cursor(entries.len());
            return;
        }
        // The transcript owns history; only the unspoken current entry is scanned.
        for index in start..entries.len().min(start.saturating_add(64)) {
            let entry = &entries[index];
            if entry.subagent_id.is_some() {
                self.advance_reply_cursor(index + 1);
                continue;
            }
            if matches!(entry.kind, SessionEntryKind::UserMessage { .. }) {
                self.awaiting_user = false;
            }
            let SessionEntryKind::AssistantMessage { chunks } = &entry.kind else {
                self.advance_reply_cursor(index + 1);
                continue;
            };
            if self.awaiting_user {
                self.advance_reply_cursor(index + 1);
                continue;
            }
            let complete = index < self.reply_finished
                || entries[index + 1..]
                    .iter()
                    .any(|entry| entry.subagent_id.is_none());
            let markdown = chunks
                .iter()
                .filter_map(|chunk| match chunk {
                    AssistantChunk::Message(message) => Some(message.as_str()),
                    AssistantChunk::Thought(_) => None,
                })
                .flat_map(str::chars)
                .take(32_000)
                .collect::<String>();
            let text = streaming_speech_text(&markdown, complete);
            if !text.starts_with(&self.reply_spoken) {
                // A rewritten entry must not replay prose that was already heard.
                self.advance_reply_cursor(index + 1);
                continue;
            }
            if text != self.reply_observed {
                self.reply_observed = text.clone();
                self.reply_changed_at = Some(now);
            }
            let settled = self.reply_changed_at.is_some_and(|changed| {
                now.saturating_duration_since(changed) >= Duration::from_millis(180)
            });
            while self.pending_speech.len() < 12 {
                let remaining = &text[self.reply_spoken.len()..];
                let Some(end) = speech_chunk_end(remaining, complete, settled) else {
                    break;
                };
                let chunk = &remaining[..end];
                if !chunk.trim().is_empty() {
                    self.pending_speech.push_back(chunk.trim().to_owned());
                }
                self.reply_spoken.push_str(chunk);
            }
            if complete && self.reply_spoken.len() == text.len() {
                self.advance_reply_cursor(index + 1);
            } else {
                break;
            }
        }
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

fn streaming_speech_text(markdown: &str, complete: bool) -> String {
    if complete {
        return speech_text(markdown);
    }
    // Unfinished Markdown may still become an image, link or inline code.
    // Wait at its opening delimiter so a later token cannot change spoken prose.
    let mut brackets = Vec::new();
    let mut parentheses = Vec::new();
    let mut closed_bracket = None;
    let mut backtick = None;
    let mut escaped = false;
    let mut previous = None;
    for (index, character) in markdown.char_indices() {
        if escaped {
            escaped = false;
            previous = Some(character);
            continue;
        }
        match character {
            '\\' => escaped = true,
            '[' => brackets.push(if previous == Some('!') {
                index - 1
            } else {
                index
            }),
            ']' => closed_bracket = brackets.pop(),
            '(' => parentheses.push(if previous == Some(']') {
                closed_bracket.unwrap_or(index)
            } else {
                index
            }),
            ')' => {
                parentheses.pop();
            }
            '`' => {
                backtick = if backtick.is_some() {
                    None
                } else {
                    Some(index)
                }
            }
            _ => {}
        }
        previous = Some(character);
    }
    let end = brackets
        .into_iter()
        .chain(parentheses)
        .chain(backtick)
        .min()
        .unwrap_or(markdown.len());
    speech_text(&markdown[..end])
}

fn speech_chunk_end(text: &str, complete: bool, settled: bool) -> Option<usize> {
    let mut last_space = None;
    let mut characters = text.char_indices().peekable();
    let mut count = 0;
    while let Some((index, character)) = characters.next() {
        count += 1;
        let end = index + character.len_utf8();
        if character.is_whitespace() && index > 0 {
            last_space = Some(end);
        }
        if matches!(character, '.' | '!' | '?' | '…')
            && (characters
                .peek()
                .is_some_and(|(_, next)| next.is_whitespace())
                || (characters.peek().is_none() && (complete || settled)))
        {
            return Some(end);
        }
        if count >= 240 {
            return Some(last_space.unwrap_or(end));
        }
    }
    (complete && !text.is_empty()).then_some(text.len())
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

    fn voice_can_run(&self, cx: &App) -> bool {
        if !self.voice.continuous {
            return self.voice_is_visible(cx);
        }
        cx.try_global::<HandsFreeOwner>()
            .and_then(|owner| owner.0.as_ref())
            .is_some_and(|(id, _)| *id == self.session_id)
            && self.workspace.upgrade().is_some()
            && SolutionAgentStore::try_global(cx)
                .is_some_and(|store| store.read(cx).session(self.session_id).is_some())
            && solutions::SolutionStore::try_global(cx).is_some_and(|store| {
                store
                    .read(cx)
                    .solutions()
                    .iter()
                    .any(|solution| solution.id == self.session.read(cx).solution_id)
            })
    }

    fn claim_hands_free(&mut self, cx: &mut Context<Self>) {
        let previous = cx
            .try_global::<HandsFreeOwner>()
            .and_then(|owner| owner.0.as_ref())
            .map(|(_, view)| view.clone());
        if let Some(previous) = previous.and_then(|view| view.upgrade())
            && previous.entity_id() != cx.entity_id()
        {
            previous.update(cx, |view, cx| view.stop_voice(cx));
        }
        cx.set_global(HandsFreeOwner(Some((
            self.session_id,
            cx.entity().downgrade(),
        ))));
    }

    fn release_hands_free(&self, cx: &mut Context<Self>) {
        if cx
            .try_global::<HandsFreeOwner>()
            .and_then(|owner| owner.0.as_ref())
            .is_some_and(|(_, view)| view.entity_id() == cx.entity_id())
        {
            cx.set_global(HandsFreeOwner::default());
        }
    }

    fn interrupt_speech(&mut self, cx: &App) {
        self.voice.cancel_speech();
        self.voice
            .reset_reply_cursor(self.session.read(cx).entries.len());
        self.voice.awaiting_user = true;
    }

    fn queue_voice_replies(&mut self, cx: &App) {
        if self.voice.read_aloud && self.voice_can_run(cx) {
            self.voice.queue_replies(
                &self.session.read(cx).entries,
                cx.background_executor().now(),
            );
        }
    }

    pub(super) fn stop_voice(&mut self, cx: &mut Context<Self>) {
        self.release_hands_free(cx);
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
        if continuous {
            self.claim_hands_free(cx);
        }
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
                self.voice
                    .reset_reply_cursor(reply_start_index(&session.entries, &session.state));
                self.ensure_voice_keepalive(cx);
                self.ensure_voice_poll(window, cx);
            }
            Err(error) => {
                self.release_hands_free(cx);
                self.show_toast(format!("Could not open microphone: {error}").into(), cx);
            }
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
            self.voice
                .reset_reply_cursor(reply_start_index(&session.entries, &session.state));
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
        self.interrupt_speech(cx);
        cx.notify();
    }

    pub(super) fn voice_turn_stopped(
        &mut self,
        reason: &agent_client_protocol::schema::v1::StopReason,
        cx: &mut Context<Self>,
    ) {
        if !self.voice.read_aloud || !self.voice_can_run(cx) {
            return;
        }
        let session = self.session.read(cx);
        if *reason == agent_client_protocol::schema::v1::StopReason::EndTurn {
            self.voice.reply_finished = session.entries.len();
            self.voice
                .queue_replies(&session.entries, cx.background_executor().now());
        } else {
            self.voice.advance_reply_cursor(session.entries.len());
        }
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
                        || !this.voice_can_run(cx)
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
        if !self.voice_can_run(cx) || self.compose_disabled(cx) || self.expanded_window.is_some() {
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
                    self.interrupt_speech(cx);
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
        self.queue_voice_replies(cx);
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
        if !self.voice_can_run(cx) || self.compose_disabled(cx) || self.expanded_window.is_some() {
            return;
        }
        self.interrupt_speech(cx);
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
                if !this.voice_can_run(cx)
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
        let mut chunks = Vec::new();
        let mut remaining = source.as_str();
        while let Some(end) = speech_chunk_end(remaining, true, true) {
            chunks.push(remaining[..end].to_owned());
            remaining = &remaining[end..];
        }
        assert!(chunks.len() > 1);
        assert_eq!(chunks.iter().cloned().collect::<String>(), source);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 240));
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
        let mut voice = SessionVoice::default();
        voice.reset_reply_cursor(1);
        voice.reply_finished = entries.len();
        voice.queue_replies(&entries, Instant::now());
        assert_eq!(voice.pending_speech, ["New answer."]);
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

    fn user(text: &str) -> std::sync::Arc<SessionEntry> {
        std::sync::Arc::new(SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: None,
            kind: SessionEntryKind::UserMessage {
                id: None,
                content_md: text.into(),
                chunks: Vec::new(),
            },
        })
    }

    fn tool() -> std::sync::Arc<SessionEntry> {
        std::sync::Arc::new(SessionEntry {
            created_ms: 0,
            mod_seq: 0,
            subagent_id: None,
            kind: SessionEntryKind::ToolCall {
                id: "long-command".into(),
                label_md: "sleep 30".into(),
                kind: agent_client_protocol::schema::v1::ToolKind::Execute,
                status: crate::session_entry::ToolStatus::InProgress,
                content_md: vec!["Tool output must stay silent".into()],
                raw_input: None,
                raw_output: None,
                tool_name: None,
                locations: Vec::new(),
                status_started_at: None,
            },
        })
    }

    #[test]
    fn streaming_speech_reads_sentences_and_tool_preludes_before_end_turn() {
        let mut voice = SessionVoice::default();
        voice.reset_reply_cursor(0);
        let now = Instant::now();
        let mut entries = vec![assistant("Проверю, установлен ли k9s")];
        voice.queue_replies(&entries, now);
        assert!(voice.pending_speech.is_empty());
        entries.push(tool());
        voice.queue_replies(&entries, now);
        assert_eq!(voice.pending_speech, ["Проверю, установлен ли k9s"]);
        entries.push(assistant("Первая фраза. Следующая ещё поступает"));
        voice.queue_replies(&entries, now);
        assert_eq!(voice.pending_speech.back().unwrap(), "Первая фраза.");
        voice.queue_replies(&entries, now + Duration::from_secs(2));
        assert_eq!(
            voice.pending_speech.len(),
            2,
            "unfinished words are not flushed on idle"
        );
        voice.reply_finished = entries.len();
        voice.queue_replies(&entries, now + Duration::from_secs(2));
        assert_eq!(
            voice.pending_speech.back().unwrap(),
            "Следующая ещё поступает"
        );
        voice.queue_replies(&entries, now + Duration::from_secs(3));
        assert_eq!(
            voice.pending_speech.len(),
            3,
            "terminal notifications cannot repeat speech"
        );
    }

    #[test]
    fn sentence_tail_settles_without_waiting_for_agent_turn_completion() {
        let mut voice = SessionVoice::default();
        voice.reset_reply_cursor(0);
        let now = Instant::now();
        let entries = vec![assistant("Готово.")];
        voice.queue_replies(&entries, now);
        assert!(voice.pending_speech.is_empty());
        voice.queue_replies(&entries, now + Duration::from_millis(180));
        assert_eq!(voice.pending_speech, ["Готово."]);
        assert_eq!(voice.reply_finished, 0, "no EndTurn is needed");
    }

    #[test]
    fn streaming_markdown_never_reads_fenced_code_or_repeats_prose() {
        let mut voice = SessionVoice::default();
        voice.reset_reply_cursor(0);
        let now = Instant::now();
        let mut entries = vec![assistant("Ready.\n\n```rust\nsecret_code();")];
        voice.queue_replies(&entries, now);
        voice.queue_replies(&entries, now + Duration::from_millis(200));
        assert_eq!(voice.pending_speech, ["Ready."]);
        entries[0] = assistant("Ready.\n\n```rust\nsecret_code();\n```\n\nNext. Done");
        voice.queue_replies(&entries, now + Duration::from_millis(220));
        assert_eq!(voice.pending_speech, ["Ready.", "Next."]);
        voice.reply_finished = 1;
        voice.queue_replies(&entries, now + Duration::from_millis(230));
        assert_eq!(voice.pending_speech, ["Ready.", "Next.", "Done"]);
    }

    #[test]
    fn incomplete_images_and_links_cannot_leak_or_rewrite_spoken_text() {
        let now = Instant::now();
        let mut voice = SessionVoice::default();
        voice.reset_reply_cursor(0);
        let mut entries = vec![assistant("First. ![Image description. Do not speak")];
        voice.queue_replies(&entries, now);
        voice.queue_replies(&entries, now + Duration::from_millis(200));
        assert_eq!(voice.pending_speech, ["First."]);
        entries[0] = assistant(
            "First. ![Image description. Do not speak](image.png) [Read more.](https://example.",
        );
        voice.queue_replies(&entries, now + Duration::from_millis(220));
        assert_eq!(voice.pending_speech, ["First."]);
        entries[0] = assistant(
            "First. ![Image description. Do not speak](image.png) [Read more.](https://example.com) Tail",
        );
        voice.queue_replies(&entries, now + Duration::from_millis(240));
        assert_eq!(voice.pending_speech, ["First.", "Read more."]);
        voice.reply_finished = 1;
        voice.queue_replies(&entries, now + Duration::from_millis(260));
        assert_eq!(voice.pending_speech, ["First.", "Read more.", "Tail"]);
    }

    #[test]
    fn speech_queue_backpressure_preserves_unspoken_tail() {
        let text = (0..80)
            .map(|i| format!("Sentence {i}. "))
            .collect::<String>();
        let entries = vec![assistant(&text)];
        let mut voice = SessionVoice::default();
        voice.reset_reply_cursor(0);
        voice.reply_finished = entries.len();
        let mut spoken = Vec::new();
        for _ in 0..10 {
            voice.queue_replies(&entries, Instant::now());
            assert!(voice.pending_speech.len() <= 12);
            spoken.extend(voice.pending_speech.drain(..));
        }
        assert_eq!(spoken.join(" "), text.trim());
        assert_eq!(voice.reply_start, Some(1));
    }

    #[test]
    fn interruption_suppresses_old_stream_until_new_user_input() {
        let now = Instant::now();
        let mut voice = SessionVoice::default();
        voice.reset_reply_cursor(0);
        let mut entries = vec![assistant("Old sentence. Unfinished")];
        voice.queue_replies(&entries, now);
        assert_eq!(voice.pending_speech.len(), 1);
        voice.cancel_speech();
        entries[0] = assistant("Old sentence. Unfinished old answer now complete.");
        entries.push(tool());
        entries.push(assistant("Old continuation must also stay silent."));
        voice.reply_finished = entries.len();
        voice.queue_replies(&entries, now);
        assert!(voice.pending_speech.is_empty());
        entries.push(user("New question"));
        entries.push(assistant("New answer."));
        voice.reply_finished = entries.len();
        voice.queue_replies(&entries, now);
        assert_eq!(voice.pending_speech, ["New answer."]);
    }

    #[gpui::test]
    async fn hands_free_stays_with_original_chat_until_explicit_transfer(
        cx: &mut gpui::TestAppContext,
    ) {
        let (first, workspace, original, _tmp) = voice_fixture(cx).await;
        let (solution, agent, project) = cx.update(|cx| {
            let session = original.read(cx);
            (
                session.solution_id,
                session.agent_id.clone(),
                session.project.clone().unwrap(),
            )
        });
        let second_id = cx
            .update(|cx| {
                SolutionAgentStore::global(cx).update(cx, |store, cx| {
                    store.create_session(solution, agent, project, cx)
                })
            })
            .await
            .unwrap();
        let second_session = cx.update(|cx| {
            SolutionAgentStore::global(cx)
                .read(cx)
                .session(second_id)
                .unwrap()
        });
        let weak = cx.update(|cx| workspace.root(cx).unwrap().downgrade());
        let second = cx.add_window(|window, cx| {
            SolutionSessionView::for_test(second_id, second_session, weak, window, cx)
        });
        first
            .update(cx, |view, window, cx| {
                view.claim_hands_free(cx);
                view.voice.continuous = true;
                view.voice.read_aloud = true;
                view.compose_editor.update(cx, |editor, cx| {
                    editor.set_text("Original draft", window, cx)
                });
            })
            .unwrap();
        cx.update(|cx| {
            SolutionAgentStore::global(cx).update(cx, |store, cx| {
                store.set_active_dialog_session(solution, Some(second_id), cx)
            })
        });
        first
            .update(cx, |view, window, cx| {
                assert!(!view.voice_is_visible(cx));
                assert!(view.voice_can_run(cx));
                assert!(view.poll_voice(window, cx));
                assert!(view.voice.continuous);
                view.deliver_voice_text("Spoken original input".into(), window, cx);
                assert_eq!(
                    view.compose_editor.read(cx).text(cx),
                    "Original draft\nSpoken original input"
                );
                view.voice
                    .pending_speech
                    .push_back("Old queued audio".into());
            })
            .unwrap();
        second
            .update(cx, |view, _, cx| {
                assert!(view.compose_editor.read(cx).text(cx).is_empty());
                view.claim_hands_free(cx);
                view.voice.continuous = true;
                assert!(view.voice_can_run(cx));
            })
            .unwrap();
        first
            .update(cx, |view, _, cx| {
                assert!(!view.voice.continuous);
                assert!(!view.voice_can_run(cx));
                assert!(view.voice.pending_speech.is_empty());
            })
            .unwrap();
        second
            .update(cx, |view, _, cx| {
                view.stop_voice(cx);
                assert!(cx.global::<HandsFreeOwner>().0.is_none());
            })
            .unwrap();
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
    async fn hands_free_keepalive_ownership_survives_hidden_dialog(cx: &mut gpui::TestAppContext) {
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
            view.claim_hands_free(cx);
            view.voice.continuous = true;
            view.voice.read_aloud = true;
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
            assert!(view.poll_voice(window, cx));
            assert!(view.voice.continuous);
            assert!(view.voice_can_run(cx));
            view.stop_voice(cx);
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
    async fn actual_thread_tool_start_flushes_commentary_without_end_turn(
        cx: &mut gpui::TestAppContext,
    ) {
        use agent_client_protocol::schema::v1 as acp;
        let (view, _workspace, session, _tmp) = voice_fixture(cx).await;
        view.update(cx, |view, _, cx| {
            view.voice.read_aloud = true;
            view.voice
                .reset_reply_cursor(session.read(cx).entries.len());
        })
        .unwrap();
        let thread = cx.update(|cx| session.read(cx).acp_thread().unwrap().clone());
        cx.update(|cx| {
            thread.update(cx, |thread, cx| {
                thread
                    .handle_session_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            "Проверю, установлен ли k9s".into(),
                        )),
                        cx,
                    )
                    .unwrap();
                thread.flush_end_of_turn_tail(cx);
                thread
                    .handle_session_update(
                        acp::SessionUpdate::ToolCall(
                            acp::ToolCall::new(
                                acp::ToolCallId::new("pending-command"),
                                "Long command".to_string(),
                            )
                            .kind(acp::ToolKind::Execute)
                            .status(acp::ToolCallStatus::InProgress),
                        ),
                        cx,
                    )
                    .unwrap();
            })
        });
        cx.executor().run_until_parked();
        view.update(cx, |view, _, cx| {
            view.queue_voice_replies(cx);
            assert_eq!(view.voice.pending_speech, ["Проверю, установлен ли k9s"]);
            assert!(session.read(cx).entries.iter().any(|entry| matches!(
                entry.kind,
                SessionEntryKind::ToolCall {
                    status: crate::session_entry::ToolStatus::InProgress,
                    ..
                }
            )));
            view.voice_turn_stopped(&acp::StopReason::EndTurn, cx);
            assert_eq!(view.voice.pending_speech.len(), 1);
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
