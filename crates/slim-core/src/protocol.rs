use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingMode {
    #[default]
    Auto,
    ReadOnly,
    Plan,
    /// Full access like Auto, but the model acts by writing one JavaScript
    /// program per step (`codemode`) instead of calling tools one by one.
    Code,
}

impl OperatingMode {
    pub fn allows_mutation(self) -> bool {
        matches!(self, Self::Auto | Self::Code)
    }

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::ReadOnly,
            Self::ReadOnly => Self::Plan,
            Self::Plan => Self::Code,
            Self::Code => Self::Auto,
        }
    }
}
