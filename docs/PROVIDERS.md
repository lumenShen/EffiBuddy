# 供应商、模型与 API Key

EffiBuddy 的模型注册表保存在 `<数据目录>/providers.json`，在设置 →「模型」中编辑。预置供应商开箱即用，填 Key 即可；每个供应商可以选择三种流式协议之一：

| 协议 | 请求路径 | 典型供应商 |
| --- | --- | --- |
| OpenAI Chat Completions | `/chat/completions` | DeepSeek、OpenAI、Z.ai、BigModel、Kimi、OpenRouter、SiliconFlow、OpenCode Zen、OpenCode Go、Ollama |
| Anthropic Messages | `/v1/messages` | Anthropic，以及 Anthropic 兼容端点 |
| OpenAI Responses | `/responses` | OpenAI Responses API |

三种协议统一映射到 `ProviderEvent`，支持 reasoning/thinking、流式 tool-call 重组、usage 与 prompt cache 统计。

以上供应商都已预置在注册表中，也可以新增自定义供应商与模型。

## OpenCode 网关

两个网关都走 OpenAI Chat Completions：

- **Zen**：`https://opencode.ai/inference/openai/v1`（旧版 `/zen/v1` 仍可用），只需 Bearer Key；
- **Go**：`https://opencode.ai/inference/go/openai/v1`，每次请求额外要求 `x-opencode-session`，EffiBuddy 会用当前会话 ID 自动填充。自定义供应商只要 Base URL 指向 Go 网关，同样生效。

两者都会带上 `EffiBuddy/<版本>` 的 User-Agent。免费档模型只对 OpenCode 官方客户端开放，第三方客户端请使用付费模型或 Go。

## API Key 解析顺序

1. 供应商各自的 `api_key`（在设置页保存）；
2. 内置环境变量：`DEEPSEEK_API_KEY`、`OPENAI_API_KEY`、`ANTHROPIC_API_KEY`、`ZAI_API_KEY`、`BIGMODEL_API_KEY`、`MOONSHOT_API_KEY`、`OPENROUTER_API_KEY`、`SILICONFLOW_API_KEY`、`OPENCODE_API_KEY`（Zen）、`OPENCODE_GO_API_KEY`（Go）；
3. 旧版 `EFFIBUDDY_API_KEY` 以及 `<workspace>/deepseekAPIKey.env` 会迁移到 DeepSeek 供应商。

本地地址（`localhost` / `127.0.0.1`，例如 Ollama）不要求 Key。设置页中的 Key 输入框始终为空：已配置时提示「已配置 · 留空不修改」，保存时留空不会覆盖已有 Key；设置页永远不显示明文。

## 默认值

| 项 | 默认值 |
| --- | --- |
| 供应商 | `deepseek` |
| 模型 | `deepseek-flash`（上下文窗口 1,048,576） |
| 推理档位 | 供应商默认 |
| temperature | 0.2 |
| 工具审批 | 写文件 / 改文件 / shell 需要批准 |

输入框左下角的模型 chip 可以按会话切换供应商 / 模型 / 推理档位；设置页的「模型」分区管理完整的供应商 CRUD、连通性测试与「从 API 获取模型」。旧版 `config.json` 中的 Base URL / Key / Model 会在首次启动时自动迁移。
