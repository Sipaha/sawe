//! Reads `claude`'s own on-disk session transcript
//! (`~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`) for facts its
//! stream-json output leaves out.
//!
//! The case this exists for: a turn that `claude` starts by itself — a
//! `ScheduleWakeup` / `CronCreate` firing, or a message from another Claude
//! session — reaches stdout only as `command_lifecycle {state:"started"}`. The
//! prompt that started the turn is not replayed, even under
//! `--replay-user-messages`. Only the transcript records it, as the user entry
//! whose `uuid` equals the lifecycle's `command_uuid`, tagged with a
//! `turnOrigin`. Without it the conversation shows the agent answering a prompt
//! nobody can see. Observed 2026-09-30: an agent's forgotten 30-minute wakeup
//! re-ran a finished code review, and the agent told the user they had
//! repeated the command.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// `~/.claude/projects/<encoded-cwd>/`: the per-project root `claude` writes
/// session transcripts and subagent dirs under. `None` when `cwd` is empty or
/// the home directory can't be resolved.
pub fn claude_project_dir_for(cwd: &Path) -> Option<PathBuf> {
    if cwd.as_os_str().is_empty() {
        return None;
    }
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '.' => '-',
            other => other,
        })
        .collect();
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("projects")
            .join(encoded),
    )
}

/// The session's transcript file. `cwd` is the directory `claude` was spawned
/// in, not wherever the agent has since `cd`-ed to.
pub fn session_transcript_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    Some(claude_project_dir_for(cwd)?.join(format!("{session_id}.jsonl")))
}

/// How far back from EOF [`find_entry`] looks. The entry is written just
/// before the `command_lifecycle` that names it, so it sits at the very end;
/// the window only has to clear a few large tool results written since.
const TAIL_WINDOW_BYTES: u64 = 1024 * 1024;

/// The transcript entry whose `uuid` is `uuid`, searched in the last
/// [`TAIL_WINDOW_BYTES`] of the file, newest first.
pub fn find_entry(path: &Path, uuid: &str) -> Option<Value> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buffer).ok()?;
    find_entry_in(&String::from_utf8_lossy(&buffer), uuid)
}

fn find_entry_in(text: &str, uuid: &str) -> Option<Value> {
    text.lines()
        .rev()
        // Cheap substring pre-filter before parsing. A line cut in half by the
        // window start can't parse and is skipped.
        .filter(|line| line.contains(uuid))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry.get("uuid").and_then(Value::as_str) == Some(uuid))
}

/// Longest prompt excerpt quoted into the note. A peer message can be pages
/// long; the agent's reply carries the substance anyway.
const MAX_QUOTED_CHARS: usize = 600;

/// The note to show the user for a turn `claude` started on its own, or `None`
/// when `entry` is an ordinary prompt. Task notifications are left out on
/// purpose: they already show as the background task's tool call finishing,
/// and a note for each would drown the conversation.
pub fn self_started_turn_note(entry: &Value) -> Option<String> {
    let heading = match entry.get("turnOrigin").and_then(Value::as_str)? {
        "scheduled" => {
            "Scheduled wakeup fired. The agent queued this prompt for itself \
             earlier (ScheduleWakeup / CronCreate). It was not sent by you:"
        }
        "peer" => "Message from another Claude session. It was not sent by you:",
        _ => return None,
    };
    let prompt = prompt_text(entry.get("message")?.get("content")?);
    let prompt = prompt.trim();
    let quoted: String = prompt.chars().take(MAX_QUOTED_CHARS).collect();
    let ellipsis = if quoted.len() < prompt.len() {
        "…"
    } else {
        ""
    };
    Some(format!("{heading}\n\n{quoted}{ellipsis}"))
}

fn prompt_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim shape of the entry claude 2.1.282 wrote when a
    // `ScheduleWakeup` fired (trimmed of unrelated fields).
    const SCHEDULED_ENTRY: &str = r#"{"parentUuid":"c1d5","isSidechain":false,"type":"user","message":{"role":"user","content":"WAKEUP-PROBE ping"},"isMeta":true,"uuid":"1182c7b1-b4fc-4cd8-9486-7437b6c864ad","promptSource":"system","turnOrigin":"scheduled","sessionId":"9ae4"}"#;

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).expect("fixture is valid JSON")
    }

    #[test]
    fn project_dir_encodes_slashes_and_dots() {
        let dir = claude_project_dir_for(Path::new("/home/spk/.spk/sawe/ss/lena-review"))
            .expect("home dir resolves");
        assert!(
            dir.ends_with(".claude/projects/-home-spk--spk-sawe-ss-lena-review"),
            "got {dir:?}"
        );
        assert!(claude_project_dir_for(Path::new("")).is_none());
    }

    #[test]
    fn scheduled_turn_gets_a_note_quoting_its_prompt() {
        let note = self_started_turn_note(&parse(SCHEDULED_ENTRY)).expect("scheduled → note");
        assert!(note.starts_with("Scheduled wakeup fired."), "{note}");
        assert!(note.ends_with("\n\nWAKEUP-PROBE ping"), "{note}");
    }

    #[test]
    fn peer_message_gets_a_note_from_text_blocks() {
        let entry = parse(
            r#"{"type":"user","turnOrigin":"peer","message":{"role":"user","content":[{"type":"text","text":"Another Claude session sent a message: hi"}]}}"#,
        );
        let note = self_started_turn_note(&entry).expect("peer → note");
        assert!(note.starts_with("Message from another Claude session."));
        assert!(note.ends_with("Another Claude session sent a message: hi"));
    }

    #[test]
    fn user_sdk_and_task_notification_turns_get_no_note() {
        for origin in [
            r#""turnOrigin":"sdk","#,
            r#""turnOrigin":"task_notification","#,
            "",
        ] {
            let entry = parse(&format!(
                r#"{{"type":"user",{origin}"message":{{"role":"user","content":"x"}}}}"#
            ));
            assert!(
                self_started_turn_note(&entry).is_none(),
                "origin {origin:?}"
            );
        }
    }

    #[test]
    fn long_prompt_is_truncated_on_a_char_boundary() {
        let long = "ж".repeat(MAX_QUOTED_CHARS + 10);
        let entry = serde_json::json!({
            "turnOrigin": "scheduled",
            "message": {"role": "user", "content": long},
        });
        let note = self_started_turn_note(&entry).expect("note");
        let quoted = note.rsplit("\n\n").next().expect("quoted part");
        assert_eq!(quoted.chars().count(), MAX_QUOTED_CHARS + 1);
        assert!(quoted.ends_with('…'));
    }

    #[test]
    fn find_entry_matches_uuid_field_not_substring() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s.jsonl");
        let uuid = "1182c7b1-b4fc-4cd8-9486-7437b6c864ad";
        // A later line mentioning the uuid in its body must not shadow the
        // entry that owns it.
        let mention = format!(r#"{{"type":"assistant","uuid":"other","text":"{uuid}"}}"#);
        std::fs::write(&path, format!("{SCHEDULED_ENTRY}\n{mention}\n")).expect("write");
        let entry = find_entry(&path, uuid).expect("found");
        assert_eq!(
            entry.get("turnOrigin").and_then(Value::as_str),
            Some("scheduled")
        );
        assert!(find_entry(&path, "missing").is_none());
        assert!(find_entry(&dir.path().join("absent.jsonl"), uuid).is_none());
    }
}
