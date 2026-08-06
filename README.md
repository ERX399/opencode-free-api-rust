已将所有翻译内容整合在下面这个单独的文本框中（使用 ```text 代码块）：

# OpenCode 免费 API

一个面向 OpenAI 兼容上游服务的小型 Rust 反向代理。它暴露了健康检查、模型列表和对话补全端点，将模型过滤为 `*-免费`（`*-free`）类型，并按顺序在配置的多个节点之间进行故障转移。

## 下载

每个 [GitHub Release](../../releases) 都附带了各平台的免安装压缩包。压缩包内含可执行文件和一键启动脚本,无需安装任何依赖,解压即用。

启动后,将任何 OpenAI 兼容客户端指向 `http://localhost:8788/v1`,API 密钥留空即可。启动脚本是纯文本,如需设置 `API_TOKEN`、`AUTH_TOKEN` 或自定义 `NODES` 列表,直接编辑即可。

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

## 构建

```sh
cargo test
cargo build --release
```

release 配置已启用 `opt-level = "z"`、`lto`、`strip` 和 `panic = "abort"`。需要更小的二进制可用 UPX 压缩（约 -50%）。

### 交叉编译并打包全部平台

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
# Android targets use the NDK toolchain, not zigbuild
cargo build --release --target aarch64-linux-android
cargo build --release --target x86_64-linux-android
```

将产物整理进 `dist/` 后,运行 `scripts/make_packages.sh` 生成各平台的免安装压缩包（含一键启动脚本）。

Android 平台需要安装 [Android NDK](https://developer.android.com/ndk),并设置 `ANDROID_NDK_HOME`。在 `~/.cargo/config.toml` 中指定链接器：

```toml
[target.aarch64-linux-android]
linker = "<NDK>/toolchains/llvm/prebuilt/<host>/bin/aarch64-linux-android24-clang"
[target.x86_64-linux-android]
linker = "<NDK>/toolchains/llvm/prebuilt/<host>/bin/x86_64-linux-android24-clang"
```

然后在 Termux 或 root 环境中执行 `./start.sh` 即可运行。

同样的构建流程会在每次推送 `v*` 标签时通过 [`release` 工作流](../../.github/workflows/release.yml) 自动执行,并把压缩包上传到 GitHub Release。
