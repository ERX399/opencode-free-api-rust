pub type ResponseBox = tiny_http::ResponseBox;
// 请求/响应转换器：body 改写、SSE 聚合、reasoning 补偿、模型映射。
use serde_json::{json, Value};
use std::io::Cursor;
use tiny_http::{Header, Request as HttpRequest, Response, StatusCode};


pub fn cors_response(status: u16, body: Vec<u8>) -> ResponseBox {
    response(status, body, None, None)
}

pub fn json_response(status: u16, payload: Value) -> ResponseBox {
    let body = serde_json::to_vec(&payload).expect("serialize JSON response");
    response(status, body, Some("application/json; charset=utf-8"), None)
}

pub fn error_response(status: u16, kind: &str, message: &str) -> ResponseBox {
    json_response(status, json!({"error": {"type": kind, "message": message}}))
}

pub fn bad_request() -> ResponseBox {
    error_response(
        400,
        "invalid_request_error",
        "request body must be valid JSON up to 64 MiB",
    )
}

pub fn response(
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

pub fn append_cors(headers: &mut Vec<Header>) {
    headers.push(header("Access-Control-Allow-Origin", "*"));
    headers.push(header("Access-Control-Allow-Methods", "GET, POST, OPTIONS"));
    headers.push(header(
        "Access-Control-Allow-Headers",
        "Content-Type, Authorization",
    ));
}

pub fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).expect("valid static header")
}

pub fn request_header(request: &HttpRequest, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str().to_owned())
}

pub fn is_stream_request(payload: &Value) -> bool {
    payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub fn thinking_mode(payload: &Value) -> bool {
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

// 将上游 SSE 聚合为 OpenAI 非流式 JSON。thinking 时把 reasoning_content 合并进 message。
pub fn aggregate_sse(sse: &[u8], thinking: bool) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(sse).map_err(|_| "upstream stream is not utf-8".to_owned())?;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason: Option<Value> = None;
    let mut usage: Option<Value> = None;
    let mut identity: Option<Value> = None;
    let mut tool_calls: Vec<Value> = Vec::new();
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
        if identity.is_none() && (payload.get("id").is_some() || payload.get("model").is_some()) {
            identity = Some(payload.clone());
        }
        if payload.get("usage").is_some_and(|value| !value.is_null()) {
            usage = payload.get("usage").cloned();
        }
        let Some(choice) = payload.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else {
            continue;
        };
        if finish_reason.is_none() {
            finish_reason = choice.get("finish_reason").cloned().filter(|v| !v.is_null());
        }
        let Some(delta) = choice.get("delta") else {
            continue;
        };
        if let Some(part) = delta.get("reasoning_content").and_then(Value::as_str) {
            reasoning.push_str(part);
        }
        if let Some(part) = delta.get("content").and_then(Value::as_str) {
            content.push_str(part);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if tool_calls.len() <= index {
                    tool_calls.resize_with(index + 1, || json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}}));
                }
                let slot = &mut tool_calls[index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    slot["id"] = Value::String(id.to_owned());
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    slot["function"]["name"] = Value::String(name.to_owned());
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    if let Some(existing) = slot.pointer("/function/arguments").and_then(Value::as_str) {
                        let merged = format!("{existing}{args}");
                        slot["function"]["arguments"] = Value::String(merged);
                    }
                }
            }
        }
    }
    if content.is_empty() && reasoning.is_empty() && tool_calls.is_empty() {
        // 兼容 mock/非标准上游直接返回完整 JSON 的情况
        if let Ok(mut payload) = serde_json::from_slice::<Value>(sse) {
            if payload.get("choices").is_some() {
                if thinking {
                    if payload.get("reasoning_content").is_none() {
                        payload["reasoning_content"] = Value::String(String::new());
                    }
                    if let Some(choices) = payload.get_mut("choices").and_then(Value::as_array_mut) {
                        for choice in choices {
                            if let Some(message) = choice.get_mut("message") {
                                if message.get("reasoning_content").is_none() {
                                    message["reasoning_content"] = Value::String(String::new());
                                }
                            }
                        }
                    }
                }
                return serde_json::to_vec(&payload).map_err(|e| e.to_string());
            }
        }
        return Err("upstream stream contained no content".to_owned());
    }
    let mut message = json!({"role": "assistant"});
    if thinking && !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !content.is_empty() {
        message["content"] = Value::String(content);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let mut response = json!({
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason.unwrap_or(Value::String("stop".to_owned()))
        }]
    });
    if let Some(id) = &identity {
        for key in ["id", "created", "model"] {
            if let Some(value) = id.get(key) {
                response[key] = value.clone();
            }
        }
    }
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    if thinking {
        if response.get("reasoning_content").is_none() {
            response["reasoning_content"] = Value::String(String::new());
        }
        if let Some(choices) = response.get_mut("choices").and_then(Value::as_array_mut) {
            for choice in choices {
                if let Some(message) = choice.get_mut("message") {
                    if message.get("reasoning_content").is_none() {
                        message["reasoning_content"] = Value::String(String::new());
                    }
                }
            }
        }
    }
    serde_json::to_vec(&response).map_err(|error| error.to_string())
}

// 思考模式下客户端期望响应必带 reasoning_content；
// 实测上游 reasoning_tokens 记账不可靠（思考了也常报 0），
// 因此只按"字段缺失"补空串，不看 token 数
#[allow(dead_code)]
pub fn ensure_completion_reasoning(body: &mut Vec<u8>) {
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
pub fn stream_chunk_identity(text: &str) -> Option<Value> {
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

pub fn fallback_reasoning_chunk(text: &str) -> String {
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
pub fn stream_has_reasoning(text: &str) -> bool {
    text.contains("\"reasoning_content\"")
}

pub fn inject_reasoning_into_finish_block(block: &str) -> Option<String> {
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
pub fn transform_stream_body(body: &mut Vec<u8>) {
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
