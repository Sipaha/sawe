//! Per-chat "supervisor": types, on-disk verdict log, and pure predicates.
//! GPUI-free so it unit-tests in isolation. Orchestration lives in `store.rs`
//! (`tick_supervisor`) and `mcp.rs` (the verdict tools).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::SolutionSessionId;

pub const IDLE_THRESHOLD_SECS: u64 = 60;
pub const MAX_CONSECUTIVE_CONTINUES: u32 = 15;
pub const AUDIT_EVERY: u32 = 5;
pub const BACKOFF_SCHEDULE_MINS: [u64; 8] = [1, 1, 2, 3, 5, 10, 30, 60];
/// Watchdog grace period for a judge that errored or ended WITHOUT calling its
/// verdict tool. If a supervised session has been `Judging` for longer than this
/// (measured from `last_fired_at`), `tick_supervisor` treats it as a transient
/// judge failure (`on_judge_failed`) so the session never wedges in `Judging`
/// forever. Chosen over a per-judge session-state subscription because it
/// uniformly catches crash, error, AND silent-end with no fragile state plumbing.
pub const JUDGE_TIMEOUT_SECS: u64 = 5 * 60;

/// Watchdog grace period for a meta-auditor that errored or ended WITHOUT
/// calling `supervisor_audit_verdict`. Unlike the judge, an auditor spawns
/// while the supervised session is `Watching` (not `Judging`), so the
/// judge-stuck timeout never catches it; without this sweep its
/// `auditor_sessions` handle would leak forever and meta-audit would be
/// permanently disabled for that session. Same magnitude as the judge timeout.
/// An auditor timeout only cleans up the stale handle — it does NOT apply
/// supervision backoff (the auditor failing is not the judge failing).
pub const AUDITOR_TIMEOUT_SECS: u64 = 5 * 60;

/// Decision returned by [`continue_guard`] — what the store should do after
/// incrementing `consecutive_continues` on a `Continue` verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueGuard {
    /// Normal path: nudge the supervised session and stay in `Watching`.
    Nudge,
    /// Every `AUDIT_EVERY` continues: nudge AND launch the meta-auditor.
    Audit,
    /// `MAX_CONSECUTIVE_CONTINUES` reached: stop nudging, escalate to the user.
    ForceAsk,
}

/// Pure guard decision based on how many consecutive continues have been issued.
/// ForceAsk wins at `>= MAX_CONSECUTIVE_CONTINUES`; Audit fires every
/// `AUDIT_EVERY` steps (when `> 0 && count % AUDIT_EVERY == 0`); else Nudge.
pub fn continue_guard(consecutive_continues: u32) -> ContinueGuard {
    if consecutive_continues >= MAX_CONSECUTIVE_CONTINUES {
        return ContinueGuard::ForceAsk;
    }
    if consecutive_continues > 0 && consecutive_continues.is_multiple_of(AUDIT_EVERY) {
        return ContinueGuard::Audit;
    }
    ContinueGuard::Nudge
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictAction {
    Continue = 0,
    Compact = 1,
    Done = 2,
    /// Escalate a question to the human operator (work pauses for the user).
    Ask = 3,
    /// Pose a clarifying question to the WORKING AGENT (not the human): the
    /// question is sent into the supervised session, the agent answers, and the
    /// supervisor re-evaluates on the next wake-up with the answer in hand. Use
    /// when it's unclear whether the work is actually finished. Subject to the
    /// same consecutive-nudge guards as `Continue` so it can't loop forever.
    AskAgent = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Verdict,
    Audit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictRecord {
    pub ts_ms: i64,
    pub kind: VerdictKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<VerdictAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_ok: Option<bool>,
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoppedReason {
    Quota,
    ProviderError,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorStatus {
    Disabled,
    Watching,
    Judging,
    WaitingUser,
    /// The user manually stopped the agent: supervision is enabled but
    /// deliberately standing by — it must NOT re-engage on the current dialog
    /// state (no judge, no nudge). Cleared back to `Watching` by the next human
    /// message (`reset_supervisor_continue_counter`). Distinct from
    /// `WaitingUser` (the supervisor asked a question) and from `Disabled`
    /// (supervision off): here the user, not the supervisor, hit the brakes.
    Held,
    Stopped(StoppedReason),
}

impl SupervisorStatus {
    /// DB string form: `watching` / `judging` / `waiting_user` / `disabled`
    /// / `stopped:quota` / `stopped:provider_error` / `stopped:done`.
    pub fn to_db_string(&self) -> String {
        match self {
            Self::Disabled => "disabled".into(),
            Self::Watching => "watching".into(),
            Self::Judging => "judging".into(),
            Self::WaitingUser => "waiting_user".into(),
            Self::Held => "held".into(),
            Self::Stopped(StoppedReason::Quota) => "stopped:quota".into(),
            Self::Stopped(StoppedReason::ProviderError) => "stopped:provider_error".into(),
            Self::Stopped(StoppedReason::Done) => "stopped:done".into(),
        }
    }

    /// Short human-readable label for UI surfaces (status-row popover header,
    /// Eye-icon tooltip).
    pub fn human_label(&self) -> &'static str {
        match self {
            Self::Disabled => "Off",
            Self::Watching => "Watching",
            Self::Judging => "Reviewing…",
            Self::WaitingUser => "Waiting for you",
            Self::Held => "On hold",
            Self::Stopped(StoppedReason::Quota) => "Stopped (quota)",
            Self::Stopped(StoppedReason::ProviderError) => "Stopped (error)",
            Self::Stopped(StoppedReason::Done) => "Done",
        }
    }

    pub fn parse_db_string(s: &str) -> Self {
        match s {
            "watching" => Self::Watching,
            "judging" => Self::Judging,
            "waiting_user" => Self::WaitingUser,
            "held" => Self::Held,
            "stopped:quota" => Self::Stopped(StoppedReason::Quota),
            "stopped:provider_error" => Self::Stopped(StoppedReason::ProviderError),
            "stopped:done" => Self::Stopped(StoppedReason::Done),
            _ => Self::Disabled,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SupervisorState {
    pub session_id: SolutionSessionId,
    pub enabled: bool,
    pub custom_prompt: Option<String>,
    pub consecutive_continues: u32,
    pub backoff_attempt: u32,
    pub last_fired_at: Option<i64>,
    /// Epoch-millis before which the watchdog must NOT fire a new judge for this
    /// session. Set by `on_judge_failed` on a transient backoff
    /// (`now + BACKOFF_SCHEDULE_MINS[attempt-1] * 60_000`); cleared to `None` on a
    /// successful verdict and whenever supervision is (re)enabled so a recovered
    /// or freshly-enabled supervisor is never permanently gated. `tick_supervisor`
    /// gates firing on `now_ms >= next_eligible_ms.unwrap_or(0)`.
    pub next_eligible_ms: Option<i64>,
    pub status: SupervisorStatus,
    /// How many times the supervisor has fired (spawned a judge) since it was
    /// last (re)enabled. Surfaced next to the status-row Eye icon as an
    /// at-a-glance "how active has the observer been" counter. Reset to 0 on
    /// every enable/disable toggle (`set_supervision_enabled`). Persisted with
    /// the rest of the supervisor row so it survives a restart.
    pub trigger_count: u32,
    /// Epoch-millis of the last time the human typed into this session's compose
    /// box. TRANSIENT (not persisted): the watchdog treats the session as "still
    /// active" until `IDLE_THRESHOLD_SECS` after the last keystroke, so the
    /// supervisor never fires a nudge while the user is mid-message. Reset to
    /// `None` on restart (a cold session has no in-flight draft to protect).
    pub last_user_input_ms: Option<i64>,
}

impl SupervisorState {
    pub fn new(session_id: SolutionSessionId) -> Self {
        Self {
            session_id,
            enabled: false,
            custom_prompt: None,
            consecutive_continues: 0,
            backoff_attempt: 0,
            last_fired_at: None,
            next_eligible_ms: None,
            status: SupervisorStatus::Disabled,
            trigger_count: 0,
            last_user_input_ms: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerdictStats {
    pub total: usize,
    /// Indexed by `VerdictAction as usize` (Continue, Compact, Done, Ask, AskAgent).
    pub by_action: [usize; 5],
    pub audits: usize,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeFailure {
    Quota,
    Transient,
}

/// Inputs to [`build_judge_briefing`]. All paths are pre-resolved by the
/// caller (the store, which knows the solution root) so this builder stays
/// pure and unit-testable.
pub struct JudgeBriefingContext {
    pub supervised_session_id: String,
    pub diary_path: String,
    pub verdicts_path: String,
    /// Path to the durable user-intent record the judge maintains (see
    /// [`intent_path`]).
    pub intent_path: String,
    pub compact_dir: String,
    pub custom_prompt: Option<String>,
    /// Human-readable context-window fullness of the supervised session at
    /// spawn time (e.g. `"187,000 / 200,000 tokens (94%)"`), injected so the
    /// judge can weigh a `compact` verdict without an extra round-trip. `None`
    /// when usage is unknown (cold session, no live token reading yet).
    pub context_usage: Option<String>,
    pub audit: bool,
    /// Absolute path to the editor binary, used to reach the Solution MCP
    /// socket from Bash via `<bridge_bin> --nc <socket_path>`. The judge's
    /// claude process is NOT reliably given the editor's `solution_agent.*`
    /// MCP tools (claude's MCP-server registration is flaky and silently
    /// drops servers), so the judge talks to the socket through this bridge
    /// binary — a plain shell pipe it always has — instead of MCP tools.
    pub bridge_bin: String,
    /// Absolute path to this Solution's MCP unix socket (the per-solution
    /// `mcp.sock`). Target of the `--nc` bridge pipe above.
    pub socket_path: String,
}

const JUDGE_INSTRUCTIONS: &str = include_str!("../resources/supervisor_judge_instructions.md");
const AUDIT_INSTRUCTIONS: &str = include_str!("../resources/supervisor_audit_instructions.md");

/// System prompt for the ephemeral judge/auditor sessions, appended INSTEAD of
/// the solution's default worker system prompt. The default prompt frames the
/// session as a worker ("You are working inside a Solution… run build/test/git…"),
/// which can pull the judge into doing the task instead of judging it. This
/// override keeps Claude's standard tool-using behaviour but re-frames the
/// session as a read-only outside evaluator whose only output is a verdict tool
/// call. The per-turn briefing carries the concrete instructions.
pub const SUPERVISOR_SYSTEM_PROMPT: &str = "\
You are an independent Supervisor evaluating another AI coding session — you are \
NOT a worker on its task. Do NOT write or edit code, run the task, or make git \
commits. Your sole job is to read the supervised session and its artifacts, then \
issue exactly ONE verdict. You reach the editor (to read the conversation and to \
submit your verdict) by piping JSON-RPC through the `--nc` socket bridge from \
Bash — NOT through `mcp__*` tools (do NOT ToolSearch for editor tools; they are \
not in your toolset). The first message gives you the exact bridge command and \
the `solution_agent.*` method names to call. You may read files and update your \
diary, but stay outside the work and judge it from the outside.";

/// Render the judge's single user-turn briefing by substituting the runtime
/// paths into the instruction template. The meta-auditor variant (`audit:
/// true`) swaps in a different template but shares the same placeholder set.
pub fn build_judge_briefing(ctx: &JudgeBriefingContext) -> String {
    let template = if ctx.audit {
        AUDIT_INSTRUCTIONS
    } else {
        JUDGE_INSTRUCTIONS
    };
    let custom_section = match &ctx.custom_prompt {
        Some(prompt) => {
            format!("## Operator's specific instruction for this chat\n\n{prompt}\n")
        }
        None => String::new(),
    };
    let context_section = match &ctx.context_usage {
        Some(usage) => format!(
            "## Context-window fullness (right now)\n\n{usage}\n\nWeigh this against \
             what comes next (see the `compact` verdict): the higher the fullness AND \
             the heavier the next step, the stronger the case for a `compact` verdict \
             now. Don't treat any single percentage as a hard gate — a long/expensive \
             next run at moderate fullness (~65%+) warrants compacting before it, \
             while a short next step is fine at higher fullness.\n"
        ),
        None => String::new(),
    };
    template
        .replace("{SUPERVISED_SESSION_ID}", &ctx.supervised_session_id)
        .replace("{DIARY_PATH}", &ctx.diary_path)
        .replace("{VERDICTS_PATH}", &ctx.verdicts_path)
        .replace("{INTENT_PATH}", &ctx.intent_path)
        .replace("{COMPACT_DIR}", &ctx.compact_dir)
        .replace("{BRIDGE_BIN}", &ctx.bridge_bin)
        .replace("{SOCKET_PATH}", &ctx.socket_path)
        .replace("{CONTEXT_USAGE_SECTION}", &context_section)
        .replace("{CUSTOM_PROMPT_SECTION}", &custom_section)
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
    file.write_all(section.as_bytes())
}

pub fn append_verdict(dir: &Path, rec: &VerdictRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut line = serde_json::to_string(rec).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(verdicts_path(dir))?;
    file.write_all(line.as_bytes())
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
    let mut stats = VerdictStats {
        total: records.len(),
        ..Default::default()
    };
    for rec in records {
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

pub fn should_fire(
    enabled: bool,
    status: &SupervisorStatus,
    session_idle_or_errored: bool,
    last_activity_ms: i64,
    now_ms: i64,
    threshold_secs: u64,
) -> bool {
    if !enabled || !session_idle_or_errored {
        return false;
    }
    if !matches!(status, SupervisorStatus::Watching) {
        return false;
    }
    let silent_ms = now_ms.saturating_sub(last_activity_ms);
    silent_ms >= (threshold_secs as i64) * 1000
}

pub fn classify_judge_error(message: &str) -> JudgeFailure {
    let m = message.to_ascii_lowercase();
    // Quota/usage/billing exhaustion → stop immediately, do not retry.
    if m.contains("usage limit")
        || m.contains("rate_limit")
        || m.contains("rate limit")
        || m.contains("quota")
        || m.contains("insufficient")
        || m.contains("billing")
        || m.contains("credit")
    {
        return JudgeFailure::Quota;
    }
    JudgeFailure::Transient
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SolutionSessionId;

    #[test]
    fn continue_guard_cap_and_audit_cadence() {
        use ContinueGuard::*;
        assert!(matches!(continue_guard(1), Nudge));
        assert!(matches!(continue_guard(4), Nudge));
        assert!(matches!(continue_guard(5), Audit)); // every 5th
        assert!(matches!(continue_guard(10), Audit));
        assert!(matches!(continue_guard(15), ForceAsk)); // hard cap wins at 15
        assert!(matches!(continue_guard(16), ForceAsk));
    }

    fn sid() -> SolutionSessionId {
        SolutionSessionId::parse("abcd1234").unwrap()
    }

    #[test]
    fn dir_is_under_agents_session_supervisor() {
        let root = std::path::Path::new("/tmp/sol");
        let dir = supervisor_dir(root, sid());
        assert_eq!(dir, root.join(".agents").join("abcd1234").join("supervisor"));
    }

    #[test]
    fn session_log_path_and_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = session_log_path(tmp.path(), sid());
        assert_eq!(
            path,
            tmp.path().join(".agents").join("abcd1234").join("session-log.md")
        );
        append_session_log(&path, "Compaction c01", "did the first thing", 0).unwrap();
        append_session_log(&path, "✓ Session complete (Supervisor)", "all done", 0).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("## Compaction c01"));
        assert!(contents.contains("did the first thing"));
        assert!(contents.contains("## ✓ Session complete (Supervisor)"));
        assert!(contents.contains("all done"));
        // appends accumulate (compaction entry precedes the completion entry)
        assert!(contents.find("Compaction c01").unwrap() < contents.find("all done").unwrap());
    }

    #[test]
    fn append_then_read_roundtrips_and_skips_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let rec = VerdictRecord {
            ts_ms: 1000,
            kind: VerdictKind::Verdict,
            action: Some(VerdictAction::Continue),
            audit_ok: None,
            reasoning: "two items left".into(),
            message: None,
            question: None,
            tokens: Some(1234),
        };
        append_verdict(dir, &rec).unwrap();
        // a corrupt line must not poison the reader
        std::fs::OpenOptions::new()
            .append(true)
            .open(verdicts_path(dir))
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"{not json}\n"))
            .unwrap();
        let back = read_verdicts(dir);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].action, Some(VerdictAction::Continue));
        assert_eq!(back[0].tokens, Some(1234));
    }

    #[test]
    fn stats_counts_by_action() {
        let recs = vec![
            mk(VerdictAction::Continue, 100),
            mk(VerdictAction::Continue, 200),
            mk(VerdictAction::Compact, 0),
            mk(VerdictAction::Done, 0),
        ];
        let s = verdict_stats(&recs);
        assert_eq!(s.total, 4);
        assert_eq!(s.by_action[VerdictAction::Continue as usize], 2);
        assert_eq!(s.by_action[VerdictAction::Compact as usize], 1);
        assert_eq!(s.total_tokens, 300);
    }

    fn mk(action: VerdictAction, tokens: u64) -> VerdictRecord {
        VerdictRecord {
            ts_ms: 0,
            kind: VerdictKind::Verdict,
            action: Some(action),
            audit_ok: None,
            reasoning: String::new(),
            message: None,
            question: None,
            tokens: if tokens > 0 { Some(tokens) } else { None },
        }
    }

    #[test]
    fn should_fire_respects_threshold_and_status() {
        let watching = SupervisorStatus::Watching;
        // 61s of silence, enabled, idle → fire
        assert!(should_fire(true, &watching, true, 0, 61_000, 60));
        // only 59s → don't fire
        assert!(!should_fire(true, &watching, true, 0, 59_000, 60));
        // disabled flag → never
        assert!(!should_fire(false, &watching, true, 0, 999_000, 60));
        // already judging → never
        assert!(!should_fire(true, &SupervisorStatus::Judging, true, 0, 999_000, 60));
        // not idle/errored (e.g. running) → never
        assert!(!should_fire(true, &watching, false, 0, 999_000, 60));
    }

    #[test]
    fn briefing_substitutes_paths_and_custom_prompt() {
        let ctx = JudgeBriefingContext {
            supervised_session_id: "abcd1234".into(),
            diary_path: "/sol/.agents/abcd1234/supervisor/diary.md".into(),
            verdicts_path: "/sol/.agents/abcd1234/supervisor/verdicts.jsonl".into(),
            intent_path: "/sol/.agents/abcd1234/supervisor/user_intent.md".into(),
            compact_dir: "/sol/.agents/abcd1234".into(),
            custom_prompt: Some("don't stop before tests pass".into()),
            context_usage: Some("187,000 / 200,000 tokens (94%)".into()),
            audit: false,
            bridge_bin: "/path/to/sawe".into(),
            socket_path: "/run/sol/mcp.sock".into(),
        };
        let out = build_judge_briefing(&ctx);
        assert!(out.contains("abcd1234"));
        assert!(out.contains("/sol/.agents/abcd1234/supervisor/diary.md"));
        assert!(out.contains("don't stop before tests pass"));
        assert!(out.contains("187,000 / 200,000 tokens (94%)"));
        // The `--nc` bridge command is fully materialized for the judge.
        assert!(out.contains("/path/to/sawe --nc /run/sol/mcp.sock"));
        assert!(!out.contains("{DIARY_PATH}"), "all placeholders substituted");
        assert!(!out.contains("{BRIDGE_BIN}"));
        assert!(!out.contains("{SOCKET_PATH}"));
        assert!(!out.contains("{CUSTOM_PROMPT_SECTION}"));
        assert!(!out.contains("{CONTEXT_USAGE_SECTION}"));
    }

    #[test]
    fn briefing_omits_custom_section_when_absent() {
        let ctx = JudgeBriefingContext {
            supervised_session_id: "abcd1234".into(),
            diary_path: "d".into(),
            verdicts_path: "v".into(),
            intent_path: "i".into(),
            compact_dir: "c".into(),
            custom_prompt: None,
            context_usage: None,
            audit: false,
            bridge_bin: "/path/to/sawe".into(),
            socket_path: "/run/sol/mcp.sock".into(),
        };
        let out = build_judge_briefing(&ctx);
        assert!(!out.contains("{CUSTOM_PROMPT_SECTION}"));
        assert!(!out.contains("{CONTEXT_USAGE_SECTION}"));
    }

    #[test]
    fn classify_error_quota_vs_transient() {
        assert!(matches!(classify_judge_error("usage limit reached"), JudgeFailure::Quota));
        assert!(matches!(classify_judge_error("Error: rate_limit_error"), JudgeFailure::Quota));
        assert!(matches!(classify_judge_error("overloaded_error"), JudgeFailure::Transient));
        assert!(matches!(classify_judge_error("connection reset"), JudgeFailure::Transient));
    }
}
