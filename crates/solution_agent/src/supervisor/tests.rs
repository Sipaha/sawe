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
    assert_eq!(
        dir,
        root.join(".agents").join("abcd1234").join("supervisor")
    );
}

#[test]
fn session_log_path_and_append() {
    let tmp = tempfile::tempdir().unwrap();
    let path = session_log_path(tmp.path(), sid());
    assert_eq!(
        path,
        tmp.path()
            .join(".agents")
            .join("abcd1234")
            .join("session-log.md")
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
fn wipe_supervisor_memory_removes_the_three_files_and_tolerates_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(diary_path(dir), "diary").unwrap();
    std::fs::write(verdicts_path(dir), "{}\n").unwrap();
    std::fs::write(intent_path(dir), "intent").unwrap();
    // An unrelated file in the same dir must survive the wipe.
    let keep = dir.join("session-log.md");
    std::fs::write(&keep, "log").unwrap();

    wipe_supervisor_memory(dir);

    assert!(!diary_path(dir).exists());
    assert!(!verdicts_path(dir).exists());
    assert!(!intent_path(dir).exists());
    assert!(keep.exists(), "unrelated files are left untouched");

    // Idempotent: a second wipe with the files already gone is a silent
    // no-op (NotFound is swallowed), not a panic.
    wipe_supervisor_memory(dir);
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
        dropped: false,
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
        body.push_str(&format!(
            "{{\"n\":{i},\"pad\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}}\n"
        ));
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

#[test]
fn stats_excludes_dropped_records() {
    let mut dropped = mk(VerdictAction::Continue, 999);
    dropped.dropped = true;
    let recs = vec![
        mk(VerdictAction::Continue, 100),
        dropped,
        mk(VerdictAction::Compact, 50),
    ];
    let s = verdict_stats(&recs);
    assert_eq!(s.total, 2, "dropped verdict excluded from total");
    assert_eq!(
        s.by_action[VerdictAction::Continue as usize], 1,
        "only the acted Continue counts"
    );
    assert_eq!(s.by_action[VerdictAction::Compact as usize], 1);
    assert_eq!(s.total_tokens, 150, "dropped verdict's tokens excluded");
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
        dropped: false,
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
    assert!(!should_fire(
        true,
        &SupervisorStatus::Judging,
        true,
        0,
        999_000,
        60
    ));
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
        nonce: "noncevalue123".into(),
    };
    let out = build_judge_briefing(&ctx);
    assert!(out.contains("abcd1234"));
    assert!(out.contains("/sol/.agents/abcd1234/supervisor/diary.md"));
    assert!(out.contains("don't stop before tests pass"));
    assert!(out.contains("187,000 / 200,000 tokens (94%)"));
    // The `--nc` bridge command is fully materialized for the judge.
    assert!(out.contains("/path/to/sawe --nc /run/sol/mcp.sock"));
    // The verdict nonce reaches the briefing verbatim so the judge can echo it.
    assert!(out.contains("noncevalue123"));
    assert!(
        !out.contains("{DIARY_PATH}"),
        "all placeholders substituted"
    );
    assert!(!out.contains("{BRIDGE_BIN}"));
    assert!(!out.contains("{SOCKET_PATH}"));
    assert!(!out.contains("{VERDICT_NONCE}"));
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
        nonce: "n".into(),
    };
    let out = build_judge_briefing(&ctx);
    assert!(!out.contains("{CUSTOM_PROMPT_SECTION}"));
    assert!(!out.contains("{CONTEXT_USAGE_SECTION}"));
}

#[test]
fn classify_error_quota_vs_transient() {
    assert!(matches!(
        classify_judge_error("usage limit reached"),
        JudgeFailure::Quota
    ));
    assert!(matches!(
        classify_judge_error("Error: rate_limit_error"),
        JudgeFailure::Quota
    ));
    assert!(matches!(
        classify_judge_error("overloaded_error"),
        JudgeFailure::Transient
    ));
    assert!(matches!(
        classify_judge_error("connection reset"),
        JudgeFailure::Transient
    ));
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
    // Prose about rate-limiting / quotas / billing must NOT be mistaken for a
    // wall (finding #4) — else a real hang skips reconnect and supervision
    // dies as Stopped(Quota). These are the exact words an agent writes when
    // working ON such code.
    assert!(!is_usage_limit_error(
        "added rate limit handling; insufficient test coverage remains"
    ));
    assert!(!is_usage_limit_error(
        "checked the user's remaining credit and billing status in the invoice module"
    ));
    assert!(!is_usage_limit_error(
        "the quota field defaults to 100 when the billing plan is free"
    ));
    // The session-limit message must classify as Quota (was Transient).
    assert!(matches!(
        classify_judge_error(
            "You've hit your session limit · resets 8:20pm (Asia/Novosibirsk)"
        ),
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
    let now = chrono::Utc.with_ymd_and_hms(2026, 6, 29, 10, 0, 0).unwrap();
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
fn parse_reset_recent_past_resumes_now_not_tomorrow() {
    use chrono::TimeZone as _;
    // A session wall "resets 8:20pm (Novosibirsk)" read 5 min LATE (the stuck
    // watchdog only notices ~5 min after the wall printed). The limit reset
    // moments ago; rolling to tomorrow would over-park ~24 h (finding #6).
    // 8:20pm Novosibirsk (UTC+7) == 13:20 UTC; now = 13:25 UTC (== 8:25pm).
    let now = chrono::Utc.with_ymd_and_hms(2026, 6, 29, 13, 25, 0).unwrap();
    let got = parse_usage_limit_reset_ms(
        "You've hit your session limit · resets 8:20pm (Asia/Novosibirsk)",
        now.timestamp_millis(),
    )
    .expect("parse");
    // Resumes ~now (reset just passed), NOT tomorrow.
    assert_eq!(got, now.timestamp_millis());
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
