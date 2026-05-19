use crate::{Result, config::Provider, openai::response::ModelList};
use std::collections::HashSet;

/// Default OpenAI-compatible model identifiers exposed by the project.
pub const OPENCLAW_CODEX_MODELS: &[&str] = &[
    "gpt-5.1",
    "gpt-5.1-codex-max",
    "gpt-5.1-codex-mini",
    "gpt-5.2",
    "gpt-5.2-codex",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
];

/// Default Grok model identifiers exposed by the project.
pub const GROK_MODELS: &[&str] = &["grok-4.3", "grok-4.3-fast", "grok-4"];

/// Resolves the default model identifiers into a trimmed, de-duplicated list.
#[must_use]
pub fn resolve_model_ids() -> Vec<String> {
    resolve_model_ids_for_provider(Provider::Codex)
}

/// Resolves model identifiers for the selected provider.
#[must_use]
pub fn resolve_model_ids_for_provider(provider: Provider) -> Vec<String> {
    let ids = match provider {
        Provider::Codex => OPENCLAW_CODEX_MODELS,
        Provider::Grok => GROK_MODELS,
    };
    normalize_model_ids(ids.iter().map(ToString::to_string))
}

/// Builds a [`ModelList`] from the default model identifiers.
///
/// # Errors
///
/// This currently forwards construction through [`ModelList::from_ids`] and is
/// fallible only to preserve the crate-wide result-based call sites.
pub fn resolve_model_list() -> Result<ModelList> {
    resolve_model_list_for_provider(Provider::Codex)
}

/// Builds a [`ModelList`] for the selected provider.
///
/// # Errors
///
/// This currently forwards construction through [`ModelList::from_ids`] and is
/// fallible only to preserve the crate-wide result-based call sites.
pub fn resolve_model_list_for_provider(provider: Provider) -> Result<ModelList> {
    Ok(ModelList::from_ids(resolve_model_ids_for_provider(
        provider,
    )))
}

fn normalize_model_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_openclaw_codex_models() {
        let ids = resolve_model_ids();
        let expected = OPENCLAW_CODEX_MODELS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        assert_eq!(ids, expected);
    }

    #[test]
    fn defaults_include_gpt_55_models() {
        let ids = resolve_model_ids();

        assert!(ids.iter().any(|id| id == "gpt-5.5"));
        assert!(!ids.iter().any(|id| id == "gpt-5.5-mini"));
    }

    #[test]
    fn grok_defaults_include_grok_models() {
        let ids = resolve_model_ids_for_provider(Provider::Grok);

        assert!(ids.iter().any(|id| id == "grok-4.3"));
        assert!(!ids.iter().any(|id| id == "gpt-5.5"));
    }
}
