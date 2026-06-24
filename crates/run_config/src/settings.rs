use settings::{RegisterSetting, Settings};

#[derive(Clone, Debug, RegisterSetting)]
pub struct RunConfigSettings {
    /// Show the Run Configurations toolbar strip under the title bar.
    pub toolbar: bool,
}

impl Default for RunConfigSettings {
    fn default() -> Self {
        Self { toolbar: true }
    }
}

impl Settings for RunConfigSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let defaults = Self::default();
        let toolbar = content
            .run_config
            .as_ref()
            .and_then(|c| c.toolbar)
            .unwrap_or(defaults.toolbar);
        Self { toolbar }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_defaults_to_true() {
        assert!(RunConfigSettings::default().toolbar);
    }
}
