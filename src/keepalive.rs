// 保活与服务自检：网卡 IP / 公网 IP 探测、会话保活心跳。
// Android 端由 ProxyService 前台服务 + BootReceiver 保证进程存活（见 android/），
// 这里负责 Rust 侧自身可观测性与长连接会话的周期性维持。
use std::net::UdpSocket;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::protocol::{client, current_session_id, read_body};

// 探测本机出口网卡 IP（UDP connect 不发包，仅取路由源地址）。
pub fn primary_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip.to_string())
    }
}

pub fn public_ip() -> Option<String> {
    let agent = client(Duration::from_secs(5), Duration::from_secs(5));
    let text = agent.get("https://api.ipify.org").call().ok()?.into_body();
    let ip = String::from_utf8(read_body(text)).ok()?;
    let ip = ip.trim().to_owned();
    if ip.is_empty() {
        None
    } else {
        Some(ip)
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// 会话保活：周期性触碰 session（重置 TTL），避免长间隔请求间会话过期导致
// 上游重新分配会话。健康检查 /health 已覆盖进程存活探测。
pub fn spawn_session_keepalive() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(10 * 60));
        let _ = current_session_id();
    });
}