//! Tier-enforcing wrapper for [`McpServerTool`] (S-BAK).
//!
//! [`TierGuardTool<T>`] wraps an inner [`McpServerTool`] and rejects calls
//! whose registered tier exceeds the connecting caller's [`CallerCapabilities`].
//! The caps used for the check come from a process-global cell populated by
//! [`set_process_caps`] (set once during [`crate::lifecycle::start_server`]
//! from the [`crate::tier::BRIDGE_CAPS_ENV_VAR`] env value).
//!
//! Trade-off: the cell is process-global, not per-connection — every connection
//! that lands on this server sees the same caps. That's correct for the common
//! pattern in this fork (one `--nc` subprocess per ACP subagent, env value
//! stamped on the subprocess and propagated to its `nc` child) but it would
//! be wrong if the editor itself accepted multiple concurrent subagent
//! connections with different capability profiles. A future task
//! ([git-panel-plan §S-BAK-PER-CONN]) can either thread caps through
//! `serve_connection` (touches upstream `context_server`) or have `nc` send
//! them as a handshake notification on the socket — for now we accept the
//! coarseness.

use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use gpui::AsyncApp;

use crate::tier::{CallerCapabilities, ToolTier};

static PROCESS_CAPS: OnceLock<CallerCapabilities> = OnceLock::new();

/// Apply the process-global caller capabilities. Called once from
/// [`crate::lifecycle::start_server`] using the [`crate::tier::BRIDGE_CAPS_ENV_VAR`]
/// env value. Subsequent calls are silently ignored (`OnceLock`).
pub(crate) fn set_process_caps(caps: CallerCapabilities) {
    let _ = PROCESS_CAPS.set(caps);
}

/// Read the active capability profile. Defaults to
/// [`CallerCapabilities::SUBAGENT_DEFAULT`] (Write tier) when not yet set —
/// matches the spec rule "missing env = Write".
pub fn current_caps() -> CallerCapabilities {
    PROCESS_CAPS
        .get()
        .copied()
        .unwrap_or(CallerCapabilities::SUBAGENT_DEFAULT)
}

/// Wrap an [`McpServerTool`] with tier-check enforcement. The inner tool's
/// `Input`, `Output`, and `NAME` are passed through unchanged so the wire
/// protocol view is identical to the unwrapped tool.
pub struct TierGuardTool<T: McpServerTool + Clone> {
    inner: T,
    tier: ToolTier,
    /// S-SOL-PRT — optional extractor that pulls the (repo_path,
    /// branch, op_name) triple from the tool's typed input so the
    /// registry can run a [`branch_protection`]-style check before
    /// dispatch. `None` means the tool isn't tied to a single branch
    /// (e.g. read-only scans, repo-busy probes) and the protection
    /// check is skipped.
    protection: Option<std::sync::Arc<TypedExtractor<T>>>,
}

impl<T: McpServerTool + Clone> Clone for TierGuardTool<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            tier: self.tier,
            protection: self.protection.clone(),
        }
    }
}

/// Closure-shaped variant of [`BranchProtectionExtractor`] used
/// internally by [`TierGuardTool`]. Holding a typed extractor here
/// means `T::Input` doesn't need to implement `Serialize`.
pub(crate) type TypedExtractor<T> =
    dyn Fn(&<T as McpServerTool>::Input) -> Option<BranchProtectionHint> + Send + Sync;

impl<T: McpServerTool + Clone> TierGuardTool<T> {
    pub fn new(inner: T, tier: ToolTier) -> Self {
        Self {
            inner,
            tier,
            protection: None,
        }
    }

    /// Variant that registers a branch-protection extractor alongside
    /// the tier wrap. The extractor is invoked with the typed input on
    /// each call; it returns `None` to skip the check or `Some(hint)`
    /// with the repo+branch+op the dispatcher should evaluate.
    pub fn new_with_protection<F>(inner: T, tier: ToolTier, extractor: F) -> Self
    where
        F: Fn(&T::Input) -> Option<BranchProtectionHint> + Send + Sync + 'static,
    {
        Self {
            inner,
            tier,
            protection: Some(std::sync::Arc::new(extractor)),
        }
    }
}

impl<T> McpServerTool for TierGuardTool<T>
where
    T: McpServerTool + Clone,
{
    type Input = T::Input;
    type Output = T::Output;
    const NAME: &'static str = T::NAME;

    fn annotations(&self) -> context_server::types::ToolAnnotations {
        self.inner.annotations()
    }

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let caps = current_caps();
        if !self.tier.permits(caps.allowed_tier) {
            // Custom error code -32401 per S-BAK plan §P-4: tools rejected
            // for insufficient capability return a recognisable code distinct
            // from the standard JSON-RPC error codes.
            anyhow::bail!(
                "tool {} requires {:?} capability (caller has {:?}) [code=-32401]",
                T::NAME,
                self.tier,
                caps.allowed_tier
            );
        }

        // S-SOL-PRT — registry-level branch-protection enforcement.
        // Runs only for tools that registered an extractor; pure-read
        // tools (no `affects_branch`) bypass this entirely. The
        // `confirmed` flag is part of the [`BranchProtectionHint`]
        // returned by the extractor — extractors that don't surface a
        // confirmation field default to `false` (fail-closed for
        // `RequiresConfirmation`).
        if let Some(extractor) = self.protection.clone() {
            if let Some(hint) = extractor(&input) {
                let confirmed = hint.confirmed;
                let target = resolve_target(hint, cx).await;
                if let Some(target) = target
                    && let Some(decision) = evaluate_branch_protection(&target)
                {
                    apply_branch_protection_decision(T::NAME, confirmed, decision)?;
                }
            }
        }

        self.inner.run(input, cx).await
    }
}

async fn resolve_target(
    hint: BranchProtectionHint,
    cx: &mut AsyncApp,
) -> Option<BranchProtectionTarget> {
    let repo_path = if let Some(p) = hint.repo_path {
        Some(p)
    } else if let Some(id) = hint.repo_id {
        cx.update(|cx| resolve_repo_path(id, cx))
    } else {
        None
    }?;
    Some(BranchProtectionTarget {
        repo_path,
        branch: hint.branch,
        op_name: hint.op_name,
    })
}

/// Resolver that converts a `repo_id` (the wire-format `u64`
/// `RepositoryId`) to its working-directory path. Installed once by
/// `git_ui::init` so the editor_mcp crate doesn't need a downward dep
/// on `git_ui` / `project`.
pub type RepoPathResolver =
    Box<dyn Fn(u64, &mut gpui::App) -> Option<std::path::PathBuf> + Send + Sync + 'static>;

static REPO_PATH_RESOLVER: Mutex<Option<RepoPathResolver>> = Mutex::new(None);

pub fn set_repo_path_resolver(resolver: Option<RepoPathResolver>) {
    if let Ok(mut guard) = REPO_PATH_RESOLVER.lock() {
        *guard = resolver;
    }
}

fn resolve_repo_path(id: u64, cx: &mut gpui::App) -> Option<std::path::PathBuf> {
    let guard = REPO_PATH_RESOLVER.lock().ok()?;
    let resolver = guard.as_ref()?;
    resolver(id, cx)
}

/// Triple a branch-protection extractor returns. The dispatcher hands
/// it to the registered [`BranchProtectionChecker`] callback.
#[derive(Debug, Clone)]
pub struct BranchProtectionTarget {
    pub repo_path: std::path::PathBuf,
    pub branch: String,
    pub op_name: &'static str,
}

/// Hint a tool's `affects_branch` extractor pulls from its typed
/// input. Carries enough information for the registry to construct a
/// [`BranchProtectionTarget`] — the registry itself resolves
/// `repo_id` to a working directory via the active workspace when a
/// path isn't provided directly.
#[derive(Debug, Clone)]
pub struct BranchProtectionHint {
    /// Either a literal `repo_path` or a `repo_id` that the registry
    /// resolves through the App context. At least one MUST be set.
    pub repo_path: Option<std::path::PathBuf>,
    pub repo_id: Option<u64>,
    /// Branch the op targets. Tools that operate on "current branch"
    /// fill this from the payload at register time when the payload
    /// doesn't surface it directly — typical pattern is "field
    /// `target_branch`" or "infer from `name` for delete-branch".
    pub branch: String,
    /// Stable op identifier, matching the names
    /// `solutions::branch_protection::check` keys off (e.g.
    /// `"force_push"`, `"reset"`, `"merge"`).
    pub op_name: &'static str,
    /// Whether the call payload carries `confirmed: true` (or the
    /// equivalent typed field). Subagent flows that consult the
    /// `RequiresConfirmation` decision honour this — `false` is
    /// fail-closed (rejects unconfirmed). Defaults to `false` for
    /// extractors that don't surface the confirmation flag.
    pub confirmed: bool,
}

/// Three-state branch-protection decision — mirrors
/// `solutions::branch_protection::Decision` so this crate doesn't have
/// to depend on `solutions`. The hook owner (typically `solutions::init`)
/// converts between the two.
#[derive(Debug, Clone)]
pub enum BranchProtectionDecision {
    Allowed,
    RequiresConfirmation { reason: String },
    Forbidden { reason: String },
}

/// Callback that resolves a [`BranchProtectionTarget`] to a
/// [`BranchProtectionDecision`] — registered once at process start.
pub type BranchProtectionChecker =
    Box<dyn Fn(&BranchProtectionTarget) -> BranchProtectionDecision + Send + Sync + 'static>;

static BRANCH_PROTECTION_CHECKER: Mutex<Option<BranchProtectionChecker>> = Mutex::new(None);

/// Install the registry-level branch-protection callback. Idempotent
/// for reset-to-`None`; otherwise replaces the previously-registered
/// checker.
pub fn set_branch_protection_checker(checker: Option<BranchProtectionChecker>) {
    if let Ok(mut guard) = BRANCH_PROTECTION_CHECKER.lock() {
        *guard = checker;
    }
}

/// Evaluate the registered branch-protection checker against the
/// provided target. Returns `None` when no checker has been installed
/// yet (e.g. early init or in unit tests that don't load
/// `solutions::init`). Holds the inner mutex only for the duration of
/// the synchronous decision call — callers must not hold any other
/// lock while invoking this.
fn evaluate_branch_protection(target: &BranchProtectionTarget) -> Option<BranchProtectionDecision> {
    let guard = BRANCH_PROTECTION_CHECKER.lock().ok()?;
    let checker = guard.as_ref()?;
    Some(checker(target))
}

fn apply_branch_protection_decision(
    tool_name: &str,
    confirmed: bool,
    decision: BranchProtectionDecision,
) -> Result<()> {
    match decision {
        BranchProtectionDecision::Allowed => Ok(()),
        BranchProtectionDecision::Forbidden { reason } => {
            anyhow::bail!("tool {tool_name} refused by branch protection: {reason} [code=-32402]");
        }
        BranchProtectionDecision::RequiresConfirmation { reason } => {
            // Subagent payloads must include `confirmed: true` for ops
            // that require confirmation; in-process callers go through
            // their own modal and shouldn't hit this path. Unconfirmed
            // → reject with the same -32402 code so callers can
            // distinguish branch-protection refusals from generic
            // tool errors.
            if confirmed {
                Ok(())
            } else {
                anyhow::bail!(
                    "tool {tool_name} requires confirmation: {reason}; \
                     re-call with `confirmed: true` [code=-32402]"
                );
            }
        }
    }
}

// Per-tool integration tests for TierGuardTool would require a fresh process
// per test (the caps cell is a `OnceLock`); the tier-permit logic itself is
// covered by unit tests in `tier.rs`. End-to-end enforcement is exercised
// indirectly by any e2e test that connects through the `--nc` bridge with a
// non-default `SAWE_MCP_BRIDGE_CAPS` value.
