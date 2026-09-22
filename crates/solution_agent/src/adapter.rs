use std::collections::HashMap;
use std::sync::Arc;

use gpui::SharedString;
use solutions::Solution;
use ui::IconName;

use crate::model::AgentServerId;

/// Brand chrome for an agent: the logo, name and vendor that identify it in
/// the "new session" picker and on its session tabs, plus the models it
/// normally runs.
///
/// Resolved from a bare agent id by [`agent_brand`] rather than through
/// [`AdapterRegistry`] on purpose — the chrome that needs it renders in
/// places where the registry is not reachable or not populated (the store's
/// test harness registers **no** adapters, and a session restored from disk
/// can name an agent this build no longer ships). The adapters below then
/// read their own `display_name`/`icon` back out of the same const, so the
/// picker, the tabs and the status row cannot disagree about a provider.
pub struct AgentBrand {
    /// Short product name — "Claude", "Codex".
    pub name: &'static str,
    /// Who makes it — "Anthropic", "OpenAI". Shown greyed beside the models.
    pub vendor: &'static str,
    /// The vendor's mark, drawn as a monochrome mask by `ui::Icon` (the fill
    /// colours inside the SVG files are ignored, so both logos follow the
    /// theme).
    pub logo: IconName,
    /// The models this agent normally runs, **default first**, `·`-separated.
    ///
    /// Display-only and deliberately version-free: the authoritative list
    /// comes from the agent CLI at probe time
    /// (`native_controls::probe_models` → `ModelCatalog`), and it moves every
    /// few weeks. A version number baked in here would be stale before the
    /// next release, whereas the family names are what both CLIs' own model
    /// pickers are keyed on and have been stable for a year.
    pub models: &'static str,
}

/// The brand for `agent_id`, or `None` for an agent this build does not ship.
pub fn agent_brand(agent_id: &str) -> Option<&'static AgentBrand> {
    match agent_id {
        crate::claude_adapter::CLAUDE_ACP_AGENT_ID => Some(&crate::claude_adapter::BRAND),
        crate::codex_adapter::CODEX_AGENT_ID => Some(&crate::codex_adapter::BRAND),
        _ => None,
    }
}

pub trait SolutionAgentAdapter: Send + Sync {
    fn agent_id(&self) -> AgentServerId;
    fn display_name(&self) -> SharedString;
    fn icon(&self) -> IconName;
    fn build_initial_system_prompt(&self, solution: &Solution) -> String;
    fn supports_resume(&self) -> bool {
        false
    }
}

pub struct AdapterRegistry {
    by_id: HashMap<AgentServerId, Arc<dyn SolutionAgentAdapter>>,
    order: Vec<AgentServerId>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self {
            by_id: HashMap::new(),
            order: Vec::new(),
        }
    }

    pub fn register(&mut self, adapter: Arc<dyn SolutionAgentAdapter>) {
        let id = adapter.agent_id();
        if !self.by_id.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.by_id.insert(id, adapter);
    }

    pub fn get(&self, id: &AgentServerId) -> Option<Arc<dyn SolutionAgentAdapter>> {
        self.by_id.get(id).cloned()
    }

    pub fn supported_ids(&self) -> &[AgentServerId] {
        &self.order
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubAdapter(&'static str);
    impl SolutionAgentAdapter for StubAdapter {
        fn agent_id(&self) -> AgentServerId {
            SharedString::from(self.0)
        }
        fn display_name(&self) -> SharedString {
            SharedString::from(self.0)
        }
        fn icon(&self) -> IconName {
            IconName::Sparkle
        }
        fn build_initial_system_prompt(&self, _: &Solution) -> String {
            String::new()
        }
    }

    #[test]
    fn registry_dedupes_and_preserves_first_insert_order() {
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(StubAdapter("a")));
        reg.register(Arc::new(StubAdapter("b")));
        reg.register(Arc::new(StubAdapter("a")));
        assert_eq!(
            reg.supported_ids(),
            &[SharedString::from("a"), SharedString::from("b")]
        );
        assert!(reg.get(&SharedString::from("a")).is_some());
        assert!(reg.get(&SharedString::from("c")).is_none());
    }
}
