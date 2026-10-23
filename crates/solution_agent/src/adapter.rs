use std::collections::HashMap;
use std::sync::Arc;

use gpui::SharedString;
use solutions::Solution;
use ui::IconName;

use crate::model::AgentServerId;

/// Branding is downloaded from SPK AI Hub and cached for offline history.
#[derive(Clone)]
pub struct AgentBrand {
    pub name: String,
    pub vendor: String,
    pub logo: IconName,
    pub icon_path: Option<SharedString>,
    pub permissions: bool,
    pub context_usage_current: bool,
    pub effort_options: Vec<String>,
    pub color: u32,
    pub models: String,
}
impl AgentBrand {
    pub fn tint(&self) -> ui::Color {
        ui::Color::Custom(gpui::rgb(self.color).into())
    }
    pub fn icon(&self) -> ui::Icon {
        self.icon_path
            .as_ref()
            .map(|path| ui::Icon::from_external_svg(path.clone()))
            .unwrap_or_else(|| ui::Icon::new(self.logo))
    }
}
#[derive(Default)]
struct Brands {
    by_id: HashMap<String, Arc<AgentBrand>>,
    order: Vec<String>,
    default: String,
    actions: HashMap<String, String>,
}
static BRANDS: std::sync::LazyLock<std::sync::RwLock<Brands>> =
    std::sync::LazyLock::new(Default::default);
pub fn agent_brand(agent_id: &str) -> Option<Arc<AgentBrand>> {
    if let Some(brand) = BRANDS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .by_id
        .get(agent_id)
        .cloned()
    {
        return Some(brand);
    }
    #[cfg(any(test, feature = "test-support"))]
    {
        match agent_id {
            "claude-acp" => return Some(Arc::new(crate::claude_adapter::BRAND.clone())),
            "codex-native" => return Some(Arc::new(crate::codex_adapter::BRAND.clone())),
            "kimi-code" => return Some(Arc::new(crate::kimi_adapter::BRAND.clone())),
            _ => {}
        }
    }
    None
}
pub fn available_brands() -> Vec<(AgentServerId, Arc<AgentBrand>)> {
    let brands = BRANDS.read().unwrap_or_else(|e| e.into_inner());
    #[cfg(any(test, feature = "test-support"))]
    if brands.order.is_empty() {
        drop(brands);
        return ["codex-native", "claude-acp", "kimi-code"]
            .into_iter()
            .filter_map(|id| agent_brand(id).map(|b| (id.into(), b)))
            .collect();
    }
    brands
        .order
        .iter()
        .filter_map(|id| brands.by_id.get(id).map(|b| (id.clone().into(), b.clone())))
        .collect()
}
pub fn default_harness() -> SharedString {
    let id = BRANDS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .default
        .clone();
    #[cfg(any(test, feature = "test-support"))]
    if id.is_empty() {
        return "claude-acp".into();
    }
    id.into()
}
pub fn harness_for_action(action: &str) -> Option<String> {
    let result = BRANDS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .actions
        .get(action)
        .cloned();
    #[cfg(any(test, feature = "test-support"))]
    if result.is_none() {
        return match action {
            "console_panel::NewCodexChat" => Some("codex-native".into()),
            "console_panel::NewKimiChat" => Some("kimi-code".into()),
            _ => None,
        };
    }
    result
}
pub(crate) fn install_brands(
    entries: Vec<(String, Arc<AgentBrand>, Vec<String>)>,
    default: String,
) {
    let mut brands = BRANDS.write().unwrap_or_else(|e| e.into_inner());
    brands.order = entries.iter().map(|e| e.0.clone()).collect();
    brands.default = default;
    brands.actions.clear();
    for (id, brand, actions) in entries {
        for action in actions {
            brands.actions.insert(action, id.clone());
        }
        brands.by_id.insert(id, brand);
    }
}

pub struct HubAdapter {
    pub id: AgentServerId,
    pub brand: Arc<AgentBrand>,
    pub instruction_file: String,
    pub resume: bool,
}
impl SolutionAgentAdapter for HubAdapter {
    fn agent_id(&self) -> AgentServerId {
        self.id.clone()
    }
    fn display_name(&self) -> SharedString {
        self.brand.name.clone().into()
    }
    fn icon(&self) -> IconName {
        IconName::Sparkle
    }
    fn build_initial_system_prompt(&self, solution: &Solution) -> String {
        crate::client_instructions::solution_system_prompt(solution, &self.instruction_file)
    }
    fn supports_resume(&self) -> bool {
        self.resume
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

/// Connection state for discovery UI; cached harnesses remain selectable offline.
pub fn hub_catalog_status() -> Option<String> { crate::hub_catalog::status() }
