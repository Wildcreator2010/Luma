//! 浏览器登录桥。
//!
//! Bambu 的短信接口要求 Geetest 人机验证，原生 HTTP 客户端无法通过；
//! 密码接口对部分账号也只返回笼统的“账号或密码错误”。因此提供一条
//! “在官方页面登录”的路径：
//!
//! 1. 用**独立 profile** 启动本机 Edge 或 Chrome，打开官方登录页；
//! 2. 打开一个只监听 `127.0.0.1` 的 CDP 调试端口；
//! 3. 通过 CDP 读取 Cookie、Web Storage，并旁路抓取 `Authorization`
//!    请求头（Geetest、短信、扫码都在这条路径里完成）；
//! 4. 用候选值调用 `prepare_session` 校验，成功即写入凭据管理器并关闭浏览器。
//!
//! 约束：
//! - 绝不把 token 写入日志或磁盘（除 Credential Manager）。
//! - 使用独立 profile（`%LOCALAPPDATA%\Luma\browser-login`），
//!   不读取用户日常浏览器的 Cookie。
//! - 浏览器进程由本模块负责关闭，避免调试端口长期暴露。

use std::collections::HashSet;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use luma_cloud::{store_token, AuthToken, BambuAuth, BambuCloudConfig};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// 主循环节拍：足够快地响应登录完成，又不会明显占用 CPU。
const POLL_INTERVAL: Duration = Duration::from_millis(600);
/// 读取 Cookie / Storage 的间隔。CDP 调用有开销，不需要每轮都做。
const DUMP_INTERVAL: Duration = Duration::from_millis(2000);
/// 单次 CDP 调用的等待上限。
const CDP_TIMEOUT: Duration = Duration::from_secs(5);
/// 连续多少次探测不到调试端口就认为浏览器已被关闭。
const MAX_PROBE_FAILURES: u32 = 4;
/// 启动后的宽限期：冷启动 Edge 可能超过 10 秒才监听端口，
/// 这段时间内的探测失败不计入“已关闭”。
const LAUNCH_GRACE: Duration = Duration::from_secs(15);

pub struct BrowserLogin {
    child: Child,
    port: u16,
    profile: PathBuf,
    launched_at: Instant,
}

pub struct BrowserLoginSuccess {
    pub devices: usize,
    pub uid: String,
}

impl BrowserLogin {
    /// 启动一个带独立 profile 与本地调试端口的浏览器窗口。
    pub fn launch() -> Result<Self> {
        let browser = find_browser()
            .ok_or_else(|| anyhow!("未找到 Microsoft Edge 或 Google Chrome，请先安装其中之一"))?;
        let port = free_port()?;
        let profile = browser_profile_dir();
        std::fs::create_dir_all(&profile)
            .with_context(|| format!("创建浏览器配置目录失败：{}", profile.display()))?;

        let child = Command::new(&browser)
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--remote-debugging-address=127.0.0.1")
            // Chrome 111+ 需要显式放行 WebSocket 客户端来源。
            .arg("--remote-allow-origins=*")
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-sync",
                "--disable-background-networking",
                "--window-size=1100,900",
                "--lang=zh-CN",
                "--new-window",
                "https://bambulab.cn/sign-in",
            ])
            .spawn()
            .with_context(|| format!("启动浏览器失败：{}", browser.display()))?;

        tracing::info!(port, "browser login window launched");
        Ok(Self {
            child,
            port,
            profile,
            launched_at: Instant::now(),
        })
    }

    /// 轮询直到拿到可用 token、超时或浏览器被关闭。
    ///
    /// 返回前会关闭浏览器，避免调试端口继续暴露。
    pub async fn wait_for_token(&mut self, timeout: Duration) -> Result<BrowserLoginSuccess> {
        let result = self.poll_token(timeout).await;
        self.close();
        result
    }

    async fn poll_token(&mut self, timeout: Duration) -> Result<BrowserLoginSuccess> {
        let deadline = Instant::now() + timeout;
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(6))
            .build()?;
        let auth = BambuAuth::new(BambuCloudConfig::china_mainland())?;

        let mut page: Option<Cdp> = None;
        let mut browser: Option<Cdp> = None;
        let mut tried: HashSet<String> = HashSet::new();
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut last_dump = Instant::now() - DUMP_INTERVAL;
        let mut probe_failures = 0;

        loop {
            if Instant::now() >= deadline {
                bail!("浏览器登录超时，请重试");
            }

            // 浏览器级连接：可读到含 HttpOnly 的全部 Cookie。
            if browser.is_none() {
                match fetch_json(&http, self.port, "/json/version").await {
                    Ok(version) => {
                        probe_failures = 0;
                        if let Some(url) =
                            version.get("webSocketDebuggerUrl").and_then(Value::as_str)
                        {
                            browser = Cdp::connect(url).await.ok();
                        }
                    }
                    Err(_) => {
                        // 宽限期内浏览器可能还没起来；只有确认进程已退出，
                        // 或宽限期后仍连续探测失败，才判定用户关闭了窗口。
                        let exited = matches!(self.child.try_wait(), Ok(Some(_)));
                        if self.launched_at.elapsed() > LAUNCH_GRACE || exited {
                            probe_failures += 1;
                            if probe_failures >= MAX_PROBE_FAILURES {
                                bail!("浏览器窗口已关闭，登录未完成");
                            }
                        }
                    }
                }
            }

            // 页面级连接：读 Web Storage，并旁路抓请求头。
            if page.is_none() {
                if let Ok(targets) = fetch_json(&http, self.port, "/json").await {
                    if let Some(target) = find_login_target(&targets) {
                        if let Some(url) =
                            target.get("webSocketDebuggerUrl").and_then(Value::as_str)
                        {
                            if let Ok(mut cdp) = Cdp::connect(url).await {
                                let _ = cdp.call("Network.enable", json!({})).await;
                                let _ = cdp.call("Runtime.enable", json!({})).await;
                                page = Some(cdp);
                            }
                        }
                    }
                }
            }

            // 事件流里可能直接带着 Authorization 头，可信度最高。
            if let Some(cdp) = page.as_mut() {
                cdp.pump_events().await;
                for event in cdp.take_events() {
                    collect_event_candidates(&event, &mut candidates);
                }
            }

            if last_dump.elapsed() >= DUMP_INTERVAL {
                last_dump = Instant::now();

                let mut drop_browser = false;
                if let Some(cdp) = browser.as_mut() {
                    match cdp.call("Storage.getCookies", json!({})).await {
                        Ok(Some(response)) => collect_cookie_candidates(&response, &mut candidates),
                        Ok(None) | Err(_) => drop_browser = true,
                    }
                }
                if drop_browser {
                    browser = None;
                }

                let mut drop_page = false;
                if let Some(cdp) = page.as_mut() {
                    let expression = "JSON.stringify({ \
                         local: Object.fromEntries(Object.entries(localStorage)), \
                         session: Object.fromEntries(Object.entries(sessionStorage)), \
                         cookie: document.cookie, \
                         href: location.href \
                     })";
                    match cdp
                        .call(
                            "Runtime.evaluate",
                            json!({
                                "expression": expression,
                                "returnByValue": true,
                                "awaitPromise": true,
                            }),
                        )
                        .await
                    {
                        Ok(Some(response)) => {
                            collect_storage_candidates(&response, &mut candidates)
                        }
                        Ok(None) | Err(_) => drop_page = true,
                    }
                }
                if drop_page {
                    page = None;
                }
            }

            if let Some(success) = validate(&auth, &mut candidates, &mut tried).await {
                return Ok(success);
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// 主动关闭本次启动的浏览器，并清理独立 profile 里的登录态。
    pub fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // 不在磁盘上保留 Cookie，下一次登录必须重新走官方页面。
        let network = self.profile.join("Default").join("Network");
        for path in [network.join("Cookies"), network.join("Cookies-journal")] {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for BrowserLogin {
    fn drop(&mut self) {
        self.close();
    }
}

/// 候选 token 值。`priority` 越小越可信。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    value: String,
    priority: u8,
    source: &'static str,
}

impl Candidate {
    fn new(value: &str, priority: u8, source: &'static str) -> Option<Self> {
        let value = strip_bearer(value.trim().trim_matches('"'));
        if value.len() < 20 || value.len() > 8192 {
            return None;
        }
        Some(Self {
            value: value.to_string(),
            priority,
            source,
        })
    }
}

fn strip_bearer(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.len() > 7 && trimmed.as_bytes()[..7].eq_ignore_ascii_case(b"bearer ") {
        trimmed[7..].trim()
    } else {
        trimmed
    }
}

/// 按去重规则并入候选队列。
fn push_candidate(out: &mut Vec<Candidate>, candidate: Candidate) {
    if out.iter().any(|existing| existing.value == candidate.value) {
        return;
    }
    out.push(candidate);
}

/// 逐个校验候选 token；成功则落盘并返回。
async fn validate(
    auth: &BambuAuth,
    candidates: &mut Vec<Candidate>,
    tried: &mut HashSet<String>,
) -> Option<BrowserLoginSuccess> {
    candidates.sort_by_key(|candidate| candidate.priority);
    candidates.truncate(64);

    let pending: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| !tried.contains(&candidate.value))
        .cloned()
        .collect();

    for candidate in pending {
        tried.insert(candidate.value.clone());
        let token = AuthToken::new(candidate.value, None, None);
        match auth.prepare_session(token).await {
            Ok((token, devices, uid)) => {
                tracing::info!(
                    source = candidate.source,
                    devices = devices.len(),
                    "browser login captured a valid token"
                );
                if let Err(error) = store_token(&token) {
                    tracing::error!(%error, "failed to persist token from browser login");
                    return None;
                }
                return Some(BrowserLoginSuccess {
                    devices: devices.len(),
                    uid,
                });
            }
            Err(error) => {
                tracing::debug!(
                    source = candidate.source,
                    error = %error,
                    "browser login candidate rejected"
                );
                continue;
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// CDP 最小客户端
// ---------------------------------------------------------------------------

struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    events: Vec<Value>,
}

impl Cdp {
    async fn connect(ws_url: &str) -> Result<Self> {
        let (socket, _) = tokio::time::timeout(CDP_TIMEOUT, connect_async(ws_url))
            .await
            .map_err(|_| anyhow!("连接浏览器调试端口超时"))??;
        Ok(Self {
            socket,
            next_id: 1,
            events: Vec::new(),
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Option<Value>> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({ "id": id, "method": method, "params": params });
        self.socket
            .send(Message::Text(payload.to_string().into()))
            .await?;

        let deadline = Instant::now() + CDP_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let Some(message) = tokio::time::timeout(remaining, self.socket.next())
                .await
                .ok()
                .flatten()
            else {
                return Ok(None);
            };
            let Ok(message) = message else {
                return Ok(None);
            };
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            match value.get("id").and_then(Value::as_u64) {
                Some(response_id) if response_id == id => return Ok(Some(value)),
                Some(_) => continue,
                // 没有 id 的是事件，缓存起来交给调用方解析。
                None => self.events.push(value),
            }
        }
    }

    /// 非阻塞地把挂起的事件读进缓存。
    async fn pump_events(&mut self) {
        loop {
            match tokio::time::timeout(Duration::from_millis(10), self.socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(value) = serde_json::from_str::<Value>(text.as_str()) {
                        if value.get("id").is_none() {
                            self.events.push(value);
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn take_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.events)
    }
}

// ---------------------------------------------------------------------------
// 候选提取（纯函数，便于单测）
// ---------------------------------------------------------------------------

fn collect_cookie_candidates(response: &Value, out: &mut Vec<Candidate>) {
    let Some(cookies) = response
        .get("result")
        .and_then(|result| result.get("cookies"))
        .and_then(Value::as_array)
    else {
        return;
    };

    for cookie in cookies {
        let name = cookie.get("name").and_then(Value::as_str).unwrap_or("");
        let value = cookie.get("value").and_then(Value::as_str).unwrap_or("");
        let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or("");
        if !domain.contains("bambulab") {
            continue;
        }
        if !is_access_token_cookie_name(name) {
            continue;
        }
        let priority = if name.eq_ignore_ascii_case("token") {
            0
        } else {
            1
        };
        if let Some(candidate) = Candidate::new(value, priority, "cookie") {
            push_candidate(out, candidate);
        }
    }
}

fn collect_storage_candidates(response: &Value, out: &mut Vec<Candidate>) {
    let Some(raw) = response
        .get("result")
        .and_then(|result| result.get("result"))
        .and_then(|result| result.get("value"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return;
    };

    scan_json(&value, "", out, 0);
}

fn scan_json(value: &Value, key: &str, out: &mut Vec<Candidate>, depth: u8) {
    if depth > 3 {
        return;
    }
    match value {
        Value::String(text) => {
            if key == "cookie" {
                for (name, cookie_value) in parse_cookie_string(text) {
                    let priority = name_priority(&name, &cookie_value);
                    if let Some(candidate) =
                        Candidate::new(&cookie_value, priority, "document.cookie")
                    {
                        push_candidate(out, candidate);
                    }
                }
                return;
            }
            // 有些站点把 JSON 再塞进字符串里。
            if text.starts_with('{') {
                if let Ok(nested) = serde_json::from_str::<Value>(text) {
                    scan_json(&nested, key, out, depth + 1);
                    return;
                }
            }
            let priority = name_priority(key, text);
            if let Some(candidate) = Candidate::new(text, priority, "storage") {
                push_candidate(out, candidate);
            }
        }
        Value::Object(map) => {
            for (child_key, child) in map {
                scan_json(child, child_key, out, depth + 1);
            }
        }
        Value::Array(items) => {
            for item in items {
                scan_json(item, key, out, depth + 1);
            }
        }
        _ => {}
    }
}

fn parse_cookie_string(raw: &str) -> Vec<(String, String)> {
    raw.split(';')
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

/// 名称和值形态共同决定可信度：0 最高。
fn name_priority(name: &str, value: &str) -> u8 {
    let name = name.to_ascii_lowercase();
    if name.contains("access") {
        0
    } else if name.contains("token") || name == "authorization" {
        1
    } else if value.starts_with("eyJ") {
        2
    } else if value.len() > 80 {
        3
    } else {
        4
    }
}

fn collect_event_candidates(event: &Value, out: &mut Vec<Candidate>) {
    let method = event.get("method").and_then(Value::as_str).unwrap_or("");
    let params = event.get("params").unwrap_or(&Value::Null);

    match method {
        "Network.requestWillBeSent" => {
            if let Some(headers) = params
                .get("request")
                .and_then(|request| request.get("headers"))
                .and_then(Value::as_object)
            {
                collect_authorization(headers, out);
            }
        }
        "Network.requestWillBeSentExtraInfo" => {
            if let Some(headers) = params.get("headers").and_then(Value::as_object) {
                collect_authorization(headers, out);
            }
        }
        // 登录成功的瞬间，服务端会下发 `Set-Cookie: token=...`。
        // 这是最早、最可信的时机（Cookie 可能是 HttpOnly）。
        "Network.responseReceivedExtraInfo" => {
            if let Some(headers) = params.get("headers").and_then(Value::as_object) {
                collect_set_cookie(headers, out);
            }
        }
        _ => {}
    }
}

fn collect_authorization(headers: &serde_json::Map<String, Value>, out: &mut Vec<Candidate>) {
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        let Some(value) = value.as_str() else {
            continue;
        };
        if let Some(candidate) = Candidate::new(value, 0, "request.header") {
            push_candidate(out, candidate);
        }
    }
}

fn collect_set_cookie(headers: &serde_json::Map<String, Value>, out: &mut Vec<Candidate>) {
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        let lines: Vec<String> = match value {
            Value::String(text) => vec![text.clone()],
            Value::Array(items) => items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect(),
            _ => Vec::new(),
        };

        for line in lines {
            let pair = line.split(';').next().unwrap_or("");
            let Some((cookie_name, cookie_value)) = pair.split_once('=') else {
                continue;
            };
            let cookie_name = cookie_name.trim();
            if !is_access_token_cookie_name(cookie_name) {
                continue;
            }
            let priority = if cookie_name.eq_ignore_ascii_case("token") {
                0
            } else {
                1
            };
            // 只接受名字明确像认证凭据的 Cookie，避免把会话 ID 当 token。
            if let Some(candidate) = Candidate::new(cookie_value, priority, "set-cookie") {
                push_candidate(out, candidate);
            }
        }
    }
}

fn is_access_token_cookie_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "token"
        || name == "authorization"
        || (name.contains("access") && !name.contains("csrf"))
}

// ---------------------------------------------------------------------------
// 浏览器发现与 HTTP 辅助
// ---------------------------------------------------------------------------

fn browser_profile_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Luma").join("browser-login")
}

fn find_browser() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        let Some(root) = std::env::var_os(key) else {
            continue;
        };
        let root = PathBuf::from(root);
        candidates.push(root.join("Microsoft/Edge/Application/msedge.exe"));
        candidates.push(root.join("Google/Chrome/Application/chrome.exe"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

async fn fetch_json(client: &reqwest::Client, port: u16, path: &str) -> Result<Value> {
    let url = format!("http://127.0.0.1:{port}{path}");
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

fn find_login_target(targets: &Value) -> Option<&Value> {
    targets.as_array()?.iter().find(|target| {
        target.get("type").and_then(Value::as_str) == Some("page")
            && target
                .get("url")
                .and_then(Value::as_str)
                .map(|url| url.contains("bambulab"))
                .unwrap_or(false)
    })
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_candidates_are_prioritised_by_name() {
        let response = json!({
            "result": {
                "cookies": [
                    { "name": "theme", "value": "dark", "domain": ".bambulab.cn" },
                    { "name": "token", "value": "eyJhbGciOiJIUzI1NiJ9.payload.signature", "domain": ".bambulab.cn" },
                    { "name": "access_token", "value": "abcdefghijklmnopqrstuvwxyz", "domain": ".bambulab.cn" },
                    { "name": "access_token", "value": "should-be-ignored-value", "domain": ".example.com" }
                ]
            }
        });
        let mut out = Vec::new();
        collect_cookie_candidates(&response, &mut out);
        assert_eq!(out.len(), 2, "只应保留 bambulab 域下足够长的值");
        assert_eq!(out.iter().map(|c| c.priority).min(), Some(0));
        assert!(out.iter().any(|c| c.value.ends_with("signature")));
    }

    #[test]
    fn storage_scan_walks_nested_json_and_cookie_string() {
        let inner = json!({ "accessToken": "token-value-long-enough-1234567890" }).to_string();
        let raw = json!({
            "local": { "user": inner },
            "cookie": "a=1; access_token=cookie-token-value-long-enough"
        })
        .to_string();
        let response = json!({ "result": { "result": { "value": raw } } });

        let mut out = Vec::new();
        collect_storage_candidates(&response, &mut out);
        assert!(out.iter().any(|c| c.value.contains("token-value")));
        assert!(out.iter().any(|c| c.value.contains("cookie-token")));
    }

    #[test]
    fn authorization_header_is_the_top_priority_source() {
        let event = json!({
            "method": "Network.requestWillBeSentExtraInfo",
            "params": { "headers": { "Authorization": "Bearer header-token-value-1234567890" } }
        });
        let mut out = Vec::new();
        collect_event_candidates(&event, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].priority, 0);
        assert_eq!(out[0].source, "request.header");
        assert_eq!(out[0].value, "header-token-value-1234567890");
    }

    #[test]
    fn csrf_and_refresh_cookies_are_not_access_tokens() {
        let response = json!({
            "result": {
                "cookies": [
                    { "name": "bbl_csrf_token", "value": "csrf-token-value-long-enough", "domain": ".bambulab.cn" },
                    { "name": "refreshToken", "value": "refresh-token-value-long-enough", "domain": ".bambulab.cn" },
                    { "name": "token", "value": "access-token-value-long-enough", "domain": ".bambulab.cn" }
                ]
            }
        });
        let mut out = Vec::new();
        collect_cookie_candidates(&response, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].priority, 0);
        assert_eq!(out[0].value, "access-token-value-long-enough");
    }

    #[test]
    fn short_and_noise_values_are_rejected() {
        assert!(Candidate::new("short", 0, "cookie").is_none());
        assert!(Candidate::new(&"x".repeat(10_000), 0, "cookie").is_none());
        assert_eq!(
            Candidate::new("  Bearer abcdefghijklmnopqrstuvwxyz  ", 0, "cookie")
                .unwrap()
                .value,
            "abcdefghijklmnopqrstuvwxyz"
        );
    }

    #[test]
    fn set_cookie_token_is_captured_and_session_ids_ignored() {
        let event = json!({
            "method": "Network.responseReceivedExtraInfo",
            "params": {
                "headers": {
                    "Set-Cookie": "token=abc123def456ghi789jkl; Path=/; HttpOnly; Secure"
                }
            }
        });
        let mut out = Vec::new();
        collect_event_candidates(&event, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "set-cookie");
        assert_eq!(out[0].value, "abc123def456ghi789jkl");

        let unrelated = json!({
            "method": "Network.responseReceivedExtraInfo",
            "params": {
                "headers": { "Set-Cookie": "sid=zzzzzzzzzzzzzzzzzzzzzzzzz; Path=/" }
            }
        });
        let mut out = Vec::new();
        collect_event_candidates(&unrelated, &mut out);
        assert!(out.is_empty(), "非认证 Cookie 不应作为候选");
    }
}
