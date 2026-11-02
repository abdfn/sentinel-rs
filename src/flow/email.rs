//! 通用邮箱取码 API 客户端（纯 HTTP 契约，不绑定任何具体服务 / 私有凭据）。
//!
//! ### 环境变量
//! * `REG_MAIL_ENTRY`（可选）—— **固定邮箱**：`email----code_url`（4 个连字符分隔）。
//!   设置后跳过 `POST /mailboxes`，直接用这个邮箱注册、直接从这个地址取码。
//!   与 `REG_MAIL_API_BASE_URL` 同时设置时**以本项为准**。
//! * `REG_MAIL_API_BASE_URL`（领邮箱模式必填）—— 邮箱服务基地址，必须 `https://`。
//!   本地自建 / mock 服务可用 `http://127.0.0.1:PORT/...`，但必须同时显式设置
//!   `REG_MAIL_ALLOW_INSECURE=1`（回环地址才放行，别的 `http://` 一律拒绝）。
//! * `REG_MAIL_API_TOKEN`（可选）—— 非空时作为 `Authorization: Bearer <token>` 发送。
//!
//! ### 契约
//! * `POST {base}/mailboxes` → `201 {"email": "...", "code_url": "https://..."}`
//! * `GET  {code_url}`       → 取码页。两种形态都认：
//!   - HTML（最新在前，`<article>` 卡片里 `<div class="meta">时间：…</div>` +
//!     `<div class="code">123456</div>`）
//!   - JSON（任意嵌套，邮件对象里带 6 位验证码 + 可选时间字段，字段名见
//!     [`super::otp::parse_cards_json`]）
//! * `GET  {base}/health`    → 可选自检 `{"ok": true}`，只在 dry-run 请求计划里出现
//!
//! 取码逻辑复用 [`super::otp`]，本文件只做 "取正文"。

use super::http::{Client, RequestSpec};
use super::otp;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

pub const ENV_BASE_URL: &str = "REG_MAIL_API_BASE_URL";
pub const ENV_TOKEN: &str = "REG_MAIL_API_TOKEN";
pub const ENV_ALLOW_INSECURE: &str = "REG_MAIL_ALLOW_INSECURE";
/// `email----code_url`：固定邮箱，跳过领邮箱 API。
pub const ENV_ENTRY: &str = "REG_MAIL_ENTRY";
/// 固定邮箱列表文件（每行一条 `email----code_url`，`#` 开头忽略）。
pub const ENV_ENTRY_FILE: &str = "REG_MAIL_ENTRY_FILE";

/// 邮箱 API 基地址 + 可选 bearer token。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailApiConfig {
    pub base_url: String,
    /// 空字符串表示服务端不需要鉴权（不发送 `Authorization` 头）。
    pub token: String,
    /// `REG_MAIL_ALLOW_INSECURE=1`：允许回环地址走 `http://`（本地 mock 用）。
    pub allow_insecure: bool,
}

impl EmailApiConfig {
    /// 从可注入的读取器解析；`REG_MAIL_API_BASE_URL` 缺失时返回 `Ok(None)`。
    pub fn from_lookup<F>(lookup: &F) -> Result<Option<Self>>
    where
        F: Fn(&str) -> Option<String> + ?Sized,
    {
        let get = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let allow_insecure = is_truthy(get(ENV_ALLOW_INSECURE).as_deref());
        match (get(ENV_BASE_URL), get(ENV_TOKEN)) {
            (Some(base), token) => Ok(Some(Self::validated(
                base,
                token.unwrap_or_default(),
                allow_insecure,
            )?)),
            (None, Some(_)) => bail!(
                "已设置 {ENV_TOKEN} 但缺少 {ENV_BASE_URL}（邮箱 API 基地址是必填项）"
            ),
            (None, None) => Ok(None),
        }
    }

    /// 校验 + 规范化（去掉末尾 `/`）。
    fn validated(base_url: String, token: String, allow_insecure: bool) -> Result<Self> {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        let token = token.trim().to_string();
        if base_url.is_empty() {
            bail!("邮箱 API base_url 为空（{ENV_BASE_URL}）");
        }
        if base_url.starts_with("https://") {
            // ok
        } else if base_url.starts_with("http://") {
            if !allow_insecure {
                bail!(
                    "邮箱 API base_url 必须是 https://（当前 {base_url}）；\
                     本地自建 / mock 服务请显式设置 {ENV_ALLOW_INSECURE}=1"
                );
            }
            if !is_loopback_url(&base_url) {
                bail!(
                    "{ENV_ALLOW_INSECURE}=1 只放行回环地址（127.0.0.1 / localhost / ::1），\
                     当前 {base_url} 不是回环地址"
                );
            }
        } else {
            bail!("邮箱 API base_url 必须带 scheme（https:// 或放行后的 http://）: {base_url}");
        }
        Ok(Self {
            base_url,
            token,
            allow_insecure,
        })
    }

    /// 拼接端点（保证单个 `/`）。
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'))
    }

    /// 鉴权头：token 为空时不发（服务端可选鉴权）。
    fn auth_header(&self) -> Option<(String, String)> {
        if self.token.is_empty() {
            None
        } else {
            Some((
                "authorization".to_string(),
                format!("Bearer {}", self.token),
            ))
        }
    }
}

fn is_truthy(raw: Option<&str>) -> bool {
    matches!(
        raw.unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// URL 的主机部分是否是回环地址（不解析 DNS，只看字面量）。
pub fn is_loopback_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or(authority); // 丢掉 userinfo
    let host = match hostport.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => hostport.split(':').next().unwrap_or(""),
    };
    let host = host.to_ascii_lowercase();
    host == "localhost" || host == "::1" || host == "127.0.0.1" || host.starts_with("127.")
}

/// 固定邮箱：调用方直接给一个已存在的收件地址和它的取码地址，跳过领邮箱 API。
///
/// 环境变量 `REG_MAIL_ENTRY`，格式 `email----code_url`（分隔符是 **4 个连字符**，
/// 邮箱里的 `+` / `-` 不受影响；取码地址里的查询串原样保留）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedMailbox {
    pub email: String,
    pub code_url: String,
}

impl FixedMailbox {
    /// 解析 `email----code_url` 并校验两端。
    pub fn parse(raw: &str, allow_insecure: bool) -> Result<Self> {
        // Windows 下用 PowerShell 写文件常带 UTF-8 BOM，先剥掉再解析。
        let raw = raw.trim().trim_start_matches('\u{feff}');
        let (email, code_url) = raw.split_once("----").with_context(|| {
            format!("邮箱条目格式应为 email----code_url（4 个连字符分隔），当前 {raw:?}")
        })?;
        let email = email.trim().to_string();
        let code_url = code_url.trim().to_string();
        if !email.contains('@') {
            bail!("邮箱条目里的邮箱非法: {email:?}");
        }
        check_fetchable_url("code_url", &code_url, allow_insecure)?;
        Ok(Self { email, code_url })
    }
}

/// 读取固定邮箱列表：`REG_MAIL_ENTRY`（单条，或多行 / 分号分隔）+
/// `REG_MAIL_ENTRY_FILE`（文件每行一条）。两个都设就按顺序拼起来。
pub fn entries_from_lookup<F>(lookup: &F) -> Result<Vec<FixedMailbox>>
where
    F: Fn(&str) -> Option<String> + ?Sized,
{
    let allow_insecure = is_truthy(lookup(ENV_ALLOW_INSECURE).as_deref());
    let mut out = Vec::new();
    if let Some(raw) = lookup(ENV_ENTRY) {
        for line in split_entry_lines(&raw) {
            out.push(FixedMailbox::parse(line, allow_insecure)?);
        }
    }
    if let Some(path) = lookup(ENV_ENTRY_FILE)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 {ENV_ENTRY_FILE} 失败: {path}"))?;
        for line in split_entry_lines(&text) {
            out.push(FixedMailbox::parse(line, allow_insecure)?);
        }
    }
    Ok(out)
}

/// 一行一条：忽略空行和 `#` 注释，允许 `\n` / `\r\n` / `;` 分隔。
fn split_entry_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split(['\n', '\r', ';'])
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// 展示用地址：去掉查询串（`api_key` 之类），只留 `scheme://host/path`。
pub fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((head, _)) => format!("{head}?<redacted>"),
        None => url.to_string(),
    }
}

// ─────────────────────────── 响应模型 ───────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MailboxResponse {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub code_url: String,
    #[serde(default)]
    pub expires_at: Option<JsonValue>,
}

/// 领到的邮箱。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    pub email: String,
    pub code_url: String,
    pub expires_at: Option<String>,
}

impl Mailbox {
    pub fn from_response(raw: MailboxResponse, allow_insecure: bool) -> Result<Self> {
        let email = raw.email.trim().to_string();
        let code_url = raw.code_url.trim().to_string();
        if !email.contains('@') {
            bail!("邮箱 API 返回的 email 非法: {email:?}");
        }
        check_fetchable_url("code_url", &code_url, allow_insecure)?;
        Ok(Self {
            email,
            code_url,
            expires_at: raw.expires_at.map(|v| match v {
                JsonValue::String(s) => s,
                other => other.to_string(),
            }),
        })
    }
}

/// 取码页地址必须可安全抓取：`https://`，或在 `REG_MAIL_ALLOW_INSECURE=1`
/// 下才放行的回环 `http://`。
fn check_fetchable_url(field: &str, url: &str, allow_insecure: bool) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if url.starts_with("http://") && allow_insecure && is_loopback_url(url) {
        return Ok(());
    }
    bail!("取码地址不是 https（且未放行回环 http）: {field} = {url:?}")
}

/// 解析 `POST /mailboxes` 的响应（201）。
pub fn parse_mailbox_response(status: u16, body: &str, allow_insecure: bool) -> Result<Mailbox> {
    if status != 201 {
        let detail = serde_json::from_str::<JsonValue>(body)
            .ok()
            .and_then(|v| v.get("error").map(|e| e.to_string()))
            .unwrap_or_else(|| body.chars().take(200).collect());
        bail!("创建邮箱失败: HTTP {status}: {detail}");
    }
    let raw: MailboxResponse =
        serde_json::from_str(body).context("邮箱 API 响应不是合法 JSON")?;
    Mailbox::from_response(raw, allow_insecure)
}

/// 解析 `GET /health` 的响应（可选端点）。
pub fn parse_health_response(status: u16, body: &str) -> Result<JsonValue> {
    if !(200..300).contains(&status) {
        bail!("邮箱 API 健康检查失败: HTTP {status}");
    }
    let value: JsonValue =
        serde_json::from_str(body).context("健康检查响应不是合法 JSON")?;
    if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        bail!("邮箱 API 健康检查返回异常: {value}");
    }
    Ok(value)
}

// ─────────────────────────── 客户端 ───────────────────────────

/// 通用邮箱 API 客户端。`new_offline` 只用于 dry-run 构造请求计划。
pub struct MailApiClient<'a> {
    http: Option<&'a Client>,
    cfg: EmailApiConfig,
}

impl<'a> MailApiClient<'a> {
    pub fn new(http: &'a Client, cfg: EmailApiConfig) -> Self {
        Self {
            http: Some(http),
            cfg,
        }
    }

    pub fn new_offline(cfg: EmailApiConfig) -> Self {
        Self { http: None, cfg }
    }

    fn http(&self) -> Result<&'a Client> {
        self.http
            .context("MailApiClient 未绑定 HTTP client（offline 模式不能发请求）")
    }

    pub fn config(&self) -> &EmailApiConfig {
        &self.cfg
    }

    pub fn health_spec(&self) -> RequestSpec {
        RequestSpec::get("email.health", self.cfg.endpoint("health")).with_headers(vec![(
            "accept".to_string(),
            "application/json".to_string(),
        )])
    }

    pub fn create_mailbox_spec(
        &self,
        local_part: Option<&str>,
        subdomain: Option<&str>,
    ) -> RequestSpec {
        let mut body = serde_json::Map::new();
        if let Some(local) = local_part.map(str::trim).filter(|s| !s.is_empty()) {
            body.insert("local_part".into(), json!(local));
        }
        if let Some(sub) = subdomain.map(str::trim).filter(|s| !s.is_empty()) {
            body.insert("subdomain".into(), json!(sub));
        }
        let mut headers = vec![("accept".to_string(), "application/json".to_string())];
        if let Some(auth) = self.cfg.auth_header() {
            headers.push(auth);
        }
        RequestSpec::post_json(
            "email.create_mailbox",
            self.cfg.endpoint("mailboxes"),
            Some(headers),
            Some(JsonValue::Object(body)),
        )
    }

    pub fn inbox_spec(&self, code_url: &str) -> RequestSpec {
        inbox_spec(code_url)
    }

    pub async fn health(&self) -> Result<JsonValue> {
        let resp = self.http()?.execute(&self.health_spec()).await?;
        parse_health_response(resp.status, &resp.body)
    }

    pub async fn create_mailbox(
        &self,
        local_part: Option<&str>,
        subdomain: Option<&str>,
    ) -> Result<Mailbox> {
        let resp = self
            .http()?
            .execute(&self.create_mailbox_spec(local_part, subdomain))
            .await?;
        let mailbox = parse_mailbox_response(resp.status, &resp.body, self.cfg.allow_insecure)?;
        info!("[email] 已领取邮箱: {}", mailbox.email);
        Ok(mailbox)
    }

    pub async fn fetch_inbox(&self, code_url: &str) -> Result<String> {
        fetch_inbox(self.http()?, code_url).await
    }

    /// 轮询取码：`max_wait_secs` 内每 `interval_secs` 拉一次取码页。
    pub async fn poll_otp(
        &self,
        code_url: &str,
        max_wait_secs: u64,
        interval_secs: u64,
    ) -> Result<String> {
        poll_otp(self.http()?, code_url, max_wait_secs, interval_secs).await
    }
}

// ─────────────────────────── 取码（与领取方式无关）───────────────────────────

/// 取码页请求：HTML 卡片和 JSON 接口都可能，所以两种 `accept` 都带上。
pub fn inbox_spec(code_url: &str) -> RequestSpec {
    RequestSpec::get("email.inbox", code_url.to_string()).with_headers(vec![
        (
            "accept".to_string(),
            "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8".to_string(),
        ),
        (
            "cache-control".to_string(),
            "no-cache, no-store, max-age=0".to_string(),
        ),
        ("pragma".to_string(), "no-cache".to_string()),
    ])
}

/// 拉一次取码页正文（HTML 或 JSON，交给 [`super::otp::parse_cards_any`] 判定）。
pub async fn fetch_inbox(http: &Client, code_url: &str) -> Result<String> {
    let resp = http.execute(&inbox_spec(code_url)).await?;
    if !resp.is_success() {
        bail!(
            "取码页返回 HTTP {}: {}",
            resp.status,
            resp.body.chars().take(200).collect::<String>()
        );
    }
    Ok(resp.body)
}

/// 轮询取码：`max_wait_secs` 内每 `interval_secs` 拉一次取码页。
///
/// 支持两种响应：HTML 卡片页、JSON 邮件接口（都规约成 [`otp::InboxCard`]）。
pub async fn poll_otp(
    http: &Client,
    code_url: &str,
    max_wait_secs: u64,
    interval_secs: u64,
) -> Result<String> {
    let started = now_ts();
    let deadline = Instant::now() + Duration::from_secs(max_wait_secs);
    let interval = Duration::from_secs(interval_secs.max(1));
    let mut baseline: Option<HashSet<String>> = None;
    let mut last_error = String::from("尚未出现本次请求之后的新验证码");

    info!(
        "[email] 开始轮询验证码：最长 {}s，间隔 {}s",
        max_wait_secs,
        interval.as_secs()
    );
    while Instant::now() < deadline {
        match fetch_inbox(http, code_url).await {
            Ok(body) => {
                let cards = otp::parse_cards_any(&body);
                if baseline.is_none() {
                    baseline = Some(
                        cards
                            .iter()
                            .filter(|card| otp::card_timestamp(card).is_none())
                            .filter_map(|card| card.otp())
                            .collect(),
                    );
                }
                let empty = HashSet::new();
                if let Some(code) = otp::select_fresh_otp_from_cards(
                    &cards,
                    Some(started),
                    baseline.as_ref().unwrap_or(&empty),
                ) {
                    info!("[email] 已取得验证码");
                    return Ok(code);
                }
            }
            Err(err) => {
                last_error = format!("{err:#}");
                warn!("[email] 取码页请求失败: {last_error}");
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(interval.min(remaining)).await;
    }
    bail!("等待邮箱验证码超时: {last_error}")
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
