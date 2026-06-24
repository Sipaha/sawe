//! Tool registry: holds boxed registration callbacks until `start_server`
//! drains them and applies to the live `McpServer`.
use crate::tier::ToolTier;
use crate::tier_guard::{BranchProtectionHint, TierGuardTool};
use context_server::listener::{McpServer, McpServerTool};
use gpui::{App, Global};
use std::cell::RefCell;
use std::collections::HashMap;

type Registration = Box<dyn FnOnce(&mut McpServer) + 'static>;

#[derive(Default)]
pub(crate) struct Registry {
    pending: RefCell<Vec<Registration>>,
    /// Wire-protocol tool name → declared tier. Tools registered via the
    /// legacy [`register_tool`] (no tier metadata) don't appear here; lookups
    /// against this map fall back to [`ToolTier::Destructive`] in
    /// [`tier_for`] — fail-safe migration policy from S-BAK.
    tool_tiers: RefCell<HashMap<&'static str, ToolTier>>,
    started: RefCell<bool>,
}

impl Global for Registry {}

pub fn init(cx: &mut App) {
    if cx.try_global::<Registry>().is_none() {
        cx.set_global(Registry::default());
        register_builtin_tools(cx);
    }
}

pub fn register_tool<F>(cx: &mut App, registration: F)
where
    F: FnOnce(&mut McpServer) + 'static,
{
    // Auto-init the Registry on first use so domain crates don't have to
    // worry about init order relative to editor_mcp::init.
    init(cx);
    let registry = cx.global::<Registry>();
    if *registry.started.borrow() {
        debug_assert!(false, "register_tool called after start_server");
        log::error!("editor_mcp: register_tool called after start_server — tool not registered");
        return;
    }
    registry.pending.borrow_mut().push(Box::new(registration));
}

/// Register a tool with an explicit capability tier. New tools (and migrated
/// existing tools) use this entry point. The legacy [`register_tool`] is kept
/// for backward compatibility — its tools default to
/// [`ToolTier::Destructive`] in [`tier_for`] and will be rejected for
/// subagent callers until migrated.
///
/// `name` must be the same wire-protocol name the tool registers itself
/// under (e.g. `"editor.git.log"`).
pub fn register_tool_with_tier<F>(cx: &mut App, name: &'static str, tier: ToolTier, registration: F)
where
    F: FnOnce(&mut McpServer) + 'static,
{
    init(cx);
    let registry = cx.global::<Registry>();
    if *registry.started.borrow() {
        debug_assert!(false, "register_tool_with_tier called after start_server");
        log::error!(
            "editor_mcp: register_tool_with_tier(\"{name}\") called after start_server — tool not registered"
        );
        return;
    }
    if let Some(prev) = registry.tool_tiers.borrow_mut().insert(name, tier) {
        debug_assert!(false, "duplicate tier registration for tool \"{name}\"");
        log::warn!("editor_mcp: tool \"{name}\" tier overwritten ({prev:?} -> {tier:?})",);
    }
    registry.pending.borrow_mut().push(Box::new(registration));
}

/// Register a typed tool wrapped in [`TierGuardTool`] so the declared tier
/// is enforced at dispatch time — caller capabilities below the tool's tier
/// produce a `-32401`-coded error before [`McpServerTool::run`] runs. Use
/// this for new and migrated tools; the legacy [`register_tool_with_tier`]
/// path declares the tier in metadata but does NOT enforce it at dispatch
/// (kept while the existing fleet of tools is migrated incrementally).
pub fn register_typed_tool_with_tier<T>(cx: &mut App, tier: ToolTier, tool: T)
where
    T: McpServerTool + Clone + 'static,
{
    init(cx);
    let registry = cx.global::<Registry>();
    if *registry.started.borrow() {
        debug_assert!(
            false,
            "register_typed_tool_with_tier called after start_server"
        );
        log::error!(
            "editor_mcp: register_typed_tool_with_tier(\"{}\") called after start_server — tool not registered",
            T::NAME
        );
        return;
    }
    if let Some(prev) = registry.tool_tiers.borrow_mut().insert(T::NAME, tier) {
        debug_assert!(
            false,
            "duplicate tier registration for tool \"{}\"",
            T::NAME
        );
        log::warn!(
            "editor_mcp: tool \"{}\" tier overwritten ({prev:?} -> {tier:?})",
            T::NAME,
        );
    }
    let registration: Registration = Box::new(move |server: &mut McpServer| {
        server.add_tool(TierGuardTool::new(tool, tier));
    });
    registry.pending.borrow_mut().push(registration);
}

/// Register a typed tool with both tier enforcement (S-BAK) and a
/// branch-protection extractor (S-SOL-PRT). The extractor runs against
/// the JSON-serialised input on each call; if it returns
/// `Some(target)`, the registry consults the global branch-protection
/// checker (installed by `solutions::init`) and rejects the call when
/// the decision is `Forbidden` or `RequiresConfirmation` without
/// `confirmed: true`. Tools that don't operate on a single branch use
/// the simpler [`register_typed_tool_with_tier`] entry point.
pub fn register_typed_tool_with_protection<T, F>(
    cx: &mut App,
    tier: ToolTier,
    tool: T,
    extractor: F,
) where
    T: context_server::listener::McpServerTool + Clone + 'static,
    F: Fn(&T::Input) -> Option<BranchProtectionHint> + Send + Sync + 'static,
{
    init(cx);
    let registry = cx.global::<Registry>();
    if *registry.started.borrow() {
        debug_assert!(
            false,
            "register_typed_tool_with_protection called after start_server"
        );
        log::error!(
            "editor_mcp: register_typed_tool_with_protection(\"{}\") called after start_server — tool not registered",
            T::NAME
        );
        return;
    }
    if let Some(prev) = registry.tool_tiers.borrow_mut().insert(T::NAME, tier) {
        debug_assert!(
            false,
            "duplicate tier registration for tool \"{}\"",
            T::NAME
        );
        log::warn!(
            "editor_mcp: tool \"{}\" tier overwritten ({prev:?} -> {tier:?})",
            T::NAME,
        );
    }
    let registration: Registration = Box::new(move |server: &mut McpServer| {
        server.add_tool(TierGuardTool::new_with_protection(tool, tier, extractor));
    });
    registry.pending.borrow_mut().push(registration);
}

/// Look up the declared tier for `tool_name`. Returns [`ToolTier::Destructive`]
/// for any tool that was registered without an explicit tier — fail-safe
/// default per S-BAK migration policy.
pub fn tier_for(cx: &App, tool_name: &str) -> ToolTier {
    cx.try_global::<Registry>()
        .and_then(|r| r.tool_tiers.borrow().get(tool_name).copied())
        .unwrap_or(ToolTier::Destructive)
}

pub(crate) fn drain(cx: &mut App) -> Vec<Registration> {
    let registry = cx.global::<Registry>();
    std::mem::take(&mut *registry.pending.borrow_mut())
}

pub(crate) fn mark_started(cx: &mut App) {
    let registry = cx.global::<Registry>();
    *registry.started.borrow_mut() = true;
}

#[cfg(test)]
pub(crate) fn pending_count(cx: &App) -> usize {
    cx.global::<Registry>().pending.borrow().len()
}

pub(crate) fn register_builtin_tools(cx: &mut App) {
    register_tool_with_tier(cx, "editor.capabilities", ToolTier::ReadOnly, |server| {
        server.add_tool(crate::tools::capabilities::CapabilitiesTool);
    });
    register_tool_with_tier(cx, "editor.get_operation", ToolTier::ReadOnly, |server| {
        server.add_tool(crate::tools::operations::GetOperationTool);
    });
    register_tool_with_tier(cx, "editor.cancel_operation", ToolTier::Write, |server| {
        server.add_tool(crate::tools::operations::CancelOperationTool);
    });
    register_tool_with_tier(cx, "editor.subscribe", ToolTier::ReadOnly, |server| {
        server.add_tool(crate::tools::subscribe::SubscribeTool);
    });
    register_tool_with_tier(cx, "editor.unsubscribe", ToolTier::ReadOnly, |server| {
        server.add_tool(crate::tools::subscribe::UnsubscribeTool);
    });
    register_tool_with_tier(
        cx,
        "editor.list_subscriptions",
        ToolTier::ReadOnly,
        |server| {
            server.add_tool(crate::tools::subscribe::ListSubscriptionsTool);
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn registry_collects_registrations(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init(cx);
            let baseline = pending_count(cx);
            register_tool(cx, |_server| {
                // captures, doesn't need to actually do anything
            });
            register_tool(cx, |_server| {});
            assert_eq!(pending_count(cx), baseline + 2);
        });
    }

    #[gpui::test]
    async fn drain_removes_pending(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init(cx);
            let baseline = pending_count(cx);
            register_tool(cx, |_| {});
            register_tool(cx, |_| {});
            let drained = drain(cx);
            assert_eq!(drained.len(), baseline + 2);
            assert_eq!(pending_count(cx), 0);
        });
    }

    #[gpui::test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "register_tool called after start_server")]
    async fn register_after_start_panics_in_debug(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init(cx);
            mark_started(cx);
            register_tool(cx, |_| {});
        });
    }

    #[gpui::test]
    async fn register_tool_works_without_explicit_init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // Note: NO call to editor_mcp::init() before register_tool.
            // Should not panic — register_tool auto-inits the Registry.
            register_tool(cx, |_| {});
            assert!(cx.try_global::<Registry>().is_some());
        });
    }
}
