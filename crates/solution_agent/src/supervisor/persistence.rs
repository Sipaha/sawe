//! On-disk supervisor breadcrumbs: path resolution, the diary / verdict-log /
//! user-intent files, the append-only session log, and the tail-capping used to
//! keep those append-only logs bounded. GPUI-free, best-effort IO.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::model::SolutionSessionId;

use super::state::{VerdictKind, VerdictRecord};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerdictStats {
    pub total: usize,
    /// Indexed by `VerdictAction as usize` (Continue, Compact, Done, Ask,
    /// AskAgent, Wait).
    pub by_action: [usize; 6],
    pub audits: usize,
    pub total_tokens: u64,
}

pub fn supervisor_dir(solution_root: &Path, session_id: SolutionSessionId) -> PathBuf {
    solution_root
        .join(".agents")
        .join(session_id.to_string())
        .join("supervisor")
}

pub fn diary_path(dir: &Path) -> PathBuf {
    dir.join("diary.md")
}

pub fn verdicts_path(dir: &Path) -> PathBuf {
    dir.join("verdicts.jsonl")
}

/// Durable, compaction-surviving record of the user's standing intent — the
/// judge maintains it (reads the live conversation, distills the user's
/// directives + their context, supersedes contradicted decisions) so the goal
/// is never lost when a `compact` verdict wipes the transcript.
pub fn intent_path(dir: &Path) -> PathBuf {
    dir.join("user_intent.md")
}

/// Best-effort wipe of the observer's durable memory for a session — the diary,
/// the verdict log, AND the distilled user-intent record. Called when the HUMAN
/// manually clears (`/clear`) or compacts (`/compact`) the session, so the
/// supervisor reasons from a clean slate afterwards instead of re-reading stale
/// notes and re-litigating already-settled directives. Deliberately NOT called
/// on an observer-initiated `compact` verdict — that path relies on
/// `user_intent.md` surviving so the goal isn't lost when the transcript is
/// wiped. Silent on a missing file (nothing to wipe); logs other IO errors.
pub fn wipe_supervisor_memory(dir: &Path) {
    for path in [diary_path(dir), verdicts_path(dir), intent_path(dir)] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("wipe_supervisor_memory: {}: {e}", path.display()),
        }
    }
}

/// Cumulative, compaction-surviving record of what was accomplished over a
/// session's lifetime, at `<solution_root>/.agents/<session_id>/session-log.md`.
/// Each custom compaction appends the agent's own `state.md` summary here, and
/// the supervisor's `done` verdict appends a final wrap-up — so the operator can
/// return later and read the whole arc even after context compactions wiped the
/// live dialogue.
pub fn session_log_path(solution_root: &Path, session_id: SolutionSessionId) -> PathBuf {
    solution_root
        .join(".agents")
        .join(session_id.to_string())
        .join("session-log.md")
}

/// Append a timestamped `## {header}` section followed by `body` to `path`,
/// creating the file (and its parent dir) on first write. Best-effort: the
/// caller logs the error. `now_ms` is the Unix-millis timestamp to stamp (the
/// caller passes `chrono::Utc::now().timestamp_millis()` — kept as a param so
/// this stays pure and unit-testable).
/// Size caps for the append-only supervisor breadcrumb logs. A multi-day
/// supervised session would otherwise grow these without bound (one diary note
/// per failure/timeout, one verdict line per judge fire, one section per
/// compaction). These are tail-read only (the auditor + operator look at the
/// recent end), so capping keeps the last N bytes and discards the head.
pub const VERDICTS_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
pub const DIARY_LOG_MAX_BYTES: u64 = 1024 * 1024;
pub const SESSION_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Best-effort cap on an append-only log: if `path` exceeds `max_bytes`, rewrite
/// it keeping only the last `max_bytes` worth of WHOLE lines. Drops the partial
/// leading line so a `.jsonl` is never left with a truncated record. Called
/// right after each append; silent no-op on any IO error (these are
/// breadcrumbs, never load-bearing) and when the file is already under the cap.
pub fn cap_log_tail(path: &Path, max_bytes: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= max_bytes {
        return;
    }
    let Ok(contents) = std::fs::read(path) else {
        return;
    };
    let start = contents.len().saturating_sub(max_bytes as usize);
    let slice = &contents[start..];
    // Skip the (likely partial) first line so we keep only whole records.
    let line_start = slice
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let _ = std::fs::write(path, &slice[line_start..]);
}

pub fn append_session_log(
    path: &Path,
    header: &str,
    body: &str,
    now_ms: i64,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stamp = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default();
    let section = format!("\n## {header} — {stamp}\n\n{}\n", body.trim_end());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(section.as_bytes())?;
    cap_log_tail(path, SESSION_LOG_MAX_BYTES);
    Ok(())
}

/// Replace the standing-intent record with `text`.
///
/// The judge does NOT write this file: it returns the updated record in its
/// verdict and the editor writes it here. That is what lets the observer hold
/// no write capability over any file, and it leaves every path under
/// `.agents/<sid>/` with exactly one writer — the editor.
pub fn write_intent_record(dir: &Path, text: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut body = text.trim_end().to_string();
    body.push('\n');
    std::fs::write(intent_path(dir), body)
}

/// Append one dated entry to the diary. Multi-line notes are indented under
/// their bullet so the file stays a readable markdown list. Shared by the
/// judge's own note (carried in its verdict) and the editor's mechanical notes,
/// so the two cannot drift into different shapes.
pub fn append_diary_entry(dir: &Path, note: &str, now_ms: i64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let stamp = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();
    let mut entry = String::new();
    for (idx, line) in note.trim().lines().enumerate() {
        if idx == 0 {
            entry.push_str(&format!("- {stamp} {line}\n"));
        } else {
            entry.push_str(&format!("  {line}\n"));
        }
    }
    if entry.is_empty() {
        return Ok(());
    }
    let path = diary_path(dir);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(entry.as_bytes())?;
    cap_log_tail(&path, DIARY_LOG_MAX_BYTES);
    Ok(())
}

/// Read a supervisor breadcrumb file for injection into the briefing, keeping
/// at most `max_bytes` of its TAIL (whole lines) — the recent end is the part
/// that informs the next verdict, and a pathological file must not be able to
/// push the actual transcript out of the judge's context. `None` when the file
/// is missing or empty, which the briefing renders as "no record yet".
pub fn read_for_briefing(path: &Path, max_bytes: usize) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= max_bytes {
        return Some(trimmed.to_string());
    }
    // The cut is a byte offset into text that is routinely NOT ASCII — the
    // record quotes the user and the judge is told to keep their language — and
    // slicing a `&str` anywhere but a char boundary panics. This runs on the
    // main thread every time a judge is spawned, so that panic would be an
    // editor crash, not a lost briefing. Walk forward to the next boundary:
    // forward rather than back, so the result is never LONGER than the cap.
    let mut start = trimmed.len().saturating_sub(max_bytes);
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    let tail = &trimmed[start..];
    let line_start = tail.find('\n').map(|i| i + 1).unwrap_or(0);
    Some(format!(
        "[earlier entries dropped — showing the last {max_bytes} bytes]\n{}",
        &tail[line_start..]
    ))
}

/// How much of the intent record / diary the briefing carries. Generous: the
/// judge used to read both files in full with its own file tool, so injecting
/// them costs the same tokens it already spent, minus two round-trips.
pub const BRIEFING_RECORD_MAX_BYTES: usize = 64 * 1024;

pub fn append_verdict(dir: &Path, rec: &VerdictRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut line = serde_json::to_string(rec).map_err(std::io::Error::other)?;
    line.push('\n');
    let path = verdicts_path(dir);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())?;
    cap_log_tail(&path, VERDICTS_LOG_MAX_BYTES);
    Ok(())
}

pub fn read_verdicts(dir: &Path) -> Vec<VerdictRecord> {
    let Ok(contents) = std::fs::read_to_string(verdicts_path(dir)) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<VerdictRecord>(l).ok())
        .collect()
}

pub fn verdict_stats(records: &[VerdictRecord]) -> VerdictStats {
    let mut stats = VerdictStats::default();
    for rec in records {
        // A dropped verdict was produced but never delivered — counting it would
        // inflate the acted-nudge tally the meta-auditor reasons about.
        if rec.dropped {
            continue;
        }
        stats.total += 1;
        if matches!(rec.kind, VerdictKind::Audit) {
            stats.audits += 1;
        }
        if let Some(action) = rec.action {
            stats.by_action[action as usize] += 1;
        }
        stats.total_tokens += rec.tokens.unwrap_or(0);
    }
    stats
}
