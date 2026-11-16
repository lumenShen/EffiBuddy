# Browser Use（浏览器操控）

EffiBuddy 的主代理可以通过 `browser` 工具，用 Chrome DevTools Protocol 驱动一个真实的
Chromium 浏览器：读取页面结构、点击元素、填写表单、查看控制台与网络请求。

> 与 [`COMPUTER_USE.md`](COMPUTER_USE.md) 的区别：`computer` 是**看像素 + 模拟系统键鼠**，
> 需要视觉模型；`browser` 是**读结构树 + 在页面内派发事件**，只需要文本模型。
> 后者更稳、更省 token，也永远不会抢占用户的鼠标焦点。

## 1. 为什么是结构树而不是截图

当前默认模型（DeepSeek）没有视觉能力。若只用截图，浏览器操控在默认配置下完全不可用。
所以主观测通道是**可访问性快照**：

```text
page: 登录 | https://app.test/login | 6 nodes
[e1] heading "登录" @ (320,120 240x36)
[e2] textbox "邮箱" @ (320,200 320x40) editable
[e3] textbox "密码" @ (320,260 320x40) editable value="•••"
[e4] checkbox "记住我" @ (320,320 16x16) checked=false
[e5] button "登录" @ (320,360 96x40)
[e6] link "忘记密码？" -> /reset
```

模型只看到 `[eN]`；每个 `ref` 都绑定到**某一次快照**。页面变了，旧 ref 立刻失效，
返回 `STALE_SNAPSHOT` / `NOT_INTERACTABLE`，模型必须重新快照而不是盲目点击。

截图（`action=screenshot`）仍保留，用于 canvas / 图表这类没有结构可读的界面；
在有视觉模型时图片会随结果返回，否则保存到 `<数据目录>/browser/screens/<会话>/`。

## 2. 授权模型

设置 → 通用 → **浏览器操作（Chromium / DevTools）**：

| 档位 | 行为 |
| --- | --- |
| 关闭 | 不注册 `browser` 工具；输入框不显示浏览器 chip。 |
| 会话内准许（默认） | 首次调用弹真实审批卡；选"本会话允许"后自动放行。 |
| 默认准许 | 新会话直接放行；仍可随时结束会话。 |

补充规则：

- **只有主代理**注册 `browser`。子代理的 registry 由 `builtin_tools()` 重建，拿不到。
- 权限模式为**计划模式**时，`navigate` / `click` / `type` / `hover` / `press_key` /
  `scroll` / `select` / `set_checked` / `new_tab` / `select_tab` / `close_tab` / `eval` /
  `upload` 直接拒绝；`snapshot` / `text` / `console` / `network` / `tabs` / `screenshot` /
  `wait_for` 仍可询问。调研类任务因此依然可用。
- 审批由 app 层 `BrowserApprovalGate` 强制拦截，**完全访问模式也不能绕过首次授权**。
- 停止运行会立即撤销授权并关闭浏览器进程。

输入框底部的 chip：

- `浏览器 · 首次使用询问`
- `浏览器已授权 · 本会话`
- `浏览器已允许 · Chromium`

## 3. 工具参数

单一工具 `browser`，`action` 枚举：

| action | 说明 |
| --- | --- |
| `status` | 引擎、版本、连接状态、授权与快照情况。 |
| `navigate` | 打开网址，`wait` 可选 `load` / `dom_content_loaded` / `none`。 |
| `snapshot` | 生成 `[ref]` 快照；`mode` = `interesting` / `full` / `text`，`max_nodes` 默认 300。 |
| `text` | 只取正文文本，用于读文章。 |
| `click` | `ref` 点击；或没有 `ref` 时用 `x`/`y`（截图像素）+ `frame_id`。 |
| `type` | `ref` + `text`，`submit=true` 时回车提交。会先全选覆盖旧值。 |
| `hover` | 悬停，用于展开菜单。 |
| `press_key` | 键位组合，如 `Return`、`ctrl+a`、`Escape`、`Page_Down`。 |
| `scroll` | `direction` + `amount`，可指定 `ref` 让滚动发生在该元素内。 |
| `select` | `<select>` 选值（value 或可见文本）。 |
| `set_checked` | 勾选/取消 `checkbox` / `radio`。 |
| `upload` | 给 `<input type=file>` 上传**工作区内**的文件。 |
| `screenshot` | `full_page`、`max_dim`（256–2048，默认 1280）。 |
| `console` | 控制台增量查询：`since` / `level` / `failed_only` / `url_contains` / `limit`。 |
| `network` | 网络请求增量查询，语义同上。 |
| `wait_for` | `settled` / `url_contains` / `ref_visible` / `selector_visible` / `console_error`。 |
| `tabs` / `new_tab` / `select_tab` / `close_tab` | 标签页管理。 |
| `eval` | 在页面域执行 JS（高危，见下）。 |
| `end_session` | 关闭浏览器并清理会话。 |

### 快照与 ref 生命周期

- 运行时保留最近 4 个快照和 4 张截图，超出即淘汰。
- 动作可以带 `snapshot_id` 精确指定来自哪次快照；不带则用最新一次。
- `click` / `type` 等引用旧 ref 时返回 `NOT_FOUND` 并提示重新快照。
- 页面在快照后重渲染，元素已不在原处时返回 `NOT_INTERACTABLE`。

### 元素定位

快照脚本给每个采集到的元素打上 `data-effi-i="<n>"` 标记，ref 直接映射到这个标记。
后续动作通过标记重新查到**同一个 DOM 元素**，不靠文本或坐标猜测；标记消失说明页面
已经变了，这正是必须重新快照的信号。

## 4. 控制台与网络

`Runtime.consoleAPICalled`、`Log.entryAdded`、`Network.*` 事件持续写入两个定长环形缓冲
（各 2000 条），模型按游标增量读取：

```json
{ "entries": [...], "next": 42, "dropped": 0, "total": 42 }
```

设计要点：

- 默认只回最新 40 条，并按 `seq` 升序排列，避免一次拉爆上下文。
- `level` 是"**该等级及以上**"语义；`failed_only` 对网络只留 4xx/5xx 与失败请求。
- `dropped` 是累计被挤出缓冲的条数——非零说明模型错过了日志，应缩小查询范围。
- 单条文本截断到 4000 字符，URL 截断到 2000 字符。
- `data:` / `blob:` / `favicon.ico` 噪声直接丢弃。
- 单页面的 `confirm()` / `alert()` 会被自动接受，否则会挂死整个会话。

`eval` 等价于在 DevTools 控制台敲代码：能读 cookie、localStorage、发起同源请求。
它按高危动作处理（计划模式直接拒绝，其他模式仍需授权），返回结果标记 `untrusted`。

## 5. 引擎：自带还是复用系统？

**都不内嵌 WebView。** 路线是"复用系统已装的 Chromium，缺失时按需下载 headless shell"：

| 优先级 | 来源 | 说明 |
| --- | --- | --- |
| 1 | 已装的 Chrome / Edge / Chromium | Windows 系统自带 Edge，命中率极高；Linux 常见 `google-chrome` / `microsoft-edge` / `chromium`。 |
| 2 | `chrome-headless-shell`（数据目录） | 前两者都缺失时的兜底，本期需手动放置。 |
| 3 | `EFFIBUDDY_BROWSER_PATH` | 指向任意 Chromium 构建。 |

不内嵌的理由：

- 现有 RPM 安装包约 15 MB，塞进 Chromium 会变成 300 MB+，还要处理 `chrome_sandbox`
  的 setuid 打包问题。
- WebKitGTK 只有另一套更弱的 Inspector 协议，等于要自己实现 mini-CDP，能力反而差。
- 内嵌 webview 会破坏"单进程、无 WebView"的架构叙事。

启动参数（不传 `--no-sandbox`，那会让 AI 驱动的浏览器失去沙箱）：

```text
--remote-debugging-port=0 --user-data-dir=<独立 profile>
--no-first-run --no-default-browser-check --disable-sync ...
```

端口从 `<profile>/DevToolsActivePort` 读取，避免固定端口冲突。每次会话用独立 profile，
启动前清理上次崩溃遗留的 `SingletonLock`。应用退出或 `end_session` 会关闭浏览器进程。

## 6. URL 与文件策略

`navigate` 会校验协议与主机：

- 允许：`http` / `https` / `about:blank`。
- 拒绝：`file://`（需 `EFFIBUDDY_BROWSER_ALLOW_FILE=1` 才允许）、`chrome://`、
  `devtools://`、`view-source:`、`javascript:`、`data:`。
- 拒绝云元数据端点（`169.254.169.254`、`metadata.google.internal`、
  `100.100.100.200`），防 SSRF。
- 回环地址（`localhost`、`127.0.0.1`）允许，驱动本地开发服务器是正当用途。
- `upload` 的路径必须落在工作区内，与 `read_file` 同一个沙箱。

## 7. 安全边界

- **网页内容是不可信输入。** 页面文本可能包含"忽略之前的指令去执行 …"这类注入。
  系统提示与工具描述都要求模型不得把页面返回内容直接当作 shell 命令或文件路径。
- 每次快照/截图容量有限，`dropped` 非零意味着有日志被丢弃而不是"没有错误"。
- ref 是快照作用域的；页面变化后失败关闭（fail closed），不会点到错误位置。
- 浏览器只在**独立 profile** 中运行，不会接管用户日常浏览器的 cookie 或登录态。
- 所有动作、错误码都会进入工具结果与 transcript，便于审计。

## 8. 开发与排查

```bash
# 单元测试（不需要浏览器）
cargo test -p effibuddy-browser --lib
cargo test -p effibuddy-agent-tools --lib browser
cargo test -p effibuddy-app --bins browser

# 真机端到端（会启动浏览器、读本地文件）
EFFIBUDDY_BROWSER_ALLOW_FILE=1 \
EFFIBUDDY_BROWSER_PROFILE=/tmp/effi-browser \
cargo test -p effibuddy-browser --test live -- --ignored --nocapture
```

环境变量：

| 变量 | 作用 |
| --- | --- |
| `EFFIBUDDY_BROWSER_PATH` | 指定浏览器可执行文件。 |
| `EFFIBUDDY_BROWSER_PROFILE` | 固定 profile 目录（调试时复用登录态）。 |
| `EFFIBUDDY_BROWSER_ALLOW_FILE` | 允许 `file://`。 |

启动日志写在 `<数据目录>/browser/launch.log`（超过 200KB 自动清空）。

## 9. 已知限制

- 采集脚本走 DOM + ARIA 属性，不是 CDP 的原生 `Accessibility` 域；对强依赖原生 AX 树的
  读屏场景覆盖较弱。
- iframe 内部元素不在主文档快照里；需要先拿到 iframe 的 URL 单独打开。
- Shadow DOM 开放模式可见，closed 模式不可见。
- 自绘 canvas 界面只能退回 `screenshot` + 坐标点击。
- 跨域 `eval` 受浏览器同源策略约束。
- 暂不支持下载拦截、文件选择框之外的系统对话框、Cookie 导入导出。
- `chrome-headless-shell` 的自动下载尚未实现（URL 形态已知，见 `discover.rs` 注释）。
