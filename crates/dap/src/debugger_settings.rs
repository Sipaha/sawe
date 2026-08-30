use dap_types::SteppingGranularity;
use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Debug, RegisterSetting)]
pub struct DebuggerSettings {
    /// Determines the stepping granularity.
    ///
    /// Default: line
    pub stepping_granularity: SteppingGranularity,
    /// Whether the breakpoints should be reused across Zed sessions.
    ///
    /// Default: true
    pub save_breakpoints: bool,
    /// INERT in this fork. It gated the debugger's dock-strip icon; the dock
    /// strips are gone and the band's utility contents carry their own buttons.
    /// Nothing reads this field. Kept so existing settings files stay valid.
    ///
    /// Default: true
    pub button: bool,
    /// Time in milliseconds until timeout error when connecting to a TCP debug adapter
    ///
    /// Default: 2000ms
    pub timeout: u64,
    /// Whether to log messages between active debug adapters and Zed
    ///
    /// Default: true
    pub log_dap_communications: bool,
    /// Whether to format dap messages in when adding them to debug adapter logger
    ///
    /// Default: true
    pub format_dap_log_messages: bool,
    /// INERT in this fork. The debugger is hosted in the Solution band's
    /// utility section rather than a dock, so it has no dock position to set —
    /// see FORK.md #97. Kept so existing settings files stay valid.
    ///
    /// Default: Bottom
    pub dock: settings::DockPosition,
}

impl Settings for DebuggerSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let content = content.debugger.clone().unwrap();
        Self {
            stepping_granularity: dap_granularity_from_settings(
                content.stepping_granularity.unwrap(),
            ),
            save_breakpoints: content.save_breakpoints.unwrap(),
            button: content.button.unwrap(),
            timeout: content.timeout.unwrap(),
            log_dap_communications: content.log_dap_communications.unwrap(),
            format_dap_log_messages: content.format_dap_log_messages.unwrap(),
            dock: content.dock.unwrap(),
        }
    }
}

fn dap_granularity_from_settings(
    granularity: settings::SteppingGranularity,
) -> dap_types::SteppingGranularity {
    match granularity {
        settings::SteppingGranularity::Instruction => dap_types::SteppingGranularity::Instruction,
        settings::SteppingGranularity::Line => dap_types::SteppingGranularity::Line,
        settings::SteppingGranularity::Statement => dap_types::SteppingGranularity::Statement,
    }
}
