//! Provider and model registry.
//!
//! The registry is the durable description of "what can run the agent": a list
//! of providers (endpoint + credential + wire format) and the models exposed by
//! each one. It intentionally contains no UI state; the desktop app persists it
//! as `providers.json` and rebuilds a [`crate::ModelProvider`] for every turn.
//!
//! Resolution is deliberately generous: a session may reference a model that is
//! no longer in the registry (renamed upstream, removed by a user, loaded from
//! an old transcript). In that case we synthesize metadata with conservative
//! defaults instead of failing the turn.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::provider::ModelProvider;
use crate::providers::{AnthropicConfig, AnthropicMessagesProvider};
use crate::providers::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::providers::{OpenAiResponsesConfig, OpenAiResponsesProvider};

pub const MODEL_CONFIG_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_PROVIDER_ID: &str = "deepseek";
pub const DEFAULT_MODEL_ID: &str = "deepseek-flash";
pub const DEFAULT_CONTEXT_WINDOW: usize = 128_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ApiFormat {
    #[default]
    #[serde(rename = "openai-chat-completions")]
    OpenAiChatCompletions,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
}

impl ApiFormat {
    pub fn label(self) -> &'static str {
        match self {
            ApiFormat::OpenAiChatCompletions => "OpenAI Chat Completions",
            ApiFormat::AnthropicMessages => "Anthropic Messages",
            ApiFormat::OpenAiResponses => "OpenAI Responses",
        }
    }

    pub fn endpoint_hint(self) -> &'static str {
        match self {
            ApiFormat::OpenAiChatCompletions => "/chat/completions",
            ApiFormat::AnthropicMessages => "/v1/messages",
            ApiFormat::OpenAiResponses => "/responses",
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_context_window() -> usize {
    DEFAULT_CONTEXT_WINDOW
}

fn default_max_output_tokens() -> u32 {
    DEFAULT_MAX_OUTPUT_TOKENS
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    #[serde(default = "default_true")]
    pub tool_call: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub json_schema: bool,
    #[serde(default)]
    pub web_search: bool,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            tool_call: true,
            vision: false,
            reasoning: false,
            json_schema: false,
            web_search: false,
        }
    }
}

impl ModelCapabilities {
    pub fn tool() -> Self {
        Self::default()
    }

    pub fn vision() -> Self {
        Self {
            vision: true,
            ..Self::default()
        }
    }

    pub fn reasoning() -> Self {
        Self {
            reasoning: true,
            ..Self::default()
        }
    }

    pub fn full() -> Self {
        Self {
            tool_call: true,
            vision: true,
            reasoning: true,
            json_schema: true,
            web_search: false,
        }
    }

    pub fn labels(&self) -> Vec<&'static str> {
        let mut labels = Vec::new();
        if self.tool_call {
            labels.push("工具");
        }
        if self.vision {
            labels.push("视觉");
        }
        if self.reasoning {
            labels.push("推理");
        }
        if self.json_schema {
            labels.push("JSON Schema");
        }
        if self.web_search {
            labels.push("联网");
        }
        labels
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningLevel {
    pub value: String,
    #[serde(default)]
    pub label: String,
}

impl ReasoningLevel {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }

    pub fn display_label(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.value
        } else {
            &self.label
        }
    }
}

pub fn standard_reasoning_levels() -> Vec<ReasoningLevel> {
    vec![
        ReasoningLevel::new("low", "低"),
        ReasoningLevel::new("medium", "中"),
        ReasoningLevel::new("high", "高"),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_context_window")]
    pub context_window: usize,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
}

impl ModelConfig {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            enabled: true,
            context_window: DEFAULT_CONTEXT_WINDOW,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            capabilities: ModelCapabilities::default(),
            reasoning_levels: Vec::new(),
            default_reasoning_level: None,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_context(mut self, context_window: usize, max_output_tokens: u32) -> Self {
        self.context_window = context_window;
        self.max_output_tokens = max_output_tokens;
        self
    }

    pub fn with_capabilities(mut self, capabilities: ModelCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_reasoning_levels(mut self, levels: Vec<ReasoningLevel>) -> Self {
        self.default_reasoning_level = levels.first().map(|level| level.value.clone());
        self.reasoning_levels = levels;
        self
    }

    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    pub fn supports_reasoning_level(&self, value: &str) -> bool {
        self.reasoning_levels
            .iter()
            .any(|level| level.value == value)
    }

    pub fn resolved_reasoning_level(&self, requested: Option<&str>) -> Option<String> {
        if let Some(requested) = requested.filter(|value| self.supports_reasoning_level(value)) {
            return Some(requested.to_string());
        }
        if let Some(default) = self
            .default_reasoning_level
            .as_deref()
            .filter(|value| self.supports_reasoning_level(value))
        {
            return Some(default.to_string());
        }
        self.reasoning_levels
            .first()
            .map(|level| level.value.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub api_format: ApiFormat,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

impl ProviderConfig {
    pub fn new(id: impl Into<String>, name: impl Into<String>, api_format: ApiFormat) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            builtin: false,
            enabled: true,
            api_format,
            base_url: String::new(),
            api_key: String::new(),
            headers: BTreeMap::new(),
            models: Vec::new(),
        }
    }

    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    pub fn model(&self, model_id: &str) -> Option<&ModelConfig> {
        self.models.iter().find(|model| model.id == model_id)
    }

    pub fn model_mut(&mut self, model_id: &str) -> Option<&mut ModelConfig> {
        self.models.iter_mut().find(|model| model.id == model_id)
    }

    pub fn enabled_models(&self) -> impl Iterator<Item = &ModelConfig> {
        self.models.iter().filter(|model| model.enabled)
    }

    pub fn default_model(&self) -> Option<&ModelConfig> {
        self.enabled_models().next().or_else(|| self.models.first())
    }

    pub fn effective_api_key(&self) -> String {
        let stored = self.api_key.trim();
        if !stored.is_empty() {
            return stored.to_string();
        }
        if let Some(value) = builtin_env_key(&self.id)
            .and_then(|name| std::env::var(name).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return value;
        }
        if self.id == DEFAULT_PROVIDER_ID {
            if let Some(key) = legacy_deepseek_key() {
                return key;
            }
        }
        String::new()
    }

    /// Local endpoints (Ollama, vLLM, ...) do not need a key. Everything else
    /// reports a credential requirement so the UI can warn before a failed run.
    pub fn requires_api_key(&self) -> bool {
        let base = self.base_url.to_ascii_lowercase();
        if self.id == "ollama" {
            return false;
        }
        !(base.contains("localhost")
            || base.contains("127.0.0.1")
            || base.contains("0.0.0.0")
            || base.contains("[::1]"))
    }

    pub fn has_credentials(&self) -> bool {
        !self.requires_api_key() || !self.effective_api_key().is_empty()
    }

    pub fn base_url_or<'a>(&'a self, fallback: &'a str) -> &'a str {
        let trimmed = self.base_url.trim();
        if trimmed.is_empty() {
            fallback
        } else {
            trimmed
        }
    }
}

fn builtin_env_key(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "zai" => Some("ZAI_API_KEY"),
        "bigmodel" => Some("BIGMODEL_API_KEY"),
        "moonshot" => Some("MOONSHOT_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "siliconflow" => Some("SILICONFLOW_API_KEY"),
        _ => None,
    }
}

fn legacy_deepseek_key() -> Option<String> {
    std::env::var("EFFIBUDDY_API_KEY")
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<String>,
}

impl ModelSelection {
    pub fn new(provider_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            reasoning_level: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfigFile {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub default_selection: Option<ModelSelection>,
}

fn default_schema_version() -> u32 {
    MODEL_CONFIG_SCHEMA_VERSION
}

impl Default for ModelConfigFile {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl ModelConfigFile {
    pub fn with_builtins() -> Self {
        Self {
            schema_version: MODEL_CONFIG_SCHEMA_VERSION,
            providers: builtin_providers(),
            default_selection: Some(ModelSelection::new(DEFAULT_PROVIDER_ID, DEFAULT_MODEL_ID)),
        }
    }

    pub fn provider(&self, provider_id: &str) -> Option<&ProviderConfig> {
        self.providers
            .iter()
            .find(|provider| provider.id == provider_id)
    }

    pub fn provider_mut(&mut self, provider_id: &str) -> Option<&mut ProviderConfig> {
        self.providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
    }

    /// Add any builtin provider that is missing from an older file. Existing
    /// providers (including user edits and deleted models) are left untouched.
    pub fn merge_missing_builtins(&mut self) {
        for builtin in builtin_providers() {
            if self.provider(&builtin.id).is_none() {
                self.providers.push(builtin);
            }
        }
    }

    /// Keep the default selection pointing at a real, enabled provider/model.
    pub fn normalize(&mut self) {
        self.merge_missing_builtins();
        if self.providers.is_empty() {
            self.providers = builtin_providers();
        }
        let mut current = self.default_selection.take();
        if let Some(selection) = &current {
            let valid = self
                .provider(&selection.provider_id)
                .is_some_and(|provider| {
                    provider.enabled && provider.model(&selection.model_id).is_some()
                });
            if !valid {
                current = None;
            }
        }
        if current.is_none() {
            current = self
                .providers
                .iter()
                .find(|provider| provider.enabled && provider.has_credentials())
                .or_else(|| self.providers.iter().find(|provider| provider.enabled))
                .and_then(|provider| {
                    provider
                        .default_model()
                        .map(|model| ModelSelection::new(provider.id.clone(), model.id.clone()))
                });
        }
        self.default_selection = current;
    }

    /// Resolve a session's selection, falling back through the default
    /// selection and finally the first usable provider.
    pub fn resolve(
        &self,
        provider_id: Option<&str>,
        model_id: Option<&str>,
        reasoning_level: Option<&str>,
    ) -> ResolvedModel {
        let mut provider = provider_id
            .and_then(|id| self.provider(id))
            .filter(|provider| provider.enabled);

        // Old sessions only stored a model id. Prefer the provider that
        // actually exposes it instead of assuming the default provider.
        if provider.is_none() {
            if let Some(model_id) = model_id {
                provider = self
                    .provider_for_model(model_id)
                    .filter(|provider| provider.enabled);
            }
        }
        if provider.is_none() {
            provider = self
                .default_selection
                .as_ref()
                .and_then(|selection| self.provider(&selection.provider_id))
                .filter(|provider| provider.enabled);
        }
        if provider.is_none() {
            provider = self
                .providers
                .iter()
                .find(|provider| provider.enabled && provider.has_credentials())
                .or_else(|| self.providers.iter().find(|provider| provider.enabled))
                .or_else(|| self.providers.first());
        }

        let Some(provider) = provider else {
            // A registry with no providers can still resolve to a synthetic
            // DeepSeek entry; the app will fall back to the demo provider when
            // it has no credentials.
            let provider = builtin_providers()
                .into_iter()
                .find(|provider| provider.id == DEFAULT_PROVIDER_ID)
                .expect("builtin DeepSeek provider");
            return ResolvedModel::new(provider, None, reasoning_level);
        };

        let requested_model = model_id
            .map(str::trim)
            .filter(|model_id| !model_id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.default_selection
                    .as_ref()
                    .filter(|selection| selection.provider_id == provider.id)
                    .map(|selection| selection.model_id.clone())
            });

        ResolvedModel::new(
            provider.clone(),
            requested_model.as_deref(),
            reasoning_level,
        )
    }

    /// Apply the pre-registry `AppConfig` (base URL / key / model) the first
    /// time `providers.json` is created.
    pub fn apply_legacy(&mut self, base_url: &str, api_key: &str, model: &str) {
        let base_url = base_url.trim().trim_end_matches('/');
        let api_key = api_key.trim();
        let model = model.trim();

        if let Some(provider) = self.providers.iter_mut().find(|provider| {
            provider.base_url.trim().trim_end_matches('/') == base_url && !base_url.is_empty()
        }) {
            if !api_key.is_empty() {
                provider.api_key = api_key.to_string();
            }
            if !model.is_empty() {
                let exists = provider.model(model).is_some();
                if !exists {
                    let mut fallback = ModelConfig::new(model);
                    if let Some(template) = provider.models.first() {
                        fallback.context_window = template.context_window;
                        fallback.max_output_tokens = template.max_output_tokens;
                        fallback.capabilities = template.capabilities.clone();
                    }
                    provider.models.push(fallback);
                }
                self.default_selection = Some(ModelSelection::new(provider.id.clone(), model));
            }
            return;
        }

        // Unknown endpoint: preserve the exact old behavior as a custom
        // provider instead of silently changing the user's base URL.
        let mut provider = ProviderConfig::new(
            "migrated",
            "迁移的模型服务",
            ApiFormat::OpenAiChatCompletions,
        );
        provider.base_url = base_url.to_string();
        provider.api_key = api_key.to_string();
        if !model.is_empty() {
            provider.models.push(ModelConfig::new(model));
        }
        self.providers.insert(0, provider);
        if !model.is_empty() {
            self.default_selection = Some(ModelSelection::new("migrated", model));
        }
    }

    /// Resolve a provider id for the UI when an old session has only a model id.
    pub fn provider_for_model(&self, model_id: &str) -> Option<&ProviderConfig> {
        self.providers
            .iter()
            .find(|provider| provider.enabled && provider.model(model_id).is_some())
            .or_else(|| {
                self.default_selection
                    .as_ref()
                    .and_then(|selection| self.provider(&selection.provider_id))
            })
    }

    pub fn has_live_credentials(&self) -> bool {
        self.providers
            .iter()
            .any(|provider| provider.enabled && provider.has_credentials())
    }

    /// True when at least one provider has an actual API key (stored or from
    /// the environment). Local keyless providers are intentionally excluded -
    /// they are usable only when explicitly selected.
    pub fn has_any_api_key(&self) -> bool {
        self.providers
            .iter()
            .any(|provider| provider.enabled && !provider.effective_api_key().trim().is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider: ProviderConfig,
    pub model: ModelConfig,
    pub reasoning_level: Option<String>,
    /// True when the provider has a usable credential (or does not need one).
    pub live: bool,
    /// The requested model id was not present in the registry.
    pub synthesized: bool,
}

impl ResolvedModel {
    pub fn new(
        provider: ProviderConfig,
        requested_model: Option<&str>,
        reasoning_level: Option<&str>,
    ) -> Self {
        let requested_model = requested_model
            .map(str::trim)
            .filter(|model_id| !model_id.is_empty());

        let (model, synthesized) = match requested_model {
            Some(model_id) => match provider.model(model_id) {
                Some(model) => (model.clone(), false),
                None => {
                    let mut model = ModelConfig::new(model_id);
                    if let Some(template) = provider.default_model() {
                        model.context_window = template.context_window;
                        model.max_output_tokens = template.max_output_tokens;
                        model.capabilities = template.capabilities.clone();
                        model.reasoning_levels = template.reasoning_levels.clone();
                    }
                    (model, true)
                }
            },
            None => match provider.default_model() {
                Some(model) => (model.clone(), false),
                None => (ModelConfig::new(DEFAULT_MODEL_ID), true),
            },
        };

        let reasoning_level = model.resolved_reasoning_level(reasoning_level);
        let live = provider.has_credentials();
        Self {
            provider,
            model,
            reasoning_level,
            live,
            synthesized,
        }
    }

    pub fn selection(&self) -> ModelSelection {
        ModelSelection {
            provider_id: self.provider.id.clone(),
            model_id: self.model.id.clone(),
            reasoning_level: self.reasoning_level.clone(),
        }
    }

    pub fn provider_label(&self) -> &str {
        self.provider.display_name()
    }

    pub fn model_label(&self) -> &str {
        self.model.display_name()
    }

    pub fn context_window(&self) -> usize {
        self.model.context_window.max(1)
    }

    pub fn max_output_tokens(&self) -> u32 {
        self.model.max_output_tokens.max(1)
    }

    pub fn build_provider(&self) -> Result<Arc<dyn ModelProvider>, ProviderError> {
        let api_key = self.provider.effective_api_key();
        let base_url = if self.provider.base_url.trim().is_empty() {
            if self.provider.id == DEFAULT_PROVIDER_ID {
                "https://api.deepseek.com"
            } else {
                return Err(ProviderError::Other(format!(
                    "供应商「{}」还没有配置 Base URL",
                    self.provider.display_name()
                )));
            }
        } else {
            self.provider.base_url.trim()
        };
        let extra_headers = self
            .provider
            .headers
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();

        match self.provider.api_format {
            ApiFormat::OpenAiChatCompletions => {
                let config = OpenAiCompatConfig {
                    base_url: base_url.to_string(),
                    api_key,
                    context_window: self.context_window(),
                    extra_headers,
                    include_usage: true,
                    request_timeout: std::time::Duration::from_secs(300),
                    connect_timeout: std::time::Duration::from_secs(20),
                };
                Ok(Arc::new(OpenAiCompatProvider::new(config)?))
            }
            ApiFormat::AnthropicMessages => {
                let config = AnthropicConfig {
                    base_url: base_url.to_string(),
                    api_key,
                    context_window: self.context_window(),
                    extra_headers,
                    anthropic_version: "2023-06-01".to_string(),
                    request_timeout: std::time::Duration::from_secs(300),
                    connect_timeout: std::time::Duration::from_secs(20),
                };
                Ok(Arc::new(AnthropicMessagesProvider::new(config)?))
            }
            ApiFormat::OpenAiResponses => {
                let config = OpenAiResponsesConfig {
                    base_url: base_url.to_string(),
                    api_key,
                    context_window: self.context_window(),
                    extra_headers,
                    request_timeout: std::time::Duration::from_secs(300),
                    connect_timeout: std::time::Duration::from_secs(20),
                };
                Ok(Arc::new(OpenAiResponsesProvider::new(config)?))
            }
        }
    }
}

/// Fetch the model ids exposed by a provider. This is a best-effort helper for
/// the settings UI; providers that do not implement `/models` surface an error.
pub async fn list_models(provider: &ProviderConfig) -> Result<Vec<String>, String> {
    let api_key = provider.effective_api_key();
    let base = provider.base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(format!(
            "供应商「{}」还没有配置 Base URL",
            provider.display_name()
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| format!("无法创建 HTTP 客户端：{error}"))?;

    let (url, request) = match provider.api_format {
        ApiFormat::AnthropicMessages => {
            let url = if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            let mut request = client
                .get(url.clone())
                .header("anthropic-version", "2023-06-01");
            if !api_key.is_empty() {
                request = request.header("x-api-key", &api_key);
            }
            (url, request)
        }
        ApiFormat::OpenAiChatCompletions | ApiFormat::OpenAiResponses => {
            let base = base
                .trim_end_matches("/chat/completions")
                .trim_end_matches("/responses")
                .trim_end_matches('/');
            let url = format!("{base}/models");
            let mut request = client.get(url.clone());
            if !api_key.is_empty() {
                request = request.bearer_auth(&api_key);
            }
            (url, request)
        }
    };

    let mut request = request;
    for (key, value) in &provider.headers {
        request = request.header(key, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("请求 {url} 失败：{error}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = body.chars().take(600).collect::<String>();
        return Err(format!("{} {}", status.as_u16(), detail.trim()));
    }
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|error| format!("响应不是合法 JSON：{error}"))?;
    let mut ids = Vec::new();
    if let Some(data) = value.get("data").and_then(|data| data.as_array()) {
        for id in data
            .iter()
            .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
            .filter(|id| !id.trim().is_empty())
        {
            ids.push(id.to_string());
        }
    } else if let Some(models) = value.get("models").and_then(|models| models.as_array()) {
        for id in models
            .iter()
            .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
            .filter(|id| !id.trim().is_empty())
        {
            ids.push(id.to_string());
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

// --- builtin presets -------------------------------------------------------

fn model(
    id: &str,
    name: &str,
    context_window: usize,
    max_output_tokens: u32,
    capabilities: ModelCapabilities,
) -> ModelConfig {
    ModelConfig::new(id)
        .with_name(name)
        .with_context(context_window, max_output_tokens)
        .with_capabilities(capabilities)
}

fn reasoning_model(
    id: &str,
    name: &str,
    context_window: usize,
    max_output_tokens: u32,
    capabilities: ModelCapabilities,
) -> ModelConfig {
    model(id, name, context_window, max_output_tokens, capabilities)
        .with_reasoning_levels(standard_reasoning_levels())
}

fn provider(
    id: &str,
    name: &str,
    api_format: ApiFormat,
    base_url: &str,
    models: Vec<ModelConfig>,
) -> ProviderConfig {
    ProviderConfig {
        id: id.to_string(),
        name: name.to_string(),
        builtin: true,
        enabled: true,
        api_format,
        base_url: base_url.to_string(),
        api_key: String::new(),
        headers: BTreeMap::new(),
        models,
    }
}

pub fn builtin_providers() -> Vec<ProviderConfig> {
    vec![
        provider(
            "deepseek",
            "DeepSeek",
            ApiFormat::OpenAiChatCompletions,
            "https://api.deepseek.com",
            vec![
                model(
                    "deepseek-flash",
                    "DeepSeek Flash",
                    1_048_576,
                    8_192,
                    ModelCapabilities::reasoning(),
                ),
                model(
                    "deepseek-chat",
                    "DeepSeek Chat",
                    128_000,
                    8_192,
                    ModelCapabilities::tool(),
                ),
                model(
                    "deepseek-reasoner",
                    "DeepSeek Reasoner",
                    128_000,
                    8_192,
                    ModelCapabilities::reasoning(),
                ),
            ],
        ),
        provider(
            "openai",
            "OpenAI",
            ApiFormat::OpenAiChatCompletions,
            "https://api.openai.com/v1",
            vec![
                reasoning_model(
                    "gpt-5.1",
                    "GPT-5.1",
                    400_000,
                    32_768,
                    ModelCapabilities::full(),
                ),
                reasoning_model(
                    "gpt-5.1-mini",
                    "GPT-5.1 mini",
                    400_000,
                    16_384,
                    ModelCapabilities::full(),
                ),
                model(
                    "gpt-4.1",
                    "GPT-4.1",
                    1_047_576,
                    32_768,
                    ModelCapabilities::full(),
                ),
                model(
                    "gpt-4.1-mini",
                    "GPT-4.1 mini",
                    1_047_576,
                    32_768,
                    ModelCapabilities::full(),
                ),
                reasoning_model(
                    "o4-mini",
                    "o4-mini",
                    200_000,
                    16_384,
                    ModelCapabilities::reasoning(),
                ),
            ],
        ),
        provider(
            "anthropic",
            "Anthropic",
            ApiFormat::AnthropicMessages,
            "https://api.anthropic.com",
            vec![
                model(
                    "claude-opus-4-5",
                    "Claude Opus 4.5",
                    200_000,
                    32_768,
                    ModelCapabilities::full(),
                ),
                model(
                    "claude-sonnet-4-5",
                    "Claude Sonnet 4.5",
                    200_000,
                    32_768,
                    ModelCapabilities::full(),
                ),
                model(
                    "claude-haiku-4-5",
                    "Claude Haiku 4.5",
                    200_000,
                    32_768,
                    ModelCapabilities::vision(),
                ),
            ],
        ),
        provider(
            "zai",
            "Z.ai",
            ApiFormat::OpenAiChatCompletions,
            "https://api.z.ai/api/paas/v4",
            vec![
                model(
                    "GLM-5.3",
                    "GLM-5.3",
                    204_800,
                    16_384,
                    ModelCapabilities::full(),
                ),
                model(
                    "GLM-5.3-Flash",
                    "GLM-5.3 Flash",
                    204_800,
                    16_384,
                    ModelCapabilities::tool(),
                ),
                model(
                    "GLM-5.1",
                    "GLM-5.1",
                    204_800,
                    16_384,
                    ModelCapabilities::full(),
                ),
                model(
                    "GLM-4.7",
                    "GLM-4.7",
                    204_800,
                    16_384,
                    ModelCapabilities::tool(),
                ),
                model(
                    "GLM-4.6V",
                    "GLM-4.6V",
                    204_800,
                    16_384,
                    ModelCapabilities::vision(),
                ),
            ],
        ),
        provider(
            "bigmodel",
            "BigModel",
            ApiFormat::OpenAiChatCompletions,
            "https://open.bigmodel.cn/api/paas/v4",
            vec![
                model(
                    "GLM-5.3",
                    "GLM-5.3",
                    204_800,
                    16_384,
                    ModelCapabilities::full(),
                ),
                model(
                    "GLM-5.3-Flash",
                    "GLM-5.3 Flash",
                    204_800,
                    16_384,
                    ModelCapabilities::tool(),
                ),
                model(
                    "GLM-5.1",
                    "GLM-5.1",
                    204_800,
                    16_384,
                    ModelCapabilities::full(),
                ),
                model(
                    "GLM-4.7",
                    "GLM-4.7",
                    204_800,
                    16_384,
                    ModelCapabilities::tool(),
                ),
                model(
                    "GLM-4.6V",
                    "GLM-4.6V",
                    204_800,
                    16_384,
                    ModelCapabilities::vision(),
                ),
            ],
        ),
        provider(
            "moonshot",
            "Kimi",
            ApiFormat::OpenAiChatCompletions,
            "https://api.moonshot.cn/v1",
            vec![
                model(
                    "kimi-k3.1-pro",
                    "Kimi K3.1 Pro",
                    262_144,
                    16_384,
                    ModelCapabilities::full(),
                ),
                model(
                    "kimi-k3",
                    "Kimi K3",
                    262_144,
                    16_384,
                    ModelCapabilities::full(),
                ),
                model(
                    "kimi-k2.7-code",
                    "Kimi K2.7 Code",
                    262_144,
                    16_384,
                    ModelCapabilities::tool(),
                ),
                model(
                    "kimi-k2.6",
                    "Kimi K2.6",
                    262_144,
                    16_384,
                    ModelCapabilities::vision(),
                ),
            ],
        ),
        provider(
            "openrouter",
            "OpenRouter",
            ApiFormat::OpenAiChatCompletions,
            "https://openrouter.ai/api/v1",
            vec![
                model(
                    "anthropic/claude-sonnet-4.5",
                    "Claude Sonnet 4.5",
                    200_000,
                    32_768,
                    ModelCapabilities::full(),
                ),
                reasoning_model(
                    "openai/gpt-5.1",
                    "GPT-5.1",
                    400_000,
                    32_768,
                    ModelCapabilities::full(),
                ),
                model(
                    "google/gemini-3-pro-preview",
                    "Gemini 3 Pro",
                    1_048_576,
                    32_768,
                    ModelCapabilities::full(),
                ),
                model(
                    "deepseek/deepseek-chat",
                    "DeepSeek Chat",
                    128_000,
                    8_192,
                    ModelCapabilities::tool(),
                ),
                model(
                    "qwen/qwen3-max",
                    "Qwen3 Max",
                    262_144,
                    16_384,
                    ModelCapabilities::tool(),
                ),
            ],
        ),
        provider(
            "siliconflow",
            "SiliconFlow",
            ApiFormat::OpenAiChatCompletions,
            "https://api.siliconflow.cn/v1",
            vec![
                model(
                    "deepseek-ai/DeepSeek-V3.2",
                    "DeepSeek V3.2",
                    128_000,
                    8_192,
                    ModelCapabilities::tool(),
                ),
                model(
                    "Qwen/Qwen3-235B-A22B",
                    "Qwen3 235B",
                    128_000,
                    16_384,
                    ModelCapabilities::tool(),
                ),
                model(
                    "moonshotai/Kimi-K2",
                    "Kimi K2",
                    128_000,
                    16_384,
                    ModelCapabilities::tool(),
                ),
            ],
        ),
        provider(
            "ollama",
            "Ollama（本地）",
            ApiFormat::OpenAiChatCompletions,
            "http://localhost:11434/v1",
            vec![
                model(
                    "llama3.3",
                    "Llama 3.3",
                    128_000,
                    8_192,
                    ModelCapabilities::tool(),
                ),
                model(
                    "qwen3:32b",
                    "Qwen3 32B",
                    32_768,
                    8_192,
                    ModelCapabilities::tool(),
                ),
                model(
                    "deepseek-r1:32b",
                    "DeepSeek R1 32B",
                    128_000,
                    8_192,
                    ModelCapabilities::reasoning(),
                ),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_is_valid_and_complete() {
        let file = ModelConfigFile::with_builtins();
        assert!(file.providers.len() >= 9);
        for provider in &file.providers {
            assert!(provider.builtin);
            assert!(!provider.base_url.trim().is_empty());
            assert!(!provider.models.is_empty(), "{} has no models", provider.id);
            let mut ids = std::collections::HashSet::new();
            for model in &provider.models {
                assert!(
                    ids.insert(model.id.clone()),
                    "duplicate model in {}",
                    provider.id
                );
                assert!(model.context_window > 0);
                assert!(model.max_output_tokens > 0);
            }
        }
        let selection = file.default_selection.expect("default selection");
        assert_eq!(selection.provider_id, DEFAULT_PROVIDER_ID);
        assert_eq!(selection.model_id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn resolve_falls_back_for_unknown_provider_and_model() {
        let file = ModelConfigFile::with_builtins();
        let resolved = file.resolve(Some("missing"), Some("some-old-model"), None);
        assert_eq!(resolved.provider.id, DEFAULT_PROVIDER_ID);
        assert_eq!(resolved.model.id, "some-old-model");
        assert!(resolved.synthesized);
        assert_eq!(resolved.context_window(), 1_048_576);
    }

    #[test]
    fn resolve_respects_reasoning_levels() {
        let file = ModelConfigFile::with_builtins();
        let resolved = file.resolve(Some("openai"), Some("gpt-5.1"), Some("high"));
        assert_eq!(resolved.reasoning_level.as_deref(), Some("high"));

        let fallback = file.resolve(Some("openai"), Some("gpt-5.1"), Some("bogus"));
        assert_eq!(fallback.reasoning_level.as_deref(), Some("low"));

        let none = file.resolve(Some("deepseek"), Some("deepseek-chat"), Some("high"));
        assert_eq!(none.reasoning_level, None);
    }

    #[test]
    fn legacy_migration_matches_known_endpoint() {
        let mut file = ModelConfigFile::with_builtins();
        file.apply_legacy("https://api.deepseek.com/", "sk-test", "deepseek-reasoner");
        let provider = file.provider("deepseek").expect("deepseek");
        assert_eq!(provider.api_key, "sk-test");
        let selection = file.default_selection.expect("selection");
        assert_eq!(selection.provider_id, "deepseek");
        assert_eq!(selection.model_id, "deepseek-reasoner");
    }

    #[test]
    fn legacy_migration_preserves_custom_endpoint() {
        let mut file = ModelConfigFile::with_builtins();
        file.apply_legacy("https://example.test/v1", "sk-custom", "my-model");
        let provider = file.provider("migrated").expect("migrated provider");
        assert_eq!(provider.base_url, "https://example.test/v1");
        assert_eq!(provider.api_key, "sk-custom");
        let selection = file.default_selection.expect("selection");
        assert_eq!(selection.provider_id, "migrated");
        assert_eq!(selection.model_id, "my-model");
    }

    #[test]
    fn merge_missing_builtins_keeps_user_edits() {
        let mut file = ModelConfigFile::with_builtins();
        file.providers.retain(|provider| provider.id != "openai");
        file.provider_mut("deepseek").unwrap().api_key = "sk-keep".to_string();
        file.merge_missing_builtins();
        assert!(file.provider("openai").is_some());
        assert_eq!(file.provider("deepseek").unwrap().api_key, "sk-keep");
    }

    #[test]
    fn old_model_config_file_without_new_fields_deserializes() {
        let json = r#"{
            "schema_version": 1,
            "providers": [{
                "id": "custom",
                "name": "Custom",
                "api_format": "openai-chat-completions",
                "base_url": "https://example.test/v1",
                "models": [{"id": "m"}]
            }]
        }"#;
        let mut file: ModelConfigFile = serde_json::from_str(json).expect("deserialize");
        assert!(file.provider("custom").unwrap().enabled);
        assert!(file.provider("custom").unwrap().models[0].enabled);
        file.normalize();
        assert!(file.provider("deepseek").is_some());
    }

    #[test]
    fn local_providers_do_not_require_keys() {
        let file = ModelConfigFile::with_builtins();
        let ollama = file.provider("ollama").unwrap();
        assert!(!ollama.requires_api_key());
        assert!(ollama.has_credentials());
        assert!(file.provider("openai").unwrap().requires_api_key());
    }
}
