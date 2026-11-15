# 发布与更新

发布由 GitHub Actions 全自动完成（[.github/workflows/release.yml](../.github/workflows/release.yml)）：推送 `v*` tag（或在 Actions 页手动触发并填 tag）即启动 Windows + Linux + macOS 三平台构建，全部成功后自动把产物上传到 [Releases](https://github.com/lumenShen/EffiBuddy/releases)。

## 产物

| 平台 | 产物 |
| --- | --- |
| Linux x64 | `EffiBuddy-<版本>-linux-x86_64.tar.gz`（便携）+ `effibuddy_<版本>_amd64.deb`（Debian/Ubuntu）+ `effibuddy-<版本>-1*.x86_64.rpm`（Fedora/RHEL/SUSE） |
| Windows x64 | `EffiBuddy-<版本>-windows-x86_64.zip`（便携）+ `EffiBuddy-<版本>-windows-x86_64-setup.exe`（NSIS 安装包，含开始菜单/桌面快捷方式与卸载器） |
| macOS arm64 | `EffiBuddy-<版本>-macos-aarch64.tar.gz`（便携；未签名，首次运行需 `xattr -dr com.apple.quarantine`） |

## 随包 bash 运行时

每个包都带一份 `runtime/` bash 运行时，构建时拉取上游最新版本：

- **Linux**：`scripts/fetch-bash.sh` 从 `ftp.gnu.org` 解析最新 GNU bash，用 musl 编成静态二进制（便携包放 `runtime/bash`，deb/rpm 安装到 `/usr/lib/effibuddy/runtime/`）；
- **macOS**：同一脚本按系统库构建（`runtime/bash`）；
- **Windows**：`scripts/bundle-msys-shell.py` 读取 MSYS2 实时仓库数据库，下载当前最新的 bash 与常用工具、逐包校验 sha256 后解出 `runtime/msys/`。

`shell` 工具启动时按可执行文件位置发现这份运行时（也支持 `EFFIBUDDY_SHELL=/path/to/bash` 覆盖），找不到才回退系统 `sh -lc` / `cmd /C`。GNU bash 与 MSYS2 组件按各自许可证分发，版本与来源记录在包内的 `runtime/BASH-VERSION`（Windows 为 `runtime/msys/BASH-VERSION.txt`）。

## 版本与触发

tag 必须与 `Cargo.toml` 的 workspace 版本一致，否则构建直接失败（单一版本源）：

```bash
git tag v0.2.0 && git push origin v0.2.0   # 触发发布
```

## 应用内更新检查

`crates/app/src/update.rs`：启动时和「设置 → 关于」手动查询 GitHub `releases/latest`；发现更新时聊天区顶部显示提示条（可跳过该版本或仅本次隐藏），「关于」页展示当前/最新版本并可直达发布页。

安装包资产按平台与架构匹配：Windows x64 取 setup/zip，macOS 按 `std::env::consts::ARCH` 取同架构 tar.gz，Linux 读 `/etc/os-release` 判断 rpm/deb 偏好（识别不到时 deb 优先，退而求其次是同架构便携包）。自动检查可在配置里关（`update_check_enabled`），已跳过的版本记录在 `update_skip_version`。仓库未发布 release 时静默不做任何提示。
