//! Tracking surface for Claude Code's **background shells** — Bash commands
//! launched with `run_in_background=true` from the parent claude process.
//! Unlike inline Bash calls whose output is returned inline in the tool
//! result, a background shell runs detached and writes its combined
//! stdout/stderr to an on-disk `.output` file whose path is surfaced in the
//! launch announcement printed to the parent's conversation transcript.
//!
//! This module owns:
//!
//! - [`BackgroundShellId`] — newtype around the short random token Claude Code
//!   assigns to each background task (e.g. `bvb4ful1z`).
//! - [`BackgroundShell`] + [`BackgroundShellSnapshot`] — in-memory tracking
//!   state per shell.
//! - [`ShellRuntimeState`] — running / exited / killed lifecycle enum.
//! - [`parse_task_notification`] — parser for `<task-notification>` completion blocks.
//! - [`parse_kill_shell_input`] — extractor for `KillShell` tool_call inputs.
//! - [`tail_output`] — incremental tail helper for plain-text `.output` files.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use regex::Regex;
use serde_json::Value;

use chrono::{DateTime, Utc};
use gpui::SharedString;

use crate::session_entry::{AssistantChunk, SessionEntry, SessionEntryKind};

/// Opaque identifier assigned by Claude Code to a background shell task.
/// Short random token (e.g. `bvb4ful1z`), not a hex digest.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackgroundShellId(SharedString);

impl BackgroundShellId {
    pub fn new(id: impl Into<SharedString>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }

    /// First 9 chars — shell ids are short random tokens (e.g. `bvb4ful1z`),
    /// so this usually returns the whole id; the cap guards a pathological id.
    pub fn short(&self) -> String {
        self.0.chars().take(9).collect()
    }
}

impl std::fmt::Display for BackgroundShellId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_ref())
    }
}

/// Runtime lifecycle of a background shell process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellRuntimeState {
    Running,
    /// Process exited; inner value is the exit code when known.
    Exited(Option<i32>),
    Killed,
}

impl ShellRuntimeState {
    /// Serialize to the `state_text` column convention used by the
    /// `solution_session_background_shell` table: `"running"`,
    /// `"exited:N"` / `"exited"` (when the code is unknown), or `"killed"`.
    pub fn to_state_text(&self) -> String {
        match self {
            ShellRuntimeState::Running => "running".to_string(),
            ShellRuntimeState::Exited(Some(code)) => format!("exited:{code}"),
            ShellRuntimeState::Exited(None) => "exited".to_string(),
            ShellRuntimeState::Killed => "killed".to_string(),
        }
    }
}

/// In-memory tracking record for one background shell.
#[derive(Clone, Debug)]
pub struct BackgroundShell {
    pub id: BackgroundShellId,
    /// Command line captured at launch (truncated to `COMMAND_CAP` (4096) chars at the call-site).
    pub command: SharedString,
    /// The `/tmp/claude-<uid>/.../tasks/<id>.output` path from the launch announcement.
    pub output_path: PathBuf,
    pub registered_at: DateTime<Utc>,
    pub latest: Option<BackgroundShellSnapshot>,
    /// File length (`new_offset`) recorded by the last successful tail of
    /// `output_path`. `refresh_background_shell_snapshot` re-reads the full
    /// trailing window each time (it passes `0` to `tail_output` for a live
    /// tail, not an incremental one), so this is used purely for change
    /// detection — the refresh only emits a change event when the new length
    /// differs from this stored value.
    pub last_offset: u64,
    pub state: ShellRuntimeState,
}

/// A point-in-time snapshot of a background shell's output file.
#[derive(Clone, Debug)]
pub struct BackgroundShellSnapshot {
    pub mtime: SystemTime,
    /// Trailing chunk of the shell's stdout/stderr, capped (later task).
    pub output_tail: SharedString,
}

impl BackgroundShell {
    /// The strip/tab label for this shell's derived `StreamId::Shell` stream:
    /// `<short-id>·<command>`, command truncated to ~24 chars (the strip is
    /// narrow). Moved here from the desktop strip (phase 6d-A) so
    /// `SolutionSession::rebuild_streams` — which is `cx`-free — can stamp
    /// `Stream::label` from the same logic the old pill used.
    pub fn stream_label(&self) -> SharedString {
        const CMD_CAP: usize = 24;
        let cmd: String = if self.command.chars().count() > CMD_CAP {
            let truncated: String = self.command.chars().take(CMD_CAP).collect();
            format!("{truncated}…")
        } else {
            self.command.to_string()
        };
        SharedString::from(format!("{}·{}", self.id.short(), cmd))
    }

    /// Convert this shell's last-observed snapshot into the single
    /// fenced-output [`SessionEntry`] that is the body of its derived
    /// `StreamId::Shell` stream (phase 6d-A). Mirrors the content the retired
    /// `session_view::build_shell_drill_in_entries` produced, but as plain data
    /// (no `Markdown` entity), so it is `cx`-free and can run inside
    /// `SolutionSession::rebuild_streams`.
    ///
    /// `created_ms` and `mod_seq` both derive from the snapshot mtime (unix-ms;
    /// `0` when there is no snapshot yet). mtime advances every time the shell
    /// writes output, so a per-stream `seq` keyed on the entry `mod_seq` bumps
    /// when the tail changes — that is the delta cursor the 6d-B wire will read.
    /// `now` feeds only the human "observed X ago" header line and is a
    /// parameter (not `Utc::now()` inline) so tests stay deterministic.
    pub fn stream_entry(&self, now: DateTime<Utc>) -> SessionEntry {
        let state_label = match (&self.state, self.latest.is_none()) {
            // A shell still "running" but with no fresh snapshot is flagged
            // stale so the body matches the old strip pill's wording.
            (ShellRuntimeState::Running, true) => "running (stale)".to_string(),
            (ShellRuntimeState::Running, false) => "running".to_string(),
            (ShellRuntimeState::Exited(Some(code)), _) => format!("exited ({code})"),
            (ShellRuntimeState::Exited(None), _) => "exited".to_string(),
            (ShellRuntimeState::Killed, _) => "killed".to_string(),
        };
        let observed = match &self.latest {
            Some(snapshot) => shell_relative_time(snapshot.mtime, now),
            None => "no output yet".to_string(),
        };
        let header = format!(
            "`{}` · {} · {} · {}",
            self.command,
            state_label,
            observed,
            self.id.short()
        );
        let body = match &self.latest {
            Some(snapshot) => format!("```\n{}\n```", snapshot.output_tail),
            None => "_No output captured yet._".to_string(),
        };
        let text = format!("{header}\n\n{body}");
        let ms = self
            .latest
            .as_ref()
            .and_then(|snap| snap.mtime.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|dur| dur.as_millis() as u64);
        SessionEntry {
            created_ms: ms.map(|m| m as i64).unwrap_or(0),
            mod_seq: ms.unwrap_or(0),
            subagent_id: None,
            kind: SessionEntryKind::AssistantMessage {
                chunks: vec![AssistantChunk::Message(text)],
            },
        }
    }
}

/// Cap on the command string captured for a background shell. Generous: the
/// pill label truncates to 24 chars for the strip and the tab content renders
/// the command as its own fenced block, so display never depends on this being
/// short — it only guards against a pathological multi-megabyte
/// `raw_input.command`.
pub const COMMAND_CAP: usize = 4096;

/// Extract a background shell's launch command from a Bash tool call's
/// `raw_input`: prefer `command`, fall back to `description`. Capped at
/// [`COMMAND_CAP`] chars (ellipsis suffix on overflow). Empty `SharedString`
/// when neither key holds a non-empty string.
pub fn command_label_from_raw_input(raw_input: &serde_json::Value) -> SharedString {
    let picked = raw_input
        .get("command")
        .or_else(|| raw_input.get("description"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    if picked.chars().count() > COMMAND_CAP {
        let truncated: String = picked.chars().take(COMMAND_CAP).collect();
        SharedString::from(format!("{truncated}…"))
    } else {
        SharedString::from(picked)
    }
}

/// "X ago" formatter for a shell snapshot's `SystemTime` mtime. Converts to a
/// UTC `DateTime` and formats relative to `now`; an mtime before the epoch
/// (clock skew) or in the future degrades to `"just now"`. Lives here (moved
/// from `session_view` with the shell drill-in in phase 6d-A) so the `cx`-free
/// `stream_entry` normalizer can reuse it.
fn shell_relative_time(mtime: SystemTime, now: DateTime<Utc>) -> String {
    let secs = match mtime.duration_since(std::time::UNIX_EPOCH) {
        Ok(dur) => now.timestamp().saturating_sub(dur.as_secs() as i64).max(0),
        Err(_) => return "just now".to_string(),
    };
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

static SHELL_ID_RE: OnceLock<Regex> = OnceLock::new();
static SHELL_OUTPUT_PATH_RE: OnceLock<Regex> = OnceLock::new();

fn shell_id_re() -> &'static Regex {
    SHELL_ID_RE.get_or_init(|| Regex::new(r"\bID:\s+(\w+)\b").expect("static regex compiles"))
}

fn shell_output_path_re() -> &'static Regex {
    SHELL_OUTPUT_PATH_RE.get_or_init(|| {
        Regex::new(r"written to:\s+(\S+\.output)\b").expect("static regex compiles")
    })
}

/// Best-effort parse of a `Bash(run_in_background=true)` launch announcement.
/// Returns `Some((shell_id, output_path))` when both the `ID:` token and the
/// `written to: <…>.output` path are present; `None` otherwise (caller silently
/// skips registration so a reshaped future announcement doesn't spam the log).
///
/// The caller feeds this the tool call's CONTENT, not its `raw_output`:
/// `claude_native::translate` only ever sets `raw_input`, so `raw_output` is
/// always `None` and the announcement only ever lands in the tool_result body.
pub fn parse_bash_bg_launch(announcement: &str) -> Option<(BackgroundShellId, PathBuf)> {
    let shell_id = shell_id_re().captures(announcement)?.get(1)?.as_str();
    let output_path = shell_output_path_re()
        .captures(announcement)?
        .get(1)?
        .as_str();
    Some((BackgroundShellId::new(shell_id), PathBuf::from(output_path)))
}

// ---------------------------------------------------------------------------
// Task 3 — `parse_task_notification`
// ---------------------------------------------------------------------------

/// Parsed content of a `<task-notification>` block emitted by Claude Code in a
/// `user`-role message when a background shell completes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskNotification {
    pub id: BackgroundShellId,
    pub status: ShellRuntimeState,
}

static TASK_ID_RE: OnceLock<Regex> = OnceLock::new();
static EXIT_CODE_RE: OnceLock<Regex> = OnceLock::new();

fn task_id_re() -> &'static Regex {
    TASK_ID_RE.get_or_init(|| {
        Regex::new(r"<task-id>\s*([^<\s]+)\s*</task-id>").expect("static regex compiles")
    })
}

fn exit_code_re() -> &'static Regex {
    EXIT_CODE_RE
        .get_or_init(|| Regex::new(r"\(exit code (-?\d+)\)").expect("static regex compiles"))
}

/// Parse a `<task-notification>` block. Returns `None` if the block or its
/// `<task-id>` is absent. `<status>completed</status>` maps to
/// `ShellRuntimeState::Exited(Some(N))` where N is parsed from the
/// `(exit code N)` suffix in `<summary>` (defaulting to `Exited(None)` when the
/// code is absent/unparseable). Any non-"completed" status also maps defensively
/// to `Exited(None)` (we don't yet know other status spellings).
pub fn parse_task_notification(text: &str) -> Option<TaskNotification> {
    if !text.contains("<task-notification>") {
        return None;
    }
    let id_str = task_id_re().captures(text)?.get(1)?.as_str();
    let id = BackgroundShellId::new(id_str);
    let exit_code = exit_code_re()
        .captures(text)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse::<i32>().ok());
    let status = ShellRuntimeState::Exited(exit_code);
    Some(TaskNotification { id, status })
}

// ---------------------------------------------------------------------------
// Task 4 — `parse_kill_shell_input`
// ---------------------------------------------------------------------------

/// Extract the target shell id from a `KillShell` tool_call's `raw_input`.
/// Accepts either `shell_id` or `bash_id` (string). Returns `None` if neither
/// is a non-empty string.
pub fn parse_kill_shell_input(raw_input: &Value) -> Option<BackgroundShellId> {
    let id_str = raw_input
        .get("shell_id")
        .or_else(|| raw_input.get("bash_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some(BackgroundShellId::new(id_str))
}

// ---------------------------------------------------------------------------
// Task 5 — `tail_output`
// ---------------------------------------------------------------------------

/// 64 KiB cap on the trailing chunk returned by [`tail_output`].
const OUTPUT_TAIL_CAP: usize = 64 * 1024;

/// Result of a [`tail_output`] call.
#[derive(Debug, Clone)]
pub struct OutputTail {
    /// Trailing chunk of the file's bytes as UTF-8 (lossy), capped at `OUTPUT_TAIL_CAP`.
    pub text: String,
    /// Offset just past EOF after the read; pass back as `since_offset` next call.
    pub new_offset: u64,
    pub mtime: SystemTime,
}

/// Tail a plain-text background-shell `.output` file. Unlike `tail_jsonl` (last
/// line only), this returns the trailing window of the file content for display.
/// Reads from `max(since_offset, len - OUTPUT_TAIL_CAP)` to EOF. Resets
/// `since_offset` to 0 when it exceeds `len` (truncation/rotation). A missing file
/// propagates as `Err` (`std::io::ErrorKind::NotFound`) — the caller treats that as
/// "no snapshot yet", NOT a hard failure.
pub fn tail_output(path: &Path, since_offset: u64) -> std::io::Result<OutputTail> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let mtime = metadata.modified()?;
    let len = metadata.len();
    // Truncation / rotation: stored offset points past EOF — re-read from start.
    let since_offset = if since_offset > len { 0 } else { since_offset };
    // Start reading from the further of since_offset and (len - cap).
    let read_start = std::cmp::max(since_offset, len.saturating_sub(OUTPUT_TAIL_CAP as u64));
    file.seek(SeekFrom::Start(read_start))?;
    let to_read = len - read_start;
    let mut buf = Vec::with_capacity(to_read as usize);
    file.take(to_read).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(OutputTail {
        text,
        new_offset: len,
        mtime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn background_shell_id_short_returns_whole_id_when_short() {
        let id = BackgroundShellId::new("bvb4ful1z");
        assert_eq!(id.short(), "bvb4ful1z");
    }

    #[test]
    fn background_shell_id_short_caps_at_nine_chars() {
        let id = BackgroundShellId::new("abcdefghij_toolong");
        assert_eq!(id.short(), "abcdefghi");
    }

    #[test]
    fn background_shell_id_short_handles_id_shorter_than_nine() {
        let id = BackgroundShellId::new("abc");
        assert_eq!(id.short(), "abc");
    }

    #[test]
    fn background_shell_clone_round_trip_ids_equal() {
        let shell = BackgroundShell {
            id: BackgroundShellId::new("bvb4ful1z"),
            command: SharedString::from("cargo build --bin sawe"),
            output_path: PathBuf::from("/tmp/claude-1000/tasks/bvb4ful1z.output"),
            registered_at: chrono::Utc::now(),
            latest: None,
            last_offset: 0,
            state: ShellRuntimeState::Running,
        };
        let cloned = shell.clone();
        assert_eq!(shell.id, cloned.id);
    }

    const REAL_ANNOUNCEMENT: &str = "Command running in background with ID: bvb4ful1z. Output is being written to: /tmp/claude-1000/-home-spk--spk-sawe-dev-solutions-alphasol/6e524c79-5089-4a74-9419-bd18e9119e0b/tasks/bvb4ful1z.output. You will be notified when it completes. To check interim output, use Read on that file path.";

    #[test]
    fn parse_bash_bg_launch_happy_path() {
        let result = parse_bash_bg_launch(REAL_ANNOUNCEMENT).unwrap();
        assert_eq!(result.0.as_str(), "bvb4ful1z");
        assert_eq!(
            result.1,
            PathBuf::from(
                "/tmp/claude-1000/-home-spk--spk-sawe-dev-solutions-alphasol/6e524c79-5089-4a74-9419-bd18e9119e0b/tasks/bvb4ful1z.output"
            )
        );
    }

    #[test]
    fn parse_bash_bg_launch_trailing_dot_not_captured() {
        // The sentence has ". You will be notified..." after .output — the dot must NOT be in the path.
        let result = parse_bash_bg_launch(REAL_ANNOUNCEMENT).unwrap();
        assert!(!result.1.to_str().unwrap().ends_with('.'));
    }

    #[test]
    fn parse_bash_bg_launch_missing_id_returns_none() {
        let text = "Output is being written to: /tmp/claude-1000/tasks/abc123.output. You will be notified when it completes.";
        assert!(parse_bash_bg_launch(text).is_none());
    }

    #[test]
    fn parse_bash_bg_launch_missing_path_returns_none() {
        let text = "Command running in background with ID: foo123. No output path here.";
        assert!(parse_bash_bg_launch(text).is_none());
    }

    #[test]
    fn parse_bash_bg_launch_non_output_extension_returns_none() {
        let text = "Command running in background with ID: foo123. Output is being written to: /tmp/x.txt. You will be notified.";
        assert!(parse_bash_bg_launch(text).is_none());
    }

    #[test]
    fn parse_bash_bg_launch_garbage_returns_none() {
        assert!(parse_bash_bg_launch("").is_none());
        assert!(parse_bash_bg_launch("completely unrelated text").is_none());
    }

    #[test]
    fn parse_bash_bg_launch_different_valid_id() {
        let text = "Command running in background with ID: abc123xyz. Output is being written to: /tmp/claude-1000/tasks/abc123xyz.output. You will be notified.";
        let result = parse_bash_bg_launch(text).unwrap();
        assert_eq!(result.0.as_str(), "abc123xyz");
        assert_eq!(
            result.1,
            PathBuf::from("/tmp/claude-1000/tasks/abc123xyz.output")
        );
    }

    // -----------------------------------------------------------------------
    // Task 3 — parse_task_notification
    // -----------------------------------------------------------------------

    const REAL_NOTIFICATION: &str = r#"<task-notification>
<task-id>bvb4ful1z</task-id>
<tool-use-id>toolu_01AqJufkNFAd7Aef3ojZ8d5J</tool-use-id>
<output-file>/tmp/claude-1000/.../tasks/bvb4ful1z.output</output-file>
<status>completed</status>
<summary>Background command "Sleep for 60 seconds in background" completed (exit code 0)</summary>
</task-notification>"#;

    #[test]
    fn parse_task_notification_exit_code_zero() {
        let result = parse_task_notification(REAL_NOTIFICATION).unwrap();
        assert_eq!(result.id.as_str(), "bvb4ful1z");
        assert_eq!(result.status, ShellRuntimeState::Exited(Some(0)));
    }

    #[test]
    fn parse_task_notification_exit_code_137() {
        let text = r#"<task-notification>
<task-id>abc123def</task-id>
<status>completed</status>
<summary>Command failed (exit code 137)</summary>
</task-notification>"#;
        let result = parse_task_notification(text).unwrap();
        assert_eq!(result.id.as_str(), "abc123def");
        assert_eq!(result.status, ShellRuntimeState::Exited(Some(137)));
    }

    #[test]
    fn parse_task_notification_completed_no_exit_code() {
        let text = r#"<task-notification>
<task-id>xyz789</task-id>
<status>completed</status>
<summary>Background command finished</summary>
</task-notification>"#;
        let result = parse_task_notification(text).unwrap();
        assert_eq!(result.id.as_str(), "xyz789");
        assert_eq!(result.status, ShellRuntimeState::Exited(None));
    }

    #[test]
    fn parse_task_notification_no_block_returns_none() {
        let text = "Just some regular text without any task notification block";
        assert!(parse_task_notification(text).is_none());
    }

    #[test]
    fn parse_task_notification_negative_exit_code() {
        let text = r#"<task-notification>
<task-id>neg99x</task-id>
<status>completed</status>
<summary>Killed (exit code -1)</summary>
</task-notification>"#;
        let result = parse_task_notification(text).unwrap();
        assert_eq!(result.status, ShellRuntimeState::Exited(Some(-1)));
    }

    // -----------------------------------------------------------------------
    // Task 4 — parse_kill_shell_input
    // -----------------------------------------------------------------------

    #[test]
    fn shell_runtime_state_to_state_text_round_trip() {
        assert_eq!(ShellRuntimeState::Running.to_state_text(), "running");
        assert_eq!(
            ShellRuntimeState::Exited(Some(0)).to_state_text(),
            "exited:0"
        );
        assert_eq!(
            ShellRuntimeState::Exited(Some(137)).to_state_text(),
            "exited:137"
        );
        assert_eq!(
            ShellRuntimeState::Exited(Some(-1)).to_state_text(),
            "exited:-1"
        );
        assert_eq!(ShellRuntimeState::Exited(None).to_state_text(), "exited");
        assert_eq!(ShellRuntimeState::Killed.to_state_text(), "killed");
    }

    #[test]
    fn parse_kill_shell_input_shell_id_key() {
        let input = serde_json::json!({"shell_id": "bvb4ful1z"});
        let result = parse_kill_shell_input(&input).unwrap();
        assert_eq!(result.as_str(), "bvb4ful1z");
    }

    #[test]
    fn parse_kill_shell_input_bash_id_key() {
        let input = serde_json::json!({"bash_id": "abc"});
        let result = parse_kill_shell_input(&input).unwrap();
        assert_eq!(result.as_str(), "abc");
    }

    #[test]
    fn parse_kill_shell_input_empty_object_returns_none() {
        let input = serde_json::json!({});
        assert!(parse_kill_shell_input(&input).is_none());
    }

    #[test]
    fn parse_kill_shell_input_empty_string_returns_none() {
        let input = serde_json::json!({"shell_id": ""});
        assert!(parse_kill_shell_input(&input).is_none());
    }

    // -----------------------------------------------------------------------
    // Task 5 — tail_output
    // -----------------------------------------------------------------------

    #[test]
    fn tail_output_short_file_fully_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.output");
        std::fs::write(&path, b"hello world\n").unwrap();
        let result = tail_output(&path, 0).unwrap();
        assert_eq!(result.text, "hello world\n");
        assert_eq!(result.new_offset, 12);
    }

    #[test]
    fn tail_output_large_file_returns_only_trailing_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.output");
        // Write OUTPUT_TAIL_CAP + 1000 bytes: prefix 'A's then trailing 'B's.
        let prefix_size = 1000usize;
        let cap = OUTPUT_TAIL_CAP;
        let mut content = vec![b'A'; prefix_size];
        content.extend(vec![b'B'; cap]);
        std::fs::write(&path, &content).unwrap();
        let result = tail_output(&path, 0).unwrap();
        // Only trailing cap bytes should be returned.
        assert_eq!(result.text.len(), cap);
        assert!(result.text.chars().all(|c| c == 'B'));
        assert_eq!(result.new_offset, (prefix_size + cap) as u64);
    }

    #[test]
    fn tail_output_incremental_read_only_new_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("incremental.output");
        std::fs::write(&path, b"first line\n").unwrap();
        let first = tail_output(&path, 0).unwrap();
        assert_eq!(first.text, "first line\n");
        // Append more content.
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"second line\n").unwrap();
        drop(file);
        let second = tail_output(&path, first.new_offset).unwrap();
        assert_eq!(second.text, "second line\n");
    }

    #[test]
    fn tail_output_truncated_file_resets_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotated.output");
        std::fs::write(&path, b"original content\n").unwrap();
        let first = tail_output(&path, 0).unwrap();
        // Simulate truncation: write new shorter content, pass old offset > new len.
        std::fs::write(&path, b"new\n").unwrap();
        let result = tail_output(&path, first.new_offset).unwrap();
        // Offset was reset to 0 → reads full new content.
        assert_eq!(result.text, "new\n");
        assert_eq!(result.new_offset, 4);
    }

    #[test]
    fn tail_output_missing_file_returns_not_found() {
        let result = tail_output(std::path::Path::new("/nonexistent/path/foo.output"), 0);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    // -----------------------------------------------------------------------
    // Phase 6d-A — stream_label / stream_entry (the derived Shell stream)
    // -----------------------------------------------------------------------

    fn running_shell(command: &str, tail: Option<&str>) -> BackgroundShell {
        BackgroundShell {
            id: BackgroundShellId::new("bvb4ful1z"),
            command: SharedString::from(command.to_string()),
            output_path: PathBuf::from("/tmp/claude/tasks/bvb4ful1z.output"),
            registered_at: chrono::Utc::now(),
            latest: tail.map(|t| BackgroundShellSnapshot {
                mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_720_000_000),
                output_tail: SharedString::from(t.to_string()),
            }),
            last_offset: 0,
            state: ShellRuntimeState::Running,
        }
    }

    #[test]
    fn stream_label_truncates_long_command() {
        let shell = running_shell("cargo build --bin sawe --profile release-fast", None);
        let label = shell.stream_label();
        assert!(label.starts_with("bvb4ful1z·"));
        assert!(label.ends_with('…'));
    }

    #[test]
    fn stream_label_keeps_short_command() {
        let mut shell = running_shell("ls -la", None);
        shell.id = BackgroundShellId::new("abc");
        assert_eq!(shell.stream_label().as_ref(), "abc·ls -la");
    }

    #[test]
    fn command_label_prefers_command_then_description() {
        let v = serde_json::json!({"command": "ls -la", "description": "listing"});
        assert_eq!(command_label_from_raw_input(&v).as_ref(), "ls -la");
        let v = serde_json::json!({"description": "listing"});
        assert_eq!(command_label_from_raw_input(&v).as_ref(), "listing");
        let v = serde_json::json!({});
        assert_eq!(command_label_from_raw_input(&v).as_ref(), "");
    }

    #[test]
    fn command_label_keeps_commands_longer_than_120_chars() {
        // Old behaviour truncated at 120; a 205-char command must now survive whole.
        let long = format!("echo {}", "x".repeat(200));
        let v = serde_json::json!({ "command": long });
        let out = command_label_from_raw_input(&v);
        assert_eq!(out.chars().count(), 205);
        assert!(!out.ends_with('…'));
    }

    #[test]
    fn command_label_caps_pathological_command() {
        let huge = "a".repeat(COMMAND_CAP + 500);
        let v = serde_json::json!({ "command": huge });
        let out = command_label_from_raw_input(&v);
        assert_eq!(out.chars().count(), COMMAND_CAP + 1); // COMMAND_CAP chars + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn stream_entry_with_snapshot_fences_output_and_derives_seq_from_mtime() {
        let shell = running_shell("echo hi", Some("hello\nworld\n"));
        let entry = shell.stream_entry(chrono::Utc::now());
        assert!(entry.subagent_id.is_none());
        // mtime = 1_720_000_000 s → unix-ms; both created_ms and mod_seq derive
        // from it so a per-stream seq advances when the tail (and mtime) change.
        assert_eq!(entry.mod_seq, 1_720_000_000_000);
        assert_eq!(entry.created_ms, 1_720_000_000_000);
        let SessionEntryKind::AssistantMessage { chunks } = &entry.kind else {
            panic!("expected AssistantMessage");
        };
        let AssistantChunk::Message(text) = &chunks[0] else {
            panic!("expected a plain Message chunk");
        };
        assert!(text.contains("```\nhello\nworld\n\n```"));
        assert!(text.contains("`echo hi`"));
    }

    #[test]
    fn stream_entry_exited_state_label_carries_exit_code() {
        // A shell whose stream is derived after a terminal flip (rare — terminal
        // shells are usually skipped by `rebuild_streams`) still labels its exit
        // code, preserving the old drill-in wording.
        let mut shell = running_shell("make", Some("done\n"));
        shell.state = ShellRuntimeState::Exited(Some(137));
        let entry = shell.stream_entry(chrono::Utc::now());
        let SessionEntryKind::AssistantMessage { chunks } = &entry.kind else {
            panic!("expected AssistantMessage");
        };
        let AssistantChunk::Message(text) = &chunks[0] else {
            panic!("expected a plain Message chunk");
        };
        assert!(text.contains("exited (137)"), "state label: {text}");
    }

    #[test]
    fn stream_entry_without_snapshot_is_no_output_and_zero_seq() {
        let shell = running_shell("sleep 60", None);
        let entry = shell.stream_entry(chrono::Utc::now());
        assert_eq!(entry.mod_seq, 0);
        assert_eq!(entry.created_ms, 0);
        let SessionEntryKind::AssistantMessage { chunks } = &entry.kind else {
            panic!("expected AssistantMessage");
        };
        let AssistantChunk::Message(text) = &chunks[0] else {
            panic!("expected a plain Message chunk");
        };
        assert!(text.contains("_No output captured yet._"));
        assert!(text.contains("running (stale)"));
    }
}
