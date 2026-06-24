//! Inversion-of-control providers for solution-aware extensions of `git_ui`
//! (P-9 in `docs/superpowers/plans/git-panel-plan.md`).
//!
//! `git_ui` defines the trait surface; `solution_git` implements it and
//! registers concrete providers at `solution_git::init`. `git_ui` consumes
//! providers through this registry, so there's no upward dep `git_ui →
//! solution_git`.
//!
//! When a provider is `None` (no solution layer registered, or no Solution is
//! open), `git_ui` falls back to its single-repo behavior.

pub mod log_data_source;
pub mod push;
pub mod update;

use std::sync::OnceLock;

pub use log_data_source::{AggregatedCommit, LogDataSource, LogQuery};
pub use push::SolutionPushProvider;
pub use update::SolutionUpdateProvider;

struct Providers {
    solution_push: OnceLock<Box<dyn SolutionPushProvider>>,
    solution_update: OnceLock<Box<dyn SolutionUpdateProvider>>,
    log_data_source: OnceLock<Box<dyn LogDataSource>>,
}

static PROVIDERS: Providers = Providers {
    solution_push: OnceLock::new(),
    solution_update: OnceLock::new(),
    log_data_source: OnceLock::new(),
};

pub fn set_solution_push_provider(provider: Box<dyn SolutionPushProvider>) {
    if PROVIDERS.solution_push.set(provider).is_err() {
        log::warn!("git_ui::providers: solution_push provider was already registered");
    }
}

pub fn solution_push_provider() -> Option<&'static dyn SolutionPushProvider> {
    PROVIDERS.solution_push.get().map(|b| b.as_ref())
}

pub fn set_solution_update_provider(provider: Box<dyn SolutionUpdateProvider>) {
    if PROVIDERS.solution_update.set(provider).is_err() {
        log::warn!("git_ui::providers: solution_update provider was already registered");
    }
}

pub fn solution_update_provider() -> Option<&'static dyn SolutionUpdateProvider> {
    PROVIDERS.solution_update.get().map(|b| b.as_ref())
}

pub fn set_log_data_source(provider: Box<dyn LogDataSource>) {
    if PROVIDERS.log_data_source.set(provider).is_err() {
        log::warn!("git_ui::providers: log_data_source provider was already registered");
    }
}

pub fn log_data_source() -> Option<&'static dyn LogDataSource> {
    PROVIDERS.log_data_source.get().map(|b| b.as_ref())
}
