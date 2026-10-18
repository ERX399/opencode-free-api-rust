// 版本更新检测：启动时后台拉取 GitHub latest release，与本地版本比较。
use std::sync::Mutex;
use ureq::ResponseExt;
use std::time::Duration;

pub const REPO_API: &str = "https://api.github.com/repos/ERX399/opencode-free-api-rust/releases/latest";
const CHECK_INTERVAL_SECS: u64 = 60 * 60;

pub static LATEST_TAG: Mutex<Option<String>> = Mutex::new(None);
/// 从 tag 提取版本号（去 v 前缀），非 vX.Y.Z 形态返回 None。
pub fn tag_version(tag: &str) -> Option<String> {
    let trimmed = tag.trim().trim_start_matches('v').trim_start_matches('V');
    let core = trimmed.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() < 2 || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    Some(core.to_owned())
}

/// 比较两个数字版本号，a > b 返回 true。
/// 先剥离 pre-release / build 后缀（如 `0.3.3-plus` -> `0.3.3`），避免 `3-plus` 被当成 0 而误判降级。
pub fn version_gt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim()
            .trim_start_matches(['v', 'V'])
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (va, vb) = (parse(a), parse(b));
    for i in 0..va.len().max(vb.len()) {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

// 兜底：API 探测失败时直接 GET releases/latest 页面，从跳转地址提取 tag。
fn fetch_latest_tag_fallback() -> Option<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .timeout_connect(Some(Duration::from_secs(8)))
        .build()
        .into();
    // 不跟随重定向，302 的 Location 就是 .../releases/tag/vX.Y.Z
    // ureq 默认跟随重定向，最终落在 .../tag/vX.Y.Z 页面，直接取最终 URL 末段。
    let response = agent
        .get("https://github.com/ERX399/opencode-free-api-rust/releases/latest")
        .call()
        .ok()?;
    let url = response.get_uri().to_string();
    let tag = url.rsplit('/').next()?.to_owned();
    if tag.starts_with('v') {
        Some(tag)
    } else {
        None
    }
}

fn fetch_latest_tag() -> Option<String> {
    fetch_latest_tag_api().or_else(fetch_latest_tag_fallback)
}

fn fetch_latest_tag_api() -> Option<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .timeout_connect(Some(Duration::from_secs(8)))
        .build()
        .into();
    let response = agent
        .get(REPO_API)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "opencode-free-api")
        .call()
        .ok()?;
    let mut body_reader = response.into_body().into_reader();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut body_reader, &mut bytes).ok()?;
    let body: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    body.get("tag_name")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn check_once(local_version: &str) {
    if let Some(tag) = fetch_latest_tag() {
        let tag = tag_version(&tag).unwrap_or(tag);
        if version_gt(&tag, local_version) {
            *LATEST_TAG.lock().expect("latest tag lock") = Some(tag.clone());
            eprintln!("\n╔══════════════════════════════════════════╗");
            eprintln!("║  发现新版本 v{tag}（当前 v{local_version}）");
            eprintln!("║  https://github.com/ERX399/opencode-free-api-rust/releases/latest");
            eprintln!("╚══════════════════════════════════════════╝\n");
        } else {
            *LATEST_TAG.lock().expect("latest tag lock") = None;
        }
    }
}

/// 启动后台检测线程：启动后 5 秒首次检测，之后每小时一次。
pub fn spawn_update_check(local_version: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        loop {
            check_once(local_version);
            std::thread::sleep(Duration::from_secs(CHECK_INTERVAL_SECS));
        }
    });
}

pub fn latest_tag() -> Option<String> {
    LATEST_TAG.lock().expect("latest tag lock").clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_suffix_does_not_regress() {
        // 0.3.3-plus 的数值部分等于 0.3.3，不应被判为低于 0.3.3
        assert!(!version_gt("0.3.3", "0.3.3-plus"));
        assert!(version_gt("0.3.4", "0.3.3-plus"));
        assert!(!version_gt("0.3.3-plus", "0.3.4"));
    }

    #[test]
    fn tag_version_strips_suffix() {
        assert_eq!(tag_version("v0.3.3-plus"), Some("0.3.3".to_owned()));
        assert_eq!(tag_version("0.3.1-fix1"), Some("0.3.1".to_owned()));
        assert_eq!(tag_version("v1.2.3"), Some("1.2.3".to_owned()));
        assert_eq!(tag_version("garbage"), None);
    }
}
