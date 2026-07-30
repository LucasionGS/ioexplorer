//! Everything spotlight derives from `[spotlight]`, resolved as one unit.

use std::rc::Rc;

use crate::{
    config::SpotlightConfig,
    spotlight::{ai::AiProvider, passwords, prefixes, software::Catalog, vpn},
};

/// The config section and the tables built from it, swapped together.
///
/// Grouping these is a correctness requirement rather than tidiness:
/// `PrefixKind::Ai` carries an index into `ai_providers`, so a table resolved
/// from one config paired with a provider list from another would send a prompt
/// to the wrong model. Holding them in one immutable bundle makes that
/// unrepresentable — a reload replaces the whole thing or none of it.
pub struct SpotlightRuntime {
    pub config: SpotlightConfig,
    pub prefixes: prefixes::PrefixTable,
    pub software: Catalog,
    pub ai_providers: Vec<AiProvider>,
    /// The password manager in play, kept here as well as inside the prefix it
    /// produced. The window's tick sends a debounced vault query without a
    /// prefix in hand, and re-scanning `PATH` on every tick to find out who to
    /// ask would be absurd.
    pub passwords: Option<passwords::Provider>,
}

impl SpotlightRuntime {
    /// Resolves the whole runtime from a config section.
    ///
    /// Scans `PATH` for a VPN and a password-manager client, so unlike
    /// [`prefixes::resolve_with_ai`] this is not pure over its input.
    pub fn resolve(config: SpotlightConfig) -> Rc<Self> {
        let vpn = vpn::resolve(&config.vpn);
        let passwords = passwords::resolve(&config.passwords);
        let (prefixes, ai_providers) = prefixes::resolve_with_ai(&config, vpn, passwords);
        let software = Catalog::resolve(&config.software);

        Rc::new(Self {
            config,
            prefixes,
            software,
            ai_providers,
            passwords,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotlight::prefixes::PrefixKind;

    #[test]
    fn ai_prefixes_index_inside_the_provider_list() {
        // The invariant the bundle exists to protect. Resolving scans `PATH`,
        // so assert the relationship rather than a specific set of prefixes.
        let config = toml::from_str::<SpotlightConfig>(
            r#"
[[ai]]
label = "Claude"
provider = "anthropic"
prefix = "ai"
model = "claude-sonnet-5"

[[ai]]
label = "Local"
provider = "ollama"
prefix = "local"
model = "llama"
"#,
        )
        .expect("valid spotlight config");

        let runtime = SpotlightRuntime::resolve(config);

        let ai_prefixes: Vec<_> = runtime
            .prefixes
            .all()
            .iter()
            .filter_map(|prefix| match prefix.kind {
                PrefixKind::Ai(index) => Some(index),
                _ => None,
            })
            .collect();

        assert_eq!(ai_prefixes.len(), 2);
        for index in ai_prefixes {
            assert!(
                runtime.ai_providers.get(index).is_some(),
                "prefix points at provider {index}, which does not exist"
            );
        }
    }
}
