// 版本更新检测：启动时后台拉取 GitHub latest release，与本地版本比较。
use std::sync::Mutex;
use std::time::Duration;

pub const REPO_API: &str = "https://api.github.com/repos/ERX399/opencode-free-api-rust/releases/latest";
const CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60;

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
pub fn version_gt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split('.')
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

fn fetch_latest_tag() -> Option<String> {
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

/// 启动后台检测线程：启动后 5 秒首次检测，之后每 6 小时一次。
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
