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
    /// Command line captured at launch (truncated to ~120 chars at the call-site).
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

/// Best-effort parse of a `Bash(run_in_background=true)` tool_call's `raw_output`.
/// Returns `Some((shell_id, output_path))` when both the `ID:` token and the
/// `written to: <…>.output` path are present; `None` otherwise (caller silently
/// skips registration so a reshaped future announcement doesn't spam the log).
pub fn parse_bash_bg_launch(raw_output: &str) -> Option<(BackgroundShellId, PathBuf)> {
    let shell_id = shell_id_re().captures(raw_output)?.get(1)?.as_str();
    let output_path = shell_output_path_re()
        .captures(raw_output)?
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
}
