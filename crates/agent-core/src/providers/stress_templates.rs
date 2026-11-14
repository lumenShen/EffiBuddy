//! Virtual-data templates for the stress lab — dozens of message, reasoning,
//! final-text and tool-call templates randomly injected into sessions.
//!
//! Everything here is inert data: the [`super::stress::StressProvider`] picks
//! templates with a seeded RNG and streams them at the configured token rate.
//! Tool templates are matched against `ChatRequest.tools`, so they only fire
//! when the harness actually registered the tool (virtual set, real set or a
//! subagent-capable set).

use serde_json::{Value, json};

/// Seeded user prompts — the "input" side of the concurrency storm.
/// Mixed Chinese/English, short questions to long task briefs.
pub const USER_PROMPTS: &[&str] = &[
    "帮我梳理这个 crate 的模块边界，指出哪些 pub 接口泄漏了实现细节。",
    "Read `harness.rs` and summarize the agent loop. Focus on cancellation paths.",
    "把测试失败的原因分析一下，优先修复 flaky 的那几个。",
    "I need a migration plan from serde_json::Value arguments to typed args. Keep it under 300 words.",
    "为新增的 provider 写一套离线测试：鉴权失败、流中断、tool-call 重组三种场景。",
    "扫描整个 workspace，把所有 unwrap() 按风险分级列出来。",
    "Refactor the markdown chunker so fence-safe splitting is unit-testable, then add tests.",
    "这个渲染管线在 4K 屏上掉帧，帮我定位是布局还是 shaping 的问题。",
    "Give me a table of every public struct in agent-core with its invariants.",
    "起草一个双周迭代计划：第一周性能基线，第二周并发压测，第三周修复与回归。",
    "写一个脚本把 sessions/*.json 合并成一份统计报告，注意内存占用。",
    "Review the diff I left in the working tree. Flag anything unsafe or O(n²), then suggest fixes with code.",
];

/// Reasoning templates streamed before each step. 200–2000 chars.
pub const REASONING: &[&str] = &[
    "先确定检索范围：目标符号集中在 harness 与 provider 边界。取消信号经由 CancellationToken 自上而下传播，任何 await 点都要检查一次。风险最大的是流式重组那段——分片乱序会导致 tool-call 参数拼接错位，先写一个确定性复现再动手。",
    "The dependency graph suggests the change is local. Two constraints matter: (1) the event pump must never be notified per delta, and (2) the transcript stays authoritative in SharedSession, so the UI snapshot is always derivable. I will measure before touching the renderer.",
    "从使用统计看，九成调用落在三条热路径上。与其全局加锁，不如把会话内的消息索引变成 O(1) 查表：push 时登记、压缩时重建。注意 poison 恢复路径也要走同一入口，否则 panic 之后索引和 Vec 会失配。",
    "Benchmark noise first: two worker threads mean the event forwarder competes with provider streams for the same runtime. Pinning matters less than batching. Collect p50/p95 for both event latency and frame interval before changing anything structural.",
    "这里有一个容易被忽略的边界：压缩会删除旧消息，任何按 id 缓存的游标都要在压缩后失效。用代数（generation counter）标记缓存有效性比逐项校验便宜得多。先在测试里模拟压缩中途到达的 delta。",
    "Plan: (1) inventory all write sites that hit the session directory, (2) route them through the store swap, (3) verify with an inode-level check that nothing outside the tmpfs root changed. The tool sandbox root must come from the same RAM handle, not from config.",
    "并行度不是越高越好：1000 路 × 42 事件/秒已经让泵批大小到了几十，继续加线程只会增加调度抖动。先用当前拓扑测出瓶颈在锁、分配还是系统调用，再决定是否引入工作分片。",
    "记忆点：流式协议的三种线格式都以「先 reasoning 后正文」为主序，但 Anthropic 会在正文块之间插入 thinking 块。模板注入时保持同样的交替节奏，前端才不会把 reveal 缓冲当成异常路径。",
    "Consider the failure envelope: a tool that panics must become an error observation, not a run failure; a provider that stalls must trip the soft timeout without killing the stream. The virtual set lets me exercise both without real subprocesses.",
    "把「输入」与「输出」分开度量：输入是用户提示与工具观察的注入速率，输出是 token 流与事件速率。聚合吞吐要按时间窗对齐，否则完成态会话会把均值拉低，报告里要用滑动窗口而非累计平均。",
];

/// Final assistant texts — rich markdown (0.5–8KB): fences, tables, lists.
pub const FINAL_TEXTS: &[&str] = &[
    "## 结论\n\n改动集中在两处：事件泵的批处理与消息索引的 O(1) 化。\n\n| 指标 | 之前 | 之后 |\n| --- | --- | --- |\n| 单 delta 成本 | O(messages) | O(1) |\n| 泵批大小 | 1 | ~37 |\n| p95 帧间隔 | 14ms | 9ms |\n\n后续如果聚合吞吐继续抬升，下一步是给 `SessionState` 的索引加代数校验。\n\n```rust\nlet index = *session\n    .message_index\n    .get(&message_id)\n    .ok_or(AgentError::Other(\"stale message index\".into()))?;\n```\n\n以上。",
    "**Summary** — the loop is bounded by three resources: the token budget, the tool timeout and the event channel.\n\n1. **Token budget** — compaction triggers at 78% of the context window.\n2. **Tool timeout** — 180s wall clock, enforced with `tokio::time::timeout`.\n3. **Events** — unbounded channel; the pump drains per frame.\n\n> Keep the pump single-threaded. Parallel dispatch would break ordering guarantees that the reveal buffer relies on.\n\nNothing else needs to change for this iteration.",
    "### 验证记录\n\n- `cargo test -p effibuddy-agent-core`：156 通过，0 失败。\n- 压测：1000 路并发、每路 500 tok/s，30 秒窗口。\n- 资源：RSS 峰值 2.1GiB，CPU 均值 83%（16 核）。\n\n```text\n[stress] tok/s 498,341  events/s 21,152  cpu 83.2%  rss 1.9G\n```\n\n风险与遗留：\n\n1. 子代理保存节流窗口内崩溃会丢最后一个增量（可接受）。\n2. 图谱投影在 1000 节点下每帧重建 0.8ms，暂不处理。\n3. `write_file` 走的是内存工作区，落盘语义需要在文档里注明。",
    "I compared three approaches before settling:\n\n| Approach | Throughput | Complexity | Risk |\n| --- | --- | --- | --- |\n| Per-session timers | 412k tok/s | low | timer storms |\n| Shared scheduler | 505k tok/s | high | fan-out bugs |\n| Token-paced ticks | **498k tok/s** | medium | rounding |\n\nToken-paced ticks won: the rounding error is bounded by one tick budget and the code stays a plain loop.\n\n```rust\nconst TICK: Duration = Duration::from_millis(20);\nlet budget = self.rate * TICK.as_secs_f32();\n```\n\nThe scheduler variant remains viable if we ever need per-stream priority, but I would not build it speculatively.",
    "修复完成。关键点：\n\n1. **根因**是重试只在「尚无可见输出」时触发，而虚拟 provider 的首个分片在 20ms 内就到达，掩盖了真正的网络类失败。\n2. 修复后注入三种故障（断流、HTTP 429、JSON 截断）全部按预期退避。\n\n```json\n{\"retry\": {\"attempts\": 2, \"base_delay_ms\": 600}}\n```\n\n残留两个 TODO 已在任务面板登记，均不阻塞合入。",
    "**Observation**: the event pump's batch size is the single most useful stress metric. It collapses three signals into one number:\n\n- if it is `1` during a storm, notifications are leaking per-delta;\n- if it spikes above `~512`, the UI thread is starving;\n- if it oscillates, a structural event is forcing rebuilds mid-stream.\n\nI added it to the dashboard next to tokens/s. A healthy run at 1000 streams sits between 30 and 60.",
    "### 下一步计划\n\n- [x] 基线报告（本次）\n- [ ] 索引化 `subagent_tasks` 查找\n- [ ] 摘要缓存按 `(mtime, size)` 失效\n- [ ] 图谱投影仅结构性重建\n\n> 原则：先测量，再动结构。所有优化必须能被压测报告证伪。",
    "The virtual tool chain completed 12 calls across 4 steps. Notable:\n\n- `write_file` → `read_file` round-trip is coherent inside the RAM workspace (content hash matched).\n- `grep` over generated sources found 41 matches; count mode verified the same total.\n- Cleanup confirmed: `/dev/shm/effibuddy-stress-*` removed on exit, RSS returned to baseline.\n\n```text\nsteps: 4   tool_calls: 12   tokens: 18,204   wall: 36.4s\n```\n\nNo leaks, no orphaned tasks.",
    "两个结论，一个建议。\n\n**结论一**：每路 500 tok/s 在 1000 路下聚合 50 万 tok/s，泵批均值 41、p95 帧间隔 11ms，前端在采样档完全胜任。\n**结论二**：全量转发档的瓶颈不在渲染而在 `subagent_tasks` 的线性扫描——每次结构事件全表扫一遍。\n\n建议：把任务表换成 `HashMap<AgentId, usize>` 索引，改动面小，收益直接乘以事件数。",
    "## Perf notes for reviewers\n\n- Frame cost is dominated by element-tree build; markdown caches absorb re-parses (hit rate 98.4% during streaming).\n- The reveal buffer turns 42 chunk events/s into a typewriter cadence; backlog never exceeded 160 chars/frame budget.\n- All measurements used the release profile with debug symbols (stack sampling stays readable).\n\nRepro: `cargo run --release -p effibuddy-stress -- --mode sessions --n 1000 --seconds 30`.\n\nIf a number looks off, check the sliding window first — cumulative averages hide ramp-up.",
    "已按计划完成三件事：\n\n1. 会话持久化改道内存（tmpfs），压测全程零磁盘写入；\n2. 虚拟工作区为 1000 路会话提供隔离的文件语义；\n3. 退出时显式释放，tmpfs 页即时归还内核。\n\n```rust\nlet ram = RamDir::acquire(\"stress\")?;\nlet store = SessionStore::with_directory(ram.join(\"sessions\"));\n// ... run ...\nlet freed = ram.release();\n```\n\n验证方式与指标口径见 `docs/STRESS_LAB.md`。",
    "**Verdict**: ship the O(1) message index. Evidence:\n\n1. Delta latency p95 dropped from 2.1ms to 0.3ms at 1000 streams.\n2. CPU share attributable to transcript lookup fell from 22% to <2%.\n3. No semantic change: the index is rebuilt on compaction and cross-checked on every push.\n\nThe remaining hot spot is serialization; the RAM store keeps it honest (same JSON, same atomic rename, no disk wear).",
];

/// One tool-call template. `build_args` draws from the session RNG so two
/// sessions diverge; `build_output` fabricates the observation the harness
/// will feed back on the next step. Only templates whose tool is registered
/// (matched via `ChatRequest.tools`) can be selected.
pub struct ToolTemplate {
    pub name: &'static str,
    /// One-line summary used in logs/report samples.
    pub label: &'static str,
    pub build_args: fn(&mut crate::providers::stress::Rng) -> Value,
    pub build_output: fn(&Value) -> String,
}

pub const TOOL_TEMPLATES: &[ToolTemplate] = &[
    ToolTemplate {
        name: "list_dir",
        label: "list_dir .",
        build_args: |rng| {
            let _ = rng;
            json!({ "path": "." })
        },
        build_output: |args| {
            let path = args["path"].as_str().unwrap_or(".");
            if path == "." {
                "src/\ndocs/\nassets/\ncrates/\nCargo.toml\nREADME.md\n".to_string()
            } else {
                "mod.rs\nharness.rs\nsession.rs\nprovider.rs\ntool.rs\n".to_string()
            }
        },
    },
    ToolTemplate {
        name: "list_dir",
        label: "list_dir src/modules",
        build_args: |rng| {
            let _ = rng;
            json!({ "path": "src/modules", "depth": 2 })
        },
        build_output: |_args| {
            "core/\n  state.rs\n  events.rs\nrender/\n  list.rs\n  markdown.rs\nnet/\n  provider.rs\n"
                .to_string()
        },
    },
    ToolTemplate {
        name: "read_file",
        label: "read_file src/lib.rs",
        build_args: |rng| {
            let file = ["src/lib.rs", "src/harness.rs"][rng.below(2) as usize];
            json!({ "path": file })
        },
        build_output: |args| {
            let file = args["path"].as_str().unwrap_or("src/lib.rs");
            format!(
                "// {file}\npub mod agent {{\n    pub const VERSION: &str = \"0.1.0\";\n    pub fn bootstrap() {{\n        // entry point\n    }}\n}}\n"
            )
        },
    },
    ToolTemplate {
        name: "read_file",
        label: "read_file docs/plan.md",
        build_args: |_rng| json!({ "path": "docs/plan.md" }),
        build_output: |_args| {
            "# Plan\n\n- [x] baseline\n- [ ] concurrency\n- [ ] report\n\nAll work happens inside the RAM workspace.\n"
                .to_string()
        },
    },
    ToolTemplate {
        name: "write_file",
        label: "write_file src/generated_*.rs",
        build_args: |rng| {
            let n = rng.below(10_000);
            json!({
                "path": format!("src/generated_{n}.rs"),
                "content": "//! Generated for the stress lab (RAM workspace).\npub struct Generated {\n    pub id: u32,\n    pub label: String,\n    pub active: bool,\n}\n\nimpl Generated {\n    pub fn id(&self) -> u32 {\n        self.id\n    }\n}\n"
            })
        },
        build_output: |args| {
            let bytes = args["content"].as_str().map(str::len).unwrap_or(0);
            format!(
                "wrote {} bytes to {}",
                bytes,
                args["path"].as_str().unwrap_or("?")
            )
        },
    },
    ToolTemplate {
        name: "write_file",
        label: "write_file reports/*.md",
        build_args: |rng| {
            let n = rng.below(1000);
            json!({
                "path": format!("reports/run_{n}.md"),
                "content": "# Run report\n\n- tokens/s: 498k\n- events/s: 21k\n- cpu: 83%\n"
            })
        },
        build_output: |args| format!("saved {}", args["path"].as_str().unwrap_or("?")),
    },
    ToolTemplate {
        name: "edit_file",
        label: "edit_file src/main.rs",
        build_args: |_rng| {
            json!({
                "path": "src/main.rs",
                "old_string": "fn main() {",
                "new_string": "fn main() {\n    #[cfg(feature = \"stress\")]\n    stress::arm();"
            })
        },
        build_output: |_args| "1 replacement applied to src/main.rs".to_string(),
    },
    ToolTemplate {
        name: "edit_file",
        label: "edit_file reports/run_*.md",
        build_args: |rng| {
            let n = rng.below(1000);
            json!({
                "path": format!("reports/run_{n}.md"),
                "old_string": "- cpu: 83%",
                "new_string": "- cpu: 81%"
            })
        },
        build_output: |_args| "1 replacement applied".to_string(),
    },
    ToolTemplate {
        name: "grep",
        label: "grep pattern in src",
        build_args: |rng| {
            let pattern =
                ["pub fn", "unwrap()", "async fn", "#[cfg(feature"][rng.below(4) as usize];
            json!({ "pattern": pattern, "path": "src", "max_results": 20 })
        },
        build_output: |args| {
            let pattern = args["pattern"].as_str().unwrap_or("?");
            let hits = 3 + (pattern.len() % 9);
            (0..hits)
                .map(|i| format!("src/module_{i}.rs:{}: {pattern} …", 40 + i * 3))
                .collect::<Vec<_>>()
                .join("\n")
        },
    },
    ToolTemplate {
        name: "grep",
        label: "grep count mode",
        build_args: |rng| {
            let pattern = ["TODO", "unsafe", "spin_loop", "remove_dir_all"][rng.below(4) as usize];
            json!({ "pattern": pattern, "path": "src", "count_only": true })
        },
        build_output: |args| {
            let pattern = args["pattern"].as_str().unwrap_or("?");
            format!("{pattern}: 12 matches in 4 files")
        },
    },
    ToolTemplate {
        name: "todo_write",
        label: "todo_write 4 items",
        build_args: |_rng| {
            json!({
                "todos": [
                    { "content": "确认目标与约束", "status": "completed" },
                    { "content": "阅读工作区与相关文件", "status": "in_progress" },
                    { "content": "实现改动并运行验证", "status": "pending" },
                    { "content": "总结结果与剩余风险", "status": "pending" }
                ]
            })
        },
        build_output: |_args| "todo list updated (4 items)".to_string(),
    },
    ToolTemplate {
        name: "agent",
        label: "agent spawn scout",
        build_args: |rng| {
            let n = rng.below(100_000);
            json!({
                "agent": "scout",
                "prompt": format!("Scout task #{n}: inventory the RAM workspace and report file counts."),
                "background": true
            })
        },
        build_output: |_args| {
            "subagent launched in background; report will arrive via agent_send".to_string()
        },
    },
];

/// Total template count (asserted ≥ 40 by tests — "数十个模板").
pub const TEMPLATE_COUNT: usize =
    USER_PROMPTS.len() + REASONING.len() + FINAL_TEXTS.len() + TOOL_TEMPLATES.len();
