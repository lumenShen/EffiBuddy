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

## 已验证基准（release，32 核 / 38G）

| 场景 | 结果 |
| --- | --- |
| headless 1000 会话 × 500 tok/s | 聚合 525k tok/s（105%），事件 9.1 万/s，CPU 均值 4.3 核 / 峰 7.3 核，RSS 峰 1.14G，39k 轮 0 失败 |
| headless 1000 子代理 × 500 tok/s（RAM 持久化） | 506,855 tok/s（101.4%），24,405 次 RAM 保存，释放 243.7M |
| 应用内 1000 会话（采样渲染） | 541,906 tok/s（108.4%），UI 存活 |
| 极限内存（上文） | RSS 3.71G，水位分布精准，零泄漏零失败 |

**隔离验证**：真实会话目录逐次 md5 不变；`/dev/shm/effibuddy-stress-*` 每次退出即清空；仓库零污染。
