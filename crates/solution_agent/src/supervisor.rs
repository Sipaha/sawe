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
    /// The agent has (legitimately) stopped to wait on an asynchronous task it
    /// said it would resume after — a background build/test, a long command, a
    /// deploy. Instead of nudging it (which would just interrupt), the
    /// supervisor SLEEPS for `wait_seconds` (clamped to [`MAX_WAIT_SECS`]) and
    /// re-evaluates on the next wake-up. If the dialogue has moved on by then the
    /// session is progressing; if a lot of time has passed with no progress the
    /// judge should `continue` (wake it to check the result) instead of waiting
    /// again. Does NOT count toward the consecutive-continue guard.
    Wait = 5,
}

/// Hard ceiling on a single `Wait` verdict's sleep, and the default when the
/// judge omits a duration. A wait re-evaluates after at most this long, so a
/// genuinely-stuck background task is re-checked (and can be woken) within the
/// window rather than parking the supervisor indefinitely.
pub const MAX_WAIT_SECS: u64 = 300;
pub const MIN_WAIT_SECS: u64 = 10;
pub const DEFAULT_WAIT_SECS: u64 = 120;

/// Clamp a judge-supplied wait duration into `[MIN_WAIT_SECS, MAX_WAIT_SECS]`,
/// defaulting to `DEFAULT_WAIT_SECS` when absent. Pure for unit-testing.
pub fn clamp_wait_secs(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_WAIT_SECS)
        .clamp(MIN_WAIT_SECS, MAX_WAIT_SECS)
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
    /// TRANSIENT (not persisted): set when a human reply supersedes the
    /// in-flight judge while it is `Judging` (`supersede_judge_on_user_reply`).
    /// `apply_verdict` consumes it (takes + clears) to DROP a verdict that
    /// raced in after the user already steered the agent — no nudge / Observer
    /// breadcrumb (bug #1). Cleared whenever a fresh judge is spawned (status →
    /// `Judging`) so it can never pre-suppress the next cycle's verdict.
    pub judge_superseded: bool,
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
            judge_superseded: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerdictStats {
    pub total: usize,
    /// Indexed by `VerdictAction as usize` (Continue, Compact, Done, Ask,
    /// AskAgent, Wait).
    pub by_action: [usize; 6],
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

/// True when `message` is one of claude's subscription / API usage walls —
/// the ~5-hour "session limit", the "weekly limit", or the API
/// quota/rate/billing forms. They all mean "stop issuing requests until the
/// limit resets". The wording differs between the two subscription limits
/// (and from the API errors), so this matches every observed phrasing rather
/// than a single token. Case-insensitive.
///
/// Real examples this must catch:
///   "You've hit your session limit · resets 8:20pm (Asia/Novosibirsk)"  (5h)
///   "You've reached your weekly limit · resets ..."                     (weekly)
///   "rate_limit_error" / "usage limit reached" / "insufficient quota"   (API)
pub fn is_usage_limit_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("usage limit")
        || m.contains("session limit")
        || m.contains("weekly limit")
        || m.contains("rate_limit")
        || m.contains("rate limit")
        || m.contains("quota")
        || m.contains("insufficient")
        || m.contains("billing")
        || m.contains("credit")
        // Subscription phrasings that don't contain any of the words above:
        || m.contains("hit your limit")
        || m.contains("reached your limit")
        || m.contains("limit · resets")
        || m.contains("limit reached")
}

pub fn classify_judge_error(message: &str) -> JudgeFailure {
    // Quota/usage/billing exhaustion → stop immediately, do not retry.
    if is_usage_limit_error(message) {
        return JudgeFailure::Quota;
    }
    JudgeFailure::Transient
}

/// Parse the reset moment from a claude usage-limit message and return it as
/// epoch-millis (UTC). The message carries a wall-clock time plus, usually, a
/// timezone in parentheses, e.g.:
///   "You've hit your session limit · resets 8:20pm (Asia/Novosibirsk)"
///   "...weekly limit · resets Wed 9am (America/New_York)"
///   "...resets 20:20"                              (24h, no tz)
///
/// Resolution order for the timezone: the IANA name in parentheses first
/// (claude prints it), falling back to the machine's local timezone (which is
/// what claude reports against anyway). A weekday token (`mon`..`sun`) selects
/// the next matching date — used by the weekly limit; without one the time is
/// taken as today (or tomorrow if already past) — the session limit.
///
/// Returns `None` when there is no parseable `resets <time>` clause, so the
/// caller can fall back to a terminal stop. An *under*-estimate (e.g. a weekly
/// reset printed without a weekday, resolved to tomorrow) is self-correcting:
/// the resumed turn re-hits the still-active limit and re-schedules.
pub fn parse_usage_limit_reset_ms(message: &str, now_ms: i64) -> Option<i64> {
    use chrono::{Datelike, Duration, NaiveTime, TimeZone, Utc, Weekday};

    let lower = message.to_ascii_lowercase();
    let after = lower.split("resets").nth(1)?;

    // Timezone in parentheses, if present: "(Asia/Novosibirsk)". Parsed from
    // the ORIGINAL message (IANA names are case-sensitive).
    let tz: Option<chrono_tz::Tz> = message
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .and_then(|(name, _)| name.trim().parse::<chrono_tz::Tz>().ok());

    // Optional weekday token (weekly limit).
    let weekday: Option<Weekday> = after.split_whitespace().find_map(parse_weekday);

    // First time-looking token: "8:20pm" / "8pm" / "20:20" / "9".
    let (hour, minute) = after
        .split_whitespace()
        .find_map(|tok| parse_clock(tok.trim_matches(|c: char| c == '.' || c == ',')))?;

    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;

    // Build the target instant in the resolved timezone, then convert to UTC
    // millis. Done generically over the timezone so the local-fallback and the
    // named-tz path share one code path.
    fn resolve<Tz: TimeZone>(
        tz: Tz,
        now_ms: i64,
        time: NaiveTime,
        weekday: Option<Weekday>,
    ) -> Option<i64> {
        let now = Utc.timestamp_millis_opt(now_ms).single()?.with_timezone(&tz);
        let mut date = now.date_naive();
        match weekday {
            Some(target) => {
                // Advance to the next date whose weekday matches (today counts
                // only if the time hasn't passed yet).
                for _ in 0..8 {
                    if date.weekday() == target {
                        if let Some(dt) = tz
                            .from_local_datetime(&date.and_time(time))
                            .single()
                            .filter(|dt| dt.timestamp_millis() > now_ms)
                        {
                            return Some(dt.timestamp_millis());
                        }
                    }
                    date += Duration::days(1);
                }
                None
            }
            None => {
                let today = tz.from_local_datetime(&date.and_time(time)).single();
                match today {
                    Some(dt) if dt.timestamp_millis() > now_ms => Some(dt.timestamp_millis()),
                    _ => tz
                        .from_local_datetime(&(date + Duration::days(1)).and_time(time))
                        .single()
                        .map(|dt| dt.timestamp_millis()),
                }
            }
        }
    }

    match tz {
        Some(tz) => resolve(tz, now_ms, time, weekday),
        None => resolve(chrono::Local, now_ms, time, weekday),
    }
}

/// Parse a 3+ letter weekday prefix (`mon`, `tuesday`, …) — lowercase input.
fn parse_weekday(token: &str) -> Option<chrono::Weekday> {
    use chrono::Weekday::*;
    let t = token.trim_matches(|c: char| !c.is_ascii_alphabetic());
    if t.len() < 3 {
        return None;
    }
    Some(match &t[..3] {
        "mon" => Mon,
        "tue" => Tue,
        "wed" => Wed,
        "thu" => Thu,
        "fri" => Fri,
        "sat" => Sat,
        "sun" => Sun,
        _ => return None,
    })
}

/// Parse a clock token into `(hour_0_23, minute)`. Accepts 12h (`8pm`,
/// `8:20pm`, `12am`) and 24h (`20:20`, `8:20`, `9`). Returns `None` for tokens
/// that don't look like a time (so weekday / timezone tokens are skipped).
fn parse_clock(token: &str) -> Option<(u32, u32)> {
    let t = token.trim();
    let (digits, meridiem) = if let Some(rest) = t.strip_suffix("pm") {
        (rest, Some(true))
    } else if let Some(rest) = t.strip_suffix("am") {
        (rest, Some(false))
    } else {
        (t, None)
    };
    let digits = digits.trim();
    if digits.is_empty() || !digits.chars().next()?.is_ascii_digit() {
        return None;
    }
    let (h_str, m_str) = match digits.split_once(':') {
        Some((h, m)) => (h, m),
        None => (digits, "0"),
    };
    let mut hour: u32 = h_str.parse().ok()?;
    let minute: u32 = m_str.parse().ok()?;
    if minute > 59 {
        return None;
    }
    match meridiem {
        Some(true) => {
            // pm: 12pm stays 12, 1–11pm add 12.
            if hour == 12 {
                hour = 12;
            } else if hour <= 11 {
                hour += 12;
            } else {
                return None; // "13pm" is nonsense
            }
        }
        Some(false) => {
            // am: 12am is midnight (0), others unchanged.
            if hour == 12 {
                hour = 0;
            } else if hour > 11 {
                return None;
            }
        }
        None => {
            // 24h. Bare hour like "9" is valid; 0..=23 only.
        }
    }
    if hour > 23 {
        return None;
    }
    Some((hour, minute))
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

    #[test]
    fn clamp_wait_secs_bounds_and_default() {
        assert_eq!(clamp_wait_secs(None), DEFAULT_WAIT_SECS);
        assert_eq!(clamp_wait_secs(Some(0)), MIN_WAIT_SECS);
        assert_eq!(clamp_wait_secs(Some(5)), MIN_WAIT_SECS);
        assert_eq!(clamp_wait_secs(Some(90)), 90);
        assert_eq!(clamp_wait_secs(Some(10_000)), MAX_WAIT_SECS);
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
    fn cap_log_tail_trims_to_whole_lines_under_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("verdicts.jsonl");
        let mut body = String::new();
        for i in 0..1000 {
            body.push_str(&format!("{{\"n\":{i},\"pad\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}}\n"));
        }
        std::fs::write(&path, &body).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();

        cap_log_tail(&path, 4096);
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after <= 4096, "capped under max: {after}");
        assert!(after < before, "file shrank");
        let contents = std::fs::read_to_string(&path).unwrap();
        // No partial leading line — the first kept line is a whole record.
        let first = contents.lines().next().unwrap();
        assert!(
            first.starts_with('{') && first.ends_with('}'),
            "no partial leading line: {first:?}"
        );
        // The most recent line is always retained.
        assert!(contents.contains("\"n\":999"));

        // No-op when already under the cap (contents byte-identical).
        cap_log_tail(&path, 10 * 1024 * 1024);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
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

    #[test]
    fn usage_limit_detects_both_subscription_limits() {
        // The real ~5-hour (session) and weekly subscription walls — neither
        // contains "usage limit" / "rate limit" / "quota", so the old check
        // missed them and the supervisor retried the wall forever.
        assert!(is_usage_limit_error(
            "You've hit your session limit · resets 8:20pm (Asia/Novosibirsk)"
        ));
        assert!(is_usage_limit_error(
            "You've reached your weekly limit · resets Wed 9am"
        ));
        // API / billing forms still match.
        assert!(is_usage_limit_error("rate_limit_error"));
        assert!(is_usage_limit_error("insufficient quota"));
        // Non-limit errors do not.
        assert!(!is_usage_limit_error("overloaded_error"));
        assert!(!is_usage_limit_error("connection reset by peer"));
        // The session-limit message must classify as Quota (was Transient).
        assert!(matches!(
            classify_judge_error("You've hit your session limit · resets 8:20pm (Asia/Novosibirsk)"),
            JudgeFailure::Quota
        ));
    }

    #[test]
    fn parse_reset_session_limit_named_tz() {
        use chrono::{TimeZone, Utc};
        // 2026-06-29 12:15:00 UTC == 19:15 in Asia/Novosibirsk (UTC+7).
        let now = Utc.with_ymd_and_hms(2026, 6, 29, 12, 15, 0).unwrap();
        let got = parse_usage_limit_reset_ms(
            "You've hit your session limit · resets 8:20pm (Asia/Novosibirsk)",
            now.timestamp_millis(),
        )
        .expect("parse");
        // 8:20pm Novosibirsk == 13:20:00 UTC, same day (still ahead of 19:15).
        let want = Utc.with_ymd_and_hms(2026, 6, 29, 13, 20, 0).unwrap();
        assert_eq!(got, want.timestamp_millis());
    }

    #[test]
    fn parse_reset_24h_and_rolls_to_tomorrow() {
        use chrono_tz::Tz;
        // Use a fixed named tz in the message so the test is independent of the
        // machine's local zone. now = 10:00 UTC == 17:00 in Novosibirsk.
        let tz: Tz = "Asia/Novosibirsk".parse().unwrap();
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 6, 29, 10, 0, 0)
            .unwrap();
        use chrono::TimeZone as _;
        // "resets 9:00" (24h) — 9:00 already passed today (17:00 now) → tomorrow.
        let got = parse_usage_limit_reset_ms(
            "weekly limit · resets 9:00 (Asia/Novosibirsk)",
            now.timestamp_millis(),
        )
        .expect("parse");
        let want = tz
            .with_ymd_and_hms(2026, 6, 30, 9, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(got, want);
    }

    #[test]
    fn parse_reset_weekday_picks_next_matching_day() {
        use chrono::TimeZone as _;
        use chrono_tz::Tz;
        let tz: Tz = "Asia/Novosibirsk".parse().unwrap();
        // 2026-06-29 is a Monday. Next Wednesday is 2026-07-01.
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 29, 3, 0, 0).unwrap();
        let got = parse_usage_limit_reset_ms(
            "You've reached your weekly limit · resets Wed 9am (Asia/Novosibirsk)",
            now.timestamp_millis(),
        )
        .expect("parse");
        let want = tz
            .with_ymd_and_hms(2026, 7, 1, 9, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(got, want);
    }

    #[test]
    fn parse_reset_none_when_no_clause() {
        let now = 1_782_000_000_000;
        assert_eq!(parse_usage_limit_reset_ms("rate_limit_error", now), None);
        assert_eq!(
            parse_usage_limit_reset_ms("You've hit your session limit", now),
            None
        );
    }

    #[test]
    fn parse_clock_forms() {
        assert_eq!(parse_clock("8:20pm"), Some((20, 20)));
        assert_eq!(parse_clock("8pm"), Some((20, 0)));
        assert_eq!(parse_clock("12am"), Some((0, 0)));
        assert_eq!(parse_clock("12pm"), Some((12, 0)));
        assert_eq!(parse_clock("20:20"), Some((20, 20)));
        assert_eq!(parse_clock("9"), Some((9, 0)));
        assert_eq!(parse_clock("wed"), None);
        assert_eq!(parse_clock("13pm"), None);
        assert_eq!(parse_clock("8:99"), None);
    }
}
