//! The `computer` tool: a thin adapter from the model protocol to
//! [`effibuddy_computer::ComputerRuntime`].
//!
//! Safety decisions (who may control, when to pause) live in the runtime and
//! the app's approval gate. This file only validates model input and shapes
//! observations.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use effibuddy_agent_core::{
    ApprovalPolicy, ImageAttachment, Tool, ToolContext, ToolError, ToolOutput,
};
use effibuddy_computer::{
    CaptureTarget, ClickRequest, ComputerError, ComputerRuntime, DragRequest, InputStrategy,
    KeyRequest, MouseButton, MoveRequest, ScrollRequest, TypeRequest,
};

use crate::args::{as_object, optional_string, optional_u64, required_string};

const MAX_IMAGE_BYTES: usize = 2_500_000;

pub struct ComputerTool {
    runtime: Arc<ComputerRuntime>,
    /// Whether the active model advertises image input. When false, screenshots
    /// are saved to disk and the model receives the path plus a warning.
    vision: bool,
}

impl ComputerTool {
    pub fn new(runtime: Arc<ComputerRuntime>, vision: bool) -> Self {
        Self { runtime, vision }
    }
}

#[async_trait]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        "computer"
    }

    fn description(&self) -> &str {
        "Control the user's desktop: read screen/window screenshots and simulate mouse/keyboard. \
         ALWAYS take a screenshot first; every click/drag/scroll/move coordinate is a pixel of \
         that returned image and should pass its frame_id. Re-screenshot after actions to verify \
         the result. Default workflow for independent work: lock_desktop, list_windows, \
         move_window_to_desktop, screenshot(target=window), act, screenshot again. If the user \
         explicitly asks you to operate an app already on their current screen, do NOT call \
         lock_desktop or move that window; if a desktop is already locked, call end_session \
         first and continue on the current desktop. Never operate EffiBuddy's own windows. One action per \
         call. If an action fails, read the error code and re-observe instead of guessing. \
         Requires a vision-capable model for screenshots."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "status",
                        "list_displays",
                        "list_windows",
                        "screenshot",
                        "click",
                        "drag",
                        "scroll",
                        "move",
                        "type",
                        "key",
                        "activate_window",
                        "list_desktops",
                        "lock_desktop",
                        "move_window_to_desktop",
                        "restore_window",
                        "switch_desktop",
                        "end_session"
                    ]
                },
                "target": {
                    "type": "string",
                    "enum": ["screen", "window"],
                    "description": "screenshot target; defaults to screen"
                },
                "window_id": {"type": "string"},
                "display": {"type": "string"},
                "frame_id": {
                    "type": "string",
                    "description": "frame_id returned by screenshot; actions use this raster's pixels"
                },
                "x": {"type": "number"},
                "y": {"type": "number"},
                "to_x": {"type": "number"},
                "to_y": {"type": "number"},
                "duration_ms": {"type": "integer", "minimum": 50, "maximum": 5000},
                "button": {"type": "string", "enum": ["left", "right", "middle"]},
                "click_count": {"type": "integer", "minimum": 1, "maximum": 3},
                "modifiers": {
                    "type": "string",
                    "description": "e.g. ctrl+shift; held for the action"
                },
                "strategy": {
                    "type": "string",
                    "enum": ["auto", "background", "foreground"],
                    "description": "auto uses background for parked windows; foreground restores/activates"
                },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"]
                },
                "amount": {"type": "integer", "minimum": 1, "maximum": 50},
                "text": {"type": "string"},
                "key": {"type": "string", "description": "e.g. Return, ctrl+s, super+c"},
                "repeat": {"type": "integer", "minimum": 1, "maximum": 20},
                "hold_ms": {"type": "integer", "minimum": 0, "maximum": 5000},
                "max_dim": {"type": "integer", "minimum": 256, "maximum": 2048},
                "desktop_id": {"type": "string"},
                "name": {"type": "string"}
            },
            "required": ["action"]
        })
    }

    fn approval(&self) -> ApprovalPolicy {
        // The app's ComputerApprovalGate turns this into either a real consent
        // card (Session mode) or an automatic approval (Always mode). Keeping
        // the policy at Ask means FullAccess cannot silently skip the consent
        // gate.
        ApprovalPolicy::Ask
    }

    fn summarize(&self, arguments: &Value) -> String {
        let action = arguments
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("action");
        let window = arguments
            .get("window_id")
            .and_then(Value::as_str)
            .map(|id| format!("窗口 {id}"))
            .unwrap_or_else(|| "屏幕".to_string());
        match action {
            "screenshot" => {
                let target = arguments
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or("screen");
                if target == "window" {
                    format!("使用电脑：读取{window}画面")
                } else {
                    "使用电脑：读取全屏画面".to_string()
                }
            }
            "click" => format!(
                "使用电脑：在{window}点击 ({}, {})",
                arguments.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                arguments.get("y").and_then(Value::as_f64).unwrap_or(0.0)
            ),
            "drag" => "使用电脑：拖动鼠标".to_string(),
            "scroll" => "使用电脑：滚动页面".to_string(),
            "move" => "使用电脑：移动鼠标".to_string(),
            "type" => format!(
                "使用电脑：输入 {} 个字符",
                arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| text.chars().count())
                    .unwrap_or(0)
            ),
            "key" => format!(
                "使用电脑：按键 {}",
                arguments
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
            "lock_desktop" => "使用电脑：锁定隔离工作区".to_string(),
            "move_window_to_desktop" => "使用电脑：把窗口移入隔离工作区".to_string(),
            "end_session" => "使用电脑：结束桌面访问".to_string(),
            "list_windows" => "使用电脑：读取窗口列表".to_string(),
            "status" => "使用电脑：读取电脑操作能力".to_string(),
            other => format!("使用电脑：{other}"),
        }
    }

    async fn execute(&self, ctx: ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
        let action = required_string(&arguments, "action")?;
        let session = ctx.session_id.as_str();
        let cancel = &ctx.cancel;

        if ctx.cancelled() {
            return Err(ToolError::new("computer action cancelled by the user"));
        }

        match action.as_str() {
            "status" => {
                let status = self.runtime.status(session).await.map_err(tool_error)?;
                let mut value = status;
                value["vision_model"] = json!(self.vision);
                value["frame_limit"] = json!(effibuddy_computer::frame::MAX_FRAMES);
                value["note"] = json!(if self.vision {
                    "Screenshots are attached to the tool result as images."
                } else {
                    "当前模型未开启视觉能力；截图会保存到磁盘并在结果中返回路径。请在设置→模型中启用视觉模型。"
                });
                Ok(ToolOutput::text(compact(value)))
            }
            "list_displays" => {
                let displays = self
                    .runtime
                    .list_displays(session, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({ "displays": displays }))))
            }
            "list_windows" => {
                let windows = self
                    .runtime
                    .list_windows(session, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "windows": windows,
                    "hint": "Use screenshot(target=window, window_id=...) after choosing a window."
                }))))
            }
            "list_desktops" => {
                let desktops = self
                    .runtime
                    .list_desktops(session, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({ "desktops": desktops }))))
            }
            "screenshot" => {
                let target_name =
                    optional_string(&arguments, "target")?.unwrap_or_else(|| "screen".to_string());
                let window_id = optional_string(&arguments, "window_id")?;
                let display = optional_string(&arguments, "display")?;
                let max_dim = optional_u64(&arguments, "max_dim")?.map(|value| value as u32);
                let target = if target_name == "window" {
                    let window_id = window_id.ok_or_else(|| {
                        ToolError::new("`window_id` is required when target is window")
                    })?;
                    CaptureTarget::Window { window_id }
                } else {
                    CaptureTarget::Screen { display }
                };
                let capture = self
                    .runtime
                    .screenshot(session, target, max_dim, cancel)
                    .await
                    .map_err(tool_error)?;
                let mut value = json!({
                    "action": "screenshot",
                    "frame": &capture.frame,
                    "image_attached": false,
                    "hint": "Coordinates for click/drag/scroll/move refer to this image; pass frame_id."
                });
                if self.vision && capture.png.len() <= MAX_IMAGE_BYTES {
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&capture.png);
                    value["image_attached"] = json!(true);
                    Ok(
                        ToolOutput::text(compact(value)).with_image(ImageAttachment::png(
                            encoded,
                            capture.frame.image_width,
                            capture.frame.image_height,
                        )),
                    )
                } else {
                    let path = self.runtime.save_screenshot(session, &capture);
                    value["image_path"] = json!(path.display().to_string());
                    value["image_attached"] = json!(false);
                    value["note"] = json!(if self.vision {
                        "截图超过大小上限，已保存到文件；本模型看不到这张图。可提高 max_dim 降低分辨率或改用窗口截图。"
                    } else {
                        "当前模型未开启视觉能力，已把截图保存到文件；请在设置→模型中启用支持视觉的模型。"
                    });
                    Ok(ToolOutput::text(compact(value)))
                }
            }
            "click" => {
                let x = coordinate(&arguments, "x")?;
                let y = coordinate(&arguments, "y")?;
                let request = ClickRequest {
                    frame_id: optional_string(&arguments, "frame_id")?,
                    x,
                    y,
                    button: parse_button(&arguments)?,
                    click_count: optional_u64(&arguments, "click_count")?
                        .unwrap_or(1)
                        .clamp(1, 3) as u32,
                    modifiers: parse_modifiers_arg(&arguments)?,
                    window_id: optional_string(&arguments, "window_id")?,
                    strategy: parse_strategy(&arguments)?,
                };
                self.runtime
                    .click(session, request, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "action": "click",
                    "ok": true,
                    "hint": "Take a screenshot again to verify the effect."
                }))))
            }
            "drag" => {
                let request = DragRequest {
                    frame_id: optional_string(&arguments, "frame_id")?,
                    from_x: coordinate(&arguments, "x")?,
                    from_y: coordinate(&arguments, "y")?,
                    to_x: coordinate(&arguments, "to_x")?,
                    to_y: coordinate(&arguments, "to_y")?,
                    duration_ms: optional_u64(&arguments, "duration_ms")?.unwrap_or(400),
                    button: parse_button(&arguments)?,
                    modifiers: parse_modifiers_arg(&arguments)?,
                    window_id: optional_string(&arguments, "window_id")?,
                    strategy: parse_strategy(&arguments)?,
                };
                self.runtime
                    .drag(session, request, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "action": "drag",
                    "ok": true,
                    "hint": "Take a screenshot again to verify the drop landed."
                }))))
            }
            "scroll" => {
                let direction =
                    optional_string(&arguments, "direction")?.unwrap_or_else(|| "down".to_string());
                let amount = optional_u64(&arguments, "amount")?
                    .unwrap_or(3)
                    .clamp(1, 50) as i32;
                let (dx, dy) = match direction.as_str() {
                    "up" => (0, -amount),
                    "down" => (0, amount),
                    "left" => (-amount, 0),
                    "right" => (amount, 0),
                    other => {
                        return Err(ToolError::new(format!(
                            "invalid scroll direction `{other}`"
                        )));
                    }
                };
                let request = ScrollRequest {
                    frame_id: optional_string(&arguments, "frame_id")?,
                    x: coordinate(&arguments, "x")?,
                    y: coordinate(&arguments, "y")?,
                    dx,
                    dy,
                    window_id: optional_string(&arguments, "window_id")?,
                    strategy: parse_strategy(&arguments)?,
                };
                self.runtime
                    .scroll(session, request, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "action": "scroll",
                    "direction": direction,
                    "amount": amount,
                    "ok": true
                }))))
            }
            "move" => {
                let request = MoveRequest {
                    frame_id: optional_string(&arguments, "frame_id")?,
                    x: coordinate(&arguments, "x")?,
                    y: coordinate(&arguments, "y")?,
                    window_id: optional_string(&arguments, "window_id")?,
                };
                self.runtime
                    .pointer_move(session, request, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(
                    json!({ "action": "move", "ok": true }),
                )))
            }
            "type" => {
                let text = required_string(&arguments, "text")?;
                let request = TypeRequest {
                    text,
                    window_id: optional_string(&arguments, "window_id")?,
                    strategy: parse_strategy(&arguments)?,
                };
                self.runtime
                    .type_text(session, request, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "action": "type",
                    "ok": true,
                    "hint": "Take a screenshot again to verify the text landed."
                }))))
            }
            "key" => {
                let key = required_string(&arguments, "key")?;
                let chord = effibuddy_computer::parse_chord(&key)
                    .map_err(|error| ToolError::new(format!("invalid key chord: {error}")))?;
                let request = KeyRequest {
                    chord,
                    repeat: optional_u64(&arguments, "repeat")?
                        .unwrap_or(1)
                        .clamp(1, 20) as u32,
                    hold_ms: optional_u64(&arguments, "hold_ms")?.unwrap_or(0),
                    window_id: optional_string(&arguments, "window_id")?,
                    strategy: parse_strategy(&arguments)?,
                };
                self.runtime
                    .press_key(session, request, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(
                    json!({ "action": "key", "ok": true }),
                )))
            }
            "activate_window" => {
                let window_id = required_string(&arguments, "window_id")?;
                self.runtime
                    .activate_window(session, &window_id, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "action": "activate_window",
                    "window_id": window_id,
                    "ok": true
                }))))
            }
            "lock_desktop" => {
                let name =
                    optional_string(&arguments, "name")?.unwrap_or_else(|| "EffiBuddy".to_string());
                let desktop = self
                    .runtime
                    .lock_desktop(session, &name, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "action": "lock_desktop",
                    "desktop": desktop,
                    "hint": "Move the target window into this desktop, then screenshot that window."
                }))))
            }
            "move_window_to_desktop" => {
                let window_id = required_string(&arguments, "window_id")?;
                let desktop_id = required_string(&arguments, "desktop_id")?;
                self.runtime
                    .move_window_to_desktop(session, &window_id, &desktop_id, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "action": "move_window_to_desktop",
                    "window_id": window_id,
                    "desktop_id": desktop_id,
                    "ok": true
                }))))
            }
            "restore_window" => {
                let window_id = required_string(&arguments, "window_id")?;
                self.runtime
                    .restore_window(session, &window_id, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "action": "restore_window",
                    "window_id": window_id,
                    "ok": true
                }))))
            }
            "switch_desktop" => {
                let desktop_id = required_string(&arguments, "desktop_id")?;
                self.runtime
                    .switch_desktop(session, &desktop_id, cancel)
                    .await
                    .map_err(tool_error)?;
                Ok(ToolOutput::text(compact(json!({
                    "action": "switch_desktop",
                    "desktop_id": desktop_id,
                    "ok": true
                }))))
            }
            "end_session" => {
                self.runtime.end_session(session).await;
                Ok(ToolOutput::text(compact(json!({
                    "action": "end_session",
                    "ok": true
                }))))
            }
            other => Err(ToolError::new(format!(
                "unknown computer action `{other}`; use status to inspect capabilities"
            ))),
        }
    }
}

fn compact(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn tool_error(error: ComputerError) -> ToolError {
    ToolError::new(format!("[{}] {}", error.code(), error))
}

fn coordinate(arguments: &Value, key: &str) -> Result<f64, ToolError> {
    let object = as_object(arguments)?;
    let value = object
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| ToolError::new(format!("missing numeric `{key}`")))?;
    if !value.is_finite() {
        return Err(ToolError::new(format!("`{key}` must be a finite number")));
    }
    Ok(value)
}

fn parse_button(arguments: &Value) -> Result<MouseButton, ToolError> {
    match optional_string(arguments, "button")?.as_deref() {
        None | Some("left") => Ok(MouseButton::Left),
        Some("right") => Ok(MouseButton::Right),
        Some("middle") => Ok(MouseButton::Middle),
        Some(other) => Err(ToolError::new(format!("invalid mouse button `{other}`"))),
    }
}

fn parse_modifiers_arg(arguments: &Value) -> Result<Vec<effibuddy_computer::Modifier>, ToolError> {
    let raw = optional_string(arguments, "modifiers")?.unwrap_or_default();
    effibuddy_computer::parse_modifiers(&raw)
        .map_err(|error| ToolError::new(format!("invalid modifiers: {error}")))
}

fn parse_strategy(arguments: &Value) -> Result<InputStrategy, ToolError> {
    match optional_string(arguments, "strategy")?.as_deref() {
        None | Some("auto") => Ok(InputStrategy::Auto),
        Some("background") => Ok(InputStrategy::Background),
        Some("foreground") => Ok(InputStrategy::Foreground),
        Some(other) => Err(ToolError::new(format!("invalid strategy `{other}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use effibuddy_agent_core::{Tool, ToolContext};
    use effibuddy_computer::testing::FakeBackend;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn tool() -> (ComputerTool, Arc<FakeBackend>) {
        let backend = Arc::new(FakeBackend::new());
        let runtime = Arc::new(ComputerRuntime::new(backend.clone()));
        runtime.grant("s");
        (ComputerTool::new(runtime, true), backend)
    }

    fn context() -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
            session_id: "s".to_string(),
            agent_id: "main".to_string(),
            agent_name: None,
            agent_depth: 0,
            call_id: "call".to_string(),
            cancel: CancellationToken::new(),
            nested: None,
            subagents: None,
            checkpoint_sink: None,
        }
    }

    #[tokio::test]
    async fn screenshot_returns_an_image_and_frame() {
        let (tool, _) = tool();
        let output = tool
            .execute(context(), json!({"action": "screenshot"}))
            .await
            .unwrap();
        assert_eq!(output.images.len(), 1);
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["image_attached"], true);
        assert!(value["frame"]["id"].as_str().unwrap().starts_with('f'));
    }

    #[tokio::test]
    async fn non_vision_screenshot_is_saved_to_disk() {
        let backend = Arc::new(FakeBackend::new());
        let runtime = Arc::new(ComputerRuntime::new(backend));
        runtime.grant("s");
        let tool = ComputerTool::new(runtime, false);
        let output = tool
            .execute(context(), json!({"action": "screenshot"}))
            .await
            .unwrap();
        assert!(output.images.is_empty());
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["image_attached"], false);
        assert!(value["image_path"].as_str().unwrap().ends_with(".png"));
        let path = value["image_path"].as_str().unwrap();
        assert!(std::path::Path::new(path).exists());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn click_maps_through_the_frame() {
        let (tool, backend) = tool();
        let shot = tool
            .execute(context(), json!({"action": "screenshot"}))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&shot.content).unwrap();
        let frame_id = value["frame"]["id"].as_str().unwrap().to_string();
        tool.execute(
            context(),
            json!({"action": "click", "frame_id": frame_id, "x": 12, "y": 34}),
        )
        .await
        .unwrap();
        assert_eq!(backend.click_calls(), vec![(12.0, 34.0)]);
    }

    #[tokio::test]
    async fn type_and_key_are_forwarded() {
        let (tool, backend) = tool();
        tool.execute(context(), json!({"action": "type", "text": "hello"}))
            .await
            .unwrap();
        assert_eq!(backend.typed(), "hello");
        tool.execute(context(), json!({"action": "key", "key": "ctrl+s"}))
            .await
            .unwrap();
        assert_eq!(backend.keys(), vec!["s"]);
    }

    #[tokio::test]
    async fn stale_frame_is_reported_as_an_error() {
        let (tool, _) = tool();
        let error = tool
            .execute(
                context(),
                json!({"action": "click", "frame_id": "missing", "x": 1, "y": 1}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("STALE_FRAME"));
    }
}
