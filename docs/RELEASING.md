# 发布与更新

发布由 GitHub Actions 全自动完成（[.github/workflows/release.yml](../.github/workflows/release.yml)）：推送 `v*` tag（或在 Actions 页手动触发并填 tag）即启动 Windows + Linux + macOS 三平台构建，全部成功后自动把产物上传到 [Releases](https://github.com/lumenShen/EffiBuddy/releases)。

## 产物

| 平台 | 产物 |
| --- | --- |
| Linux x64 | `EffiBuddy-<版本>-linux-x86_64.tar.gz`（便携）+ `effibuddy_<版本>_amd64.deb`（Debian/Ubuntu）+ `effibuddy-<版本>-1*.x86_64.rpm`（Fedora/RHEL/SUSE） |
| Windows x64 | `EffiBuddy-<版本>-windows-x86_64.zip`（便携）+ `EffiBuddy-<版本>-windows-x86_64-setup.exe`（NSIS 安装包，组件页可选择开始菜单/桌面快捷方式与固定到任务栏，含卸载器） |
| macOS arm64 | `EffiBuddy-<版本>-macos-aarch64.tar.gz`（便携；未签名，首次运行需 `xattr -dr com.apple.quarantine`） |

## 随包 bash 运行时

每个包都带一份 `runtime/` bash 运行时，构建时拉取上游最新版本：

- **Linux**：`scripts/fetch-bash.sh` 从 `ftp.gnu.org` 解析最新 GNU bash，用 musl 编成静态二进制（便携包放 `runtime/bash`，deb/rpm 安装到 `/usr/lib/effibuddy/runtime/`）；
- **macOS**：同一脚本按系统库构建（`runtime/bash`）；
- **Windows**：`scripts/bundle-msys-shell.py` 读取 MSYS2 实时仓库数据库，下载当前最新的 bash 与常用工具、逐包校验 sha256 后解出 `runtime/msys/`。

`shell` 工具启动时按可执行文件位置发现这份运行时（也支持 `EFFIBUDDY_SHELL=/path/to/bash` 覆盖），找不到才回退系统 `sh -lc` / `cmd /C`。GNU bash 与 MSYS2 组件按各自许可证分发，版本与来源记录在包内的 `runtime/BASH-VERSION`（Windows 为 `runtime/msys/BASH-VERSION.txt`）。

## Windows 安装包（NSIS）

脚本在 [`installer/effibuddy.nsi`](../installer/effibuddy.nsi)，Modern UI 2 向导，页面顺序为：欢迎 → 组件 → 目录 → 安装 → 完成（可选"立即启动"）。

组件页让用户**逐项决定**快捷方式的创建，不再强制：

| 组件 | 默认 | 说明 |
| --- | --- | --- |
| EffiBuddy（必需） | 锁定 | 程序本体、`runtime\msys`、卸载器 |
| 创建开始菜单快捷方式 | 勾选 | 开始菜单目录 + 卸载入口 |
| 创建桌面快捷方式 | 勾选 | 桌面单个 `.lnk` |
| 固定到任务栏 | **不勾选** | 见下 |

安装包同时写入控制面板「程序和功能」条目（`EstimatedSize`、`DisplayIcon`、`QuietUninstallString` 等），卸载时逐项还原。

### 固定到任务栏为什么是"尽力而为"

Windows 没有受支持的"固定到任务栏"API。唯一半官方途径是 shell 的 `taskbarpin` 动词，而微软在 Windows 11 22H2+ 与多数受管设备上逐步禁用它；由于任务栏布局自 Windows 7 起存在注册表二进制块里，往 `User Pinned\TaskBar` 丢一个 `.lnk` **并不会**真的完成固定。

所以 [`installer/pin-taskbar.ps1`](../installer/pin-taskbar.ps1) 的做法是：调用动词 → 回来核实 shell 是否真的生成了固定项 → **只有核实成功才 `exit 0`**。失败一律 `exit 1`，安装器（`Section "固定到任务栏"`）只打日志、不写 `TaskbarPinned` 标记、不中断安装。已知限制：安装器以管理员身份运行，动词在提权上下文里执行，部分系统会固定到管理员配置或直接拒绝，这两种情况同样落到 `exit 1`。

卸载由 [`installer/unpin-taskbar.ps1`](../installer/unpin-taskbar.ps1) 处理，同样是尽力而为，且**永远 `exit 0`**——残留的固定图标绝不能卡住卸载。

两个 `.ps1` 都必须保持 **ASCII-only**：`powershell.exe` 5.1 读无 BOM 的 `.ps1` 时按 ANSI 解码，中文会变乱码。CI 里有一个校验步骤会解析语法并拦下非 ASCII 字符。

本地编译安装包（需先构建 `target\release\effibuddy.exe` 与 `runtime\msys`）：

```bash
makensis /DVERSION=0.2.0 /DVI_VERSION=0.2.0.0 /DSRCDIR=<仓库根> /DOUTFILE=setup.exe installer/effibuddy.nsi
```

`VI_VERSION` 必须是四段纯数字（`VIProductVersion` 的限制）；CI 会自动把 `0.2.0-beta.1` 这类版本裁成 `0.2.0.0`。

## 版本与触发

tag 必须与 `Cargo.toml` 的 workspace 版本一致，否则构建直接失败（单一版本源）：

```bash
git tag v0.2.0 && git push origin v0.2.0   # 触发发布
```

## 应用内更新检查

`crates/app/src/update.rs`：启动时和「设置 → 关于」手动查询 GitHub `releases/latest`；发现更新时聊天区顶部显示提示条（可跳过该版本或仅本次隐藏），「关于」页展示当前/最新版本并可直达发布页。

安装包资产按平台与架构匹配：Windows x64 取 setup/zip，macOS 按 `std::env::consts::ARCH` 取同架构 tar.gz，Linux 读 `/etc/os-release` 判断 rpm/deb 偏好（识别不到时 deb 优先，退而求其次是同架构便携包）。自动检查可在配置里关（`update_check_enabled`），已跳过的版本记录在 `update_skip_version`。仓库未发布 release 时静默不做任何提示。
