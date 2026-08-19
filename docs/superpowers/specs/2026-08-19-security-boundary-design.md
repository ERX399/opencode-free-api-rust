# 安全边界轻量修复设计

## 目标

在不改变代理默认开放模式、OpenAI 兼容接口、STRIP_FREE 映射、上游鉴权、流式转发和通配 CORS 兼容性的前提下，修复审计确认的信息泄露与 AUTH_TOKEN 配置陷阱

## 范围

本次只处理三个相关问题

1. 未鉴权健康接口暴露上游节点、局域网 IP、公网 IP 和端口
2. 首页服务端渲染并注入服务器网络地址
3. 空白 AUTH_TOKEN 被当作启用配置但永远无法通过认证

CORS 继续保持 `Access-Control-Allow-Origin: *`，不新增来源白名单，也不改变跨域客户端兼容性

## 方案

### 健康接口

`/health`、`/healthz`、`/ready`、`/api/health` 和 `/api/status` 继续允许匿名访问，确保首页轮询无需凭证

健康响应只保留仪表盘和基础监控需要的字段

- `status`
- `requests`
- `started`

移除以下字段

- `nodes`
- `port`
- `local_ip`
- `public_ip`

这样即使服务绑定公网地址，健康接口也不会泄露上游拓扑或主机网络信息

### 首页地址显示

首页不再由服务端替换 `__LOCAL_IP__`、`__PUBLIC_IP__` 和 `__PORT__`

前端使用当前浏览器访问的 `location` 信息生成可用地址

- 本机地址使用 `location.hostname` 和当前端口
- 局域网与公网地址不再声称由服务端自动探测
- 页面保留 API 路径展示和基础状态轮询

Rust 端保留 `local_ip` 和 `public_ip` 字段用于启动日志，避免扩大本次改动范围，但不再写入 HTML 或健康 JSON

### AUTH_TOKEN 配置

读取 `AUTH_TOKEN` 后先执行首尾空白清理

- 未设置或清理后为空：继续保持默认开放行为
- 非空值：启用 Bearer 鉴权
- 纯空白值：按未设置处理，并在启动日志中明确提示鉴权未启用

客户端 Bearer token 比较也采用相同的规范化规则，保留现有大小写敏感的 `Bearer ` scheme 行为

启动日志明确区分两种状态

- 已启用 `AUTH_TOKEN` 鉴权
- 未启用 `AUTH_TOKEN` 鉴权，服务处于开放模式

这样不改变默认行为，但降低部署者误以为服务已受保护的风险

## 数据流

1. `App::from_env` 读取并规范化 `AUTH_TOKEN`
2. `handle` 对健康路径返回最小 JSON，不执行上游请求
3. `index_response` 只替换版本号，网络地址由浏览器运行时决定
4. `poll` 继续读取健康 JSON 中的 `started` 和 `requests`
5. `loadModels`、聊天请求、路由和上游 API token 流程保持不变

## 错误处理

- 健康接口继续返回 HTTP 200 和最小 JSON
- 首页无法推导端口时使用当前页面端口，不回退到服务端注入的端口
- `AUTH_TOKEN` 空白配置不导致启动失败，也不导致所有请求无法认证
- CORS 头保持现有行为

## 测试计划

新增或调整单元测试覆盖

1. 未设置、空字符串和纯空白 `AUTH_TOKEN` 的规范化结果
2. 非空 token 启用鉴权
3. 健康 JSON 不包含 `nodes`、`local_ip`、`public_ip`、`port`
4. 首页源码不包含服务端网络地址占位符替换结果
5. 首页 JavaScript 使用当前访问地址生成 API 地址
6. 现有路由、模型映射、请求转发和流式测试继续通过

验证命令

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`

## 不在范围内

- 不修改 CORS 通配策略
- 不强制公网部署必须设置 AUTH_TOKEN
- 不新增鉴权详细健康接口
- 不修改上游节点故障转移、请求计数、流式转发或模型映射机制
- 不处理本轮审计中未完成验证的 DoS、注入和 panic 方向
