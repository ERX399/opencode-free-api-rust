use std::io::{self, Write};

use serde_json::{Value, json};
// 喵喵喵？喵喵？喵喵喵！！！喵？

use crate::convert::{
    fallback_reasoning_chunk_from, inject_reasoning_into_finish_block, stream_chunk_identity,
    stream_has_reasoning,
};

const FORCE_FLUSH_BYTES: usize = 1 << 20;
const DONE_MARKER: &[u8] = b"[DONE]";

pub struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut blocks = Vec::new();
        while let Some((end, width)) = self.boundary() {
            let rest = self.buf.split_off(end + width);
            let block = std::mem::replace(&mut self.buf, rest);
            blocks.push(decode(&block));
        }
        if self.buf.len() > FORCE_FLUSH_BYTES {
            let (text, used) = decode_prefix(&self.buf);
            self.buf.drain(..used);
            if !text.is_empty() {
                blocks.push(text);
            }
        }
        blocks
    }

    pub fn finish(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let block = std::mem::take(&mut self.buf);
        let text = decode(&block);
        if text.trim().is_empty() {
            return None;
        }
        Some(format!("{text}\n\n"))
    }

    fn boundary(&self) -> Option<(usize, usize)> {
        let mut index = 0;
        while index + 1 < self.buf.len() {
            if self.buf[index] == b'\n' && self.buf[index + 1] == b'\n' {
                return Some((index, 2));
            }
            if self.buf[index] == b'\r'
                && self.buf[index + 1] == b'\n'
                && index + 3 < self.buf.len()
                && self.buf[index + 2] == b'\r'
                && self.buf[index + 3] == b'\n'
            {
                return Some((index, 4));
            }
            index += 1;
        }
        None
    }
}

fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

fn decode_prefix(bytes: &[u8]) -> (String, usize) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.replace("\r\n", "\n"), bytes.len()),
        Err(error) if error.error_len().is_none() => {
            let valid = error.valid_up_to();
            (decode(&bytes[..valid]), valid)
        }
        Err(_) => (decode(bytes), bytes.len()),
    }
}

pub struct ReasoningFallback {
    enabled: bool,
    seen: bool,
    injected: bool,
    identity: Option<Value>,
}

impl ReasoningFallback {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            seen: false,
            injected: false,
            identity: None,
        }
    }

    pub fn apply(&mut self, block: &str) -> String {
        if !self.enabled {
            return block.to_owned();
        }
        if stream_has_reasoning(block) {
            self.seen = true;
            return block.to_owned();
        }
        if self.identity.is_none() {
            self.identity = stream_chunk_identity(block);
        }
        if !self.seen && !self.injected {
            if let Some(rewritten) = inject_reasoning_into_finish_block(block) {
                self.injected = true;
                return rewritten;
            }
            if block.contains("data: [DONE]") {
                self.injected = true;
                return format!("{}{block}", self.fallback());
            }
        }
        block.to_owned()
    }

    pub fn finish(&mut self) -> Option<String> {
        if !self.enabled || self.seen || self.injected {
            return None;
        }
        self.injected = true;
        Some(self.fallback())
    }

    fn fallback(&self) -> String {
        fallback_reasoning_chunk_from(self.identity.as_ref())
    }
}

pub struct ChunkedWriter {
    writer: Box<dyn Write + Send + 'static>,
    finished: bool,
}

impl ChunkedWriter {
    pub fn new(mut writer: Box<dyn Write + Send + 'static>, status: u16) -> io::Result<Self> {
        let reason = tiny_http::StatusCode(status).default_reason_phrase();
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\n\
             Content-Type: text/event-stream; charset=utf-8\r\n\
             Cache-Control: no-cache\r\n\
             Access-Control-Allow-Origin: *\r\n\
             Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
             Access-Control-Allow-Headers: Content-Type, Authorization\r\n\
             Transfer-Encoding: chunked\r\n\
             Connection: keep-alive\r\n\r\n"
        );
        writer.write_all(head.as_bytes())?;
        writer.flush()?;
        Ok(Self {
            writer,
            finished: false,
        })
    }

    pub fn write_bytes(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.writer
            .write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
        self.writer.write_all(data)?;
        self.writer.write_all(b"\r\n")?;
        self.writer.flush()
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.writer.write_all(b"0\r\n\r\n")?;
        self.writer.flush()
    }
}

impl Drop for ChunkedWriter {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish();
        }
    }
}

pub fn saw_done(tail: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let mut probe = Vec::with_capacity(tail.len() + bytes.len());
    probe.extend_from_slice(tail);
    probe.extend_from_slice(bytes);
    let found = probe
        .windows(DONE_MARKER.len())
        .any(|window| window == DONE_MARKER);
    let keep = DONE_MARKER.len() - 1;
    let start = probe.len().saturating_sub(keep);
    *tail = probe[start..].to_vec();
    found
}

pub trait LiveSink {
    fn feed(&mut self, bytes: Vec<u8>) -> Vec<u8>;
    fn heartbeat(&mut self) -> Vec<u8>;
    fn shutdown(&mut self, failure: Option<&str>) -> Vec<u8>;
}

pub struct ChatSink {
    thinking: bool,
    parser: SseBuffer,
    fallback: ReasoningFallback,
    tail: Vec<u8>,
    forwarded: bool,
    done: bool,
}

impl ChatSink {
    pub fn new(thinking: bool) -> Self {
        Self {
            thinking,
            parser: SseBuffer::new(),
            fallback: ReasoningFallback::new(thinking),
            tail: Vec::new(),
            forwarded: false,
            done: false,
        }
    }
}

impl LiveSink for ChatSink {
    fn feed(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        self.forwarded = true;
        if !self.thinking {
            self.done |= saw_done(&mut self.tail, &bytes);
            return bytes;
        }
        let mut out = String::new();
        for block in self.parser.push(&bytes) {
            self.done |= block.contains("data: [DONE]");
            out.push_str(&self.fallback.apply(&block));
        }
        out.into_bytes()
    }

    fn heartbeat(&mut self) -> Vec<u8> {
        b": keepalive\n\n".to_vec()
    }

    fn shutdown(&mut self, failure: Option<&str>) -> Vec<u8> {
        let mut out = String::new();
        if self.thinking {
            if let Some(block) = self.parser.finish() {
                self.done |= block.contains("data: [DONE]");
                out.push_str(&self.fallback.apply(&block));
            }
            if let Some(extra) = self.fallback.finish() {
                out.push_str(&extra);
            }
        }
        if let Some(message) = failure {
            if !self.forwarded {
                let frame = json!({"error": {"type": "upstream_error", "message": message}});
                out.push_str(&format!("data: {frame}\n\n"));
            }
        }
        if !self.done {
            out.push_str("data: [DONE]\n\n");
        }
        out.into_bytes()
    }
}

struct OpenBlock {
    index: u64,
    key: Option<i64>,
}

pub struct AnthropicSink {
    parser: SseBuffer,
    model_hint: String,
    id: Option<String>,
    model: Option<String>,
    started: bool,
    next_index: u64,
    open: Option<OpenBlock>,
    opened: bool,
    used_tool: bool,
    tool_seq: u64,
    input_tokens: u64,
    output_tokens: u64,
}

impl AnthropicSink {
    pub fn new(model_hint: String) -> Self {
        Self {
            parser: SseBuffer::new(),
            model_hint,
            id: None,
            model: None,
            started: false,
            next_index: 0,
            open: None,
            opened: false,
            used_tool: false,
            tool_seq: 0,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn handle(&mut self, block: &str, out: &mut String) {
        let Some(data) = data_payload(block) else {
            return;
        };
        if data.trim() == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        if !self.started {
            self.start(&value);
            let event = self.start_event();
            emit(out, "message_start", &event);
        }
        if let Some(usage) = value.get("usage") {
            if let Some(tokens) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                self.input_tokens = tokens;
            }
            if let Some(tokens) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.output_tokens = tokens;
            }
        }
        let Some(delta) = value.pointer("/choices/0/delta") else {
            return;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                self.text(text, out);
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.tool(call, out);
            }
        }
    }

    fn start(&mut self, value: &Value) {
        self.started = true;
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            self.id = Some(id.to_owned());
        }
        if let Some(model) = value.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_owned());
        }
    }

    fn start_event(&self) -> Value {
        json!({
            "type": "message_start",
            "message": {
                "id": self.id.clone().unwrap_or_else(|| "msg_0".to_owned()),
                "type": "message",
                "role": "assistant",
                "model": self.model.clone().unwrap_or_else(|| self.model_hint.clone()),
                "content": [],
                "usage": { "input_tokens": self.input_tokens, "output_tokens": 0 }
            }
        })
    }

    fn text(&mut self, text: &str, out: &mut String) {
        self.open_block(None, out);
        let event = json!({
            "type": "content_block_delta",
            "index": self.index(),
            "delta": { "type": "text_delta", "text": text }
        });
        emit(out, "content_block_delta", &event);
    }

    fn tool(&mut self, call: &Value, out: &mut String) {
        let key = call.get("index").and_then(Value::as_i64).unwrap_or(0);
        if self.open.as_ref().and_then(|block| block.key) != Some(key) {
            self.close(out);
            self.tool_seq += 1;
            let index = self.next_index;
            self.next_index += 1;
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("toolu_{}", self.tool_seq));
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let event = json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
            });
            emit(out, "content_block_start", &event);
            self.open = Some(OpenBlock {
                index,
                key: Some(key),
            });
            self.opened = true;
            self.used_tool = true;
        }
        if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
            if !args.is_empty() {
                let event = json!({
                    "type": "content_block_delta",
                    "index": self.index(),
                    "delta": { "type": "input_json_delta", "partial_json": args }
                });
                emit(out, "content_block_delta", &event);
            }
        }
    }

    fn open_block(&mut self, key: Option<i64>, out: &mut String) {
        if self.open.as_ref().map(|block| block.key) == Some(key) {
            return;
        }
        self.close(out);
        let index = self.next_index;
        self.next_index += 1;
        let event = json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "text", "text": "" }
        });
        emit(out, "content_block_start", &event);
        self.open = Some(OpenBlock { index, key: None });
        self.opened = true;
    }

    fn close(&mut self, out: &mut String) {
        if let Some(block) = self.open.take() {
            let event = json!({ "type": "content_block_stop", "index": block.index });
            emit(out, "content_block_stop", &event);
        }
    }

    fn index(&self) -> u64 {
        self.open.as_ref().map(|block| block.index).unwrap_or(0)
    }
}

impl LiveSink for AnthropicSink {
    fn feed(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        let mut out = String::new();
        for block in self.parser.push(&bytes) {
            self.handle(&block, &mut out);
        }
        out.into_bytes()
    }

    fn heartbeat(&mut self) -> Vec<u8> {
        b"event: ping\ndata: {\"type\":\"ping\"}\n\n".to_vec()
    }

    fn shutdown(&mut self, failure: Option<&str>) -> Vec<u8> {
        let mut out = String::new();
        if let Some(block) = self.parser.finish() {
            self.handle(&block, &mut out);
        }
        if let Some(message) = failure {
            let event = json!({
                "type": "error",
                "error": { "type": "api_error", "message": message }
            });
            emit(&mut out, "error", &event);
            return out.into_bytes();
        }
        if !self.started {
            self.started = true;
            let event = self.start_event();
            emit(&mut out, "message_start", &event);
        }
        self.close(&mut out);
        if !self.opened {
            self.open_block(None, &mut out);
            let event = json!({
                "type": "content_block_delta",
                "index": self.index(),
                "delta": { "type": "text_delta", "text": "" }
            });
            emit(&mut out, "content_block_delta", &event);
            self.close(&mut out);
        }
        let stop = if self.used_tool { "tool_use" } else { "end_turn" };
        let event = json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop, "stop_sequence": Value::Null },
            "usage": { "output_tokens": self.output_tokens }
        });
        emit(&mut out, "message_delta", &event);
        emit(&mut out, "message_stop", &json!({ "type": "message_stop" }));
        out.into_bytes()
    }
}

fn emit(out: &mut String, name: &str, event: &Value) {
    out.push_str(&format!("event: {name}\ndata: {event}\n\n"));
}

fn data_payload(block: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            parts.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

pub struct ResponsesSink {
    parser: SseBuffer,
    model_hint: String,
    id: Option<String>,
    model: Option<String>,
    created: Option<Value>,
    text: String,
    input_tokens: u64,
    output_tokens: u64,
    started: bool,
}

impl ResponsesSink {
    pub fn new(model_hint: String) -> Self {
        Self {
            parser: SseBuffer::new(),
            model_hint,
            id: None,
            model: None,
            created: None,
            text: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            started: false,
        }
    }

    fn handle(&mut self, block: &str, out: &mut String) {
        let Some(data) = data_payload(block) else {
            return;
        };
        if data.trim() == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        if !self.started {
            self.started = true;
            if let Some(id) = value.get("id").and_then(Value::as_str) {
                self.id = Some(id.to_owned());
            }
            if let Some(model) = value.get("model").and_then(Value::as_str) {
                self.model = Some(model.to_owned());
            }
            if let Some(created) = value.get("created") {
                self.created = Some(created.clone());
            }
            let event = json!({
                "type": "response.created",
                "response": self.created_response()
            });
            emit(out, "response.created", &event);
        }
        if let Some(usage) = value.get("usage") {
            if let Some(tokens) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                self.input_tokens = tokens;
            }
            if let Some(tokens) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.output_tokens = tokens;
            }
        }
        let Some(delta) = value
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        self.text.push_str(delta);
        let event = json!({
            "type": "response.output_text.delta",
            "item_id": self.item_id(),
            "output_index": 0,
            "content_index": 0,
            "delta": delta
        });
        emit(out, "response.output_text.delta", &event);
    }

    fn item_id(&self) -> String {
        self.id.clone().unwrap_or_else(|| "msg_0".to_owned())
    }

    fn response_id(&self) -> String {
        format!("resp_{}", self.item_id())
    }

    fn model(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| self.model_hint.clone())
    }

    fn created(&self) -> Value {
        self.created.clone().unwrap_or(json!(0))
    }

    fn created_response(&self) -> Value {
        json!({
            "id": self.response_id(),
            "object": "response",
            "created_at": self.created(),
            "status": "in_progress",
            "model": self.model(),
            "output": [],
            "output_text": "",
            "usage": { "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 }
        })
    }

    fn completed_response(&self) -> Value {
        let item_id = self.item_id();
        let text = self.text.clone();
        json!({
            "id": self.response_id(),
            "object": "response",
            "created_at": self.created(),
            "status": "completed",
            "model": self.model(),
            "output": [{
                "type": "message",
                "id": item_id,
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": text, "annotations": [] }]
            }],
            "output_text": text,
            "usage": {
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
                "total_tokens": self.input_tokens + self.output_tokens
            }
        })
    }
}

impl LiveSink for ResponsesSink {
    fn feed(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        let mut out = String::new();
        for block in self.parser.push(&bytes) {
            self.handle(&block, &mut out);
        }
        out.into_bytes()
    }

    fn heartbeat(&mut self) -> Vec<u8> {
        b": keepalive\n\n".to_vec()
    }

    fn shutdown(&mut self, failure: Option<&str>) -> Vec<u8> {
        let mut out = String::new();
        if let Some(block) = self.parser.finish() {
            self.handle(&block, &mut out);
        }
        if let Some(message) = failure {
            let event = json!({ "type": "error", "message": message });
            emit(&mut out, "error", &event);
            return out.into_bytes();
        }
        if !self.started {
            self.started = true;
            let event = json!({
                "type": "response.created",
                "response": self.created_response()
            });
            emit(&mut out, "response.created", &event);
        }
        let event = json!({
            "type": "response.completed",
            "response": self.completed_response()
        });
        emit(&mut out, "response.completed", &event);
        out.into_bytes()
    }
}
