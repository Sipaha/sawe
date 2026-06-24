//! Panel button helpers.
//!
//! The donor Sawe fork defined these in the `panel` crate; the re-fork's
//! `panel` crate is kept close to upstream and does not export them, so they
//! live here to keep edits scoped to `git_ui`.

use ui::{IconName, IntoElement, SharedString, prelude::*};

pub fn panel_button(label: impl Into<SharedString>) -> ui::Button {
    let label = label.into();
    let id = ElementId::Name(label.to_lowercase().replace(' ', "_").into());
    ui::Button::new(id, label)
        .label_size(ui::LabelSize::Small)
        // TODO: Change this once we use on_surface_bg in button_like
        .layer(ui::ElevationIndex::ModalSurface)
        .size(ui::ButtonSize::Compact)
}

pub fn panel_filled_button(label: impl Into<SharedString>) -> ui::Button {
    panel_button(label).style(ui::ButtonStyle::Filled)
}

pub fn panel_icon_button(id: impl Into<SharedString>, icon: IconName) -> ui::IconButton {
    let id = ElementId::Name(id.into());

    IconButton::new(id, icon)
        // TODO: Change this once we use on_surface_bg in button_like
        .layer(ui::ElevationIndex::ModalSurface)
}
