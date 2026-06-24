mod file_format;
mod mcp;
mod model;
mod provider;
mod providers;
mod settings;
mod store;

pub use model::*;
pub use provider::*;
pub use settings::RunConfigSettings;
pub use store::{RunCommand, RunConfigStore, RunConfigStoreEvent, register_provider};

use ::settings::Settings as _;
use gpui::App;

pub fn init(cx: &mut App) {
    RunConfigSettings::register(cx);
    RunConfigStore::init_global(cx);
    providers::register_builtin(cx);
    mcp::register(cx);
}
