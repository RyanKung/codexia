use crate::{Error, Result, openai::response::ModelList};
use serde::Deserialize;
use std::{collections::HashSet, fs, path::Path};

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
    "gpt-5.5-mini",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelOptions {
    pub replacement_models: Vec<String>,
    pub extra_models: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModelFileObject {
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    extra_models: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ModelFile {
    List(Vec<String>),
    Object(ModelFileObject),
}

impl ModelOptions {
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let file = serde_json::from_str::<ModelFile>(&raw)?;
        Ok(match file {
            ModelFile::List(models) => Self {
                replacement_models: models,
                extra_models: Vec::new(),
            },
            ModelFile::Object(object) => Self {
                replacement_models: object.models,
                extra_models: object.extra_models,
            },
        })
    }
}

pub fn resolve_model_ids(
    file_options: Option<ModelOptions>,
    cli_options: ModelOptions,
) -> Vec<String> {
    let mut ids = file_options
        .as_ref()
        .filter(|options| has_non_empty_model(&options.replacement_models))
        .map(|options| options.replacement_models.clone())
        .unwrap_or_else(|| {
            OPENCLAW_CODEX_MODELS
                .iter()
                .map(ToString::to_string)
                .collect()
        });

    if has_non_empty_model(&cli_options.replacement_models) {
        ids = cli_options.replacement_models.clone();
    }

    if let Some(options) = file_options {
        ids.extend(options.extra_models);
    }
    ids.extend(cli_options.extra_models);

    normalize_model_ids(ids)
}

pub fn resolve_model_list(
    models_file: Option<&Path>,
    cli_options: ModelOptions,
) -> Result<ModelList> {
    let file_options = models_file
        .map(ModelOptions::from_file)
        .transpose()
        .map_err(|error| Error::config(format!("failed to load models file: {error}")))?;
    Ok(ModelList::from_ids(resolve_model_ids(
        file_options,
        cli_options,
    )))
}

fn has_non_empty_model(models: &[String]) -> bool {
    models.iter().any(|model| !model.trim().is_empty())
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
        let ids = resolve_model_ids(None, ModelOptions::default());
        let expected = OPENCLAW_CODEX_MODELS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        assert_eq!(ids, expected);
    }

    #[test]
    fn defaults_include_gpt_55_models() {
        let ids = resolve_model_ids(None, ModelOptions::default());

        assert!(ids.iter().any(|id| id == "gpt-5.5"));
        assert!(ids.iter().any(|id| id == "gpt-5.5-mini"));
    }

    #[test]
    fn cli_replacement_overrides_defaults_and_dedupes() {
        let ids = resolve_model_ids(
            None,
            ModelOptions {
                replacement_models: vec!["custom".into(), " custom ".into(), "".into()],
                extra_models: vec![],
            },
        );

        assert_eq!(ids, vec!["custom"]);
    }

    #[test]
    fn file_replacement_and_cli_extra_are_combined() {
        let ids = resolve_model_ids(
            Some(ModelOptions {
                replacement_models: vec!["file-model".into()],
                extra_models: vec!["file-extra".into()],
            }),
            ModelOptions {
                replacement_models: vec![],
                extra_models: vec!["cli-extra".into()],
            },
        );

        assert_eq!(ids, vec!["file-model", "file-extra", "cli-extra"]);
    }

    #[test]
    fn parses_models_file_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        fs::write(
            &path,
            r#"{"models":["a"],"extra_models":["b","a","  c  "]}"#,
        )
        .unwrap();

        let list = resolve_model_list(Some(&path), ModelOptions::default()).unwrap();
        let ids = list
            .data
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["a", "b", "c"]);
    }
}
