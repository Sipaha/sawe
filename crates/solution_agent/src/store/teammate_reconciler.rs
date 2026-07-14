//! Background-teammate reconciler: the Store-side subsystem that tracks
//! managed background agents and background shells spawned inside a live
//! `claude` turn, watches their on-disk `~/.claude/projects/...` artifacts,
//! reconciles finished teammate (`Task`/`Agent`) streams, and scans the
//! parent-session JSONL transcript for `<task-notification>` completion
//! blocks. Relocated verbatim from `store.rs` (Tier-4 god-object refactor)
//! — these are `impl SolutionAgentStore` methods that still own
//! `&mut SolutionAgentStore` / `Context<Self>`; this split moves *source
//! text*, not state ownership.
//!
//! The teammate/shell-reap hardening carried by this cluster is preserved
//! byte-for-byte: #43 (cold-load subagent-dir purge), #47 (stuck-tool
//! liveness gate reading `pty_running` / `silent_secs`) and #48
//! (parent-liveness background-shell reap).

use super::*;

/// `~/.claude/projects/<encoded-cwd>/` — the per-project root claude
/// writes session transcripts and subagent dirs under. `None` when `cwd`
/// is empty (legacy session) or `home_dir()` can't be resolved.
pub(crate) fn claude_project_dir_for(cwd: &std::path::Path) -> Option<PathBuf> {
    if cwd.as_os_str().is_empty() {
        return None;
    }
    let raw = cwd.to_string_lossy();
    let mut encoded = String::with_capacity(raw.len() + 1);
    for c in raw.chars() {
        match c {
            '/' | '.' => encoded.push('-'),
            other => encoded.push(other),
        }
    }
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("projects")
            .join(encoded),
    )
}

pub(crate) fn background_agent_dir_for(cwd: &std::path::Path, acp_session_id: &str) -> Option<PathBuf> {
    Some(
        claude_project_dir_for(cwd)?
            .join(acp_session_id)
            .join("subagents"),
    )
}

/// The PARENT session's on-disk JSONL transcript:
/// `~/.claude/projects/<encoded-cwd>/<acp_session_id>.jsonl`. claude
/// appends every parent-thread message (including the `<task-notification>`
/// user message a background shell emits on completion) to this file. Uses
/// the same cwd encoding as [`background_agent_dir_for`]. `None` under the
/// same conditions (empty cwd / unresolvable home).
pub(crate) fn parent_session_jsonl_for(cwd: &std::path::Path, acp_session_id: &str) -> Option<PathBuf> {
    Some(claude_project_dir_for(cwd)?.join(format!("{acp_session_id}.jsonl")))
}

/// How many of a session's raw claude JSONL transcripts to keep on disk: the
/// live one plus the last `KEEP_RAW_TRANSCRIPTS - 1` abandoned rotations.
pub(crate) const KEEP_RAW_TRANSCRIPTS: usize = 3;

/// Push `abandoned` onto a session's transcript ring and return the ids that
/// now fall outside the keep-window (oldest first). Pure (no IO) so the
/// retention math is unit-tested directly. With `keep = 3` the live transcript
/// is kept implicitly and the last 2 abandoned ones are retained.
pub(crate) fn push_and_evict_transcripts(
    history: &mut VecDeque<String>,
    abandoned: String,
    keep: usize,
) -> Vec<String> {
    history.push_back(abandoned);
    let mut evicted = Vec::new();
    while history.len() > keep.saturating_sub(1) {
        match history.pop_front() {
            Some(old) => evicted.push(old),
            None => break,
        }
    }
    evicted
}

/// Defensive per-tick read cap for the parent-JSONL scan. A single JSONL
/// message line is small; this only bounds a pathological burst.
pub(crate) const PARENT_JSONL_READ_CAP: u64 = 1024 * 1024;

/// Read `[offset, end)` of `path` and split it into COMPLETE lines (those
/// terminated by `\n`). Returns the complete lines plus the byte count
/// consumed (the offset of the byte just past the last `\n`), so a trailing
/// partial line is left unconsumed for the next tick. Returns `None` on any
/// IO error. The read is capped at [`PARENT_JSONL_READ_CAP`] bytes per call.
pub(crate) fn read_complete_lines_from(
    path: &std::path::Path,
    offset: u64,
    end: u64,
) -> Option<(Vec<String>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let to_read = std::cmp::min(end.saturating_sub(offset), PARENT_JSONL_READ_CAP);
    if to_read == 0 {
        return Some((Vec::new(), 0));
    }
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::with_capacity(to_read as usize);
    file.take(to_read).read_to_end(&mut buf).ok()?;
    // Consume up to and including the last newline; bytes after it are a
    // partial line we re-read next tick.
    let last_newline = buf.iter().rposition(|b| *b == b'\n');
    let Some(last_newline) = last_newline else {
        // No newline in the window. Two cases:
        //   - We filled the whole cap → a single line longer than the cap
        //     (e.g. a large inline `Read` result in the transcript). Pinning
        //     the offset here would WEDGE the scan forever (consumed=0 every
        //     tick), silently killing live completion detection for the
        //     session. Skip the oversized region by advancing past the cap;
        //     we may land mid-line but resync at the next newline (a fragment
        //     can't false-match the `<task-notification>` literal).
        //   - We read short of the cap → just a partial trailing line still
        //     being written. Wait (consume 0) for the newline to arrive.
        let consumed = if to_read == PARENT_JSONL_READ_CAP {
            to_read
        } else {
            0
        };
        return Some((Vec::new(), consumed));
    };
    let consumed = (last_newline + 1) as u64;
    let complete = &buf[..=last_newline];
    let lines = String::from_utf8_lossy(complete)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    Some((lines, consumed))
}

/// Pure scan: for each raw JSONL line, if it carries a `<task-notification>`
/// completion block whose `<task-id>` matches a tracked shell, emit the
/// `(id, terminal-state)` pair. The `<...>` tags and `(exit code N)` suffix
/// appear LITERALLY in the JSON string value (only newlines inside are
/// `\n`-escaped), so the existing regex-based `parse_task_notification`
/// matches the raw line directly — no JSON parse / unescape needed.
pub(crate) fn scan_lines_for_completions(
    lines: &[String],
    background_shells: &std::collections::HashMap<
        crate::background_shell::BackgroundShellId,
        crate::background_shell::BackgroundShell,
    >,
) -> Vec<(
    crate::background_shell::BackgroundShellId,
    crate::background_shell::ShellRuntimeState,
)> {
    let mut out = Vec::new();
    for line in lines {
        if !line.contains("<task-notification>") {
            continue;
        }
        if let Some(tn) = crate::background_shell::parse_task_notification(line) {
            if background_shells.contains_key(&tn.id) {
                out.push((tn.id, tn.status));
            }
        }
    }
    out
}

impl SolutionAgentStore {
    /// Spawn (idempotently) a per-session watcher on the
    /// `~/.claude/projects/<encoded-cwd>/<session-id>/subagents/`
    /// directory. Each `PathEvent` on an `agent-<id>.jsonl` filename
    /// triggers a `refresh_background_agent_snapshot` for the matching
    /// tracked `BackgroundAgent`. The watcher task lives in
    /// `background_agent_watchers` keyed by `session_id` — drop the
    /// entry (or drop the store) to cancel.
    ///
    /// Called from the tool-call handler (Task 8) when claude announces
    /// a managed agent. Safe to call repeatedly: a second call for the
    /// same session is a no-op.
    pub(crate) fn ensure_background_agent_watcher(
        &mut self,
        session_id: SolutionSessionId,
        fs: Arc<dyn fs::Fs>,
        cx: &mut Context<Self>,
    ) {
        if self.teammate_watchers.has_agent_watcher(session_id) {
            return;
        }
        let Some(session) = self.session(session_id) else {
            return;
        };
        let acp_session_id = session.read(cx).acp_session_id.clone();
        let cwd = session.read(cx).cwd.clone();
        let subagents_dir = match background_agent_dir_for(&cwd, acp_session_id.0.as_ref()) {
            Some(p) => p,
            None => {
                log::warn!(
                    "background_agents: cannot resolve subagents dir for session {}",
                    session_id
                );
                return;
            }
        };
        let task = cx.spawn(async move |this, cx| {
            let (mut stream, _watcher) = fs
                .watch(&subagents_dir, std::time::Duration::from_millis(200))
                .await;
            use futures::StreamExt;
            while let Some(events) = stream.next().await {
                for event in events {
                    let Some(name) = event.path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if !name.starts_with("agent-") || !name.ends_with(".jsonl") {
                        continue;
                    }
                    let agent_id_str = name
                        .trim_start_matches("agent-")
                        .trim_end_matches(".jsonl")
                        .to_string();
                    // Dropping the Result is the established cancellation
                    // signal: if the store entity is gone, the watcher
                    // task is about to be dropped anyway.
                    let _ = this.update(cx, |this, cx| {
                        this.refresh_background_agent_snapshot(
                            session_id,
                            crate::background_agent::BackgroundAgentId::new(agent_id_str),
                            cx,
                        );
                    });
                }
            }
        });
        self.teammate_watchers.arm_agent_watcher(session_id, task);
    }

    /// Tail the JSONL file for `agent_id` on `session_id`, parse the
    /// last line into a [`BackgroundAgentSnapshot`], write it to
    /// `BackgroundAgent::latest`, and emit
    /// [`SolutionAgentStoreEvent::SessionBackgroundAgentsChanged`] iff
    /// the snapshot was actually stored. No-op when the session has
    /// gone away, the agent isn't tracked anymore, the file can't be
    /// read, or it has no usable last line.
    pub(crate) fn refresh_background_agent_snapshot(
        &mut self,
        session_id: SolutionSessionId,
        agent_id: crate::background_agent::BackgroundAgentId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let Some((jsonl_path, since_offset)) = session
            .read(cx)
            .background_agents
            .get(&agent_id)
            .map(|ba| (ba.jsonl_path.clone(), ba.last_offset))
        else {
            return;
        };
        let tail = match crate::background_agent::tail_jsonl(&jsonl_path, since_offset) {
            Ok(t) => t,
            Err(_) => return,
        };
        let new_offset = tail.new_offset;
        let snapshot = tail.last_line.as_ref().map(|line| {
            let mut snap = crate::background_agent::parse_jsonl_snapshot(line);
            snap.mtime = tail.mtime;
            snap
        });
        let mut changed = false;
        session.update(cx, |s, _| {
            // Allocate the pill's next `change_seq`-axis stamp BEFORE the
            // `get_mut` borrow (both need `&mut s`). Only consumed when this
            // tail yields a new snapshot.
            let next_seq = snapshot.as_ref().map(|_| s.bump_change_seq());
            if let Some(ba) = s.background_agents.get_mut(&agent_id) {
                // Always advance the offset (or rewind on truncation —
                // `tail_jsonl` already handled the reset). Only update
                // `latest` when this tail actually yielded a new line;
                // otherwise the previously-known snapshot remains the
                // user-visible state.
                ba.last_offset = new_offset;
                if let Some(snap) = snapshot {
                    // A managed background agent reaching a terminal stop is
                    // fresh session activity — reset the silence clock so the
                    // supervisor gives the parent a full idle window to resume
                    // on its own before judging, exactly like a background shell
                    // completing (`mark_background_shell_state`). Only on the
                    // transition into terminal (a done agent's JSONL stops
                    // growing, so this fires once). `stop_reason` still feeds
                    // `is_messageable`/supervisor gating, but no longer drives a
                    // stream close — the subagent `Stop` hook is the sole close
                    // authority (`close_teammate_on_stop`).
                    let was_terminal = ba
                        .latest
                        .as_ref()
                        .is_some_and(|s| s.stop_reason.is_some());
                    let now_terminal = snap.stop_reason.is_some();
                    ba.latest = Some(snap);
                    ba.latest_seq = next_seq.unwrap_or(ba.latest_seq);
                    changed = true;
                    if now_terminal && !was_terminal {
                        s.last_activity_at = Utc::now();
                    }
                }
            }
            if changed {
                // The folded pill's body + `seq` derive from `ba.latest` ONLY
                // inside `rebuild_streams`, so a non-terminal snapshot advance
                // must rebuild or the pill freezes at its first-observed state
                // (mirrors the shell twin's unconditional rebuild).
                s.rebuild_streams();
            }
        });
        if changed {
            cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
                session_id,
            ));
        }
    }

    /// Spawn (idempotently) a per-session watcher on the `tasks/`
    /// directory that hosts the background-shell `.output` files (passed
    /// in as `tasks_dir` — it's the parent of the announcement path; we do
    /// NOT re-derive it from cwd the way the managed-agent watcher does,
    /// since the layout is `/tmp/claude-<uid>/<encoded-cwd>/<ses>/tasks/`
    /// rather than `~/.claude/...`). Each `PathEvent` on a `<id>.output`
    /// filename triggers a `refresh_background_shell_snapshot` for the
    /// matching tracked `BackgroundShell`. The watcher task lives in
    /// `background_shell_watchers` keyed by `session_id` — drop the entry
    /// (or drop the store) to cancel. Safe to call repeatedly: a second
    /// call for the same session is a no-op.
    pub(crate) fn ensure_background_shell_watcher(
        &mut self,
        session_id: SolutionSessionId,
        fs: Arc<dyn fs::Fs>,
        tasks_dir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.teammate_watchers.has_shell_watcher(session_id) {
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            let (mut stream, _watcher) = fs
                .watch(&tasks_dir, std::time::Duration::from_millis(200))
                .await;
            use futures::StreamExt;
            while let Some(events) = stream.next().await {
                for event in events {
                    let Some(name) = event.path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if !name.ends_with(".output") {
                        continue;
                    }
                    let shell_id_str = name.trim_end_matches(".output").to_string();
                    // Dropping the Result is the established cancellation
                    // signal: if the store entity is gone, the watcher
                    // task is about to be dropped anyway.
                    let _ = this.update(cx, |this, cx| {
                        this.refresh_background_shell_snapshot(
                            session_id,
                            crate::background_shell::BackgroundShellId::new(shell_id_str),
                            cx,
                        );
                    });
                }
            }
        });
        self.teammate_watchers.arm_shell_watcher(session_id, task);
    }

    /// Live-tail the `.output` file for `shell_id` on `session_id`, write
    /// the trailing window into `BackgroundShell::latest`, and emit
    /// [`SolutionAgentStoreEvent::SessionBackgroundShellsChanged`] iff the
    /// file actually advanced. Unlike the managed-agent snapshot (last
    /// JSONL line only), this reads the full trailing window for display —
    /// so we pass `0` to `tail_output` and let its 64 KiB cap bound the
    /// read. No-op when the session is gone, the shell isn't tracked, or
    /// the file can't be read yet (missing file → "no snapshot yet", not a
    /// failure). Does NOT touch `state`: registration sets `Running` and
    /// the terminal-state transition (Task 8) owns the rest.
    pub(crate) fn refresh_background_shell_snapshot(
        &mut self,
        session_id: SolutionSessionId,
        shell_id: crate::background_shell::BackgroundShellId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let Some((output_path, stored_last_offset)) = session
            .read(cx)
            .background_shells
            .get(&shell_id)
            .map(|sh| (sh.output_path.clone(), sh.last_offset))
        else {
            return;
        };
        // Always read the full trailing window (offset 0) for display; the
        // `changed` decision below uses the file length, not this read start.
        let tail = match crate::background_shell::tail_output(&output_path, 0) {
            Ok(t) => t,
            Err(_) => return,
        };
        let new_offset = tail.new_offset;
        // The file advanced iff its end moved past what we last recorded.
        // A first non-empty read (stored offset 0, file non-empty) also
        // counts as changed.
        let changed = new_offset != stored_last_offset;
        let tail_text = tail.text;
        let tail_mtime = tail.mtime;
        session.update(cx, |s, _| {
            if let Some(sh) = s.background_shells.get_mut(&shell_id) {
                sh.last_offset = new_offset;
                if !tail_text.is_empty() {
                    sh.latest = Some(crate::background_shell::BackgroundShellSnapshot {
                        mtime: tail_mtime,
                        output_tail: tail_text.into(),
                    });
                }
            }
            // Phase 6d-A: refresh the derived Shell stream so its fenced body +
            // mtime-based `seq` track the new tail.
            s.rebuild_streams();
        });
        if changed {
            cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
                session_id,
            ));
        }
    }

    /// Flip a tracked background shell's [`ShellRuntimeState`] (terminal
    /// signal handler). Mutates the in-memory map entry, emits
    /// [`SolutionAgentStoreEvent::SessionBackgroundShellsChanged`], and
    /// fire-and-forget upserts the row's `state_text` to SQLite (rebuilt
    /// from the in-memory shell). No-op when the session or the shell id is
    /// no longer tracked. Used by both terminal signals: the `KillShell`
    /// tool_call (→ `Killed`) and the `<task-notification>` user message
    /// (→ `Exited(code)`).
    pub(crate) fn mark_background_shell_state(
        &mut self,
        session_id: SolutionSessionId,
        shell_id: crate::background_shell::BackgroundShellId,
        new_state: crate::background_shell::ShellRuntimeState,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        // Capture the row fields under a short read scope, mutating the state
        // in the same `update` so the persisted row matches the in-memory one.
        let row = session.update(cx, |s, _| {
            let shell = s.background_shells.get_mut(&shell_id)?;
            if shell.state == new_state {
                // Idempotent: a duplicate terminal signal (e.g. a re-observed
                // KillShell on a cold→live replay) must not re-emit.
                return None;
            }
            shell.state = new_state.clone();
            // A background command COMPLETING is fresh session activity: reset
            // the silence clock. While it ran, `has_live_background_work` kept
            // the supervisor quiet, but `last_activity_at` stayed frozen at
            // launch — so the moment it finishes the accrued silence is already
            // past `IDLE_THRESHOLD_SECS` and the judge would fire INSTANTLY,
            // racing (and usually losing to) the agent resuming ON ITS OWN to
            // read the result (a `Bash(run_in_background)` orphan continuation /
            // `<task-notification>`). Bumping the clock here gives the agent a
            // full fresh idle window to self-resume before the supervisor
            // judges; if it genuinely doesn't, the judge fires after the window
            // as intended. (The send-time session-idle re-check in
            // `apply_verdict` is the backstop for the residual race.)
            if matches!(
                new_state,
                crate::background_shell::ShellRuntimeState::Exited(_)
                    | crate::background_shell::ShellRuntimeState::Killed
            ) {
                s.last_activity_at = Utc::now();
            }
            let row = crate::db::BackgroundShellRow {
                solution_session_id: session_id.to_string(),
                shell_id: shell.id.as_str().to_string(),
                command: shell.command.to_string(),
                output_path: shell.output_path.to_string_lossy().into_owned(),
                registered_at_ms: shell.registered_at.timestamp_millis(),
                last_tail: shell
                    .latest
                    .as_ref()
                    .map(|snap| snap.output_tail.to_string()),
                last_mtime_ms: shell.latest.as_ref().and_then(|snap| {
                    snap.mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_millis() as i64)
                }),
                state_text: new_state.to_state_text(),
            };
            // Phase 6d-A: a terminal flip (`Exited`/`Killed`) drops this shell
            // from the derived stream mirror (only `Running` shells are folded
            // in) — that IS the auto-close. Rebuild here now the `shell` borrow
            // above has been released into the owned `row`.
            s.rebuild_streams();
            Some(row)
        });
        let Some(row) = row else {
            return;
        };
        cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
            session_id,
        ));
        if let Some(db) = self.persistence.clone() {
            cx.background_spawn(async move {
                db.save_background_shell(row).await.log_err();
            })
            .detach();
        }
    }

    /// Scan a freshly-observed thread entry for a `<task-notification>`
    /// completion block and, when it targets a tracked background shell,
    /// flip that shell to its terminal [`ShellRuntimeState`] via
    /// [`Self::mark_background_shell_state`].
    ///
    /// claude's harness injects a `<task-notification>` **user-role message**
    /// into the thread when a `Bash(run_in_background=true)` command finishes.
    /// That arrives as an [`acp_thread::AgentThreadEntry::UserMessage`], NOT a
    /// `ToolCall`, so `apply_subagent_lifecycle` (which early-returns on
    /// non-ToolCall entries) never sees it — hence this separate scan, called
    /// from the `NewEntry` / `EntryUpdated` arms.
    ///
    /// No-op for any other entry shape, an unparseable / non-notification
    /// user message, or a notification whose `<task-id>` isn't a shell we
    /// track. The text is read from the user message's `ContentBlock` via
    /// `to_markdown`, which returns the raw markdown source (the unescaped
    /// `<task-notification>` block) for a `Markdown` block.
    pub(crate) fn observe_task_notification(
        &mut self,
        session_id: SolutionSessionId,
        local_entry_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(session_entity) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        let notification = {
            let session = session_entity.read(cx);
            let Some(thread) = session.acp_thread() else {
                return;
            };
            let thread_ref = thread.read(cx);
            let Some(entry) = thread_ref.entries().get(local_entry_index) else {
                return;
            };
            let acp_thread::AgentThreadEntry::UserMessage(message) = entry else {
                return;
            };
            let text = message.content.to_markdown(cx);
            crate::background_shell::parse_task_notification(text)
        };
        let Some(notification) = notification else {
            return;
        };
        if session_entity
            .read(cx)
            .background_shells
            .contains_key(&notification.id)
        {
            self.mark_background_shell_state(session_id, notification.id, notification.status, cx);
        }
    }

    /// Subagent-tab lifecycle hook. Inspects the entry at `entry_index` in
    /// the session's live `AcpThread` and:
    ///   * if it's a brand-new `Task`/`Agent` ToolCall in `InProgress` and
    ///     not already tracked → captures its friendly label in
    ///     `SolutionSession::teammate_labels` and emits
    ///     [`SolutionAgentStoreEvent::SessionSubagentsChanged`];
    ///   * if it's a tracked id whose status just flipped to a terminal
    ///     state (`Completed`/`Failed`/`Rejected`/`Canceled`) → closes the
    ///     inline Task's stream (reclaiming its label) and emits the same event.
    ///
    /// Any other shape (non-tool entry, non-Task tool, status still
    /// `InProgress`/`Pending` on an already-tracked id, terminal status on
    /// an unknown id) is a no-op and emits nothing. Mutations are gated
    /// behind a structural check to keep `SessionSubagentsChanged` from
    /// firing on every chunk of a streaming Task subagent's body.
    ///
    /// The cold-thread branch is excluded: an entry only exists in a live
    /// `AcpThread`, so when the session is cold (`acp_thread()` is `None`)
    /// there is nothing to track yet. The next live attach will replay the
    /// in-flight tool calls through `NewEntry`, which re-enters this hook.
    pub(crate) fn apply_subagent_lifecycle(
        &mut self,
        session_id: SolutionSessionId,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(session_entity) = self.sessions.get(&session_id).cloned() else {
            return;
        };
        // Capture the relevant ToolCall fields in a small read scope so we
        // can mutate the session entity right after without overlapping
        // borrows.
        struct Snapshot {
            id: SharedString,
            is_task_like: bool,
            is_in_progress: bool,
            is_terminal: bool,
            label_from_raw_input: Option<SharedString>,
            subagent_type: Option<String>,
            /// The tool's programmatic name (e.g. `"Task"`, `"Agent"`)
            /// captured so the post-lifecycle branch can dispatch on
            /// `eq_ignore_ascii_case("agent")` without re-borrowing the
            /// entry from the thread.
            tool_name: Option<String>,
            /// JSON-encoded `raw_output` payload (only meaningful for the
            /// terminal `Agent` branch — claude's managed-agent dispatcher
            /// stashes `agentId` + `output_file` here when the tool call
            /// completes). Empty for in-progress / non-Agent calls.
            raw_output_text: Option<String>,
            /// The tool call's rendered content text (the `tool_result` body).
            /// For an async `Agent` launch claude puts the "Async agent
            /// launched successfully… agentId: … output_file: …" announcement
            /// HERE (the tool_result content), NOT in `raw_output` — so the
            /// managed-agent registration parses this as a fallback. Populated
            /// only for terminal task-like calls; `None` otherwise.
            content_text: Option<String>,
            /// Raw tool-call input JSON, captured so the background-shell
            /// branch can read `run_in_background` + `command` without
            /// re-borrowing the entry. `None` when the tool call has no
            /// `raw_input`.
            raw_input: Option<serde_json::Value>,
        }
        let snapshot = {
            let session = session_entity.read(cx);
            let Some(thread) = session.acp_thread() else {
                return;
            };
            let thread_ref = thread.read(cx);
            let Some(entry) = thread_ref.entries().get(entry_index) else {
                return;
            };
            let acp_thread::AgentThreadEntry::ToolCall(call) = entry else {
                return;
            };
            let tool_name = call
                .tool_name
                .as_ref()
                .map(|s| s.as_ref())
                .unwrap_or_default();
            let is_task_like = matches!(tool_name, "Task" | "Agent");
            let is_in_progress = matches!(call.status, acp_thread::ToolCallStatus::InProgress);
            let is_terminal = matches!(
                call.status,
                acp_thread::ToolCallStatus::Completed
                    | acp_thread::ToolCallStatus::Failed
                    | acp_thread::ToolCallStatus::Rejected
                    | acp_thread::ToolCallStatus::Canceled
            );
            let (label_from_raw_input, subagent_type) = match call.raw_input.as_ref() {
                Some(raw) => {
                    let desc = raw
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(|s| SharedString::from(s.to_owned()));
                    let stype = raw
                        .get("subagent_type")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_owned());
                    (desc, stype)
                }
                None => (None, None),
            };
            let tool_name_owned = if tool_name.is_empty() {
                None
            } else {
                Some(tool_name.to_string())
            };
            let raw_output_text = call
                .raw_output
                .as_ref()
                .and_then(|v| serde_json::to_string(v).ok());
            // The content is read by the two terminal announcement parsers — the
            // async-`Agent` one (task-like) and the background-`Bash` one — and
            // BOTH need it: `claude_native::translate` only ever calls
            // `.raw_input(..)`, never `.raw_output(..)`, so `call.raw_output` is
            // always `None` and `raw_output_text` is always empty. Skip the
            // string-building for every other tool.
            let content_text = if is_terminal && (is_task_like || tool_name == "Bash") {
                let mut text = String::new();
                for content in &call.content {
                    if let acp_thread::ToolCallContent::ContentBlock(block) = content {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(block.to_markdown(cx));
                    }
                }
                (!text.is_empty()).then_some(text)
            } else {
                None
            };
            Snapshot {
                id: SharedString::from(call.id.0.to_string()),
                is_task_like,
                is_in_progress,
                is_terminal,
                label_from_raw_input,
                subagent_type,
                tool_name: tool_name_owned,
                raw_output_text,
                content_text,
                raw_input: call.raw_input.clone(),
            }
        };

        // Background-shell registration (Tasks 7 + 9 of the Background
        // Shells Strip plan). claude's `Bash(run_in_background=true)` launches
        // a detached process that writes its combined stdout/stderr to an
        // on-disk `tasks/<id>.output` file; the path + short id are surfaced
        // in the launch announcement carried in the tool call's `raw_output`.
        //
        // This MUST run before the `is_task_like` early-return below: `Bash`
        // is not in the `Task | Agent` task-like set, so the gate would
        // otherwise skip it. We register the shell, persist it, arm the
        // per-session `tasks/` watcher, and do one inline tail to close the
        // launch→first-write race — then fall through to the early-return
        // (which fires for `Bash`, leaving the subagent-pill logic untouched).
        if snapshot.is_terminal
            && snapshot.tool_name.as_deref() == Some("Bash")
            && snapshot
                .raw_input
                .as_ref()
                .and_then(|v| v.get("run_in_background"))
                .and_then(|v| v.as_bool())
                == Some(true)
        {
            // The "Command running in background with ID: … Output is being
            // written to: ….output" announcement lives in the tool_result body,
            // which the native translator surfaces as the tool call's CONTENT —
            // `raw_output` is never set (see the `content_text` comment above).
            // Parse `raw_output` first for forward-compat with a dispatcher that
            // stashes it there, then fall back to content. Without the content
            // fallback the shell never registers and its strip pill never
            // appears — exactly the gap the async-`Agent` path already patched.
            let announcement = snapshot
                .raw_output_text
                .as_deref()
                .and_then(crate::background_shell::parse_bash_bg_launch)
                .or_else(|| {
                    snapshot
                        .content_text
                        .as_deref()
                        .and_then(crate::background_shell::parse_bash_bg_launch)
                });
            if let Some((shell_id, output_path)) = announcement {
                let already = session_entity
                    .read(cx)
                    .background_shells
                    .contains_key(&shell_id);
                if !already {
                    // Command label: prefer `raw_input.command`, fall back to
                    // `raw_input.description`; truncate to 120 chars so a long
                    // pipeline doesn't blow out the strip.
                    let command_label: SharedString = snapshot
                        .raw_input
                        .as_ref()
                        .and_then(|v| {
                            v.get("command")
                                .or_else(|| v.get("description"))
                                .and_then(|c| c.as_str())
                        })
                        .map(|s| s.chars().take(120).collect::<String>())
                        .unwrap_or_default()
                        .into();
                    let registered_at = chrono::Utc::now();
                    let id_for_insert = shell_id.clone();
                    let path_for_insert = output_path.clone();
                    let command_for_insert = command_label.clone();
                    session_entity.update(cx, |s, _| {
                        s.background_shells.insert(
                            id_for_insert.clone(),
                            crate::background_shell::BackgroundShell {
                                id: id_for_insert.clone(),
                                command: command_for_insert,
                                output_path: path_for_insert,
                                registered_at,
                                latest: None,
                                last_offset: 0,
                                state: crate::background_shell::ShellRuntimeState::Running,
                            },
                        );
                        s.background_shell_order.push(id_for_insert);
                        // Phase 6d-A: the shell's derived `StreamId::Shell` tab
                        // is produced by `rebuild_streams` from `background_shells`,
                        // so every shell mutation must rebuild or the mirror drifts.
                        s.rebuild_streams();
                    });
                    cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
                        session_id,
                    ));

                    // Persist to SQLite if the store has a backing DB. The
                    // in-memory test stores leave `persistence` as `None`.
                    if let Some(db) = self.persistence.clone() {
                        let row = crate::db::BackgroundShellRow {
                            solution_session_id: session_id.to_string(),
                            shell_id: shell_id.as_str().to_string(),
                            command: command_label.to_string(),
                            output_path: output_path.to_string_lossy().into_owned(),
                            registered_at_ms: registered_at.timestamp_millis(),
                            last_tail: None,
                            last_mtime_ms: None,
                            state_text: "running".to_string(),
                        };
                        cx.background_spawn(async move {
                            db.save_background_shell(row).await.log_err();
                        })
                        .detach();
                    }

                    // Arm the per-session watcher on the `tasks/` directory
                    // (the announcement path's parent). A session without a
                    // project skips the watcher — the row is still registered
                    // and the inline refresh below seeds the first snapshot.
                    if let (Some(fs), Some(tasks_dir)) = (
                        session_entity
                            .read(cx)
                            .project
                            .as_ref()
                            .map(|p| p.read(cx).fs().clone()),
                        output_path.parent().map(|p| p.to_path_buf()),
                    ) {
                        self.ensure_background_shell_watcher(session_id, fs, tasks_dir, cx);
                    }

                    // Close the launch→watcher-subscribe race: claude often
                    // has already written the first bytes by the time `Bash`
                    // returns, but `fs.watch` resolves on a background task.
                    self.refresh_background_shell_snapshot(session_id, shell_id, cx);
                }
            }
        }

        // `KillShell` terminal tool_call → mark the targeted background shell
        // `Killed`. claude emits a `KillShell` ToolCall (Execute kind) whose
        // `raw_input` carries the `shell_id`/`bash_id` of the shell to stop;
        // when it completes, the shell is dead. Like the `Bash(bg)` branch
        // above, this runs BEFORE the `is_task_like` early-return because
        // `KillShell` is not in the `Task | Agent` set.
        if snapshot.is_terminal && snapshot.tool_name.as_deref() == Some("KillShell") {
            if let Some(shell_id) = snapshot
                .raw_input
                .as_ref()
                .and_then(crate::background_shell::parse_kill_shell_input)
            {
                if session_entity
                    .read(cx)
                    .background_shells
                    .contains_key(&shell_id)
                {
                    self.mark_background_shell_state(
                        session_id,
                        shell_id,
                        crate::background_shell::ShellRuntimeState::Killed,
                        cx,
                    );
                }
            }
        }

        if !snapshot.is_task_like {
            return;
        }
        let id = snapshot.id;

        let changed = if snapshot.is_in_progress {
            // Defensive: a duplicate NewEntry for the same id (or an
            // InProgress→InProgress EntryUpdated as raw_input streams in) must
            // not re-insert or re-emit. Only the first observation registers
            // the tab.
            let already_tracked = session_entity.read(cx).teammate_labels.contains_key(&id);
            if already_tracked {
                // Label is intentionally locked at first observation. Later
                // EntryUpdated events that finally fill in raw_input.description
                // are discarded here on purpose — otherwise a streamed tool_use
                // input would relabel the tab mid-flight and flicker the strip.
                false
            } else {
                let label = snapshot
                    .label_from_raw_input
                    .unwrap_or_else(|| label_fallback(&id, snapshot.subagent_type.as_deref()));
                // Capture the durable friendly label for BOTH inline `Task` and
                // async `Agent` teammates (both are `is_task_like`). It rides
                // `Stream.label` via `rebuild_streams` and is reclaimed when the
                // teammate's stream closes (`close_stream`).
                session_entity.update(cx, |s, _| {
                    s.teammate_labels.insert(id.clone(), label);
                });
                true
            }
        } else if snapshot.is_terminal {
            // Symmetric defensive guard: a terminal-status EntryUpdated on an
            // id we never registered (e.g. the InProgress event arrived after
            // a status flip on a cold→live transition) is a no-op.
            let tracked = session_entity.read(cx).teammate_labels.contains_key(&id);
            if tracked {
                // Terminal status is a GENUINE "teammate done" signal ONLY for an
                // inline `Task` (its tool-call stays InProgress for the whole run
                // and completes only when the Task finishes). An async `Agent`
                // teammate's spawn tool-call flips to Completed IMMEDIATELY at
                // spawn-ack while the teammate keeps streaming `subagent_id`-tagged
                // entries into the parent thread for minutes — so closing its
                // demux `Teammate` stream here would suppress the still-live
                // teammate (decision #5: the parent-thread demux IS its source of
                // truth). So auto-close the stream for `Task` only; the async
                // `Agent`'s real done-signal (stop_reason / completion) drives its
                // close in a later phase. `close_stream` reclaims the inline Task's
                // `teammate_labels` entry; the async `Agent` keeps its label (its
                // stream stays open past spawn-ack) and reclaims it on its own close.
                let is_async_agent = tool_name_is_agent(snapshot.tool_name.as_deref());
                session_entity.update(cx, |s, _| {
                    if !is_async_agent {
                        s.close_stream(
                            crate::stream::StreamId::Teammate(id.clone()),
                            gpui::SharedString::new_static("done"),
                        );
                    }
                });
                true
            } else {
                false
            }
        } else {
            // Pending / WaitingForConfirmation transitions on a Task/Agent
            // tool call are not lifecycle signals — claude almost never goes
            // through these for subagents (they spawn directly into
            // InProgress), but be defensive in case future SDK shapes do.
            false
        };

        if changed {
            self.mark_subagents_changed(session_id, cx);
        }

        // Managed-agent registration (Task 8 of the Background Agents Strip
        // plan). claude_code's `Agent` tool is its async sub-agent dispatch;
        // when the call completes its `raw_output` carries `agentId: <hex>`
        // + `output_file: <path>.output` so we can tail the JSONL transcript
        // the worker is appending to. We register a `BackgroundAgent` for
        // every fresh announcement and spawn the per-session directory
        // watcher (idempotent — `ensure_background_agent_watcher` no-ops on
        // a duplicate call). The Task branch above already removed the
        // subagent pill, so the Agent dispatch briefly shows as an active
        // subagent and then transitions to a background-agent strip entry —
        // matches the pre-feature behaviour for `Task` and adds the strip
        // on top.
        if snapshot.is_terminal && tool_name_is_agent(snapshot.tool_name.as_deref()) {
            // The announcement (`agentId:` + `output_file:`) lives in the
            // tool_result body, which the native adapter surfaces as the tool
            // call's CONTENT, not `raw_output` (that stays null for an async
            // `Agent` launch). Parse `raw_output` first for forward-compat with
            // any dispatcher that stashes it there, then fall back to content —
            // the current claude path. Without the content fallback the
            // background-agent pill never registers, so an actively-streaming
            // teammate shows no strip tab and its output (tagged in the parent
            // thread) has nowhere to go but the Main tab.
            let announcement = crate::background_agent::managed_agent_announcement(
                snapshot.raw_output_text.as_deref(),
                snapshot.content_text.as_deref(),
            );
            if let Some((agent_id_str, output_file)) = announcement {
                let canonical =
                    std::fs::read_link(&output_file).unwrap_or_else(|_| output_file.clone());
                // Capture the parent `Agent` spawn tool-call's tool_use id
                // BEFORE the `BackgroundAgentId::new` binding below shadows the
                // outer `id` (= `snapshot.id`). This is the key of the teammate's
                // demux `Teammate` stream, needed to auto-close it on the agent's
                // real terminal `stop_reason`.
                let parent_toolu = id.clone();
                let id = crate::background_agent::BackgroundAgentId::new(agent_id_str);
                let already = session_entity.read(cx).background_agents.contains_key(&id);
                if !already {
                    let id_for_insert = id.clone();
                    let path_for_insert = canonical.clone();
                    session_entity.update(cx, |s, _| {
                        let parent_toolu_for_pending = parent_toolu.clone();
                        s.background_agents.insert(
                            id_for_insert.clone(),
                            crate::background_agent::BackgroundAgent {
                                id: id_for_insert.clone(),
                                jsonl_path: path_for_insert,
                                registered_at: chrono::Utc::now(),
                                latest: None,
                                last_offset: 0,
                                parent_tool_use_id: Some(parent_toolu),
                                latest_seq: 0,
                                killed: false,
                            },
                        );
                        s.background_agent_order.push(id_for_insert.clone());
                        // A `Stop` hook may have arrived before this registration
                        // (the hook races the `agentId:` announcement). Honor it
                        // now: close the teammate stream immediately.
                        if s.take_pending_stop(&id_for_insert) {
                            s.close_stream(
                                crate::stream::StreamId::Teammate(parent_toolu_for_pending),
                                gpui::SharedString::new_static("done"),
                            );
                        }
                    });
                    cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
                        session_id,
                    ));

                    // Persist to SQLite if the store has a backing DB.
                    // In-memory test stores leave `persistence` as `None`
                    // and rely on the in-RAM map only.
                    if let Some(db) = self.persistence.clone() {
                        let row = crate::db::BackgroundAgentRow {
                            solution_session_id: session_id.to_string(),
                            agent_id: id.as_str().to_string(),
                            jsonl_path: canonical.to_string_lossy().into_owned(),
                            registered_at_ms: chrono::Utc::now().timestamp_millis(),
                            last_seen_label: None,
                            last_mtime_ms: None,
                            stop_reason: None,
                        };
                        cx.background_spawn(async move {
                            db.save_background_agent(row).await.log_err();
                        })
                        .detach();
                    }

                    // The watcher needs a `fs::Fs` handle. `SolutionAgentStore`
                    // has no `fs` field; source it from the session's project
                    // (most live sessions have one). A session without a
                    // project just skips the watcher — the row is still
                    // registered and the UI can render the pill, but live
                    // tailing waits for a project attach.
                    if let Some(fs) = session_entity
                        .read(cx)
                        .project
                        .as_ref()
                        .map(|p| p.read(cx).fs().clone())
                    {
                        self.ensure_background_agent_watcher(session_id, fs, cx);
                    }

                    // Close the registration→watcher-subscribe race window:
                    // claude writes the first JSONL line nearly instantly
                    // after `Agent` returns, but `fs.watch` resolves on a
                    // background task — so without an inline refresh the
                    // first snapshot can be missed entirely and the pill
                    // would sit at the default `Generating…` until the
                    // sub-agent's next write.
                    self.refresh_background_agent_snapshot(session_id, id, cx);
                }
            }
        }

        // Self-heal immediately when a Task terminalises: the mid-session
        // reconcile is cheap for one session and catches the same terminal-but-
        // missed cases the event-driven close above can drop (e.g. a Task whose
        // tool-call flipped terminal on an EntryUpdated we didn't route through
        // the `is_terminal` branch). It is SELECTIVE, so a still-live teammate
        // in this session is untouched.
        self.reconcile_finished_teammate_streams(session_id, cx);
    }

    /// One pass over every session's background agents. The `Stop` hook
    /// ([`Self::close_teammate_on_stop`]) closes every normal completion the
    /// moment it happens, so this tick is now a pure backstop: it removes
    /// agents that have been silently dead beyond
    /// `agent.managed_agent_stale_timeout_secs +
    /// agent.managed_agent_dead_linger_secs`, plus (for a still-live parent)
    /// agents that went silent past the generous
    /// [`BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS`] cap — catching a genuinely
    /// dropped hook or a wedged subprocess. This window has to stay long: the
    /// `Stop` hook fires only at end-of-turn, never mid-tool-call, so a
    /// background `Agent` running one long silent tool call (a multi-minute
    /// build, a slow test, a quiet network call) writes nothing to its JSONL
    /// for the duration and must not be mistaken for a lost hook (hardening
    /// #9). Dead detection itself (orange pill) is rendering-side using the
    /// same stale timeout — the tick just drops the entries that have fully
    /// expired.
    pub fn tick_background_agents(&mut self, cx: &mut Context<Self>) {
        let expiry = std::time::Duration::from_secs(
            MANAGED_AGENT_STALE_TIMEOUT_SECS + MANAGED_AGENT_DEAD_LINGER_SECS,
        );
        let lost_hook_backstop =
            std::time::Duration::from_secs(BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS);
        let now = std::time::SystemTime::now();
        let session_ids: Vec<SolutionSessionId> =
            self.all_sessions().map(|e| e.read(cx).id).collect();
        for session_id in session_ids {
            let Some(session) = self.session(session_id) else {
                continue;
            };
            // Skip sessions with no registered agents — the vast majority of
            // sessions never spawn a managed agent, and `update` is not free.
            if session.read(cx).background_agents.is_empty() {
                continue;
            }
            let to_remove: Vec<crate::background_agent::BackgroundAgentId> =
                session.update(cx, |s, _| {
                    // The `Stop` hook is authoritative for normal completion
                    // (`Self::close_teammate_on_stop`), so this filter is purely a
                    // backstop for a dropped hook or a wedged subprocess — it no
                    // longer treats JSONL `stop_reason` as a signal at all. A live
                    // `acp_thread` means the owning subprocess is still up and the
                    // hook SHOULD fire soon, so a live-parent agent gets the same
                    // generous `BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS` cap the
                    // shell reaper uses — output-silence alone is not death, a
                    // long silent tool call (build/test/curl) writes nothing to
                    // the JSONL for the duration (hardening #9). With no thread
                    // (reconnect / crash / close) no hook can ever arrive, so the
                    // ordinary stale+linger timeout applies.
                    let parent_alive = s.acp_thread().is_some();
                    let candidates: Vec<crate::background_agent::BackgroundAgentId> = s
                        .background_agent_order
                        .iter()
                        .filter(|id| {
                            let Some(ba) = s.background_agents.get(id) else {
                                return false;
                            };
                            // A KILLED agent (its subprocess was replaced by a
                            // reconnect) can never report anything again, even
                            // though the session now has a NEW live thread — so
                            // the lost-hook backstop must not apply to it. It
                            // lingers on the ordinary stale+linger window, long
                            // enough for the user to see the terminal tab, and is
                            // then reaped like any dead agent.
                            let stale_threshold = if parent_alive && !ba.killed {
                                lost_hook_backstop
                            } else {
                                expiry
                            };
                            // Age from the snapshot's mtime when one exists, else
                            // from `registered_at` — mirroring the shell reaper.
                            // A snapshot-less async agent (JSONL never parsed)
                            // must still age out or its map entry (and stream)
                            // would leak forever.
                            let age = match ba.latest.as_ref() {
                                Some(snap) => now.duration_since(snap.mtime).unwrap_or_default(),
                                None => {
                                    let registered: std::time::SystemTime =
                                        ba.registered_at.into();
                                    now.duration_since(registered).unwrap_or_default()
                                }
                            };
                            age > stale_threshold
                        })
                        .cloned()
                        .collect();
                    // Safety-net close of each reaped teammate's demux stream:
                    // covers a missed terminal-transition edge in
                    // `refresh_background_agent_snapshot` (e.g. an agent that
                    // registered already-terminal, or one reaped as stale-dead).
                    // Collect before removal so `close_stream` (which takes
                    // `&mut s`) runs after the borrow ends; it is idempotent.
                    let close_teammates: Vec<(SharedString, SharedString)> = candidates
                        .iter()
                        .filter_map(|id| {
                            let ba = s.background_agents.get(id)?;
                            let parent = ba.parent_tool_use_id.clone()?;
                            // A killed agent closes as "killed", never "done" — it
                            // was reaped mid-flight with its parent subprocess and
                            // must not be reported as a completion.
                            let reason = if ba.killed {
                                crate::background_agent::KILLED_REASON
                            } else {
                                ba.latest
                                    .as_ref()
                                    .and_then(|snap| snap.stop_reason.clone())
                                    .unwrap_or_else(|| gpui::SharedString::new_static("done"))
                            };
                            Some((parent, reason))
                        })
                        .collect();
                    for id in &candidates {
                        s.background_agents.remove(id);
                        s.background_agent_order.retain(|x| x != id);
                    }
                    for (parent_toolu, reason) in close_teammates {
                        s.close_stream(crate::stream::StreamId::Teammate(parent_toolu), reason);
                    }
                    candidates
                });
            if !to_remove.is_empty() {
                cx.emit(SolutionAgentStoreEvent::SessionBackgroundAgentsChanged(
                    session_id,
                ));
                if let Some(db) = self.persistence.clone() {
                    let session_id_string = session_id.to_string();
                    for agent_id in to_remove {
                        let db = db.clone();
                        let session_id_string = session_id_string.clone();
                        let agent_id_string = agent_id.as_str().to_string();
                        cx.background_spawn(async move {
                            db.delete_background_agent(session_id_string, agent_id_string)
                                .await
                                .log_err();
                        })
                        .detach();
                    }
                }
            }
        }
    }

    /// One pass over every session's teammate pills, closing each stream whose
    /// completion is provable. Runs on the 5s tick so it fires mid-session,
    /// unlike the →Idle GC. See
    /// [`Self::reconcile_finished_teammate_streams`] for the per-session rules.
    pub fn reconcile_all_finished_teammate_streams(&mut self, cx: &mut Context<Self>) {
        let session_ids: Vec<SolutionSessionId> =
            self.all_sessions().map(|e| e.read(cx).id).collect();
        for session_id in session_ids {
            self.reconcile_finished_teammate_streams(session_id, cx);
        }
    }

    /// Mid-session SELECTIVE reconcile of a session's teammate (subagent) pills.
    ///
    /// Desktop strip pills mirror `session.streams`; a teammate pill vanishes
    /// only when its `Teammate(toolu)` stream closes. The event-driven close
    /// paths ([`Self::apply_subagent_lifecycle`] for inline `Task`s,
    /// [`Self::refresh_background_agent_snapshot`] for async `Agent`s) can miss
    /// their signal (a dropped `EntryUpdated`, a missed JSONL watcher write, a
    /// last line that isn't terminal). The ONLY catch-all today is the →Idle
    /// strip GC in [`Self::mutate_state`], which is gated on the `!Idle → Idle`
    /// transition — so a session that stays busy for a long time NEVER runs it
    /// and finished-teammate pills linger until the session finally goes Idle
    /// (observed: ~1 hour). This reconcile closes such a stream the moment its
    /// completion is provable, WITHOUT waiting for →Idle.
    ///
    /// Unlike the →Idle GC (which blanket-closes every non-async teammate
    /// because Idle proves nothing is running), this must be SELECTIVE — some
    /// teammates are genuinely still running mid-session. A `Teammate(toolu)`
    /// stream is closed only when (any):
    ///   1. it is NOT a registered async `Agent` and has no matching tool-call
    ///      entry left in the thread (rewound/removed → orphaned);
    ///   2. it is an inline `Task` (its spawn tool-call's `tool_name` is NOT
    ///      agent) whose tool-call entry is TERMINAL; or
    ///   3. it is an async `Agent` (some `background_agent` has
    ///      `parent_tool_use_id == toolu`) whose latest snapshot is stale
    ///      beyond [`MANAGED_AGENT_STALE_TIMEOUT_SECS`] (dead/orphaned parent)
    ///      or, with a still-live parent, beyond the generous
    ///      [`BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS`] cap (a genuinely lost
    ///      hook or wedged subprocess — kept long because the `Stop` hook
    ///      fires only at end-of-turn, so a long silent tool call must not be
    ///      mistaken for a lost hook, hardening #9). Normal completion is
    ///      closed by the `Stop` hook ([`Self::close_teammate_on_stop`]) — a
    ///      JSONL `stop_reason` is no longer, by itself, a close trigger
    ///      here.
    ///
    /// It is NEVER closed for a live inline `Task` (tool-call present +
    /// non-terminal), a fresh async `Agent` (snapshot recent), or —
    /// critically — an async `Agent` merely because its spawn tool-call is
    /// terminal: that is spawn-ack, the teammate streams for minutes after, and
    /// closing there is the pre-6c premature-close regression. Async
    /// classification is by the `background_agents` map (NOT the tool-call
    /// tool_name), so a spawn-ack terminal `Agent` whose registration is still
    /// pending is kept via the tool_name guard below.
    pub(crate) fn reconcile_finished_teammate_streams(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        // Cheap read-side guard: the vast majority of sessions have no teammate
        // pills, and `update` is not free.
        let has_teammate = session.read(cx).streams.keys().any(|id| {
            matches!(id, crate::stream::StreamId::Teammate(_))
        });
        if !has_teammate {
            return;
        }
        let now = std::time::SystemTime::now();
        let stale = std::time::Duration::from_secs(MANAGED_AGENT_STALE_TIMEOUT_SECS);
        let closed_any = session.update(cx, |s, _| {
            // The `Stop` hook is authoritative for normal completion, so this
            // is purely a lost-hook / wedged-subprocess backstop. A live
            // `acp_thread` means the hook SHOULD fire soon, so a live-parent
            // agent gets the same generous `BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS`
            // cap the shell reaper uses before being presumed lost — the hook
            // fires only at end-of-turn, so a long silent tool call must not be
            // mistaken for a dropped hook (hardening #9); with no thread no
            // hook can ever arrive and the tight ordinary timeout applies.
            let parent_alive = s.acp_thread().is_some();
            let live_parent_stale =
                std::time::Duration::from_secs(BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS);
            let teammate_ids: Vec<SharedString> = s
                .streams
                .keys()
                .filter_map(|id| match id {
                    crate::stream::StreamId::Teammate(toolu) => Some(toolu.clone()),
                    _ => None,
                })
                .collect();
            // (parent toolu, close reason)
            let mut to_close: Vec<(SharedString, SharedString)> = Vec::new();
            for toolu in teammate_ids {
                // Async classification FIRST and by the `background_agents` map,
                // not the tool-call tool_name: an async teammate's stream is kept
                // alive by its registration + tagged entries, so it must never
                // fall through to the tool-call rules below (rule 1 would close a
                // live async whose spawn tool-call was rewound).
                let async_agent = s
                    .background_agents
                    .values()
                    .find(|ba| ba.parent_tool_use_id.as_ref() == Some(&toolu));
                if let Some(ba) = async_agent {
                    // A KILLED agent (subprocess replaced by a reconnect) can
                    // never report again even though the session now holds a NEW
                    // live thread, so the lost-hook backstop must not shield
                    // it — it ages out on the tight staleness window like any
                    // orphan, keeping its terminal tab visible only briefly.
                    let stale = if parent_alive && !ba.killed {
                        live_parent_stale
                    } else {
                        stale
                    };
                    let stale_mtime = ba.latest.as_ref().is_some_and(|snap| {
                        now.duration_since(snap.mtime).unwrap_or_default() > stale
                    });
                    // An async agent whose JSONL never produced a parseable
                    // snapshot (`latest == None`) has no mtime to age from, so
                    // the branch above never fires and its `Teammate` pill would
                    // linger forever (the →Idle GC excludes async parents).
                    // Mirror the shell reaper's fallback: age from
                    // `registered_at` and close once older than `stale`.
                    let stale_no_snapshot = ba.latest.is_none() && {
                        let registered: std::time::SystemTime = ba.registered_at.into();
                        now.duration_since(registered).unwrap_or_default() > stale
                    };
                    if stale_mtime || stale_no_snapshot {
                        // Never report a killed agent's stream as "done" — it was
                        // reaped mid-flight with its parent subprocess.
                        let reason = if ba.killed {
                            crate::background_agent::KILLED_REASON
                        } else {
                            ba.latest
                                .as_ref()
                                .and_then(|snap| snap.stop_reason.clone())
                                .unwrap_or_else(|| gpui::SharedString::new_static("done"))
                        };
                        to_close.push((toolu, reason));
                    }
                    // else: fresh async teammate still streaming → keep.
                    continue;
                }
                // Not a registered async agent → an inline `Task` or an orphan.
                let toolcall = s.entries.iter().find_map(|e| match &e.kind {
                    crate::session_entry::SessionEntryKind::ToolCall {
                        id,
                        status,
                        tool_name,
                        ..
                    } if id.as_str() == toolu.as_ref() => {
                        Some((status.clone(), tool_name.clone()))
                    }
                    _ => None,
                });
                match toolcall {
                    None => {
                        // Rule 1: the spawn tool-call entry is gone (rewound /
                        // removed) and this is not a live async agent → orphaned.
                        to_close.push((toolu, gpui::SharedString::new_static("orphaned")));
                    }
                    Some((status, tool_name)) => {
                        if tool_name_is_agent(tool_name.as_deref()) {
                            // Spawn-ack terminal on an async `Agent` whose
                            // background_agent registration hasn't landed yet:
                            // the teammate keeps streaming. Keep it — the async
                            // branch (once registered) or the background-agent GC
                            // owns its real close. Closing here is the pre-6c
                            // premature-close bug.
                        } else if status.is_terminal() {
                            // Rule 2: inline `Task` tool-call terminal → done.
                            to_close.push((toolu, gpui::SharedString::new_static("done")));
                        }
                        // else: live inline `Task` (non-terminal) → keep.
                    }
                }
            }
            if to_close.is_empty() {
                return false;
            }
            for (toolu, reason) in to_close {
                s.close_stream(crate::stream::StreamId::Teammate(toolu), reason);
            }
            true
        });
        if closed_any {
            self.mark_subagents_changed(session_id, cx);
        }
    }

    /// One pass over every session: incrementally tail the PARENT session
    /// JSONL for `<task-notification>` lines and flip matching tracked shells
    /// to their terminal [`ShellRuntimeState`]. Runs on the same 5s tick as
    /// the reap pass, BEFORE it, so a freshly-Exited shell is flipped this
    /// tick (and the reap can later drop it once stale).
    pub fn scan_parent_jsonls_for_completions(&mut self, cx: &mut Context<Self>) {
        let session_ids: Vec<SolutionSessionId> =
            self.all_sessions().map(|e| e.read(cx).id).collect();
        for session_id in session_ids {
            self.scan_parent_jsonl_for_completions(session_id, cx);
        }
    }

    /// Scan a single session's parent JSONL transcript for newly-appended
    /// `<task-notification>` completion lines and flip the matching tracked
    /// shells via [`Self::mark_background_shell_state`].
    ///
    /// Forward-only: the per-session offset is lazily initialised to the
    /// file's CURRENT length on first sight (so historical notifications are
    /// never re-applied) and only advanced past the last COMPLETE newline, so
    /// a half-written trailing line is re-read next tick. No-op when the
    /// session tracks no shells, when none of them are still `Running`, or
    /// when the parent JSONL can't be resolved / doesn't exist.
    pub(crate) fn scan_parent_jsonl_for_completions(
        &mut self,
        session_id: SolutionSessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session(session_id) else {
            self.teammate_watchers.clear_scan_offset(session_id);
            return;
        };
        let (has_shells, any_running, cwd, acp_session_id) = {
            let s = session.read(cx);
            let any_running = s.background_shells.values().any(|sh| {
                matches!(
                    sh.state,
                    crate::background_shell::ShellRuntimeState::Running
                )
            });
            (
                !s.background_shells.is_empty(),
                any_running,
                s.cwd.clone(),
                s.acp_session_id.0.to_string(),
            )
        };
        if !has_shells {
            // Re-arm from the then-current EOF the next time a shell registers.
            self.teammate_watchers.clear_scan_offset(session_id);
            return;
        }
        if !any_running {
            // Everything already terminal — nothing left to flip.
            return;
        }
        let Some(path) = parent_session_jsonl_for(&cwd, &acp_session_id) else {
            return;
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => return,
        };
        let len = metadata.len();
        // Lazy-init: first sight pins the cursor at the current EOF so we only
        // observe completions forward from now.
        let offset = match self.teammate_watchers.scan_offset(session_id) {
            Some(off) => {
                // Truncation / rotation: cursor past EOF → re-read from start.
                if off > len { 0 } else { off }
            }
            None => {
                self.teammate_watchers.set_scan_offset(session_id, len);
                return;
            }
        };
        if len <= offset {
            return;
        }
        let (lines, consumed) = match read_complete_lines_from(&path, offset, len) {
            Some(read) => read,
            None => return,
        };
        // Advance the cursor past the bytes we've fully consumed (the last
        // complete newline), leaving any trailing partial line for next tick.
        self.teammate_watchers
            .set_scan_offset(session_id, offset + consumed);
        if lines.is_empty() {
            return;
        }
        let completions = {
            let s = session.read(cx);
            scan_lines_for_completions(&lines, &s.background_shells)
        };
        for (shell_id, state) in completions {
            self.mark_background_shell_state(session_id, shell_id, state, cx);
        }
    }

    /// 5s healthcheck for background shells, the analog of
    /// [`tick_background_agents`]. Reaps a shell when it is in a terminal
    /// state (`Exited`/`Killed`) OR when it has gone stale beyond
    /// `managed_agent_stale_timeout_secs + managed_agent_dead_linger_secs`.
    ///
    /// The staleness check is load-bearing, not redundant: even though
    /// `scan_parent_jsonls_for_completions` now flips most finished shells to
    /// `Exited` live (via the parent-JSONL `<task-notification>` scan), a shell
    /// whose subprocess dies without emitting a notification (crash, restart,
    /// killed harness) would otherwise leak as a "Running" pill forever. Age is
    /// measured from `latest.mtime` (the output file's last-observed write — it
    /// stops advancing once the command finishes) when a snapshot exists, else
    /// from `registered_at` (a shell that produced zero output and finished must
    /// still age out).
    ///
    /// The staleness threshold for a still-`Running` shell depends on whether its
    /// PARENT agent subprocess is still alive (hardening #9). Output-silence is
    /// NOT death: a long silent build/`sleep` produces no output but is running.
    /// While the parent is alive its completion WILL be marked `Exited` by the
    /// parent-JSONL scan when it finishes, so a silent-Running shell is kept up to
    /// the generous [`BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS`] cap — preserving the
    /// `has_live_background_work` supervisor-suppression instead of dropping it at
    /// ~7min and letting the supervisor act while background work is still live.
    /// Only when the parent subprocess is GONE (no `acp_thread` → no completion
    /// can ever arrive, the documented orphan-leak case) does the ordinary
    /// `STALE + DEAD_LINGER` timeout apply. Terminal (`Exited`/`Killed`) shells
    /// are always reaped immediately, regardless of parent state.
    pub fn tick_background_shells(&mut self, cx: &mut Context<Self>) {
        let expiry = std::time::Duration::from_secs(
            MANAGED_AGENT_STALE_TIMEOUT_SECS + MANAGED_AGENT_DEAD_LINGER_SECS,
        );
        let live_parent_cap =
            std::time::Duration::from_secs(BACKGROUND_SHELL_LIVE_PARENT_MAX_SECS);
        let now = std::time::SystemTime::now();
        let session_ids: Vec<SolutionSessionId> =
            self.all_sessions().map(|e| e.read(cx).id).collect();
        for session_id in session_ids {
            let Some(session) = self.session(session_id) else {
                continue;
            };
            if session.read(cx).background_shells.is_empty() {
                continue;
            }
            let to_remove: Vec<crate::background_shell::BackgroundShellId> =
                session.update(cx, |s, _| {
                    // A live `acp_thread` means the owning agent subprocess is
                    // still up, so a completing shell's `<task-notification>` will
                    // still reach the parent-JSONL scan; a silent-Running shell is
                    // presumed alive and only aged out at the generous cap. No
                    // thread (reconnect / crash / close) → the shell is orphaned
                    // and can never be flipped `Exited`, so the ordinary staleness
                    // timeout applies.
                    let running_stale_threshold = if s.acp_thread().is_some() {
                        live_parent_cap
                    } else {
                        expiry
                    };
                    let candidates: Vec<crate::background_shell::BackgroundShellId> = s
                        .background_shell_order
                        .iter()
                        .filter(|id| {
                            let Some(shell) = s.background_shells.get(id) else {
                                return false;
                            };
                            if matches!(
                                shell.state,
                                crate::background_shell::ShellRuntimeState::Exited(_)
                                    | crate::background_shell::ShellRuntimeState::Killed
                            ) {
                                return true;
                            }
                            // Age from the output file's last-observed mtime when a
                            // snapshot exists, else from registration time.
                            let age = match shell.latest.as_ref() {
                                Some(snap) => now.duration_since(snap.mtime).unwrap_or_default(),
                                None => {
                                    let registered: std::time::SystemTime =
                                        shell.registered_at.into();
                                    now.duration_since(registered).unwrap_or_default()
                                }
                            };
                            age > running_stale_threshold
                        })
                        .cloned()
                        .collect();
                    for id in &candidates {
                        s.background_shells.remove(id);
                        s.background_shell_order.retain(|x| x != id);
                    }
                    // Phase 6d-A: reaping a still-`Running`-but-stale shell drops
                    // its derived stream; rebuild so the mirror matches the map.
                    if !candidates.is_empty() {
                        s.rebuild_streams();
                    }
                    candidates
                });
            if !to_remove.is_empty() {
                cx.emit(SolutionAgentStoreEvent::SessionBackgroundShellsChanged(
                    session_id,
                ));
                if let Some(db) = self.persistence.clone() {
                    let session_id_string = session_id.to_string();
                    for shell_id in to_remove {
                        let db = db.clone();
                        let session_id_string = session_id_string.clone();
                        let shell_id_string = shell_id.to_string();
                        cx.background_spawn(async move {
                            db.delete_background_shell(session_id_string, shell_id_string)
                                .await
                                .log_err();
                        })
                        .detach();
                    }
                }
            }
        }
    }

    /// Purge stale persisted `background_agents` rows on cold-load — this is
    /// NOT a restore pass.
    ///
    /// Async `Agent` subagents do not survive an editor restart: the `claude`
    /// session restarts and the subagents are gone (they stop writing their
    /// JSONL). So every persisted `BackgroundAgentRow` is stale by the time we
    /// cold-hydrate. Re-registering them (the old behavior) is exactly what made
    /// finished/dead teammate pills reappear in the console after a restart, and
    /// they were never reaped. We therefore register NONE and drop ALL rows.
    ///
    /// Their teammate streams stay collapsed: the cold-load path
    /// (`hydrate_streams_main_only`) already folds every tagged teammate stream
    /// into `hydration_orphan_streams` (rendered Main-only, no pill). With
    /// nothing re-registered here, there is no JSONL watcher, so no new tagged
    /// entries ever arrive and the orphan is never reopened — it stays collapsed.
    ///
    /// Always called inside the foreground hydrate path with the DB rows already
    /// loaded (caller pre-fetches off the foreground thread).
    pub(crate) fn reconcile_background_agents_for(
        &mut self,
        _session_id: SolutionSessionId,
        rows: Vec<crate::db::BackgroundAgentRow>,
        cx: &mut Context<Self>,
    ) {
        if rows.is_empty() {
            return;
        }
        // Treat every persisted row as dead (see doc comment): the subagents did
        // not survive the restart, so we drop all rows and register none.
        let to_drop_from_db: Vec<(String, String)> = rows
            .into_iter()
            .map(|row| (row.solution_session_id, row.agent_id))
            .collect();

        if !to_drop_from_db.is_empty()
            && let Some(db) = self.persistence.clone()
        {
            cx.background_spawn(async move {
                for (sid, aid) in to_drop_from_db {
                    db.delete_background_agent(sid, aid).await.log_err();
                }
            })
            .detach();
        }
    }
}
