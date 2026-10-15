//! SQLite 持久层（rusqlite）：hosts / known_hosts / settings / audit_log 统一存储。
//! 与旧 Node Agent（node:sqlite）写出的 bitgalaxy.db 完全兼容；
//! 首次打开时自动从历史 JSON 文件迁移（迁移后改名 .bak）。
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

pub type DbResult<T> = Result<T, rusqlite::Error>;

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
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS hosts (
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
                );
                CREATE TABLE IF NOT EXISTS settings (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS audit_log (
                    id   INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts   TEXT NOT NULL,
                    type TEXT NOT NULL,
                    meta TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS chat_messages (
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    chat_id    TEXT NOT NULL,
                    role       TEXT NOT NULL,
                    content    TEXT,
                    thinking   TEXT,
                    error      INTEGER NOT NULL DEFAULT 0,
                    tool_json  TEXT,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_chat_messages_chat ON chat_messages(chat_id, id);",
            )
            .expect("初始化数据库 schema 失败");
    }

    /// 在锁内执行只读/写操作（低频短事务，WAL 下足够）
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> DbResult<T>) -> DbResult<T> {
        let conn = self.0.lock().unwrap();
        f(&conn)
    }

    /// 首次打开时从旧 JSON 文件迁移（仅当目标表为空且文件存在）
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

    fn migrate_hosts(&self, data_dir: &Path) {
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

    fn migrate_known_hosts(&self, data_dir: &Path) {
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

    fn migrate_settings(&self, data_dir: &Path) {
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

    fn migrate_audit(&self, data_dir: &Path) {
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
    fn migrate_ai(&self, data_dir: &Path) {
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
