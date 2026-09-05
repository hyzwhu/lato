use crate::{CompactSession, CompactionId, ModelSelection, TurnId};

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    StartTurn(StartTurn),
    SteerTurn(UserInput),
    CancelTurn {
        turn_id: TurnId,
    },
    CompactSession(CompactSession),
    CancelCompaction {
        compaction_id: CompactionId,
    },
    SelectModel {
        selection: ModelSelection,
        model_family: Option<String>,
        context_window: Option<u64>,
    },
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StartTurn {
    pub input: UserInput,
    pub behavior: StartBehavior,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartBehavior {
    Reject,
    Replace,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct UserInput {
    pub text: String,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}
