use std::{fmt, str::FromStr};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(IdError::Empty(stringify!($name)));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::parse(value).expect("static IDs must not be empty")
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::parse(value).expect("runtime IDs must not be empty")
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = <String as serde::Deserialize>::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum IdError {
    #[error("{0} must not be empty")]
    Empty(&'static str),
}

string_id!(SessionId);
string_id!(TurnId);
string_id!(EventId);
string_id!(ToolCallId);
string_id!(ModelCallId);
string_id!(JournalRecordId);
string_id!(CompactionId);
string_id!(TaskId);
string_id!(AgentId);
string_id!(LeaseId);
