use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingMode {
    #[default]
    Auto,
    ReadOnly,
    Plan,
}

impl OperatingMode {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::ReadOnly,
            Self::ReadOnly => Self::Plan,
            Self::Plan => Self::Auto,
        }
    }
}
