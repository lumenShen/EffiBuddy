# 压测实验室（Stress Lab）

EffiBuddy 的超高并发压测设施：**1000 路并发会话/子代理 × 每路 500 tok/s（可调至 20k）× 虚拟模型数据 × 内存工作区**，用于后端与前端性能的极限测试与优化验证。

> **隔离保证**：全部压测代码位于 Cargo feature `stress` 与独立 crate `crates/stress` 之后。
> 默认构建（含 release）**不包含任何压测代码**；只有显式启用才会编译进去。

```bash
# headless 后端极限压测（独立 bin，永不进入正式应用）
cargo run --release -p effibuddy-stress -- --mode sessions --n 1000 --seconds 30

# 应用内压测（真实 UI 泵 + 渲染管线；需显式 feature）
cargo run --release -p effibuddy-app --features stress -- --stress 1000
```

## 内存工作区（零硬盘写入）

压测期间**一切写入改道内存**，保护 SSD 寿命；结束后及时释放：

- Linux 下根目录为 `/dev/shm/effibuddy-stress-<uuid>/`（tmpfs，fs 接口完全不变，原子 rename 照常）；非 Linux 回退临时目录。
- `agent_core::RamDir`（`acquire`/`release`/`Drop`）三重释放保证：显式释放、任务退出、进程崩溃兜底。
- 三个接入点：
  1. **会话持久化**：应用内压测把 `SessionStore` 临时换到 RAM 目录（`with_directory`），子代理会话持久化随之入内存；结束时恢复真实 store 并刷新侧栏。
  2. **真实工具工作区**：`--real-tools` 时 `builtin_tools` 根目录指向 RAM 工作区（预置骨架文件）；`shell`/`run_js`/`agent` 被排除以防逃逸。
  3. **虚拟工具**（默认）：`stress_tools()` 纯内存虚拟文件系统，write→read→grep 往返自洽，不经过任何 fs。
- 应用内压测期间主会话的 `persist_active` 被挂起，真实会话文件保持压测前的状态。

## 模式

| 模式 | 说明 |
| --- | --- |
| `sessions` | N 路独立会话，各自 harness + 工具集循环开回合；dashboard 档不占用 UI 泵 |
| `subagents` | 走**生产级 `SubagentService`** 孵化 N 个子代理（事件天然流经真实前景泵），孵化/完成/审批走真实 UI 路径 |

前端形态（应用内）：`--stress-render dashboard|sampled|full` — 仪表盘 / 仪表盘+4 路采样迷你流 / 全量转发+全部子代理虚拟化行。

## 旗标（headless bin）

| 旗标 | 默认 | 说明 |
| --- | --- | --- |
| `--mode sessions\|subagents` | sessions | 工作负载形态 |
| `--n N` | 1000 | 并发路数 |
| `--rate TOKPS` | 500 | **每路** token 速率（聚合 = N × rate） |
| `--seconds S` | 30 | 时长 |
| `--chunk-tokens N` | 12 | 每 ProviderEvent 的 token 数（事件率 = rate/chunk） |
| `--seed N` | 0 | 场景种子（同种子同剧本） |
| `--workers N` | min(8, cores) | tokio 工作线程 |
| `--max-steps N` | 6 | 每回合场景步数上限 |
| `--real-tools` | 关 | 真实文件工具（RAM 根目录）替代虚拟工具 |
| `--context-window N` | 262144 | 上下文窗口（压缩阈值输入） |
| `--fill MIN:MAX` | 关 | **上下文填充内存测试**：每会话随机填充 transcript 至窗口的 MIN~MAX 比例 |

应用内等价旗标：`--stress [N]`、`--stress-mode`、`--stress-rate`、`--stress-seconds`、`--stress-render`、`--stress-chunk`、`--stress-seed`、`--stress-context`、`--stress-fill`、`--stress-real-tools`。

### 极限内存测试（1M 上下文 × 30%~70%）

```bash
cargo run --release -p effibuddy-stress -- \
  --mode sessions --n 1000 --seconds 120 \
  --rate 20000 --chunk-tokens 400 \
  --context-window 1000000 --fill 0.3:0.7
```

- 每会话按种子随机取水位（窗口的 30%~70%），逐回合注入定量 filler markdown（每回合增长上限 80k token，控制瞬时拷贝峰值），到达水位后转为微小「待命」回合（回合间隔 2s，避免反复 clone 超长 transcript）。
- 报告尾部输出水位分布（p5/p50/p95/max）与 RSS 峰值。
- **实测基准**（2026-11-14，32 核 / 38G RAM）：1000/1000 会话达标（p5 31.6% / p50 51.7% / p95 67.9%），累计 5.17 亿 token，RSS 峰值 **3.71G**，稳态无泄漏，0 失败，0 压缩触发（70% < 78% 阈值）。
- 填充速率建议 ≥ 20k tok/s（1M 窗口 70% ≈ 700k token，500 tok/s 需 23 分钟；20k tok/s 约 35 秒）。

## 虚拟模型数据（StressProvider）

- 位于 `agent-core::providers::stress`（feature 门控），实现 `ModelProvider`，每请求无状态（可安全共享给 1000 路 harness）。
- **46 个模板**：用户消息 12（中英混合）、reasoning 10、正文 12（富 markdown：围栏/表格/列表）、工具调用 12（list_dir/read_file/grep/write_file/edit_file/todo_write/agent…，按 `ChatRequest.tools` 过滤 — 只调用真实注册过的工具）。
- **定速**：token-bucket 精确记账（20ms tick，chunk 边界溢出计入下个 tick），长程速率收敛于目标值；支持 cancel。
- 每 step：reasoning → 工具批（1-3 个）或终稿；`Completion.usage` 携带 prompt/completion token 估算（与真实适配器约定一致，不发 Usage 事件）。
- 同种子同剧本（`DefaultHasher` 对首条用户消息 + 种子派生每回合 RNG）。

## 指标解读

- `tok/s`：聚合 **completion** token 吞吐（1s 滑窗；prompt 增量不计入）。
- `events/s`：harness 事件吞吐（TextDelta/ReasoningDelta/ToolCallStarted/RunFinished/UsageUpdated…）。
- `cpu / rss / peak`：进程级（`/proc/self/stat` 差分 + `VmRSS`/`VmHWM`），top 口径（100% = 1 核）。
- `context fill`：每会话终态 `last_prompt_tokens / 窗口`。
- 应用内仪表盘另有：实测 vs 目标 tok/s、事件吞吐、CPU/RSS、帧率、完成/取消/失败、子代理进度、吞吐 sparkline。

## 压测驱动的热路径优化（正式改进，不门控）

| 热点 | 修复 |
| --- | --- |
| harness 每 token 线性查 `messages`（O(n)/delta） | `SessionState.message_index` O(1) 查表（append-only 语义，加载重建） |
| 图谱每步级事件失效 → 1000 节点投影每帧重建 | 仅拓扑/状态变化失效 + 250ms 重建下限（`graph/mod.rs`） |
| 图谱 1000 张全量节点卡（每张 15+ 元素 + Spinner） | LOD：可见节点 >140 渲染简化条（4 元素、静态状态点） |
| `subagent_tasks` 每事件 O(n) 扫描 | `agent_id → index` HashMap（append-only + reset 清理） |
| 子代理转发器**每个结构事件** clone+序列化+保存整个子会话 | 800ms 节流 + 尾随刷新（对齐主会话持久化模式） |
| 每次 spawn 重扫 profile 目录（真实磁盘） | `refresh_catalog` 2s TTL |
| 侧栏摘要每次刷新全量解析全部 JSON | `(mtime, size)` 缓存，只解析变更文件 |
| 消息重建每次重哈希全部文本（`message_signature` / 提示栏预览） | `Message::revision` 自增指纹替代全量哈希；提示栏按指纹复用已展平预览（10k 消息 rebuild 10.4ms → 4.0ms） |
| 占用环/概览每帧重算整条 transcript 的 token（每帧两遍） | 按会话 revision 记忆化，流式期间 250ms 节流（10k 消息帧率 132 → 199 fps） |
| 折叠的右侧栏仍然每帧构建内容树 | `side.width <= 0` 时返回零宽占位符 |
| `prepare_frame` 每帧重建全部虚拟行计划 | `plan_cache` 按 items/展开态/流式态复用，空闲与滚动帧 O(1) |
| 冷启动 `ListState::reset` 后行高估算为 0 → 历史滚不上去 | `set_estimated_item_height` 首帧估算 + `splice` 无邻居时借已知均值/估算；`reset` 后重新贴底 |
| **子代理审批卡死**：children 的 write_file/edit_file 走 `BrokerApprovalGate`，审批卡片等用户点击 → 子代理挂起 600s（压测中 750/1000 卡死、UI 审批卡片堆积） | 压测子代理设为只读（`read_only: Some(true)`）— 变更类工具被过滤，剩余工具全部 `ApprovalPolicy::Auto`，零审批往返（修复后：取消 750→0、完成 419→1497） |

## 已验证基准（release，32 核 / 38G）

| 场景 | 结果 |
| --- | --- |
| headless 1000 会话 × 500 tok/s | 聚合 525k tok/s（105%），事件 9.1 万/s，CPU 均值 4.3 核 / 峰 7.3 核，RSS 峰 1.14G，39k 轮 0 失败 |
| headless 1000 子代理 × 500 tok/s（RAM 持久化） | 506,855 tok/s（101.4%），24,405 次 RAM 保存，释放 243.7M |
| 应用内 1000 会话（采样渲染） | 541,906 tok/s（108.4%），UI 存活 |
| 应用内 1000 子代理（只读） | 全部跑完零卡死（1497 回合 / 0 取消）。聚合均值 ~19k tok/s 是**生产语义天花板**：每个子代理 = 单回合（`execute_run` 一次 `run_turn`，~600-2000 token），1000 路总工作量 ~60 万 token 在 ~30s 内跑完即结束，不是性能瓶颈。要维持持续负载可配合 `--stress-fill`（单回合填充至水位）或由 runner 在子代理完成后继续孵化 |
| 极限内存（上文） | RSS 3.71G，水位分布精准，零泄漏零失败 |

**隔离验证**：真实会话目录逐次 md5 不变；`/dev/shm/effibuddy-stress-*` 每次退出即清空；仓库零污染。

## 消息流基准（10k 消息滚动）

`crates/app/src/chat_bench.rs`：用**真实 `ChatList`**（虚拟行、分块 markdown、前缀
splice）在无窗口的 GPUI 测试环境里排版并绘制一份 2500 回合 / 10000 条消息的
transcript，每回合都是「用户输入 → 思考 + 工具调用 → 工具结果 → 思考 + 正文」。

```bash
EFFIBUDDY_BENCH_10K=1 cargo test --release -p effibuddy-app --bin effibuddy \
    chat_bench -- --nocapture
```

可选：`EFFIBUDDY_BENCH_N`（消息数，默认 10000）、`EFFIBUDDY_BENCH_FRAMES`
（滚动屏数，默认 600）；`EFFIBUDDY_CHAT_TRACE=1` 打印 `rebuild` 的分段耗时
（results / index / items / signatures / prompt_marks / publish）。

| 指标 | 修复前 | 修复后 |
| --- | --- | --- |
| 冷启动可滚动范围（20625 行） | **649px**（约一屏，历史滚不上去） | 1.98M px |
| 冷启动落点 | 第 0 行（会话开头） | 第 822/825 行（最新消息） |
| 零高估算行 | 809 / 825 | 0 |
| 追加消息后 `rebuild` | 10.4ms p50（p95 11.9ms） | **3.99ms p50**（p95 4.26ms） |
| 滚动帧 `prepare_frame` | 0.38ms | 0.38ms（与内容量无关） |
| 滚动帧 `draw` | 0.51ms | 0.51ms（每屏约 11 行） |
| 流式帧 `prepare_frame` | 0.45ms | 0.45ms |

> 测试环境用 GPUI 的 `NoopTextSystem`（等宽近似），换行宽度与真实字体有差异，
> 所以「文字排版」成本偏乐观；结构性成本（重建、槽位计划、markdown 块构建）
> 与真实运行一致。

### 基准暴露并修掉的缺陷

1. **冷启动后无法回看历史**（用户可见）。`ListState::reset` 之后每一行都没有测量
   过，旧实现把「无前邻行可继承」的估算高度取 0：整条历史的估算总高只剩一屏，
   `max_offset` 塌到 649px，滚轮一向上就被夹回会话开头，中间的几千条消息永远
   滚不到。修复：`ListState::set_estimated_item_height` 由使用方声明首帧估算
   （对话流 96px），`splice` 在无邻居可继承时先借已知均值、再退回该估算。
2. **长会话打开后停在开头而不是最新消息**。`reset` 会清空逻辑滚动位置，`rebuild`
   里的 `scroll_to_bottom` 发生在行数更新之前，于是又被清掉。修复：
   `prepare_frame` 在 `first_sync` 分支 `reset` 之后重新贴底。
3. **`rebuild` 每个工具边界都重哈希全部文本**。300 万字符的 transcript 上
   `message_signature` 单项 3.7ms、`build_prompt_marks` 另有 2.4ms。修复：
   `Message::revision`（所有 mutator 经 `touch()` 自增）替代全量哈希，外部输入
   （工具结果、执行时长）仍然参与指纹；前端提示栏改为按指纹复用已展平的预览；
   工具结果复用比较改为形状（长度）而非逐字节。

## 真实窗口滚动基准（`--bench-scroll`）

`chat_bench` 只能测应用侧的结构成本：它的无窗口环境用 GPUI 的 `NoopTextSystem`，
字形排版、光栅化与 GPU 提交在那里都是免费的。要看「用户真正感知到的帧」，得跑
真窗口：

```bash
# 10k 消息、真实字体、真实 GPU；--bench-scroll [秒数]
EFFIBUDDY_PERF=1 EFFIBUDDY_VSYNC=0 \
  cargo run --release -p effibuddy-app -- --bench-scroll 8

# 可选环境变量
EFFIBUDDY_BENCH_SCROLL_MESSAGES=10000   # 种子消息数（默认 10000）
EFFIBUDDY_BENCH_SCROLL_STEP=64          # 每帧滚动像素（默认 64，0 = 只重绘不滚动）
```

会打印一行 `[bench]`（frame / work / render 的 avg-p50-p95-p99）和一行
`[bench-scroll]`（帧数、虚拟行数、每帧实际绘制行数、行计划缓存命中、以及
prepare / rail / render_chat / 各面板的分段耗时）。

### 实测（本机，10000 条消息 / 5004 个虚拟行 / 8.3MB 文本，vsync off）

| 指标 | 修复前 | 修复后 |
| --- | --- | --- |
| 帧率 | 132 fps | **199 fps** |
| 帧间隔 p50 | 7.5ms | **4.7ms** |
| `window.draw + present` p50 | 5.9ms | **2.5ms** |
| 整棵元素树构建（`render`） | 3.17ms | **0.12ms** |
| 其中右侧栏 | **3.20ms** | 0.03ms |
| 其中主内容区（含 `prepare_frame`） | 0.06ms | 0.05ms |
| 1k 消息对照 | 184 fps | 193 fps |

修复后 **帧成本与历史长度脱钩**：1k 与 10k 消息的帧率、元素树构建时间基本一致。
按 300Hz 的 3.3ms 预算，2.5ms 的帧成本可以顶住。

### 基准揪出的三个「每帧 O(历史)」热点

这三个都不在滚动路径上，靠无窗口基准和肉眼都看不出来——只有真窗口的分段计时
才能定位：

1. **输入框占用环每帧重算整条 transcript 的 token**。`context_usage()` →
   `context_meter::collect_from_session` 逐字符估算每条消息（tool call 还要
   `serde_json::to_string` 一次），10k 消息约 2–3ms，**而且每帧算两遍**（输入框
   一次、右侧栏概览一次）。修复：按会话 revision 做 O(1) 键的记忆化
   （`cached_message_usage`），流式期间用 250ms 下限节流。
2. **右侧栏折叠到 0 宽仍然构建整棵树**，于是上面的概览页照旧每帧走一遍全量
   估算。修复：`render_side_pane` 在 `side.width <= 0` 时直接返回零宽占位符。
3. **行计划每帧重建**：`prepare_frame` 每次都走一遍全部 item 生成槽位与 slot key。
   修复：`plan_cache` 记住「items 的 `Rc` 指针 + running + 四个展开集合」，未变化
   且没有流式揭示时整帧 O(1)（空闲/滚动帧的 `prepare_frame` 从 0.38ms 降到
   0.00ms；流式帧仍照常重建，因为揭示表每帧都在长）。

> 教训：虚拟列表做得再好，只要**每帧还有一处 O(消息数) 的旁路计算**，长会话就
> 会一直慢。真窗口分段计时（而不是只看总帧时间）是找到这类问题的唯一可靠手段。

## 呈现节奏（垂直同步）

渲染循环默认**与显示器刷新率同步**：呈现一帧会阻塞到显示器下一次 vblank，因此面板滑动、打字机吐字、平滑滚动这类动画每刷新周期最多推进一次，不会为了用户看不见的画面把帧率跑到几百帧。

- 刷新率**不做假设**：底层用呈现队列的 FIFO 语义（Vulkan `FIFO` / Metal `displaySyncEnabled`），60Hz、120Hz、144Hz、240Hz 或 VRR 都各自按真实节奏走，代码里没有任何固定帧率。
- 设置页「外观 → 渲染 → 垂直同步」可随时开关，开关直接重建交换链，无需重启；选择持久化到 `config.json`（`vsync` 字段，默认 `true`）。
- `EFFIBUDDY_VSYNC=0` 强制关闭同步（**优先级高于**用户设置），用于测量未限帧的原始吞吐；这类运行会在 HUD 与 bench 日志里标记 `NO-VSYNC`。
- `EFFIBUDDY_PERF=1` 启动时会打印一行 `[gpu] requested surface display sync: ...`，可直接确认本次运行是 `FIFO` 还是 `MAILBOX`。

> 做帧率对比时注意：`--bench` 会持续注入流式负载，帧率主要由注入量决定而非呈现节奏，因此两条基准线可能接近；要验证同步本身，应看 `[gpu]` 那一行以及空闲时的 `[perf]` 输出（同步生效时空闲不产生任何帧）。

