// 路由与路径判定：health/models/chat 路径识别、上游路径改写。

pub fn is_health_path(path: &str) -> bool {
    matches!(
        path,
        "/health" | "/healthz" | "/ready" | "/api/health" | "/api/status"
    )
}

pub fn normalize_path(path: &str) -> &str {
    let normalized = path.trim_end_matches('/');
    if normalized.is_empty() {
        "/"
    } else {
        normalized
    }
}

pub fn is_models_path(path: &str) -> bool {
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

pub fn is_claude_models_path(path: &str) -> bool {
    matches!(path, "/claude/v1/models" | "/anthropic/v1/models")
}

pub fn is_chat_path(path: &str) -> bool {
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

pub fn route_upstream_path(path: &str) -> String {
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

pub fn strip_free_suffix(id: &str) -> String {
    const SUFFIX: &str = "-free";
    let lower = id.to_lowercase();
    if lower.len() > SUFFIX.len() && lower.ends_with(SUFFIX) {
        id[..id.len() - SUFFIX.len()].to_owned()
    } else {
        id.to_owned()
    }
}

