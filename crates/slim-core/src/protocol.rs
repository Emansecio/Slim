use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingMode {
    #[default]
    Auto,
    ReadOnly,
    Plan,
    Jev,
}

impl OperatingMode {
    pub fn allows_mutation(self) -> bool {
        matches!(self, Self::Auto | Self::Jev)
    }

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::ReadOnly,
            Self::ReadOnly => Self::Plan,
            Self::Plan => Self::Jev,
            Self::Jev => Self::Auto,
        }
    }
}
