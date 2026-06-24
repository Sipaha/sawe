use settings::{RegisterSetting, Settings};
use std::time::Duration;

#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct SolutionAgentSettings {
    pub ephemeral: EphemeralPoolSettings,
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
        Self { ephemeral }
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
