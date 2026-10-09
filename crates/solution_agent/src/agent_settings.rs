use settings::{RegisterSetting, Settings};
use std::time::Duration;

#[derive(Clone, Debug, RegisterSetting)]
pub struct SolutionAgentSettings {
    pub default_provider: gpui::SharedString,
    pub ephemeral: EphemeralPoolSettings,
}

impl Default for SolutionAgentSettings {
    fn default() -> Self {
        Self {
            default_provider: crate::claude_adapter::CLAUDE_ACP_AGENT_ID.into(),
            ephemeral: EphemeralPoolSettings::default(),
        }
    }
}

pub fn default_provider(cx: &gpui::App) -> gpui::SharedString {
    SolutionAgentSettings::try_get(cx)
        .map(|settings| settings.default_provider.clone())
        .unwrap_or_else(|| crate::claude_adapter::CLAUDE_ACP_AGENT_ID.into())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EphemeralPoolSettings {
    pub max_concurrent: u32,
    pub queue_timeout: Duration,
    pub idle_ttl: Duration,
}

impl Default for EphemeralPoolSettings {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            queue_timeout: Duration::from_secs(30),
            idle_ttl: Duration::from_secs(60),
        }
    }
}

impl Settings for SolutionAgentSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let defaults = EphemeralPoolSettings::default();
        let ephemeral = content
            .solution_agent
            .as_ref()
            .and_then(|s| s.ephemeral.as_ref())
            .map(|e| EphemeralPoolSettings {
                max_concurrent: e.max_concurrent.unwrap_or(defaults.max_concurrent),
                queue_timeout: e
                    .queue_timeout_seconds
                    .map(|s| Duration::from_secs(s as u64))
                    .unwrap_or(defaults.queue_timeout),
                idle_ttl: e
                    .idle_ttl_seconds
                    .map(|s| Duration::from_secs(s as u64))
                    .unwrap_or(defaults.idle_ttl),
            })
            .unwrap_or(defaults);
        let default_provider = content
            .solution_agent
            .as_ref()
            .and_then(|settings| settings.default_provider.as_ref())
            .map(|id| gpui::SharedString::from(id.clone()))
            .unwrap_or_else(|| crate::claude_adapter::CLAUDE_ACP_AGENT_ID.into());
        Self {
            default_provider,
            ephemeral,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_plan() {
        let s = SolutionAgentSettings::default();
        assert_eq!(s.ephemeral.max_concurrent, 3);
        assert_eq!(s.ephemeral.queue_timeout, Duration::from_secs(30));
        assert_eq!(s.ephemeral.idle_ttl, Duration::from_secs(60));
    }
}
