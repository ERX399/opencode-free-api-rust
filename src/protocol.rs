// OpenCode V2 上游协议：会话 ID、请求头、门禁工具、节点故障转移。
use std::{
    collections::HashMap,
    env,
    io::Read,
    sync::Mutex,
    time::Duration,
};

use serde_json::Value;
use tiny_http::{Header, Request as HttpRequest};
use ureq::Agent;

pub type ResponseBox = tiny_http::ResponseBox;
pub type UpstreamResponse = http::Response<Vec<u8>>;

#[derive(Debug)]
pub struct UpstreamError {
    pub status: u16,
    pub message: String,
    pub body: Vec<u8>,
    pub retry_after: Option<String>,
    pub node: Option<String>,
}

// 节点限流冷却表：记录每个上游节点在哪个时间戳前应跳过（unix 毫秒）。
pub static NODE_COOLDOWN: std::sync::LazyLock<Mutex<HashMap<String, u64>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));


pub fn node_cooldown_until(node: &str) -> Option<u64> {
    NODE_COOLDOWN
        .lock()
        .expect("cooldown lock")
        .get(node)
        .copied()
}

pub fn set_node_cooldown(node: &str, until_ms: u64) {
    NODE_COOLDOWN
        .lock()
        .expect("cooldown lock")
        .insert(node.to_owned(), until_ms);
}

pub fn clear_node_cooldown(node: &str) {
    NODE_COOLDOWN
        .lock()
        .expect("cooldown lock")
        .remove(node);
}

// 解析上游 Retry-After 头（秒数或 HTTP 日期），返回 unix 毫秒到期时间。
pub fn parse_retry_after(value: &str) -> Option<u64> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        return Some(now + seconds.saturating_mul(1000));
    }
    None
}

pub const MAX_BODY_BYTES: u64 = 64 << 20;
// V1 旧网关与 V2 Console 网关共享同一批免费模型与免费层限流，双节点互为故障转移。
pub const DEFAULT_NODES: &[&str] = &[
    "https://opencode.ai/inference/v1",
    "https://opencode.ai/zen/v1",
];
pub const DEFAULT_USER_AGENT: &str = "opencode/latest/2.0.29/cli";
pub const SESSION_TTL_MS: u64 = 30 * 60 * 1000;
pub const ID_CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
// 上游按 x-opencode-session 归并会话；同一使用者复用同一个 session 直到过期。
pub static SESSION: Mutex<Option<(u64, String)>> = Mutex::new(None);

pub fn client(timeout: Duration, connect_timeout: Duration) -> Agent {
    Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(connect_timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

// session ID 格式：`ses_` + descending() 时间戳低 48 位 hex(12) + 14 位 base62。
// 上游会校验首 12 hex 是 ~(timestamp*0x1000+counter)，乱填一律 403 FreeTier。
pub fn opencode_session_id(timestamp_ms: u64) -> String {
    let value = !((timestamp_ms as u128) * 0x1000 + 1);
    let mut time = String::with_capacity(12);
    for shift in (0..6).rev() {
        let byte = ((value >> (shift * 8)) & 0xff) as u8;
        time.push_str(&format!("{byte:02x}"));
    }
    // hex 需小写；上面 format 已保证。再补 14 位随机 base62。
    let mut seed = timestamp_ms ^ 0x9E37_79B9_7F4A_7C15;
    let mut random = String::with_capacity(14);
    for _ in 0..14 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let index = ((seed >> 33) % ID_CHARS.len() as u64) as usize;
        random.push(ID_CHARS[index] as char);
    }
    format!("ses_{time}{random}")
}

pub fn current_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut guard = SESSION.lock().expect("session lock");
    if let Some((at, id)) = guard.as_ref() {
        if now.saturating_sub(*at) < SESSION_TTL_MS {
            return id.clone();
        }
    }
    let id = opencode_session_id(now);
    *guard = Some((now, id.clone()));
    id
}

pub fn add_headers<B>(
    mut builder: ureq::RequestBuilder<B>,
    accept: Option<&str>,
    token: &Option<String>,
) -> ureq::RequestBuilder<B> {
    builder = add_token(builder, token);
    if let Some(accept) = accept {
        builder = builder.header("Accept", accept);
    }
    builder
}

// V2 免费层会话关联头。Authorization 优先使用显式 token，否则固定 Bearer public。
pub fn add_v2_session_headers<B>(
    mut builder: ureq::RequestBuilder<B>,
    token: &Option<String>,
) -> ureq::RequestBuilder<B> {
    builder = builder
        .header("x-opencode-client", "cli")
        .header("x-opencode-project", "global");
    let session = current_session_id();
    builder = builder
        .header("x-opencode-session", &session)
        .header("x-opencode-session-id", &session)
        .header("x-session-affinity", &session)
        .header("X-Session-Id", &session);
    if token.is_none() {
        builder = builder.header("Authorization", "Bearer public");
    }
    builder
}

pub fn add_token<B>(
    mut builder: ureq::RequestBuilder<B>,
    token: &Option<String>,
) -> ureq::RequestBuilder<B> {
    builder = builder.header("User-Agent", DEFAULT_USER_AGENT);
    if let Some(token) = token {
        builder = builder.header("Authorization", &format!("Bearer {token}"));
    }
    builder
}

// 上游免费层门禁：请求必须携带内置 read 工具（名字与参数 schema 均需一致），缺 read 一律 403。
pub fn gate_tool() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "read",
            "description": "Reserved for the host runtime; do not call or select this function. Use tools explicitly supplied by the user instead.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File or directory to read"},
                    "offset": {"type": "integer", "minimum": 0, "description": "Starting line or directory entry (1-based)"},
                    "limit": {"type": "integer", "minimum": 0, "description": "Maximum lines or directory entries to read"}
                },
                "required": ["path"],
                "additionalProperties": false
            },
            "strict": false
        }
    })
}

pub fn read_body(body: ureq::Body) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut reader = body.into_reader();
    let _ = reader.read_to_end(&mut bytes);
    bytes
}

pub fn read_body_response(response: http::Response<ureq::Body>) -> UpstreamResponse {
    let (parts, body) = response.into_parts();
    let body = read_body(body);
    http::Response::from_parts(parts, body)
}

pub fn split_nodes(value: &str) -> Vec<String> {
    let mut nodes: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|node| !node.is_empty())
        .map(|node| node.trim_end_matches('/').to_owned())
        .filter(|node| !node.is_empty())
        .collect();
    nodes.sort();
    nodes.dedup();
    nodes
}

pub fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_owned())
}

pub fn env_flag(name: &str) -> bool {
    env_flag_or(name, false)
}

pub fn env_flag_or(name: &str, fallback: bool) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(fallback)
}

pub fn env_seconds(name: &str, fallback: u64) -> Duration {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(fallback))
}

// 从收到的请求里读单个响应头（tiny_http 的 Header 需要字符级比对）。
pub fn request_header(request: &HttpRequest, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
        .and_then(|header| Some(header.value.to_string()))
}

pub fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static header should be valid")
}


