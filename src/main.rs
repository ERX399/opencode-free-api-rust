use std::{
    collections::HashMap,
    env,
    io::{Cursor, Read},
    net::UdpSocket,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::blocking::{Client, Response as UpstreamResponse};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request as HttpRequest, Response, Server, StatusCode};

type ResponseBox = tiny_http::ResponseBox;

const MAX_BODY_BYTES: u64 = 64 << 20;
const DEFAULT_NODE: &str = "https://opencode.ai/zen/v1";
const EXTRA_FREE_MODELS: &[&str] = &["big-pickle"];
const VERSION: &str = env!("CARGO_PKG_VERSION");
const INDEX_HTML: &str = include_str!("../static/index.html");

#[derive(Clone)]
struct App {
    nodes: Vec<String>,
    token: Option<String>,
    auth_token: Option<String>,
    strip_free: bool,
    free_map: Arc<Mutex<HashMap<String, String>>>,
    client: Client,
    stream_client: Client,
    requests: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
    port: String,
    local_ip: String,
    public_ip: Option<String>,
    started: u64,
}

fn main() {
    setup_console_utf8();
    let host = env_or("HOST", "0.0.0.0");
    let port = env_or("PORT", "8788");
    let address = format!("{host}:{port}");
    let connect_timeout = env_seconds("CONNECT_TIMEOUT", 10);
    let nodes = split_nodes(&env::var("NODES").unwrap_or_default());
    let nodes = if nodes.is_empty() {
        vec![DEFAULT_NODE.to_owned()]
    } else {
        nodes
    };
    let local_ip = primary_ip().unwrap_or_else(|| "127.0.0.1".to_owned());
    let public_ip = public_ip();
    let app = App {
        nodes,
        token: env::var("API_TOKEN")
            .ok()
            .filter(|value| !value.is_empty()),
        auth_token: env::var("AUTH_TOKEN")
            .ok()
            .filter(|value| !value.is_empty()),
        strip_free: env_flag("STRIP_FREE"),
        free_map: Arc::new(Mutex::new(HashMap::new())),
        client: client(
            env_seconds("UPSTREAM_TIMEOUT", 90),
            connect_timeout,
        ),
        stream_client: client(env_seconds("STREAM_TIMEOUT", 1800), connect_timeout),
        requests: Arc::new(AtomicU64::new(0)),
        failures: Arc::new(AtomicU64::new(0)),
        port,
        local_ip,
        public_ip,
        started: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let workers = env::var("WORKERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .max(1);

    eprintln!(
        "正在监听 {address}，上游节点 {} 个，工作线程 {workers} 个",
        app.nodes.len()
    );
    eprintln!("  控制台: http://127.0.0.1:{}/", app.port);
    eprintln!("  本机:   http://127.0.0.1:{}/v1", app.port);
    eprintln!("  局域网: http://{}:{}/v1", app.local_ip, app.port);
    if let Some(ip) = &app.public_ip {
        eprintln!("  公网:   http://{ip}:{}/v1", app.port);
        eprintln!("  （中国大陆家庭网络可能无法直接使用公网连接，建议走局域网）");
    } else {
        eprintln!("  公网:   无法获取（探测失败）");
    }
    if app.auth_token.is_some() {
        eprintln!("已启用 AUTH_TOKEN 鉴权");
    }

    let server = match Server::http(&address) {
        Ok(server) => server,
        Err(_) => {
            eprintln!("[Error] 端口已被占用，无法在 {address} 启动 HTTP 服务");
            eprintln!("请设置环境变量 PORT 改用其他端口后重新启动");
            eprintln!("5 秒后自动退出 ...");
            thread::sleep(Duration::from_secs(5));
            std::process::exit(1);
        }
    };
    let (tx, rx) = mpsc::channel::<HttpRequest>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..workers {
        let rx = Arc::clone(&rx);
        let app = app.clone();
        thread::spawn(move || {
            while let Ok(mut request) = rx.lock().expect("request queue lock").recv() {
                let response = app.handle(&mut request);
                let _ = request.respond(response);
            }
        });
    }
    for request in server.incoming_requests() {
        let _ = tx.send(request);
    }
}

fn client(timeout: Duration, connect_timeout: Duration) -> Client {
    Client::builder()
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .build()
        .expect("create HTTP client")
}

#[cfg(windows)]
fn setup_console_utf8() {
    use std::ffi::c_int;
    const CP_UTF8: u32 = 65001;
    unsafe extern "C" {
        fn SetConsoleOutputCP(cp: u32) -> c_int;
        fn SetConsoleCP(cp: u32) -> c_int;
    }
    unsafe {
        let _ = SetConsoleOutputCP(CP_UTF8);
        let _ = SetConsoleCP(CP_UTF8);
    }
}

#[cfg(not(windows))]
fn setup_console_utf8() {}

fn primary_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip.to_string())
    }
}

fn public_ip() -> Option<String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let text = client
        .get("https://api.ipify.org")
        .send()
        .ok()?
        .text()
        .ok()?;
    let ip = text.trim().to_owned();
    if ip.is_empty() {
        None
    } else {
        Some(ip)
    }
}

impl App {
    fn handle(&self, request: &mut HttpRequest) -> ResponseBox {
        let method = request.method().clone();
        let full_path = request.url().to_owned();
        let path = full_path.split('?').next().unwrap_or(&full_path);

        if method == Method::Options {
            return cors_response(204, Vec::new());
        }
        if method == Method::Get && is_health_path(path) {
            self.requests.fetch_add(1, Ordering::Relaxed);
            return json_response(
                200,
                json!({
                    "status": "ok",
                    "nodes": self.nodes,
                    "requests": self.requests.load(Ordering::Relaxed),
                    "upstream_failures": self.failures.load(Ordering::Relaxed),
                    "port": self.port,
                    "local_ip": self.local_ip,
                    "public_ip": self.public_ip,
                    "started": self.started,
                }),
            );
        }
        if self.auth_token.is_some() && !self.authorized(request) {
            return error_response(401, "invalid_api_key", "missing or invalid bearer token");
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        if method == Method::Get && is_models_path(path) {
            return self.models(request);
        }
        if method == Method::Get && is_claude_models_path(path) {
            return self.claude_models();
        }
        if method == Method::Post && is_chat_path(path) {
            return self.chat_completions(request);
        }
        if method == Method::Get && path == "/" {
            return self.index_response();
        }
        self.forward(request, &method, &full_path)
    }

    fn authorized(&self, request: &HttpRequest) -> bool {
        let expected = self.auth_token.as_deref().unwrap_or_default();
        request_header(request, "Authorization")
            .and_then(|value| value.strip_prefix("Bearer ").map(str::trim).map(str::to_owned))
            .is_some_and(|token| token == expected)
    }

    fn models(&self, request: &HttpRequest) -> ResponseBox {
        let accept = request_header(request, "Accept");
        let response =
            match self.upstream("/models", Method::Get, Vec::new(), accept.as_deref(), false) {
                Ok(response) => response,
                Err(message) => return error_response(502, "upstream_error", &message),
            };
        let payload: Value =
            match response
                .bytes()
                .ok()
                .and_then(|body| serde_json::from_slice(&body).ok())
            {
                Some(payload) => payload,
                None => return error_response(502, "upstream_error", "invalid models response"),
            };
        let models = self.process_models(payload);
        json_response(200, json!({"data": models}))
    }

    fn process_models(&self, payload: Value) -> Vec<Value> {
        let mut map = self.free_map.lock().expect("model map lock");
        map.clear();
        payload
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|mut item| {
                let id = item.get("id").and_then(Value::as_str)?;
                if !self.is_free_model(id) {
                    return None;
                }
                if !self.strip_free {
                    map.insert(id.to_owned(), id.to_owned());
                    return Some(item);
                }
                let stripped = strip_free_suffix(id);
                if stripped.is_empty() || map.get(&stripped).is_some_and(|existing| existing != id)
                {
                    return None;
                }
                map.insert(stripped.clone(), id.to_owned());
                item.as_object_mut()
                    .map(|object| object.insert("id".to_owned(), Value::String(stripped)));
                Some(item)
            })
            .collect()
    }

    fn is_free_model(&self, id: &str) -> bool {
        let lower = id.to_lowercase();
        lower.contains("free") || EXTRA_FREE_MODELS.iter().any(|extra| lower == *extra)
    }

    fn chat_completions(&self, request: &mut HttpRequest) -> ResponseBox {
        let mut body = Vec::new();
        if request
            .as_reader()
            .take(MAX_BODY_BYTES + 1)
            .read_to_end(&mut body)
            .is_err()
            || body.len() as u64 > MAX_BODY_BYTES
        {
            return bad_request();
        }
        let payload: Value = match serde_json::from_slice(&body) {
            Ok(payload) => payload,
            Err(_) => return bad_request(),
        };
        let stream = is_stream_request(&payload);
        let body = self.remap_chat_body(payload);
        let accept = request_header(request, "Accept");
        if stream {
            return self.chat_stream(body, accept.as_deref());
        }
        match self.upstream("/chat/completions", Method::Post, body, accept.as_deref(), false) {
            Ok(response) => proxy_response(response),
            Err(message) => error_response(502, "upstream_error", &message),
        }
    }

    fn remap_chat_body(&self, mut payload: Value) -> Vec<u8> {
        if self.strip_free {
            if let Some(original) = payload
                .get("model")
                .and_then(Value::as_str)
                .and_then(|model| {
                    self.free_map
                        .lock()
                        .expect("model map lock")
                        .get(model)
                        .cloned()
                })
            {
                payload["model"] = Value::String(original);
            }
        }
        serde_json::to_vec(&payload).unwrap_or_default()
    }

    fn chat_stream(&self, body: Vec<u8>, accept: Option<&str>) -> ResponseBox {
        match self.upstream("/chat/completions", Method::Post, body, accept, true) {
            Ok(response) => {
                let status = response.status().as_u16();
                let mut headers = vec![
                    header("Content-Type", "text/event-stream; charset=utf-8"),
                    header("Cache-Control", "no-cache"),
                ];
                append_cors(&mut headers);
                Response::new(StatusCode(status), headers, response, None, None).boxed()
            }
            Err(message) => error_response(502, "upstream_error", &message),
        }
    }

    fn upstream(
        &self,
        path: &str,
        method: Method,
        body: Vec<u8>,
        accept: Option<&str>,
        stream: bool,
    ) -> Result<UpstreamResponse, String> {
        if self.nodes.is_empty() {
            return Err("no upstream nodes configured".to_string());
        }
        let client = if stream {
            &self.stream_client
        } else {
            &self.client
        };
        let mut failures = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let url = format!("{node}{path}");
            let mut request = match method {
                Method::Get => client.get(&url),
                Method::Post => client
                    .post(&url)
                    .body(body.clone())
                    .header("Content-Type", "application/json"),
                _ => continue,
            };
            if let Some(accept) = accept {
                request = request.header("Accept", accept);
            }
            if let Some(token) = &self.token {
                request = request.bearer_auth(token);
            }
            match request.send() {
                Ok(response) if response.status().is_success() => {
                    if stream {
                        eprintln!("正在通过 {node} 流式转发 {path}");
                    }
                    return Ok(response);
                }
                Ok(response) => {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                    let message = format!("{node}: HTTP {}", response.status());
                    eprintln!("上游请求失败 {message}");
                    failures.push(message);
                }
                Err(error) => {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                    eprintln!("上游请求失败 {node}: {error}");
                    failures.push(format!("{node}: {error}"));
                }
            }
        }
        Err(failures.join("; "))
    }

    fn index_response(&self) -> ResponseBox {
        let public = self.public_ip.as_deref().unwrap_or("无法获取公网 IP");
        let html = INDEX_HTML
            .replace("__VERSION__", VERSION)
            .replace("__PORT__", &self.port)
            .replace("__LOCAL_IP__", &self.local_ip)
            .replace("__PUBLIC_IP__", public);
        let body = html.into_bytes();
        let len = body.len();
        let mut headers = vec![
            header("Content-Type", "text/html; charset=utf-8"),
            header("Cache-Control", "no-cache, no-store, must-revalidate"),
        ];
        append_cors(&mut headers);
        Response::new(
            StatusCode(200),
            headers,
            Cursor::new(body),
            Some(len),
            None,
        )
        .boxed()
    }

    fn claude_models(&self) -> ResponseBox {
        let ids = self.free_model_ids();
        let data: Vec<Value> = ids
            .into_iter()
            .map(|id| {
                json!({
                    "id": id,
                    "display_name": id,
                    "created_at": 0,
                    "type": "model"
                })
            })
            .collect();
        json_response(200, json!({ "data": data }))
    }

    fn free_model_ids(&self) -> Vec<String> {
        let map = self.free_map.lock().expect("model map lock");
        if !map.is_empty() {
            let mut ids: Vec<String> = map.keys().cloned().collect();
            ids.sort();
            return ids;
        }
        drop(map);
        match self
            .upstream("/models", Method::Get, Vec::new(), None, false)
        {
            Ok(response) => {
                let payload: Value = response
                    .bytes()
                    .ok()
                    .and_then(|body| serde_json::from_slice(&body).ok())
                    .unwrap_or(Value::Null);
                let mut ids: Vec<String> = self
                    .process_models(payload)
                    .into_iter()
                    .filter_map(|item| {
                        item.get("id").and_then(Value::as_str).map(str::to_owned)
                    })
                    .collect();
                ids.sort();
                ids
            }
            Err(_) => Vec::new(),
        }
    }

    fn forward(
        &self,
        request: &mut HttpRequest,
        method: &Method,
        full_path: &str,
    ) -> ResponseBox {
        if self.nodes.is_empty() {
            return error_response(502, "upstream_error", "no upstream nodes configured");
        }
        let mut body = Vec::new();
        if matches!(method, &Method::Post | &Method::Put | &Method::Patch) {
            if request
                .as_reader()
                .take(MAX_BODY_BYTES + 1)
                .read_to_end(&mut body)
                .is_err()
                || body.len() as u64 > MAX_BODY_BYTES
            {
                return bad_request();
            }
        }
        let target = route_upstream_path(full_path);
        for node in &self.nodes {
            let url = format!("{node}{target}");
            let builder = match method {
                Method::Post => self
                    .stream_client
                    .post(&url)
                    .header("Content-Type", "application/json"),
                Method::Put => self
                    .stream_client
                    .put(&url)
                    .header("Content-Type", "application/json"),
                Method::Patch => self
                    .stream_client
                    .patch(&url)
                    .header("Content-Type", "application/json"),
                Method::Delete => self.stream_client.delete(&url),
                _ => self.stream_client.get(&url),
            };
            let result = builder.body(body.clone()).send();
            match result {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let content_type = response
                        .headers()
                        .get("Content-Type")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                        .unwrap_or_else(|| "application/json; charset=utf-8".to_owned());
                    let mut headers = vec![header("Cache-Control", "no-cache")];
                    headers.push(header("Content-Type", &content_type));
                    append_cors(&mut headers);
                    return Response::new(StatusCode(status), headers, response, None, None).boxed();
                }
                Err(error) => {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                    eprintln!("上游请求失败 {node}: {error}");
                }
            }
        }
        error_response(502, "upstream_error", "all upstream nodes failed")
    }
}

fn proxy_response(upstream: UpstreamResponse) -> ResponseBox {
    let status = upstream.status().as_u16();
    let content_type = upstream
        .headers()
        .get("Content-Type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let cache_control = upstream
        .headers()
        .get("Cache-Control")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = upstream
        .bytes()
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default();
    response(status, body, content_type.as_deref(), cache_control.as_deref())
}

fn cors_response(status: u16, body: Vec<u8>) -> ResponseBox {
    response(status, body, None, None)
}

fn json_response(status: u16, payload: Value) -> ResponseBox {
    let body = serde_json::to_vec(&payload).expect("serialize JSON response");
    response(status, body, Some("application/json; charset=utf-8"), None)
}

fn error_response(status: u16, kind: &str, message: &str) -> ResponseBox {
    json_response(status, json!({"error": {"type": kind, "message": message}}))
}

fn bad_request() -> ResponseBox {
    error_response(
        400,
        "invalid_request_error",
        "request body must be valid JSON up to 64 MiB",
    )
}

fn response(
    status: u16,
    body: Vec<u8>,
    content_type: Option<&str>,
    cache_control: Option<&str>,
) -> ResponseBox {
    let mut headers = Vec::with_capacity(4);
    if let Some(value) = content_type {
        headers.push(header("Content-Type", value));
    }
    if let Some(value) = cache_control {
        headers.push(header("Cache-Control", value));
    }
    append_cors(&mut headers);
    let data_length = Some(body.len());
    Response::new(
        StatusCode(status),
        headers,
        Cursor::new(body),
        data_length,
        None,
    )
    .boxed()
}

fn append_cors(headers: &mut Vec<Header>) {
    headers.push(header("Access-Control-Allow-Origin", "*"));
    headers.push(header("Access-Control-Allow-Methods", "GET, POST, OPTIONS"));
    headers.push(header("Access-Control-Allow-Headers", "Content-Type, Authorization"));
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).expect("valid static header")
}

fn request_header(request: &HttpRequest, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str().to_owned())
}

fn is_stream_request(payload: &Value) -> bool {
    payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn is_health_path(path: &str) -> bool {
    matches!(
        path,
        "/health" | "/healthz" | "/ready" | "/api/health" | "/api/status"
    )
}

fn is_models_path(path: &str) -> bool {
    const MODELS_PATHS: &[&str] = &[
        "/v1/models",
        "/models",
        "/api/paas/v4/models",
        "/api/v3/models",
        "/api/v1/models",
        "/v1beta/models",
    ];
    MODELS_PATHS.contains(&path)
}

fn is_claude_models_path(path: &str) -> bool {
    matches!(path, "/claude/v1/models" | "/anthropic/v1/models")
}

fn is_chat_path(path: &str) -> bool {
    const CHAT_PATHS: &[&str] = &[
        "/v1/chat/completions",
        "/chat/completions",
        "/api/paas/v4/chat/completions",
        "/api/v3/chat/completions",
        "/api/v1/chat/completions",
        "/v1beta/chat/completions",
    ];
    CHAT_PATHS.contains(&path)
}

fn route_upstream_path(path: &str) -> String {
    if path == "/" {
        return "/".to_owned();
    }
    const PREFIXES: &[&str] = &[
        "/api/paas/v4",
        "/api/v3",
        "/api/v1",
        "/claude/v1",
        "/anthropic/v1",
        "/openai/deployments/",
        "/v1beta",
        "/api",
        "/v1",
    ];
    let mut rest = path;
    for prefix in PREFIXES {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            rest = stripped;
            break;
        }
    }
    if rest.is_empty() {
        "/".to_owned()
    } else {
        rest.to_owned()
    }
}

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| matches!(value.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

fn env_seconds(name: &str, fallback: u64) -> Duration {
    Duration::from_secs(
        env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback),
    )
}

fn split_nodes(value: &str) -> Vec<String> {
    let mut nodes = Vec::new();
    for node in value
        .split(',')
        .map(|node| node.trim().trim_end_matches('/'))
        .filter(|node| !node.is_empty())
    {
        if !nodes.iter().any(|existing| existing == node) {
            nodes.push(node.to_owned());
        }
    }
    nodes
}

fn strip_free_suffix(id: &str) -> String {
    const SUFFIX: &str = "-free";
    let lower = id.to_lowercase();
    if lower.len() > SUFFIX.len() && lower.ends_with(SUFFIX) {
        id[..id.len() - SUFFIX.len()].to_owned()
    } else {
        id.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_and_deduplicates_nodes() {
        assert_eq!(
            split_nodes(" https://one.example/ , ,https://two.example/, https://one.example "),
            ["https://one.example", "https://two.example"]
        );
    }

    #[test]
    fn detects_stream_request() {
        assert!(is_stream_request(&json!({"stream": true})));
        assert!(!is_stream_request(&json!({"stream": false})));
        assert!(!is_stream_request(&json!({"messages": []})));
    }

    fn app_for_test() -> App {
        App {
            nodes: vec![DEFAULT_NODE.to_owned()],
            token: None,
            auth_token: None,
            strip_free: false,
            free_map: Arc::new(Mutex::new(HashMap::new())),
            client: client(Duration::from_secs(15), Duration::from_secs(15)),
            stream_client: client(Duration::from_secs(15), Duration::from_secs(15)),
            requests: Arc::new(AtomicU64::new(0)),
            failures: Arc::new(AtomicU64::new(0)),
            port: "8788".to_owned(),
            local_ip: "127.0.0.1".to_owned(),
            public_ip: None,
            started: 0,
        }
    }

    fn live_models(app: &App) -> Vec<Value> {
        let response = app
            .upstream("/models", Method::Get, Vec::new(), None, false)
            .expect("official zen /v1/models reachable");
        let payload: Value = response
            .bytes()
            .ok()
            .and_then(|body| serde_json::from_slice(&body).ok())
            .expect("parse /v1/models response");
        app.process_models(payload)
    }

    #[test]
    fn fetches_free_models_from_official_zen() {
        let app = app_for_test();
        let models = live_models(&app);
        assert!(!models.is_empty(), "expected free models from the official list");
        assert!(models.iter().all(|model| {
            model
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| app.is_free_model(id))
        }));
        assert!(models.iter().any(|model| {
            model
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.to_lowercase().contains("free"))
        }));
    }

    #[test]
    fn strips_free_suffix_on_live_list_and_maps_back() {
        let app = App {
            strip_free: true,
            ..app_for_test()
        };
        let models = live_models(&app);
        assert!(!models.is_empty(), "expected free models from the official list");
        for model in &models {
            let id = model["id"].as_str().expect("model id");
            assert!(
                !id.to_lowercase().ends_with("-free"),
                "{id} still carries the -free suffix"
            );
        }
        let map = app.free_map.lock().unwrap();
        assert!(!map.is_empty());
        for (stripped, original) in map.iter() {
            assert_eq!(stripped, &strip_free_suffix(original));
        }
    }
}
