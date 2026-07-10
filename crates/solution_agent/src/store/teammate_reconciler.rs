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
            let mut close_teammate: Option<(SharedString, SharedString)> = None; // (parent toolu, reason)
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
                    // growing, so this fires once).
                    let was_terminal = ba
                        .latest
                        .as_ref()
                        .is_some_and(|s| s.stop_reason.is_some());
                    let now_terminal = snap.stop_reason.is_some();
                    let parent = ba.parent_tool_use_id.clone();
                    let reason = snap.stop_reason.clone();
                    ba.latest = Some(snap);
                    changed = true;
                    if now_terminal && !was_terminal {
                        s.last_activity_at = Utc::now();
                        if let Some(parent_toolu) = parent {
                            close_teammate = Some((
                                parent_toolu,
                                reason.unwrap_or_else(|| gpui::SharedString::new_static("done")),
                            ));
                        }
                    }
                }
            }
            // Auto-close the async `Agent` teammate's demux stream on its REAL
            // terminal signal (deferred from phase 3, where the spawn tool-call
            // terminal is only spawn-ack). Done after the `ba` borrow ends so
            // `close_stream` can take `&mut s`.
            if let Some((parent_toolu, reason)) = close_teammate {
                s.close_stream(crate::stream::StreamId::Teammate(parent_toolu), reason);
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
}
