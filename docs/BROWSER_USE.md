# Browser Use（浏览器操控）

EffiBuddy 可以通过 `browser` 工具，用 Chrome DevTools Protocol 驱动一个真实的
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

## 2. 能力开关与审批

设置 → 通用 → **浏览器操作（Chromium / DevTools）**：

| 档位 | 行为 |
| --- | --- |
| 关闭 | 不注册 `browser` 工具；输入框不显示浏览器 chip。 |
| 开启（默认） | 注册 `browser` 工具，审批交给会话权限模式。 |

补充规则：

- `browser` 是普通工具（`ToolClass::Device`），**主代理与子代理都注册**，走同一套权限矩阵；只读 profile 的子代理不注册。
- 权限模式为**计划模式**时，`navigate` / `click` / `type` / `hover` / `press_key` /
  `scroll` / `select` / `set_checked` / `new_tab` / `select_tab` / `close_tab` / `eval` /
  `upload` 直接拒绝；`snapshot` / `text` / `console` / `network` / `tabs` / `screenshot` /
  `wait_for` 本地放行。调研类任务因此依然可用。
- **完全访问模式不再单独询问**：矩阵内直接批准，主代理与子代理一致。
- 所有代理共用同一个 `BrowserRuntime`，会话按 id 隔离 snapshot/frame；并发驱动同一个浏览器时以最后到达的动作顺序为准。
- 停止运行会结束本会话的驱动并隐藏悬浮条；下一次调用仍按当前权限模式审批。

输入框不再显示浏览器的状态 chip：能力开关在设置里，是否弹审批卡由输入框内的权限模式 chip 表达，同一个状态不必说两遍。

## 3. 工具参数

单一工具 `browser`，`action` 枚举：

| action | 说明 |
| --- | --- |
| `status` | 引擎、版本、连接状态、暂停/停止、**本会话的标签页 id** 与快照情况。 |
| `navigate` | 打开网址，`wait` 可选 `load` / `dom_content_loaded` / `none`。 |
| `snapshot` | 生成 `[ref]` 快照；`mode` = `interesting` / `full` / `text`，`max_nodes` 默认 300。 |
| `text` | 只取正文文本；带 `selector` 时只读该 CSS 选择器命中的区域（`all=true` 拼接全部命中）。 |
| `click` | `ref` 点击；`text` 按可见标签点击（`exact=true` 要求完全匹配）；`selector` 按 CSS 选择器点击；都没有时用 `x`/`y`（截图像素）+ `frame_id`。 |
| `type` | `ref` + `text`，`submit=true` 时回车提交。会先全选覆盖旧值。 |
| `hover` | 悬停，用于展开菜单。 |
| `press_key` | 键位组合，如 `Return`、`ctrl+a`、`Escape`、`Page_Down`。 |
| `scroll` | `direction` + `amount`，可指定 `ref` 让滚动发生在该元素内。 |
| `select` | `<select>` 选值（value 或可见文本）。 |
| `set_checked` | 勾选/取消 `checkbox` / `radio`。 |
| `upload` | 给 `<input type=file>` 上传**工作区内**的文件。 |
| `screenshot` | `full_page`、`max_dim`（256–2048，默认 1280）。 |
| `console` | 控制台增量查询：`since` / `level` / `failed_only` / `url_contains` / `limit`。 |
| `network` | 网络请求增量查询，语义同上；`include_body=true` 时额外抓取响应体（`max_body_chars`，默认 4000）。 |
| `wait_for` | `settled` / `url_contains` / `ref_visible` / `selector_visible` / `console_error`。 |
| `wait_for_request` | 等某个 URL 正则匹配的响应结束，直接返回状态码 + 响应体（`pattern` / `method` / `since` / `timeout_ms` / `max_body_chars`）。 |
| `init_script` | 注入"每个新文档都先执行"的脚本：`op=add/set/remove/clear/list` + `script`。 |
| `editor` | 网页内代码编辑器：`op=detect/list/read/write/save`，自动识别 Monaco / CodeMirror 5 / CodeMirror 6 / Ace / textarea；`file` 指定目标、`content` 写入，`save` 触发页面保存快捷键并回读是否仍有未保存改动。 |
| `drag` | 拖动：`from_ref`/`to_ref` 或 `x`/`y` → `to_x`/`to_y`，`steps` 控制中间移动事件数（默认 8）。 |
| `tabs` / `new_tab` / `select_tab` / `close_tab` | 标签页管理；`new_tab` / `select_tab` 会把该标签页设为本会话的标签页。 |
| `eval` | 在页面域执行 JS（高危，见下）；`timeout_ms` 到点后返回**已有输出**而不是整段丢弃；`await=true`（默认）等待返回的 Promise。 |
| `end_session` | 关闭本会话的标签页并清理会话；只有最后一个会话才会关闭浏览器进程。 |
| `fresh_profile` | 用一次性 profile 重启浏览器（默认是一个共享 profile，见第 5 节）；其它会话仍持有标签页时会被拒绝。 |

### 会话与标签页

整个 agent 树共用一个浏览器进程，但**每个会话（主代理或某个子代理）绑定自己的标签页**：

- 会话第一次调用 `browser` 时自动新开一个标签页，并把它的 id 记在 `SessionRuntime.tab_id`；
- 之后每次调用都会把 DevTools 会话切回这个标签页（若被别的代理切走），所以 `[ref]` 快照、`frame_id`、`snapshot_id` 只在会话内有效、不会串台；
- `status` 返回 `tab` 字段，`tabs` 能看到所有标签页及 `active` 标记；
- `new_tab` / `select_tab` 会把选中页设为本会话的页；`close_tab` 关闭后同时解绑；
- 子代理结束后只关掉它自己的页（`end_session`），不影响主代理。

控制台/网络日志仍属于当前 attach 的页面，多个代理交替操作时只保留最后一次 attach 之后的增量。
`init_script` 注入的脚本同样按会话隔离：会话成为当前页时会重新注册，切走时不复用于别人的页。

审批也按动作分组：在审批卡片上点"本会话允许"后，同一个 `action` 的后续调用不再询问
（例如连续 40 次 `eval` 只问一次），但 `navigate`、`upload` 等其它动作仍会各自询问；
`editor` / `init_script` 进一步按 `op` 区分读与写。

### 快照与 ref 生命周期

- 运行时保留最近 4 个快照和 4 张截图，超出即淘汰。
- 动作可以带 `snapshot_id` 精确指定来自哪次快照；不带则用最新一次。
- `click` / `type` 等引用旧 ref 时返回 `NOT_FOUND` 并提示重新快照。
- 页面在快照后重渲染，元素已不在原处时返回 `NOT_INTERACTABLE`。

### 元素定位

快照脚本给每个采集到的元素打上 `data-effi-i="<n>"` 标记，ref 直接映射到这个标记。
后续动作通过标记重新查到**同一个 DOM 元素**，不靠文本或坐标猜测；标记消失说明页面
已经变了，这正是必须重新快照的信号。

### 一次调用代替一串 eval

模型最容易浪费轮次的地方，是把浏览器当成"只有 eval"的工具。下面这些动作就是为了把
"先 eval 试一下 → 再 eval 修一下"变成一次调用：

| 需求 | 以前 | 现在 |
| --- | --- | --- |
| 读结果面板 | `eval(document.querySelector("#out").innerText)` | `text` + `selector` |
| 点某一行 | snapshot → 找 ref → `click` | `click` + `text`（"提交"、"第 3 关"） |
| 等构建接口 | 反复 `eval` 轮询 | `wait_for_request` + `pattern`，直接拿响应体 |
| 下一关重新注入 `__set` / `__eval` | 每个页面手动 `eval` 一遍 | `init_script` 一次，之后每个新文档自动执行 |
| 网页 IDE 里改文件 | 十几次 `eval` 猜编辑器 API | `editor` 的 `list/read/write/save` |
| 等待异步结果 | `eval` 超时后信息全丢 | `eval` + `timeout_ms`，超时也返回已打印的 `logs` |

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
它按高危动作处理（计划模式直接拒绝，其他模式按当前权限模式审批），返回结果标记 `untrusted`。

`eval` 的 `timeout_ms`（默认 15s，上限 120s）是**工具侧预算**：到点后返回
`timed_out: true` + `logs`（本次表达式打印过的内容），页面里的 Promise 仍在继续跑。
长轮询请优先用 `wait_for_request`，它给的是结构化结果而不是日志。

### 响应体与 `wait_for_request`

日志条目本身只有方法、URL、状态码；响应体按需抓取（`Network.getResponseBody`），
最多缓存 24 条（单条上限 256k 字符），所以"看请求列表"依然便宜：

```jsonc
// network，include_body=true
{ "seq": 12, "request_id": "1234.5", "method": "POST", "url": ".../game_build",
  "status": 200, "body": "{\"ok\":true}", "body_truncated": false }

// wait_for_request，pattern="/game_build"
{ "matched": true,
  "request": { "request_id": "1234.5", "method": "POST", "url": "...", "status": 200 },
  "body": "{...}", "body_available": true }
```

- 匹配用正则（`regex` crate），`method` 可选，`since` 可跳过已经等过的条目；
- 超时会返回最近几条请求的 `METHOD URL`，方便立刻改模式；
- 引擎已丢弃响应体（重定向 / 被淘汰）时返回 `body_available=false` 并说明原因，
  而不是伪造空 body；
- 二进制响应带 `base64` 标记，提示不要当文本处理。

### 注入脚本（init script）

`init_script` 对应 Playwright 的 `addInitScript`：脚本用
`Page.addScriptToEvaluateOnNewDocument` 注册，之后该标签页的每个新文档都会先执行它。
典型用法是一次性注入辅助函数，避免每进一关就 `ReferenceError`：

```jsonc
{ "action": "init_script", "op": "add",
  "script": "globalThis.__set = (v) => { ... }; globalThis.__eval = (c) => eval(c);" }
```

- 脚本属于**当前会话**；会话切到别的标签页时注入列表随之切换（每次动作前同步，
  没有变化时不发 CDP 调用）；
- 浏览器重启或新开标签页后自动重新注册；
- 只对**之后加载的文档**生效，当前页需要 `navigate` / `reload` 才生效。

### 网页内编辑器（editor）

高频场景是"在网页 IDE 里写代码"：页面把编辑缓冲放在 Monaco / CodeMirror / Ace 里，
快照只能看到一个巨大的文本框。`editor` 把这层封装成一级工具：

| op | 行为 |
| --- | --- |
| `detect` / `list` | 识别编辑器家族，列出文件（Monaco 的 model path、CodeMirror / Ace 实例、textarea）；返回 `index` / `target` / `chars` / `dirty`。 |
| `read` | 读取目标文件（默认当前聚焦的编辑器），按 `max_chars` 截断。 |
| `write` | 用编辑器**自己的 API** 替换缓冲区（触发 React / Vue 绑定），不保存。 |
| `save` | 有 `content` 时先写入，再派发 `Ctrl+S`，400ms 后回读：`saved` / `dirty_after_save`。 |

`dirty` 由脚本记住"上次读 / 写时的缓冲"来比较，所以"保存到底成功没有"是可验证的，
不用再靠 `eval` 猜。页面里没有编辑器时返回 `kind: "none"` 并提示（IDE 在 iframe 里需要单独处理）。

### 拖动

`drag` 用真实的按下 → 多次移动 → 抬起序列，分隔条、排序列表、滑块都能用；
`from_ref` / `to_ref` 会重新解析元素的**当前**位置，`steps` 太少会被某些库当成点击。

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
--remote-debugging-port=0 --user-data-dir=<稳定 profile>
--no-first-run --no-default-browser-check --disable-sync ...
```

端口从 `<profile>/DevToolsActivePort` 读取，避免固定端口冲突。启动前清理上次崩溃遗留的
`SingletonLock`。应用退出或 `end_session` 会关闭浏览器进程。

**默认只有一份 profile**（`<数据目录>/browser/profiles/<引擎>-default`），而且它是持久的：浏览器
中途崩溃或重启后，登录态、cookie、localStorage 都还在，长任务不用从头登录。**默认不会为每次启动
新建副本**，只有下面两种情况才会多出一份：

- 模型显式请求：`browser({action:"fresh_profile"})` —— 把浏览器重启到一次性 profile（全新登录态），
  这份副本在浏览器关闭时删除，之后的启动回到共享 profile。其它会话还持有标签页时会拒绝，
  因为重启会连带关掉它们的页。
- 开发者/测试覆盖：`EFFIBUDDY_BROWSER_PROFILE_FRESH=1`（每次启动一次性目录），或用
  `EFFIBUDDY_BROWSER_PROFILE` 指向固定路径。

一次性目录是有主的、可回收的：名字形如 `<引擎>-<pid>-<时间戳>`，浏览器正常退出时整个目录会被
删除；进程被强杀（调试时最常见）留下的副本，会在**下一次启动**时被扫描回收 —— 目录名里的 pid
已经不存在的副本一律删掉，并且按实际字节数写进 `browser` 的状态 notes。共享的
`<引擎>-default`、`EFFIBUDDY_BROWSER_PROFILE` 指定的目录，以及名字不符合该形状的目录都不会被碰；
在 Linux 上用 `/proc/<pid>` 判断存活，其它平台退化成"超过 24 小时未改动才回收"。

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
- 浏览器只在**EffiBuddy 自己的 profile** 中运行，不会接管用户日常浏览器的 cookie 或登录态；
  这个 profile 跨重启保留（见第 5 节），需要时可一键换成一次性目录。
- 所有动作、错误码都会进入工具结果与 transcript，便于审计。

## 8. 开发与排查

```bash
# 单元测试（不需要浏览器）
cargo test -p effibuddy-browser --lib
cargo test -p effibuddy-agent-tools --lib browser
cargo test -p effibuddy-app --bins browser

# 真机端到端（会启动浏览器、读本地文件、监听回环端口）
cargo test -p effibuddy-browser --test live -- --ignored --nocapture
```

`tests/live.rs` 覆盖了本节的每条能力：选择器取文、按标签点击、`editor` 读写、
`init_script` 跨 reload 存活、`wait_for_request` 拿响应体、`drag`，以及超时 eval 的部分输出。

环境变量：

| 变量 | 作用 |
| --- | --- |
| `EFFIBUDDY_BROWSER_PATH` | 指定浏览器可执行文件。 |
| `EFFIBUDDY_BROWSER_PROFILE` | 固定 profile 目录（默认是持久的 `<引擎>-default`）。 |
| `EFFIBUDDY_BROWSER_PROFILE_FRESH` | 开发/测试覆盖：`1` 时每次启动用一次性 profile。模型侧的等价能力是 `fresh_profile` 动作。 |
| `EFFIBUDDY_BROWSER_ALLOW_FILE` | 允许 `file://`。 |

启动日志写在 `<数据目录>/browser/launch.log`（超过 200KB 自动清空）。

profile 目录不会无限增长：EffiBuddy 启动时和每次启动浏览器前都会扫一遍
`<数据目录>/browser/profiles/`，回收 owner 进程已消失的一次性副本（单份约 200MB、
上万个小文件，不回收的话几次调试就能堆到 GB）。想知道回收了什么，看 `browser({action:"status"})`
的 notes。

## 9. 稳定性：会话、状态与审批

长时间任务最容易死在三件事上：登录态丢了、事实记不住了、每个动作都要点一次同意。

### 登录态

profile 持久化（见第 5 节）解决"浏览器重启就要重新登录"：cookie、localStorage、
IndexedDB 都留在 EffiBuddy 自己那一份 profile 目录里，默认不会被复制出第二份。需要一次
"干净的、未登录的"浏览器时用 `browser({action:"fresh_profile"})`（用完即删，共享 profile
里的登录态不受影响）；要彻底重置共享 profile 就删掉那个目录。

### 事实：`state` 工具

`state` 是会话级 key-value 草稿板，替代"把已确认的答案、待办映射全塞进上下文"：

```jsonc
// 记下来
{ "action": "set", "key": "answer.3", "value": { "choice": "B", "why": "build passed" } }
// 需要时取回
{ "action": "get", "key": "answer.3" }
// 看看有什么
{ "action": "list" }
```

- 作用域是**当前会话**（子代理各有自己的），进程内存，`run_js` 里也能用
  （`tools.state({action: "get", key: "answer.3"})`）；
- 每个值上限 16k 字符、每个会话 200 个键——它是草稿板，不是文件系统；
- 删除会话时一并清理；
- 自动批准（没有外部副作用），只读动作声明为 `ReadOnly`。

### 轮询：`run_js` 的 `sleep` / `retry`

`run_js` 仍是同步沙箱，但现在提供：

```js
retry(() => tools.browser({ action: "wait_for_request", pattern: "/game_build" }),
      { attempts: 20, delay_ms: 2000, factor: 1.5 });
sleep(500);
poll(() => tools.state({ action: "get", key: "answer.3" }).found);
```

`sleep` 会阻塞脚本线程（有总预算，默认等于沙箱超时，上限 120s），因此"等 10 秒再问一次"
不再需要拆成多次模型调用。

### 审批

"本会话允许"按**动作**缓存（`Tool::approval_key`）：连续 40 次 `browser eval` 只问一次，
但 `navigate` / `click` / `upload` 各自独立，不会因为批准了 `eval` 就顺带放开写操作。
计划模式下仍按风险矩阵拒绝变更类动作。

## 10. 已知限制

- 采集脚本走 DOM + ARIA 属性，不是 CDP 的原生 `Accessibility` 域；对强依赖原生 AX 树的
  读屏场景覆盖较弱。
- iframe 内部元素不在主文档快照里；需要先拿到 iframe 的 URL 单独打开。`editor` 也只看主文档，
  IDE 位于 iframe 时需单独处理。
- Shadow DOM 开放模式可见，closed 模式不可见。
- 自绘 canvas 界面只能退回 `screenshot` + 坐标点击。
- 跨域 `eval` 受浏览器同源策略约束。
- `wait_for_request` 只能看到当前 attach 页面的请求；响应体被 Chromium 淘汰后
  （大文件、长时间之后）需要重新触发请求。
- `eval` 超时后页面里的 Promise 仍在运行，运行时不会强杀脚本（`Runtime.terminateExecution`
  会连页面自己的事件处理一起打断）。
- `state` 是进程内存，重启应用后清空；需要跨重启请写文件。
- 暂不支持下载拦截、文件选择框之外的系统对话框、Cookie 导入导出。
- `chrome-headless-shell` 的自动下载尚未实现（URL 形态已知，见 `discover.rs` 注释）。
