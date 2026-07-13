//! One-time import of the legacy solutions.json file into SolutionsDb, plus
//! verification of the identity migration's row counts.
//!
//! `run_one_time_migration` runs at SolutionStore::init_global on every startup
//! but exits early when the JSON file is absent or the DB already has rows.
//! After a successful import it renames the JSON file to *.migrated.bak so the
//! user can verify and so we never re-import the same data.

use crate::db::SolutionsDb;
use crate::persistence::load_or_default;
use anyhow::{Context, Result, bail};
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

    let mut catalog_ids: collections::HashMap<String, i64> = collections::HashMap::default();
    for c in &cfg.catalog {
        let id = gpui::block_on(db.insert_catalog_project(
            c.name.clone(),
            c.remote_url.clone(),
            c.default_branch.clone(),
        ))?;
        catalog_ids.insert(c.id.clone(), id);
    }
    for s in &cfg.solutions {
        let solution_id = gpui::block_on(db.insert_solution(
            s.name.clone(),
            s.root.to_string_lossy().into_owned(),
            s.last_opened_at.map(|t| t.timestamp_millis()),
        ))?;
        for (i, m) in s.members.iter().enumerate() {
            gpui::block_on(db.insert_solution_member(
                solution_id,
                m.catalog_id.clone(),
                m.local_path.to_string_lossy().into_owned(),
                i as i32,
                catalog_ids.get(&m.catalog_id).copied(),
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

/// Read the row-count report the identity migration wrote inside its own
/// transaction and fail loudly if any table lost a row. `Ok(())` when the report
/// row is absent (a DB predating the report table); a DB created fresh at the
/// numeric schema has a report of all-zeroes, which trivially verifies.
pub fn verify_identity_migration(db: &SolutionsDb) -> Result<()> {
    let Some((
        solutions_before,
        solutions_after,
        members_before,
        members_after,
        active_before,
        active_after,
        catalog_before,
        catalog_after,
    )) = gpui::block_on(db.load_identity_migration_report())?
    else {
        return Ok(());
    };

    let mismatches: Vec<String> = [
        ("solutions", solutions_before, solutions_after),
        ("solution_members", members_before, members_after),
        ("active_member", active_before, active_after),
        ("catalog_projects", catalog_before, catalog_after),
    ]
    .iter()
    .filter(|(_, before, after)| before != after)
    .map(|(table, before, after)| format!("{table}: {before} rows before, {after} after"))
    .collect();

    if !mismatches.is_empty() {
        bail!(
            "identity migration lost rows — {}. The pre-migration data is NOT recoverable from \
             this DB; restore the backup taken before the upgrade.",
            mismatches.join("; ")
        );
    }
    log::info!(
        "solutions::migrate: identity migration verified — {solutions_after} solution(s), \
         {members_after} member(s), {active_after} active selection(s), {catalog_after} catalog row(s)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SolutionsDb;

    #[gpui::test]
    async fn verify_passes_on_a_fresh_db() {
        let db = SolutionsDb::open_test_db("solutions_migrate_verify_fresh").await;
        // A fresh DB still runs the identity migration, over empty legacy
        // tables — so the report row exists and reads 0-before/0-after.
        verify_identity_migration(&db).expect("a fresh DB's all-zero report must verify");
    }

    #[gpui::test]
    async fn verify_fails_when_the_report_shows_a_lost_row() {
        let db = SolutionsDb::open_test_db("solutions_migrate_verify_lost").await;
        // The migration already wrote the (all-zero) row 1; overwrite it with a
        // report that lost a member row. REPLACE is safe here — the report table
        // is not an FK parent of anything.
        db.write(|connection| {
            connection
                .exec(
                    "INSERT OR REPLACE INTO identity_migration_report (
                         id, solutions_before, solutions_after, members_before, members_after,
                         active_before, active_after, catalog_before, catalog_after)
                     VALUES (1, 16, 16, 40, 39, 5, 5, 12, 12)",
                )
                .expect("prepare report insert")()
            .expect("report insert");
        })
        .await;
        let err = verify_identity_migration(&db).expect_err("a lost member row must fail");
        assert!(
            err.to_string()
                .contains("solution_members: 40 rows before, 39 after"),
            "the error must name the table and the counts; got: {err}"
        );
    }
}
