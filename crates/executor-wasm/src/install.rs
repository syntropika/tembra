//! Installation chooses dependency providers and permissions; artifacts carry neither authority.
use crate::Grants;
use serde::Deserialize;
use std::collections::BTreeMap;
use tembra_contracts::OrbProvider;

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Install {
    pub(crate) requires: BTreeMap<String, OrbProvider>,
    pub grants: Grants,
}

impl Install {
    /// Select one installed Orb for a WIT import; WIT supplies its methods and signatures.
    pub fn requires(
        mut self,
        import: impl Into<String>,
        orb: impl Into<String>,
        api_version: u32,
    ) -> Self {
        self.requires.insert(
            import.into(),
            OrbProvider {
                orb: orb.into(),
                api_version,
            },
        );
        self
    }
}
