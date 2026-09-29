//! Auto-compaction: when a session's context crosses its threshold while the
//! agent is working, the editor starts the handoff ladder on its own
//! (`store::compaction_ladder`). This replaced the observer's `compact` verdict
//! (FORK.md #220): a percentage the user set is a better trigger than a judge
//! reading the transcript, and it works on sessions nobody supervises.
//!
//! The setting is per session, on by default, and persisted only when it
//! differs from the default (`solution_session_auto_compact`).

use gpui::Context;

use crate::model::{AutoCompactSetting, HandoffAsker, SessionContextCount, SolutionSessionId};
use crate::store::compaction_ladder::HandoffRequest;
use crate::store::{SolutionAgentStore, SolutionAgentStoreEvent};

impl SolutionAgentStore {
    pub(crate) fn auto_compact_setting(&self, id: SolutionSessionId) -> AutoCompactSetting {
        self.auto_compact.get(&id).copied().unwrap_or_default()
    }

    /// Change a session's auto-compaction. Switching it off also stands down a
    /// handoff it already started; the user's own request is left alone.
    pub(crate) fn set_auto_compact(
        &mut self,
        id: SolutionSessionId,
        setting: AutoCompactSetting,
        cx: &mut Context<Self>,
    ) {
        if self.auto_compact_setting(id) == setting {
            return;
        }
        if setting == AutoCompactSetting::default() {
            self.auto_compact.remove(&id);
        } else {
            self.auto_compact.insert(id, setting);
        }
        if let Some(db) = &self.persistence
            && let Err(error) = db.set_auto_compact(id, setting)
        {
            log::error!("Could not persist auto-compaction for {id}: {error}");
        }
        // A new threshold is a new decision: let it fire in this context even
        // if the old one already did.
        self.auto_compact_fired.remove(&id);
        let automatic_ladder = self.session(id).is_some_and(|session| {
            session
                .read(cx)
                .handoff_ladder
                .as_ref()
                .is_some_and(|ladder| ladder.asker == HandoffAsker::Auto)
        });
        if !setting.enabled && automatic_ladder {
            self.reset_compaction_ladder(id, cx);
        }
        cx.emit(SolutionAgentStoreEvent::SessionStateChanged(id));
        cx.notify();
    }

    /// Start the handoff ladder for every working session whose context has
    /// crossed its threshold. Once per context: a user who stops the handoff is
    /// not asked again until the next context (or a new threshold). Only while
    /// the agent's turn is running — the context grows only then, and an idle
    /// session restored above its threshold must not be compacted behind the
    /// user's back on startup.
    pub(crate) fn tick_auto_compaction(&mut self, cx: &mut Context<Self>) {
        let due: Vec<(SolutionSessionId, SessionContextCount)> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                let s = session.read(cx);
                if s.is_supervisor_ephemeral || s.is_ephemeral || s.is_handing_off() {
                    return None;
                }
                let setting = self.auto_compact_setting(*id);
                if !setting.enabled
                    || self.auto_compact_fired.get(id) == Some(&s.context_count)
                    || !self.main_turn_in_flight(*id, cx)
                {
                    return None;
                }
                let (used, max) = crate::model::session_context_usage(s, cx)?;
                let threshold = u128::from(setting.effective_threshold(max));
                (u128::from(used) * 100 >= u128::from(max) * threshold)
                    .then_some((*id, s.context_count))
            })
            .collect();
        for (id, context) in due {
            self.auto_compact_fired.insert(id, context);
            if self.request_handoff(id, HandoffAsker::Auto, false, None, cx)
                == HandoffRequest::CompactNow
            {
                self.run_ladder_compaction(id, cx);
            }
        }
    }

    /// The editor's own clock for context handoffs: start automatic ones, then
    /// move every ladder a rung.
    pub(crate) fn tick_context_handoffs(&mut self, cx: &mut Context<Self>) {
        self.tick_auto_compaction(cx);
        self.tick_compaction_ladders(cx);
    }
}
