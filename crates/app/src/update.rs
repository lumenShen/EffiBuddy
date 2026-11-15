//! In-app update check against this repository's GitHub Releases.
//!
//! The release workflow (`.github/workflows/release.yml`) publishes build
//! artifacts on every `v*` tag; this module asks GitHub for the latest
//! release and compares its tag with the running binary's version. It never
//! downloads anything by itself — the user is pointed at the release page or
//! the prebuilt asset matching the current platform.

use serde::Deserialize;

/// `owner/repo` the client polls. Overridable through `EFFIBUDDY_UPDATE_REPO`
/// so local runs and tests can point at a sandbox repository.
pub const UPDATE_REPO: &str = "lumenShen/EffiBuddy";

/// The running binary's version (Cargo workspace version).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// UI-side state of the update check. Lives on `AppView`; the banner renders
/// while a comparable, non-skipped newer release is known.
#[derive(Default)]
pub struct UpdateState {
    /// Latest release known so far (`None` = not yet checked / none published).
    pub latest: Option<UpdateInfo>,
    /// A request is in flight (startup or the About-page button).
    pub checking: bool,
    /// Last failure, shown only in the About page — never as a banner.
    pub error: Option<String>,
    /// Set when the user closes the banner; reset by the next manual check.
    pub dismissed: bool,
}

/// Effective `owner/repo`, honoring the dev override.
pub fn update_repo() -> String {
    std::env::var("EFFIBUDDY_UPDATE_REPO")
        .ok()
        .map(|repo| repo.trim().to_string())
        .filter(|repo| !repo.is_empty())
        .unwrap_or_else(|| UPDATE_REPO.to_string())
}

/// The latest published release, shaped for the UI.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateInfo {
    /// Release tag, e.g. `v0.2.0`.
    pub version: String,
    /// Human-facing release page.
    pub html_url: String,
    /// Prebuilt asset for the current platform, if the release carries one.
    pub asset_url: Option<String>,
}

#[derive(Deserialize)]
struct ReleasePayload {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

pub fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!("effibuddy/{}", current_version()))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| format!("无法创建 HTTP 客户端: {error}"))
}

/// Fetch the latest release. `Ok(None)` means the repository has no published
/// release yet — that is not an error, just nothing to compare against.
pub async fn fetch_latest(client: &reqwest::Client) -> Result<Option<UpdateInfo>, String> {
    let repo = update_repo();
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| format!("网络请求失败: {error}"))?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(if status.as_u16() == 403 {
            "GitHub API 限流（稍后再试）".to_string()
        } else {
            format!("GitHub API 返回 {status}")
        });
    }

    let payload: ReleasePayload = response
        .json()
        .await
        .map_err(|error| format!("解析响应失败: {error}"))?;
    let version = payload.tag_name.trim().to_string();
    if version.is_empty() {
        return Ok(None);
    }
    let html_url = if payload.html_url.trim().is_empty() {
        format!("https://github.com/{repo}/releases/latest")
    } else {
        payload.html_url
    };
    let asset_url = pick_asset(&payload.assets);
    Ok(Some(UpdateInfo {
        version,
        html_url,
        asset_url,
    }))
}

/// Prefer the installer for the current platform, fall back to the portable
/// archive. Names come from the release workflow, so the suffixes are stable.
/// On Linux the installer preference follows the distro family (rpm vs deb).
fn pick_asset(assets: &[ReleaseAsset]) -> Option<String> {
    pick_asset_for(cfg!(target_os = "windows"), linux_prefers_rpm(), assets)
}

/// Fedora/RHEL/SUSE 安装 .rpm，Debian/Ubuntu 安装 .deb；识别不到发行版时
/// 以 deb 优先（桌面 Linux 用户的大多数）。
fn linux_prefers_rpm() -> bool {
    let Ok(content) = std::fs::read_to_string("/etc/os-release") else {
        return false;
    };
    os_release_prefers_rpm(&content)
}

fn os_release_prefers_rpm(content: &str) -> bool {
    const RPM_IDS: [&str; 7] = ["fedora", "rhel", "centos", "rocky", "almalinux", "sles", "opensuse"];
    content.lines().any(|line| {
        line.strip_prefix("ID=")
            .map(|id| id.trim().trim_matches('"'))
            .is_some_and(|id| RPM_IDS.iter().any(|known| id.starts_with(known)))
    })
}

fn is_windows_setup(name: &str) -> bool {
    name.ends_with("-windows-x86_64-setup.exe")
}

fn is_windows_zip(name: &str) -> bool {
    name.ends_with("-windows-x86_64.zip")
}

fn is_deb(name: &str) -> bool {
    name.ends_with("_amd64.deb")
}

fn is_rpm(name: &str) -> bool {
    name.ends_with(".x86_64.rpm")
}

fn is_linux_tar(name: &str) -> bool {
    name.ends_with("-linux-x86_64.tar.gz")
}

fn pick_asset_for(windows: bool, prefer_rpm: bool, assets: &[ReleaseAsset]) -> Option<String> {
    let matchers: Vec<fn(&str) -> bool> = if windows {
        vec![is_windows_setup, is_windows_zip]
    } else if prefer_rpm {
        vec![is_rpm, is_deb, is_linux_tar]
    } else {
        vec![is_deb, is_rpm, is_linux_tar]
    };
    for matcher in &matchers {
        if let Some(asset) = assets.iter().find(|asset| matcher(&asset.name)) {
            return Some(asset.browser_download_url.clone());
        }
    }
    None
}

/// `v0.2.0`, `0.2.0`, `0.2.0-rc.1` and `0.2.0+build` all parse; anything else
/// (nightly tags, prose) yields `None` and is treated as "not comparable".
pub fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let version = version.trim();
    let version = version.strip_prefix('v').unwrap_or(version);
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Semver-style triple comparison; incomparable versions never count as
/// "newer" so a stray tag can't nag users.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.com/{name}"),
        }
    }

    #[test]
    fn parse_version_handles_tags() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version(" v1.10.3 "), Some((1, 10, 3)));
        assert_eq!(parse_version("0.2.0-rc.1"), Some((0, 2, 0)));
        assert_eq!(parse_version("0.2.0+build.5"), Some((0, 2, 0)));
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version("v1"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn is_newer_compares_triples() {
        assert!(is_newer("0.1.0", "v0.2.0"));
        assert!(is_newer("0.1.9", "0.2.0"));
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(!is_newer("0.2.0", "v0.2.0"));
        assert!(!is_newer("0.2.1", "v0.2.0"));
        // Prerelease of the same triple is not newer.
        assert!(!is_newer("0.2.0", "0.2.0-rc.1"));
        // Incomparable tags never claim an update.
        assert!(!is_newer("0.1.0", "nightly"));
    }

    #[test]
    fn pick_asset_prefers_installer_then_archive() {
        let assets = vec![
            asset("EffiBuddy-0.2.0-linux-x86_64.tar.gz"),
            asset("effibuddy_0.2.0_amd64.deb"),
            asset("effibuddy-0.2.0-1.x86_64.rpm"),
            asset("EffiBuddy-0.2.0-windows-x86_64.zip"),
            asset("EffiBuddy-0.2.0-windows-x86_64-setup.exe"),
            asset("source.tar.gz"),
        ];
        let windows = pick_asset_for(true, false, &assets);
        assert_eq!(
            windows.as_deref(),
            Some("https://example.com/EffiBuddy-0.2.0-windows-x86_64-setup.exe")
        );
        let deb_first = pick_asset_for(false, false, &assets);
        assert_eq!(
            deb_first.as_deref(),
            Some("https://example.com/effibuddy_0.2.0_amd64.deb")
        );
        let rpm_first = pick_asset_for(false, true, &assets);
        assert_eq!(
            rpm_first.as_deref(),
            Some("https://example.com/effibuddy-0.2.0-1.x86_64.rpm")
        );
    }

    #[test]
    fn pick_asset_falls_back_to_archive() {
        let assets = vec![asset("EffiBuddy-0.2.0-windows-x86_64.zip")];
        assert!(pick_asset_for(true, false, &assets).is_some());
        assert!(pick_asset_for(false, false, &assets).is_none());
        // 只有 rpm 可用时，deb 优先的发行版也会退而求其次。
        let rpm_only = vec![asset("effibuddy-0.2.0-1.fc45.x86_64.rpm")];
        assert!(pick_asset_for(false, false, &rpm_only).is_some());
    }

    #[test]
    fn os_release_detects_rpm_family() {
        assert!(os_release_prefers_rpm(
            "NAME=\"Fedora Linux\"\nID=fedora\nID_LIKE=\"rhel\"\n"
        ));
        assert!(os_release_prefers_rpm(
            "NAME=\"openSUSE Tumbleweed\"\nID=opensuse-tumbleweed\n"
        ));
        assert!(!os_release_prefers_rpm(
            "PRETTY_NAME=\"Ubuntu 24.04\"\nID=ubuntu\nID_LIKE=debian\n"
        ));
        assert!(!os_release_prefers_rpm("ID=debian\n"));
        // ID_LIKE 不参与判定（发行版以 ID 为准）。
        assert!(!os_release_prefers_rpm("ID=arch\nID_LIKE=\"rhel fedora\"\n"));
    }

    #[test]
    fn update_repo_env_override() {
        // SAFETY: single-threaded use of this variable within the test suite.
        unsafe {
            std::env::set_var("EFFIBUDDY_UPDATE_REPO", "demo/sandbox");
        }
        assert_eq!(update_repo(), "demo/sandbox");
        unsafe {
            std::env::set_var("EFFIBUDDY_UPDATE_REPO", "   ");
        }
        assert_eq!(update_repo(), UPDATE_REPO);
        unsafe {
            std::env::remove_var("EFFIBUDDY_UPDATE_REPO");
        }
        assert_eq!(update_repo(), UPDATE_REPO);
    }
}
