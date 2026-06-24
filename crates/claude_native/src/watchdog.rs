//! Silence watchdog for an in-flight `claude` turn.
//!
//! `claude` can wedge mid-turn — a tool call that never returns, a model that
//! stops emitting deltas — without ever sending the turn-ending `result`. The
//! deterministic turn-end (Phase 5) fixes the common case, but a process that
//! is *alive yet silent* would otherwise hang Running forever. The [`Watchdog`]
//! arms a silence timer (default 15 min) while a prompt is in flight; if no
//! output arrives within the window it asks an [`Analyzer`] whether the process
//! is genuinely hung. Only a [`Verdict::Hung`] triggers recovery (the same
//! kill + `--resume` respawn the Stop-escalation uses); `Working` or `Unknown`
//! re-arms the timer.
//!
//! The real analyzer spawns a one-shot headless `claude -p --output-format
//! json` (no MCP, hard 60 s timeout) and parses a `{ "verdict": ... }` reply.
//! Critically, ANY analyzer failure — launch error, timeout, unparseable
//! output — maps to [`Verdict::Unknown`], NEVER [`Verdict::Hung`]: a broken
//! analyzer must never kill a healthy turn.
//!
//! The clock is the GPUI executor's (virtual under test), so the silence window
//! and analyzer timeout are injectable and tests run deterministically without
//! a live `claude`.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use futures::FutureExt as _;
use futures::io::AsyncReadExt as _;
use gpui::{AsyncApp, Task};
use scheduler::Instant;
use util::ResultExt as _;

/// The verdict the analyzer reaches about a silent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The process is wedged and will not progress — recover it.
    Hung,
    /// The process is legitimately busy (long tool call, slow model) — wait.
    Working,
    /// The analyzer could not decide (launch/parse/timeout failure). Treated
    /// like `Working`: re-arm, never recover. This is the do-nothing-on-fail
    /// guarantee — a broken analyzer must not kill a healthy turn.
    Unknown,
}

/// Context handed to the analyzer so it can judge whether a silent turn is
/// hung. Owned and cheap to format into the `claude -p` prompt. Kept
/// deliberately simple — the heuristics live in the prompt text, not here.
#[derive(Debug, Clone)]
pub struct AnalyzerContext {
    /// How long the turn has been silent (no output) when the analyzer fired.
    pub silence_duration: Duration,
    /// OS process id of the wedged `claude`, for the analyzer to reason about
    /// (e.g. "is pid N still running / consuming CPU").
    pub process_id: Option<u32>,
    /// The most recent translated events the user saw before silence began,
    /// newest last. The analyzer summarizes these into the prompt.
    pub recent_events: Vec<String>,
    /// The id of a tool call that is still in flight (issued, no result), if
    /// any — the most common wedge is a tool call that never returns.
    pub pending_tool_use: Option<String>,
}

/// Decides whether a silent turn is hung. Injected so tests supply a stub
/// verdict and production uses [`ClaudeAnalyzer`] (a one-shot `claude -p`).
pub trait Analyzer {
    fn assess(&self, context: AnalyzerContext, cx: &mut AsyncApp) -> Task<Verdict>;
}

/// Recovery action invoked exactly once when the analyzer returns `Hung`. In
/// production this calls `ClaudeNativeConnection::recover_session`; tests count
/// invocations through a shared cell.
pub type RecoveryCallback = Rc<dyn Fn(&mut AsyncApp)>;

/// Per-session silence watchdog. One is created when a prompt starts and
/// dropped (cancelling its timer task) when the prompt resolves.
pub struct Watchdog {
    _timer: Task<()>,
}

impl Watchdog {
    /// Arm the watchdog for an in-flight turn. `last_output` is the shared cell
    /// the update-pump bumps on every message (so any output resets the silence
    /// timer); `silence_window` is the quiet period that triggers an analysis
    /// (default 15 min in production, tiny under test). On a `Hung` verdict the
    /// watchdog invokes `recovery` once and stops; on `Working`/`Unknown` it
    /// re-arms.
    pub fn arm(
        last_output: Rc<Cell<Instant>>,
        silence_window: Duration,
        analyzer: Rc<dyn Analyzer>,
        context_provider: Rc<dyn Fn() -> AnalyzerContext>,
        recovery: RecoveryCallback,
        cx: &mut AsyncApp,
    ) -> Self {
        let timer = cx.spawn(async move |cx| {
            loop {
                // Sleep until the silence window *could* have elapsed measured
                // from the last seen output. Re-checking `last_output` after the
                // sleep handles output that arrived while we slept: we re-arm
                // for the remaining quiet time instead of analyzing prematurely.
                let now = cx.background_executor().now();
                let elapsed = now.saturating_duration_since(last_output.get());
                if elapsed < silence_window {
                    let remaining = silence_window - elapsed;
                    cx.background_executor().timer(remaining).await;
                    continue;
                }

                let context = context_provider();
                let verdict = analyzer.assess(context, cx).await;
                match verdict {
                    Verdict::Hung => {
                        recovery(cx);
                        return;
                    }
                    // Re-arm: reset the baseline so we wait a fresh window
                    // rather than spinning on the now-stale `last_output`.
                    Verdict::Working | Verdict::Unknown => {
                        last_output.set(cx.background_executor().now());
                    }
                }
            }
        });
        Self { _timer: timer }
    }
}

/// Production analyzer: a one-shot headless `claude -p --output-format json`
/// with NO `--mcp-config` and a hard timeout. It is a separate, short-lived
/// process from the wedged session's `claude` — a second opinion, not the
/// patient. Any failure path returns [`Verdict::Unknown`].
pub struct ClaudeAnalyzer {
    binary: PathBuf,
    timeout: Duration,
}

/// Hard ceiling on the analyzer subprocess. The watchdog itself is patient
/// (15 min); the *analysis* must be quick or it is worthless.
const DEFAULT_ANALYZER_TIMEOUT: Duration = Duration::from_secs(60);

impl ClaudeAnalyzer {
    pub fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            timeout: DEFAULT_ANALYZER_TIMEOUT,
        }
    }

    /// The structured prompt embedding the wedged turn's context. The model is
    /// asked for a one-word verdict we then parse.
    fn build_prompt(context: &AnalyzerContext) -> String {
        let pending = context
            .pending_tool_use
            .as_deref()
            .unwrap_or("(none in flight)");
        let recent = if context.recent_events.is_empty() {
            "(no recent output)".to_string()
        } else {
            context.recent_events.join("\n")
        };
        format!(
            "You are diagnosing whether another `claude` process is hung.\n\
             It produced no output for {silence} seconds.\n\
             Its process id is {pid}.\n\
             A tool call still in flight: {pending}.\n\
             The last events the user saw:\n{recent}\n\n\
             Reply with ONLY a JSON object {{\"verdict\":\"hung\"}} if the \
             process is wedged and will not progress, or \
             {{\"verdict\":\"working\"}} if it is legitimately busy.",
            silence = context.silence_duration.as_secs(),
            pid = context
                .process_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        )
    }

    /// Parse the analyzer subprocess's stdout. The headless `--output-format
    /// json` reply wraps the model's text under a `result` field; we look for a
    /// `verdict` token anywhere in the payload. Anything we cannot read as a
    /// clear "hung" becomes `Unknown` (never `Hung` by accident — only an
    /// explicit hung verdict kills a turn, and even then through this narrow
    /// match).
    fn parse_verdict(stdout: &str) -> Verdict {
        let Some(value) = serde_json::from_str::<serde_json::Value>(stdout).log_err() else {
            return Verdict::Unknown;
        };
        // The headless wrapper nests the model's answer under `result` (a
        // string). Fall back to scanning the whole payload so a future shape
        // change degrades to a substring search rather than a misfire.
        let haystack = value
            .get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| value.to_string())
            .to_ascii_lowercase();
        if haystack.contains("\"verdict\"") {
            if haystack.contains("hung") {
                return Verdict::Hung;
            }
            if haystack.contains("working") {
                return Verdict::Working;
            }
        }
        Verdict::Unknown
    }

    /// Spawn the one-shot analyzer, returning its stdout or an error. Kept
    /// separate so `assess` can map every error to `Unknown` in one place.
    async fn run(
        binary: PathBuf,
        prompt: String,
        timeout: Duration,
        cx: &AsyncApp,
    ) -> Result<String> {
        let mut command = smol::process::Command::new(&binary);
        command
            .arg("-p")
            .arg(&prompt)
            .arg("--output-format")
            .arg("json")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = command.spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("analyzer stdout missing"))?;

        let read = async move {
            let mut buffer = String::new();
            stdout.read_to_string(&mut buffer).await?;
            anyhow::Ok(buffer)
        };
        let timer = cx.background_executor().timer(timeout);

        futures::select_biased! {
            output = read.fuse() => output,
            _ = timer.fuse() => {
                child.kill().log_err();
                Err(anyhow::anyhow!("analyzer timed out after {timeout:?}"))
            }
        }
    }
}

impl Analyzer for ClaudeAnalyzer {
    fn assess(&self, context: AnalyzerContext, cx: &mut AsyncApp) -> Task<Verdict> {
        let binary = self.binary.clone();
        let timeout = self.timeout;
        let prompt = Self::build_prompt(&context);
        cx.spawn(
            async move |cx| match Self::run(binary, prompt, timeout, cx).await {
                Ok(stdout) => Self::parse_verdict(&stdout),
                // Launch / read / timeout failure: do nothing (never `Hung`).
                Err(error) => {
                    log::warn!("claude_native: analyzer failed, treating as Unknown: {error}");
                    Verdict::Unknown
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell as StdCell;

    /// Stub analyzer returning a fixed verdict and recording its call count.
    struct StubAnalyzer {
        verdict: Verdict,
        calls: Rc<StdCell<usize>>,
    }

    impl Analyzer for StubAnalyzer {
        fn assess(&self, _context: AnalyzerContext, _cx: &mut AsyncApp) -> Task<Verdict> {
            self.calls.set(self.calls.get() + 1);
            Task::ready(self.verdict)
        }
    }

    fn test_context() -> AnalyzerContext {
        AnalyzerContext {
            silence_duration: Duration::from_secs(900),
            process_id: Some(4242),
            recent_events: vec!["assistant: working on it".to_string()],
            pending_tool_use: Some("toolu_1".to_string()),
        }
    }

    /// A tiny silence window so the virtual clock reaches it in a few ticks.
    const WINDOW: Duration = Duration::from_millis(30);

    fn arm_watchdog(
        verdict: Verdict,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Watchdog,
        Rc<Cell<Instant>>,
        Rc<StdCell<usize>>,
        Rc<StdCell<usize>>,
    ) {
        let now = cx.executor().now();
        let last_output = Rc::new(Cell::new(now));
        let analyzer_calls = Rc::new(StdCell::new(0usize));
        let recovery_calls = Rc::new(StdCell::new(0usize));

        let analyzer: Rc<dyn Analyzer> = Rc::new(StubAnalyzer {
            verdict,
            calls: analyzer_calls.clone(),
        });
        let recovery: RecoveryCallback = {
            let recovery_calls = recovery_calls.clone();
            Rc::new(move |_cx: &mut AsyncApp| {
                recovery_calls.set(recovery_calls.get() + 1);
            })
        };
        let context_provider: Rc<dyn Fn() -> AnalyzerContext> = Rc::new(test_context);

        let watchdog = cx.update(|cx| {
            let mut async_cx = cx.to_async();
            Watchdog::arm(
                last_output.clone(),
                WINDOW,
                analyzer,
                context_provider,
                recovery,
                &mut async_cx,
            )
        });
        (watchdog, last_output, analyzer_calls, recovery_calls)
    }

    #[gpui::test]
    async fn hung_verdict_invokes_recovery_once(cx: &mut gpui::TestAppContext) {
        let (_watchdog, _last_output, analyzer_calls, recovery_calls) =
            arm_watchdog(Verdict::Hung, cx);

        cx.executor().advance_clock(WINDOW * 2);
        cx.run_until_parked();

        assert_eq!(analyzer_calls.get(), 1, "analyzer should fire once");
        assert_eq!(
            recovery_calls.get(),
            1,
            "Hung must invoke recovery exactly once"
        );

        // Watchdog stopped after recovery: advancing further must not re-fire.
        cx.executor().advance_clock(WINDOW * 4);
        cx.run_until_parked();
        assert_eq!(recovery_calls.get(), 1, "recovery must not fire again");
    }

    #[gpui::test]
    async fn working_verdict_rearms_without_recovery(cx: &mut gpui::TestAppContext) {
        let (_watchdog, _last_output, analyzer_calls, recovery_calls) =
            arm_watchdog(Verdict::Working, cx);

        cx.executor().advance_clock(WINDOW * 2);
        cx.run_until_parked();
        assert_eq!(recovery_calls.get(), 0, "Working must not recover");
        assert!(analyzer_calls.get() >= 1, "analyzer should have fired");

        // Re-armed: a second window elapsing fires the analyzer again.
        let calls_after_first = analyzer_calls.get();
        cx.executor().advance_clock(WINDOW * 2);
        cx.run_until_parked();
        assert!(
            analyzer_calls.get() > calls_after_first,
            "Working should re-arm the window"
        );
        assert_eq!(recovery_calls.get(), 0, "still no recovery");
    }

    #[gpui::test]
    async fn unknown_verdict_rearms_without_recovery(cx: &mut gpui::TestAppContext) {
        let (_watchdog, _last_output, analyzer_calls, recovery_calls) =
            arm_watchdog(Verdict::Unknown, cx);

        cx.executor().advance_clock(WINDOW * 2);
        cx.run_until_parked();
        assert_eq!(recovery_calls.get(), 0, "Unknown must not recover");
        assert!(analyzer_calls.get() >= 1);

        let calls_after_first = analyzer_calls.get();
        cx.executor().advance_clock(WINDOW * 2);
        cx.run_until_parked();
        assert!(
            analyzer_calls.get() > calls_after_first,
            "Unknown should re-arm the window (do-nothing-on-fail)"
        );
        assert_eq!(recovery_calls.get(), 0);
    }

    #[gpui::test]
    async fn output_before_window_resets_timer(cx: &mut gpui::TestAppContext) {
        let (_watchdog, last_output, analyzer_calls, recovery_calls) =
            arm_watchdog(Verdict::Hung, cx);

        // Advance most of the window, then "receive output" by bumping the cell
        // — exactly what the pump does on every message.
        cx.executor().advance_clock(WINDOW / 2);
        cx.run_until_parked();
        last_output.set(cx.executor().now());

        // Advancing the rest of the original window must NOT trigger analysis,
        // because the bump pushed the deadline out.
        cx.executor()
            .advance_clock(WINDOW / 2 + Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(analyzer_calls.get(), 0, "output should reset the timer");
        assert_eq!(recovery_calls.get(), 0);

        // Once a full quiet window passes after the bump, it does fire.
        cx.executor().advance_clock(WINDOW * 2);
        cx.run_until_parked();
        assert_eq!(analyzer_calls.get(), 1, "fires once the new window elapses");
        assert_eq!(recovery_calls.get(), 1);
    }

    #[test]
    fn parse_verdict_maps_failures_to_unknown() {
        // Not JSON at all.
        assert_eq!(ClaudeAnalyzer::parse_verdict("not json"), Verdict::Unknown);
        // JSON but no verdict.
        assert_eq!(
            ClaudeAnalyzer::parse_verdict(r#"{"result":"I am unsure"}"#),
            Verdict::Unknown
        );
        // Explicit verdicts.
        assert_eq!(
            ClaudeAnalyzer::parse_verdict(r#"{"result":"{\"verdict\":\"hung\"}"}"#),
            Verdict::Hung
        );
        assert_eq!(
            ClaudeAnalyzer::parse_verdict(r#"{"result":"{\"verdict\":\"working\"}"}"#),
            Verdict::Working
        );
    }
}
