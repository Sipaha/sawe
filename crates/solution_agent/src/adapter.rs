use std::collections::HashMap;
use std::sync::Arc;

use gpui::SharedString;
use solutions::Solution;
use ui::IconName;

use crate::model::AgentServerId;

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
