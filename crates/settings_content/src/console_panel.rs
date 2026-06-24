use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::MergeFrom;

use crate::DockPosition;

/// Configuration for the console panel (terminal + AI-chat tabs).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ConsolePanelSettingsContent {
    /// Where to dock the console panel.
    ///
    /// Default: bottom
    pub default_position: Option<DockPosition>,

    /// Default width of the panel in pixels (used when docked left or right).
    ///
    /// Default: 360
    pub default_width: Option<f32>,

    /// Default height of the panel in pixels (used when docked bottom).
    ///
    /// Default: 240
    pub default_height: Option<f32>,

    /// Whether to show the console panel button in the status bar.
    ///
    /// Default: true
    pub button_visible: Option<bool>,
}
