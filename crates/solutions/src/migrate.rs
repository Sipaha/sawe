//! One-time import of the legacy solutions.json file into SolutionsDb.
//!
//! Runs at SolutionStore::init_global on every startup but exits early
//! when the JSON file is absent or the DB already has rows. After a
//! successful import, renames the JSON file to *.migrated.bak so the
//! user can verify and so we never re-import the same data.

use crate::db::SolutionsDb;
use crate::persistence::load_or_default;
use anyhow::{Context, Result};
use std::path::Path;

pub fn run_one_time_migration(db: &SolutionsDb, json_path: &Path) -> Result<()> {
    if !json_path.exists() {
        return Ok(());
    }
    let existing_catalog = gpui::block_on(db.load_all_catalog_projects())?;
    let existing_solutions = gpui::block_on(db.load_all_solutions_with_members())?;
    if !existing_catalog.is_empty() || !existing_solutions.is_empty() {
        log::info!(
            "solutions::migrate: skipping import — DB already has {} catalog rows and {} solution rows",
            existing_catalog.len(),
            existing_solutions.len()
        );
        return Ok(());
    }

    let cfg = load_or_default(json_path).context("parse legacy solutions.json")?;
    log::info!(
        "solutions::migrate: importing {} catalog and {} solution(s) from {}",
        cfg.catalog.len(),
        cfg.solutions.len(),
        json_path.display()
    );

    for c in &cfg.catalog {
        gpui::block_on(db.save_catalog_project(
            c.id.0.clone(),
            c.name.clone(),
            c.remote_url.clone(),
            c.default_branch.clone(),
        ))?;
    }
    for s in &cfg.solutions {
        let last_ms = s.last_opened_at.map(|t| t.timestamp_millis());
        gpui::block_on(db.save_solution(
            s.id.0.clone(),
            s.name.clone(),
            s.root.to_string_lossy().into_owned(),
            last_ms,
        ))?;
        for (i, m) in s.members.iter().enumerate() {
            gpui::block_on(db.set_solution_member(
                s.id.0.clone(),
                m.catalog_id.0.clone(),
                m.local_path.to_string_lossy().into_owned(),
                i as i32,
            ))?;
        }
    }

    let bak = json_path.with_extension("json.migrated.bak");
    std::fs::rename(json_path, &bak)
        .with_context(|| format!("renaming {} -> {}", json_path.display(), bak.display()))?;
    log::info!(
        "solutions::migrate: import done; original file moved to {}",
        bak.display()
    );
    Ok(())
}
