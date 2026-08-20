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

use serde_json::{Value, json};
use tiny_http::{Header, Method, Request as HttpRequest, Response, Server, StatusCode};
use ureq::Agent;

type ResponseBox = tiny_http::ResponseBox;
type UpstreamResponse = http::Response<Vec<u8>>;

#[derive(Debug)]
struct UpstreamError {
    status: u16,
    message: String,
    body: Vec<u8>,
}

const MAX_BODY_BYTES: u64 = 64 << 20;
const DEFAULT_NODE: &str = "https://opencode.ai/zen/v1";
const EXTRA_FREE_MODELS: &[&str] = &["big-pickle"];
const FREE_MAP_TTL: u64 = 60;
const UPSTREAM_USER_AGENT: &str = "opencode/1.0";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const INDEX_HTML: &str = include_str!("../static/index.html");

#[derive(Clone)]
struct App {
    nodes: Vec<String>,
    token: Option<String>,
    auth_token: Option<String>,
    strip_free: bool,
    free_map: Arc<Mutex<FreeMap>>,
    client: Agent,
    stream_client: Agent,
    requests: Arc<AtomicU64>,
    port: String,
    local_ip: String,
    public_ip: Option<String>,
    started: u64,
}

struct FreeMap {
    entries: HashMap<String, String>,
    refreshed_at: u64,
}

impl FreeMap {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            refreshed_at: 0,
        }
    }
}

impl App {
    fn from_env() -> Self {
        let connect_timeout = env_seconds("CONNECT_TIMEOUT", 10);
        let nodes = split_nodes(&env::var("NODES").unwrap_or_default());
        let nodes = if nodes.is_empty() {
            vec![DEFAULT_NODE.to_owned()]
        } else {
            nodes
        };
        let port = env_or("PORT", "8788");
        App {
            nodes,
            token: env::var("API_TOKEN").ok().filter(|value| !value.is_empty()),
            auth_token: env::var("AUTH_TOKEN")
                .ok()
                .filter(|value| !value.is_empty()),
            strip_free: env_flag("STRIP_FREE"),
            free_map: Arc::new(Mutex::new(FreeMap::new())),
            client: client(env_seconds("UPSTREAM_TIMEOUT", 90), connect_timeout),
            stream_client: client(env_seconds("STREAM_TIMEOUT", 1800), connect_timeout),
            requests: Arc::new(AtomicU64::new(0)),
            port,
            local_ip: primary_ip().unwrap_or_else(|| "127.0.0.1".to_owned()),
            public_ip: public_ip(),
            started: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

fn main() {
    setup_console_utf8();
    let address = format!("{}:{}", env_or("HOST", "0.0.0.0"), env_or("PORT", "8788"));
    let app = App::from_env();
    let workers = env::var("WORKERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        })
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

fn client(timeout: Duration, connect_timeout: Duration) -> Agent {
    Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(connect_timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

fn add_headers<B>(
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

fn add_token<B>(
    mut builder: ureq::RequestBuilder<B>,
    token: &Option<String>,
) -> ureq::RequestBuilder<B> {
    builder = builder.header("User-Agent", UPSTREAM_USER_AGENT);
    if let Some(token) = token {
        builder = builder.header("Authorization", &format!("Bearer {token}"));
    }
    builder
}

fn read_body(body: ureq::Body) -> Vec<u8> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let mut reader = body.into_reader();
    let _ = reader.read_to_end(&mut bytes);
    bytes
}

fn read_body_response(response: http::Response<ureq::Body>) -> UpstreamResponse {
    let (parts, body) = response.into_parts();
    let body = read_body(body);
    http::Response::from_parts(parts, body)
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
    let agent = client(Duration::from_secs(5), Duration::from_secs(5));
    let text = agent.get("https://api.ipify.org").call().ok()?.into_body();
    let ip = String::from_utf8(read_body(text)).ok()?;
    let ip = ip.trim().to_owned();
    if ip.is_empty() { None } else { Some(ip) }
}

impl App {
    fn handle(&self, request: &mut HttpRequest) -> ResponseBox {
        let method = request.method().clone();
        let full_path = request.url().to_owned();
        let raw_path = full_path.split('?').next().unwrap_or(&full_path);
        let path = normalize_path(raw_path);

        if method == Method::Options {
            return cors_response(204, Vec::new());
        }
        if method == Method::Get && is_health_path(path) {
            return json_response(
                200,
                json!({
                    "status": "ok",
                    "nodes": self.nodes,
                    "requests": self.requests.load(Ordering::Relaxed),
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
            .and_then(|value| {
                value
                    .strip_prefix("Bearer ")
                    .map(str::trim)
                    .map(str::to_owned)
            })
            .is_some_and(|token| token == expected)
    }

    fn models(&self, request: &HttpRequest) -> ResponseBox {
        let accept = request_header(request, "Accept");
        let response =
            match self.upstream("/models", Method::Get, Vec::new(), accept.as_deref(), false) {
                Ok(response) => response,
                Err(error) => return error_response(error.status, "upstream_error", &error.message),
            };
        let payload: Value = match serde_json::from_slice(&response.into_body()).ok() {
            Some(payload) => payload,
            None => return error_response(502, "upstream_error", "invalid models response"),
        };
        let models = self.process_models(payload);
        json_response(200, json!({"data": models}))
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
        let thinking = thinking_mode(&payload);
        let body = self.remap_chat_body(payload);
        let accept = request_header(request, "Accept");
        if stream {
            return self.chat_stream(body, accept.as_deref(), thinking);
        }
        match self.upstream(
            "/chat/completions",
            Method::Post,
            body,
            accept.as_deref(),
            false,
        ) {
            Ok(response) => {
                if thinking {
                    let (parts, mut body) = response.into_parts();
                    ensure_completion_reasoning(&mut body);
                    return proxy_response(http::Response::from_parts(parts, body));
                }
                proxy_response(response)
            }
            Err(error) => {
                if (400..500).contains(&error.status) && !error.body.is_empty() {
                    response(
                        error.status,
                        error.body,
                        Some("application/json; charset=utf-8"),
                        None,
                    )
                } else {
                    error_response(error.status, "upstream_error", &error.message)
                }
            }
        }
    }

    fn remap_chat_body(&self, mut payload: Value) -> Vec<u8> {
        if self.strip_free {
            self.refresh_free_map();
            if let Some(original) = payload
                .get("model")
                .and_then(Value::as_str)
                .and_then(|model| {
                    self.free_map
                        .lock()
                        .expect("model map lock")
                        .entries
                        .get(model)
                        .cloned()
                })
            {
                payload["model"] = Value::String(original);
            }
        }
        // reasoning.effort 写法归一到 reasoning_effort，两种写法都能触发思考模式；
        // 已有显式 reasoning_effort 时优先保留原值
        if payload.get("reasoning_effort").is_none() {
            if let Some(effort) = payload
                .get("reasoning")
                .and_then(|reasoning| reasoning.get("effort"))
                .and_then(Value::as_str)
                .filter(|effort| !effort.is_empty())
            {
                payload["reasoning_effort"] = Value::String(effort.to_owned());
            }
        }
        // Codex/thinking 协议要求把推理内容回传给上游，否则上游报 400。
        // OpenCode Console (zen/v1) 期望在请求顶层带上 reasoning_content；
        // 客户端通常不回传，这里伪装一个空串让上游通过校验。
        if thinking_mode(&payload) {
            if !payload
                .get("reasoning_content")
                .is_some_and(Value::is_string)
            {
                payload["reasoning_content"] = Value::String(String::new());
            }
            if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
                for message in messages {
                    if message.get("role").and_then(Value::as_str) == Some("assistant")
                        && !message
                            .get("reasoning_content")
                            .is_some_and(Value::is_string)
                    {
                        message["reasoning_content"] = Value::String(String::new());
                    }
                }
            }
        }
        serde_json::to_vec(&payload).unwrap_or_default()
    }

    fn chat_stream(&self, body: Vec<u8>, accept: Option<&str>, thinking: bool) -> ResponseBox {
        match self.upstream("/chat/completions", Method::Post, body, accept, true) {
            Ok(response) => {
                let status = response.status().as_u16();
                let mut body = response.into_body();
                if thinking {
                    transform_stream_body(&mut body);
                }
                let mut headers = vec![
                    header("Content-Type", "text/event-stream; charset=utf-8"),
                    header("Cache-Control", "no-cache"),
                ];
                append_cors(&mut headers);
                Response::new(StatusCode(status), headers, Cursor::new(body), None, None).boxed()
            }
            Err(error) => {
                if (400..500).contains(&error.status) && !error.body.is_empty() {
                    response(
                        error.status,
                        error.body,
                        Some("text/event-stream; charset=utf-8"),
                        None,
                    )
                } else {
                    error_response(error.status, "upstream_error", &error.message)
                }
            }
        }
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
        Response::new(StatusCode(200), headers, Cursor::new(body), Some(len), None).boxed()
    }
}

impl App {
    fn process_models(&self, payload: Value) -> Vec<Value> {
        let mut map = self.free_map.lock().expect("model map lock");
        map.entries.clear();
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
                    map.entries.insert(id.to_owned(), id.to_owned());
                    return Some(item);
                }
                let stripped = strip_free_suffix(id);
                if map
                    .entries
                    .get(&stripped)
                    .is_some_and(|existing| existing != id)
                {
                    return None;
                }
                map.entries.insert(stripped.clone(), id.to_owned());
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

    fn refresh_free_map(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let stale = {
            let map = self.free_map.lock().expect("model map lock");
            now.saturating_sub(map.refreshed_at) >= FREE_MAP_TTL
        };
        if !stale {
            return;
        }
        let refreshed = self
            .upstream("/models", Method::Get, Vec::new(), None, false)
            .ok()
            .and_then(|response| {
                serde_json::from_slice(&response.into_body())
                    .ok()
                    .filter(|value: &Value| value.get("data").is_some())
            });
        if let Some(payload) = refreshed {
            self.process_models(payload);
            self.free_map.lock().expect("model map lock").refreshed_at = now;
        }
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
        self.refresh_free_map();
        let map = self.free_map.lock().expect("model map lock");
        let mut ids: Vec<String> = map.entries.keys().cloned().collect();
        ids.sort();
        ids
    }
}

impl App {
    fn upstream(
        &self,
        path: &str,
        method: Method,
        body: Vec<u8>,
        accept: Option<&str>,
        stream: bool,
    ) -> Result<UpstreamResponse, UpstreamError> {
        if self.nodes.is_empty() {
            return Err(UpstreamError {
                status: 502,
                message: "no upstream nodes configured".to_string(),
                body: Vec::new(),
            });
        }
        let agent = if stream {
            &self.stream_client
        } else {
            &self.client
        };
        let mut failures = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let url = format!("{node}{path}");
            let outcome = match method {
                Method::Post => {
                    let builder = add_headers(agent.post(&url), accept, &self.token);
                    builder
                        .header("Content-Type", "application/json")
                        .send(&body)
                        .map(read_body_response)
                }
                Method::Get => {
                    let builder = add_headers(agent.get(&url), accept, &self.token);
                    builder.call().map(read_body_response)
                }
                _ => continue,
            };
            match outcome {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status();
                        let body_bytes = response.into_body();
                        let body_str = String::from_utf8_lossy(&body_bytes).trim().to_owned();
                        let message = if body_str.is_empty() {
                            format!("{node}: HTTP {status}")
                        } else {
                            format!("{node}: HTTP {status} - {body_str}")
                        };
                        eprintln!("上游请求失败 {message}");
                        if status.is_client_error() {
                            return Err(UpstreamError {
                                status: status.as_u16(),
                                message,
                                body: body_bytes,
                            });
                        }
                        failures.push(message);
                        continue;
                    }
                    if stream {
                        eprintln!("正在通过 {node} 流式转发 {path}");
                    }
                    return Ok(response);
                }
                Err(error) => {
                    eprintln!("上游请求失败 {node}: {error}");
                    failures.push(format!("{node}: {error}"));
                }
            }
        }
        Err(UpstreamError {
            status: 502,
            message: failures.join("; "),
            body: Vec::new(),
        })
    }

    fn forward(&self, request: &mut HttpRequest, method: &Method, full_path: &str) -> ResponseBox {
        if self.nodes.is_empty() {
            return error_response(502, "upstream_error", "no upstream nodes configured");
        }
        let mut body = Vec::new();
        if matches!(method, &Method::Post | &Method::Put | &Method::Patch)
            && (request
                .as_reader()
                .take(MAX_BODY_BYTES + 1)
                .read_to_end(&mut body)
                .is_err()
                || body.len() as u64 > MAX_BODY_BYTES)
        {
            return bad_request();
        }
        let target = route_upstream_path(full_path);
        for node in &self.nodes {
            let url = format!("{node}{target}");
            let result = self.call_with_method(method, &url, &body);
            match result {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let content_type = response
                        .headers()
                        .get("Content-Type")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                        .unwrap_or_else(|| "application/json; charset=utf-8".to_owned());
                    let body = response.into_body();
                    let mut headers = vec![header("Cache-Control", "no-cache")];
                    headers.push(header("Content-Type", &content_type));
                    append_cors(&mut headers);
                    return Response::new(
                        StatusCode(status),
                        headers,
                        Cursor::new(body),
                        None,
                        None,
                    )
                    .boxed();
                }
                Err(error) => {
                    eprintln!("上游请求失败 {node}: {error}");
                }
            }
        }
        error_response(502, "upstream_error", "all upstream nodes failed")
    }

    fn call_with_method(
        &self,
        method: &Method,
        url: &str,
        body: &[u8],
    ) -> Result<UpstreamResponse, ureq::Error> {
        let agent = &self.stream_client;
        let response = match method {
            Method::Post | Method::Put | Method::Patch => {
                let builder = match method {
                    Method::Post => add_token(agent.post(url), &self.token),
                    Method::Put => add_token(agent.put(url), &self.token),
                    _ => add_token(agent.patch(url), &self.token),
                };
                builder
                    .header("Content-Type", "application/json")
                    .send(body)?
            }
            Method::Delete => add_token(agent.delete(url), &self.token).call()?,
            _ => add_token(agent.get(url), &self.token).call()?,
        };
        Ok(read_body_response(response))
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
    let body = upstream.into_body();
    response(
        status,
        body,
        content_type.as_deref(),
        cache_control.as_deref(),
    )
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
    headers.push(header(
        "Access-Control-Allow-Headers",
        "Content-Type, Authorization",
    ));
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

fn thinking_mode(payload: &Value) -> bool {
    payload
        .get("thinking")
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || payload
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        || payload
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
}

// 思考模式下客户端期望响应必带 reasoning_content；
// 实测上游 reasoning_tokens 记账不可靠（思考了也常报 0），
// 因此只按"字段缺失"补空串，不看 token 数
fn ensure_completion_reasoning(body: &mut Vec<u8>) {
    let mut payload: Value = match serde_json::from_slice(body) {
        Ok(payload) => payload,
        Err(_) => return,
    };
    if payload.get("choices").is_none() {
        return;
    }
    if !payload
        .get("reasoning_content")
        .is_some_and(Value::is_string)
    {
        payload["reasoning_content"] = Value::String(String::new());
    }
    if let Some(choices) = payload.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            if choice.get("reasoning_content").is_some() {
                continue;
            }
            if let Some(message) = choice.get_mut("message") {
                if !message
                    .get("reasoning_content")
                    .is_some_and(Value::is_string)
                {
                    message["reasoning_content"] = Value::String(String::new());
                }
            }
        }
    }
    if let Ok(updated) = serde_json::to_vec(&payload) {
        *body = updated;
    }
}

// 从流里找一个带 id/model 的 chunk，借它的身份字段给兜底 chunk，
// 否则严格的 OpenAI SDK 会因缺字段解析失败
fn stream_chunk_identity(text: &str) -> Option<Value> {
    for block in text.split("\n\n") {
        let Some(data_line) = block.lines().find_map(|line| {
            line.strip_prefix("data:")
                .map(str::trim_start)
                .filter(|value| !value.is_empty() && *value != "[DONE]")
        }) else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<Value>(data_line) else {
            continue;
        };
        if payload.get("id").is_some() || payload.get("model").is_some() {
            return Some(payload);
        }
    }
    None
}

fn fallback_reasoning_chunk(text: &str) -> String {
    let mut chunk = json!({ "choices": [{ "index": 0, "delta": { "reasoning_content": "" } }] });
    if let Some(object) = chunk.as_object_mut() {
        if let Some(identity) = stream_chunk_identity(text) {
            for key in ["id", "object", "created", "model"] {
                if let Some(value) = identity.get(key) {
                    object.insert(key.to_owned(), value.clone());
                }
            }
        }
    }
    format!("data: {chunk}\n\n")
}

// 思考模式下上游已经发过 reasoning_content delta 就不再动手；
// reasoning_tokens 实测记账不可靠，不作为补偿依据
fn stream_has_reasoning(text: &str) -> bool {
    text.contains("\"reasoning_content\"")
}

fn inject_reasoning_into_finish_block(block: &str) -> Option<String> {
    let data_line = block.lines().find_map(|line| {
        line.strip_prefix("data:")
            .map(str::trim_start)
            .filter(|value| !value.is_empty() && *value != "[DONE]")
    })?;
    let mut payload: Value = serde_json::from_str(data_line).ok()?;
    let mut modified = false;
    if let Some(choices) = payload.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            if choice.get("finish_reason").is_none() {
                continue;
            }
            if let Some(delta) = choice.get_mut("delta") {
                if delta.get("content").is_none() && delta.get("reasoning_content").is_none() {
                    delta["reasoning_content"] = Value::String(String::new());
                    modified = true;
                }
            }
        }
    }
    if !modified {
        return None;
    }
    let rewritten = serde_json::to_string(&payload).ok()?;
    let mut rebuilt = Vec::new();
    let mut replaced = false;
    for line in block.lines() {
        if !replaced
            && line
                .strip_prefix("data:")
                .is_some_and(|value| !value.trim().is_empty())
        {
            rebuilt.push(format!("data: {rewritten}"));
            replaced = true;
        } else {
            rebuilt.push(line.to_owned());
        }
    }
    Some(rebuilt.join("\n") + "\n\n")
}

// 思考模式下上游从头到尾没发过 reasoning_content delta 时，
// 在 finish chunk 里补一个空串，客户端才不会抱怨"推理内容缺失"
fn transform_stream_body(body: &mut Vec<u8>) {
    let text = String::from_utf8_lossy(body).replace("\r\n", "\n");
    if stream_has_reasoning(&text) {
        return;
    }
    let fallback = fallback_reasoning_chunk(&text);
    let mut transformed = String::with_capacity(text.len() + fallback.len());
    let mut injected = false;
    for block in text.split_inclusive("\n\n") {
        if !injected {
            if let Some(rewritten) = inject_reasoning_into_finish_block(block) {
                transformed.push_str(&rewritten);
                injected = true;
                continue;
            }
            if block.lines().any(|line| line.trim() == "data: [DONE]") {
                transformed.push_str(&fallback);
                injected = true;
            }
        }
        transformed.push_str(block);
    }
    if !injected {
        transformed.push_str(&fallback);
    }
    *body = transformed.into_bytes();
}

fn is_health_path(path: &str) -> bool {
    matches!(
        path,
        "/health" | "/healthz" | "/ready" | "/api/health" | "/api/status"
    )
}

fn normalize_path(path: &str) -> &str {
    let normalized = path.trim_end_matches('/');
    if normalized.is_empty() {
        "/"
    } else {
        normalized
    }
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
        "/api/v1beta",
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
    fn upstream_requests_use_opencode_user_agent() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let request = server.recv().unwrap();
            tx.send(request_header(&request, "User-Agent")).unwrap();
            request.respond(Response::empty(200)).unwrap();
        });
        let url = format!("http://{address}/models");
        add_token(
            client(Duration::from_secs(5), Duration::from_secs(5)).get(&url),
            &None,
        )
        .call()
        .unwrap();
        handle.join().unwrap();
        assert_eq!(rx.recv().unwrap().as_deref(), Some("opencode/1.0"));
    }

    #[test]
    fn detects_stream_request() {
        assert!(is_stream_request(&json!({"stream": true})));
        assert!(!is_stream_request(&json!({"stream": false})));
        assert!(!is_stream_request(&json!({"messages": []})));
    }

    #[test]
    fn routes_upstream_paths_correctly() {
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("///"), "/");
        assert_eq!(normalize_path("/v1/models/"), "/v1/models");
        assert_eq!(
            normalize_path("/v1/chat/completions///"),
            "/v1/chat/completions"
        );
        assert_eq!(route_upstream_path("/v1/models"), "/models");
        assert_eq!(route_upstream_path("/models"), "/models");
        assert_eq!(route_upstream_path("/api/paas/v4/models"), "/models");
        assert_eq!(
            route_upstream_path("/api/v3/chat/completions"),
            "/chat/completions"
        );
        assert_eq!(route_upstream_path("/api/v1/models"), "/models");
        assert_eq!(route_upstream_path("/claude/v1/models"), "/models");
        assert_eq!(route_upstream_path("/anthropic/v1/models"), "/models");
        assert_eq!(route_upstream_path("/v1beta/models"), "/models");
        assert_eq!(
            route_upstream_path("/api/v1beta/chat/completions"),
            "/chat/completions"
        );
        assert_eq!(
            route_upstream_path("/v1/chat/completions"),
            "/chat/completions"
        );
        assert_eq!(
            route_upstream_path("/openai/v1/models"),
            "/openai/v1/models"
        );
        assert_eq!(route_upstream_path("/v2/models"), "/v2/models");
        assert_eq!(route_upstream_path("/"), "/");
    }

    fn app_for_test() -> App {
        App {
            nodes: vec![DEFAULT_NODE.to_owned()],
            token: None,
            auth_token: None,
            strip_free: false,
            free_map: Arc::new(Mutex::new(FreeMap::new())),
            client: client(Duration::from_secs(15), Duration::from_secs(15)),
            stream_client: client(Duration::from_secs(15), Duration::from_secs(15)),
            requests: Arc::new(AtomicU64::new(0)),
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
        let payload: Value =
            serde_json::from_slice(&response.into_body()).expect("parse /v1/models response");
        app.process_models(payload)
    }

    #[test]
    fn fetches_free_models_from_official_zen() {
        let app = app_for_test();
        let models = live_models(&app);
        assert!(
            !models.is_empty(),
            "expected free models from the official list"
        );
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
        assert!(
            !models.is_empty(),
            "expected free models from the official list"
        );
        for model in &models {
            let id = model["id"].as_str().expect("model id");
            assert!(
                !id.to_lowercase().ends_with("-free"),
                "{id} still carries the -free suffix"
            );
        }
        let map = app.free_map.lock().unwrap();
        assert!(!map.entries.is_empty());
        for (stripped, original) in map.entries.iter() {
            assert_eq!(stripped, &strip_free_suffix(original));
        }
    }

    #[test]
    fn refresh_free_map_populates_empty_map() {
        let app = App {
            strip_free: true,
            ..app_for_test()
        };
        let map = app.free_map.lock().unwrap();
        assert!(map.entries.is_empty(), "fresh app should have empty map");
        drop(map);
        app.refresh_free_map();
        let map = app.free_map.lock().unwrap();
        assert!(
            !map.entries.is_empty(),
            "refresh_free_map should populate the map"
        );
        for (stripped, original) in map.entries.iter() {
            assert_eq!(stripped, &strip_free_suffix(original));
        }
    }

    #[test]
    fn remap_chat_body_primes_map_when_strip_free() {
        let app = App {
            strip_free: true,
            ..app_for_test()
        };
        assert!(
            app.free_map.lock().unwrap().entries.is_empty(),
            "fresh app should have empty map"
        );
        app.refresh_free_map();
        let mapping = app.free_map.lock().unwrap();
        let (stripped, original) = mapping
            .entries
            .iter()
            .find(|(key, value)| key != value)
            .expect("strip_free should produce at least one mapped id");
        let stripped = stripped.clone();
        let original = original.clone();
        drop(mapping);
        let body = app.remap_chat_body(json!({
            "model": stripped,
            "messages": [],
            "stream": false
        }));
        let parsed: Value = serde_json::from_slice(&body).expect("valid json out");
        assert_eq!(
            parsed["model"], original,
            "chat body should have been remapped to the real -free id after priming, got {parsed}"
        );
    }

    #[test]
    fn thinking_mode_detects_thinking_and_effort() {
        assert!(thinking_mode(&json!({"thinking": {"enabled": true}})));
        assert!(!thinking_mode(&json!({"thinking": {"enabled": false}})));
        assert!(thinking_mode(&json!({"reasoning_effort": "high"})));
        assert!(thinking_mode(&json!({"reasoning": {"effort": "high"}})));
        assert!(!thinking_mode(&json!({"reasoning": {"effort": ""}})));
        assert!(!thinking_mode(&json!({"messages": []})));
    }

    #[test]
    fn remap_injects_empty_reasoning_in_thinking_mode() {
        let app = app_for_test();
        let body = app.remap_chat_body(json!({
            "model": "some-model",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"}
            ],
            "thinking": {"enabled": true},
            "stream": true
        }));
        let parsed: Value = serde_json::from_slice(&body).expect("valid json out");
        assert_eq!(parsed["reasoning_content"], json!(""));
        let assistant = parsed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message present");
        assert_eq!(assistant["reasoning_content"], json!(""));
    }

    #[test]
    fn remap_merges_reasoning_effort_object_form() {
        let app = app_for_test();
        let body = app.remap_chat_body(json!({
            "model": "some-model",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning": {"effort": "high"}
        }));
        let parsed: Value = serde_json::from_slice(&body).expect("valid json out");
        assert_eq!(parsed["reasoning_effort"], json!("high"));
        assert_eq!(parsed["reasoning_content"], json!(""), "thinking mode should kick in via merged effort");
    }

    #[test]
    fn remap_prefers_explicit_reasoning_effort() {
        let app = app_for_test();
        let body = app.remap_chat_body(json!({
            "model": "some-model",
            "messages": [],
            "reasoning_effort": "low",
            "reasoning": {"effort": "high"}
        }));
        let parsed: Value = serde_json::from_slice(&body).expect("valid json out");
        assert_eq!(parsed["reasoning_effort"], json!("low"));
    }

    #[test]
    fn ensure_completion_reasoning_injects_when_tokens_only() {
        let mut body = serde_json::to_vec(&json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"completion_tokens": 10, "completion_tokens_details": {"reasoning_tokens": 5}}
        }))
        .unwrap();
        ensure_completion_reasoning(&mut body);
        let parsed: Value = serde_json::from_slice(&body).expect("valid json out");
        assert_eq!(parsed["reasoning_content"], json!(""));
        assert_eq!(parsed["choices"][0]["message"]["reasoning_content"], json!(""));
    }

    #[test]
    fn ensure_completion_reasoning_keeps_existing_content() {
        let mut body = serde_json::to_vec(&json!({
            "reasoning_content": "thoughts",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi", "reasoning_content": "inner"}}],
            "usage": {"completion_tokens_details": {"reasoning_tokens": 5}}
        }))
        .unwrap();
        ensure_completion_reasoning(&mut body);
        let parsed: Value = serde_json::from_slice(&body).expect("valid json out");
        assert_eq!(parsed["reasoning_content"], json!("thoughts"));
        assert_eq!(parsed["choices"][0]["message"]["reasoning_content"], json!("inner"));
    }

    #[test]
    fn ensure_completion_reasoning_injects_without_usage_too() {
        let mut body = serde_json::to_vec(&json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}}],
            "usage": {"completion_tokens": 10}
        }))
        .unwrap();
        ensure_completion_reasoning(&mut body);
        let parsed: Value = serde_json::from_slice(&body).expect("valid json out");
        assert_eq!(parsed["reasoning_content"], json!(""));
        assert_eq!(parsed["choices"][0]["message"]["reasoning_content"], json!(""));
    }

    #[test]
    fn ensure_completion_reasoning_noop_without_choices() {
        let original = json!({"error": {"message": "boom"}});
        let mut body = serde_json::to_vec(&original).unwrap();
        ensure_completion_reasoning(&mut body);
        let parsed: Value = serde_json::from_slice(&body).expect("valid json out");
        assert_eq!(parsed, original);
    }

    #[test]
    fn ensure_completion_reasoning_ignores_non_json() {
        let mut body = b"<html>not json</html>".to_vec();
        ensure_completion_reasoning(&mut body);
        assert_eq!(body, b"<html>not json</html>");
    }

    #[test]
    fn stream_has_reasoning_detects_deltas() {
        let seen = r#"data: {"choices":[{"delta":{"reasoning_content":"hmm"}}]}
data: [DONE]"#;
        assert!(stream_has_reasoning(seen));
        assert!(!stream_has_reasoning("data: {\"usage\":{}}\n"));
    }

    #[test]
    fn transform_stream_injects_into_finish_chunk() {
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":7}}}\n\ndata: [DONE]\n\n";
        let mut body = stream.as_bytes().to_vec();
        transform_stream_body(&mut body);
        let transformed = String::from_utf8(body).unwrap();
        let finish_line = transformed
            .lines()
            .find(|line| line.contains("finish_reason"))
            .expect("finish chunk present");
        assert!(
            finish_line.contains("\"reasoning_content\":\"\""),
            "finish chunk should carry the fallback reasoning_content, got {finish_line}"
        );
        assert_eq!(transformed.matches("reasoning_content").count(), 1);
    }

    #[test]
    fn transform_stream_appends_fallback_without_finish_chunk() {
        let stream = "data: {\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":7}}}\n\ndata: [DONE]\n\n";
        let mut body = stream.as_bytes().to_vec();
        transform_stream_body(&mut body);
        let transformed = String::from_utf8(body).unwrap();
        let done = transformed.rfind("data: [DONE]").expect("DONE present");
        let fallback = transformed
            .rfind("reasoning_content")
            .expect("fallback chunk present");
        assert!(fallback < done, "fallback chunk must precede [DONE]");
    }

    #[test]
    fn transform_stream_fallback_carries_chunk_identity() {
        let stream = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":7}}}\n\ndata: [DONE]\n\n";
        let mut body = stream.as_bytes().to_vec();
        transform_stream_body(&mut body);
        let transformed = String::from_utf8(body).unwrap();
        let fallback_line = transformed
            .lines()
            .find(|line| line.contains("reasoning_content"))
            .expect("fallback chunk present");
        let payload: Value =
            serde_json::from_str(fallback_line.strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(payload["id"], json!("chatcmpl-1"));
        assert_eq!(payload["object"], json!("chat.completion.chunk"));
        assert_eq!(payload["model"], json!("m"));
    }

    #[test]
    fn transform_stream_fallback_without_identity_still_valid() {
        let stream = "data: {\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":7}}}\n\ndata: [DONE]\n\n";
        let mut body = stream.as_bytes().to_vec();
        transform_stream_body(&mut body);
        let transformed = String::from_utf8(body).unwrap();
        let fallback_line = transformed
            .lines()
            .find(|line| line.contains("reasoning_content"))
            .expect("fallback chunk present");
        let payload: Value =
            serde_json::from_str(fallback_line.strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(payload["choices"][0]["delta"]["reasoning_content"], json!(""));
    }

    #[test]
    fn transform_stream_handles_crlf() {
        let stream = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":3}}}\r\n\r\ndata: [DONE]\r\n\r\n";
        let mut body = stream.as_bytes().to_vec();
        transform_stream_body(&mut body);
        let transformed = String::from_utf8(body).unwrap();
        assert!(transformed.contains("\"reasoning_content\":\"\""));
    }

    #[test]
    fn transform_stream_noop_when_upstream_already_streams_reasoning() {
        let stream = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking...\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":7}}}\n\ndata: [DONE]\n\n";
        let mut body = stream.as_bytes().to_vec();
        let original = body.clone();
        transform_stream_body(&mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn transform_stream_injects_without_any_usage_chunk() {
        let stream = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let mut body = stream.as_bytes().to_vec();
        transform_stream_body(&mut body);
        let transformed = String::from_utf8(body).unwrap();
        assert!(transformed.contains("\"reasoning_content\":\"\""));
    }

    fn start_mock_upstream(
        capture: Arc<Mutex<Vec<u8>>>,
        response_body: Vec<u8>,
        content_type: &'static str,
    ) -> String {
        let server = Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = Vec::new();
                let _ = request.as_reader().read_to_end(&mut body);
                *capture.lock().unwrap() = body;
                let len = response_body.len();
                let resp = Response::new(
                    StatusCode(200),
                    vec![Header::from_bytes("Content-Type", content_type).unwrap()],
                    Cursor::new(response_body.clone()),
                    Some(len),
                    None,
                );
                let _ = request.respond(resp);
            }
        });
        format!("http://{address}")
    }

    fn start_app_server(app: App) -> String {
        let server = Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let (tx, rx) = mpsc::channel::<HttpRequest>();
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..2 {
            let rx = Arc::clone(&rx);
            let app = app.clone();
            thread::spawn(move || {
                while let Ok(mut request) = rx.lock().expect("request queue lock").recv() {
                    let response = app.handle(&mut request);
                    let _ = request.respond(response);
                }
            });
        }
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let _ = tx.send(request);
            }
        });
        format!("http://{address}")
    }

    #[test]
    fn e2e_request_side_injects_reasoning_content() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let upstream_body = serde_json::to_vec(&json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"completion_tokens": 3}
        }))
        .unwrap();
        let upstream_url =
            start_mock_upstream(Arc::clone(&capture), upstream_body, "application/json");
        let app = App {
            nodes: vec![upstream_url],
            ..app_for_test()
        };
        let base = start_app_server(app);
        let client = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .new_agent();
        let request_body = json!({
            "model": "some-model",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"}
            ],
            "reasoning_effort": "high",
            "stream": false
        });
        let response = client
            .post(&format!("{base}/v1/chat/completions"))
            .header("Content-Type", "application/json")
            .send(serde_json::to_vec(&request_body).unwrap())
            .expect("app should answer");
        assert_eq!(response.status().as_u16(), 200);
        let forwarded: Value =
            serde_json::from_slice(&capture.lock().unwrap()).expect("forwarded body is JSON");
        assert_eq!(
            forwarded["reasoning_content"],
            json!(""),
            "top-level reasoning_content should be injected, got {forwarded}"
        );
        let assistant = forwarded["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message forwarded");
        assert_eq!(assistant["reasoning_content"], json!(""));
    }

    #[test]
    fn e2e_response_side_adds_missing_reasoning_content() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let upstream_body = serde_json::to_vec(&json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "answer"}, "finish_reason": "stop"}],
            "usage": {"completion_tokens": 12, "completion_tokens_details": {"reasoning_tokens": 9}}
        }))
        .unwrap();
        let upstream_url =
            start_mock_upstream(Arc::clone(&capture), upstream_body, "application/json");
        let app = App {
            nodes: vec![upstream_url],
            ..app_for_test()
        };
        let base = start_app_server(app);
        let client = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .new_agent();
        let request_body = json!({
            "model": "some-model",
            "messages": [{"role": "user", "content": "think hard"}],
            "reasoning_effort": "high",
            "stream": false
        });
        let response = client
            .post(&format!("{base}/v1/chat/completions"))
            .header("Content-Type", "application/json")
            .send(serde_json::to_vec(&request_body).unwrap())
            .expect("app should answer");
        let payload: Value =
            serde_json::from_slice(&read_body(response.into_body())).expect("JSON response");
        assert_eq!(
            payload["reasoning_content"],
            json!(""),
            "missing reasoning_content should be filled, got {payload}"
        );
        assert_eq!(payload["choices"][0]["message"]["reasoning_content"], json!(""));
    }

    #[test]
    fn e2e_stream_side_adds_missing_reasoning_delta() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":7}}}\n\ndata: [DONE]\n\n";
        let upstream_url = start_mock_upstream(
            Arc::clone(&capture),
            sse.as_bytes().to_vec(),
            "text/event-stream; charset=utf-8",
        );
        let app = App {
            nodes: vec![upstream_url],
            ..app_for_test()
        };
        let base = start_app_server(app);
        let client = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .new_agent();
        let request_body = json!({
            "model": "some-model",
            "messages": [{"role": "user", "content": "think"}],
            "reasoning_effort": "high",
            "stream": true
        });
        let response = client
            .post(&format!("{base}/v1/chat/completions"))
            .header("Content-Type", "application/json")
            .send(serde_json::to_vec(&request_body).unwrap())
            .expect("app should answer");
        let body = read_body(response.into_body());
        let text = String::from_utf8(body).unwrap();
        let finish_line = text
            .lines()
            .find(|line| line.contains("finish_reason"))
            .expect("finish chunk present");
        assert!(
            finish_line.contains("\"reasoning_content\":\"\""),
            "finish chunk should carry fallback reasoning_content, got {finish_line}"
        );
    }
}
