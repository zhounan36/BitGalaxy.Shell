//! SQLite 持久层（rusqlite）。P6.5 按域拆库：
//! - hosts.db（hosts + known_hosts：SSH 应用与 AI 基座共用读写）
//! - settings.db（settings + audit_log）
//! - chat.db（chat_messages，仅 AI 基座写入）
//! 迁移链（均幂等，见 Stores::open_domains）：旧单库 bitgalaxy.db → 域库
//! （成功后旧库改名 .migrated.bak）；更早的 JSON 文件 → 对应域库（迁移后改名 .bak）。
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

pub type DbResult<T> = Result<T, rusqlite::Error>;

/// hosts 域 schema（hosts.db）
pub const HOSTS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS hosts (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    host             TEXT NOT NULL,
    port             INTEGER NOT NULL DEFAULT 22,
    username         TEXT NOT NULL,
    auth_kind        TEXT NOT NULL,
    password         TEXT,
    private_key_path TEXT,
    passphrase       TEXT,
    tags             TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS known_hosts (
    host        TEXT NOT NULL,
    port        INTEGER NOT NULL,
    key_type    TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    PRIMARY KEY (host, port)
);";

/// settings 域 schema（settings.db：键值设置 + 审计日志）
pub const SETTINGS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS audit_log (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    ts   TEXT NOT NULL,
    type TEXT NOT NULL,
    meta TEXT NOT NULL
);";

/// chat 域 schema（chat.db：AI 对话记录，仅基座写入）
pub const CHAT_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS chat_messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id    TEXT NOT NULL,
    role       TEXT NOT NULL,
    content    TEXT,
    thinking   TEXT,
    error      INTEGER NOT NULL DEFAULT 0,
    tool_json  TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chat_messages_chat ON chat_messages(chat_id, id);";

impl Db {
    pub fn open(file: &Path) -> Db {
        let db = Db::open_raw(file);
        db.init_schema();
        db
    }

    /// 独立域库（如 sql-connections.db / hosts.db / settings.db / chat.db）：
    /// 只开连接 + WAL，不初始化默认 schema（各域自己建表）
    pub fn open_domain(file: &Path, schema: &str) -> Db {
        let db = Db::open_raw(file);
        db.0
            .lock()
            .unwrap()
            .execute_batch(&format!(
                "PRAGMA busy_timeout = 2000;{schema}"
            ))
            .expect("初始化域库 schema 失败");
        db
    }

    fn open_raw(file: &Path) -> Db {
        let conn = Connection::open(file).expect("打开 SQLite 数据库失败");
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA journal_size_limit = 67108864;
             PRAGMA busy_timeout = 2000;",
        )
        .ok();
        Db(Arc::new(Mutex::new(conn)))
    }

    fn init_schema(&self) {
        self.0
            .lock()
            .unwrap()
            .execute_batch(&format!(
                "{HOSTS_SCHEMA}{SETTINGS_SCHEMA}{CHAT_SCHEMA}"
            ))
            .expect("初始化数据库 schema 失败");
    }

    /// 在锁内执行只读/写操作（低频短事务，WAL 下足够）
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> DbResult<T>) -> DbResult<T> {
        let conn = self.0.lock().unwrap();
        f(&conn)
    }

    /// 首次打开时从旧 JSON 文件迁移（仅当目标表为空且文件存在）。
    /// P6.5 拆库后按域分别调用：hosts 域 migrate_hosts/migrate_known_hosts，
    /// settings 域 migrate_settings/migrate_audit/migrate_ai（见 Stores::open_domains）
    pub fn migrate_from_json(&self, data_dir: &Path) {
        self.migrate_hosts(data_dir);
        self.migrate_known_hosts(data_dir);
        self.migrate_settings(data_dir);
        self.migrate_audit(data_dir);
        self.migrate_ai(data_dir);
    }

    fn backup(file: &PathBuf) {
        let mut bak = file.clone().into_os_string();
        bak.push(".bak");
        let _ = std::fs::rename(file, PathBuf::from(bak));
    }

    pub fn migrate_hosts(&self, data_dir: &Path) {
        let file = data_dir.join("hosts.json");
        if !file.is_file() {
            return;
        }
        let count: i64 = self
            .with(|c| c.query_row("SELECT COUNT(*) FROM hosts", [], |r| r.get(0)))
            .unwrap_or(0);
        if count > 0 {
            return;
        }
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                let _ = self.with(|c| {
                    for h in &rows {
                        let auth = &h["auth"];
                        c.execute(
                            "INSERT INTO hosts (id, name, host, port, username, auth_kind, password, private_key_path, passphrase, tags, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                            rusqlite::params![
                                h["id"].as_str().unwrap_or_default(),
                                h["name"].as_str().unwrap_or_default(),
                                h["host"].as_str().unwrap_or_default(),
                                h["port"].as_i64().unwrap_or(22),
                                h["username"].as_str().unwrap_or_default(),
                                auth["kind"].as_str().unwrap_or("password"),
                                auth["password"].as_str(),
                                auth["privateKeyPath"].as_str(),
                                auth["passphrase"].as_str(),
                                h["tags"].as_array().map(|t| serde_json::to_string(t).unwrap_or_default()).as_deref(),
                                h["createdAt"].as_str().unwrap_or_default(),
                                h["updatedAt"].as_str().unwrap_or_default(),
                            ],
                        )?;
                    }
                    Ok(())
                });
            }
        }
        Self::backup(&file);
    }

    pub fn migrate_known_hosts(&self, data_dir: &Path) {
        let file = data_dir.join("known-hosts.json");
        if !file.is_file() {
            return;
        }
        let count: i64 = self
            .with(|c| c.query_row("SELECT COUNT(*) FROM known_hosts", [], |r| r.get(0)))
            .unwrap_or(0);
        if count > 0 {
            return;
        }
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                let _ = self.with(|c| {
                    for e in &rows {
                        c.execute(
                            "INSERT OR REPLACE INTO known_hosts (host, port, key_type, fingerprint) VALUES (?1,?2,?3,?4)",
                            rusqlite::params![
                                e["host"].as_str().unwrap_or_default(),
                                e["port"].as_i64().unwrap_or(22),
                                e["keyType"].as_str().unwrap_or_default(),
                                e["fingerprint"].as_str().unwrap_or_default(),
                            ],
                        )?;
                    }
                    Ok(())
                });
            }
        }
        Self::backup(&file);
    }

    pub fn migrate_settings(&self, data_dir: &Path) {
        let file = data_dir.join("settings.json");
        if !file.is_file() {
            return;
        }
        let count: i64 = self
            .with(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM settings WHERE key LIKE 'app.%'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap_or(0);
        if count > 0 {
            return;
        }
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text) {
                let _ = self.with(|c| {
                    for (k, v) in &map {
                        c.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                            rusqlite::params![format!("app.{k}"), serde_json::to_string(v).unwrap_or_default()],
                        )?;
                    }
                    Ok(())
                });
            }
        }
        Self::backup(&file);
    }

    pub fn migrate_audit(&self, data_dir: &Path) {
        let file = data_dir.join("audit-queue.jsonl");
        if !file.is_file() {
            return;
        }
        let count: i64 = self
            .with(|c| c.query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0)))
            .unwrap_or(0);
        if count > 0 {
            return;
        }
        if let Ok(text) = std::fs::read_to_string(&file) {
            let _ = self.with(|c| {
                for line in text.lines().filter(|l| !l.is_empty()) {
                    let Ok(ev) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(line) else {
                        continue;
                    };
                    let ts = ev.get("ts").and_then(|v| v.as_str()).unwrap_or_default();
                    let ty = ev.get("type").and_then(|v| v.as_str()).unwrap_or_default();
                    let meta: serde_json::Map<String, serde_json::Value> =
                        ev.iter().filter(|(k, _)| k != &"ts" && k != &"type").map(|(k, v)| (k.clone(), v.clone())).collect();
                    c.execute(
                        "INSERT INTO audit_log (ts, type, meta) VALUES (?1, ?2, ?3)",
                        rusqlite::params![ts, ty, serde_json::to_string(&meta).unwrap_or_default()],
                    )?;
                }
                Ok(())
            });
        }
        Self::backup(&file);
    }

    /// ai.json → settings(ai.apiKey / ai.baseUrl / ai.model)
    pub fn migrate_ai(&self, data_dir: &Path) {
        let file = data_dir.join("ai.json");
        if !file.is_file() {
            return;
        }
        let count: i64 = self
            .with(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM settings WHERE key LIKE 'ai.%'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap_or(0);
        if count > 0 {
            return;
        }
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text) {
                let _ = self.with(|c| {
                    for (k, v) in &map {
                        if v.is_null() {
                            continue;
                        }
                        c.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                            rusqlite::params![format!("ai.{k}"), serde_json::to_string(v).unwrap_or_default()],
                        )?;
                    }
                    Ok(())
                });
            }
        }
        Self::backup(&file);
    }
}

// ── P6.5 旧单库 → 域库迁移 ───────────────────────────────────────────────

/// 旧单库 bitgalaxy.db → hosts.db / settings.db / chat.db：
/// 逐表 INSERT OR REPLACE（幂等，失败重试不产生重复行），全部成功后旧库改名
/// `bitgalaxy.db.migrated.bak` 并清掉残留 -wal/-shm。bak 已存在（迁移过）直接跳过。
/// 迁移失败返回 Err：调用方记录日志继续用域库启动，下次启动自动重试。
pub fn migrate_from_legacy_db(data_dir: &Path, hosts: &Db, settings: &Db, chat: &Db) -> Result<(), String> {
    let legacy_path = data_dir.join("bitgalaxy.db");
    if !legacy_path.is_file() {
        return Ok(());
    }
    let bak_path = data_dir.join("bitgalaxy.db.migrated.bak");
    if bak_path.is_file() {
        return Ok(()); // 已迁移过
    }

    let copy = |db: &Db, tables: &[(&str, &str)]| -> Result<(), String> {
        for (legacy_table, domain_table) in tables {
            db.with(|c| {
                c.execute(
                    &format!(
                        "INSERT OR REPLACE INTO main.{domain_table}
                         SELECT * FROM legacy.{legacy_table}
                         WHERE EXISTS (SELECT 1 FROM legacy.sqlite_master
                                       WHERE type='table' AND name='{legacy_table}')"
                    ),
                    [],
                )?;
                Ok(())
            })
            .map_err(|e| format!("迁移表 {legacy_table} 失败：{e}"))?;
        }
        Ok(())
    };

    // 旧库连接：只读用途打开（不初始化 schema），ATTACH 到各域库再逐表搬运
    let legacy = Connection::open(&legacy_path).map_err(|e| format!("打开旧库失败：{e}"))?;
    legacy
        .execute_batch("PRAGMA busy_timeout = 2000;")
        .ok();
    let attach = |db: &Db| -> Result<(), String> {
        db.with(|c| {
            c.execute("ATTACH DATABASE ?1 AS legacy", [legacy_path.to_string_lossy().as_ref()])?;
            Ok(())
        })
        .map_err(|e| format!("ATTACH 旧库失败：{e}"))
    };
    let detach = |db: &Db| {
        let _ = db.with(|c| c.execute("DETACH DATABASE legacy", []));
    };

    attach(hosts)?;
    let r1 = copy(hosts, &[("hosts", "hosts"), ("known_hosts", "known_hosts")]);
    detach(hosts);
    r1?;

    attach(settings)?;
    let r2 = copy(settings, &[("settings", "settings"), ("audit_log", "audit_log")]);
    detach(settings);
    r2?;

    attach(chat)?;
    let r3 = copy(chat, &[("chat_messages", "chat_messages")]);
    detach(chat);
    r3?;

    // 搬运完成：checkpoint 旧库 WAL 后改名归档（残留 -wal/-shm 一并清理）
    let _ = legacy.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    drop(legacy);
    std::fs::rename(&legacy_path, &bak_path)
        .map_err(|e| format!("旧库改名失败（数据已迁移，下次启动将重试）：{e}"))?;
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(data_dir.join(format!("bitgalaxy.db{ext}")));
    }
    Ok(())
}
