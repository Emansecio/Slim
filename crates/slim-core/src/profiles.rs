#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileId {
    Default,
    Fast,
    Deep,
    Compact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Profile {
    pub id: ProfileId,
    pub provider: String,
    pub model: String,
    pub effort: String,
}

impl Profile {
    pub fn builtin(id: ProfileId) -> Self {
        let (provider, model, effort) = match id {
            ProfileId::Default => ("openai-compatible", "default-model", "balanced"),
            ProfileId::Fast => ("openai-compatible", "fast-model", "low"),
            ProfileId::Deep => ("openai-compatible", "deep-model", "high"),
            ProfileId::Compact => ("openai-compatible", "compact-model", "minimal"),
        };

        Self {
            id,
            provider: provider.into(),
            model: model.into(),
            effort: effort.into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProfileCatalog;

impl ProfileCatalog {
    pub fn get(&self, id: ProfileId) -> Profile {
        Profile::builtin(id)
    }
}
