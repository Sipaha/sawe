use gpui::WindowButtonLayout;
use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Copy, Clone, Debug, RegisterSetting)]
pub struct TitleBarSettings {
    // Sawe fork: the project-info chain (project name +
    // worktree/branch) is replaced by the solution-tab strip in the
    // title bar (see Phase 2 Task 8) and by the active-solution +
    // branch surface in the fork status bar (Phase 2 Task 9). These
    // settings no longer have a render site to gate, but we keep
    // them on the struct to avoid breaking users' existing
    // `settings.json` `title_bar` blocks.
    #[allow(dead_code)]
    pub show_branch_status_icon: bool,
    pub show_onboarding_banner: bool,
    pub show_user_picture: bool,
    #[allow(dead_code)]
    pub show_branch_name: bool,
    #[allow(dead_code)]
    pub show_project_items: bool,
    // Sign-in UI is hidden in sawe — Zed accounts are not used.
    #[allow(dead_code)]
    pub show_sign_in: bool,
    pub show_user_menu: bool,
    pub show_menus: bool,
    pub button_layout: Option<WindowButtonLayout>,
}

impl Settings for TitleBarSettings {
    fn from_settings(s: &SettingsContent) -> Self {
        let content = s.title_bar.clone().unwrap();
        TitleBarSettings {
            show_branch_status_icon: content.show_branch_status_icon.unwrap(),
            show_onboarding_banner: content.show_onboarding_banner.unwrap(),
            show_user_picture: content.show_user_picture.unwrap(),
            show_branch_name: content.show_branch_name.unwrap(),
            show_project_items: content.show_project_items.unwrap(),
            show_sign_in: content.show_sign_in.unwrap(),
            show_user_menu: content.show_user_menu.unwrap(),
            show_menus: content.show_menus.unwrap(),
            button_layout: content.button_layout.unwrap_or_default().into_layout(),
        }
    }
}
