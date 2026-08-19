# 安全边界轻量修复实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在保持默认开放模式、代理核心路由和通配 CORS 兼容性的前提下，修复健康接口与首页网络信息泄露，并规范化 `AUTH_TOKEN` 配置行为，最终发布 `v0.1.7`

**Architecture:** 保留现有单文件 Rust 服务结构，只在 `App::from_env`、请求鉴权、健康响应和首页模板替换边界做最小改动。健康接口继续匿名可访问但只返回基础运行字段，首页改为浏览器依据当前访问地址生成 API 地址，认证 token 统一在读取和比较时规范化。实现、测试、版本和 CI 改动在同一个发布提交中完成，不创建中间提交

**Tech Stack:** Rust 2024、tiny_http 0.12、ureq 3、serde_json 1、静态 HTML/CSS/JavaScript、Cargo test、Clippy

## Global Constraints

- `AUTH_TOKEN` 未设置或清理后为空时继续保持默认开放行为
- 非空 `AUTH_TOKEN` 继续使用 `Authorization: Bearer <token>` 鉴权
- `AUTH_TOKEN` 读取值和客户端 Bearer token 都执行首尾空白清理
- `/health`、`/healthz`、`/ready`、`/api/health`、`/api/status` 继续允许匿名访问
- 健康响应只包含 `status`、`requests`、`started`
- 健康响应不得包含 `nodes`、`port`、`local_ip`、`public_ip`
- 首页不再由 Rust 注入 `__LOCAL_IP__`、`__PUBLIC_IP__`、`__PORT__`
- CORS 继续发送 `Access-Control-Allow-Origin: *`
- 不修改上游 API token、STRIP_FREE 映射、故障转移、流式转发或请求计数逻辑
- 不在代码中添加注释
- 发布版本必须是 `0.1.7`，Git tag 必须是 `v0.1.7`
- 所有实现、测试、版本、文档和 release workflow 改动合并为一个提交

---

### Task 1: 规范化入口鉴权配置

**Files:**
- Modify: `src/main.rs:60-90`、`src/main.rs:120-122`、`src/main.rs:276-281`
- Test: `src/main.rs` 的 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces `env_token(name: &str) -> Option<String>`，读取环境变量、执行 `trim()`、空值返回 `None`
- `App::from_env()` 使用 `env_token("AUTH_TOKEN")`
- `authorized()` 对收到的 Bearer token 执行相同的首尾空白清理后比较

- [ ] **Step 1: 写失败测试，覆盖 token 规范化规则**

在 `tests` 模块中增加纯函数测试，直接验证 `env_token` 的预期行为，测试必须使用不依赖进程环境的辅助函数，避免并行测试修改全局环境变量

```rust
#[test]
fn normalizes_optional_tokens() {
    assert_eq!(normalize_token(Some("  secret  ")), Some("secret".to_owned()));
    assert_eq!(normalize_token(Some("   ")), None);
    assert_eq!(normalize_token(Some("")), None);
    assert_eq!(normalize_token(None), None);
}
```

为便于测试，先定义计划中的纯函数接口 `normalize_token(value: Option<&str>) -> Option<String>`，再让 `env_token` 调用它

运行：`cargo test normalizes_optional_tokens -- --exact`

预期：FAIL，因为 `normalize_token` 尚未实现

- [ ] **Step 2: 实现最小 token 规范化逻辑**

在环境读取辅助函数附近加入无副作用的规范化函数，并让 `from_env` 使用它

```rust
fn normalize_token(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

fn env_token(name: &str) -> Option<String> {
    normalize_token(env::var(name).ok().as_deref())
}
```

将 `auth_token: env::var("AUTH_TOKEN").ok().filter(...)` 替换为 `auth_token: env_token("AUTH_TOKEN")`

在 `authorized()` 中将请求 token通过 `normalize_token(Some(token.as_str()))` 规范化后再比较，保持 `Bearer ` scheme 大小写敏感

- [ ] **Step 3: 增加启动状态日志**

保留现有启用日志，并增加 `else` 分支输出明确的开放模式提示，日志不得输出 token 内容

```rust
if app.auth_token.is_some() {
    eprintln!("已启用 AUTH_TOKEN 鉴权");
} else {
    eprintln!("未启用 AUTH_TOKEN 鉴权，服务处于开放模式");
}
```

- [ ] **Step 4: 运行定向测试和格式检查**

运行：`cargo test normalizes_optional_tokens -- --exact`

预期：PASS

运行：`cargo fmt --check`

预期：PASS

---

### Task 2: 收敛健康接口响应

**Files:**
- Modify: `src/main.rs:243-255`
- Test: `src/main.rs` 的 `#[cfg(test)] mod tests`

**Interfaces:**
- `handle()` 的健康路径继续在认证前返回，保证匿名首页轮询可用
- 健康 JSON 的字段集合固定为 `status`、`requests`、`started`

- [ ] **Step 1: 写健康响应字段测试**

抽出纯函数 `health_payload(app: &App) -> Value`，测试其对象不包含敏感字段

```rust
#[test]
fn health_payload_excludes_network_details() {
    let app = app_for_test();
    let payload = health_payload(&app);
    assert_eq!(payload["status"], "ok");
    assert!(payload.get("requests").is_some());
    assert!(payload.get("started").is_some());
    for field in ["nodes", "port", "local_ip", "public_ip"] {
        assert!(payload.get(field).is_none(), "unexpected field: {field}");
    }
}
```

运行：`cargo test health_payload_excludes_network_details -- --exact`

预期：FAIL，因为辅助函数尚未实现

- [ ] **Step 2: 实现最小健康 payload**

将健康分支中的内联 `json!` 提取为：

```rust
fn health_payload(&self) -> Value {
    json!({
        "status": "ok",
        "requests": self.requests.load(Ordering::Relaxed),
        "started": self.started,
    })
}
```

在 `handle()` 中使用 `json_response(200, self.health_payload())`

- [ ] **Step 3: 验证健康路径和 CORS 不变**

保留 `is_health_path` 的全部别名、查询字符串剥离、尾斜杠规范化和 `cors_response`/`json_response` 路径，不修改 CORS 头

运行：`cargo test routes_upstream_paths_correctly -- --exact`

预期：PASS

运行：`cargo test health_payload_excludes_network_details -- --exact`

预期：PASS

---

### Task 3: 移除首页服务端网络地址注入

**Files:**
- Modify: `static/index.html:160-165`、`static/index.html:206-211`
- Modify: `src/main.rs:363-370`
- Test: `src/main.rs` 的 `#[cfg(test)] mod tests`

**Interfaces:**
- `index_response()` 只替换 `__VERSION__`
- 首页地址由浏览器使用 `location.protocol`、`location.hostname` 和 `location.port` 生成
- 健康轮询继续读取 `started` 和 `requests`，不再依赖 `port`、`local_ip`、`public_ip`

- [ ] **Step 1: 先更新静态页面结构和脚本**

将快速开始区域的本机、局域网、公网三行改为基于当前访问地址的说明，保留 `url-loopback`、`url-lan`、`url-public` 元素供脚本更新，但不写入服务器 IP 占位符

将脚本加入纯前端地址构造函数：

```javascript
function currentBase() {
  const port = location.port ? `:${location.port}` : '';
  return `${location.protocol}//${location.hostname}${port}`;
}
```

在 `poll()` 中设置三个地址为 `currentBase() + '/v1'`，移除对 `j.port`、`j.local_ip`、`j.public_ip` 的读取和服务端 IP 回退文本

- [ ] **Step 2: 删除 Rust 首页敏感字段替换**

将 `index_response()` 的链式替换缩减为只替换版本号

```rust
let html = INDEX_HTML.replace("__VERSION__", VERSION);
```

不再替换 `__PORT__`、`__LOCAL_IP__`、`__PUBLIC_IP__`

- [ ] **Step 3: 增加首页模板安全测试**

增加测试确认首页响应模板不含服务端网络占位符，并且仍包含版本号

```rust
#[test]
fn index_template_has_no_server_network_placeholders() {
    assert!(!INDEX_HTML.contains("__PORT__"));
    assert!(!INDEX_HTML.contains("__LOCAL_IP__"));
    assert!(!INDEX_HTML.contains("__PUBLIC_IP__"));
    assert!(INDEX_HTML.contains("__VERSION__"));
}
```

- [ ] **Step 4: 检查前端行为和可访问性**

确认地址显示不依赖健康响应中的敏感字段，`location.hostname` 为空或 `location.port` 为空时仍生成合法 URL，按钮、状态轮询、模型统计和 GitHub 链接保持不变

运行：`cargo test index_template_has_no_server_network_placeholders -- --exact`

预期：PASS

---

### Task 4: 更新文档与版本到 v0.1.7

**Files:**
- Modify: `Cargo.toml:2`
- Modify: `Cargo.lock` 的 package version 条目
- Modify: `README.md`、`README.en.md` 中与健康接口和 AUTH_TOKEN 行为相关的说明
- Modify: `static/index.html` 页面文案中的版本占位使用方式（不写死版本）
- Modify: `docs/superpowers/specs/2026-08-19-security-boundary-design.md` 仅在实现偏离设计时同步

**Interfaces:**
- Cargo package version 为 `0.1.7`
- GitHub 发布 tag 为 `v0.1.7`
- README 明确说明 `AUTH_TOKEN` 为空时为开放模式、空白值会被清理、健康接口只返回基础运行信息

- [ ] **Step 1: 更新 Cargo 版本号**

将 `Cargo.toml` 的 `version = "0.1.6"` 改为 `version = "0.1.7"`

运行：`cargo check`

预期：成功，并更新 `Cargo.lock` 中根 package 的版本

- [ ] **Step 2: 更新中英文 README**

在配置说明中明确：

- `API_TOKEN` 为空时不发送上游 Authorization 头
- `AUTH_TOKEN` 未设置或清理后为空时保持开放模式
- `AUTH_TOKEN` 非空时启用 Bearer 鉴权
- 健康接口不再暴露节点列表和网络地址

示例值继续使用 `my-secret` 等占位符，不写入真实密钥

- [ ] **Step 3: 检查版本与敏感数据**

运行：`git diff --check`

运行：`rg -n -i "(sk-|ghp_|github_pat_|xox[bap]-|Bearer\\s+[A-Za-z0-9_-]{16,})" --glob '!target/**' --glob '!docs/**' .`

预期：无真实密钥命中，且版本相关文件统一为 `0.1.7`

---

### Task 5: 全量验证并准备单一发布提交

**Files:**
- Verify: `src/main.rs`
- Verify: `static/index.html`
- Verify: `Cargo.toml`、`Cargo.lock`、`README.md`、`README.en.md`
- Verify: `.github/workflows/release.yml`
- Include: `docs/superpowers/specs/2026-08-19-security-boundary-design.md`

**Interfaces:**
- 所有 Rust 测试、格式检查和 Clippy 验证通过
- 远程 `origin/main` 没有在本地检查后新增提交
- 本地提交图是 `origin/main` 的直接后继，不覆盖远程历史

- [ ] **Step 1: 运行格式、Clippy 和测试**

运行：`cargo fmt --check`

预期：PASS

运行：`cargo clippy --all-targets -- -D warnings`

预期：PASS，无 warning

运行：`cargo test`

预期：全部测试通过

- [ ] **Step 2: 检查最终差异和提交范围**

运行：`git diff --check`

运行：`git status --short`

运行：`git diff --stat origin/main`

确认只包含本次安全修复、轻量化发布、版本升级、文档和设计文件，不包含 `.env`、token、target、测试日志或临时压缩文件

- [ ] **Step 3: 重新确认远程提交图**

运行：`git fetch origin main`

运行：`git rev-parse origin/main`

运行：`git merge-base --is-ancestor origin/main HEAD`

预期：远程 main 是本地 HEAD 的祖先，且不会覆盖远程历史

- [ ] **Step 4: 创建唯一发布提交**

```bash
git add -A
git commit -m "release: v0.1.7" -m "Harden health and dashboard data exposure, improve AUTH_TOKEN handling, and compress release binaries with UPX LZMA"
```

提交前确认 staged diff 中没有敏感数据，作者/提交者按项目要求使用 ERX399

- [ ] **Step 5: 创建并推送 v0.1.7 tag**

```bash
git tag -a v0.1.7 -m "Release v0.1.7"
git push origin main
git push origin v0.1.7
```

仅在提交成功、远程历史比较通过且用户已确认发布后执行

- [ ] **Step 6: 用 gh 验证 release workflow**

```bash
gh run list --repo ERX399/opencode-free-api-rust --workflow release.yml --limit 5
gh run watch <RUN_ID> --repo ERX399/opencode-free-api-rust --exit-status
gh release view v0.1.7 --repo ERX399/opencode-free-api-rust
```

确认 workflow 成功、release 存在、Linux/Windows/macOS 压缩产物和 Android 产物符合预期，并再次核对远程 tag 指向提交 SHA

- [ ] **Step 7: 核对发布后的仓库状态**

运行：`git status --short`

运行：`git log --oneline --decorate -3`

运行：`git ls-remote origin refs/heads/main refs/tags/v0.1.7`

预期：工作区干净，远程 main 指向唯一发布提交，`v0.1.7` 指向同一发布提交，未发生强制推送或历史覆盖

## Self-review checklist

- 健康接口字段、首页渲染、token 规范化和文档更新均有对应任务
- 所有新增函数签名在任务接口中明确
- 测试步骤包含具体命令和预期结果
- 没有 `TODO`、`TBD` 或未定义实现步骤
- 版本升级、单提交、远程历史检查和 release 验证均有明确步骤
- CORS 通配策略和代理核心功能未被计划修改
