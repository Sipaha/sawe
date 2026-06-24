use gpui::Pixels;
use settings::{RegisterSetting, Settings};
use ui::px;
use workspace::dock::DockPosition;

#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct ConsolePanelSettings {
    /// Where to dock the console panel.
    pub default_position: DockPosition,
    /// Default width of the panel in pixels (when docked left or right).
    pub default_width: Pixels,
    /// Default height of the panel in pixels (when docked bottom).
    pub default_height: Pixels,
    /// Whether to show the console panel button in the status bar.
    pub button_visible: bool,
}

impl Settings for ConsolePanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let console_panel = content.console_panel.clone().unwrap();
        Self {
            default_position: console_panel.default_position.unwrap().into(),
            default_width: px(console_panel.default_width.unwrap()),
            default_height: px(console_panel.default_height.unwrap()),
            button_visible: console_panel.button_visible.unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use settings::SettingsContent;
    use settings_content::DockPosition as ContentDockPosition;
    use workspace::dock::DockPosition;

    use super::*;

    #[test]
    fn defaults_match_spec() {
        let mut content = SettingsContent::default();
        content.console_panel = Some(settings_content::ConsolePanelSettingsContent {
            default_position: Some(ContentDockPosition::Bottom),
            default_width: Some(360.0),
            default_height: Some(240.0),
            button_visible: Some(true),
        });
        let settings = ConsolePanelSettings::from_settings(&content);
        assert_eq!(settings.default_position, DockPosition::Bottom);
        assert_eq!(settings.default_width, px(360.0));
        assert_eq!(settings.default_height, px(240.0));
        assert!(settings.button_visible);
    }

    #[test]
    fn overrides_are_applied() {
        let mut content = SettingsContent::default();
        content.console_panel = Some(settings_content::ConsolePanelSettingsContent {
            default_position: Some(ContentDockPosition::Right),
            default_width: Some(480.0),
            default_height: Some(300.0),
            button_visible: Some(false),
        });
        let settings = ConsolePanelSettings::from_settings(&content);
        assert_eq!(settings.default_position, DockPosition::Right);
        assert_eq!(settings.default_width, px(480.0));
        assert_eq!(settings.default_height, px(300.0));
        assert!(!settings.button_visible);
    }
}
