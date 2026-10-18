<div align="center">

![:name](https://count.getloli.com/@opencode-free-api?name=opencode-free-api&theme=minecraft&padding=6&offset=0&align=top&scale=1&pixelated=1&darkmode=auto)

# OpenCode Free API

_✨ OpenAI 兼容反代 · 单文件部署 ✨_

[![License](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Version](https://img.shields.io/badge/version-0.1.8-blue.svg)](../../releases)
[![GitHub](https://img.shields.io/badge/作者-ERX399-blue)](https://github.com/ERX399)

[简体中文](./README.md) | [English](./README.en.md)

</div>

面向 OpenAI 兼容上游的轻量 Rust 反向代理。只暴露 `*-free` 模型，多节点故障转移，单文件二进制零依赖部署。

## 下载

每个 [GitHub Release](../../releases) 提供各平台的免安装压缩包——单文件可执行，无依赖、无脚本，解压即用。

启动后控制台会打印三个访问地址（本机 / 局域网 / 公网）。将任意 OpenAI 兼容客户端指向其中之一即可，API Key 留空。如需设置 `API_TOKEN`、`AUTH_TOKEN` 或自定义 `NODES`，在运行前通过环境变量配置（见下表）。

## 配置

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `HOST` | `0.0.0.0` | 绑定地址 |
| `PORT` | `8788` | 绑定端口 |
| `NODES` | `https://opencode.ai/zen/v1` | 逗号分隔的上游基础 URL，默认官方 OpenCode Zen 网关 |
| `API_TOKEN` | 未设置 | 发往上游的可选 Bearer 令牌，未设置时不发送 Authorization 请求头（填你的 Zen Key） |
| `AUTH_TOKEN` | 未设置 | 设置后请求须携带 `Authorization: Bearer <value>`（`/health` 与预检除外） |
| `STRIP_FREE` | off | 去掉 `/v1/models` 返回 ID 的 `-free` 后缀，调用时映射回真实 ID |
| `UPSTREAM_TIMEOUT` | `90` | 非流式上游请求超时（秒） |
| `STREAM_TIMEOUT` | `1800` | 流式上游请求超时（秒） |
| `STREAM_LIVE` | on | `stream: true` 时逐块转发上游 SSE（真流式）；设为 off 回退为缓冲式聚合 |
| `STREAM_HEARTBEAT` | `15` | 流式空闲心跳间隔（秒），期间以 SSE 注释行保活 |
| `STREAM_IDLE_TIMEOUT` | `300` | 流式空闲超时（秒），上游静默超时后中断并收尾 |
| `CONNECT_TIMEOUT` | `10` | 上游 TCP/TLS 连接超时（秒） |
| `WORKERS` | CPU 核心数 | 并发工作线程数 |

```sh
HOST=0.0.0.0 PORT=8788 \
NODES=https://api-one.example,https://api-two.example \
AUTH_TOKEN=my-secret \
./opencode-free-api
```

## API

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/health` `/healthz` `/ready` `/api/health` `/api/status` | 健康检查与统计（请求数、监听地址、启动时间） |
| `GET` | `/v1/models` | 免费模型列表，仅暴露上游 `*-free` 模型 |
| `GET` | `/claude/v1/models` `/anthropic/v1/models` | Claude 兼容模型列表 |
| `POST` | `/v1/chat/completions` | 对话补全，支持流式 SSE |
| `POST` | `/v1/messages` `/claude/v1/messages` `/anthropic/v1/messages` | Anthropic Messages 协议，支持流式事件流 |
| `POST` | `/v1/responses` `/openai/v1/responses` | OpenAI Responses 协议，支持流式事件流 |
| `*` | 其他 | 透传上游，失败则切换下一节点 |

`/v1` 前缀的端点同时兼容 `/models`、`/chat/completions` 及 `/api/v1`、`/api/v3`、`/api/paas/v4`、`/v1beta` 等常见别名。`/v1/models` 拉取上游列表并只保留免费模型；上游请求使用 V2 三段式固定 `User-Agent: opencode/latest/2.0.32/cli`（跟随 npm 包 `@opencode/cli` 的版本，需要时手动同步），流式请求以 SSE 逐块转发并在失败时切换多节点。`/health` 报告请求数和监听地址信息。

## 流式转发

`POST /v1/chat/completions` 带 `stream: true` 时，上游 SSE 被逐块透传——上游每产出一个块就立即 flush 给客户端，不再等整段响应收完。`POST /v1/messages`（Anthropic 协议）同样逐块下发：上游 OpenAI 增量块实时翻译成 `message_start` / `content_block_start` / `content_block_delta` / `content_block_stop` / `message_delta` / `message_stop` 事件流，工具调用按 `tool_use` 块加 `input_json_delta` 片段转发。`POST /v1/responses`（Responses 协议）则是 `response.created` → 若干 `response.output_text.delta` → `response.completed`：

- 故障转移在写出首字节之前完成；首字节之后不再切换节点
- 上游静默达到 `STREAM_HEARTBEAT` 秒时下发保活（Anthropic 通道是 `event: ping`，OpenAI 与 Responses 通道是 SSE 注释行 `: keepalive`）；持续静默达到 `STREAM_IDLE_TIMEOUT` 秒则中断转发
- 中断时 OpenAI 通道补 `data: [DONE]` 收尾，Anthropic 与 Responses 通道补 `event: error` 且不再下发 `message_stop` / `response.completed`
- 中断发生在尚未下发任何内容时，会先补一个错误帧
- HTTP/1.0 客户端不支持 chunked 定界，自动回落为缓冲式响应
- 思考模式的兜底 `reasoning_content` 同样按块决策，语义与缓冲式路径一致
- Anthropic 通道与缓冲式路径保持一致：`stop_reason` 出现过工具调用即 `tool_use`、否则 `end_turn`；内容为空时补一个空 `text` 块；不下发 `thinking` 块
- Responses 通道与缓冲式路径保持一致：`id` 为 `resp_` 前缀、`output_text` 完整累积、`usage` 在 `response.completed` 里给全；`tool_calls` 与思考内容按现状忽略

## 思考模式（thinking）

请求里带 `thinking.enabled`、`reasoning_effort` 或 `reasoning.effort` 即视为思考模式：

- **请求侧**：自动在顶层与各 assistant 消息注入空的 `reasoning_content`，客户端不回传推理内容时也能通过上游（OpenCode Console）的校验
- **响应侧**：上游只返回 `reasoning_tokens` 却不带 `reasoning_content` 时，非流式响应与流式 SSE 都会补发空串，避免 ChatGPT-Next-Web 等客户端报"未找到推理内容"
- 上游返回了真实的 `reasoning_content` 时原样透传，不做任何改写

## 前端页面

访问根路径（如 `http://localhost:8788/`）返回内嵌状态页，展示服务状态、请求统计、接口说明与监听地址。启用 JS 后会轮询 `/health` 与 `/v1/models` 实时刷新，并检测 GitHub Release 显示更新横幅。

## Android

从 [Releases](../../releases) 下载 `OC-Free-API-arm64.apk` 安装即可，内嵌前台服务与常驻通知、开机自启。

**保活提示**：国产 ROM（vivo / OPPO / 小米 / 华为 等）会激进冻结后台进程，导致切后台后代理无响应。安装后**必须**在系统电池设置里手动允许「OC Free API」后台运行 / 高耗电 / 自启动，否则后台不可用。

## 构建

```sh
cargo test
cargo build --release
```

release 配置已启用 `opt-level = "z"`、`lto`、`strip`、`panic = "abort"`。可用 UPX 进一步压缩（约 -50%）。

### 交叉编译全部平台

需要 [rustup](https://rustup.rs)、[zig](https://ziglang.org) 和 [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)：

```sh
rustup target add \
  x86_64-pc-windows-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
  x86_64-apple-darwin aarch64-apple-darwin \
  aarch64-linux-android x86_64-linux-android
cargo install cargo-zigbuild --locked
for t in x86_64-pc-windows-gnu x86_64-unknown-linux-musl \
         aarch64-unknown-linux-musl x86_64-apple-darwin \
         aarch64-apple-darwin; do
  cargo zigbuild --release --target "$t"
done
# Android 使用 NDK 工具链，不走 zigbuild
cargo build --release --target aarch64-linux-android
cargo build --release --target x86_64-linux-android
```

将产物整理进 `dist/`，运行 `scripts/make_packages.sh` 生成各平台免安装压缩包。

Android 需安装 [Android NDK](https://developer.android.com/ndk) 并设置 `ANDROID_NDK_HOME`，在 `~/.cargo/config.toml` 指定链接器：

```toml
[target.aarch64-linux-android]
linker = "<NDK>/toolchains/llvm/prebuilt/<host>/bin/aarch64-linux-android24-clang"
[target.x86_64-linux-android]
linker = "<NDK>/toolchains/llvm/prebuilt/<host>/bin/x86_64-linux-android24-clang"
```

同样的流程在每次推送 `v*` 标签时由 [`release` 工作流](../../.github/workflows/release.yml) 自动执行，并将压缩包上传到 GitHub Release。
