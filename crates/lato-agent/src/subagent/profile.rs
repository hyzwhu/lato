use lato_core::{AgentProfile, TaskError, TaskErrorCode};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinProfileName {
    Explorer,
    Worker,
    Reviewer,
}

impl BuiltinProfileName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explorer => "explorer",
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
        }
    }

    pub fn resolve(self) -> AgentProfile {
        match self {
            Self::Explorer => AgentProfile::explorer(),
            Self::Worker => AgentProfile::worker(),
            Self::Reviewer => AgentProfile::reviewer(),
        }
    }
}

impl TryFrom<&str> for BuiltinProfileName {
    type Error = TaskError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "explorer" => Ok(Self::Explorer),
            "worker" => Ok(Self::Worker),
            "reviewer" => Ok(Self::Reviewer),
            _ => Err(TaskError::new(
                TaskErrorCode::InvalidProfile,
                format!("unknown built-in task profile: {value}"),
            )),
        }
    }
}
