//! Todo list tool.

use async_trait::async_trait;
use serde_json::{Value, json};

use effibuddy_agent_core::{
    ApprovalPolicy, TodoItem, TodoStatus, Tool, ToolContext, ToolError, ToolOutput,
};

use crate::args::as_object;

const MAX_TODOS: usize = 50;
const MAX_CONTENT_CHARS: usize = 500;

#[derive(Clone, Default)]
pub struct TodoWriteTool;

impl TodoWriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "Replace the session todo list. Use for multi-step work: mark items \
         in_progress/completed as you go, and keep at most one item in progress."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "maxItems": MAX_TODOS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        ApprovalPolicy::Auto
    }

    async fn execute(&self, _ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let object = as_object(&arguments)?;
        let raw_todos = object
            .get("todos")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::new("`todos` must be an array"))?;
        if raw_todos.len() > MAX_TODOS {
            return Err(ToolError::new(format!(
                "too many todos ({}); limit is {MAX_TODOS}",
                raw_todos.len()
            )));
        }

        let mut todos = Vec::with_capacity(raw_todos.len());
        for (index, raw) in raw_todos.iter().enumerate() {
            let item = raw
                .as_object()
                .ok_or_else(|| ToolError::new(format!("todo[{index}] must be an object")))?;
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|content| !content.is_empty())
                .ok_or_else(|| {
                    ToolError::new(format!("todo[{index}].content must be a non-empty string"))
                })?;
            if content.chars().count() > MAX_CONTENT_CHARS {
                return Err(ToolError::new(format!(
                    "todo[{index}].content is longer than {MAX_CONTENT_CHARS} characters"
                )));
            }
            let status = match item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending")
            {
                "pending" => TodoStatus::Pending,
                "in_progress" => TodoStatus::InProgress,
                "completed" => TodoStatus::Completed,
                other => {
                    return Err(ToolError::new(format!(
                        "todo[{index}].status must be pending, in_progress or completed (got `{other}`)"
                    )));
                }
            };
            todos.push(TodoItem::new(content, status));
        }

        let in_progress = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::InProgress)
            .count();
        let completed = todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Completed)
            .count();
        let summary = format!(
            "Updated todo list: {} items ({} completed, {} in progress).",
            todos.len(),
            completed,
            in_progress
        );
        Ok(
            ToolOutput::text(summary)
                .with_effect(effibuddy_agent_core::ToolEffect::SetTodos(todos)),
        )
    }
}
