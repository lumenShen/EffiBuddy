# Computer Use（桌面读图与键鼠操作）

EffiBuddy 的主代理可以通过 `computer` 工具读取屏幕/窗口，模拟鼠标键盘，并在需要时把目标窗口放进独立的“工作桌面/后台停车位”，避免反复弹窗打扰用户。

> 这是高风险能力。默认档位是**会话内准许**：第一次使用时弹审批卡，允许后本会话内不再重复询问；悬浮条始终提供“接管操作”和“停止操作”。

## 1. 授权模型

设置 → 通用 → **电脑操作（键鼠模拟）**：

| 档位 | 行为 |
| --- | --- |
| 关闭 | 不注册 `computer` 工具；输入框不显示提示。 |
| 会话内准许（默认） | 首次调用弹真实审批卡；选“本会话允许”后，本会话内自动放行。切换会话后重新询问。 |
| 默认准许 | 新会话直接放行；仍保留悬浮条的接管/停止。 |

补充规则：

- **只有主代理**注册 `computer`，子代理永远拿不到该工具。
- 权限模式为 **计划模式** 时，点击/输入/拖动/滚动/移动窗口等修改类动作直接拒绝；截图和列表类只读动作仍可询问。
- `computer` 的审批由 app 层 `ComputerApprovalGate` 强制拦截，**完全访问模式也不能绕过首次授权**。
- 用户点“停止操作”会撤销本会话授权，下次使用需要重新批准。
- 应用重启后不保留授权。

输入框底部的状态 chip：

- `电脑操作 · 首次使用询问`
- `电脑操作已授权 · 本会话`
- `电脑操作已允许 · 键鼠模拟`
- 当前模型未开启视觉能力时使用警告色，提示截图无法发给模型。

## 2. 悬浮控制条

电脑会话第一次真正读取屏幕/操作时，显示一个无标题栏悬浮条：

```
◌ EffiBuddy 正在帮您使用电脑        [接管操作] [停止操作]
  正在读取窗口「记事本」…
```

- **接管操作**：暂停 AI 的下一次输入，文案变为“你已接管操作…”，按钮变为 **我帮你点好了**。
- **我帮你点好了**：解除暂停，AI 继续执行。
- **停止操作**：置 stopped、撤销授权、恢复停车窗口，并立即中断当前 agent run。
- 全屏截图前悬浮条会隐藏约 180ms，避免它出现在给模型的画面里；窗口级截图不隐藏。
- X11 / Windows 使用 `PopUp` + `ABOVE/TOPMOST`；Wayland 没有 layer-shell，位置与置顶由合成器决定（GNOME 通常会居中），这是已知限制。

## 3. 工具参数

单一工具 `computer`，`action` 枚举：

| action | 说明 |
| --- | --- |
| `status` | 平台、后端、能力矩阵、本会话授权状态、视觉模型状态。 |
| `list_displays` | 显示器列表。 |
| `list_windows` | 窗口 id、标题、进程、客户区/外框、活动窗口、所在桌面。 |
| `screenshot` | `target=screen|window`，可带 `window_id`、`display`、`max_dim`。返回 PNG + `frame_id`。 |
| `click` | 截图图像像素坐标 `(x, y)`，支持 `button`、`click_count`、`modifiers`、`strategy`。 |
| `drag` | `(x,y) → (to_x,to_y)`，支持 `duration_ms`、`modifiers`。 |
| `scroll` | `direction=up|down|left|right`、`amount` 滚动刻度。 |
| `move` | 只移动指针。 |
| `type` | 输入文本（Unicode）。 |
| `key` | 组合键，如 `Return`、`ctrl+s`、`super+c`、`Page_Down`。 |
| `activate_window` | 激活/恢复目标窗口。 |
| `list_desktops` | 当前桌面与隔离桌面/停车位。 |
| `lock_desktop` | 锁定/创建隔离工作区（X11 workspace 或 Windows parking lane）。 |
| `move_window_to_desktop` | 把窗口移入隔离桌面。 |
| `restore_window` | 恢复单个窗口。 |
| `switch_desktop` | 切换桌面。 |
| `end_session` | 结束电脑会话、恢复窗口、隐藏悬浮条。 |

### 坐标与 frame_id

- 所有点击/拖动/滚动/移动坐标都是**最近一次返回的截图图像像素**，不是屏幕物理像素。
- `screenshot` 返回 `frame`：`id`、图像尺寸、原生尺寸、原点、缩放比例。
- 动作带 `frame_id` 时只接受该帧；不带时使用最新帧。
- 窗口在截图后移动/缩放时，运行时返回 `STALE_FRAME`，要求重新截图，而不是把点击发到错误位置。
- 坐标越界返回 `OUT_OF_BOUNDS`。

### 输入策略

| strategy | 行为 |
| --- | --- |
| `auto`（默认） | 停车窗口用后台消息；普通窗口用前台输入。 |
| `background` | 只用 `WM_*` 消息（Windows 停车窗口）；其他平台报错。 |
| `foreground` | 必要时恢复窗口并激活，然后用全局输入；会短暂显示窗口。 |

## 4. 平台能力矩阵

| 能力 | Linux X11 | Linux Wayland | Windows |
| --- | --- | --- | --- |
| 全屏截图 | ✅ XGetImage | ✅ Screenshot portal | ✅ BitBlt 虚拟屏幕 |
| 窗口列表 | ✅ EWMH | ❌ | ✅ EnumWindows |
| 窗口级截图 | ✅ XComposite/窗口 drawable | ❌ | ✅ PrintWindow(PW_CLIENTONLY) |
| 鼠标/键盘 | ✅ XTest | ✅ RemoteDesktop portal | ✅ SendInput / PostMessage |
| 隔离工作区 | ✅ EWMH workspace | ❌ | ✅ 后台停车位 |
| 悬浮条置顶 | ✅ PopUp + ABOVE | ❌ 由合成器决定 | ✅ HWND_TOPMOST |

### Linux X11

- 窗口枚举：`_NET_CLIENT_LIST`、`_NET_WM_NAME`、`_NET_WM_PID`、`_NET_WM_DESKTOP`。
- 截图：root `GetImage`；窗口优先 `XCompositeNameWindowPixmap`，失败回退窗口 drawable。
- 输入：XTest；Unicode 文本通过临时重映射空闲 keycode 实现。
- 隔离：找/建名为 `EffiBuddy` 的 EWMH workspace，移动窗口并切换过去；会话结束切回原桌面。
- 限制：切换工作区会改变用户当前看到的桌面。

### Linux Wayland

- 像素：`org.freedesktop.portal.Screenshot`（非交互）。截图不依赖输入授权，即使远程控制被拒绝也能读图。
- 输入：`RemoteDesktop` + `ScreenCast` 同一 session；拿到 monitor stream 后用绝对坐标注入键鼠。首次使用会弹系统“屏幕共享/远程控制”授权对话框。
- 限制：
  - 不允许枚举窗口或捕获任意窗口，只支持全屏。
  - 不允许程序化切换工作区/移动任意窗口。
  - 悬浮条位置与置顶不可控。
  - 截图可能是物理像素（如 2560×1600），输入使用合成器逻辑坐标（如 1920×1200）；后端会按最近截图尺寸自动缩放。

### Windows

- 窗口枚举：`EnumWindows`，过滤隐藏/工具窗口和 EffiBuddy 自身进程。
- 截图：`PrintWindow(PW_CLIENTONLY | PW_RENDERFULLCONTENT)`，失败回退 `BitBlt`；全屏用虚拟屏幕 DC。
- 前台输入：`SendInput`（鼠标、滚轮、Unicode、虚拟键）；必要时 `AttachThreadInput` + `SetForegroundWindow`。
- 隔离：**后台停车位**——保存原 bounds，`SetWindowPos` 移到虚拟屏幕外；截图仍用 PrintWindow，点击/拖动/文本通过 `PostMessage(WM_*)` 发给窗口/焦点子窗口。不切换用户的虚拟桌面、不弹窗。
- 兼容性：Electron/游戏等忽略 `WM_*` 的应用可能只能改 `strategy=foreground`，此时窗口会临时恢复并置前。
- 会话结束/`end_session` 会把停车窗口恢复到原位置。

## 5. 默认工作流

模型会按工具描述执行：

1. `status` 了解平台能力。
2. 需要独立工作时 `lock_desktop`。
3. `list_windows` 找到目标窗口。
4. `move_window_to_desktop` 把窗口移入隔离桌面（Windows 为停车位）。
5. `screenshot(target=window, window_id=...)` 获取 `frame_id`。
6. `click` / `type` / `key` / `drag` / `scroll`，每次动作后重新截图确认。
7. `end_session` 结束，恢复窗口与桌面。

如果用户明确要求操作**当前屏幕上的应用**，模型应跳过 `lock_desktop`/`move_window_to_desktop`，直接对当前窗口截图和操作；如果本会话已经锁定了隔离桌面，先 `end_session`（保留授权）再继续在当前桌面工作。

## 6. 视觉模型

截图通过 `ToolResult.images` 传给模型：

- OpenAI Chat Completions：user 消息 `image_url` data URL。
- Anthropic Messages：user content `image`（base64）。
- OpenAI Responses：`input_image`。
- 工具结果先连续输出所有 tool 消息，再统一追加一条图片 user 消息，避免 OpenAI 在多个 tool 结果之间插入 user 消息。

当前默认 DeepSeek 模型没有视觉能力。`screenshot` 会把 PNG 保存到 `<数据目录>/computer/<session>/` 并在结果中返回路径；请在设置 → 模型中启用支持视觉的模型。

单张图片上限约 2.5MB；默认把最长边缩到 1280（`max_dim` 可调 256–2048）。

## 7. 开发与排查

```bash
# 不经过模型，直接检查平台能力
cargo run -p effibuddy-app -- --computer-probe status

# 显示器（Wayland 首次会初始化 RemoteDesktop/ScreenCast 会话）
cargo run -p effibuddy-app -- --computer-probe displays

# 窗口列表（X11/Windows）
cargo run -p effibuddy-app -- --computer-probe windows

# 全屏截图（Wayland 首次会弹 portal 授权）
cargo run -p effibuddy-app -- --computer-probe screenshot --out /tmp/effi.png

# 悬浮条 UI 预览（FakeBackend，不实际控制）
cargo run -p effibuddy-app -- --preview-computer
```

强制选择 Linux 后端：

```bash
EFFIBUDDY_COMPUTER_BACKEND=x11 cargo run -p effibuddy-app
EFFIBUDDY_COMPUTER_BACKEND=wayland cargo run -p effibuddy-app
```

## 8. 安全边界

- 不操作 EffiBuddy 自身进程的窗口。
- 坐标必须来自当前截图；stale frame 失败关闭。
- 用户随时可接管/停止；停止会中断当前 run。
- 授权按会话保存，不跨会话、不跨重启。
- 不会静默绕过审批：FullAccess 仍要过首次电脑授权；Plan 模式阻止修改类动作。
- 所有动作、错误码会进入工具结果与 transcript，便于审计。

## 9. 已知限制

- v1 没有辅助功能树（UIA/AT-SPI），复杂自绘/Canvas 界面依赖视觉坐标；后续可在 `ComputerBackend` 上增加语义层。
- Wayland 只支持全屏；窗口级读图需要 PipeWire ScreenCast 消费窗口帧，未在本版实现。
- Windows 后台停车位对部分应用无效，需要 `strategy=foreground`。
- 悬浮条在 Wayland 无法保证顶部居中或置顶。
- 图片会随会话 JSON 持久化，长会话可能变大；单张已限幅。
