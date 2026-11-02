//! rusqlite 持久化：建表 + 账号落库。
//!
//! 表结构对齐 Python `core/db.py` 的 `accounts` 语义（email 唯一、状态、时间），
//! 但把常用字段（password / access_token / 2fa secret / cookies）拍平成列，
//! 便于 Rust 侧直接查询（Python 侧是 `payload` JSON 一列）。
//!
//! 时间统一存 unix 秒（`TEXT` 列里放十进制数字），避免引入 chrono。

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: i64 = 3;

/// 一条注册结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    pub email: String,
    pub password: String,
    pub access_token: String,
    /// TOTP secret（base32）；没绑 2FA 时为空串。
    pub totp_secret: String,
    /// Cookie 头或 cookie JSON。
    pub cookies: String,
    /// `pending` / `registered` / `failed` …
    pub status: String,
    /// 套餐（`plan_type`：free / plus / pro …）。
    pub plan_type: String,
    /// 套餐展示名（`plan_display_name`：Free / Plus / Pro …）。
    pub plan_display: String,
    /// 服务端 `is_trial`。
    pub is_trial: bool,
    /// 服务端 `trial_state`（原样字符串，null 时为空串）。
    pub trial_state: String,
    /// `is_eligible_for_yearly_plus_new_user_subscription`：新用户优惠资格。
    pub eligible_new_user: bool,
    /// 是否真有折扣/促销（`discount` / `applied_discounts` / `eligible_promo_campaigns` 任一非空）。
    pub has_discount: bool,
    /// 折扣摘要（给前端直接展示）。
    pub discount_summary: String,
    /// 套餐信息最后刷新时间。
    pub plan_updated_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Account {
    pub fn new(email: impl Into<String>, password: impl Into<String>) -> Self {
        let now = now_ts();
        Self {
            email: email.into(),
            password: password.into(),
            status: "pending".to_string(),
            created_at: now,
            updated_at: now,
            ..Default::default()
        }
    }
}

/// SQLite 句柄（`Connection` 不是 Sync，单线程用）。
pub struct Store {
    conn: Connection,
}

impl Store {
    /// 打开文件库（自动建父目录）。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建数据库目录失败: {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开 SQLite 失败: {}", path.display()))?;
        // 并发注册时多个进程/任务同时写库：WAL + busy_timeout 避免 SQLITE_BUSY 丢记录。
        conn.busy_timeout(std::time::Duration::from_secs(15))
            .context("设置 SQLite busy_timeout 失败")?;
        Ok(Self { conn })
    }

    /// 内存库（单测用）。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("打开内存 SQLite 失败")?;
        Ok(Self { conn })
    }

    /// 建表 + 索引（幂等）。
    pub fn init(&self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;

                CREATE TABLE IF NOT EXISTS accounts (
                    id            INTEGER PRIMARY KEY AUTOINCREMENT,
                    email         TEXT    NOT NULL UNIQUE,
                    password      TEXT    NOT NULL DEFAULT '',
                    access_token  TEXT    NOT NULL DEFAULT '',
                    totp_secret   TEXT    NOT NULL DEFAULT '',
                    cookies       TEXT    NOT NULL DEFAULT '',
                    status        TEXT    NOT NULL DEFAULT 'pending',
                    created_at    INTEGER NOT NULL DEFAULT 0,
                    updated_at    INTEGER NOT NULL DEFAULT 0
                );

                CREATE INDEX IF NOT EXISTS idx_accounts_status  ON accounts(status, id DESC);
                CREATE INDEX IF NOT EXISTS idx_accounts_created ON accounts(created_at DESC);

                CREATE TABLE IF NOT EXISTS email_pool (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    email       TEXT NOT NULL UNIQUE,
                    code_url    TEXT NOT NULL DEFAULT '',
                    source      TEXT NOT NULL DEFAULT 'mail_api',
                    status      TEXT NOT NULL DEFAULT 'available',
                    created_at  INTEGER NOT NULL DEFAULT 0,
                    updated_at  INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS schema_meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS proxy_pool (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    line            TEXT NOT NULL UNIQUE,
                    enabled         INTEGER NOT NULL DEFAULT 1,
                    note            TEXT NOT NULL DEFAULT '',
                    last_status     TEXT NOT NULL DEFAULT '',
                    last_checked_at INTEGER NOT NULL DEFAULT 0,
                    created_at      INTEGER NOT NULL DEFAULT 0
                );
                "#,
            )
            .context("初始化 schema 失败")?;

        // 老库补列（幂等）
        for (name, ddl) in [
            ("plan_type", "TEXT NOT NULL DEFAULT ''"),
            ("plan_display", "TEXT NOT NULL DEFAULT ''"),
            ("is_trial", "INTEGER NOT NULL DEFAULT 0"),
            ("trial_state", "TEXT NOT NULL DEFAULT ''"),
            ("eligible_new_user", "INTEGER NOT NULL DEFAULT 0"),
            ("has_discount", "INTEGER NOT NULL DEFAULT 0"),
            ("discount_summary", "TEXT NOT NULL DEFAULT ''"),
            ("plan_updated_at", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            self.ensure_column("accounts", name, ddl)?;
        }
        self.conn
            .execute(
                "INSERT INTO schema_meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![SCHEMA_VERSION.to_string()],
            )
            .context("写入 schema_version 失败")?;
        Ok(())
    }

    /// 幂等补列：表里没有就 `ALTER TABLE ADD COLUMN`。
    fn ensure_column(&self, table: &str, column: &str, ddl: &str) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .with_context(|| format!("读取 {table} 表结构失败"))?;
        let exists = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .context("遍历表结构失败")?
            .filter_map(std::result::Result::ok)
            .any(|name| name.eq_ignore_ascii_case(column));
        drop(stmt);
        if !exists {
            self.conn
                .execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"), [])
                .with_context(|| format!("给 {table} 补列 {column} 失败"))?;
        }
        Ok(())
    }

    /// 保存账号（按 email upsert），返回行 id。
    pub fn save_account(&self, account: &Account) -> Result<i64> {
        if account.email.trim().is_empty() {
            anyhow::bail!("account.email 不能为空");
        }
        let updated_at = if account.updated_at == 0 {
            now_ts()
        } else {
            account.updated_at
        };
        let created_at = if account.created_at == 0 {
            updated_at
        } else {
            account.created_at
        };
        self.conn
            .execute(
                "INSERT INTO accounts
                    (email, password, access_token, totp_secret, cookies, status,
                     plan_type, plan_display, is_trial, trial_state, eligible_new_user,
                     has_discount, discount_summary, plan_updated_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(email) DO UPDATE SET
                    password        = excluded.password,
                    access_token    = excluded.access_token,
                    totp_secret     = excluded.totp_secret,
                    cookies         = excluded.cookies,
                    status          = excluded.status,
                    plan_type       = excluded.plan_type,
                    plan_display    = excluded.plan_display,
                    is_trial        = excluded.is_trial,
                    trial_state     = excluded.trial_state,
                    eligible_new_user = excluded.eligible_new_user,
                    has_discount    = excluded.has_discount,
                    discount_summary = excluded.discount_summary,
                    plan_updated_at = excluded.plan_updated_at,
                    updated_at      = excluded.updated_at",
                params![
                    account.email,
                    account.password,
                    account.access_token,
                    account.totp_secret,
                    account.cookies,
                    account.status,
                    account.plan_type,
                    account.plan_display,
                    account.is_trial as i64,
                    account.trial_state,
                    account.eligible_new_user as i64,
                    account.has_discount as i64,
                    account.discount_summary,
                    account.plan_updated_at,
                    created_at,
                    updated_at,
                ],
            )
            .context("写入 accounts 失败")?;
        self.conn
            .query_row(
                "SELECT id FROM accounts WHERE email = ?1",
                params![account.email],
                |row| row.get::<_, i64>(0),
            )
            .context("读取刚写入账号的 id 失败")
    }

    pub fn get_account(&self, email: &str) -> Result<Option<Account>> {
        self.conn
            .query_row(
                "SELECT email, password, access_token, totp_secret, cookies, status,
                        plan_type, plan_display, is_trial, trial_state, eligible_new_user,
                        has_discount, discount_summary, plan_updated_at, created_at, updated_at
                 FROM accounts WHERE email = ?1",
                params![email],
                row_to_account,
            )
            .optional()
            .context("查询 accounts 失败")
    }

    pub fn list_accounts(&self, limit: usize) -> Result<Vec<Account>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT email, password, access_token, totp_secret, cookies, status,
                        plan_type, plan_display, is_trial, trial_state, eligible_new_user,
                        has_discount, discount_summary, plan_updated_at, created_at, updated_at
                 FROM accounts ORDER BY id DESC LIMIT ?1",
            )
            .context("准备查询语句失败")?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_account)
            .context("查询 accounts 失败")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("读取 accounts 行失败")?);
        }
        Ok(out)
    }

    // ── 代理池 ────────────────────────────────────────────────────────

    /// 列出代理池（`enabled_only` 时只取启用的），按 id 升序。
    pub fn list_proxies(&self, enabled_only: bool) -> Result<Vec<ProxyEntry>> {
        let sql = if enabled_only {
            "SELECT id, line, enabled, note, last_status, last_checked_at, created_at
             FROM proxy_pool WHERE enabled = 1 ORDER BY id"
        } else {
            "SELECT id, line, enabled, note, last_status, last_checked_at, created_at
             FROM proxy_pool ORDER BY id"
        };
        let mut stmt = self.conn.prepare(sql).context("准备代理池查询失败")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ProxyEntry {
                    id: row.get(0)?,
                    line: row.get(1)?,
                    enabled: row.get::<_, i64>(2)? != 0,
                    note: row.get(3)?,
                    last_status: row.get(4)?,
                    last_checked_at: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .context("查询代理池失败")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("读取代理池行失败")?);
        }
        Ok(out)
    }

    /// 新增代理；已存在（line 相同）返回 `Ok(false)`。
    pub fn add_proxy(&self, line: &str, note: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "INSERT INTO proxy_pool (line, enabled, note, created_at) VALUES (?1, 1, ?2, ?3)
                 ON CONFLICT(line) DO NOTHING",
                params![line.trim(), note.trim(), now_ts()],
            )
            .context("写入 proxy_pool 失败")?;
        Ok(changed > 0)
    }

    pub fn set_proxy_enabled(&self, id: i64, enabled: bool) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "UPDATE proxy_pool SET enabled = ?1 WHERE id = ?2",
                params![enabled as i64, id],
            )
            .context("更新 proxy_pool 失败")?;
        Ok(changed > 0)
    }

    pub fn delete_proxy(&self, id: i64) -> Result<bool> {
        let changed = self
            .conn
            .execute("DELETE FROM proxy_pool WHERE id = ?1", params![id])
            .context("删除 proxy_pool 失败")?;
        Ok(changed > 0)
    }

    pub fn count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get::<_, i64>(0))
            .context("统计 accounts 失败")
    }

    pub fn schema_version(&self) -> Result<i64> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("读取 schema_version 失败")?;
        Ok(raw.and_then(|v| v.parse().ok()).unwrap_or(0))
    }
}

/// 代理池里的一条记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEntry {
    pub id: i64,
    pub line: String,
    pub enabled: bool,
    pub note: String,
    pub last_status: String,
    pub last_checked_at: i64,
    pub created_at: i64,
}

fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        email: row.get(0)?,
        password: row.get(1)?,
        access_token: row.get(2)?,
        totp_secret: row.get(3)?,
        cookies: row.get(4)?,
        status: row.get(5)?,
        plan_type: row.get(6)?,
        plan_display: row.get(7)?,
        is_trial: row.get::<_, i64>(8)? != 0,
        trial_state: row.get(9)?,
        eligible_new_user: row.get::<_, i64>(10)? != 0,
        has_discount: row.get::<_, i64>(11)? != 0,
        discount_summary: row.get(12)?,
        plan_updated_at: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

/// 当前 unix 秒。
pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
