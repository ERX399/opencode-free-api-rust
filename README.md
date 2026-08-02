已将所有翻译内容整合在下面这个单独的文本框中（使用 ```text 代码块）：

# OpenCode 免费 API

一个面向 OpenAI 兼容上游服务的小型 Rust 反向代理。它暴露了健康检查、模型列表和对话补全端点，将模型过滤为 `*-免费`（`*-free`）类型，并按顺序在配置的多个节点之间进行故障转移。

## 下载

每个 [GitHub Release](../../releases) 都附带了各平台的预编译单文件二进制文件。无需任何依赖——将对应文件复制到您的机器上并运行即可。

| 平台 | 文件 | 备注 |
| --- | --- | --- |
| Linux x86_64 | `opencode-free-api-linux-x86_64` | 静态链接（musl） |
| Linux arm64 | `opencode-free-api-linux-aarch64` | 静态链接（musl），例如 Raspberry Pi |
| Windows x86_64 | `opencode-free-api-windows-x86_64.exe` | 仅使用系统 DLL |
| macOS x86_64 | `opencode-free-api-macos-x86_64` | Intel 芯片 |
| macOS arm64 | `opencode-free-api-macos-arm64` | Apple Silicon 芯片 |

在 Linux/macOS 上，请先赋予可执行权限：`chmod +x opencode-free-api-*`。

## 运行时配置

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `HOST` | `0.0.0.0` | 绑定地址 |
| `PORT` | `8788` | 绑定端口 |
| `NODES` | `https://opencode.ai/zen/v1` | 逗号分隔的上游基础 URL；默认使用官方 OpenCode Zen 网关 |
| `API_TOKEN` | 未设置 | 发送给每个上游的可选 Bearer 令牌（请填入您的 Zen 密钥） |
| `AUTH_TOKEN` | 未设置 | 若设置，则请求必须携带 `Authorization: Bearer <value>`（`/health` 和预检请求除外） |
| `STRIP_FREE` | off | 从 `/v1/models` 返回的模型 ID 中移除 `-free` 后缀，并将调用映射回真实 ID |
| `UPSTREAM_TIMEOUT` | `90` | 非流式上游请求的超时时间（秒） |
| `STREAM_TIMEOUT` | `1800` | 流式上游请求的超时时间（秒） |
| `CONNECT_TIMEOUT` | `10` | 上游 TCP/TLS 连接超时时间（秒） |
| `WORKERS` | CPU 核心数 | 并发请求工作线程数量 |

```sh
HOST=0.0.0.0 PORT=8788 \
NODES=https://api-one.example,https://api-two.example \
AUTH_TOKEN=my-secret \
./opencode-free-api
```

## API

- `GET /health`
- `GET /v1/models`
- `POST /v1/chat/completions`

本项目不包含任何内嵌的节点地址、凭证或已恢复的二进制内容。项目采用 MIT 许可证发布。

`/v1/models` 会拉取实时的上游模型列表（默认使用官方 OpenCode Zen 目录，例如 `https://opencode.ai/zen/v1/models`），并在后处理中仅保留免费模型。携带 `"stream": true` 的请求会以 Server-Sent Events 形式逐块转发；错误会记录到 stderr，代理会故障转移到下一个节点。`/health` 会报告请求计数和上游失败计数。
