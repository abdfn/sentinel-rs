#!/usr/bin/env python3
"""sentinel-rs 控制台后端。

职责：
* 只读展示：邮箱池（mail.txt）/ 账号池（SQLite）/ 代理池（SQLite）
* 代理池配置：增删、启用/停用（落库，注册时 CLI 自动读取启用的代理）
* 账号套餐：调用 chatgpt backend-api 拉 plan / trial 信息并落库

零第三方依赖：stdlib http.server + sqlite3 + urllib。
"""
from __future__ import annotations

import argparse
import json
import os
import sqlite3
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

ROOT = os.path.dirname(os.path.abspath(__file__))
DB_PATH = os.environ.get("SENTINEL_DB_PATH", os.path.join(ROOT, "..", "data", "sentinel-rs.db"))
MAIL_FILE = os.environ.get("MAIL_FILE", os.path.join(ROOT, "..", "mail.txt"))
PROXY_FILE = os.environ.get("PROXY_FILE", os.path.join(ROOT, "..", "proxy.txt"))

UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36"
)
PLAN_URL = "https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27"


# ─────────────────────────── SQLite ───────────────────────────

def db() -> sqlite3.Connection:
    os.makedirs(os.path.dirname(os.path.abspath(DB_PATH)), exist_ok=True)
    conn = sqlite3.connect(DB_PATH, timeout=15)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.executescript(
        """
        CREATE TABLE IF NOT EXISTS proxy_pool (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            line            TEXT NOT NULL UNIQUE,
            enabled         INTEGER NOT NULL DEFAULT 1,
            note            TEXT NOT NULL DEFAULT '',
            last_status     TEXT NOT NULL DEFAULT '',
            last_checked_at INTEGER NOT NULL DEFAULT 0,
            created_at      INTEGER NOT NULL DEFAULT 0
        );
        """
    )
    # 老库补列（与 Rust store.rs 对齐）
    cols = {row["name"] for row in conn.execute("PRAGMA table_info(accounts)")}
    for name, ddl in [
        ("plan_type", "TEXT NOT NULL DEFAULT ''"),
        ("plan_display", "TEXT NOT NULL DEFAULT ''"),
        ("is_trial", "INTEGER NOT NULL DEFAULT 0"),
        ("trial_state", "TEXT NOT NULL DEFAULT ''"),
        ("eligible_new_user", "INTEGER NOT NULL DEFAULT 0"),
        ("has_discount", "INTEGER NOT NULL DEFAULT 0"),
        ("discount_summary", "TEXT NOT NULL DEFAULT ''"),
        ("plan_updated_at", "INTEGER NOT NULL DEFAULT 0"),
    ]:
        if cols and name not in cols:
            conn.execute(f"ALTER TABLE accounts ADD COLUMN {name} {ddl}")
    conn.commit()
    return conn


def seed_proxies_from_file(conn: sqlite3.Connection) -> None:
    """库里代理池为空时，把 proxy.txt 导进去（只导一次）。"""
    count = conn.execute("SELECT COUNT(*) FROM proxy_pool").fetchone()[0]
    if count or not os.path.exists(PROXY_FILE):
        return
    added = 0
    with open(PROXY_FILE, "r", encoding="utf-8-sig", errors="replace") as fh:
        for raw in fh:
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            cur = conn.execute(
                "INSERT INTO proxy_pool (line, enabled, created_at) VALUES (?, 1, strftime('%s','now'))"
                " ON CONFLICT(line) DO NOTHING",
                (line,),
            )
            added += cur.rowcount
    conn.commit()
    if added:
        print(f"[console] 已从 {PROXY_FILE} 导入 {added} 条代理", flush=True)


# ─────────────────────────── 读取 ───────────────────────────

def mask(value: str, keep: int = 6) -> str:
    value = (value or "").strip()
    if not value:
        return ""
    if len(value) <= keep:
        return "*" * len(value)
    return value[:2] + "*" * 6 + value[-keep:]


def parse_proxy_line(line: str) -> dict:
    parts = line.split(":")
    if len(parts) == 4:
        host, port, user, _pwd = parts
    elif len(parts) == 2:
        host, port, user = parts[0], parts[1], ""
    else:
        return {"host": "", "port": "", "session": "", "user_label": "", "valid": False}
    session = ""
    segs = user.split("-")
    for i, seg in enumerate(segs):
        if seg == "session" and i + 1 < len(segs):
            session = segs[i + 1]
    user_label = "-".join(segs[:2]) if len(segs) >= 2 else user
    return {"host": host, "port": port, "session": session, "user_label": user_label, "valid": True}


def read_accounts(conn: sqlite3.Connection) -> list[dict]:
    rows = conn.execute(
        "select id,email,status,access_token,totp_secret,plan_type,plan_display,"
        "is_trial,trial_state,eligible_new_user,has_discount,discount_summary,"
        "plan_updated_at,created_at from accounts order by id"
    ).fetchall()
    out = []
    for r in rows:
        out.append(
            {
                "id": r["id"],
                "email": r["email"],
                "status": r["status"],
                "has_token": bool((r["access_token"] or "").strip()),
                "token_masked": mask(r["access_token"] or ""),
                "has_totp": bool((r["totp_secret"] or "").strip()),
                "totp_masked": mask(r["totp_secret"] or ""),
                "plan_type": r["plan_type"] or "",
                "plan_display": r["plan_display"] or "",
                "is_trial": bool(r["is_trial"]),
                "trial_state": r["trial_state"] or "",
                "eligible_new_user": bool(r["eligible_new_user"]),
                "has_discount": bool(r["has_discount"]),
                "discount_summary": r["discount_summary"] or "",
                "plan_updated_at": r["plan_updated_at"] or 0,
                "created_at": r["created_at"] or 0,
            }
        )
    return out


def read_mail() -> list[dict]:
    items = []
    if not os.path.exists(MAIL_FILE):
        return items
    with open(MAIL_FILE, "r", encoding="utf-8-sig", errors="replace") as fh:
        for raw in fh:
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            email, _, code_url = line.partition("----")
            items.append(
                {
                    "email": email.strip(),
                    "code_url": code_url.split("?", 1)[0],
                    "has_key": "api_key=" in code_url,
                }
            )
    return items


def read_proxies(conn: sqlite3.Connection) -> list[dict]:
    rows = conn.execute(
        "select id,line,enabled,note,last_status,last_checked_at,created_at "
        "from proxy_pool order by id"
    ).fetchall()
    out = []
    for r in rows:
        info = parse_proxy_line(r["line"])
        out.append(
            {
                "id": r["id"],
                "host": info["host"],
                "port": info["port"],
                "session": info["session"],
                "user_label": info["user_label"],
                "valid": info["valid"],
                "enabled": bool(r["enabled"]),
                "note": r["note"] or "",
                "last_status": r["last_status"] or "",
            }
        )
    return out


# ─────────────────────────── 套餐 ───────────────────────────

def fetch_plan(token: str) -> dict:
    req = urllib.request.Request(
        PLAN_URL,
        headers={
            "authorization": "Bearer " + token,
            "accept": "application/json",
            "user-agent": UA,
            "referer": "https://chatgpt.com/",
        },
    )
    with urllib.request.urlopen(req, timeout=25) as resp:
        data = json.loads(resp.read().decode())

    accounts = data.get("accounts") or {}
    entry = None
    for key, value in accounts.items():
        if key != "default":
            entry = value
            break
    if entry is None:
        entry = accounts.get("default") or {}

    account = entry.get("account") or {}
    entitlement = entry.get("entitlement") or {}
    trial = entitlement.get("trial")

    # 真优惠只认这几个信号：折扣对象、已应用折扣、促销活动、试用。
    discount = entitlement.get("discount")
    applied = entitlement.get("applied_discounts") or []
    campaigns = entry.get("eligible_promo_campaigns") or {}
    promo = account.get("promo_data") or {}

    parts = []
    if isinstance(discount, dict) and discount:
        parts.append("折扣 " + json.dumps(discount, ensure_ascii=False)[:80])
    elif discount:
        parts.append(f"折扣 {discount}")
    if applied:
        parts.append("已应用 " + json.dumps(applied, ensure_ascii=False)[:80])
    if campaigns:
        parts.append("活动 " + json.dumps(campaigns, ensure_ascii=False)[:80])
    if isinstance(promo, dict) and promo:
        parts.append("促销 " + json.dumps(promo, ensure_ascii=False)[:80])
    if trial not in (None, {}, []):
        parts.append("试用 " + json.dumps(trial, ensure_ascii=False)[:80])

    has_discount = bool(parts)

    return {
        "plan_type": account.get("plan_type") or "",
        "plan_display": account.get("plan_display_name") or account.get("plan_type") or "",
        "is_trial": bool(account.get("is_trial")),
        "trial_state": "" if account.get("trial_state") is None else str(account.get("trial_state")),
        "eligible_new_user": bool(entry.get("is_eligible_for_yearly_plus_new_user_subscription")),
        "has_active_subscription": bool(entitlement.get("has_active_subscription")),
        "trial_present": trial not in (None, {}, []),
        "has_discount": has_discount,
        "discount_summary": " | ".join(parts),
    }


# ─────────────────────────── HTTP ───────────────────────────

class Handler(BaseHTTPRequestHandler):
    server_version = "sentinel-console/1.1"

    def log_message(self, fmt, *args):
        pass

    def _send(self, status: int, body: bytes, ctype: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _json(self, payload: dict, status: int = 200) -> None:
        self._send(status, json.dumps(payload, ensure_ascii=False).encode(), "application/json; charset=utf-8")

    def _body(self) -> dict:
        length = int(self.headers.get("Content-Length") or 0)
        if not length:
            return {}
        try:
            return json.loads(self.rfile.read(length).decode())
        except Exception:
            return {}

    # ---------- GET ----------
    def do_GET(self) -> None:  # noqa: N802
        path = urlparse(self.path).path
        if path in ("/", "/index.html"):
            index = os.path.join(ROOT, "index.html")
            if not os.path.exists(index):
                return self._json({"error": "index.html 不存在"}, 500)
            with open(index, "rb") as fh:
                return self._send(200, fh.read(), "text/html; charset=utf-8")
        if path == "/api/summary":
            try:
                conn = db()
                seed_proxies_from_file(conn)
                accounts = read_accounts(conn)
                mail = read_mail()
                proxies = read_proxies(conn)
                conn.close()
                by_email = {a["email"].lower(): a for a in accounts}
                for entry in mail:
                    acc = by_email.get(entry["email"].lower())
                    entry["registered"] = bool(acc)
                    entry["account_id"] = acc["id"] if acc else None
                    entry["has_totp"] = bool(acc and acc["has_totp"])
                return self._json(
                    {
                        "accounts": accounts,
                        "mail": mail,
                        "proxies": proxies,
                        "stats": {
                            "accounts": len(accounts),
                            "registered": sum(1 for a in accounts if a["status"] == "registered"),
                            "with_totp": sum(1 for a in accounts if a["has_totp"]),
                            "with_plan": sum(1 for a in accounts if a["plan_display"]),
                            "trial": sum(1 for a in accounts if a["is_trial"] or a["trial_state"]),
                            "with_discount": sum(1 for a in accounts if a["has_discount"]),
                            "mail_total": len(mail),
                            "mail_used": sum(1 for m in mail if m["registered"]),
                            "proxy_total": len(proxies),
                            "proxy_enabled": sum(1 for p in proxies if p["enabled"]),
                        },
                        "paths": {
                            "db": os.path.abspath(DB_PATH),
                            "mail": os.path.abspath(MAIL_FILE),
                            "proxy_file": os.path.abspath(PROXY_FILE),
                        },
                    }
                )
            except Exception as err:
                return self._json({"error": f"{type(err).__name__}: {err}"}, 500)
        if path == "/api/health":
            return self._json({"ok": True})
        self._json({"error": "not found"}, 404)

    # ---------- POST ----------
    def do_POST(self) -> None:  # noqa: N802
        path = urlparse(self.path).path
        body = self._body()
        try:
            conn = db()
            if path == "/api/proxies":
                raw = body.get("lines") or body.get("line") or ""
                added = 0
                for line in raw.replace(";", "\n").splitlines():
                    line = line.strip()
                    if not line or line.startswith("#"):
                        continue
                    if parse_proxy_line(line)["valid"]:
                        cur = conn.execute(
                            "INSERT INTO proxy_pool (line, enabled, note, created_at)"
                            " VALUES (?, 1, ?, strftime('%s','now')) ON CONFLICT(line) DO NOTHING",
                            (line, body.get("note") or ""),
                        )
                        added += cur.rowcount
                conn.commit()
                conn.close()
                return self._json({"ok": True, "added": added})

            if path == "/api/proxies/import":
                added = 0
                if os.path.exists(PROXY_FILE):
                    with open(PROXY_FILE, "r", encoding="utf-8-sig", errors="replace") as fh:
                        for raw in fh:
                            line = raw.strip()
                            if not line or line.startswith("#") or not parse_proxy_line(line)["valid"]:
                                continue
                            cur = conn.execute(
                                "INSERT INTO proxy_pool (line, enabled, created_at)"
                                " VALUES (?, 1, strftime('%s','now')) ON CONFLICT(line) DO NOTHING",
                                (line,),
                            )
                            added += cur.rowcount
                conn.commit()
                conn.close()
                return self._json({"ok": True, "added": added})

            if path == "/api/proxies/toggle":
                pid = int(body.get("id") or 0)
                enabled = 1 if body.get("enabled") else 0
                cur = conn.execute("UPDATE proxy_pool SET enabled = ? WHERE id = ?", (enabled, pid))
                conn.commit()
                conn.close()
                return self._json({"ok": bool(cur.rowcount)})

            if path == "/api/proxies/delete":
                pid = int(body.get("id") or 0)
                cur = conn.execute("DELETE FROM proxy_pool WHERE id = ?", (pid,))
                conn.commit()
                conn.close()
                return self._json({"ok": bool(cur.rowcount)})

            if path == "/api/accounts/refresh":
                target = body.get("id")
                rows = conn.execute(
                    "select id,email,access_token from accounts"
                    + (" where id = ?" if target else "")
                    + " order by id",
                    ((int(target),) if target else ()),
                ).fetchall()
                results = []
                for r in rows:
                    token = (r["access_token"] or "").strip()
                    if not token:
                        results.append({"id": r["id"], "email": r["email"], "ok": False,
                                        "error": "没有 access_token"})
                        continue
                    try:
                        plan = fetch_plan(token)
                        conn.execute(
                            "update accounts set plan_type=?, plan_display=?, is_trial=?, "
                            "trial_state=?, eligible_new_user=?, has_discount=?, discount_summary=?, "
                            "plan_updated_at=strftime('%s','now') where id=?",
                            (
                                plan["plan_type"],
                                plan["plan_display"],
                                1 if plan["is_trial"] else 0,
                                plan["trial_state"],
                                1 if plan["eligible_new_user"] else 0,
                                1 if plan["has_discount"] else 0,
                                plan["discount_summary"],
                                r["id"],
                            ),
                        )
                        results.append({"id": r["id"], "email": r["email"], "ok": True, "plan": plan})
                    except urllib.error.HTTPError as err:
                        results.append({"id": r["id"], "email": r["email"], "ok": False,
                                        "error": f"HTTP {err.code}"})
                    except Exception as err:  # 网络/解析
                        results.append({"id": r["id"], "email": r["email"], "ok": False,
                                        "error": f"{type(err).__name__}: {err}"})
                conn.commit()
                conn.close()
                return self._json({"ok": True, "results": results})

            conn.close()
            self._json({"error": "not found"}, 404)
        except Exception as err:
            self._json({"error": f"{type(err).__name__}: {err}"}, 500)


def main() -> int:
    parser = argparse.ArgumentParser(description="sentinel-rs 控制台")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8790)
    args = parser.parse_args()

    conn = db()
    seed_proxies_from_file(conn)
    conn.close()

    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"[console] http://{args.host}:{args.port}", flush=True)
    print(f"[console] db={os.path.abspath(DB_PATH)}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
