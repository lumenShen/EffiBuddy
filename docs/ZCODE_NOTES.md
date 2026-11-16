# ZCode 学习笔记与移植清单

参考仓库：[zai-org/ZCode](https://github.com/zai-org/ZCode)（v3.18.0 源码）。它不是 Rust/GPUI 项目，但它的 agent 运行时、对话信息密度和子代理生命周期经过大量真实会话打磨。本文件记录读了什么、为什么这样移植，以及哪些部分刻意没有照搬。

## 1. 子代理：从“一个大 agent”到“可委派的工作单元”

ZCode 的核心模型：

- `Agent` 是模型可见的工具；`Task` 只是历史兼容别名。
- 子代理是同一进程里的第二个 `AgentRuntime`，有自己的 transcript、工具集、取消令牌和审批门。
- 两种执行方式：前台等待报告；后台立即返回 `agentId`，完成后通过 `<task-notification>` 唤醒父代理。
- 父代理只收到最终报告，拿不到完整子 transcript；UI 才有权查看全文。
- 子代理永远不能再派生子代理；前台取消必须能穿透到子任务。
- 子会话有独立 id，终态有 `completed / failed / cancelled / lost` 等状态；UI 投影只保留 running/success/failed/cancelled 四类。

EffiBuddy 的落地方式：

| ZCode 概念 | EffiBuddy 实现 |
| --- | --- |
| `Agent` 工具 | `agent-tools/src/agent.rs` 的 `AgentTool`，参数 `description / prompt / subagent_type / run_in_background` |
| Agent profiles | `agent-core/src/subagent.rs` 的 `SubagentProfile`，内置 `general-purpose / explore / reviewer` |
| 独立 child runtime | `app/src/subagent.rs` 的 `SubagentService`，每个子代理一个 `AgentHarness` + `SessionState` |
| hand-back 报告 | `[Subagent hand-back]` 框 + 2 空格缩进 + usage 尾部，明确“这是模型输出，不是用户指令” |
| 后台通知 | `<task-notification>` 作为 `Message::synthetic_user` 注入父 transcript，再 `run_pending()` 唤醒 |
| 子会话只读面板 | 聊天流中的代理卡片 + 右侧子代理面板 + 复用 `ChatList` 的只读子会话 |
| 审批转发 | 子 harness 有自己的 `BrokerApprovalGate`，事件带 `agent_id` 进入统一 UI 队列 |

刻意没做的：

- fork / worktree 隔离、远程子代理、teammates/swarms；
- 子 agent 再派生（永久拒绝 `agent` 工具）；
- 子代理 transcript 进入父上下文；
- 后台通知直接插入正在进行的 tool-call 中间（必须等父 turn 结束）。

## 2. 对话 UI：密度、层级与“活动感”

ZCode 最值得学的不是某个组件，而是**只保留必要边框，用排版层级承载信息**：

- 旧式“每个工具调用一张大卡片”会让长会话非常吵。ZCode 用单行摘要：图标 + 类别 + 主要参数 + 状态词 + 悬浮才出现的 chevron。
- 连续工具调用折叠成一行统计；展开后走垂直导轨，而不是铺满屏幕。
- thinking/reasoning 默认折叠，流式时用「Thinking…」/ 梯度文字提示，只显示一行最新预览。
- 用户消息是右对齐气泡；assistant 是无气泡全宽正文，行高更大（1.75）。
- 状态用三层文字色（foreground / subtle / subtlest）和极少量语义色，而不是到处加框。
- 页面右上角一个状态胶囊汇总优先信息（运行中的 workflow / 当前 todo / git 变更 / 上下文），点击展开面板。
- 输入框是视觉锚点：大圆角、细边框、左侧模式与权限、右侧模型与发送；上下文环常驻。
- 命令面板（Cmd/Ctrl+K）是“键盘优先”的入口，不只是搜索框。

EffiBuddy 已落地：

- 工具行改成紧凑摘要；连续 3 个以上工具调用折叠为「N 个工具调用 · 读取 · 搜索」，可展开；
- 思考行默认折叠，流式时显示「正在思考 · N 秒」+ 最新一行，结束后显示「思考了 N 秒」；
- 聊天页头部增加状态胶囊（运行中子代理 / 待批准 / 当前计划 / 上下文），点击定位右侧页签；
- 输入区增加上下文仪表、模型/权限 chip、`+` 打开命令面板；
- 侧栏按「今天 / 昨天 / 最近 7 天 / 更早」分组，活跃会话有左侧墨色指示条，删除按钮 hover 可见；
- 空状态使用时间问候 + 建议提示 + 能力 chip；
- 新增 `Ctrl K` 命令面板（命令 + 会话搜索，评分排序与「最近使用」置顶，↑↓ 选择、Enter 执行）；
- 子代理卡片、运行中计时、后台 badge、停止按钮与只读子会话。

刻意没做的：

- 把整棵 DOM 级虚拟化搬过来：EffiBuddy 继续按“消息项”虚拟化，子会话使用独立 `ListState`；
- Tailwind/Radix/shadcn 的类名和交互原样复制：只保留语义 token 与信息层级，交互用 GPUI 原生事件实现；
- Streamdown/Shiki/KaTeX/Mermaid：当前 Markdown 渲染器保持轻量，代码块不引入高亮依赖；
- 每帧重解析整个 transcript：仍按结构事件重建快照，delta 只推进 reveal 缓冲。

## 3. 前端架构对照

| ZCode（React/Electron） | EffiBuddy（Rust/GPUI） |
| --- | --- |
| 语义 CSS 变量 + Tailwind 4 | `Theme` 结构体 + `Hsla` token，组件显式接收 `&Theme` |
| `useSyncExternalStore` + 投影 store | `SharedSession` + `AppEvent` 单泵；结构事件才重建 `ChatList` |
| `@tanstack/react-virtual` 按 turn 虚拟化 | GPUI 变高 `List` / `ListState`，按消息项 splice |
| `use-stick-to-bottom` + 自定义锚点 | `ChatList::sync_scroll` 在布局前吸附，滚动句柄更新 `pinned` |
| Lexical contenteditable + IME | 自绘 `EntityInputHandler` 多行输入框 |
| framer-motion / CSS keyframes | `with_animation` + 显式 `on_next_frame` 驱动 reveal |
| Lucide icons | 内嵌 SVG 常量，按 `text_color` 着色 |

两者共同的约束是：**流式输出不能引起布局抖动**。ZCode 用 word-opacity reveal，EffiBuddy 用按字符推进 + 末尾光标；都在文本已占最终布局的前提下只改变可见性。

## 4. 模型配置与右侧页签

ZCode 的模型配置不是“一个 Base URL + 一个模型”，而是一份可编辑的 Provider/Model 注册表：预置供应商、自定义供应商、模型元数据、默认选择、连通性测试和从 API 拉取模型。EffiBuddy 的移植边界：

| ZCode | EffiBuddy |
| --- | --- |
| `provider_config.json`（providerConfigRules + modelConfigRules） | `providers.json`（`ProviderConfig` / `ModelConfig` / `default_selection`） |
| Access/API/Model Overlay、schemaVersion 迁移 | 带 `schema_version` 的 `ModelConfigFile`；旧 `config.json` 自动迁移 |
| Anthropic Messages / OpenAI Chat Completions / OpenAI Responses | 三个原生流式适配器 |
| 设置页左侧 Provider 导航 + 右侧详情 | 设置 → 模型：供应商列表 + 连接/模型详情 |
| Composer Provider/Model 菜单 | 输入框模型 chip → 供应商切换 + 模型列表 + 推理档位 |
| 每会话模型选择 | `SessionState.provider_id/model/reasoning_level`；子代理继承父选择 |
| 右侧 Side Pane：tab bar、overview、recent closed、resizable | `app/side_pane.rs`：概览（计划 + 用量详情）/子代理/子会话/文件/变更页签 + 总览搜索 + 最近关闭 + 宽度拖拽 |

刻意没做的：OAuth / Coding Plan 账号体系、Provider 模板市场、模型能力自动探测、跨进程 provider 注册表热更新。`providers.json` 是单进程本地文件，写入是原子的；损坏时保留原文件、回退内置配置并在设置页提示。

## 5. 后续值得继续移植的

1. 按 turn（而不是按消息）虚拟化，进一步降低超长会话的布局成本。
2. 子代理目录的 listing delta（首轮全量、之后只发增删）——目录变大后省 token。
3. 自定义子代理档案的编辑器（设置页目前只读展示，编辑仍需手动改文件）。
4. 会话级工作树/侧栏 rich hover card（分支、diff 统计、最后活动）。
5. Markdown 表格复制工具栏 hover 化与真实溢出检测（zcode 仅 hover 出现工具栏；当前常驻渲染，并用 640px 宽度阈值近似提示可滚动）。
6. 变更页 diff 的渲染缓存（zcode diff-viewer 有 memo；当前每次渲染全量重算 diff 解析、词级标注与折叠）。

## 6. 子代理目录与自定义档案（本次模块）

参考：ZCode 的 `.zcode/agents/*.md` 体系（`bootstrap/src/subagents.ts`、`packages/shared/src/subagent-profile-parser.ts`、`core/src/subagent/listing.ts`、`bootstrap/src/zcode-protocol/subagent-session-query.ts`）。

ZCode 的核心决策，以及 EffiBuddy 的移植方式：

| ZCode 决策 | EffiBuddy 落地 |
| --- | --- |
| 手写宽松 frontmatter 解析器（刻意不做完整 YAML），支持行内列表、dash 列表、引号、`#` 注释；非法取值静默归一化 | `agent-core/src/profile_files.rs` 同款解析器，13 个单元测试覆盖列表/引号/注释/CRLF/BOM/通配符 |
| 必填 name/description，缺了只产诊断跳过文件，不阻断启动 | `ProfileDiagnostic { file, message }`，诊断在设置 → 子代理展示 |
| 用户级 + 项目级两个作用域，按 name 合并，文件覆盖内置 | 用户级 `<数据目录>/EffiBuddy/agents`、项目级 `<工作区>/.effibuddy/agents`；`merge_profiles` 后者覆盖前者 |
| `tools` 是允许列表（`*`/空 = 继承），`disallowedTools` 是黑名单，黑名单优先 | `SubagentProfile.disallowed_tools` + `allows()` 先查黑名单；`child_tools()` 在运行时同样先过滤黑名单 |
| 目录不进工具描述，目录清单进 system prompt（`agent_listing` attachment） | `agent` 工具描述改为通用文案；目录渲染进 system prompt `# Subagents` 段（`PromptContext.agent_catalog`），经 `HarnessConfig.agent_catalog`（`Arc<RwLock<Vec<_>>>`）每 step 重建时取当前快照 |
| 每个新父 turn 取一次目录快照，本轮固定 | 每次 spawn 前 `refresh_catalog()` 重新扫描两个目录，快照写入共享 RwLock，模型下一个 step 就能看到新档案 |
| 模型覆盖：`inherit`/空 = 继承父模型；显式值解析失败**抛错不回退** | `SubagentProfile.model`：`prepare()` 里经 `ModelConfigFile.resolve` 解析；未知模型或供应商无 Key → spawn 直接失败并给出明确错误 |
| metadata 是唯一跨进程终态；重启后无终态者标 lost；ended 列表用 keyset 游标分页 | 子会话文件写入 `ChildRunRecord`（唯一跨进程终态）；重开会话 `restore_subagent_history()` 从磁盘恢复，无记录者标 `SubagentStatus::Lost`；侧栏历史行 30 条 + 「加载更多」按钮（内存列表用可见条数上限代替 keyset 游标，语义等价） |
| 插件/项目文件不能提权（permissionMode 剥离） | EffiBuddy 档案没有 permissionMode 字段，无需剥离；保留名 `main` 被拒绝（防止和主代理混淆） |

刻意没做的：

- **listing delta**（首轮全量 + 之后只发增删）：EffiBuddy 的目录段随 system prompt 每 step 重建，永远新鲜；delta 机制的收益（省 token）在目录变大后才明显，列为后续项。
- **chokidar 热重载**：改为每次 spawn 前重扫目录，行为等价、实现更简单。
- 插件来源档案（`plugin:name` 命名空间）、`agents-state.json` 禁用开关、`memory`/`skills`/`mcpServers` 字段。
- 恢复会话不重建组（workflow）任务——组由 `SessionState.graph` 持久化恢复，走原路径。

验证：`cargo test` 153 个测试全绿（新增 13 个解析器测试）；另用真实文件做了端到端加载验证（项目档案加载、内置合并、坏文件诊断），验证目录已删除。

## 7. 权限模式、命令面板、Markdown 表格与 diff 视图（本次模块）

参考：ZCode 的权限模式循环（Shift+Tab：默认询问 / 自动编辑 / 计划模式 / 完全访问，plan 先只读调研）、QuickPick 命令面板（评分排序、最近使用置顶、键盘选中保持可见）、markdown-table.tsx（表格真渲染 + 复制导出）与 diff 视图（词级高亮 + hunk 折叠）。

### 权限模式四档

| ZCode 决策 | EffiBuddy 落地 |
| --- | --- |
| 四档模式 Shift+Tab 循环，composer 有模式入口 | `agent-core/src/tool.rs` 的 `PermissionMode`（serde snake_case：`default` / `accept_edits` / `plan` / `full_access`）+ `next()` 循环；`shift-tab` 绑定 `CyclePermissionMode`，composer 权限 chip 四态可点击（只用 Theme token：默认询问 warning / 自动编辑 accent / 计划模式 warning / 完全访问 success） |
| 模式是审批门之上的策略层，工具协议不动 | `ModeApprovalGate` 包装内层 `BrokerApprovalGate`：按 `ToolClass`（`read_file` / `list_dir` / `grep` / `todo_write` 只读，`write_file` / `edit_file` 编辑，`shell` / `run_js` / `workflow_run` 命令，其余 Other）先做本地决策——Default 全转发、AcceptEdits 编辑类本地批准、Plan 编辑与命令类本地拒绝且只读类本地放行、FullAccess 全放行；本地决策不触达内层门，也就不产生审批卡。`ApprovalGate` trait 与子代理的门不动 |
| plan 模式「先出计划」且不能被绕过 | harness 在 Plan 下对**所有**工具询问审批门：`Auto` 策略的 `run_js` / `agent` 也过门，会话内「本会话允许」缓存不能预批准在其他模式批准的调用。只读工具本地放行零打断；编辑与命令被拒绝并附专用文案（「先出计划、等待切换模式」）；`start_run` 时向 system prompt 注入 plan 模式指令，composer footer 与审批卡同步提示 |
| 每会话记住模式，新会话有默认档 | `SessionState.permission_mode` 持久化（旧会话缺字段回落 Default、未知值拒绝）；三处新建会话入口按 `default_permission_mode` 种入（显式设置优先，旧 `auto_approve_tools=true` 迁移为完全访问）；设置页「自动批准工具调用」复选框升级为四档分段选择器，保存写 `default_permission_mode` 并同步 legacy 字段 |
| 模式切换立即生效 | 模式闭包每次审批请求实时读会话字段，运行中 Shift+Tab 对下一次工具调用即生效 |

刻意没做的 / 已知边界：

- 权限模式贯通整棵树：主会话把模式闭包放进 `HarnessConfig.permission_mode`，子代理 harness 也装 `ModeApprovalGate`，所以完全访问下子代理不再逐次弹卡、计划模式下子代理同样被本地拒绝（审批卡文案改为说明当前档位，不再声称「不受影响」）；
- plan 模式指令在 `start_run` 时快照，运行中切到 Plan 不更新 system prompt，由 Plan 专用拒绝文案补偿；
- 未知 `permission_mode` 值会让整个会话文件反序列化失败（严格拒绝是设计取舍，代价是该 transcript 打不开）。

### 命令面板：评分、MRU 与键盘导航

| ZCode（QuickPick） | EffiBuddy 落地 |
| --- | --- |
| 前缀/子串/子序列分级评分，中文可搜 | `palette_score` 纯函数：标题前缀 120 > 标题子串 100 > keywords 前缀/子串 90/80 > 副标题子串 70 > 标题子序列 50 > keywords 子序列 40；每项带中英 keywords，query 含汉字时跳过子序列级 |
| 最近使用置顶 | `config.palette_recent`（去重置顶、上限 20）随每次执行落盘；`order_palette_items` 把命中项按 MRU 置顶（组名改「最近使用」、置顶上限 6、总上限 14），按「最近使用 / 建议 / 面板 / 会话」分组渲染并给最近项加徽标 |
| ↑/↓ 移动选中、Enter 执行、选中保持可见 | gpui 0.2.2 源码验证：面板 backdrop 的 `on_key_down` 会被输入框自身的 Up/Down action 吃掉，故用 `App::intercept_keystrokes` 全局拦截——面板开启时捕获 ↑/↓ 环绕移动选中并阻断输入框光标移动，关闭时零影响；`ScrollHandle::scroll_to_item` 让选中行保持可见；Enter 执行当前选中项（顺带修复了 on_submit 读到已被清空输入的旧 bug） |

另外把 `--preview-palette` 修为可独立打开面板（原先写在 `seed_preview` 里，单独传参是死分支）。

刻意没做的：

- 每次执行命令仍在渲染线程同步写 `config.json` 记录 MRU（与既有主题切换的落盘模式一致），暂未移出渲染线程。

### Markdown 表格真渲染

| ZCode（markdown-table.tsx） | EffiBuddy 落地 |
| --- | --- |
| GFM 表格真渲染，不是伪代码块 | 新建 `ui/table.rs`：`TableCollector` 在 pulldown 事件层收集表格（`Start(Table)` 建收集器、`End(Table)` 产出 `render_table` 真表格元素），删除原「table」伪代码块；`render_markdown` / `render_markdown_streaming` 签名与流式光标行为不变，app 与 agent-core 零改动 |
| CJK 双宽列宽、横向滚动 | `measure_columns` 用 unicode-width 量宽（单列 40 单位截断、6 单位下限，>8 列回退均分权重）；表头加粗、1px 网线、隔行底色，外层 `overflow_x_scroll`，宽表右缘渐变提示可滚 |
| 复制 Markdown / CSV | 工具栏照抄代码块按钮模式（`cx.write_to_clipboard`）：`table_to_markdown` 还原 GFM（`\|` 与 `<br>` 转义，可被解析器无损 round-trip），`table_to_csv` 按 RFC-4180 转义并对 `= + - @` 开头的单元格加 `'` 前缀防 CSV 公式注入 |

刻意没做的：

- 工具栏常驻渲染而非 hover 出现（ZCode 仅 hover 出现），每张表顶部多占一行——列入第 5 节；
- 滚动渐变提示用「估算总宽 > 640px」阈值近似，GPUI 构建期拿不到容器实际宽度（table.rs 注释已说明这是 best-effort）。

### 变更页 diff：词级高亮与 hunk 折叠

| ZCode（diff-viewer） | EffiBuddy 落地 |
| --- | --- |
| 相似行的词级强调 | `word_diff_segments` / `annotate_hunk_rows` 用 similar 2 的行内 diff（需 opt-in `inline` 特性）处理相邻 -/+ 行块：相似行按词用 `success_soft` / `danger_soft` 底色 + SEMIBOLD 强调，整行替换或独立增删行整行着色；旧/新双列行号 gutter |
| 长上下文折叠 | `parse_unified_hunks` 解析文件段 / @@ hunk / 上下文行（兼容多文件、无 @@ 的未跟踪文件合成 diff、二进制与错误提示文本），hunk 内连续未修改行折叠为「⋯ N 行未修改」可点击行（两侧各保留 3 行；git 默认 3 行上下文时不会触发，与 GitHub 同语义）；展开状态存 `SidePaneState.expanded_hunks`，返回文件列表时清空 |
| 超长兜底 | 保留 6000 行截断上限，超限追加「已截断」提示行 |

刻意没做的：

- 聊天内 write_file 工具行的 diff 升级（`chat.rs` 归其他模块，本轮只做变更页签）；
- 渲染缓存：每次渲染全量重算 diff 解析、词级标注与折叠——列入第 5 节。

验证：`cargo test --workspace` 全部通过，离线 155 个测试（agent-core 65、agent-js 7、agent-tools 21、ui 10、app 52），另有 4 个真实模型测试默认 ignored。本轮新增测试覆盖：权限门决策矩阵与会话 serde 往返（agent-core）、默认模式与面板 MRU（config）、面板评分与排序（app）、表格收集/量宽/导出转义（ui）、diff 解析/词级/折叠纯函数（app side_pane）。

## 8. Computer Use：ZCode 的 a11y-first 与 EffiBuddy 的 visual-first v1

参考：ZCode Computer Use 插件的 `skills/computer-use/SKILL.md` 与 `docs/computer-use.md`（0.6.5）。它面向 macOS/Windows/Linux，核心不是“点坐标”，而是：

- **辅助功能优先**：先观察 AX 树，按 role/title/value 找元素，用元素 index 执行 click/setValue/performAction；坐标只是 canvas/游戏/Electron 的最后手段。
- **窗口级 observation**：capture 永远绑定一个 app 的一个 window；`window_id` 决定观察和动作的目标，modal 出现时窗口解析会变化。
- **raster 与坐标绑定**：坐标是“最新返回的 raster”的像素，不是全局屏幕坐标；frame 过期会拒绝，不由模型处理 frame id 变换。
- **动作后重新观察**：每次动作后必须 `getAXState`，用 diff 减少 token；不要在没有动作时重复观察。
- **strategy 分层**：`auto/a11y/event`；event 路径要求目标已在前台，后台应用会被 `FOREGROUND_REQUIRED` 拒绝。
- **单一控制器与停止**：`CONTROLLER_BUSY` 防止多会话抢输入；`stop` / kill switch / 权限拒绝后立即停止，不能换另一种自动化技术绕过。
- **后台不抢焦点**：观察后台 app（含截图）不抢焦点；只有 Windows 启动非打包 app / 恢复最小化窗口会抢焦点。

EffiBuddy v1 的取舍：

| ZCode 决策 | EffiBuddy 落地 |
| --- | --- |
| AX 树优先 | v1 没有 UIA/AT-SPI，先做截图 + 坐标；`ComputerBackend` 预留语义层，后续可加 |
| 窗口绑定 | X11/Windows 提供窗口级截图；Wayland 只有全屏，`status` 明确降级 |
| 最新 raster 坐标 | `frame_id` + `FrameInfo`（图像/原生尺寸、原点、缩放）；stale frame 失败关闭 |
| 动作后观察 | 工具描述强制“截图→动作→再截图”；不自动附加截图，避免每步双倍图片 |
| a11y/event strategy | v1 用 `auto/background/foreground`；Windows 停车窗口走后台消息，忽略消息时回退前台 SendInput |
| 单一控制器 | 只有一个主代理 `computer` 工具；子代理被显式过滤；运行时有 stopped/paused |
| 随时停止 | 悬浮条“接管操作 / 我帮你点好了 / 停止操作”；停止撤销授权并 `interrupt` |
| 后台不打扰 | X11 专用 EWMH workspace；Windows 后台停车位；Wayland 当前桌面（无法隔离） |
| 授权 | 独立 `ComputerApprovalGate`：Session 首次必弹卡，FullAccess 也不能绕过；Plan 拒绝修改类动作 |

刻意没做的：

- UIA/AT-SPI 元素树与语义动作（v2）；
- PipeWire ScreenCast 窗口流（Wayland 窗口级读图）；
- Windows 未公开的虚拟桌面 COM（改用后台停车位）；
- 自动截图回传（模型显式 `screenshot`，避免上下文被图片淹没）。

验证：`cargo test --workspace` 242 个离线测试全绿；在 Fedora 45/GNOME Wayland 上用 `--computer-probe screenshot` 实际走通 xdg-desktop-portal 截图（2560×1600 → 1280×800），并用 `--preview-computer` 目视确认悬浮条与输入框提示；Windows 后端因当前环境没有交叉工具链，未实机编译验证。

## 9. 消息编辑与撤销：无 Git 的文件检查点

参考：ZCode 桌面端（`app.asar`，v3.18.0 打包产物）中 `chat.message.edit`、`chat.changeSummary.rewind*`、`chat.edit.resetConversationAndFiles*` 与 `v4/conversation/fileRewindPreview` / `applyFileRewind` / `editUserQuery` 这条链路。

### ZCode 的做法

- 用户消息 hover 出现「复制 / 编辑」；编辑后进入 composer 的 editing 状态，发送时走 `editUserQuery`，返回 `rewind / fork / blocked` 三种处置。
- 文件回退不是直接写文件，而是先 `fileRewindPreview`：逐文件检查 checkpoint 是否缺失/不可读、当前文件是否被其他进程改过（`external_modified`）、路径是否支持；有冲突时**一个文件都不写**。
- checkpoint 存在私有 Git ref（`refs/zcode/checkpoints/<workspace>/<id>`）下：临时 index `git add -A` → `write-tree` → `commit-tree` → `update-ref`，恢复时 `git restore --source=...` 并再次校验。bash/shell 的改动被明确标为「已忽略」，不假装能还原。
- 编辑消息的冲突弹窗只提供「仅重置对话并发送」或取消；撤销对话框则列出可安全撤销、不能安全撤销、已忽略三组文件。

### EffiBuddy 的落地

| ZCode 决策 | EffiBuddy 落地 |
| --- | --- |
| Git ref 保存整树 checkpoint | **刻意不用 Git**：`agent-core` 定义 `FileCheckpointSink`；`write_file` / `edit_file` 在真正写入前回调 `before_write`，成功后回调 `after_write`（含写入内容的 FNV-1a 摘要），失败回调 `write_failed`；`shell` 只记录命令并在 UI 标为「无法追踪」。原始字节写在 `<数据目录>/checkpoints/<session>/<user-message>/<NNNN>.bin`。 |
| 每轮 checkpoint | `SessionState.checkpoints: Vec<TurnCheckpoint>`，按**用户消息 id** 分组。后台通知、子代理写文件都通过同一 sink 追加到父 turn；`RunStarted` 时把 recorder 绑定到该 turn，延迟写回的后台子代理也不会串到下一轮。 |
| 回退前逐文件预检 | `plan_restore`：每个路径取**最早**一次 before 快照 + **最晚**一次 after 摘要；当前内容与 after 不一致 → `ExternalModified`；快照缺失/不可读、路径变成目录/符号链接、写入失败 → 各自的原因。撤销对话框逐条展示，冲突时禁止写文件（只允许「仅撤销对话」）。 |
| 安全恢复 | 恢复前再次校验当前内容；逐个文件写临时文件 → 保留原权限 → 原子 rename；新建文件在撤销时删除。不使用 `git`、不依赖仓库是否干净。 |
| 编辑消息 = 裁剪对话 + 恢复本轮文件 + 重发 | composer 上方出现编辑条，显示将移除多少条消息、多少轮对话，以及「发送时还原 N 个文件」开关；发送前重新预检，冲突则弹「仅重置对话并发送 / 取消」。 |
| 撤销 = 删消息 + 恢复文件 | 用户消息 hover 的撤销按钮只打开二级确认；确认框列出全部将还原文件、冲突文件、shell 忽略项，并用红色提示「EffiBuddy 不会使用 Git，无法从版本库恢复」。running / 后台子代理未停时拒绝执行。 |
| checkpoint 清理 | 每个会话最多保留 64 个 turn（超出后最旧的快照目录删除）；会话从归档页永久删除时一并清理 `checkpoints/<session>`。 |

刻意没做的：

- fork / worktree 分支（ZCode 的 `forkAssistant` 与本功能无关）；
- shell / 外部进程改动的字节级追踪：只能警告，不能还原；
- 部分恢复：存在冲突文件时全部不写，避免「还原了一半」；
- Git 仓库状态读取：`不动 git`，恢复结果也不需要工作区干净。

## 10. 消息排队与回合中插入（本次模块）

参考：DeepSeek Harness（本地源码 `../deepseek-harness`，HEAD 0.1.2-rc.1，等待释放语义在 tag `dsh-v0.2.2-alpha.2`）的 `packages/core/agent/src/inbox.ts` 双队列与 `agent-loop` 的 step 边界 claim；ZCode（`../ZCode_ref`）的 `steer/guide/queue` 三词体系、`computeInputRouting` 裁决表与 `pendingInputs` 单数组双车道。

### 两家的共同结论（EffiBuddy 直接采纳）

- **消息只在 step 边界进入上下文**：DSH 在 `preStep` 里 `inbox.claim`，ZCode 在完整 tool batch / text-only 收口后 drain；都不打断正在流式的 LLM 或执行中的工具。
- **运行中的消息要么延长当前回合，要么排到新回合**，从不静默丢弃。
- **注入要有防伪框架**：DSH 的通知带 `[SYSTEM NOTIFICATION - NOT USER INPUT]`；ZCode 的 user_steer 模板声明「这是运行中的真实用户输入」，且同轮有通知时改用只否认通知的变体。
- **队列管理是完整产品面**：删除、立即发送、暂停/恢复、held 时二选一确认。

### 落地

| 两家决策 | EffiBuddy 实现 |
| --- | --- |
| DSH `followup/steer/inject` 三入口 | 两条车道：**排队**（Enter → `pending_inputs`，回合成功后 auto-drain 队首开新回合）与**插入**（Ctrl/Cmd+Enter → `AgentHandle::steer`，step 边界注入当前回合）。idle 时两种按键都等于普通发送 |
| DSH `next-step` 收件箱 | `AgentHarness.steers: Arc<Mutex<Vec<SteerInput>>>`；`run_loop` 每次迭代顶部 `drain_steers()`，消息以 `Message::mid_turn_user` 落 transcript，发 `SteerDrained{steer_id, message_id}` 让宿主摘除队列条目 |
| DSH「next-step 非空关不上 turn」/ ZCode text-only 续跑 | `tool_calls.is_empty()` 时若 `has_pending_steers()` 则 `continue`——最后一段纯文本期间到达的 steer 延长回合，而不是悬空 |
| ZCode `fallbackPendingGuidesToQueue` | 中断/失败时宿主 `take_pending_steers()` 收回，队列条目统一转为 Queue；不丢文本 |
| ZCode `computeInputRouting` reasonCode | 简化为按键分流 + `queue_held`（Interrupted/Failed 挂起队列，通知轮成功也不自动恢复，用户从队列条处置） |
| DSH 队列编辑/删除/升级 steer | 队列条：每行 chip（排队/待注入）+ 预览 + 删除 + 「发送」（running 时升级为 steer 插队；idle 时立即 start_run） |
| DSH `source.kind === 'user'` / ZCode wire 模板 | transcript 存原始文本，`build_chat_messages` 对 `mid_turn` 消息包 `<user-steer>` 框架；气泡带「运行中插入」chip，且禁用编辑/撤销（checkpoint turn 属于发起回合的那条消息） |
| DSH「通知与用户消息同一通道、不同 source」 | 后台通知仍走 `synthetic_user` 独立车道；`resume_after_run` 把队首消息与待投递通知合并进同一新回合（通知在前），DSH e2e「guide 与 launch result 合流进下一次请求」的同构行为 |

刻意没做的：

- **队列持久化账本**（ZCode 的 SQLite `session_input` + 原子 promotion）：单进程桌面应用崩溃丢队列可接受，重启后用户重发；已注入的 steer 是普通消息随会话落盘。
- **编辑排队消息 / 拖拽重排 / 编辑锁**（ZCode 的 editLock、reorder、reservation 三段式）：第一期只有删除与立即发送。
- **guide/queue 会话级设置**（ZCode `followupMode`）：按键即模式，无状态可配。
- **steer 解除工具等待**（DSH 0.2.2 的 `job_output` wait 释放）：EffiBuddy 没有 job 等待原语，后续做后台任务工具时一并考虑。
- **wake 预算**（DSH `maxConsecutiveWakes`）：EffiBuddy 的唤醒只来自用户排队消息与后台子代理通知，后者已有「等父 turn 结束」的闸门，自激链条暂无成型路径。

验证：`cargo test --workspace` 全绿（agent-core 88，含新增 steer 三个回归：工具执行中注入、纯文本回合延长、中断回收）；`update.rs` 的 `pick_asset_for` 三参数化属并行改动，未触碰。

## 11. 多文件夹项目（本次模块）

参考：ZCode 的 virtual project 设计（`docs/superpowers/specs/2026-09-09-virtual-project-design.md`、`packages/services/src/project-storage/`、`packages/ui/src/project-sidebar/`）。

ZCode 的核心决策，以及 EffiBuddy 的落地方式：

| ZCode 决策 | EffiBuddy 落地 |
| --- | --- |
| 项目是具名容器，含有序 `roots[]`（`isPrimary`），不是单个路径 | `app/project_store.rs`：`Project { id, name, roots: [{ path, is_primary }] }`，持久化到 `<数据根目录>/projects.json`（schema_version + 原子写） |
| 会话按 `project_id` 分组，分组键绝不使用路径（同一文件夹可属于多个项目） | `SessionState.project_id` 精确归属；旧会话的 `project` 路径保留为迁移种子，按项目根匹配兜底，未匹配时渲染临时 legacy 分组 |
| 新会话 cwd 固定为该项目的**主**根，创建后不可变 | `new_session_in_project(project_id)` → `cwd = primary`；编辑根的后续变化不改写任何会话 |
| 其余 roots 作为 `additionalDirectories` 交给 agent | `HarnessConfig.additional_workspaces` → `ToolContext.additional_workspaces` → `SubagentRequest` → `Workspace::with_roots`；系统提示列出附加目录 |
| 相对路径锚定 cwd；工具可读写全部 roots | `Workspace` 多根：相对路径锚定主根，绝对路径允许落在任一根；展示路径主根内相对、附加根内绝对 |
| AGENTS.md/config/git 只认会话 cwd，不扫附加目录 | `project_instructions_for(session.cwd)`；文件树、git、项目级 `.effibuddy/agents` 仍跟随 cwd（ZCode 对齐） |
| 文件树按根打开；多根项目显示 `Folders` 图标与按根菜单 | 侧栏项目行 `+N` 徽标 + 全部根路径 tooltip；项目菜单按根生成「打开：<文件夹>」；完整的合并文件树刻意不做 |
| 项目编辑：草稿根列表 + 主根 + 多选目录选择器，保存时整体替换 | 项目菜单 →「管理文件夹…」弹窗：改名、多选添加、移除、设为主文件夹；「+」按钮直接多选文件夹新建项目（第一个为主根） |
| 软删除项目，保留会话归属与历史 | 项目菜单 →「删除项目」（两步确认）移除注册表条目并连带删除项目内会话的转写/检查点/草稿，磁盘文件夹保留；`projects.json` 丢失时启动对账按会话路径重建项目，历史不消失 |

刻意没做的（后续迭代）：

- 会话拖拽换项目、项目置顶/手动排序、项目重名唯一化之外的命名规则。
- 多根合并文件树与多仓 git 汇总（ZCode 同样按单根打开）。
- 项目级 agents 扫描全部根（ZCode 对 skills/commands 扫全部根，对 AGENTS.md 只扫 cwd；EffiBuddy 一期统一只扫 cwd）。
- 远程主机 / `hostId` / 项目删除恢复（单机桌面应用不需要）。

验证：`cargo test --workspace` 全绿；新增 project_store 迁移/去重/主根提升/会话解析测试与 Workspace 多根读写/越界拒绝测试。

## 12. 上下文压缩：从用户轮次启发式摘要到 ZCode 链路

参考：`apps/zcode-cli/packages/core/src/compact/{policy,manual,microcompact,prompt}.ts` 与 `runtime/methods/{compact,turn-loop}.ts`。

ZCode 的关键做法与 EffiBuddy 的落地：

| ZCode 决策 | EffiBuddy 落地 |
| --- | --- |
| 有效窗口 = context window − 输出预留；阈值 = 有效窗口 − buffer | `CompactionConfig::thresholds` 用百分比算出触发/保留/摘要预算，并扣掉 `max_tokens`（上限为窗口 1/4） |
| 每个模型步都检查，而不只在用户回合开始 | `run_loop` 每步调用 `compact_if_needed`；round 以 user/assistant 消息切分，长工具回合可压缩 |
| 先 microcompact：清空旧工具结果，保留最近 N 条 | `microcompact_tool_results` 只清 read/grep/list/shell/write/edit/run_js，保护错误、图片与最近结果；`cleared_tool_calls` 只影响 wire，transcript/UI 保留原文 |
| 摘要由模型生成，带 `<analysis>` / `<summary>` 结构与“禁止调用工具”提示 | `COMPACTION_SYSTEM_PROMPT` + `build_compaction_request`（纯文本 transcript、无工具、无图片）；`format_compaction_summary` 解析 |
| 保留最近 1 轮、摘要请求失败有重试/熔断 | 保留预算按窗口百分比；失败回退本地摘录并计失败次数，达到上限只走本地摘录 |
| 用 provider usage 校准估算；overflow 后 reactive compact | `max(本地估算含工具 schema, last_prompt_tokens)`；`ProviderError::is_context_overflow` → 强制压缩并重试一次 |
| 压缩边界不能让 tool result 失去 assistant | 边界取 round 起点，跳过孤立 tool result，并要求摘要区间至少包含一个 assistant |

设置 → 上下文：自动压缩开关、触发阈值（%）、保留最近（%）、摘要上限（%）、旧工具输出清理与保留条数。全部百分比都相对当前模型窗口，切换模型自动换算。

刻意没做：独立压缩模型、`/compact` 手动命令、按 prompt-too-long 报错逐步丢弃最老 round 的多轮重试、microcompact 的空闲时间触发（ZCode 的 idle trigger）。

## 13. 并行压缩：OpenCode background compaction

参考：<https://opencode.ai/docs/compaction#background-compaction>、`packages/core/src/session/compaction.ts`、`packages/opencode/src/session/compaction.ts`（本地 checkout 2026-08-05，后台开关晚于该快照，按文档实现）。

OpenCode 的规则：

- 阈值前约 60k token（小窗口取“上次压缩后到阈值”的中点之前）开始后台摘要；主会话继续运行，摘要先扣住不生效。
- 到阈值时换入摘要，快照点之后的对话原样保留；摘要未写完就等待。
- 快照后新增量超过阈值约 40%，或模型/指令/历史起点变化，则丢弃摘要走正常压缩。
- 预生成摘要只驻留内存；未使用的摘要仍计费。

EffiBuddy 的落地：

| 概念 | 实现 |
| --- | --- |
| 预压缩起点 | `CompactionConfig.background_start_percent`（默认 50%，相对窗口），在设置 → 上下文可改；换算时自动压在触发阈值前 5–30 个百分点 |
| 后台任务 | `harness::start_background_compaction` 用 `tokio::spawn` 持有 `Arc<dyn ModelProvider>` + 快照请求 + 取消令牌；不碰 transcript |
| 持有摘要 | `SessionState.held_compaction: Option<HeldCompaction>`，`#[serde(skip)]`，彻底内存态 |
| 到点换入 | `apply_held_compaction` 校验模型/provider、`compacted_upto`、边界消息 id、快照后增长 ≤ 40% 阈值 |
| 未写完 | 最多等 15 秒，之后丢弃并走前台压缩；任务完成但 id 已失效则静默丢弃 |
| 失效 | 换模型、`truncate_from`、加载会话都会清空；超过 5 分钟仍 Writing 视为 stale |
| 默认 | 关闭（实验开关，避免未使用摘要的 token 成本） |

刻意没做：摘要请求复用主请求 prompt cache（我们的摘要把 transcript 渲染成纯文本请求，命中率不如 OpenCode）、独立 compaction agent/模型、native provider compaction。
