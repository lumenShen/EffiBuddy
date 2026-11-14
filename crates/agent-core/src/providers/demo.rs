//! A deterministic offline provider used by the app's demo mode.
//!
//! It exercises the real harness path — reasoning, streaming, tool calls, tool
//! observations and a second model step — without any network access.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::message::{ToolCall, truncate_chars};
use crate::provider::{
    ChatRequest, Completion, ModelProvider, ProviderEvent, chunk_chars, send_event,
};

pub struct DemoProvider {
    delay: Duration,
    chunk_size: usize,
}

impl DemoProvider {
    pub fn new() -> Self {
        Self {
            delay: Duration::from_millis(14),
            chunk_size: 2,
        }
    }
}

impl Default for DemoProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelProvider for DemoProvider {
    fn name(&self) -> &str {
        "demo"
    }

    fn context_window(&self) -> usize {
        64_000
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        events: UnboundedSender<ProviderEvent>,
    ) -> Result<Completion, ProviderError> {
        let (completion, reasoning) = plan_demo_response(&request);
        for chunk in chunk_chars(&reasoning, self.chunk_size) {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            send_event(&events, ProviderEvent::ReasoningDelta(chunk))?;
            tokio::time::sleep(self.delay).await;
        }
        for chunk in chunk_chars(&completion.text, self.chunk_size) {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            send_event(&events, ProviderEvent::TextDelta(chunk))?;
            tokio::time::sleep(self.delay).await;
        }
        Ok(completion)
    }
}

fn plan_demo_response(request: &ChatRequest) -> (Completion, String) {
    let last_user_index = request
        .messages
        .iter()
        .rposition(|message| message.role == "user");
    let user_text = last_user_index
        .map(|index| request.messages[index].content.clone())
        .unwrap_or_default();
    let tool_after = last_user_index.is_some_and(|index| {
        request.messages[index + 1..]
            .iter()
            .any(|message| message.role == "tool")
    });
    let last_tool = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "tool")
        .map(|message| message.content.clone());

    let lower = user_text.to_lowercase();
    let wants_files = ["文件", "目录", "项目结构", "file", "dir", "list", "tree"]
        .iter()
        .any(|needle| lower.contains(needle));
    let wants_plan = ["计划", "规划", "步骤", "todo", "plan", "step"]
        .iter()
        .any(|needle| lower.contains(needle));

    let reasoning =
        "离线演示模式：没有配置 API Key，我会用本地规则模拟一次真实的 agent 回合。\n".to_string();

    if wants_files && !tool_after {
        let mut completion = Completion::text("我先查看一下当前工作目录的结构。\n\n");
        completion
            .tool_calls
            .push(ToolCall::new("list_dir", json!({ "path": "." })));
        return (completion, reasoning);
    }

    if wants_files {
        if let Some(tool_output) = last_tool {
            let snippet: String = tool_output.chars().take(1_800).collect();
            let text = format!(
                "这是工具返回的真实目录内容：\n\n```text\n{snippet}\n```\n\n\
                 我现在运行在 **离线 Demo Provider** 上，只能演示这条工具链。\
                 在「设置」里填入 DeepSeek（或任意 OpenAI 兼容服务）的地址与 API Key 后，\
                 我就能针对你的项目做实际的读取、编辑和验证。"
            );
            return (Completion::text(text), reasoning);
        }
    }

    if wants_plan && !tool_after {
        let mut completion =
            Completion::text("我把这件事拆成了几个可跟踪的步骤，并写进左侧任务面板。\n\n");
        completion.tool_calls.push(ToolCall::new(
            "todo_write",
            json!({
                "todos": [
                    {"content": "确认目标与约束", "status": "completed"},
                    {"content": "阅读工作区与相关文件", "status": "in_progress"},
                    {"content": "实现改动并运行验证", "status": "pending"},
                    {"content": "总结结果与剩余风险", "status": "pending"}
                ]
            }),
        ));
        return (completion, reasoning);
    }

    if wants_plan {
        let text = "计划已经写入任务面板。离线模式下我无法真正规划代码改动；\
                    配置真实模型后，我会根据工具观察结果持续更新这份清单。"
            .to_string();
        return (Completion::text(text), reasoning);
    }

    let preview = if user_text.trim().is_empty() {
        "（空消息）".to_string()
    } else {
        truncate_chars(user_text.trim(), 240)
    };
    let text = format!(
        "收到：**{preview}**\n\n\
         当前是 **离线 Demo 模式**，还没有配置模型 API Key。我仍然走完了完整的 agent loop：\
         组织上下文 → 调用 Provider → 流式输出 → 写入会话记录。\n\n\
         可以试试：\n\
         - 「看看这个项目的文件」——会触发 `list_dir` 工具调用\n\
         - 「给我一个计划」——会触发 `todo_write`，任务会出现在右侧面板\n\n\
         打开「设置」填入 DeepSeek 的 Base URL 与 API Key，即可切换到真实模型。"
    );
    (Completion::text(text), reasoning)
}
