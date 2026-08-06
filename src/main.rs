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
    time::Duration,
};

use reqwest::blocking::{Client, Response as UpstreamResponse};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request as HttpRequest, Response, Server, StatusCode};

type ResponseBox = tiny_http::ResponseBox;

const MAX_BODY_BYTES: u64 = 64 << 20;
const DEFAULT_NODE: &str = "https://opencode.ai/zen/v1";
const EXTRA_FREE_MODELS: &[&str] = &["big-pickle"];
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
}

fn main() {
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
    };
    let workers = env::var("WORKERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .max(1);

    eprintln!(
        "listening on {address} with {} upstream node(s), {workers} worker(s)",
        app.nodes.len()
    );
    if app.auth_token.is_some() {
        eprintln!("authentication enabled via AUTH_TOKEN");
    }

    let server = Server::http(&address).expect("bind HTTP server");
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

impl App {
    fn handle(&self, request: &mut HttpRequest) -> ResponseBox {
        let method = request.method();
        let path = request.url();

        if method == &Method::Options {
            return cors_response(204, Vec::new());
        }
        if method == &Method::Get && path == "/health" {
            self.requests.fetch_add(1, Ordering::Relaxed);
            return json_response(
                200,
                json!({
                    "status": "ok",
                    "nodes": self.nodes,
                    "requests": self.requests.load(Ordering::Relaxed),
                    "upstream_failures": self.failures.load(Ordering::Relaxed),
                }),
            );
        }
        if self.auth_token.is_some() && !self.authorized(request) {
            return error_response(401, "invalid_api_key", "missing or invalid bearer token");
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        if method == &Method::Get && path == "/v1/models" {
            return self.models(request);
        }
        if method == &Method::Post && path == "/v1/chat/completions" {
            return self.chat_completions(request);
        }
        if method == &Method::Get {
            return index_response();
        }
        error_response(404, "not_found", "route not found")
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
                        eprintln!("streaming {path} via {node}");
                    }
                    return Ok(response);
                }
                Ok(response) => {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                    let message = format!("{node}: HTTP {}", response.status());
                    eprintln!("upstream failure {message}");
                    failures.push(message);
                }
                Err(error) => {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                    eprintln!("upstream failure {node}: {error}");
                    failures.push(format!("{node}: {error}"));
                }
            }
        }
        Err(failures.join("; "))
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

fn index_response() -> ResponseBox {
    let mut headers = vec![header("Content-Type", "text/html; charset=utf-8")];
    append_cors(&mut headers);
    let body = INDEX_HTML.as_bytes().to_vec();
    Response::new(
        StatusCode(200),
        headers,
        Cursor::new(body),
        Some(INDEX_HTML.len()),
        None,
    )
    .boxed()
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
