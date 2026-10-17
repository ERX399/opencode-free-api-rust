// 入口：App 状态组装 + HTTP 服务主体。协议见 protocol.rs，转换器见 convert.rs，
// 路由判定见 routes.rs，保活自检见 keepalive.rs。
use std::{
    collections::HashMap,
    env,
    io::{Cursor, Read},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use tiny_http::{Method, Request as HttpRequest, Response, Server, StatusCode};
use ureq::Agent;

mod convert;
mod keepalive;
mod protocol;
mod routes;

use crate::convert::{response_with_retry_after, 
    aggregate_sse, append_cors, bad_request, cors_response, error_response, header,
    is_stream_request, json_response, response, thinking_mode, transform_stream_body,
};
use crate::keepalive::{primary_ip, public_ip, spawn_session_keepalive, unix_now};
use crate::protocol::{
    MAX_BODY_BYTES, UpstreamError, UpstreamResponse, add_headers, add_v2_session_headers,
    add_token, clear_node_cooldown, client, env_flag, env_or, env_seconds, gate_tool,
    node_cooldown_until, parse_retry_after, read_body_response, set_node_cooldown,
    request_header, split_nodes,
};
use crate::convert::{
    anthropic_stream_from_message, anthropic_to_openai, openai_to_anthropic, openai_to_responses,
    responses_stream_from_response, responses_to_openai,
};
use crate::routes::{
    is_anthropic_messages_path, is_chat_path, is_claude_models_path, is_health_path,
    is_models_path, is_responses_path, normalize_path, route_upstream_path, strip_free_suffix,
};

pub type ResponseBox = tiny_http::ResponseBox;
const FREE_MAP_TTL: u64 = 60;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const INDEX_HTML: &str = include_str!("../static/index.html");

#[derive(Clone)]
pub struct App {
    pub nodes: Vec<String>,
    pub token: Option<String>,
    pub auth_token: Option<String>,
    pub strip_free: bool,
    pub free_map: Arc<Mutex<FreeMap>>,
    pub client: Agent,
    pub stream_client: Agent,
    pub requests: Arc<AtomicU64>,
    pub port: String,
    pub local_ip: String,
    pub public_ip: Option<String>,
    pub started: u64,
}

pub struct FreeMap {
    pub entries: HashMap<String, String>,
    pub refreshed_at: u64,
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
            vec![crate::protocol::DEFAULT_NODE.to_owned()]
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
            started: unix_now(),
        }
    }
}

fn main() {
    let address = format!("{}:{}", env_or("HOST", "0.0.0.0"), env_or("PORT", "8788"));
    let app = App::from_env();
    spawn_session_keepalive();
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
        if method == Method::Post && is_anthropic_messages_path(path) {
            return self.anthropic_messages(request);
        }
        if method == Method::Post && is_responses_path(path) {
            return self.responses(request);
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
            Some("text/event-stream"),
            true,
        ) {
            Ok(response) => {
                // V2 上游只返回流式，非流式请求在本地聚合 SSE 为完整 JSON。
                let (parts, body) = response.into_parts();
                let status = parts.status.as_u16();
                if !(200..300).contains(&status) {
                    return proxy_response(http::Response::from_parts(parts, body));
                }
                match aggregate_sse(&body, thinking) {
                    Ok(aggregated) => {
                        let mut headers = vec![header("Content-Type", "application/json; charset=utf-8")];
                        append_cors(&mut headers);
                        let len = aggregated.len();
                        Response::new(StatusCode(200), headers, Cursor::new(aggregated), Some(len), None).boxed()
                    }
                    Err(message) => error_response(502, "upstream_error", &message),
                }
            }
            Err(error) => {
                if (400..500).contains(&error.status) && !error.body.is_empty() {
                    response_with_retry_after(
                        error.status,
                        error.body,
                        Some("application/json; charset=utf-8"),
                        None,
                        error.retry_after.as_deref(),
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
        // V2 上游只接受流式。下游请求统一转成 stream:true，非流式请求由本地聚合。
        // 若客户端未提供 tools，注入上游免费层门禁要求的 read 保留工具（schema 必须一致，否则 403）。
        // 上游门禁只看工具名：tools 里缺 read 就补门禁桩（含 tools 为空与非空两种情况）
        let has_read = payload
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| {
                tools.iter().any(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        == Some("read")
                })
            });
        if !has_read {
            match payload.get_mut("tools").and_then(Value::as_array_mut) {
                Some(tools) => tools.push(gate_tool()),
                None => payload["tools"] = json!([gate_tool()]),
            }
        }
        if payload.get("tool_choice").is_some_and(|c| c == "none") {
            // V2 拒绝 tool_choice:none，移除后靠保留工具描述约束
            if let Some(obj) = payload.as_object_mut() {
                obj.remove("tool_choice");
            }
        }
        payload["stream"] = Value::Bool(true);
        payload["stream_options"] = json!({"include_usage": true});
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
                    response_with_retry_after(
                        error.status,
                        error.body,
                        Some("text/event-stream; charset=utf-8"),
                        None,
                        error.retry_after.as_deref(),
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
        id.to_lowercase().contains("free")
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
                retry_after: None,
                node: None,
            });
        }
        let agent = if stream {
            &self.stream_client
        } else {
            &self.client
        };
        let mut failures = Vec::with_capacity(self.nodes.len());
        let now_ms = unix_now();
        for node in &self.nodes {
            if let Some(until) = node_cooldown_until(node) {
                if now_ms < until {
                    eprintln!("跳过冷却中的节点 {node}（剩余 {} 秒）", (until - now_ms) / 1000);
                    failures.push(format!("{node}: cooling down"));
                    continue;
                }
                clear_node_cooldown(node);
            }
            let url = format!("{node}{path}");
            let outcome = match method {
                Method::Post => {
                    let builder = add_headers(agent.post(&url), accept, &self.token);
                    let builder = if stream {
                        add_v2_session_headers(builder, &self.token)
                    } else {
                        builder
                    };
                    builder
                        .header("Content-Type", "application/json")
                        .send(&body)
                        .map(read_body_response)
                }
                Method::Get => {
                    let builder = add_headers(agent.get(&url), accept, &self.token);
                    let builder = if path == "/models" {
                        builder.header("Accept", "application/json")
                    } else {
                        builder
                    };
                    builder.call().map(read_body_response)
                }
                _ => continue,
            };
            match outcome {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status();
                        let retry_after = response
                            .headers()
                            .get("Retry-After")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        // 上游限流：把该节点冷却到 Retry-After 指定时间
                        if status.as_u16() == 429 {
                            if let Some(value) = &retry_after {
                                if let Some(until) = parse_retry_after(value) {
                                    set_node_cooldown(node, until);
                                    eprintln!("节点 {node} 触发限流，冷却至 {value} 秒后");
                                }
                            }
                        }
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
                                retry_after,
                                node: Some(node.clone()),
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
            retry_after: None,
            node: None,
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


impl App {
    // Anthropic Messages 协议：转 OpenAI chat 上游，响应转回 Anthropic 格式
    fn anthropic_messages(&self, request: &mut HttpRequest) -> ResponseBox {
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
        let stream = payload.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let model_hint = payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let chat = anthropic_to_openai(&payload);
        let chat_body = self.remap_chat_body(chat);
        match self.upstream(
            "/chat/completions",
            Method::Post,
            chat_body,
            Some("text/event-stream"),
            true,
        ) {
            Ok(response) => {
                let (parts, sse) = response.into_parts();
                let status = parts.status.as_u16();
                if !(200..300).contains(&status) {
                    return proxy_response(http::Response::from_parts(parts, sse));
                }
                match aggregate_sse(&sse, false) {
                    Ok(aggregated) => {
                        let message = openai_to_anthropic(&aggregated, &model_hint, false);
                        if stream {
                            let sse_text = anthropic_stream_from_message(&message);
                            let mut headers = vec![
                                header("Content-Type", "text/event-stream; charset=utf-8"),
                                header("Cache-Control", "no-cache"),
                            ];
                            append_cors(&mut headers);
                            Response::new(
                                StatusCode(200),
                                headers,
                                Cursor::new(sse_text.into_bytes()),
                                None,
                                None,
                            )
                            .boxed()
                        } else {
                            let mut headers =
                                vec![header("Content-Type", "application/json; charset=utf-8")];
                            append_cors(&mut headers);
                            let len = message.len();
                            Response::new(
                                StatusCode(200),
                                headers,
                                Cursor::new(message),
                                Some(len),
                                None,
                            )
                            .boxed()
                        }
                    }
                    Err(message) => error_response(502, "upstream_error", &message),
                }
            }
            Err(error) => {
                if (400..500).contains(&error.status) && !error.body.is_empty() {
                    response_with_retry_after(
                        error.status,
                        error.body,
                        Some("application/json; charset=utf-8"),
                        None,
                        error.retry_after.as_deref(),
                    )
                } else {
                    error_response(error.status, "upstream_error", &error.message)
                }
            }
        }
    }

    // OpenAI Responses 协议：转 OpenAI chat 上游，响应转回 Responses 格式
    fn responses(&self, request: &mut HttpRequest) -> ResponseBox {
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
        let stream = payload.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let model_hint = payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let chat = responses_to_openai(&payload);
        let chat_body = self.remap_chat_body(chat);
        match self.upstream(
            "/chat/completions",
            Method::Post,
            chat_body,
            Some("text/event-stream"),
            true,
        ) {
            Ok(response) => {
                let (parts, sse) = response.into_parts();
                let status = parts.status.as_u16();
                if !(200..300).contains(&status) {
                    return proxy_response(http::Response::from_parts(parts, sse));
                }
                match aggregate_sse(&sse, false) {
                    Ok(aggregated) => {
                        let output = openai_to_responses(&aggregated, &model_hint);
                        if stream {
                            let sse_text = responses_stream_from_response(&output);
                            let mut headers = vec![
                                header("Content-Type", "text/event-stream; charset=utf-8"),
                                header("Cache-Control", "no-cache"),
                            ];
                            append_cors(&mut headers);
                            Response::new(
                                StatusCode(200),
                                headers,
                                Cursor::new(sse_text.into_bytes()),
                                None,
                                None,
                            )
                            .boxed()
                        } else {
                            let mut headers =
                                vec![header("Content-Type", "application/json; charset=utf-8")];
                            append_cors(&mut headers);
                            let len = output.len();
                            Response::new(
                                StatusCode(200),
                                headers,
                                Cursor::new(output),
                                Some(len),
                                None,
                            )
                            .boxed()
                        }
                    }
                    Err(message) => error_response(502, "upstream_error", &message),
                }
            }
            Err(error) => {
                if (400..500).contains(&error.status) && !error.body.is_empty() {
                    response_with_retry_after(
                        error.status,
                        error.body,
                        Some("application/json; charset=utf-8"),
                        None,
                        error.retry_after.as_deref(),
                    )
                } else {
                    error_response(error.status, "upstream_error", &error.message)
                }
            }
        }
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{ensure_completion_reasoning, stream_chunk_identity, stream_has_reasoning};
    use crate::protocol::{DEFAULT_NODE, read_body, request_header};
    use serde_json::json;
    use tiny_http::Header;

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
        assert!(rx.recv().unwrap().as_deref().is_some_and(|ua| ua.starts_with("opencode/")));
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
