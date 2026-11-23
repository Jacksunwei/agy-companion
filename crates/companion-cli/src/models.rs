use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{CliError, load_config_checked, resolve_cli_config_path};

pub const CURATED_MODELS: &[&str] = &["gemini-2.5-pro", "gemini-2.5-flash"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModelEntry {
    pub model_name: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModelsConfig {
    pub custom_models: Vec<CustomModelEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfigSnippet {
    pub model_provider: String,
    pub custom_models_config: CustomModelsConfig,
}

pub fn build_custom_models_config(
    port: u16,
    default_model: &str,
) -> Result<ModelsConfigSnippet, CliError> {
    if port == 0 {
        return Err(CliError::InvalidPort(0));
    }

    let mut ordered_models: Vec<String> = Vec::with_capacity(CURATED_MODELS.len() + 1);
    let trimmed_default = default_model.trim();
    if !trimmed_default.is_empty() {
        ordered_models.push(trimmed_default.to_string());
    }
    for curated in CURATED_MODELS {
        if !ordered_models.iter().any(|existing| existing == curated) {
            ordered_models.push((*curated).to_string());
        }
    }

    let custom_models = ordered_models
        .into_iter()
        .map(|model| CustomModelEntry {
            model_name: format!("gemini-api://http://127.0.0.1:{port}/models/{model}"),
            label: format!("AGY Companion ({model})"),
        })
        .collect();

    Ok(ModelsConfigSnippet {
        model_provider: "gemini".to_string(),
        custom_models_config: CustomModelsConfig { custom_models },
    })
}

pub fn format_models_output(
    snippet: &ModelsConfigSnippet,
    port: u16,
    json_only: bool,
) -> Result<String, CliError> {
    let pretty = serde_json::to_string_pretty(snippet)
        .map_err(|err| CliError::Serialization(err.to_string()))?;
    if json_only {
        Ok(format!("{pretty}\n"))
    } else {
        Ok(format!(
            "Paste the following into ~/.gemini/antigravity-cli/settings.json\n\
             (local proxy endpoint: http://127.0.0.1:{port}):\n\n\
             {pretty}\n"
        ))
    }
}

pub fn run_models(
    json_only: bool,
    port_override: Option<u16>,
    config_override: Option<PathBuf>,
    out: &mut impl Write,
) -> Result<(), CliError> {
    if port_override == Some(0) {
        return Err(CliError::InvalidPort(0));
    }

    let config_path = resolve_cli_config_path(config_override.as_deref())?;
    let config = load_config_checked(&config_path)?;
    let port = port_override.unwrap_or(config.listen_port);
    let snippet = build_custom_models_config(port, &config.gemini.default_model)?;
    let formatted = format_models_output(&snippet, port, json_only)?;

    out.write_all(formatted.as_bytes())?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{build_custom_models_config, format_models_output};
    use crate::CliError;

    #[test]
    fn models_command_emits_valid_custom_models_config_json() {
        let snippet = build_custom_models_config(4141, "gemini-2.5-pro")
            .expect("default port and model should succeed");
        let rendered =
            format_models_output(&snippet, 4141, true).expect("json formatting should succeed");

        let parsed: Value =
            serde_json::from_str(&rendered).expect("models --json output must be valid JSON");
        assert_eq!(parsed["modelProvider"], "gemini");

        let models = parsed["customModelsConfig"]["customModels"]
            .as_array()
            .expect("customModels must be an array");
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0]["modelName"],
            "gemini-api://http://127.0.0.1:4141/models/gemini-2.5-pro"
        );
        assert_eq!(models[0]["label"], "AGY Companion (gemini-2.5-pro)");
        assert_eq!(
            models[1]["modelName"],
            "gemini-api://http://127.0.0.1:4141/models/gemini-2.5-flash"
        );
        assert_eq!(models[1]["label"], "AGY Companion (gemini-2.5-flash)");
        assert!(!rendered.contains("AIza"));

        let human =
            format_models_output(&snippet, 4141, false).expect("human formatting should succeed");
        assert!(human.contains("~/.gemini/antigravity-cli/settings.json"));
        assert!(human.contains("gemini-api://http://127.0.0.1:4141/models/gemini-2.5-pro"));
    }

    #[test]
    fn models_includes_custom_default_model_and_deduplicates_curated_models() {
        let pro = build_custom_models_config(4141, "gemini-2.5-pro").expect("pro");
        assert_eq!(pro.custom_models_config.custom_models.len(), 2);
        assert_eq!(
            pro.custom_models_config.custom_models[0].model_name,
            "gemini-api://http://127.0.0.1:4141/models/gemini-2.5-pro"
        );
        assert_eq!(
            pro.custom_models_config.custom_models[1].model_name,
            "gemini-api://http://127.0.0.1:4141/models/gemini-2.5-flash"
        );

        let flash = build_custom_models_config(4141, "gemini-2.5-flash").expect("flash");
        assert_eq!(flash.custom_models_config.custom_models.len(), 2);
        assert_eq!(
            flash.custom_models_config.custom_models[0].model_name,
            "gemini-api://http://127.0.0.1:4141/models/gemini-2.5-flash"
        );
        assert_eq!(
            flash.custom_models_config.custom_models[1].model_name,
            "gemini-api://http://127.0.0.1:4141/models/gemini-2.5-pro"
        );

        let custom = build_custom_models_config(5151, "gemini-3.8-flash").expect("custom");
        assert_eq!(custom.custom_models_config.custom_models.len(), 3);
        assert_eq!(
            custom.custom_models_config.custom_models[0].model_name,
            "gemini-api://http://127.0.0.1:5151/models/gemini-3.8-flash"
        );
        assert_eq!(
            custom.custom_models_config.custom_models[0].label,
            "AGY Companion (gemini-3.8-flash)"
        );
        assert_eq!(
            custom.custom_models_config.custom_models[1].model_name,
            "gemini-api://http://127.0.0.1:5151/models/gemini-2.5-pro"
        );
        assert_eq!(
            custom.custom_models_config.custom_models[2].model_name,
            "gemini-api://http://127.0.0.1:5151/models/gemini-2.5-flash"
        );
    }

    #[test]
    fn models_rejects_port_zero() {
        let err =
            build_custom_models_config(0, "gemini-2.5-pro").expect_err("port 0 must be rejected");
        assert!(matches!(err, CliError::InvalidPort(0)));
    }
}
