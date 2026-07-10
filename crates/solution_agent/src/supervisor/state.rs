//! Supervisor state machine: the persisted/transient status types, verdict
//! kinds, the pure guard predicates, and usage-limit classification/parsing.
//! GPUI-free — unit-tested in isolation via `supervisor/tests.rs`.

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

/// Liveness grace for the judge-stuck watchdog: `JUDGE_TIMEOUT_SECS` measures
/// wall-clock from the fire, but a THOROUGH judge (reading files, running
/// read-only Bash to inspect the work) can legitimately take longer than 5 min
/// while still streaming. Killing it there charges a bogus transient failure and
/// discards a nearly-complete verdict. So once the wall-clock timeout is crossed,
/// only declare the judge dead if its OWN session has been silent (no streaming /
/// tool activity) for this long — otherwise extend and re-check next tick.
pub const JUDGE_LIVENESS_SILENCE_SECS: u64 = 90;

/// Absolute cap on a single judge turn regardless of liveness — a judge that
/// streams forever (a runaway thinking loop) is still killed here so it can't pin
/// `Judging` indefinitely. Comfortably above any legitimate read-only review.
pub const JUDGE_HARD_TIMEOUT_SECS: u64 = 20 * 60;

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
/// judge omits a duration. A `wait` is one-shot: the judge commits a realistic
/// timeout up to this bound and the mechanism honors it in full (it does NOT
/// re-judge in between), so the ceiling is generous — 30 min — to let the
/// judge's own estimate stand rather than forcing a re-judge every few minutes
/// (a 5-minute cap produced dozens of identical `wait` verdicts on a session
/// parked over a long task). When the timeout elapses the mechanism wakes the
/// agent itself. A genuinely-stuck task is thus re-checked within 30 min.
pub const MAX_WAIT_SECS: u64 = 1800;
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
    /// The judge PRODUCED this verdict but the send-time gate dropped it — it was
    /// never delivered (superseded by a fresh user reply, supervision turned off /
    /// `Held` / `Stopped`, or the session resumed on its own mid-judge). Logged
    /// for the audit trail but excluded from [`verdict_stats`] and flagged to the
    /// meta-auditor so a dropped verdict is not miscounted as an acted nudge.
    /// `#[serde(default)]` keeps pre-field records readable (they read as acted).
    #[serde(default, skip_serializing_if = "is_false")]
    pub dropped: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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
    /// TRANSIENT (not persisted): an Observer nudge whose delivery was DEFERRED
    /// because the human was actively composing a message when the judge
    /// finished (a keystroke within `IDLE_THRESHOLD_SECS` — see
    /// `send_supervisor_nudge`). The start-time typing guard (`should_fire`)
    /// only prevents a NEW judge from firing mid-typing; it can't cover a judge
    /// that already fired while the user was idle and finished after the user
    /// started typing. Rather than barge into the middle of the user's
    /// sentence, the verdict is accepted (the continue-counter still bumps) but
    /// its nudge is parked here. `tick_supervisor` flushes it once the user has
    /// gone quiet for the standard idle window; a genuine user SEND
    /// (`supersede_judge_on_user_reply` on the `from_user` funnel) discards it —
    /// the user answered for themselves, so the stale observer nudge is
    /// forgotten. Reset to `None` on restart (a cold session has nothing held).
    pub pending_nudge: Option<String>,
    /// TRANSIENT (not persisted): distinguishes the TWO sources of a `Held`
    /// status. `Held` is shared by a manual user Stop (`hold_supervisor`, this is
    /// `false`) and a `done` verdict (`apply_verdict`'s `Done` arm parks in
    /// `Held` too, this is `true`). Only read while `status == Held`. It exists
    /// because self-resume must re-arm a `done`-parked session (the agent
    /// continued on its own → the "done" premise is false) but must NOT re-arm a
    /// manually-stopped one (the user's "don't drag it back until I message"
    /// rule) — see `rearm_supervisor_on_self_activity`. Both `Held`-entry sites
    /// set it explicitly, so a stale value when not `Held` is never read. `false`
    /// on restart (a cold-loaded `Held` row is treated as a manual stop — the
    /// conservative default that won't auto-resume supervision).
    pub held_by_done: bool,
    /// TRANSIENT (not persisted): a one-shot `wait` verdict's wake deadline
    /// (epoch-ms). The judge decides ONCE — "the agent is waiting on X, park
    /// until here" — and the mechanism honors that single timeout in FULL: while
    /// this is `Some` and status is `Watching`, `tick_supervisor` does NOT spawn
    /// a fresh judge (re-judging an unchanged wait is exactly the wasteful poll
    /// that produced 89 identical `wait` verdicts on one session). When
    /// `now >= wait_until_ms` the mechanism itself wakes the agent (a
    /// deterministic "the task should be done — check the result and continue"
    /// nudge, only if it's idle) and clears this; it does not re-spawn a judge
    /// just to re-decide. Cleared on a fresh judge fire, a user message, and
    /// enable/disable. `None` ⟺ no wait is parked.
    pub wait_until_ms: Option<i64>,
    /// TRANSIENT (not persisted): epoch-ms of when THIS process first started
    /// watching the session — set lazily on the first `tick_supervisor` pass
    /// that sees the row (a cold load leaves it `None`). It anchors the idle
    /// clock to "since we started watching", not to the persisted
    /// `last_activity_at` (which can be hours old from before a restart). The
    /// supervisor only judges state that changed UNDER its watch: a session
    /// that loads already-idle (`last_activity_at <= watch_started_ms`) is
    /// watched but never retroactively nudged, so reopening the editor no
    /// longer fires an instant judge on a pre-restart idle session. Once the
    /// agent produces genuinely-new activity (a turn completes this process,
    /// bumping `last_activity_at` past this baseline) the normal idle-nudge
    /// cycle re-engages. `None` on restart by design.
    pub watch_started_ms: Option<i64>,
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
            held_by_done: false,
            pending_nudge: None,
            wait_until_ms: None,
            watch_started_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeFailure {
    Quota,
    Transient,
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
    // ANCHORED to wall-shaped phrasings, NOT loose single words. This runs over
    // the agent's *whole* last assistant message (`tick_stuck_sessions`) — and an
    // agent working on rate-limiting / billing / quota code writes exactly those
    // words in prose ("added rate limit handling; insufficient test coverage").
    // A false positive there is dangerous: it SKIPS the reconnect a real hang
    // needs AND stops supervision as `Stopped(Quota)`. So bare "quota" /
    // "insufficient" / "billing" / "credit" / "rate limit" (space) are gone;
    // only the specific claude wall phrasings + API error codes remain (finding
    // #4). Every documented real wall is still covered (see the test).
    m.contains("usage limit")
        || m.contains("session limit")
        || m.contains("weekly limit")
        || m.contains("limit · resets")
        || m.contains("limit reached")
        || m.contains("hit your limit")
        || m.contains("reached your limit")
        || m.contains("rate_limit") // API error CODE (rate_limit_error) — underscore, not prose
        || m.contains("rate limit reached")
        || m.contains("rate limit exceeded")
        || m.contains("insufficient quota")
        || m.contains("quota exceeded")
        || m.contains("exceeded your quota")
        || m.contains("credit balance") // "your credit balance is too low"
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
        let now = Utc
            .timestamp_millis_opt(now_ms)
            .single()?
            .with_timezone(&tz);
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
                    // Today's time is already PAST. The stuck watchdog reads the
                    // wall ~5 min after it printed, so a session wall's "resets
                    // 8:20pm" is routinely a few minutes stale by the time we parse
                    // it — the limit reset moments ago. Rolling a full day here
                    // (`+1 day`) parks an autonomous session ~24 h for a wall that
                    // already cleared (finding #6). So if it's only RECENTLY past
                    // (within the grace window), resume ≈ now. Only when it's past
                    // by MORE than the grace (e.g. a weekly limit whose time-of-day
                    // printed without a weekday, hours past) do we under-estimate
                    // to tomorrow — which self-corrects when the re-hit reschedules.
                    Some(dt) => {
                        const RESET_GRACE_MS: i64 = 60 * 60 * 1000;
                        let past_by = now_ms - dt.timestamp_millis();
                        if (0..=RESET_GRACE_MS).contains(&past_by) {
                            Some(now_ms)
                        } else {
                            tz.from_local_datetime(&(date + Duration::days(1)).and_time(time))
                                .single()
                                .map(|dt| dt.timestamp_millis())
                        }
                    }
                    // Ambiguous/non-existent local time (DST gap): fall to tomorrow.
                    None => tz
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
pub(crate) fn parse_weekday(token: &str) -> Option<chrono::Weekday> {
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
pub(crate) fn parse_clock(token: &str) -> Option<(u32, u32)> {
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
