# 手机端构建（A + B 双轨）实施方案

本文档把「手机视图」拆成两条并行但共享一套模型的路线：

| 轨道 | 目标 | 现状 |
| --- | --- | --- |
| **A · 桌面内手机视图** | 桌面窗口里可切换的手机布局，共用会话/工具/权限，能截图能演示 | 已实现并可运行（见下文清单） |
| **B · 真·手机端构建** | 在 Android 上安装运行，带系统能力（通知、分享、键盘、相册、语音） | 本文档 + `effibuddy-mobile` 共享层 + 构建脚本骨架 |

两条轨道共用 `crates/mobile`（`effibuddy-mobile`）：布局常量、导航状态机、筛选/搜索谓词。**渲染器是唯一的差异。**

---

## 1. 共享层切分

```text
crates/mobile          # 无渲染器依赖，纯状态 + 数字
  layout.rs            # 16px 边距 / 420 列宽 / 20 圆角气泡 / 44px 触控 …
  nav.rs               # Screen / Sheet / Filter / 抽屉与 sheet 互斥 / 手势阈值 …

crates/ui              # 设计系统（GPUI）
  theme.rs             # Theme（含 mobile_light / mobile_dark）
  mobile 模块          # 直接 re-export effibuddy_mobile::layout

crates/app/src/mobile  # A 轨渲染器（GPUI）
  mod.rs               # 手机列、顶栏、抽屉、底部 sheet
  chat.rs              # 气泡 / 过程折叠 / 工具行 / 审批卡
  composer.rs          # 胶囊输入栏 / 排队条
  pages.rs             # 设置页栈
```

后续把 `crates/app/src/mobile` 的状态字段换成 `effibuddy_mobile::Nav` 即可让 A 轨也走同一状态机（当前 A 轨保留了 `drawer_p` 这类渲染期动画字段，属于渲染器私有）。

---

## 2. B 轨技术选型（需要拍板）

引擎侧（`agent-core` / `agent-tools` / `agent-js`）是纯 Rust，`aarch64-linux-android` 可编译。**UI 侧没有现成答案**，三条路线：

| 方案 | 优点 | 代价 | 判断 |
| --- | --- | --- | --- |
| B1 · GPUI 加 Android 后端 | 一套 UI 代码 | 上游 gpui 无 Android 后端；要自己接 `android-activity` + wgpu surface + IME + 安全区；属于框架级工作 | 不建议先做 |
| B2 · 可移植渲染器（egui / Slint） | 官方支持 Android；Rust 内一套 view model 可复用 | 需要把聊天模型从 `app::chat`（GPUI 耦合）抽出为 `chat-model` crate；UI 重写一遍 chrome | **推荐**，增量可控 |
| B3 · WebView + 本地 HTTP | 最快出可用包 | 与「原生流畅」目标相悖，且等于把 dsh-go 的路线换成网页壳 | 作为兜底 |

推荐 **B2**：先抽 `chat-model`（消息/工具/审批/流式快照，无 GPUI），A 轨的 GPUI 渲染器改成消费它，B 轨用 egui/Slint 画同一份数据。

### 需要从 `app::chat` 抽出的最小模型

- `ChatItem` / `TurnItem` / `AssistantItem` 的公开投影（现在 `pub(crate)`）；
- 工具结果表 `ToolResults`、流式快照 `StreamSnapshot`；
- 审批/排队/待注入列表；
- 会话列表行（标题、分组、运行/待确认/未读状态）。

抽取原则：模型只保留**渲染需要的字段**，动画、滚动句柄、图片缓存留在各渲染器。

---

## 3. B 轨系统能力矩阵

| 能力 | A 轨（桌面手机视图） | B 轨（Android） |
| --- | --- | --- |
| 触控/手势 | 鼠标模拟 | 原生触摸事件 |
| 软键盘避让 | 无（桌面临时做法：点击对话区失焦） | `WindowInsets` 动画上推 |
| 安全区（刘海/手势条） | 无 | `displayCutout` + `gestureInsets` |
| 通知与角标 | 无（仅应用内状态） | 前台服务 + NotificationChannel |
| 分享/导出 | 复用桌面导出 | ShareTarget + SAF |
| 相册/相机 | 文件选择器 | PhotoPicker |
| 语音输入 | 无 | 系统 ASR / 云端 ASR |
| 后台保活 | 不适用 | 前台服务 + 电池优化白名单 |
| 生物识别锁 | 无 | Keystore + BiometricPrompt |

A 轨不追这些；B 轨的每一项都是一个独立任务，不应混进 UI 复刻里。

---

## 4. 构建步骤（Android，待工具链就绪）

前置：JDK 17、Android SDK 35、NDK r27、`cargo-ndk`、`rustup target add aarch64-linux-android`。

```bash
# 1) 预检（不安装任何东西，只报告缺什么）
bash scripts/build-android.sh

# 2) 装工具链后编译 core
cargo ndk -t arm64-v8a -t x86_64 -o app/android/jniLibs build --release -p effibuddy-mobile -p effibuddy-agent-core

# 3) Gradle 打包
(cd app/android && ./gradlew assembleDebug)

# 4) 安装与截图验证
adb install -r app/android/build/outputs/apk/debug/app-debug.apk
adb shell am start -n com.effibuddy.mobile/.MainActivity
adb exec-out screencap -p > /tmp/effibuddy-mobile.png
```

首包目标：**只读会话 + 发送消息 + 审批按钮**，不带工具执行之外的系统能力，先把「一条对话能跑完」验证掉。

### 已知阻塞

1. `crates/computer-use` 依赖 X11/Wayland，Android 上无法编译 → 用 `cfg` 或 feature 排除；
2. `crates/browser-use` 依赖外部 Chromium/DevTools，Android 上无系统 Chromium 可连 → 首包不注册 `browser` 工具；
3. `vendor/gpui` 无 Android 后端 → 见 §2 选型；
4. `agent-tools::shell` 需要把 bash 打进包里，或改用 Android `sh`；
5. 数据目录需从 `dirs::data_local_dir()` 换成应用私有目录（`EFFIBUDDY_DATA_DIR` 已可重定向，Android 侧注入即可）。

---

## 5. A 轨已实现清单

- 手机列（≤ 420px，窄窗铺满）+ 中性灰白黑（与桌面同一套 ink 基色，强调色只出现在主按钮/发送/选中/链接）；
- 顶栏：☰、标题+●本机 玻璃胶囊、模型胶囊、新建；
- 抽屉：新对话、搜索、全部/运行中/待确认/未读、今天/昨天/本周/更早 分组、行内 ⋯ 操作、主机卡、主题/快捷操作/设置；
- 对话流：用户气泡、`已完成 · N 次工具调用` 折叠、思考行、工具行（含失败）、Markdown、复制+时长；
- **工具产物**：`write_file`/`edit_file` 渲染成可点文件卡（名称/目录/打开），`screenshot` 等图片以列宽显示；
- **消息操作**：用户气泡与助手元信息行的 `⋯` → 复制 / 引用到输入框 / 编辑并重发；
- **滚动**：离底部 120px 即停止自动跟随，改为「回到底部」胶囊，流式时不再抢滚动；
- **子代理可见**：运行中时输入栏上方显示「N 个子代理运行中」，点开名单 sheet（含停止）；
- **归档**：抽屉筛选行「归档 N」→ 归档会话 sheet（恢复）；
- 输入栏：胶囊 + 加号 + 发送/停止、排队/待注入条（立即发送、撤回）、权限胶囊、模型胶囊、todo dock；
- 审批卡：`允许本次 / 本轮都允许 / 拒绝` 全宽拇指按钮；
- 底部 sheet：模型、会话操作、快捷操作（权限/明暗/回桌面）；
- 设置页栈：设置 / 外观（明暗、6 配色、布局）/ 通用 / 模型 / 关于；
- 手势：左缘右滑开抽屉、抽屉左滑关闭、点对话区收键盘；
- 输入语义：手机上 Enter 换行、圆键发送（桌面临 Enter 发送）；
- 切换：设置开关、`Ctrl+Shift+M`、`--mobile`、`--mobile-fullscreen`、`--mobile-drawer`、`--mobile-settings[=page]`；
- 持久化：`config.json` 的 `ui_layout` 与 `mobile_accent`；
- **安全区抽象**：`effibuddy_mobile::Insets`（桌面传 0，B 轨接 WindowInsets/IME），`EFFIBUDDY_MOBILE_INSETS=top,bottom` 可预览。

### 已定的设计决策

1. **配色先对齐桌面**：去掉彩色渐变/光晕，页面用桌面同一套灰白；「雾蓝/黛青/暮紫/松石/琥珀/石墨」只改强调色，默认石墨（近黑，和桌面主按钮一致）。
2. **玻璃用「高不透明 + 发丝边 + 阴影」近似**：GPUI 无 backdrop blur，真磨砂会穿帮，所以玻璃填充 94%～98%。
3. **不引入第二套聊天模型**：手机行直接消费桌面的 `ChatViewState`，避免两份数据打架。

---

## 6. 下一步（按优先级）

| P | 事项 | 归属 |
| --- | --- | --- |
| P0 | 抽出 `chat-model`（无 GPUI 的聊天投影）——B 轨前置 | B |
| P0 | 长会话 + 流式压测（`--bench-scroll`）在手机列复跑 | A |
| P1 | 图片查看器（点开大图/缩放）、代码块长按复制 | A |
| P1 | 对话搜索页（跨会话全文）、分享/导出 | A |
| P1 | 步骤/轨迹页（把折叠的过程展开成独立页） | A |
| P2 | Android 首包（B2：egui/Slint 渲染器 + JNI 桥） | B |
| P2 | 真机 Insets 接入（cutout/gesture/IME） | B |
