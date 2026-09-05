// Derived from: Codex@633ab199cfd724aa78013c006b27a2b3d049fc3b:codex-rs/core/src/session/session.rs and codex-rs/core/src/session/handlers.rs
// License: Apache-2.0
// Lato changes: reduced the single-active-turn lifecycle to a transport-independent state machine

use crate::{CompactionId, StartBehavior, TurnId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Idle,
    Running(ActiveTurn),
    Compacting(ActiveCompaction),
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveTurn {
    pub id: TurnId,
    pub cancel_requested: bool,
    pub active_compaction: Option<ActiveCompaction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveCompaction {
    pub id: CompactionId,
    pub cancel_requested: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartDecision {
    StartNow,
    CancelThenStart { active: TurnId, pending: TurnId },
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TransitionError {
    #[error("a foreground turn is already active")]
    TurnAlreadyActive,
    #[error("the requested turn is not the active turn")]
    NotActiveTurn,
    #[error("a compaction is already active")]
    CompactionAlreadyActive,
    #[error("the requested compaction is not active")]
    NotActiveCompaction,
    #[error("the session is stopped")]
    SessionStopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMachine {
    phase: SessionPhase,
}

impl SessionMachine {
    pub fn new() -> Self {
        Self {
            phase: SessionPhase::Idle,
        }
    }

    pub fn phase(&self) -> &SessionPhase {
        &self.phase
    }

    pub fn active_turn(&self) -> Option<&ActiveTurn> {
        match &self.phase {
            SessionPhase::Running(active) => Some(active),
            SessionPhase::Idle | SessionPhase::Compacting(_) | SessionPhase::Stopped => None,
        }
    }

    pub fn active_compaction(&self) -> Option<&ActiveCompaction> {
        match &self.phase {
            SessionPhase::Compacting(active) => Some(active),
            SessionPhase::Running(active) => active.active_compaction.as_ref(),
            SessionPhase::Idle | SessionPhase::Stopped => None,
        }
    }

    pub fn request_start(
        &mut self,
        turn_id: TurnId,
        behavior: StartBehavior,
    ) -> Result<StartDecision, TransitionError> {
        match (&mut self.phase, behavior) {
            (SessionPhase::Idle, _) => {
                self.phase = SessionPhase::Running(ActiveTurn {
                    id: turn_id,
                    cancel_requested: false,
                    active_compaction: None,
                });
                Ok(StartDecision::StartNow)
            }
            (SessionPhase::Running(active), _) if active.active_compaction.is_some() => {
                Err(TransitionError::CompactionAlreadyActive)
            }
            (SessionPhase::Running(_), StartBehavior::Reject) => {
                Err(TransitionError::TurnAlreadyActive)
            }
            (SessionPhase::Running(active), StartBehavior::Replace) => {
                active.cancel_requested = true;
                Ok(StartDecision::CancelThenStart {
                    active: active.id.clone(),
                    pending: turn_id,
                })
            }
            (SessionPhase::Compacting(_), _) => Err(TransitionError::CompactionAlreadyActive),
            (SessionPhase::Stopped, _) => Err(TransitionError::SessionStopped),
        }
    }

    pub fn request_compaction(
        &mut self,
        compaction_id: CompactionId,
    ) -> Result<(), TransitionError> {
        match &self.phase {
            SessionPhase::Idle => {
                self.phase = SessionPhase::Compacting(ActiveCompaction {
                    id: compaction_id,
                    cancel_requested: false,
                });
                Ok(())
            }
            SessionPhase::Running(active) if active.active_compaction.is_some() => {
                Err(TransitionError::CompactionAlreadyActive)
            }
            SessionPhase::Running(_) => Err(TransitionError::TurnAlreadyActive),
            SessionPhase::Compacting(_) => Err(TransitionError::CompactionAlreadyActive),
            SessionPhase::Stopped => Err(TransitionError::SessionStopped),
        }
    }

    pub fn request_compaction_cancel(
        &mut self,
        compaction_id: &CompactionId,
    ) -> Result<(), TransitionError> {
        let Some(active) = self.active_compaction_mut(compaction_id) else {
            return Err(TransitionError::NotActiveCompaction);
        };
        active.cancel_requested = true;
        Ok(())
    }

    pub fn request_turn_compaction(
        &mut self,
        turn_id: &TurnId,
        compaction_id: CompactionId,
    ) -> Result<(), TransitionError> {
        let Some(active) = self.active_turn_mut(turn_id) else {
            return Err(TransitionError::NotActiveTurn);
        };
        if active.active_compaction.is_some() {
            return Err(TransitionError::CompactionAlreadyActive);
        }
        active.active_compaction = Some(ActiveCompaction {
            id: compaction_id,
            cancel_requested: false,
        });
        Ok(())
    }

    pub fn request_turn_compaction_cancel(
        &mut self,
        turn_id: &TurnId,
        compaction_id: &CompactionId,
    ) -> Result<(), TransitionError> {
        let Some(active) = self.active_turn_mut(turn_id) else {
            return Err(TransitionError::NotActiveTurn);
        };
        let Some(compaction) = active.active_compaction.as_mut() else {
            return Err(TransitionError::NotActiveCompaction);
        };
        if &compaction.id != compaction_id {
            return Err(TransitionError::NotActiveCompaction);
        }
        compaction.cancel_requested = true;
        Ok(())
    }

    pub fn finish_turn_compaction(
        &mut self,
        turn_id: &TurnId,
        compaction_id: &CompactionId,
    ) -> Result<(), TransitionError> {
        let Some(active) = self.active_turn_mut(turn_id) else {
            return Err(TransitionError::NotActiveTurn);
        };
        let Some(compaction) = active.active_compaction.as_ref() else {
            return Err(TransitionError::NotActiveCompaction);
        };
        if &compaction.id != compaction_id {
            return Err(TransitionError::NotActiveCompaction);
        }
        active.active_compaction = None;
        Ok(())
    }

    pub fn finish_compaction(
        &mut self,
        compaction_id: &CompactionId,
    ) -> Result<(), TransitionError> {
        let Some(active) = self.active_compaction() else {
            return Err(TransitionError::NotActiveCompaction);
        };
        if &active.id != compaction_id {
            return Err(TransitionError::NotActiveCompaction);
        }
        self.phase = SessionPhase::Idle;
        Ok(())
    }

    pub fn request_cancel(&mut self, turn_id: &TurnId) -> Result<(), TransitionError> {
        let Some(active) = self.active_turn_mut(turn_id) else {
            return Err(TransitionError::NotActiveTurn);
        };
        active.cancel_requested = true;
        if let Some(compaction) = active.active_compaction.as_mut() {
            compaction.cancel_requested = true;
        }
        Ok(())
    }

    pub fn finish(&mut self, turn_id: &TurnId) -> Result<(), TransitionError> {
        let Some(active) = self.active_turn() else {
            return Err(TransitionError::NotActiveTurn);
        };
        if &active.id != turn_id {
            return Err(TransitionError::NotActiveTurn);
        }
        if active.active_compaction.is_some() {
            return Err(TransitionError::CompactionAlreadyActive);
        }
        self.phase = SessionPhase::Idle;
        Ok(())
    }

    pub fn stop(&mut self) {
        self.phase = SessionPhase::Stopped;
    }

    fn active_turn_mut(&mut self, turn_id: &TurnId) -> Option<&mut ActiveTurn> {
        match &mut self.phase {
            SessionPhase::Running(active) if &active.id == turn_id => Some(active),
            SessionPhase::Idle
            | SessionPhase::Running(_)
            | SessionPhase::Compacting(_)
            | SessionPhase::Stopped => None,
        }
    }

    fn active_compaction_mut(
        &mut self,
        compaction_id: &CompactionId,
    ) -> Option<&mut ActiveCompaction> {
        match &mut self.phase {
            SessionPhase::Compacting(active) if &active.id == compaction_id => Some(active),
            SessionPhase::Running(active)
                if active
                    .active_compaction
                    .as_ref()
                    .is_some_and(|compaction| &compaction.id == compaction_id) =>
            {
                active.active_compaction.as_mut()
            }
            SessionPhase::Idle
            | SessionPhase::Running(_)
            | SessionPhase::Compacting(_)
            | SessionPhase::Stopped => None,
        }
    }
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self::new()
    }
}
