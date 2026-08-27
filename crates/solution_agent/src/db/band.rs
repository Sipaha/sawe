use anyhow::Result;
use gpui::Task;
use indoc::indoc;
use solutions::SolutionId;
use sqlez::connection::Connection;
use util::ResultExt as _;
use workspace::UtilityKind;

use crate::db::SolutionAgentDb;
use crate::model::{BandState, SolutionSessionId, clamp_band_height, clamp_divider_ratio};

impl SolutionAgentDb {
    pub fn save_band_state(&self, solution_id: SolutionId, state: BandState) -> Task<Result<()>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            upsert_band_state(&connection, solution_id, &state)
        })
    }

    /// Every persisted band row, loaded in one shot at
    /// `SolutionAgentStore::set_persistence` time. Loading the whole table
    /// rather than querying per Solution keeps the read off the render path:
    /// the band asks for its state on every frame, and there is one row per
    /// Solution the user has ever opened — a few dozen at most.
    pub fn load_band_states(&self) -> Task<Result<Vec<(SolutionId, BandState)>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            select_band_states(&connection)
        })
    }
}

fn upsert_band_state(
    connection: &Connection,
    solution_id: SolutionId,
    state: &BandState,
) -> Result<()> {
    let mut stmt = connection.exec_bound::<(i64, f32, i64, Option<String>, f32, String)>(indoc! {"
        INSERT INTO solution_band_state
            (solution_id, divider_ratio, utility_visible, active_dialog_session, band_height, utility_kind)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(solution_id) DO UPDATE SET
            divider_ratio = ?2,
            utility_visible = ?3,
            active_dialog_session = ?4,
            band_height = ?5,
            utility_kind = ?6
    "})?;
    stmt((
        solution_id.0,
        clamp_divider_ratio(state.divider_ratio),
        state.utility_visible as i64,
        state.active_dialog_session.map(|id| id.to_string()),
        clamp_band_height(state.height),
        state.utility_kind.as_str().to_string(),
    ))
}

fn select_band_states(connection: &Connection) -> Result<Vec<(SolutionId, BandState)>> {
    let mut select = connection.select::<(i64, f32, i64, Option<String>, f32, String)>(indoc! {"
        SELECT solution_id, divider_ratio, utility_visible, active_dialog_session, band_height, utility_kind
        FROM solution_band_state
    "})?;
    let rows = select()?;
    Ok(rows
        .into_iter()
        .map(
            |(
                solution_id,
                divider_ratio,
                utility_visible,
                active_dialog_session,
                band_height,
                utility_kind,
            )| {
                let active_dialog_session = active_dialog_session.and_then(|id| {
                    // A malformed id is the user's own row gone bad, not a reason
                    // to drop every other Solution's geometry — log it and treat
                    // that one Solution as having a collapsed dialog.
                    SolutionSessionId::parse(&id).log_err()
                });
                let utility_kind = UtilityKind::from_str(&utility_kind).unwrap_or_else(|| {
                    // Same degrade-one-row-not-the-whole-table posture as the
                    // malformed `active_dialog_session` above: an unrecognized
                    // kind (hand-edited row, or a future kind an older binary
                    // doesn't know) falls back to the terminal rather than
                    // dropping the rest of this Solution's geometry.
                    log::warn!(
                        "solution_band_state: unrecognized utility_kind {utility_kind:?} for solution {solution_id}, defaulting to Terminal"
                    );
                    UtilityKind::Terminal
                });
                (
                    SolutionId(solution_id),
                    BandState {
                        divider_ratio: clamp_divider_ratio(divider_ratio),
                        utility_visible: utility_visible != 0,
                        utility_kind,
                        active_dialog_session,
                        height: clamp_band_height(band_height),
                    },
                )
            },
        )
        .collect())
}
