use std::sync::Arc;

use gpui::{Entity, TestAppContext};
use solutions::SolutionId;

use crate::adapter::AdapterRegistry;
use crate::db::SolutionAgentDb;
use crate::model::SolutionSessionId;

use super::{SolutionAgentStore, tests};

/// Build a `SolutionAgentStore` with an in-memory DB and one idle session
/// inserted directly (no ACP handshake). Returns the store entity, the
/// session id, and the tempdir holding the solution root (keep the
/// tempdir alive for the duration of the test — it holds the solution
/// on disk that later tasks resolve paths under).
pub(crate) async fn seed_store_with_session(
    cx: &mut TestAppContext,
) -> (
    Entity<SolutionAgentStore>,
    SolutionSessionId,
    tempfile::TempDir,
) {
    let (solution_id, tmp, _project) = tests::setup_solution_and_project(cx).await;
    let registry = Arc::new(AdapterRegistry::new());
    cx.update(|cx| SolutionAgentStore::init_global(cx, registry));

    let executor = cx.executor();
    let db = Arc::new(SolutionAgentDb::open(executor).expect("open db"));

    let session_id = SolutionSessionId::new();
    cx.update(|cx| {
        let store = SolutionAgentStore::global(cx);
        store.update(cx, |store, cx| {
            store.set_persistence(db, cx);
            tests::insert_cold_session(
                session_id,
                solution_id,
                gpui::SharedString::from("claude-acp"),
                None,
                None,
                store,
                cx,
            );
        });
    });

    let store = cx.update(|cx| SolutionAgentStore::global(cx));
    (store, session_id, tmp)
}

/// Retrieve the solution root path for the given `SolutionId` from the
/// global `SolutionStore`. Panics if the solution is not found (test
/// misconfiguration).
#[allow(dead_code)]
pub(crate) fn solution_root_for(
    solution_id: &SolutionId,
    cx: &mut TestAppContext,
) -> std::path::PathBuf {
    cx.update(|cx| {
        solutions::SolutionStore::global(cx)
            .read(cx)
            .solutions()
            .iter()
            .find(|s| s.id == *solution_id)
            .map(|s| s.root.clone())
            .expect("solution not found in SolutionStore")
    })
}

/// Resolve the solution root for the session `id` held by `store`. Panics if
/// the session is unknown or its solution is not registered — test
/// misconfiguration in both cases.
#[allow(dead_code)]
pub(crate) fn session_solution_root(
    store: &Entity<SolutionAgentStore>,
    id: crate::model::SolutionSessionId,
    cx: &mut TestAppContext,
) -> std::path::PathBuf {
    let solution_id = cx.update(|cx| {
        store
            .read(cx)
            .session(id)
            .expect("session not found in store")
            .read(cx)
            .solution_id
            .clone()
    });
    solution_root_for(&solution_id, cx)
}
