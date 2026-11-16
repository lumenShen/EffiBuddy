//! Settings shell: general preferences and the ZCode-style model registry UI.

use std::rc::Rc;

use effibuddy_agent_core::tool::PermissionMode;
use effibuddy_agent_core::{
    ApiFormat, ChatMessage, ChatRequest, DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT_TOKENS,
    ModelCapabilities, ModelConfig, ProfileSource, ProviderConfig, ResolvedModel, SubagentProfile,
    list_models, merged_subagent_profiles,
};
use effibuddy_browser::BrowserUseMode;
use effibuddy_computer::ComputerUseMode;
use effibuddy_ui::{
    BadgeTone, Button, ButtonSize, ButtonVariant, IconButton, Spinner, TextInputOptions,
    TextInputState, TextInputView, Theme, badge, ease_out_cubic, icons, overlay_motion_duration,
    section_label, switch,
};
use gpui::{
    Animation, AnimationExt, AnyElement, Context, Entity, IntoElement, MouseButton, SharedString,
    Window, div, prelude::*, px, svg,
};
use std::path::PathBuf;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

use crate::app::AppView;
use crate::config::AppConfig;
use crate::model_config::{ModelConfigStore, default_model_metadata};
use crate::smooth::smooth_overlay;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Models,
    Context,
    Agents,
    Archived,
    Appearance,
    Memory,
    About,
}

impl SettingsSection {
    fn label(self) -> &'static str {
        match self {
            SettingsSection::General => "通用",
            SettingsSection::Models => "模型",
            SettingsSection::Context => "上下文",
            SettingsSection::Agents => "子代理",
            SettingsSection::Archived => "归档",
            SettingsSection::Appearance => "外观",
            SettingsSection::Memory => "内存",
            SettingsSection::About => "关于",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SettingsSection::General => icons::WRENCH,
            SettingsSection::Models => icons::SPARKLES,
            SettingsSection::Context => icons::GAUGE,
            SettingsSection::Agents => icons::CODE,
            SettingsSection::Archived => icons::ARCHIVE,
            SettingsSection::Appearance => icons::SUN,
            SettingsSection::Memory => icons::MEMORY,
            SettingsSection::About => icons::CHAT,
        }
    }
}

pub(crate) struct ModelEditorState {
    pub open: bool,
    pub original_id: Option<String>,
    pub error: Option<String>,
    pub enabled: bool,
    pub tool_call: bool,
    pub vision: bool,
    pub reasoning: bool,
    pub id: Entity<TextInputState>,
    pub id_view: Entity<TextInputView>,
    pub name: Entity<TextInputState>,
    pub name_view: Entity<TextInputView>,
    pub context: Entity<TextInputState>,
    pub context_view: Entity<TextInputView>,
    pub max_output: Entity<TextInputState>,
    pub max_output_view: Entity<TextInputView>,
    pub levels: Entity<TextInputState>,
    pub levels_view: Entity<TextInputView>,
}

pub(crate) struct SettingsState {
    pub section: SettingsSection,
    pub selected_provider: Option<String>,
    pub provider_name: Entity<TextInputState>,
    pub provider_name_view: Entity<TextInputView>,
    pub provider_base_url: Entity<TextInputState>,
    pub provider_base_url_view: Entity<TextInputView>,
    pub provider_api_key: Entity<TextInputState>,
    pub provider_api_key_view: Entity<TextInputView>,
    pub provider_enabled: bool,
    pub api_format_draft: Option<ApiFormat>,
    pub provider_status: Option<(bool, String)>,
    pub provider_testing: bool,
    pub provider_fetching: bool,
    pub provider_delete_confirm: bool,
    pub model_delete_confirm: Option<String>,
    pub model_editor: ModelEditorState,
    pub workspace: Entity<TextInputState>,
    pub workspace_view: Entity<TextInputView>,
    pub default_mode: PermissionMode,
    pub computer_use_mode: ComputerUseMode,
    pub browser_use_mode: BrowserUseMode,
    pub demo: bool,
    pub status: Option<(bool, String)>,
    /// Settings → 上下文 drafts. All compaction thresholds are percentages of
    /// the selected model's context window.
    pub compact_enabled: bool,
    pub compact_trigger: Entity<TextInputState>,
    pub compact_trigger_view: Entity<TextInputView>,
    pub compact_keep: Entity<TextInputState>,
    pub compact_keep_view: Entity<TextInputView>,
    pub compact_summary: Entity<TextInputState>,
    pub compact_summary_view: Entity<TextInputView>,
    pub compact_clear_tools: bool,
    pub compact_keep_results: Entity<TextInputState>,
    pub compact_keep_results_view: Entity<TextInputView>,
    pub compact_background: bool,
    pub compact_background_start: Entity<TextInputState>,
    pub compact_background_start_view: Entity<TextInputView>,
    /// Armed session id for the two-step delete on the 归档 page.
    pub archived_delete_confirm: Option<String>,
}

impl SettingsState {
    pub fn new(cx: &mut Context<AppView>, config: &AppConfig, models: &ModelConfigStore) -> Self {
        let selected_provider = models
            .default_selection()
            .map(|selection| selection.provider_id)
            .filter(|id| models.file().provider(id).is_some())
            .or_else(|| {
                models
                    .file()
                    .providers
                    .first()
                    .map(|provider| provider.id.clone())
            });

        let (provider_name, provider_name_view) = input(cx, "", "供应商名称");
        let (provider_base_url, provider_base_url_view) =
            input(cx, "", "https://api.example.com/v1");
        let (provider_api_key, provider_api_key_view) = input(cx, "", "留空表示不修改已保存的 Key");
        let (workspace, workspace_view) =
            input(cx, &config.workspace().display().to_string(), "工作区路径");
        let (compact_trigger, compact_trigger_view) = input(
            cx,
            &format!("{:.0}", config.compaction.trigger_percent),
            "80",
        );
        let (compact_keep, compact_keep_view) = input(
            cx,
            &format!("{:.0}", config.compaction.keep_recent_percent),
            "20",
        );
        let (compact_summary, compact_summary_view) = input(
            cx,
            &format!("{:.0}", config.compaction.summary_percent),
            "10",
        );
        let (compact_keep_results, compact_keep_results_view) = input(
            cx,
            &config.compaction.keep_recent_tool_results.to_string(),
            "5",
        );
        let (compact_background_start, compact_background_start_view) = input(
            cx,
            &format!("{:.0}", config.compaction.background_start_percent),
            "50",
        );
        let (model_id, model_id_view) = input(cx, "", "例如 claude-sonnet-4-5");
        let (model_name, model_name_view) = input(cx, "", "显示名称（可选）");
        let (model_context, model_context_view) = input(cx, "", "例如 200000");
        let (model_max, model_max_view) =
            input(cx, "", &format!("例如 {DEFAULT_MAX_OUTPUT_TOKENS}"));
        let (model_levels, model_levels_view) = input(cx, "", "low, medium, high（可选）");

        let mut state = Self {
            section: SettingsSection::General,
            selected_provider,
            provider_name,
            provider_name_view,
            provider_base_url,
            provider_base_url_view,
            provider_api_key,
            provider_api_key_view,
            provider_enabled: true,
            api_format_draft: None,
            provider_status: None,
            provider_testing: false,
            provider_fetching: false,
            provider_delete_confirm: false,
            model_delete_confirm: None,
            model_editor: ModelEditorState {
                open: false,
                original_id: None,
                error: None,
                enabled: true,
                tool_call: true,
                vision: false,
                reasoning: false,
                id: model_id,
                id_view: model_id_view,
                name: model_name,
                name_view: model_name_view,
                context: model_context,
                context_view: model_context_view,
                max_output: model_max,
                max_output_view: model_max_view,
                levels: model_levels,
                levels_view: model_levels_view,
            },
            workspace,
            workspace_view,
            default_mode: config.effective_default_mode(),
            computer_use_mode: config.computer_use_mode,
            browser_use_mode: config.browser_use_mode,
            demo: config.use_demo,
            status: None,
            compact_enabled: config.compaction.enabled,
            compact_trigger,
            compact_trigger_view,
            compact_keep,
            compact_keep_view,
            compact_summary,
            compact_summary_view,
            compact_clear_tools: config.compaction.clear_tool_outputs,
            compact_keep_results,
            compact_keep_results_view,
            compact_background: config.compaction.background_precompact,
            compact_background_start,
            compact_background_start_view,
            archived_delete_confirm: None,
        };
        state.load_provider_form(models, cx);
        state
    }

    pub fn load_provider_form(&mut self, models: &ModelConfigStore, cx: &mut Context<AppView>) {
        let Some(provider) = self
            .selected_provider
            .as_deref()
            .and_then(|id| models.file().provider(id))
        else {
            return;
        };
        let name = provider.name.clone();
        let base_url = provider.base_url.clone();
        self.provider_name
            .update(cx, |state, _| state.set_content_silent(name));
        self.provider_base_url
            .update(cx, |state, _| state.set_content_silent(base_url));
        self.provider_api_key
            .update(cx, |state, _| state.set_content_silent(""));
        self.provider_enabled = provider.enabled;
        self.api_format_draft = None;
        self.provider_status = None;
        self.provider_delete_confirm = false;
        self.model_delete_confirm = None;
    }
}

fn input(
    cx: &mut Context<AppView>,
    value: &str,
    placeholder: &str,
) -> (Entity<TextInputState>, Entity<TextInputView>) {
    let value = value.to_string();
    let placeholder = placeholder.to_string();
    let state = cx.new(move |cx| {
        TextInputState::new(
            cx,
            TextInputOptions::default()
                .multiline(false)
                .lines(1, 1)
                .placeholder(placeholder),
        )
        .with_content(value)
    });
    let view = cx.new(|cx| TextInputView::new(state.clone(), cx));
    (state, view)
}

impl AppView {
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_section(SettingsSection::General, window, cx);
    }

    pub(crate) fn open_settings_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_section(SettingsSection::Models, window, cx);
    }

    pub(crate) fn open_settings_section(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_open = true;
        self.model_menu.open = false;
        self.settings.section = section;
        self.settings.default_mode = self.config.effective_default_mode();
        self.settings.computer_use_mode = self.config.computer_use_mode;
        self.settings.browser_use_mode = self.config.browser_use_mode;
        self.settings.demo = self.config.use_demo;
        self.settings.compact_enabled = self.config.compaction.enabled;
        self.settings.compact_clear_tools = self.config.compaction.clear_tool_outputs;
        self.settings.compact_background = self.config.compaction.background_precompact;
        let trigger = format!("{:.0}", self.config.compaction.trigger_percent);
        let keep = format!("{:.0}", self.config.compaction.keep_recent_percent);
        let summary = format!("{:.0}", self.config.compaction.summary_percent);
        let keep_results = self.config.compaction.keep_recent_tool_results.to_string();
        let background_start = format!("{:.0}", self.config.compaction.background_start_percent);
        self.settings
            .compact_trigger
            .update(cx, move |state, _| state.set_content_silent(trigger));
        self.settings
            .compact_keep
            .update(cx, move |state, _| state.set_content_silent(keep));
        self.settings
            .compact_summary
            .update(cx, move |state, _| state.set_content_silent(summary));
        self.settings
            .compact_keep_results
            .update(cx, move |state, _| state.set_content_silent(keep_results));
        self.settings
            .compact_background_start
            .update(cx, move |state, _| {
                state.set_content_silent(background_start)
            });
        self.settings.status = None;
        self.settings.archived_delete_confirm = None;
        self.settings.workspace.update(cx, |state, cx| {
            state.set_content(self.config.workspace().display().to_string(), window, cx)
        });
        if self.settings.selected_provider.is_none() {
            self.settings.selected_provider = self
                .models
                .file()
                .providers
                .first()
                .map(|provider| provider.id.clone());
        }
        self.settings.load_provider_form(&self.models, cx);
        cx.notify();
    }

    // --- general -----------------------------------------------------------

    fn save_general_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self
            .settings
            .workspace
            .read(cx)
            .content()
            .trim()
            .to_string();
        self.config.workspace = if workspace.is_empty() {
            None
        } else {
            Some(PathBuf::from(workspace))
        };
        self.config.default_permission_mode = Some(self.settings.default_mode);
        // Keep the legacy flag in sync for hand-edited configs; approval itself
        // now goes through ModeApprovalGate.
        self.config.auto_approve_tools = self.settings.default_mode == PermissionMode::FullAccess;
        self.config.computer_use_mode = self.settings.computer_use_mode;
        self.config.browser_use_mode = self.settings.browser_use_mode;
        self.config.use_demo = self.settings.demo;
        self.computer.set_mode(self.settings.computer_use_mode);
        self.browser.set_mode(self.settings.browser_use_mode);
        match self.config.save() {
            Ok(()) => {
                self.settings.status = Some((true, "已保存。新任务会使用这些设置。".to_string()));
                if let Some(active) = &self.active {
                    let mut session = effibuddy_agent_core::lock_session(&active.shared);
                    // Only a plain task follows the global workspace. A project
                    // session keeps its project folder even when it is empty.
                    if session.messages.is_empty()
                        && session.project_id.is_none()
                        && session.project.is_none()
                    {
                        session.cwd = self.config.workspace();
                    }
                }
            }
            Err(error) => self.settings.status = Some((false, error)),
        }
        window.focus(&self.composer_state.read(cx).focus_handle());
        cx.notify();
    }

    /// Persist the context-compaction drafts. Percentages are clamped to the
    /// ranges the policy understands; invalid text falls back to the current
    /// value instead of silently becoming zero.
    fn save_compaction_settings(&mut self, cx: &mut Context<Self>) {
        fn percent(content: &str, fallback: f32) -> f32 {
            content
                .trim()
                .trim_end_matches('%')
                .parse::<f32>()
                .ok()
                .filter(|value| value.is_finite())
                .unwrap_or(fallback)
        }
        fn count(content: &str, fallback: usize) -> usize {
            content.trim().parse::<usize>().unwrap_or(fallback)
        }

        let current = self.config.compaction.clone();
        let trigger = percent(
            self.settings.compact_trigger.read(cx).content(),
            current.trigger_percent,
        )
        .clamp(10.0, 95.0);
        let keep = percent(
            self.settings.compact_keep.read(cx).content(),
            current.keep_recent_percent,
        )
        .clamp(5.0, 60.0);
        let summary = percent(
            self.settings.compact_summary.read(cx).content(),
            current.summary_percent,
        )
        .clamp(2.0, 30.0);
        let keep_results = count(
            self.settings.compact_keep_results.read(cx).content(),
            current.keep_recent_tool_results,
        )
        .clamp(1, 50);
        let background_start = percent(
            self.settings.compact_background_start.read(cx).content(),
            current.background_start_percent,
        )
        .clamp(5.0, 90.0)
        .min(trigger - 5.0)
        .max((trigger - 30.0).max(5.0));

        self.config.compaction = effibuddy_agent_core::CompactionConfig {
            enabled: self.settings.compact_enabled,
            trigger_percent: trigger,
            keep_recent_percent: keep,
            summary_percent: summary,
            clear_tool_outputs: self.settings.compact_clear_tools,
            keep_recent_tool_results: keep_results,
            max_consecutive_failures: current.max_consecutive_failures,
            background_precompact: self.settings.compact_background,
            background_start_percent: background_start,
        };

        // Reflect the clamped values so the user sees what was actually saved.
        let trigger_text = format!("{trigger:.0}");
        let keep_text = format!("{keep:.0}");
        let summary_text = format!("{summary:.0}");
        let keep_results_text = keep_results.to_string();
        let background_start_text = format!("{background_start:.0}");
        self.settings
            .compact_trigger
            .update(cx, move |state, _| state.set_content_silent(trigger_text));
        self.settings
            .compact_keep
            .update(cx, move |state, _| state.set_content_silent(keep_text));
        self.settings
            .compact_summary
            .update(cx, move |state, _| state.set_content_silent(summary_text));
        self.settings
            .compact_keep_results
            .update(cx, move |state, _| {
                state.set_content_silent(keep_results_text)
            });
        self.settings
            .compact_background_start
            .update(cx, move |state, _| {
                state.set_content_silent(background_start_text)
            });

        match self.config.save() {
            Ok(()) => {
                self.settings.status = Some((
                    true,
                    "已保存。正在运行的任务结束后，新任务生效。".to_string(),
                ))
            }
            Err(error) => self.settings.status = Some((false, error)),
        }
        cx.notify();
    }

    // --- provider list -----------------------------------------------------

    fn select_provider(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        self.settings.selected_provider = Some(provider_id.to_string());
        self.settings.load_provider_form(&self.models, cx);
        self.settings.model_editor.open = false;
        cx.notify();
    }

    fn add_custom_provider(&mut self, cx: &mut Context<Self>) {
        match self.models.add_custom_provider() {
            Ok(id) => {
                self.settings.selected_provider = Some(id);
                self.settings.section = SettingsSection::Models;
                self.settings.load_provider_form(&self.models, cx);
                self.settings.provider_status = Some((
                    true,
                    "已创建自定义供应商，请填写 Base URL 与 API Key。".to_string(),
                ));
            }
            Err(error) => self.settings.provider_status = Some((false, error)),
        }
        cx.notify();
    }

    fn delete_selected_provider(&mut self, cx: &mut Context<Self>) {
        let Some(provider_id) = self.settings.selected_provider.clone() else {
            return;
        };
        if !self.settings.provider_delete_confirm {
            self.settings.provider_delete_confirm = true;
            cx.notify();
            return;
        }
        match self.models.remove_provider(&provider_id) {
            Ok(()) => {
                self.settings.selected_provider = self
                    .models
                    .file()
                    .providers
                    .first()
                    .map(|provider| provider.id.clone());
                self.settings.provider_delete_confirm = false;
                self.settings.load_provider_form(&self.models, cx);
                self.settings.provider_status = Some((true, "供应商已删除。".to_string()));
            }
            Err(error) => {
                self.settings.provider_delete_confirm = false;
                self.settings.provider_status = Some((false, error));
            }
        }
        cx.notify();
    }

    // --- provider connection ----------------------------------------------

    fn save_provider(&mut self, cx: &mut Context<Self>) {
        let Some(provider_id) = self.settings.selected_provider.clone() else {
            return;
        };
        let name = self
            .settings
            .provider_name
            .read(cx)
            .content()
            .trim()
            .to_string();
        let base_url = self
            .settings
            .provider_base_url
            .read(cx)
            .content()
            .trim()
            .to_string();
        let api_key = self
            .settings
            .provider_api_key
            .read(cx)
            .content()
            .trim()
            .to_string();
        let Some(provider) = self.models.file().provider(&provider_id).cloned() else {
            self.settings.provider_status = Some((false, "供应商不存在。".to_string()));
            cx.notify();
            return;
        };
        let api_format = self.settings.selected_api_format(&provider);
        let result = self.models.update_provider_connection(
            &provider_id,
            if name.is_empty() {
                provider.name.clone()
            } else {
                name
            },
            api_format,
            base_url,
            if api_key.is_empty() {
                None
            } else {
                Some(api_key)
            },
            self.settings.provider_enabled,
        );
        self.settings.provider_status = Some(match result {
            Ok(()) => (true, "已保存供应商配置。".to_string()),
            Err(error) => (false, error),
        });
        self.settings
            .provider_api_key
            .update(cx, |state, _| state.set_content_silent(""));
        self.settings.load_provider_form(&self.models, cx);
        cx.notify();
    }

    fn clear_provider_key(&mut self, cx: &mut Context<Self>) {
        let Some(provider_id) = self.settings.selected_provider.clone() else {
            return;
        };
        self.settings.provider_status =
            Some(match self.models.set_secret(&provider_id, String::new()) {
                Ok(()) => (
                    true,
                    "已清除保存的 API Key。环境变量仍可能提供凭据。".to_string(),
                ),
                Err(error) => (false, error),
            });
        cx.notify();
    }

    fn test_selected_provider(&mut self, cx: &mut Context<Self>) {
        let Some(provider_id) = self.settings.selected_provider.clone() else {
            return;
        };
        let Some(mut provider) = self.models.file().provider(&provider_id).cloned() else {
            return;
        };
        provider.name = self
            .settings
            .provider_name
            .read(cx)
            .content()
            .trim()
            .to_string();
        provider.base_url = self
            .settings
            .provider_base_url
            .read(cx)
            .content()
            .trim()
            .to_string();
        provider.api_format = self.settings.selected_api_format(&provider);
        let key = self
            .settings
            .provider_api_key
            .read(cx)
            .content()
            .trim()
            .to_string();
        if !key.is_empty() {
            provider.api_key = key;
        }
        let Some(model) = provider.default_model().cloned() else {
            self.settings.provider_status =
                Some((false, "该供应商还没有模型，请先添加模型。".to_string()));
            cx.notify();
            return;
        };
        let Some(runtime) = &self.runtime else {
            self.settings.provider_status = Some((false, "后台运行时不可用。".to_string()));
            cx.notify();
            return;
        };
        self.settings.provider_testing = true;
        self.settings.provider_status = Some((true, format!("正在用 {} 探测连接…", model.id)));
        let model_id = model.id.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let result = probe_provider(provider, model_id).await;
            let _ = sender.send(result);
        });
        cx.spawn(async move |weak, cx| {
            if let Ok(result) = receiver.await {
                let _ = weak.update(cx, |this, cx| {
                    this.settings.provider_testing = false;
                    this.settings.provider_status = Some(match result {
                        Ok(message) => (true, message),
                        Err(error) => (false, error),
                    });
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn fetch_provider_models(&mut self, cx: &mut Context<Self>) {
        let Some(provider_id) = self.settings.selected_provider.clone() else {
            return;
        };
        let Some(mut provider) = self.models.file().provider(&provider_id).cloned() else {
            return;
        };
        provider.base_url = self
            .settings
            .provider_base_url
            .read(cx)
            .content()
            .trim()
            .to_string();
        provider.api_format = self.settings.selected_api_format(&provider);
        let key = self
            .settings
            .provider_api_key
            .read(cx)
            .content()
            .trim()
            .to_string();
        if !key.is_empty() {
            provider.api_key = key;
        }
        let Some(runtime) = &self.runtime else {
            self.settings.provider_status = Some((false, "后台运行时不可用。".to_string()));
            cx.notify();
            return;
        };
        self.settings.provider_fetching = true;
        self.settings.provider_status = Some((true, "正在从供应商获取模型列表…".to_string()));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let result = list_models(&provider).await;
            let _ = sender.send(result);
        });
        let provider_id = provider_id.clone();
        cx.spawn(async move |weak, cx| {
            if let Ok(result) = receiver.await {
                let _ = weak.update(cx, |this, cx| {
                    this.settings.provider_fetching = false;
                    this.settings.provider_status = Some(match result {
                        Ok(ids) if ids.is_empty() => {
                            (false, "供应商返回了空模型列表。".to_string())
                        }
                        Ok(ids) => {
                            let count = ids.len();
                            match this.models.merge_fetched_models(&provider_id, &ids, None) {
                                Ok(added) => {
                                    (true, format!("获取到 {count} 个模型，新增 {added} 个。"))
                                }
                                Err(error) => (false, error),
                            }
                        }
                        Err(error) => (false, format!("获取模型失败：{error}")),
                    });
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    // --- models -----------------------------------------------------------

    fn set_default_model(&mut self, provider_id: &str, model_id: &str, cx: &mut Context<Self>) {
        let reasoning = self
            .models
            .file()
            .provider(provider_id)
            .and_then(|provider| provider.model(model_id))
            .and_then(|model| model.default_reasoning_level.clone());
        self.settings.provider_status = Some(
            match self.models.set_default(provider_id, model_id, reasoning) {
                Ok(()) => (true, "已设为默认模型。新会话会使用它。".to_string()),
                Err(error) => (false, error),
            },
        );
        cx.notify();
    }

    fn toggle_model_enabled(&mut self, provider_id: &str, model_id: &str, cx: &mut Context<Self>) {
        let enabled = self
            .models
            .file()
            .provider(provider_id)
            .and_then(|provider| provider.model(model_id))
            .map(|model| !model.enabled)
            .unwrap_or(false);
        if let Err(error) = self
            .models
            .set_model_enabled(provider_id, model_id, enabled)
        {
            self.settings.provider_status = Some((false, error));
        }
        cx.notify();
    }

    fn delete_model(&mut self, provider_id: &str, model_id: &str, cx: &mut Context<Self>) {
        if self.settings.model_delete_confirm.as_deref() != Some(model_id) {
            self.settings.model_delete_confirm = Some(model_id.to_string());
            cx.notify();
            return;
        }
        self.settings.model_delete_confirm = None;
        self.settings.provider_status =
            Some(match self.models.remove_model(provider_id, model_id) {
                Ok(()) => (true, "模型已删除。".to_string()),
                Err(error) => (false, error),
            });
        self.settings.load_provider_form(&self.models, cx);
        cx.notify();
    }

    fn open_model_editor(
        &mut self,
        model: Option<ModelConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = &mut self.settings.model_editor;
        editor.open = true;
        editor.error = None;
        editor.original_id = model.as_ref().map(|model| model.id.clone());
        let (id, name, context, max_output, levels, enabled, tool_call, vision, reasoning) =
            if let Some(model) = model {
                (
                    model.id.clone(),
                    model.name.clone(),
                    model.context_window.to_string(),
                    model.effective_max_output_tokens().to_string(),
                    model
                        .reasoning_levels
                        .iter()
                        .map(|level| level.value.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                    model.enabled,
                    model.capabilities.tool_call,
                    model.capabilities.vision,
                    model.capabilities.reasoning,
                )
            } else {
                (
                    String::new(),
                    String::new(),
                    DEFAULT_CONTEXT_WINDOW.to_string(),
                    DEFAULT_MAX_OUTPUT_TOKENS.to_string(),
                    String::new(),
                    true,
                    true,
                    false,
                    false,
                )
            };
        editor
            .id
            .update(cx, |state, cx| state.set_content(id, window, cx));
        editor
            .name
            .update(cx, |state, cx| state.set_content(name, window, cx));
        editor
            .context
            .update(cx, |state, cx| state.set_content(context, window, cx));
        editor
            .max_output
            .update(cx, |state, cx| state.set_content(max_output, window, cx));
        editor
            .levels
            .update(cx, |state, cx| state.set_content(levels, window, cx));
        editor.enabled = enabled;
        editor.tool_call = tool_call;
        editor.vision = vision;
        editor.reasoning = reasoning;
        cx.notify();
    }

    pub(crate) fn close_model_editor(&mut self, cx: &mut Context<Self>) {
        self.settings.model_editor.open = false;
        self.settings.model_editor.error = None;
        cx.notify();
    }

    fn save_model_editor(&mut self, cx: &mut Context<Self>) {
        let Some(provider_id) = self.settings.selected_provider.clone() else {
            return;
        };
        let editor = &self.settings.model_editor;
        let model_id = editor.id.read(cx).content().trim().to_string();
        let name = editor.name.read(cx).content().trim().to_string();
        let context = editor.context.read(cx).content().trim().parse::<usize>();
        let max_output = editor.max_output.read(cx).content().trim().parse::<u32>();
        if model_id.is_empty() {
            self.settings.model_editor.error = Some("模型 ID 不能为空。".to_string());
            cx.notify();
            return;
        }
        if !context.as_ref().is_ok_and(|value| *value > 0) {
            self.settings.model_editor.error = Some("上下文窗口必须是正整数。".to_string());
            cx.notify();
            return;
        }
        if !max_output.as_ref().is_ok_and(|value| *value > 0) {
            self.settings.model_editor.error = Some("最大输出必须是正整数。".to_string());
            cx.notify();
            return;
        }
        // The output budget is drawn from the same window as the input, so a
        // value above the context window can never be satisfied.
        if let (Ok(context_window), Ok(max_output_tokens)) = (&context, &max_output)
            && *max_output_tokens as usize > *context_window
        {
            self.settings.model_editor.error = Some("最大输出不能超过上下文窗口。".to_string());
            cx.notify();
            return;
        }
        let levels: Vec<String> = editor
            .levels
            .read(cx)
            .content()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect();
        if levels.is_empty() && editor.reasoning {
            self.settings.model_editor.error =
                Some("启用推理时需要至少一个推理档位（如 low, medium, high）。".to_string());
            cx.notify();
            return;
        }
        let mut model = default_model_metadata(&model_id, &name);
        model.context_window = context.unwrap_or(DEFAULT_CONTEXT_WINDOW);
        model.max_output_tokens = max_output.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        model.enabled = editor.enabled;
        model.capabilities = ModelCapabilities {
            tool_call: editor.tool_call,
            vision: editor.vision,
            reasoning: editor.reasoning,
            json_schema: false,
            web_search: false,
        };
        model.reasoning_levels = levels
            .iter()
            .map(|value| effibuddy_agent_core::ReasoningLevel::new(value.clone(), value.clone()))
            .collect();
        model.default_reasoning_level = levels.first().cloned();
        let original = self.settings.model_editor.original_id.clone();
        let result = match original {
            Some(original) => self.models.update_model(&provider_id, &original, model),
            None => self.models.add_model(&provider_id, model),
        };
        match result {
            Ok(()) => {
                self.settings.provider_status = Some((true, "模型已保存。".to_string()));
                self.settings.model_editor.open = false;
                self.settings.model_editor.error = None;
            }
            Err(error) => self.settings.model_editor.error = Some(error),
        }
        cx.notify();
    }

    // --- rendering ---------------------------------------------------------

    pub(crate) fn render_settings(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme.clone();
        let scroll_entity = cx.entity().downgrade();
        // The Agents section catalogs profiles by scanning disk. Cache the
        // catalog for as long as the section stays open instead of rescanning
        // on every render (each keystroke notifies the whole view).
        if self.settings.section == SettingsSection::Agents {
            if self.agents_catalog.is_none() {
                let workspace = self.active_workspace();
                let user_agents_dir = crate::config::base_dir().join("agents");
                self.agents_catalog = Some(Rc::new(merged_subagent_profiles(
                    &user_agents_dir,
                    Some(&workspace),
                )));
            }
        } else {
            self.agents_catalog = None;
        }
        let nav = self.render_settings_nav(&theme, cx);
        let content = match self.settings.section {
            SettingsSection::General => self.render_settings_general(&theme, cx),
            SettingsSection::Models => self.render_settings_models(&theme, cx),
            SettingsSection::Context => self.render_settings_context(&theme, cx),
            SettingsSection::Agents => self.render_settings_agents(&theme),
            SettingsSection::Archived => self.render_settings_archived(&theme, cx),
            SettingsSection::Appearance => self.render_settings_appearance(&theme, cx),
            SettingsSection::Memory => crate::memory::render_panel(self, &theme, cx),
            SettingsSection::About => self.render_settings_about(&theme, cx),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(52.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_6()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        IconButton::new("settings-back", icons::CHEVRON_LEFT, &theme)
                            .size(28.0, 15.0)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.settings_open = false;
                                this.composer_focus_handle(cx).focus(window);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("设置"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child("Ctrl+Shift+S 打开/关闭"),
                    ),
            )
            .child(
                div().flex().flex_1().min_h(px(0.0)).child(nav).child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .child(
                            div()
                                .id("settings-content-scroll")
                                .size_full()
                                .overflow_y_scroll()
                                .track_scroll(self.scrolls.settings.handle())
                                .child(content),
                        )
                        .when(
                            !self.modal_blocks_scroll() && !self.settings.model_editor.open,
                            |element| {
                                element.child(
                                    smooth_overlay(
                                        self.scrolls.settings.clone(),
                                        scroll_entity.clone(),
                                    )
                                    .absolute()
                                    .inset_0(),
                                )
                            },
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_settings_nav(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<AnyElement> = [
            SettingsSection::General,
            SettingsSection::Models,
            SettingsSection::Context,
            SettingsSection::Agents,
            SettingsSection::Archived,
            SettingsSection::Appearance,
            SettingsSection::Memory,
            SettingsSection::About,
        ]
        .into_iter()
        .map(|section| {
            let active = self.settings.section == section;
            div()
                .id(SharedString::from(format!("settings-nav-{:?}", section)))
                .flex()
                .items_center()
                .gap_2()
                .h(px(32.0))
                .px_2()
                .rounded(px(8.0))
                .cursor_pointer()
                .when(active, |element| element.bg(theme.accent_soft))
                .when(!active, |element| {
                    element.hover(|style| style.bg(theme.hover))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings.section = section;
                    cx.notify();
                }))
                .child(
                    svg()
                        .path(section.icon())
                        .size(px(13.0))
                        .text_color(if active { theme.text } else { theme.text_faint }),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if active { theme.text } else { theme.text_muted })
                        .child(section.label()),
                )
                .into_any_element()
        })
        .collect();

        div()
            .w(px(184.0))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(theme.border)
            .px_3()
            .py_4()
            .flex()
            .flex_col()
            .gap_1()
            .children(rows)
            .into_any_element()
    }

    fn render_settings_general(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let status = self.settings.status.clone();
        centered_settings_column(
            div()
                .child(settings_page_header(
                    theme,
                    "通用",
                    "权限、演示模式与工作区",
                ))
                .child(
                    settings_card(theme, "行为", "默认安全优先")
                        .child(permission_mode_row(theme, self.settings.default_mode, cx))
                        .child(computer_use_row(theme, self.settings.computer_use_mode, cx))
                        .child(browser_use_row(theme, self.settings.browser_use_mode, cx))
                        .child(toggle_row(
                            theme,
                            "强制离线 Demo",
                            "即使配置了 Key，也使用本地演示 Provider。",
                            self.settings.demo,
                            cx.listener(|this, _, _, cx| {
                                this.settings.demo = !this.settings.demo;
                                cx.notify();
                            }),
                        )),
                )
                .child(
                    settings_card(theme, "工作区", "新会话与工具沙箱的根目录")
                        .child(self.settings.workspace_view.clone())
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child("修改后对新建会话生效；已有会话保留原工作区。"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new("save-general", "保存设置", theme)
                                .variant(ButtonVariant::Primary)
                                .size(ButtonSize::Sm)
                                .icon(icons::CHECK)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save_general_settings(window, cx)
                                })),
                        )
                        .when_some(status, |element, (ok, message)| {
                            element.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(if ok { theme.success } else { theme.danger })
                                    .child(message),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// 上下文压缩：所有阈值都以当前模型上下文窗口的百分比表示。
    fn render_settings_context(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let status = self.settings.status.clone();
        let (window, max_output) = self
            .active
            .as_ref()
            .map(|active| {
                let session = effibuddy_agent_core::lock_session(&active.shared);
                let resolved = self.models.resolve_session(&session);
                (
                    resolved.context_window(),
                    resolved.max_output_tokens() as usize,
                )
            })
            .unwrap_or((DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT_TOKENS as usize));
        centered_settings_column(
            div()
                .child(settings_page_header(
                    theme,
                    "上下文压缩",
                    "按模型上下文窗口的百分比控制；修改后对新任务生效",
                ))
                .child(
                    settings_card(theme, "自动压缩", "长会话超过阈值后生成滚动摘要")
                        .child(toggle_row(
                            theme,
                            "启用自动压缩",
                            "关闭后仍保留旧摘要，但不会自动生成新的摘要。",
                            self.settings.compact_enabled,
                            cx.listener(|this, _, _, cx| {
                                this.settings.compact_enabled = !this.settings.compact_enabled;
                                cx.notify();
                            }),
                        ))
                        .child(compact_number_row(
                            theme,
                            "触发阈值",
                            "上下文占用达到窗口的该百分比时压缩。默认 80%。",
                            self.settings.compact_trigger_view.clone(),
                            "%",
                        ))
                        .child(compact_number_row(
                            theme,
                            "保留最近",
                            "摘要后保留最近多少百分比的上下文原文。默认 20%。",
                            self.settings.compact_keep_view.clone(),
                            "%",
                        ))
                        .child(compact_number_row(
                            theme,
                            "摘要上限",
                            "生成摘要最多使用窗口的该百分比。默认 10%。",
                            self.settings.compact_summary_view.clone(),
                            "%",
                        ))
                        .child(toggle_row(
                            theme,
                            "后台预压缩（试验）",
                            "达到起点后后台写摘要，主会话继续运行；到阈值时直接换入。提前生成但未使用的摘要仍计费。",
                            self.settings.compact_background,
                            cx.listener(|this, _, _, cx| {
                                this.settings.compact_background =
                                    !this.settings.compact_background;
                                cx.notify();
                            }),
                        ))
                        .when(self.settings.compact_background, |element| {
                            element.child(compact_number_row(
                                theme,
                                "预压缩起点",
                                "达到窗口的该百分比时开始后台摘要；必须低于触发阈值。默认 50%。",
                                self.settings.compact_background_start_view.clone(),
                                "%",
                            ))
                        })
                        .child({
                            let trigger = self
                                .settings
                                .compact_trigger
                                .read(cx)
                                .content()
                                .trim()
                                .trim_end_matches('%')
                                .parse::<f32>()
                                .unwrap_or(self.config.compaction.trigger_percent)
                                .clamp(10.0, 95.0);
                            let reserve = max_output.min(window / 4);
                            let trigger_tokens = ((window as f32 * trigger / 100.0) as usize)
                                .min(window.saturating_sub(reserve));
                            let background = self
                                .settings
                                .compact_background_start
                                .read(cx)
                                .content()
                                .trim()
                                .trim_end_matches('%')
                                .parse::<f32>()
                                .unwrap_or(self.config.compaction.background_start_percent)
                                .clamp(5.0, 90.0)
                                .min(trigger - 5.0)
                                .max((trigger - 30.0).max(5.0));
                            let background_tokens =
                                ((window as f32 * background / 100.0) as usize)
                                    .min(trigger_tokens.saturating_sub(1));
                            let background_hint = if self.settings.compact_background {
                                format!(
                                    " · 后台约 {} 开始",
                                    format_context_window(background_tokens)
                                )
                            } else {
                                String::new()
                            };
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child(format!(
                                    "当前模型窗口 {} · 输出预留 {} · 约 {} tokens 触发{background_hint}",
                                    format_context_window(window),
                                    format_context_window(max_output),
                                    format_context_window(trigger_tokens)
                                ))
                        }),
                )
                .child(
                    settings_card(theme, "旧工具输出", "先清理噪声，再决定是否花一次摘要请求")
                        .child(toggle_row(
                            theme,
                            "清理旧工具结果",
                            "达到触发阈值的 90% 时，把较早的读取/搜索/命令输出替换为占位符。",
                            self.settings.compact_clear_tools,
                            cx.listener(|this, _, _, cx| {
                                this.settings.compact_clear_tools =
                                    !this.settings.compact_clear_tools;
                                cx.notify();
                            }),
                        ))
                        .child(compact_number_row(
                            theme,
                            "保留最近结果",
                            "最近 N 条工具结果永远保留原文；错误与图片结果也会保留。",
                            self.settings.compact_keep_results_view.clone(),
                            "条",
                        )),
                )
                .child(
                    settings_card(theme, "行为说明", "参考 ZCode 的压缩链路")
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.text_muted)
                                .child("摘要由当前会话模型生成；摘要请求失败时回退为本地摘录，不会中断任务。"),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.text_muted)
                                .child("供应商仍报上下文超限时，会强制压缩并重试一次请求。"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new("save-compaction", "保存设置", theme)
                                .variant(ButtonVariant::Primary)
                                .size(ButtonSize::Sm)
                                .icon(icons::CHECK)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.save_compaction_settings(cx)
                                })),
                        )
                        .when_some(status, |element, (ok, message)| {
                            element.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(if ok { theme.success } else { theme.danger })
                                    .child(message),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// ZCode-style subagent catalog: built-in / user / project groups, the
    /// Markdown contract and parser diagnostics for invalid files.
    fn render_settings_agents(&self, theme: &Theme) -> AnyElement {
        let workspace = self.active_workspace();
        let user_agents_dir = crate::config::base_dir().join("agents");
        let catalog = self.agents_catalog.clone().unwrap_or_else(|| {
            Rc::new(merged_subagent_profiles(&user_agents_dir, Some(&workspace)))
        });
        let (profiles, diagnostics) = (&catalog.0, &catalog.1);

        let mut profile_by_source: [(ProfileSource, Vec<&SubagentProfile>); 3] = [
            (ProfileSource::BuiltIn, Vec::new()),
            (ProfileSource::User, Vec::new()),
            (ProfileSource::Project, Vec::new()),
        ];
        for profile in profiles {
            profile_by_source[profile.source as usize].1.push(profile);
        }

        fn agent_row(theme: &Theme, profile: &SubagentProfile) -> AnyElement {
            let mut traits: Vec<String> = Vec::new();
            if profile.read_only {
                traits.push("只读".to_string());
            }
            if profile.allow_spawn {
                traits.push("可再派生".to_string());
            }
            traits.push(if profile.tools.is_empty() {
                "继承全部工具".to_string()
            } else {
                format!("{} 个工具", profile.tools.len())
            });
            if !profile.disallowed_tools.is_empty() {
                traits.push(format!("禁用 {}", profile.disallowed_tools.join(", ")));
            }
            match profile.model.as_deref().map(str::trim) {
                Some(model) if !model.is_empty() && !model.eq_ignore_ascii_case("inherit") => {
                    traits.push(format!("模型 {model}"));
                }
                _ => {}
            }
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .pb(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(profile.name.clone()),
                        )
                        .child(badge(theme, profile.source.label(), BadgeTone::Neutral)),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .line_height(px(16.0))
                        .text_color(theme.text_muted)
                        .child(profile.description.clone()),
                )
                .child(
                    div().flex().flex_wrap().gap_1().children(
                        traits
                            .iter()
                            .map(|trait_label| badge(theme, trait_label, BadgeTone::Neutral)),
                    ),
                )
                .into_any_element()
        }

        let mut cards = settings_card(
            theme,
            "子代理",
            "agent 工具可委派的代理类型目录；Markdown 档案可扩展或覆盖内置类型",
        )
        .child(
            div()
                .text_size(px(10.5))
                .line_height(px(16.0))
                .text_color(theme.text_faint)
                .child(format!(
                    "用户级目录：{}
项目级目录：{}
一个 .md 文件即一个代理：frontmatter 声明 name / description / tools / disallowed_tools / model / color / read_only / allow_spawn，正文作为该代理的 system prompt。",
                    user_agents_dir.display(),
                    workspace
                        .join(".effibuddy")
                        .join("agents")
                        .display()
                )),
        );
        for (source, group) in profile_by_source {
            if group.is_empty() {
                continue;
            }
            let mut group_block = div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .mt(px(10.0))
                .child(section_label(theme, source.label()));
            for profile in group {
                group_block = group_block.child(agent_row(theme, profile));
            }
            cards = cards.child(group_block);
        }
        let cards = cards.into_any_element();

        let diagnostics_card = if diagnostics.is_empty() {
            None
        } else {
            let mut card = settings_card(theme, "诊断", "无法加载的档案文件");
            for diagnostic in diagnostics {
                card = card.child(
                    div()
                        .flex()
                        .items_start()
                        .gap_2()
                        .text_size(px(11.0))
                        .line_height(px(16.0))
                        .child(
                            div()
                                .text_color(theme.warning)
                                .child(format!("{}：{}", diagnostic.file, diagnostic.message)),
                        ),
                );
            }
            Some(card.into_any_element())
        };

        let mut column = div()
            .child(settings_page_header(
                theme,
                "子代理",
                "内置、用户与项目档案",
            ))
            .child(cards);
        if let Some(diagnostics_card) = diagnostics_card {
            column = column.child(diagnostics_card);
        }
        centered_settings_column(column.into_any_element())
    }

    /// Settings → 归档: conversations that were explicitly finished and
    /// removed from the sidebar. Restore puts them back; delete removes the
    /// transcript from disk after a second click.
    fn render_settings_archived(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        for summary in &self.archived_sessions {
            let id = summary.id.clone();
            let confirm = self.settings.archived_delete_confirm.as_deref() == Some(id.as_str());
            let restore_id = id.clone();
            let delete_id = id.clone();
            let project_label = self
                .project_group(summary.project_id.as_deref(), summary.project.as_deref())
                .map(|group| group.name)
                .unwrap_or_else(|| "任务".to_string());
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .py(px(10.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(summary.title.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child(format!(
                                        "{} · {} 条消息 · {}",
                                        crate::app::relative_time(summary.updated_at),
                                        summary.message_count,
                                        project_label
                                    )),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("restore-{id}")), "恢复", theme)
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .icon(icons::ARCHIVE_RESTORE)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.restore_archived_session(&restore_id, cx);
                            })),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("delete-archived-{id}")),
                            if confirm { "确认删除" } else { "删除" },
                            theme,
                        )
                        .variant(if confirm {
                            ButtonVariant::Danger
                        } else {
                            ButtonVariant::Ghost
                        })
                        .size(ButtonSize::Sm)
                        .icon(icons::TRASH)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.delete_archived_session(&delete_id, cx);
                        })),
                    )
                    .into_any_element(),
            );
        }

        let list: AnyElement = if rows.is_empty() {
            div()
                .py(px(18.0))
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child("还没有归档的会话"),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .child("在侧栏会话上点击归档图标，完成的工作就会收进这里。"),
                )
                .into_any_element()
        } else {
            div().children(rows).into_any_element()
        };

        let status = self.settings.status.clone();
        centered_settings_column(
            div()
                .child(settings_page_header(
                    theme,
                    "归档",
                    "已收起的会话；不占用侧栏，也不会在启动时自动打开",
                ))
                .child(
                    settings_card(theme, "归档的会话", "恢复或永久删除")
                        .child(list)
                        .when_some(status, |element, (ok, message)| {
                            element.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(if ok { theme.success } else { theme.danger })
                                    .child(message),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_settings_appearance(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        centered_settings_column(
            div()
                .child(settings_page_header(theme, "外观", "浅色与深色主题"))
                .child(
                    settings_card(theme, "主题", "立即应用，无需保存").child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("theme-dark", "深色", theme)
                                    .size(ButtonSize::Sm)
                                    .variant(if theme.is_dark() {
                                        ButtonVariant::Primary
                                    } else {
                                        ButtonVariant::Secondary
                                    })
                                    .icon(icons::MOON)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if !this.theme.is_dark() {
                                            this.toggle_theme(cx);
                                        }
                                    })),
                            )
                            .child(
                                Button::new("theme-light", "浅色", theme)
                                    .size(ButtonSize::Sm)
                                    .variant(if !theme.is_dark() {
                                        ButtonVariant::Primary
                                    } else {
                                        ButtonVariant::Secondary
                                    })
                                    .icon(icons::SUN)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.theme.is_dark() {
                                            this.toggle_theme(cx);
                                        }
                                    })),
                            ),
                    ),
                )
                .child(
                    settings_card(theme, "渲染", "画面与显示器刷新率的同步方式").child(toggle_row(
                        theme,
                        "垂直同步",
                        "开启后按显示器刷新率呈现画面（60Hz 屏即最多 60 帧）。关闭会以远超刷新率的速度重绘，徒增 GPU/CPU 开销，仅用于性能调试。",
                        self.config.vsync,
                        cx.listener(|this, _, window, cx| {
                            let next = !this.config.vsync;
                            this.set_vsync(next, window, cx);
                        }),
                    )),
                )
                .into_any_element(),
        )
    }

    fn render_settings_about(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = crate::update::current_version();

        // Version card: running build, then whatever the last check learned.
        let mut update_rows = vec![stat_row(theme, "当前版本", current)];
        if self.update.checking {
            update_rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Spinner::new(theme.accent, 12.0))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.text_muted)
                            .child("正在检查更新…"),
                    )
                    .into_any_element(),
            );
        } else if let Some(error) = &self.update.error {
            update_rows.push(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.warning)
                    .child(error.clone())
                    .into_any_element(),
            );
        } else if let Some(info) = self.available_update() {
            update_rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.text_muted)
                            .child(format!("最新版本 {}", info.version)),
                    )
                    .child(badge(theme, "有可用更新", BadgeTone::Accent))
                    .into_any_element(),
            );
        } else if let Some(latest) = &self.update.latest {
            update_rows.push(stat_row(
                theme,
                "最新版本",
                &format!("{}（已是最新）", latest.version),
            ));
        } else {
            update_rows.push(stat_row(theme, "最新版本", "尚未检查"));
        }

        let mut update_buttons = vec![
            Button::new(
                "update-check",
                if self.update.checking {
                    "检查中…"
                } else {
                    "检查更新"
                },
                theme,
            )
            .variant(ButtonVariant::Primary)
            .size(ButtonSize::Sm)
            .on_click(cx.listener(|this, _, _, cx| {
                this.start_update_check(cx);
            }))
            .into_any_element(),
        ];
        if let Some(info) = self.available_update() {
            let url = info.html_url.clone();
            update_buttons.push(
                Button::new("update-open", "打开发布页", theme)
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.open_url(&url);
                    }))
                    .into_any_element(),
            );
            update_buttons.push(
                Button::new("update-skip", "跳过此版本", theme)
                    .variant(ButtonVariant::Secondary)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.skip_update(cx);
                    }))
                    .into_any_element(),
            );
        }

        centered_settings_column(
            div()
                .child(settings_page_header(theme, "关于", "版本与本地路径"))
                .child(
                    settings_card(theme, "版本", "运行中的构建与最新发布")
                        .children(update_rows)
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .children(update_buttons),
                        ),
                )
                .child(
                    settings_card(theme, "运行时", "EffiBuddy · Rust + GPUI")
                        .child(stat_row(theme, "运行时", "effibuddy-agent-core"))
                        .child(stat_row(theme, "PTC 引擎", "Boa (纯 Rust JavaScript)"))
                        .child(stat_row(
                            theme,
                            "数据环境",
                            if cfg!(debug_assertions) {
                                "开发调试（EffiBuddy-Dev）"
                            } else {
                                "正式用户（EffiBuddy）"
                            },
                        ))
                        .child(stat_row(
                            theme,
                            "会话目录",
                            &self.store.directory().display().to_string(),
                        ))
                        .child(stat_row(
                            theme,
                            "配置",
                            &AppConfig::path().display().to_string(),
                        ))
                        .child(stat_row(
                            theme,
                            "模型注册表",
                            &ModelConfigStore::path().display().to_string(),
                        )),
                )
                .into_any_element(),
        )
    }

    fn render_settings_models(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let load_error = self.models.load_error().map(str::to_string);
        let providers = self.models.file().providers.clone();
        let builtin: Vec<_> = providers
            .iter()
            .filter(|provider| provider.builtin)
            .collect();
        let custom: Vec<_> = providers
            .iter()
            .filter(|provider| !provider.builtin)
            .collect();
        let selected_id = self.settings.selected_provider.clone();
        let default_selection = self.models.default_selection();

        let provider_rows = |items: &[&ProviderConfig]| -> Vec<AnyElement> {
            items
                .iter()
                .map(|provider| {
                    let id = provider.id.clone();
                    let active = selected_id.as_deref() == Some(provider.id.as_str());
                    let configured = provider.has_credentials();
                    let default = default_selection
                        .as_ref()
                        .is_some_and(|selection| selection.provider_id == provider.id);
                    div()
                        .id(SharedString::from(format!("provider-row-{}", provider.id)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(34.0))
                        .px_2()
                        .rounded(px(8.0))
                        .cursor_pointer()
                        .when(active, |element| element.bg(theme.accent_soft))
                        .when(!active, |element| {
                            element.hover(|style| style.bg(theme.hover))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_provider(&id, cx);
                        }))
                        .child(div().size(px(6.0)).rounded(px(999.0)).flex_none().bg(
                            if !provider.enabled {
                                theme.text_faint
                            } else if configured {
                                theme.success
                            } else {
                                theme.warning
                            },
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(11.5))
                                .text_color(if active { theme.text } else { theme.text_muted })
                                .child(provider.display_name().to_string()),
                        )
                        .when(default, |element| {
                            element.child(badge(theme, "默认", BadgeTone::Accent))
                        })
                        .child(
                            div()
                                .text_size(px(9.5))
                                .text_color(theme.text_faint)
                                .child(format!("{}", provider.models.len())),
                        )
                        .into_any_element()
                })
                .collect()
        };

        let builtin_rows = provider_rows(&builtin);
        let custom_rows = provider_rows(&custom);

        let detail = self.render_provider_detail(theme, cx);

        div()
            .w_full()
            .max_w(px(1080.0))
            .mx_auto()
            .p_6()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .when_some(load_error, |element, error| {
                element.child(
                    div()
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(theme.warning)
                        .bg(theme.warning_soft)
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_size(px(11.0))
                        .text_color(theme.warning)
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(17.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("模型供应商"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_muted)
                                    .child("供应商决定端点、凭据与协议；模型决定上下文窗口、最大输出和能力。每个会话可以选择自己的模型。"),
                            ),
                    )
                    .child(
                        Button::new("add-provider", "新增供应商", theme)
                            .variant(ButtonVariant::Secondary)
                            .size(ButtonSize::Sm)
                            .icon(icons::PLUS)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.add_custom_provider(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface)
                    .flex()
                    .min_h(px(520.0))
                    .child(
                        div()
                            .w(px(210.0))
                            .flex_none()
                            .border_r_1()
                            .border_color(theme.border)
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(section_label(theme, "预置供应商"))
                            .children(builtin_rows)
                            .child(div().h(px(10.0)))
                            .child(section_label(theme, "自定义"))
                            .children(custom_rows)
                            .when(custom.is_empty(), |element| {
                                element.child(
                                    div()
                                        .px_2()
                                        .py_2()
                                        .text_size(px(10.5))
                                        .text_color(theme.text_faint)
                                        .child("还没有自定义供应商。"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap(px(14.0))
                            .child(detail),
                    ),
            )
            .into_any_element()
    }

    fn render_provider_detail(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(provider) = self
            .settings
            .selected_provider
            .as_deref()
            .and_then(|id| self.models.file().provider(id))
            .cloned()
        else {
            return div()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child("选择左侧供应商以编辑。")
                .into_any_element();
        };
        let default_selection = self.models.default_selection();
        let status = self.settings.provider_status.clone();
        let key_configured = !provider.effective_api_key().is_empty();
        let api_format = self.settings.selected_api_format(&provider);
        let provider_id = provider.id.clone();

        let format_buttons: Vec<AnyElement> = [
            ApiFormat::OpenAiChatCompletions,
            ApiFormat::AnthropicMessages,
            ApiFormat::OpenAiResponses,
        ]
        .into_iter()
        .map(|format| {
            let active = format == api_format;
            let label = match format {
                ApiFormat::OpenAiChatCompletions => "Chat",
                ApiFormat::AnthropicMessages => "Messages",
                ApiFormat::OpenAiResponses => "Responses",
            };
            Button::new(
                SharedString::from(format!("api-format-{:?}", format)),
                label,
                theme,
            )
            .size(ButtonSize::Sm)
            .variant(if active {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Secondary
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings.set_api_format(format);
                cx.notify();
            }))
            .into_any_element()
        })
        .collect();

        let model_rows: Vec<AnyElement> = provider
            .models
            .iter()
            .map(|model| {
                let model_id = model.id.clone();
                let provider_id = provider_id.clone();
                let is_default = default_selection.as_ref().is_some_and(|selection| {
                    selection.provider_id == provider_id && selection.model_id == model_id
                });
                let editing = model.clone();
                let delete_id = model.id.clone();
                let toggle_id = model.id.clone();
                let toggle_provider = provider_id.clone();
                let defaults_provider = provider_id.clone();
                let defaults_model = model.id.clone();
                div()
                    .id(SharedString::from(format!(
                        "model-row-{}:{}",
                        provider_id, model.id
                    )))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(if is_default {
                        theme.border_strong
                    } else {
                        theme.border
                    })
                    .bg(if is_default {
                        theme.surface_alt
                    } else {
                        theme.bg
                    })
                    .p(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(model.display_name().to_string()),
                            )
                            .when(is_default, |element| {
                                element.child(badge(theme, "默认", BadgeTone::Accent))
                            })
                            .when(!model.enabled, |element| {
                                element.child(badge(theme, "已停用", BadgeTone::Neutral))
                            })
                            .child(
                                switch(theme, model.enabled)
                                    .id(SharedString::from(format!(
                                        "model-switch-{}:{}",
                                        provider_id, toggle_id
                                    )))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_model_enabled(&toggle_provider, &toggle_id, cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .font_family(theme.mono_font())
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(model.id.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            // Both budgets are per model and edited through the
                            // model editor. Name them after what the user
                            // controls: the input budget is what is left of the
                            // window after reserving the output budget.
                            .child(badge(
                                theme,
                                &format!(
                                    "输入 {}",
                                    format_context_window(
                                        model
                                            .context_window
                                            .saturating_sub(
                                                model.effective_max_output_tokens() as usize
                                            )
                                            .max(1)
                                    )
                                ),
                                BadgeTone::Neutral,
                            ))
                            .child(badge(
                                theme,
                                &format!(
                                    "输出 {}",
                                    format_context_window(
                                        model.effective_max_output_tokens() as usize
                                    )
                                ),
                                BadgeTone::Accent,
                            ))
                            .children(
                                model
                                    .capabilities
                                    .labels()
                                    .into_iter()
                                    .map(|label| badge(theme, label, BadgeTone::Neutral)),
                            )
                            .children(model.reasoning_levels.iter().map(|level| {
                                badge(theme, level.display_label().to_string(), BadgeTone::Success)
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new(
                                    SharedString::from(format!("model-default-{}", model.id)),
                                    if is_default {
                                        "已默认"
                                    } else {
                                        "设为默认"
                                    },
                                    theme,
                                )
                                .variant(ButtonVariant::Ghost)
                                .size(ButtonSize::Sm)
                                .icon(icons::CHECK)
                                .disabled(is_default)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.set_default_model(
                                            &defaults_provider,
                                            &defaults_model,
                                            cx,
                                        );
                                    },
                                )),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!("model-edit-{}", model.id)),
                                    "编辑参数",
                                    theme,
                                )
                                .variant(ButtonVariant::Secondary)
                                .size(ButtonSize::Sm)
                                .icon(icons::EDIT)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.open_model_editor(Some(editing.clone()), window, cx);
                                    },
                                )),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!("model-delete-{}", model.id)),
                                    if self.settings.model_delete_confirm.as_deref()
                                        == Some(delete_id.as_str())
                                    {
                                        "确认删除"
                                    } else {
                                        "删除"
                                    },
                                    theme,
                                )
                                .variant(
                                    if self.settings.model_delete_confirm.as_deref()
                                        == Some(delete_id.as_str())
                                    {
                                        ButtonVariant::Danger
                                    } else {
                                        ButtonVariant::Ghost
                                    },
                                )
                                .size(ButtonSize::Sm)
                                .icon(icons::TRASH)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.delete_model(&provider_id, &delete_id, cx);
                                    },
                                )),
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(14.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(provider.display_name().to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child(if provider.builtin {
                                        "内置供应商 · 可编辑，不可删除"
                                    } else {
                                        "自定义供应商"
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_faint)
                                    .child("启用"),
                            )
                            .child(
                                switch(theme, self.settings.provider_enabled)
                                    .id("provider-enabled-switch")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings.provider_enabled =
                                            !this.settings.provider_enabled;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(!provider.builtin, |element| {
                        element.child(
                            Button::new(
                                "delete-provider",
                                if self.settings.provider_delete_confirm {
                                    "确认删除供应商"
                                } else {
                                    "删除供应商"
                                },
                                theme,
                            )
                            .variant(if self.settings.provider_delete_confirm {
                                ButtonVariant::Danger
                            } else {
                                ButtonVariant::Ghost
                            })
                            .size(ButtonSize::Sm)
                            .icon(icons::TRASH)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.delete_selected_provider(cx);
                            })),
                        )
                    }),
            )
            .child(
                settings_card(theme, "连接", "端点、凭据与协议")
                    .child(field_label(theme, "供应商名称", "显示在模型选择器中"))
                    .child(self.settings.provider_name_view.clone())
                    .child(field_label(theme, "API 格式", "与端点匹配的线协议"))
                    .child(div().flex().flex_wrap().gap_1().children(format_buttons))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(format!(
                                "请求路径：{}",
                                match api_format {
                                    ApiFormat::OpenAiChatCompletions => "/chat/completions",
                                    ApiFormat::AnthropicMessages => "/v1/messages",
                                    ApiFormat::OpenAiResponses => "/responses",
                                }
                            )),
                    )
                    .child(field_label(
                        theme,
                        "Base URL",
                        "例如 https://api.example.com/v1",
                    ))
                    .child(self.settings.provider_base_url_view.clone())
                    .child(field_label(
                        theme,
                        "API Key",
                        if key_configured {
                            "已配置 · 留空不修改"
                        } else {
                            "尚未配置"
                        },
                    ))
                    .child(self.settings.provider_api_key_view.clone())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("save-provider", "保存", theme)
                                    .variant(ButtonVariant::Primary)
                                    .size(ButtonSize::Sm)
                                    .icon(icons::CHECK)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_provider(cx);
                                    })),
                            )
                            .child(
                                Button::new("test-provider", "测试连接", theme)
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .icon(icons::CLOCK)
                                    .loading(self.settings.provider_testing)
                                    .disabled(self.settings.provider_testing)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.test_selected_provider(cx);
                                    })),
                            )
                            .child(
                                Button::new("fetch-models", "获取模型", theme)
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .icon(icons::LOADER)
                                    .loading(self.settings.provider_fetching)
                                    .disabled(self.settings.provider_fetching)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.fetch_provider_models(cx);
                                    })),
                            )
                            .when(key_configured, |element| {
                                element.child(
                                    Button::new("clear-key", "清除 Key", theme)
                                        .variant(ButtonVariant::Ghost)
                                        .size(ButtonSize::Sm)
                                        .icon(icons::X)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.clear_provider_key(cx);
                                        })),
                                )
                            }),
                    )
                    .when_some(status, |element, (ok, message)| {
                        element.child(
                            div()
                                .text_size(px(11.0))
                                .line_height(px(17.0))
                                .text_color(if ok { theme.success } else { theme.danger })
                                .child(message),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(section_label(theme, "模型"))
                            .child(
                                Button::new("add-model", "添加模型", theme)
                                    .variant(ButtonVariant::Secondary)
                                    .size(ButtonSize::Sm)
                                    .icon(icons::PLUS)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_model_editor(None, window, cx);
                                    })),
                            ),
                    )
                    .when(model_rows.is_empty(), |element| {
                        element.child(
                            div()
                                .rounded(px(10.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.bg)
                                .p(px(12.0))
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child("还没有模型。点击「添加模型」或「获取模型」开始。"),
                        )
                    })
                    .children(model_rows)
                    .child(
                        div()
                            .text_size(px(10.5))
                            .line_height(px(16.0))
                            .text_color(theme.text_faint)
                            .child(
                                "输入上限与输出上限按模型单独配置：点任一模型的「编辑参数」即可修改。\
                                 输出上限即请求里的 max_tokens，输入上限是剩余可用上下文；\
                                 两者都会参与上下文压缩阈值计算，修改后对新任务生效。",
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_model_editor(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.settings.model_editor.open {
            return None;
        }
        let scroll_entity = cx.entity().downgrade();
        let theme = self.theme.clone();
        let editor = &self.settings.model_editor;
        Some(
            div()
                .id("model-editor-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .bg(theme.overlay)
                .flex()
                .justify_center()
                .items_start()
                .pt(px(56.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_model_editor(cx);
                    }),
                )
                .child(
                    div()
                        .relative()
                        .w(px(520.0))
                        .child(
                            div()
                                .id("model-editor")
                                .max_h(px(660.0))
                                .overflow_y_scroll()
                                .track_scroll(self.scrolls.model_editor.handle())
                                .rounded(px(16.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.elevated)
                                .shadow(effibuddy_ui::shadow_float(&theme))
                                .p_5()
                                .flex()
                                .flex_col()
                                .gap(px(12.0))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(14.0))
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(theme.text)
                                                .child(if editor.original_id.is_some() {
                                                    "编辑模型"
                                                } else {
                                                    "添加模型"
                                                }),
                                        )
                                        .child(
                                            IconButton::new("model-editor-close", icons::X, &theme)
                                                .size(26.0, 13.0)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.close_model_editor(cx);
                                                })),
                                        ),
                                )
                                .child(field_label(&theme, "模型 ID", "请求里发送的 model 字段"))
                                .child(editor.id_view.clone())
                                .child(field_label(&theme, "显示名称", "留空则使用模型 ID"))
                                .child(editor.name_view.clone())
                                // Both budgets are full-width rows like every
                                // other field in this modal. Side-by-side
                                // columns overflow the 520px panel: the two
                                // labels plus their right-aligned hints are
                                // wider than one half, and flex text does not
                                // shrink, so the right column was pushed
                                // outside the panel and became invisible.
                                .child(field_label(
                                    &theme,
                                    "输入上限（上下文窗口）",
                                    "模型能读入的 token",
                                ))
                                .child(editor.context_view.clone())
                                .child(field_label(
                                    &theme,
                                    "输出上限（max_tokens）",
                                    "单次响应最多生成的 token",
                                ))
                                .child(editor.max_output_view.clone())
                                .child(field_label(
                                    &theme,
                                    "推理档位",
                                    "逗号分隔；留空表示不支持选择",
                                ))
                                .child(editor.levels_view.clone())
                                .child(toggle_row(
                                    &theme,
                                    "启用",
                                    "停用后不出现在模型选择器中。",
                                    editor.enabled,
                                    cx.listener(|this, _, _, cx| {
                                        this.settings.model_editor.enabled =
                                            !this.settings.model_editor.enabled;
                                        cx.notify();
                                    }),
                                ))
                                .child(toggle_row(
                                    &theme,
                                    "工具调用",
                                    "允许模型使用 built-in tools。",
                                    editor.tool_call,
                                    cx.listener(|this, _, _, cx| {
                                        this.settings.model_editor.tool_call =
                                            !this.settings.model_editor.tool_call;
                                        cx.notify();
                                    }),
                                ))
                                .child(toggle_row(
                                    &theme,
                                    "视觉输入",
                                    "标记模型可接收图片（当前 UI 尚未发送图片）。",
                                    editor.vision,
                                    cx.listener(|this, _, _, cx| {
                                        this.settings.model_editor.vision =
                                            !this.settings.model_editor.vision;
                                        cx.notify();
                                    }),
                                ))
                                .child(toggle_row(
                                    &theme,
                                    "推理输出",
                                    "标记模型会产生 reasoning/thinking 内容。",
                                    editor.reasoning,
                                    cx.listener(|this, _, _, cx| {
                                        this.settings.model_editor.reasoning =
                                            !this.settings.model_editor.reasoning;
                                        cx.notify();
                                    }),
                                ))
                                .when_some(editor.error.clone(), |element, error| {
                                    element.child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(theme.danger)
                                            .child(error),
                                    )
                                })
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_end()
                                        .gap_2()
                                        .child(
                                            Button::new("model-editor-cancel", "取消", &theme)
                                                .variant(ButtonVariant::Ghost)
                                                .size(ButtonSize::Sm)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.close_model_editor(cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("model-editor-save", "保存模型", &theme)
                                                .variant(ButtonVariant::Primary)
                                                .size(ButtonSize::Sm)
                                                .icon(icons::CHECK)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.save_model_editor(cx);
                                                })),
                                        ),
                                ),
                        )
                        .when(!self.modal_blocks_scroll(), |element| {
                            element.child(
                                smooth_overlay(
                                    self.scrolls.model_editor.clone(),
                                    scroll_entity.clone(),
                                )
                                .absolute()
                                .inset_0(),
                            )
                        })
                        .with_animation(
                            "model-editor-panel-enter",
                            Animation::new(overlay_motion_duration()).with_easing(ease_out_cubic),
                            |element, delta| element.mt(px(6.0 * (1.0 - delta))),
                        ),
                )
                .with_animation(
                    "model-editor-backdrop-enter",
                    Animation::new(overlay_motion_duration()).with_easing(ease_out_cubic),
                    |element, delta| element.opacity(delta),
                )
                .into_any_element(),
        )
    }
}

impl SettingsState {
    fn set_api_format(&mut self, format: ApiFormat) {
        self.api_format_draft = Some(format);
    }

    fn selected_api_format(&self, provider: &ProviderConfig) -> ApiFormat {
        self.api_format_draft.unwrap_or(provider.api_format)
    }
}

async fn probe_provider(provider: ProviderConfig, model_id: String) -> Result<String, String> {
    let resolved = ResolvedModel::new(provider, Some(&model_id), None);
    let client = resolved
        .build_provider()
        .map_err(|error| error.to_string())?;
    let (events, _receiver) = unbounded_channel();
    let completion = client
        .stream(
            ChatRequest {
                model: resolved.model.id.clone(),
                messages: vec![ChatMessage::user("ping")],
                tools: Vec::new(),
                temperature: Some(0.0),
                max_tokens: Some(1),
                reasoning_effort: None,
            },
            CancellationToken::new(),
            events,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "连接成功 · {} · prompt {} / completion {} tokens",
        resolved.model_label(),
        completion.usage.prompt_tokens,
        completion.usage.completion_tokens
    ))
}

fn centered_settings_column(content: AnyElement) -> AnyElement {
    div()
        .w_full()
        .flex()
        .justify_center()
        .child(
            div()
                .w_full()
                .max_w(px(720.0))
                .p_6()
                .flex()
                .flex_col()
                .gap(px(20.0))
                .child(content),
        )
        .into_any_element()
}

fn settings_page_header(theme: &Theme, title: &str, subtitle: &str) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .pb(px(4.0))
        .child(
            div()
                .text_size(px(17.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(12.0))
                .line_height(px(18.0))
                .text_color(theme.text_muted)
                .child(subtitle.to_string()),
        )
        .into_any_element()
}

pub(crate) fn settings_card(theme: &Theme, title: &str, subtitle: &str) -> gpui::Div {
    div()
        .w_full()
        .rounded(px(14.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .p(px(20.0))
        .flex()
        .flex_col()
        .gap(px(14.0))
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child(subtitle.to_string()),
                ),
        )
}

pub(crate) fn field_label(theme: &Theme, label: &str, hint: &str) -> AnyElement {
    div()
        .flex()
        .items_baseline()
        .justify_between()
        .gap_2()
        .mt_2()
        .child(
            div()
                .flex_none()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(label.to_string()),
        )
        // The hint is the part that may be long; let it shrink instead of
        // pushing the row (and anything beside it) past the panel edge.
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_right()
                .truncate()
                .text_size(px(11.0))
                .text_color(theme.text_faint)
                .child(hint.to_string()),
        )
        .into_any_element()
}

/// The default permission mode picker: a four-way segmented row that replaces
/// the old "自动批准工具调用" switch. Applies to new sessions; Shift+Tab
/// switches the active session at any time.
fn permission_mode_row(
    theme: &Theme,
    selected: PermissionMode,
    cx: &mut Context<AppView>,
) -> AnyElement {
    const MODES: [(PermissionMode, &str); 4] = [
        (PermissionMode::Default, "默认询问"),
        (PermissionMode::AcceptEdits, "自动编辑"),
        (PermissionMode::Plan, "计划模式"),
        (PermissionMode::FullAccess, "完全访问"),
    ];
    let description = match selected {
        PermissionMode::Default => "写文件与命令在执行前都会请求批准。",
        PermissionMode::AcceptEdits => "自动批准文件编辑；shell 等命令仍需确认。",
        PermissionMode::Plan => "只读调研；编辑与命令被拒绝，先输出计划。",
        PermissionMode::FullAccess => "自动批准所有工具，尽量不打断。",
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .py_1()
        .child(
            div()
                .flex()
                .items_baseline()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("默认权限模式"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child("新会话的起点 · 会话中 Shift+Tab 循环"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .children(MODES.map(|(mode, label)| {
                    let active = selected == mode;
                    Button::new(
                        SharedString::from(format!("permission-mode-{}", label)),
                        label,
                        theme,
                    )
                    .size(ButtonSize::Sm)
                    .variant(if active {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.default_mode = mode;
                        cx.notify();
                    }))
                })),
        )
        .child(
            div()
                .text_size(px(11.5))
                .text_color(theme.text_faint)
                .child(description),
        )
        .into_any_element()
}

/// Computer-use capability switch. Approval is part of the tool permission
/// matrix, so this row only decides whether the tool is registered.
fn computer_use_row(
    theme: &Theme,
    selected: ComputerUseMode,
    cx: &mut Context<AppView>,
) -> AnyElement {
    const MODES: [(ComputerUseMode, &str); 2] =
        [(ComputerUseMode::Off, "关闭"), (ComputerUseMode::On, "开启")];
    let description = match selected {
        ComputerUseMode::Off => "不注册 computer 工具；输入框不会出现键鼠提示。",
        ComputerUseMode::On => {
            "注册 computer 工具，审批交给权限模式：默认询问、自动编辑仍询问、完全访问不再打断。\
             悬浮条可随时接管或停止。"
        }
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .py_1()
        .child(
            div()
                .flex()
                .items_baseline()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("电脑操作（键鼠模拟）"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child("读取屏幕并模拟鼠标键盘 · 主代理与子代理同一套审批"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .children(MODES.map(|(mode, label)| {
                    let active = selected == mode;
                    Button::new(
                        SharedString::from(format!("computer-mode-{label}")),
                        label,
                        theme,
                    )
                    .size(ButtonSize::Sm)
                    .variant(if active {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.computer_use_mode = mode;
                        cx.notify();
                    }))
                })),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child(description),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.warning)
                        .child("需要模型开启视觉能力，截图才能发给模型；Wayland 仅支持全屏。"),
                ),
        )
        .into_any_element()
}

/// Browser-use preference row. Mirrors `computer_use_row`, but the warning is
/// about untrusted page content rather than vision capability: the ref pipeline
/// works without a vision model.
fn browser_use_row(
    theme: &Theme,
    selected: BrowserUseMode,
    cx: &mut Context<AppView>,
) -> AnyElement {
    const MODES: [(BrowserUseMode, &str); 2] =
        [(BrowserUseMode::Off, "关闭"), (BrowserUseMode::On, "开启")];
    let description = match selected {
        BrowserUseMode::Off => "不注册 browser 工具；模型无法打开或读取任何网页。",
        BrowserUseMode::On => {
            "注册 browser 工具，审批交给权限模式：计划模式只放行只读动作，完全访问不再打断。"
        }
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .py_1()
        .child(
            div()
                .flex()
                .items_baseline()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("浏览器操作（Chromium / DevTools）"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child("结构树 + [ref] 句柄 · 主代理与子代理同一套审批"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .children(MODES.map(|(mode, label)| {
                    let active = selected == mode;
                    Button::new(
                        SharedString::from(format!("browser-mode-{label}")),
                        label,
                        theme,
                    )
                    .size(ButtonSize::Sm)
                    .variant(if active {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.browser_use_mode = mode;
                        cx.notify();
                    }))
                })),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child(description),
                )
                .child(div().text_size(px(11.0)).text_color(theme.warning).child(
                    "页面内容是不可信输入：不要把它直接当作命令或文件路径执行；\
                             使用时才会启动独立的浏览器实例。",
                )),
        )
        .into_any_element()
}

fn compact_number_row(
    theme: &Theme,
    title: &str,
    description: &str,
    view: Entity<TextInputView>,
    suffix: &str,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .py_1()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child(description.to_string()),
                ),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .child(div().w(px(76.0)).child(view))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child(suffix.to_string()),
                ),
        )
        .into_any_element()
}

pub(crate) fn toggle_row(
    theme: &Theme,
    title: &str,
    description: &str,
    on: bool,
    handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .py_1()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .child(description.to_string()),
                ),
        )
        .child(
            switch(theme, on)
                .id(SharedString::from(format!("switch-{title}")))
                .on_click(handler),
        )
        .into_any_element()
}

fn stat_row(theme: &Theme, label: &str, value: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .text_size(px(11.5))
                .text_color(theme.text_faint)
                .child(label.to_string()),
        )
        .child(
            div()
                .max_w(px(320.0))
                .truncate()
                .font_family(theme.mono_font())
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(value.to_string()),
        )
        .into_any_element()
}

fn format_context_window(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_format_defaults_are_stable() {
        assert_eq!(ApiFormat::default(), ApiFormat::OpenAiChatCompletions);
    }

    #[test]
    fn context_window_formatting() {
        assert_eq!(format_context_window(128_000), "128k");
        assert_eq!(format_context_window(1_048_576), "1.0m");
    }
}
