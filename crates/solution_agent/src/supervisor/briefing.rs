//! Construction of the ephemeral judge/auditor briefing (the single user turn
//! that instructs the reviewing session), the supervisor system prompt, and the
//! single-use verdict nonces that authenticate a returned verdict. GPUI-free.

/// Inputs to [`build_judge_briefing`]. All paths are pre-resolved by the
/// caller (the store, which knows the solution root) so this builder stays
/// pure and unit-testable.
pub struct JudgeBriefingContext {
    pub supervised_session_id: String,
    pub diary_path: String,
    pub verdicts_path: String,
    /// Path to the durable user-intent record the judge maintains (see
    /// [`super::intent_path`]).
    pub intent_path: String,
    pub compact_dir: String,
    pub custom_prompt: Option<String>,
    /// Human-readable context-window fullness of the supervised session at
    /// spawn time (e.g. `"187,000 / 200,000 tokens (94%)"`), injected so the
    /// judge can weigh a `compact` verdict without an extra round-trip. `None`
    /// when usage is unknown (cold session, no live token reading yet).
    pub context_usage: Option<String>,
    /// Host-owned trigger and snapshot details; separate from transcript evidence.
    pub observation_context: Option<String>,
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
    /// Single-use credential minted for THIS briefing (see [`new_verdict_nonce`]).
    /// The judge/auditor must echo it back in its `supervisor_verdict` /
    /// `supervisor_audit_verdict` call; the store rejects any verdict whose nonce
    /// doesn't match the in-flight ephemeral session's stored nonce. Any other
    /// client on the per-solution socket lacks it, so it can't forge a verdict.
    pub nonce: String,
}

const JUDGE_INSTRUCTIONS: &str = include_str!("../../resources/supervisor_judge_instructions.md");
const AUDIT_INSTRUCTIONS: &str = include_str!("../../resources/supervisor_audit_instructions.md");

/// System prompt for the ephemeral judge/auditor sessions, appended INSTEAD of
/// the solution's default worker system prompt. The default prompt frames the
/// session as a worker ("You are working inside a Solution… run build/test/git…"),
/// which can pull the judge into doing the task instead of judging it. This
/// override keeps Claude's standard tool-using behaviour but re-frames the
/// session as a read-only outside evaluator whose only output is a verdict tool
/// call. The per-turn briefing carries the concrete instructions.
pub const SUPERVISOR_SYSTEM_PROMPT: &str = "\
You are an independent Supervisor evaluating another AI coding session — you are \
NOT a worker on its task. The operator enabled supervision to keep authorized work \
progressing autonomously. Ask the operator only for indispensable input, and first \
check whether independent TODOs can proceed without that answer. Do NOT write or edit code, run the task, or make git \
commits. Your sole job is to read the supervised session and its artifacts, then \
issue exactly ONE verdict. You reach the editor (to read the conversation and to \
submit your verdict) by piping JSON-RPC through the `--nc` socket bridge from \
an available shell tool. Do not assume any provider-specific tool names or \
discover alternate editor tools; use the supplied bridge. The first message gives you the exact bridge command and \
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
    let runtime_limits = runtime_limits_section();
    let observation_section = ctx
        .observation_context
        .as_ref()
        .map_or_else(String::new, |context| {
            format!("## Why this review started\n\n{context}\n")
        });
    let context_section = match &ctx.context_usage {
        Some(usage) => format!("## Context-window fullness (right now)\n\n{usage}\n"),
        None => String::new(),
    };
    // Keep human-readable paths separate from arguments in the executable
    // example: a solution or binary path may contain spaces or apostrophes.
    let bridge_shell = crate::prompt_template::quote_shell_argument(&ctx.bridge_bin);
    let socket_shell = crate::prompt_template::quote_shell_argument(&ctx.socket_path);
    let replacements = [
        ("{RUNTIME_LIMITS_SECTION}", runtime_limits.as_str()),
        (
            "{OBSERVATION_CONTEXT_SECTION}",
            observation_section.as_str(),
        ),
        ("{BRIDGE_BIN_SHELL}", bridge_shell.as_str()),
        ("{SOCKET_PATH_SHELL}", socket_shell.as_str()),
        (
            "{SUPERVISED_SESSION_ID}",
            ctx.supervised_session_id.as_str(),
        ),
        ("{DIARY_PATH}", ctx.diary_path.as_str()),
        ("{VERDICTS_PATH}", ctx.verdicts_path.as_str()),
        ("{INTENT_PATH}", ctx.intent_path.as_str()),
        ("{COMPACT_DIR}", ctx.compact_dir.as_str()),
        ("{BRIDGE_BIN}", ctx.bridge_bin.as_str()),
        ("{SOCKET_PATH}", ctx.socket_path.as_str()),
        ("{VERDICT_NONCE}", ctx.nonce.as_str()),
        ("{CONTEXT_USAGE_SECTION}", context_section.as_str()),
        ("{CUSTOM_PROMPT_SECTION}", custom_section.as_str()),
    ];
    crate::prompt_template::render(template, &replacements)
}

// Share the values used by the state machine so prompt prose cannot drift
// into a second, contradictory implementation of these deterministic limits.
fn runtime_limits_section() -> String {
    use super::{
        AUDIT_EVERY, DEFAULT_WAIT_SECS, MAX_CONSECUTIVE_CONTINUES, MAX_WAIT_SECS, MIN_WAIT_SECS,
    };
    format!(
        "## Limits enforced by the editor\n\n\
         Consecutive `continue`/`ask_agent` verdicts escalate to the human at \
         {MAX_CONSECUTIVE_CONTINUES}; every {AUDIT_EVERY} triggers an independent audit \
         before that cap. `wait_seconds` defaults to {DEFAULT_WAIT_SECS} and is \
         clamped to {MIN_WAIT_SECS}–{MAX_WAIT_SECS}. The editor owns these counters \
         and timers; choose the action and estimate, do not maintain duplicate counters.\n"
    )
}

/// Mint a fresh single-use credential for one judge/auditor briefing. It rides
/// in the briefing plaintext ([`JudgeBriefingContext::nonce`], `{VERDICT_NONCE}`)
/// — the only side channel the ephemeral `claude` subprocess has — and must be
/// echoed by the `supervisor_verdict` / `supervisor_audit_verdict` call, which
/// the store checks against the in-flight session's stored nonce before acting.
/// 32 chars over a 36-symbol alphabet ≈ 165 bits: unguessable by another client
/// racing on the same per-solution socket. Same rejection-sampling scheme as
/// [`crate::model::SolutionSessionId::new`] so the distribution stays uniform.
pub fn new_verdict_nonce() -> String {
    use rand::RngCore as _;
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    const LEN: usize = 32;
    let mut rng = rand::rng();
    let mut buf = [0u8; 1];
    let mut out = String::with_capacity(LEN);
    for _ in 0..LEN {
        loop {
            rng.fill_bytes(&mut buf);
            let x = buf[0] as usize;
            if x < 252 {
                out.push(ALPHABET[x % 36] as char);
                break;
            }
        }
    }
    out
}

/// Constant-time-ish equality for a verdict nonce, so a forged-verdict attempt
/// on the local socket can't byte-by-byte time its way to the real credential.
/// The threat model is low (local socket, own agent) but the check is cheap and
/// removes the trivial short-circuit `==` leak. Length mismatch short-circuits
/// (nonces are fixed-length, so an equal-length compare is the only case that
/// matters).
pub fn verdict_nonce_matches(expected: &str, provided: &str) -> bool {
    let (a, b) = (expected.as_bytes(), provided.as_bytes());
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
