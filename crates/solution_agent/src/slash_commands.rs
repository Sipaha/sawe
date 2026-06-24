//! Slash-command completion provider for the session compose editor. Surfaces `/`-commands the agent advertises via ACP.

use agent_client_protocol::schema as acp;
use anyhow::Result as AnyhowResult;
use editor::{CompletionContext, CompletionProvider as EditorCompletionProvider};
use gpui::{App, Context, Entity, Task, WeakEntity, Window};
use language::{Anchor, Buffer, CodeLabel, Point, ToPoint};
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource,
    lsp_store::CompletionDocumentation,
};

use crate::model::SolutionSession;

/// Editor `CompletionProvider` that surfaces ACP slash commands while the
/// user types `/<query>` at the very start of the compose buffer. Only
/// triggers on row 0 / column 0 — slash commands are sent as the entire
/// first line of a prompt, not embedded mid-message.
///
/// Reads the live `available_commands` list off the session's `AcpThread`
/// on each invocation so a freshly-arrived `AvailableCommandsUpdate` is
/// reflected immediately without a separate subscription (the popup is
/// only built on demand when the user types `/`).
pub(crate) struct SlashCommandsProvider {
    pub(crate) session: WeakEntity<SolutionSession>,
}

impl SlashCommandsProvider {
    fn read_commands(&self, cx: &App) -> Vec<acp::AvailableCommand> {
        let Some(session) = self.session.upgrade() else {
            return Vec::new();
        };
        session
            .read(cx)
            .acp_thread()
            .map(|thread| thread.read(cx).available_commands().to_vec())
            .unwrap_or_default()
    }
}

/// First non-empty line of `text`, trimmed and clamped to ~80 characters
/// with an ellipsis when truncation actually drops content. Returns
/// `None` for empty / whitespace-only input so callers can drop the
/// documentation field entirely.
fn first_line_summary(text: &str) -> Option<String> {
    const MAX_LEN: usize = 80;
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut buf = String::with_capacity(MAX_LEN.min(line.len()) + 1);
    let mut truncated = false;
    for (count, ch) in line.chars().enumerate() {
        if count == MAX_LEN {
            truncated = true;
            break;
        }
        buf.push(ch);
    }
    if truncated || text.lines().filter(|l| !l.trim().is_empty()).count() > 1 {
        buf.push('…');
    }
    Some(buf)
}

/// Returns the prefix of the buffer up to `position` if and only if the
/// cursor is on the first line, the line begins with `/`, and no
/// whitespace has been typed yet (i.e. we are still completing the
/// command name, not its argument).
fn slash_query_prefix(buffer: &Buffer, position: Point) -> Option<String> {
    if position.row != 0 {
        return None;
    }
    let line_start = Point::new(0, 0);
    let prefix: String = buffer.text_for_range(line_start..position).collect();
    if !prefix.starts_with('/') {
        return None;
    }
    if prefix[1..].chars().any(|c| c.is_whitespace()) {
        return None;
    }
    Some(prefix)
}

impl EditorCompletionProvider for SlashCommandsProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<editor::Editor>,
    ) -> Task<AnyhowResult<Vec<CompletionResponse>>> {
        let prefix = buffer.update(cx, |buffer, _| {
            let position = buffer_position.to_point(buffer);
            slash_query_prefix(buffer, position)
        });
        let Some(prefix) = prefix else {
            return Task::ready(Ok(Vec::new()));
        };
        let commands = self.read_commands(cx);
        if commands.is_empty() {
            return Task::ready(Ok(Vec::new()));
        }
        let snapshot = buffer.read(cx).snapshot();
        let source_range = snapshot.anchor_before(0)..snapshot.anchor_after(prefix.len());
        let query_lower = prefix[1..].to_lowercase();
        let completions: Vec<Completion> = commands
            .into_iter()
            .filter(|cmd| query_lower.is_empty() || cmd.name.to_lowercase().contains(&query_lower))
            .map(|cmd| {
                let new_text = format!("/{} ", cmd.name);
                let label = CodeLabel::plain(format!("/{}", cmd.name), None);
                // The completions popup paints `SingleLine` documentation
                // with a no-wrap `Label`, but if the agent shipped a
                // multi-line description the row blows up vertically and
                // shoulder-checks the items below. Trim to the first
                // non-empty line and cap at ~80 chars + ellipsis so each
                // row stays exactly one text line tall.
                let documentation = first_line_summary(&cmd.description)
                    .map(|line| CompletionDocumentation::SingleLine(line.into()));
                Completion {
                    replace_range: source_range.clone(),
                    new_text,
                    label,
                    documentation,
                    source: CompletionSource::Custom,
                    icon_path: None,
                    icon_color: None,
                    group: None,
                    match_start: None,
                    snippet_deduplication_key: None,
                    insert_text_mode: None,
                    confirm: None,
                }
            })
            .collect();
        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            // `true` forces the editor to re-invoke `completions()` on every
            // keystroke instead of reusing the cached list. We need that
            // because `filter_completions: false` short-circuits the
            // editor's built-in client-side filter — without a fresh
            // call we'd keep showing the unfiltered popup as the user
            // narrows the query.
            is_incomplete: true,
        }]))
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        _text: &str,
        _trigger_in_words: bool,
        cx: &mut Context<editor::Editor>,
    ) -> bool {
        let buffer = buffer.read(cx);
        let pos = position.to_point(buffer);
        slash_query_prefix(buffer, pos).is_some()
    }

    fn filter_completions(&self) -> bool {
        // Filtering happens above (case-insensitive substring match) so the
        // editor's default fuzzy filter doesn't drop entries we already
        // matched, and so descriptions (which we don't want to fuzzy on)
        // can stay in the documentation slot.
        false
    }

    fn sort_completions(&self) -> bool {
        false
    }
}
