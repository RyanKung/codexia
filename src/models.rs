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

/// Default Kiro model identifiers exposed by the local Kiro CLI model list.
pub const KIRO_MODELS: &[&str] = &[
    "auto",
    "claude-opus-4.7",
    "claude-opus-4.6",
    "claude-sonnet-4.6",
    "claude-opus-4.5",
    "claude-sonnet-4.5",
    "claude-sonnet-4",
    "claude-haiku-4.5",
    "deepseek-3.2",
    "minimax-m2.5",
    "minimax-m2.1",
    "glm-5",
    "qwen3-coder-next",
];

/// Default Cursor Agent model identifiers exposed by rotom.
pub const CURSOR_MODELS: &[&str] = &[
    "cursor/auto",
    "cursor/gpt-5",
    "cursor/sonnet-4",
    "cursor/sonnet-4-thinking",
];

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
        Provider::Kiro => KIRO_MODELS,
        Provider::Cursor => CURSOR_MODELS,
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
    let owner = model_owner(provider);
    Ok(ModelList::from_id_owners(
        resolve_model_ids_for_provider(provider)
            .into_iter()
            .map(|id| (id, owner)),
    ))
}

/// Returns the provider implied by a model identifier.
#[must_use]
pub fn provider_for_model(model: &str) -> Provider {
    let normalized = model.strip_prefix("openai-codex/").unwrap_or(model);
    if normalized.starts_with("cursor/") {
        return Provider::Cursor;
    }
    let normalized = normalized
        .strip_prefix("xai/")
        .or_else(|| normalized.strip_prefix("grok/"))
        .or_else(|| normalized.strip_prefix("kiro/"))
        .unwrap_or(normalized);
    if normalized.starts_with("grok-") {
        Provider::Grok
    } else if normalized == "auto"
        || normalized.starts_with("claude-")
        || normalized.starts_with("deepseek-")
        || normalized.starts_with("minimax-")
        || normalized.starts_with("glm-")
        || normalized.starts_with("qwen")
    {
        Provider::Kiro
    } else if normalized.starts_with("sonnet-") || normalized == "opus" {
        Provider::Cursor
    } else {
        Provider::Codex
    }
}

/// Builds a [`ModelList`] containing all models for the supplied providers.
///
/// # Errors
///
/// This currently forwards construction through [`ModelList::from_ids`] and is
/// fallible only to preserve the crate-wide result-based call sites.
pub fn resolve_model_list_for_providers(providers: &[Provider]) -> Result<ModelList> {
    let mut seen = HashSet::new();
    let ids = providers
        .iter()
        .copied()
        .flat_map(|provider| {
            let owner = model_owner(provider);
            resolve_model_ids_for_provider(provider)
                .into_iter()
                .map(move |id| (id, owner))
        })
        .filter(|(id, _)| seen.insert(id.clone()))
        .collect::<Vec<_>>();
    Ok(ModelList::from_id_owners(ids))
}

const fn model_owner(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "openai-codex",
        Provider::Grok => "xai",
        Provider::Kiro => "kiro",
        Provider::Cursor => "cursor",
    }
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

    #[test]
    fn kiro_defaults_match_cli_models() {
        let ids = resolve_model_ids_for_provider(Provider::Kiro);

        assert!(ids.iter().any(|id| id == "auto"));
        assert!(ids.iter().any(|id| id == "claude-sonnet-4.5"));
        assert!(ids.iter().any(|id| id == "qwen3-coder-next"));
    }

    #[test]
    fn cursor_defaults_use_provider_prefixes() {
        let ids = resolve_model_ids_for_provider(Provider::Cursor);

        assert!(ids.iter().any(|id| id == "cursor/auto"));
        assert!(ids.iter().any(|id| id == "cursor/gpt-5"));
        assert!(ids.iter().any(|id| id == "cursor/sonnet-4"));
    }

    #[test]
    fn provider_detection_handles_prefixed_grok_models() {
        assert_eq!(provider_for_model("grok-4.3"), Provider::Grok);
        assert_eq!(provider_for_model("xai/grok-4.3"), Provider::Grok);
        assert_eq!(provider_for_model("grok/grok-4.3"), Provider::Grok);
        assert_eq!(provider_for_model("openai-codex/grok-4.3"), Provider::Grok);
        assert_eq!(provider_for_model("openai-codex/gpt-5.5"), Provider::Codex);
        assert_eq!(provider_for_model("kiro/auto"), Provider::Kiro);
        assert_eq!(provider_for_model("cursor/gpt-5"), Provider::Cursor);
        assert_eq!(provider_for_model("sonnet-4"), Provider::Cursor);
        assert_eq!(provider_for_model("claude-sonnet-4.5"), Provider::Kiro);
    }

    #[test]
    fn model_list_owners_match_provider() {
        let models = resolve_model_list_for_providers(&[
            Provider::Codex,
            Provider::Grok,
            Provider::Kiro,
            Provider::Cursor,
        ])
        .unwrap();

        let owner_for = |id: &str| {
            models
                .data
                .iter()
                .find(|model| model.id == id)
                .map(|model| model.owned_by)
        };

        assert_eq!(owner_for("gpt-5.5"), Some("openai-codex"));
        assert_eq!(owner_for("grok-4.3"), Some("xai"));
        assert_eq!(owner_for("auto"), Some("kiro"));
        assert_eq!(owner_for("cursor/gpt-5"), Some("cursor"));
    }
}
