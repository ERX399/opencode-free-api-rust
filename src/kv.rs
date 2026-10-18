use std::{
    fs,
    path::PathBuf,
    sync::Mutex,
};

/// 简单 KV 持久化：JSON 文件，仅保存运行时可变设置（stream_live 等）。
/// 文件路径固定为二进制同目录下 `kv.json`，损坏时静默回退默认值。
pub struct Kv {
    path: PathBuf,
    inner: Mutex<serde_json::Value>,
}

impl Kv {
    pub fn open(path: PathBuf) -> Self {
        let value = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        Self {
            path,
            inner: Mutex::new(value),
        }
    }

    pub fn get_bool(&self, key: &str, fallback: bool) -> bool {
        self.inner
            .lock()
            .expect("kv lock")
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(fallback)
    }

    pub fn set_bool(&self, key: &str, value: bool) {
        {
            let mut guard = self.inner.lock().expect("kv lock");
            guard[key] = serde_json::Value::Bool(value);
        }
        self.flush();
    }

    fn flush(&self) {
        let guard = self.inner.lock().expect("kv lock");
        if let Ok(text) = serde_json::to_string_pretty(&*guard) {
            let tmp = self.path.with_extension("json.tmp");
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, &self.path);
            }
        }
    }
}