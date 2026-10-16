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

// ---------- Anthropic Messages 协议转换 ----------

// Anthropic 请求 -> OpenAI chat 请求
pub fn anthropic_to_openai(payload: &Value) -> Value {
    let mut out = json!({ "model": payload.get("model").cloned().unwrap_or(Value::Null) });
    if let Some(m) = payload.get("max_tokens") { out["max_tokens"] = m.clone(); }
    if let Some(t) = payload.get("temperature") { out["temperature"] = t.clone(); }
    if let Some(t) = payload.get("top_p") { out["top_p"] = t.clone(); }
    let thinking = payload.get("thinking").and_then(|t| t.get("type")).and_then(Value::as_str) == Some("enabled");
    if thinking {
        out["reasoning_effort"] = json!("high");
    }
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = payload.get("system") {
        let text = match system {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }
    if let Some(Value::Array(items)) = payload.get("messages") {
        for item in items {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user").to_owned();
            match item.get("content") {
                Some(Value::String(text)) => {
                    messages.push(json!({ "role": role, "content": text }));
                }
                Some(Value::Array(blocks)) => {
                    let mut text_parts: Vec<String> = Vec::new();
                    let mut tool_calls: Vec<Value> = Vec::new();
                    let mut tool_results: Vec<Value> = Vec::new();
                    for block in blocks {
                        let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                        match btype {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    text_parts.push(t.to_owned());
                                }
                            }
                            "tool_use" => {
                                let input = block.get("input").cloned().unwrap_or(json!({}));
                                tool_calls.push(json!({
                                    "id": block.get("id").cloned().unwrap_or(json!("call_0")),
                                    "type": "function",
                                    "function": {
                                        "name": block.get("name").cloned().unwrap_or(json!("")),
                                        "arguments": serde_json::to_string(&input).unwrap_or_default()
                                    }
                                }));
                            }
                            "tool_result" => {
                                let content = match block.get("content") {
                                    Some(Value::String(s)) => s.clone(),
                                    Some(Value::Array(a)) => a
                                        .iter()
                                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                                        .collect::<Vec<_>>()
                                        .join("\n"),
                                    _ => String::new(),
                                };
                                tool_results.push(json!({
                                    "role": "tool",
                                    "tool_call_id": block.get("tool_use_id").cloned().unwrap_or(json!("")),
                                    "content": content
                                }));
                            }
                            _ => {}
                        }
                    }
                    if !text_parts.is_empty() || !tool_calls.is_empty() {
                        let mut msg = json!({ "role": role });
                        if !text_parts.is_empty() {
                            msg["content"] = json!(text_parts.join("\n"));
                        }
                        if !tool_calls.is_empty() {
                            msg["tool_calls"] = json!(tool_calls);
                        }
                        messages.push(msg);
                    }
                    messages.extend(tool_results);
                }
                _ => {}
            }
        }
    }
    out["messages"] = json!(messages);
    if let Some(Value::Array(tools)) = payload.get("tools") {
        let conv: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned()?,
                        "description": t.get("description").cloned().unwrap_or(json!("")),
                        "parameters": t.get("input_schema").cloned().unwrap_or(json!({"type":"object","properties":{}}))
                    }
                }))
            })
            .collect();
        if !conv.is_empty() {
            out["tools"] = json!(conv);
        }
    }
    if let Some(tc) = payload.get("tool_choice") {
        match tc.get("type").and_then(Value::as_str) {
            Some("auto") => { out["tool_choice"] = json!("auto"); }
            Some("any") => { out["tool_choice"] = json!("required"); }
            Some("tool") => {
                out["tool_choice"] = json!({
                    "type": "function",
                    "function": { "name": tc.get("name").cloned().unwrap_or(json!("")) }
                });
            }
            _ => {}
        }
    }
    out
}

// 聚合后的 OpenAI 响应 -> Anthropic message JSON
pub fn openai_to_anthropic(openai: &[u8], model_hint: &str, thinking: bool) -> Vec<u8> {
    let payload: Value = serde_json::from_slice(openai).unwrap_or(json!({}));
    let message = payload.pointer("/choices/0/message").cloned().unwrap_or(json!({}));
    let mut content: Vec<Value> = Vec::new();
    if thinking {
        if let Some(rc) = message.get("reasoning_content").and_then(Value::as_str) {
            if !rc.is_empty() {
                content.push(json!({ "type": "thinking", "thinking": rc }));
            }
        }
    }
    let mut stop_reason = "end_turn";
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(json!({ "type": "text", "text": text }));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let args: Value = serde_json::from_str(
                call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}"),
            )
            .unwrap_or(json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": call.get("id").cloned().unwrap_or(json!("toolu_0")),
                "name": call.pointer("/function/name").cloned().unwrap_or(json!("")),
                "input": args
            }));
        }
        stop_reason = "tool_use";
    }
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    if let Some(u) = payload.get("usage") {
        input_tokens = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        output_tokens = u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    }
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("msg_0").to_owned();
    let model = payload.get("model").and_then(Value::as_str).unwrap_or(model_hint).to_owned();
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens }
    })
    .to_string()
    .into_bytes()
}

// Anthropic message JSON -> 完整 Anthropic SSE 事件流（单 delta 快照式）
pub fn anthropic_stream_from_message(message: &[u8]) -> String {
    let payload: Value = serde_json::from_slice(message).unwrap_or(json!({}));
    let mut out = String::new();
    let start = json!({
        "type": "message_start",
        "message": {
            "id": payload.get("id").cloned().unwrap_or(json!("msg_0")),
            "type": "message",
            "role": "assistant",
            "model": payload.get("model").cloned().unwrap_or(json!("")),
            "content": [],
            "usage": { "input_tokens": payload.pointer("/usage/input_tokens").cloned().unwrap_or(json!(0)), "output_tokens": 0 }
        }
    });
    out.push_str(&format!("event: message_start\ndata: {start}\n\n"));
    let mut index = 0u64;
    if let Some(blocks) = payload.get("content").and_then(Value::as_array) {
        for block in blocks {
            let btype = block.get("type").and_then(Value::as_str).unwrap_or("text");
            match btype {
                "text" | "thinking" => {
                    let (cb_key, delta_key) = if btype == "text" {
                        ("text", "text_delta")
                    } else {
                        ("thinking", "thinking_delta")
                    };
                    let text = block.get(cb_key).and_then(Value::as_str).unwrap_or("");
                    let cb_start = json!({ "type": "content_block_start", "index": index, "content_block": { "type": btype, cb_key: "" } });
                    out.push_str(&format!("event: content_block_start\ndata: {cb_start}\n\n"));
                    let delta = json!({ "type": "content_block_delta", "index": index, "delta": { "type": delta_key, cb_key: text } });
                    out.push_str(&format!("event: content_block_delta\ndata: {delta}\n\n"));
                    let cb_stop = json!({ "type": "content_block_stop", "index": index });
                    out.push_str(&format!("event: content_block_stop\ndata: {cb_stop}\n\n"));
                }
                "tool_use" => {
                    let cb_start = json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": block.get("id").cloned().unwrap_or(json!("toolu_0")),
                            "name": block.get("name").cloned().unwrap_or(json!("")),
                            "input": {}
                        }
                    });
                    out.push_str(&format!("event: content_block_start\ndata: {cb_start}\n\n"));
                    let partial = serde_json::to_string(block.get("input").unwrap_or(&json!({}))).unwrap_or_default();
                    let delta = json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": partial } });
                    out.push_str(&format!("event: content_block_delta\ndata: {delta}\n\n"));
                    let cb_stop = json!({ "type": "content_block_stop", "index": index });
                    out.push_str(&format!("event: content_block_stop\ndata: {cb_stop}\n\n"));
                }
                _ => {}
            }
            index += 1;
        }
    }
    let msg_delta = json!({
        "type": "message_delta",
        "delta": { "stop_reason": payload.get("stop_reason").cloned().unwrap_or(json!("end_turn")), "stop_sequence": Value::Null },
        "usage": { "output_tokens": payload.pointer("/usage/output_tokens").cloned().unwrap_or(json!(0)) }
    });
    out.push_str(&format!("event: message_delta\ndata: {msg_delta}\n\n"));
    out.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    out
}

// ---------- OpenAI Responses 协议转换 ----------

// Responses 请求 -> OpenAI chat 请求
pub fn responses_to_openai(payload: &Value) -> Value {
    let mut out = json!({ "model": payload.get("model").cloned().unwrap_or(Value::Null) });
    if let Some(m) = payload.get("max_output_tokens") { out["max_tokens"] = m.clone(); }
    if let Some(t) = payload.get("temperature") { out["temperature"] = t.clone(); }
    if let Some(t) = payload.get("top_p") { out["top_p"] = t.clone(); }
    if let Some(e) = payload.get("reasoning").and_then(|r| r.get("effort")) {
        if e.as_str().is_some_and(|s| !s.is_empty()) {
            out["reasoning_effort"] = e.clone();
        }
    }
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instr) = payload.get("instructions").and_then(Value::as_str) {
        if !instr.is_empty() {
            messages.push(json!({ "role": "system", "content": instr }));
        }
    }
    match payload.get("input") {
        Some(Value::String(s)) => messages.push(json!({ "role": "user", "content": s })),
        Some(Value::Array(items)) => {
            for item in items {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user").to_owned();
                let mut text_parts: Vec<String> = Vec::new();
                match item.get("content") {
                    Some(Value::String(s)) => text_parts.push(s.clone()),
                    Some(Value::Array(parts)) => {
                        for part in parts {
                            let ptype = part.get("type").and_then(Value::as_str).unwrap_or("");
                            if matches!(ptype, "input_text" | "output_text" | "text") {
                                if let Some(t) = part.get("text").and_then(Value::as_str) {
                                    text_parts.push(t.to_owned());
                                }
                            }
                        }
                    }
                    _ => {}
                }
                if !text_parts.is_empty() {
                    messages.push(json!({ "role": role, "content": text_parts.join("\n") }));
                }
            }
        }
        _ => {}
    }
    out["messages"] = json!(messages);
    if let Some(Value::Array(tools)) = payload.get("tools") {
        let conv: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name").cloned()?;
                let desc = t.get("description").cloned().unwrap_or(json!(""));
                let params = t.get("parameters").cloned().unwrap_or(json!({"type":"object","properties":{}}));
                Some(json!({ "type": "function", "function": { "name": name, "description": desc, "parameters": params } }))
            })
            .collect();
        if !conv.is_empty() {
            out["tools"] = json!(conv);
        }
    }
    out
}

// 聚合后的 OpenAI 响应 -> Responses JSON
pub fn openai_to_responses(openai: &[u8], model_hint: &str) -> Vec<u8> {
    let payload: Value = serde_json::from_slice(openai).unwrap_or(json!({}));
    let message = payload.pointer("/choices/0/message").cloned().unwrap_or(json!({}));
    let text = message.get("content").and_then(Value::as_str).unwrap_or("").to_owned();
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    if let Some(u) = payload.get("usage") {
        input_tokens = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        output_tokens = u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    }
    let item_id = payload.get("id").and_then(Value::as_str).unwrap_or("msg_0").to_owned();
    let id = format!("resp_{item_id}");
    let model = payload.get("model").and_then(Value::as_str).unwrap_or(model_hint).to_owned();
    json!({
        "id": id,
        "object": "response",
        "created_at": payload.get("created").cloned().unwrap_or(json!(0)),
        "status": "completed",
        "model": model,
        "output": [{
            "type": "message",
            "id": item_id,
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }]
        }],
        "output_text": text,
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens, "total_tokens": input_tokens + output_tokens }
    })
    .to_string()
    .into_bytes()
}

// Responses JSON -> Responses SSE 事件流（created / delta / completed）
pub fn responses_stream_from_response(response: &[u8]) -> String {
    let payload: Value = serde_json::from_slice(response).unwrap_or(json!({}));
    let text = payload.get("output_text").and_then(Value::as_str).unwrap_or("");
    let mut out = String::new();
    let created = json!({ "type": "response.created", "response": payload.clone() });
    out.push_str(&format!("event: response.created\ndata: {created}\n\n"));
    let delta = json!({
        "type": "response.output_text.delta",
        "item_id": payload.pointer("/output/0/id").cloned().unwrap_or(json!("msg_0")),
        "output_index": 0,
        "content_index": 0,
        "delta": text
    });
    out.push_str(&format!("event: response.output_text.delta\ndata: {delta}\n\n"));
    let completed = json!({ "type": "response.completed", "response": payload });
    out.push_str(&format!("event: response.completed\ndata: {completed}\n\n"));
    out
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn anthropic_request_converts_to_chat() {
        let req = json!({
            "model": "deepseek-v4.1-flash-free",
            "max_tokens": 100,
            "system": "be brief",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "tu1", "name": "bash", "input": {"x": "1"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu1", "content": "ok"}]}
            ],
            "tools": [{"name": "bash", "description": "run", "input_schema": {"type": "object", "properties": {}}}],
            "tool_choice": {"type": "auto"}
        });
        let out = anthropic_to_openai(&req);
        assert_eq!(out["messages"].as_array().unwrap().len(), 4); // system + user + assistant(tool_use) + tool
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][2]["role"], "assistant");
        assert_eq!(out["messages"][3]["role"], "tool");
        assert_eq!(out["tools"][0]["function"]["name"], "bash");
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn openai_response_converts_to_anthropic() {
        let openai = json!({
            "id": "chatcmpl-1", "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        });
        let out: Value = serde_json::from_slice(&openai_to_anthropic(&serde_json::to_vec(&openai).unwrap(), "m", false)).unwrap();
        assert_eq!(out["type"], "message");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "hello");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["input_tokens"], 5);
    }

    #[test]
    fn anthropic_stream_contains_required_events() {
        let message = json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let sse = anthropic_stream_from_message(&serde_json::to_vec(&message).unwrap());
        for event in ["message_start", "content_block_start", "content_block_delta", "content_block_stop", "message_delta", "message_stop"] {
            assert!(sse.contains(&format!("event: {event}")), "missing {event}");
        }
        assert!(sse.contains("text_delta"));
    }

    #[test]
    fn responses_request_converts_to_chat() {
        let req = json!({
            "model": "deepseek-v4.1-flash-free",
            "instructions": "sys",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ],
            "max_output_tokens": 50
        });
        let out = responses_to_openai(&req);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["content"], "hi");
        assert_eq!(out["max_tokens"], 50);
    }

    #[test]
    fn openai_response_converts_to_responses_format() {
        let openai = json!({
            "id": "chatcmpl-1", "model": "m", "created": 123,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hey"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1}
        });
        let out: Value = serde_json::from_slice(&openai_to_responses(&serde_json::to_vec(&openai).unwrap(), "m")).unwrap();
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(out["output_text"], "hey");
        assert_eq!(out["usage"]["total_tokens"], 4);
    }

    #[test]
    fn responses_stream_contains_required_events() {
        let response = json!({
            "id": "resp_1", "object": "response", "status": "completed", "model": "m",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant", "content": []}],
            "output_text": "x", "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        });
        let sse = responses_stream_from_response(&serde_json::to_vec(&response).unwrap());
        for event in ["response.created", "response.output_text.delta", "response.completed"] {
            assert!(sse.contains(&format!("event: {event}")), "missing {event}");
        }
    }
}
