//! Durable provider/model registry.
//!
//! `providers.json` is separate from `config.json` on purpose: it is a
//! ZCode-style registry with builtin presets, user-created providers, model
//! metadata and a default selection. The legacy single-provider fields in
//! `config.json` are migrated the first time the file is created.

use std::path::PathBuf;

use effibuddy_agent_core::{
    ApiFormat, DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT_TOKENS, MODEL_CONFIG_SCHEMA_VERSION,
    ModelCapabilities, ModelConfig, ModelConfigFile, ModelSelection, ProviderConfig, ResolvedModel,
    SessionState,
};

use crate::config::{AppConfig, base_dir};

pub struct ModelConfigStore {
    path: PathBuf,
    file: ModelConfigFile,
    /// Set when the on-disk file could not be parsed. The file is left intact;
    /// the in-memory registry falls back to builtins plus the legacy config.
    load_error: Option<String>,
}

impl ModelConfigStore {
    pub fn new(legacy: &AppConfig) -> Self {
        let path = Self::path();
        let mut store = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => match serde_json::from_str::<ModelConfigFile>(&contents) {
                    Ok(mut file) => {
                        let mut error = None;
                        if file.schema_version > MODEL_CONFIG_SCHEMA_VERSION {
                            error = Some(format!(
                                "providers.json 的版本 {} 高于当前支持的 {}；已使用内置配置，文件未被覆盖。",
                                file.schema_version, MODEL_CONFIG_SCHEMA_VERSION
                            ));
                            file = ModelConfigFile::with_builtins();
                            file.apply_legacy(&legacy.base_url, &legacy.api_key, &legacy.model);
                        }
                        file.normalize();
                        Self {
                            path: path.clone(),
                            file,
                            load_error: error,
                        }
                    }
                    Err(error) => {
                        let mut file = ModelConfigFile::with_builtins();
                        file.apply_legacy(&legacy.base_url, &legacy.api_key, &legacy.model);
                        file.normalize();
                        Self {
                            path: path.clone(),
                            file,
                            load_error: Some(format!(
                                "providers.json 无法解析（{error}）；已使用内置配置，原文件保留。"
                            )),
                        }
                    }
                },
                Err(error) => {
                    let mut file = ModelConfigFile::with_builtins();
                    file.apply_legacy(&legacy.base_url, &legacy.api_key, &legacy.model);
                    file.normalize();
                    Self {
                        path: path.clone(),
                        file,
                        load_error: Some(format!("无法读取 providers.json：{error}")),
                    }
                }
            }
        } else {
            let mut file = ModelConfigFile::with_builtins();
            file.apply_legacy(&legacy.base_url, &legacy.api_key, &legacy.model);
            file.normalize();
            let store = Self {
                path: path.clone(),
                file,
                load_error: None,
            };
            let _ = store.save();
            store
        };

        // Older files may lack newer builtin presets. Persist the merge so the
        // settings page does not show a different list on every launch.
        if store.load_error.is_none() {
            let before = serde_json::to_string(&store.file).unwrap_or_default();
            store.file.normalize();
            let after = serde_json::to_string(&store.file).unwrap_or_default();
            if before != after {
                let _ = store.save();
            }
        }
        store
    }

    pub fn path() -> PathBuf {
        base_dir().join("providers.json")
    }

    pub fn file(&self) -> &ModelConfigFile {
        &self.file
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn save(&self) -> Result<(), String> {
        let Some(parent) = self.path.parent() else {
            return Err("providers.json 路径无效".to_string());
        };
        std::fs::create_dir_all(parent).map_err(|error| format!("无法创建配置目录：{error}"))?;
        let json = serde_json::to_string_pretty(&self.file)
            .map_err(|error| format!("无法序列化 providers.json：{error}"))?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, json)
            .map_err(|error| format!("无法写入 {}：{error}", temporary.display()))?;
        if self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
        std::fs::rename(&temporary, &self.path)
            .map_err(|error| format!("无法完成 {} 写入：{error}", self.path.display()))
    }

    pub fn save_mut(&mut self) -> Result<(), String> {
        self.file.normalize();
        self.save()?;
        self.load_error = None;
        Ok(())
    }

    pub fn resolve_session(&self, session: &SessionState) -> ResolvedModel {
        self.file.resolve(
            session.provider_id.as_deref(),
            Some(session.model.as_str()),
            session.reasoning_level.as_deref(),
        )
    }

    pub fn resolved_for(&self, session: Option<&SessionState>) -> ResolvedModel {
        match session {
            Some(session) => self.resolve_session(session),
            None => self.file.resolve(None, None, None),
        }
    }

    /// Create a session that inherits the registry default selection.
    pub fn new_session_state(&self, workspace: PathBuf) -> SessionState {
        let resolved = self.file.resolve(None, None, None);
        let mut session = SessionState::new(workspace, resolved.model.id.clone());
        session.set_model_selection(
            resolved.provider.id.clone(),
            resolved.model.id.clone(),
            resolved.reasoning_level.clone(),
        );
        session
    }

    /// Attach a provider id to a transcript that only stored a model name.
    pub fn normalize_session(&self, session: &mut SessionState) {
        if session.provider_id.is_some() {
            return;
        }
        let resolved = self.file.resolve(
            None,
            Some(session.model.as_str()),
            session.reasoning_level.as_deref(),
        );
        session.provider_id = Some(resolved.provider.id.clone());
        session.reasoning_level = resolved.reasoning_level.clone();
    }

    /// The demo provider is used when explicitly requested, or when the selected
    /// provider has no usable credential *and* no other provider has a key. A
    /// selected provider without a key is otherwise an error at run time rather
    /// than a silent demo switch. Local keyless providers (Ollama) count as
    /// usable when selected.
    pub fn demo_mode(&self, use_demo: bool, resolved: &ResolvedModel) -> bool {
        use_demo || (!resolved.live && !self.file.has_any_api_key())
    }

    pub fn default_selection(&self) -> Option<ModelSelection> {
        self.file.default_selection.clone()
    }

    pub fn set_default(
        &mut self,
        provider_id: &str,
        model_id: &str,
        reasoning_level: Option<String>,
    ) -> Result<(), String> {
        let Some(provider) = self.file.provider(provider_id) else {
            return Err("供应商不存在".to_string());
        };
        if provider.model(model_id).is_none() {
            return Err("模型不存在".to_string());
        }
        self.file.default_selection = Some(ModelSelection {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            reasoning_level,
        });
        self.save_mut()
    }

    pub fn add_custom_provider(&mut self) -> Result<String, String> {
        let id = format!("custom-{}", effibuddy_agent_core::ids::short_id());
        let mut provider =
            ProviderConfig::new(&id, "自定义供应商", ApiFormat::OpenAiChatCompletions);
        provider.base_url = "https://api.openai.com/v1".to_string();
        self.file.providers.push(provider);
        self.save_mut()?;
        Ok(id)
    }

    pub fn remove_provider(&mut self, provider_id: &str) -> Result<(), String> {
        let Some(provider) = self.file.provider(provider_id) else {
            return Err("供应商不存在".to_string());
        };
        if provider.builtin {
            return Err("内置供应商不能删除，可以停用或清空模型。".to_string());
        }
        self.file.providers.retain(|item| item.id != provider_id);
        self.save_mut()
    }

    pub fn update_provider_connection(
        &mut self,
        provider_id: &str,
        name: String,
        api_format: ApiFormat,
        base_url: String,
        api_key: Option<String>,
        enabled: bool,
    ) -> Result<(), String> {
        let Some(provider) = self.file.provider_mut(provider_id) else {
            return Err("供应商不存在".to_string());
        };
        provider.name = name;
        provider.api_format = api_format;
        provider.base_url = base_url.trim().trim_end_matches('/').to_string();
        if let Some(api_key) = api_key {
            provider.api_key = api_key;
        }
        provider.enabled = enabled;
        self.save_mut()
    }

    pub fn add_model(&mut self, provider_id: &str, model: ModelConfig) -> Result<(), String> {
        let Some(provider) = self.file.provider_mut(provider_id) else {
            return Err("供应商不存在".to_string());
        };
        if provider.models.iter().any(|item| item.id == model.id) {
            return Err(format!("模型 {} 已存在", model.id));
        }
        let first = provider.models.is_empty();
        provider.models.push(model.clone());
        if first {
            self.file.default_selection = Some(ModelSelection {
                provider_id: provider_id.to_string(),
                model_id: model.id,
                reasoning_level: model.default_reasoning_level.clone(),
            });
        }
        self.save_mut()
    }

    pub fn update_model(
        &mut self,
        provider_id: &str,
        original_id: &str,
        model: ModelConfig,
    ) -> Result<(), String> {
        let Some(provider) = self.file.provider_mut(provider_id) else {
            return Err("供应商不存在".to_string());
        };
        if original_id != model.id && provider.models.iter().any(|item| item.id == model.id) {
            return Err(format!("模型 {} 已存在", model.id));
        }
        let Some(slot) = provider
            .models
            .iter_mut()
            .find(|item| item.id == original_id)
        else {
            return Err("模型不存在".to_string());
        };
        *slot = model.clone();
        if let Some(selection) = self.file.default_selection.as_mut() {
            if selection.provider_id == provider_id && selection.model_id == original_id {
                selection.model_id = model.id.clone();
                selection.reasoning_level = model.default_reasoning_level.clone();
            }
        }
        self.save_mut()
    }

    pub fn remove_model(&mut self, provider_id: &str, model_id: &str) -> Result<(), String> {
        let Some(index) = self
            .file
            .providers
            .iter()
            .position(|provider| provider.id == provider_id)
        else {
            return Err("供应商不存在".to_string());
        };
        let fallback = {
            let provider = &mut self.file.providers[index];
            provider.models.retain(|item| item.id != model_id);
            provider.default_model().cloned()
        };
        if self
            .file
            .default_selection
            .as_ref()
            .is_some_and(|selection| {
                selection.provider_id == provider_id && selection.model_id == model_id
            })
        {
            self.file.default_selection = fallback.map(|model| ModelSelection {
                provider_id: provider_id.to_string(),
                model_id: model.id.clone(),
                reasoning_level: model.default_reasoning_level.clone(),
            });
        }
        self.save_mut()
    }

    pub fn set_model_enabled(
        &mut self,
        provider_id: &str,
        model_id: &str,
        enabled: bool,
    ) -> Result<(), String> {
        let Some(index) = self
            .file
            .providers
            .iter()
            .position(|provider| provider.id == provider_id)
        else {
            return Err("供应商不存在".to_string());
        };
        let fallback = {
            let provider = &mut self.file.providers[index];
            let Some(model) = provider.model_mut(model_id) else {
                return Err("模型不存在".to_string());
            };
            model.enabled = enabled;
            if enabled {
                None
            } else {
                provider.default_model().cloned()
            }
        };
        if !enabled
            && self
                .file
                .default_selection
                .as_ref()
                .is_some_and(|selection| {
                    selection.provider_id == provider_id && selection.model_id == model_id
                })
        {
            self.file.default_selection = fallback.map(|model| ModelSelection {
                provider_id: provider_id.to_string(),
                model_id: model.id.clone(),
                reasoning_level: model.default_reasoning_level.clone(),
            });
        }
        self.save_mut()
    }

    pub fn set_secret(&mut self, provider_id: &str, api_key: String) -> Result<(), String> {
        let Some(provider) = self.file.provider_mut(provider_id) else {
            return Err("供应商不存在".to_string());
        };
        provider.api_key = api_key;
        self.save_mut()
    }

    /// Add models returned by the provider's `/models` endpoint. Existing
    /// entries are never overwritten, so user-edited metadata wins.
    pub fn merge_fetched_models(
        &mut self,
        provider_id: &str,
        ids: &[String],
        template: Option<&ModelConfig>,
    ) -> Result<usize, String> {
        let Some(index) = self
            .file
            .providers
            .iter()
            .position(|provider| provider.id == provider_id)
        else {
            return Err("供应商不存在".to_string());
        };
        let (added, first_model) = {
            let provider = &mut self.file.providers[index];
            let was_empty = provider.models.is_empty();
            let mut added = 0;
            for id in ids {
                if provider.model(id).is_some() {
                    continue;
                }
                let mut model = ModelConfig::new(id);
                if let Some(template) = template.or_else(|| provider.default_model()) {
                    model.context_window = template.context_window;
                    model.max_output_tokens = template.max_output_tokens;
                    model.capabilities = template.capabilities.clone();
                }
                provider.models.push(model);
                added += 1;
            }
            let first_model = if was_empty {
                provider.default_model().cloned()
            } else {
                None
            };
            (added, first_model)
        };
        if let Some(model) = first_model {
            self.file.default_selection = Some(ModelSelection {
                provider_id: provider_id.to_string(),
                model_id: model.id.clone(),
                reasoning_level: model.default_reasoning_level.clone(),
            });
        }
        self.save_mut()?;
        Ok(added)
    }
}

/// Conservative model metadata used when a user adds a model by hand. Both
/// budgets are configurable per model in the editor; these are only the seeds.
pub fn default_model_metadata(id: &str, name: &str) -> ModelConfig {
    ModelConfig::new(id)
        .with_name(name)
        .with_context(DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT_TOKENS)
        .with_capabilities(ModelCapabilities::tool())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy() -> AppConfig {
        AppConfig::default()
    }

    #[test]
    fn legacy_migration_adds_key_and_selection() {
        let mut file = ModelConfigFile::with_builtins();
        file.apply_legacy("https://api.deepseek.com", "sk-test", "deepseek-reasoner");
        assert_eq!(file.provider("deepseek").unwrap().api_key, "sk-test");
        let selection = file.default_selection.unwrap();
        assert_eq!(selection.provider_id, "deepseek");
        assert_eq!(selection.model_id, "deepseek-reasoner");
    }

    #[test]
    fn custom_provider_round_trip() {
        let mut file = ModelConfigFile::with_builtins();
        let mut provider = ProviderConfig::new("custom-test", "Custom", ApiFormat::OpenAiResponses);
        provider.base_url = "https://example.test/v1".to_string();
        provider
            .models
            .push(default_model_metadata("m1", "Model One"));
        file.providers.push(provider);
        file.normalize();
        let resolved = file.resolve(Some("custom-test"), Some("m1"), None);
        assert_eq!(resolved.provider.id, "custom-test");
        assert_eq!(resolved.model.id, "m1");
        assert_eq!(resolved.model.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn default_metadata_is_valid() {
        let model = default_model_metadata("m", "M");
        assert!(model.enabled);
        assert!(model.context_window > 0);
        assert!(model.max_output_tokens > 0);
        assert!(model.capabilities.tool_call);
    }

    #[test]
    fn legacy_config_is_only_used_for_metadata() {
        let config = legacy();
        assert_eq!(config.model, "deepseek-flash");
    }
}
