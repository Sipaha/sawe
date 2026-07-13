//! Dry-run of the identity migration against the operator's REAL databases.
//!
//! Ignored by default: these tests read `$SAWE_SOLUTIONS_DB` / `$SAWE_AGENT_DB`
//! (or the default data-dir locations), copy them into a tempdir, and migrate
//! the copies. The originals are only ever read by `std::fs::copy`; no
//! connection is ever opened against them.
//!
//! Run with:
//!   SAWE_SOLUTIONS_DB=~/.spk/sawe/data/db/0-stable/db.sqlite \
//!   SAWE_AGENT_DB=~/.spk/sawe/data/solution_agent/solution_agent.db \
//!   cargo test -p solutions --test identity_migration_rehearsal -- --ignored --nocapture

use db::sqlez::connection::Connection;
use db::sqlez::domain::Domain;
use solutions::db::SolutionsDb;
use std::path::{Path, PathBuf};

fn count(connection: &Connection, table: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let rows = connection.select::<i64>(&sql).expect("prepare count")().expect("count");
    rows.into_iter().next().unwrap_or(0)
}

fn solutions_source() -> PathBuf {
    std::env::var("SAWE_SOLUTIONS_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            paths::data_dir()
                .join("db")
                .join("0-stable")
                .join("db.sqlite")
        })
}

fn agent_source() -> PathBuf {
    std::env::var("SAWE_AGENT_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            paths::data_dir()
                .join("solution_agent")
                .join("solution_agent.db")
        })
}

/// Copy the real DB aside and open the copy. Never opens `source` itself: a
/// sqlez connection is read-write and would journal into the operator's live
/// file.
fn copy_aside(source: &Path, dir: &Path, file_name: &str) -> PathBuf {
    assert!(
        source.exists(),
        "no database at {} — set SAWE_SOLUTIONS_DB / SAWE_AGENT_DB",
        source.display()
    );
    let copy = dir.join(file_name);
    std::fs::copy(source, &copy).expect("copy the real DB");
    copy
}

/// Apply the full solutions migration list to a copy of the real solutions DB
/// and hand back the open connection.
fn migrate_solutions_copy(dir: &Path) -> Connection {
    let copy = copy_aside(&solutions_source(), dir, "db.sqlite");
    let connection = Connection::open_file(&copy.to_string_lossy());

    let solutions_before = count(&connection, "solutions");
    let members_before = count(&connection, "solution_members");
    let active_before = count(&connection, "active_member");
    let catalog_before = count(&connection, "catalog_projects");
    println!(
        "solutions db before: {solutions_before} solution(s), {members_before} member(s), \
         {active_before} active, {catalog_before} catalog"
    );

    // The inner JOIN in the migration drops an `active_member` row whose
    // catalog_id no longer names one of that solution's members. Name the
    // offenders BEFORE migrating, so a failed count assertion downstream comes
    // with the exact rows to delete rather than just a number mismatch.
    let dangling = connection
        .select::<(String, String)>(
            "SELECT a.solution_id, a.catalog_id FROM active_member a
             LEFT JOIN solution_members m
                 ON m.solution_id = a.solution_id AND m.catalog_id = a.catalog_id
             WHERE m.solution_id IS NULL",
        )
        .expect("prepare dangling active_member check")()
    .expect("dangling active_member check");
    println!("dangling active_member rows (would be dropped): {dangling:?}");

    connection
        .migrate(
            "SolutionsDb",
            <SolutionsDb as Domain>::MIGRATIONS,
            &mut |_, _, _| false,
        )
        .expect("identity migration must apply to the real DB");
    connection
}

#[test]
#[ignore = "reads the operator's real database"]
fn real_solutions_db_migrates_without_losing_rows() {
    let dir = tempfile::tempdir().expect("tempdir");

    let source = solutions_source();
    let probe = copy_aside(&source, dir.path(), "probe.sqlite");
    let probe_connection = Connection::open_file(&probe.to_string_lossy());
    let solutions_before = count(&probe_connection, "solutions");
    let members_before = count(&probe_connection, "solution_members");
    let active_before = count(&probe_connection, "active_member");
    let catalog_before = count(&probe_connection, "catalog_projects");
    drop(probe_connection);

    let connection = migrate_solutions_copy(dir.path());

    let report = connection
        .select::<(i64, i64, i64, i64, i64, i64, i64, i64)>(
            "SELECT solutions_before, solutions_after, members_before, members_after,
                    active_before, active_after, catalog_before, catalog_after
             FROM identity_migration_report",
        )
        .expect("prepare report")()
    .expect("report");
    let r = report.into_iter().next().expect("a report row must exist");
    println!(
        "solutions db after:  {} solution(s), {} member(s), {} active, {} catalog",
        r.1, r.3, r.5, r.7
    );
    assert_eq!((r.0, r.1), (solutions_before, solutions_before), "solutions");
    assert_eq!((r.2, r.3), (members_before, members_before), "members");
    assert_eq!(
        (r.4, r.5),
        (active_before, active_before),
        "active_member — a lost row here is an active_member whose catalog_id \
         names no member of that solution; delete the dangling row from the real \
         DB before upgrading (see the printed list above)"
    );
    assert_eq!((r.6, r.7), (catalog_before, catalog_before), "catalog");

    // Every member must still point at a live solution and carry a name.
    let orphans = connection
        .select::<i64>(
            "SELECT COUNT(*) FROM solution_members m
             LEFT JOIN solutions s ON s.id = m.solution_id
             WHERE s.id IS NULL OR m.name IS NULL OR LENGTH(m.name) = 0",
        )
        .expect("prepare orphan check")()
    .expect("orphan check");
    assert_eq!(orphans, vec![0], "no orphaned or unnamed members");

    let dangling_active = connection
        .select::<i64>(
            "SELECT COUNT(*) FROM active_member a
             LEFT JOIN solution_members m ON m.id = a.member_id
             WHERE m.id IS NULL",
        )
        .expect("prepare active check")()
    .expect("active check");
    assert_eq!(dangling_active, vec![0], "no active_member without a member");

    let legacy = count(&connection, "solution_legacy_ids");
    assert_eq!(
        legacy, solutions_before,
        "every old slug must be in the cross-DB map"
    );
}

#[gpui::test]
#[ignore = "reads the operator's real database"]
async fn real_agent_db_migrates_without_losing_sessions(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().expect("tempdir");

    // The agent DB's migration consumes the slug → counter map the solutions
    // migration writes, so the solutions copy has to be migrated first.
    let solutions_connection = migrate_solutions_copy(dir.path());
    let legacy_solution_ids = solutions_connection
        .select::<(String, i64)>("SELECT old_id, new_id FROM solution_legacy_ids")
        .expect("prepare legacy map")()
    .expect("legacy map");
    let members = solutions_connection
        .select::<(i64, i64, String)>("SELECT id, solution_id, local_path FROM solution_members")
        .expect("prepare members")()
    .expect("members");
    println!(
        "cross-db map: {} slug(s), {} member(s)",
        legacy_solution_ids.len(),
        members.len()
    );

    let agent_copy = copy_aside(&agent_source(), dir.path(), "solution_agent.db");
    let probe = Connection::open_file(&agent_copy.to_string_lossy());
    let sessions_before = count(&probe, "solution_sessions");
    let entries_before = count(&probe, "solution_session_entries");
    let attachments_before = count(&probe, "solution_session_attachment");
    println!(
        "agent db before: {sessions_before} session(s), {entries_before} entrie(s), \
         {attachments_before} attachment(s)"
    );
    drop(probe);

    let db = solution_agent::SolutionAgentDb::open_at_path(cx.executor(), &agent_copy)
        .expect("open the agent DB copy");
    let report = db
        .migrate_identity(legacy_solution_ids.clone(), members)
        .await
        .expect("agent identity migration");
    println!("IdentityMigrationReport: {report:?}");

    let connection = Connection::open_file(&agent_copy.to_string_lossy());
    let sessions_after = count(&connection, "solution_sessions");
    let entries_after = count(&connection, "solution_session_entries");
    let attachments_after = count(&connection, "solution_session_attachment");
    println!(
        "agent db after:  {sessions_after} session(s), {entries_after} entrie(s), \
         {attachments_after} attachment(s)"
    );
    assert_eq!(sessions_after, sessions_before, "no session row may be lost");
    assert_eq!(entries_after, entries_before, "no transcript entry may be lost");
    assert_eq!(
        attachments_after, attachments_before,
        "no attachment row may be lost"
    );
    assert_eq!(report.sessions_total, sessions_before);

    // Every session whose slug WAS in the map must now carry a numeric id;
    // every session left on a TEXT slug must be accounted for in the report's
    // unmapped list (the rows are preserved for inspection, never destroyed).
    let rows = connection
        .select::<(String, String, Option<i64>)>(
            "SELECT id, solution_id, member_id FROM solution_sessions",
        )
        .expect("prepare sessions")()
    .expect("sessions");
    let mut still_text: Vec<String> = Vec::new();
    for (_, solution_id, _) in &rows {
        if solution_id.parse::<i64>().is_err() && !still_text.contains(solution_id) {
            still_text.push(solution_id.clone());
        }
    }
    still_text.sort();
    let mut unmapped = report.sessions_unmapped.clone();
    unmapped.sort();
    println!("slugs left unmapped (rows preserved, NOT destroyed): {unmapped:?}");
    assert_eq!(
        still_text, unmapped,
        "every non-numeric solution_id left in the DB must be reported as unmapped"
    );
    for slug in &unmapped {
        assert!(
            !legacy_solution_ids.iter().any(|(old, _)| old == slug),
            "{slug} is in the solutions map yet was not remapped"
        );
    }

    let bound = rows.iter().filter(|r| r.2.is_some()).count() as i64;
    assert_eq!(
        bound, report.member_ids_backfilled,
        "member_id backfill count must match the rows actually bound"
    );
    let unmapped_sessions = rows
        .iter()
        .filter(|r| r.1.parse::<i64>().is_err())
        .count() as i64;
    assert_eq!(
        report.sessions_remapped + unmapped_sessions,
        sessions_before,
        "every session was either remapped or explicitly reported as unmapped"
    );

    // Re-running must be a no-op: no row is remapped or rebound twice.
    let second = db
        .migrate_identity(legacy_solution_ids, Vec::new())
        .await
        .expect("second agent identity migration");
    assert_eq!(second.sessions_remapped, 0, "remap must be idempotent");
    assert_eq!(second.member_ids_backfilled, 0, "backfill must be idempotent");
    assert_eq!(second.sessions_total, sessions_before);
}
