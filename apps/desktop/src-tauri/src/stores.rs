//! 业务存储：主机档案 / 已知主机(TOFU) / 键值设置 / 审计队列（全部 SQLite）。
use rusqlite::OptionalExtension;

use crate::db::Db;
use crate::protocol::{HostAuth, HostInput, HostRecord};

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(Clone)]
pub struct HostStore {
    db: Db,
}

fn row_to_record(row: &rusqlite::Row) -> rusqlite::Result<HostRecord> {
    let tags: Option<String> = row.get("tags")?;
    Ok(HostRecord {
        id: row.get("id")?,
        input: HostInput {
            name: row.get("name")?,
            host: row.get("host")?,
            port: row.get::<_, i64>("port")? as u16,
            username: row.get("username")?,
            auth: HostAuth {
                kind: row.get("auth_kind")?,
                password: row.get("password")?,
                private_key_path: row.get("private_key_path")?,
                passphrase: row.get("passphrase")?,
            },
            tags: tags.map(|t| serde_json::from_str(&t).unwrap_or_default()),
        },
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

impl HostStore {
    pub fn list(&self) -> Vec<HostRecord> {
        self.db
            .with(|c| {
                let mut stmt = c.prepare("SELECT * FROM hosts ORDER BY created_at")?;
                let rows = stmt.query_map([], row_to_record)?;
                rows.collect()
            })
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<HostRecord> {
        self.db
            .with(|c| {
                c.query_row("SELECT * FROM hosts WHERE id = ?1", [id], row_to_record)
                    .optional()
            })
            .ok()
            .flatten()
    }

    pub fn create(&self, input: &HostInput) -> rusqlite::Result<HostRecord> {
        let now = now_iso();
        let rec = HostRecord {
            input: input.clone(),
            id: uuid::Uuid::new_v4().to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.db.with(|c| {
            c.execute(
                "INSERT INTO hosts (id, name, host, port, username, auth_kind, password, private_key_path, passphrase, tags, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                rusqlite::params![
                    rec.id,
                    rec.input.name,
                    rec.input.host,
                    rec.input.port as i64,
                    rec.input.username,
                    rec.input.auth.kind,
                    rec.input.auth.password,
                    rec.input.auth.private_key_path,
                    rec.input.auth.passphrase,
                    rec.input.tags.as_ref().map(|t| serde_json::to_string(t).unwrap_or_default()),
                    rec.created_at,
                    rec.updated_at,
                ],
            )
        })?;
        Ok(rec)
    }

    pub fn update(&self, id: &str, input: &HostInput) -> rusqlite::Result<Option<HostRecord>> {
        let n = self.db.with(|c| {
            c.execute(
                "UPDATE hosts SET name=?1, host=?2, port=?3, username=?4, auth_kind=?5, password=?6, private_key_path=?7, passphrase=?8, tags=?9, updated_at=?10 WHERE id=?11",
                rusqlite::params![
                    input.name,
                    input.host,
                    input.port as i64,
                    input.username,
                    input.auth.kind,
                    input.auth.password,
                    input.auth.private_key_path,
                    input.auth.passphrase,
                    input.tags.as_ref().map(|t| serde_json::to_string(t).unwrap_or_default()),
                    now_iso(),
                    id,
                ],
            )
        })?;
        if n == 0 {
            return Ok(None);
        }
        Ok(self.get(id))
    }

    pub fn delete(&self, id: &str) -> bool {
        self.db
            .with(|c| c.execute("DELETE FROM hosts WHERE id = ?1", [id]))
            .map(|n| n > 0)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KnownHostEntry {
    pub host: String,
    pub port: u16,
    #[serde(rename = "keyType")]
    pub key_type: String,
    pub fingerprint: String,
}

#[derive(Clone)]
pub struct KnownHosts {
    db: Db,
}

impl KnownHosts {
    pub fn find(&self, host: &str, port: u16) -> Option<KnownHostEntry> {
        self.db
            .with(|c| {
                c.query_row(
                    "SELECT host, port, key_type, fingerprint FROM known_hosts WHERE host = ?1 AND port = ?2",
                    rusqlite::params![host, port as i64],
                    |row| {
                        Ok(KnownHostEntry {
                            host: row.get(0)?,
                            port: row.get::<_, i64>(1)? as u16,
                            key_type: row.get(2)?,
                            fingerprint: row.get(3)?,
                        })
                    },
                )
                .optional()
            })
            .ok()
            .flatten()
    }

    pub fn save(&self, entry: &KnownHostEntry) {
        let _ = self.db.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO known_hosts (host, port, key_type, fingerprint) VALUES (?1,?2,?3,?4)",
                rusqlite::params![entry.host, entry.port as i64, entry.key_type, entry.fingerprint],
            )
        });
    }
}

#[derive(Clone)]
pub struct SettingsStore {
    db: Db,
}

impl SettingsStore {
    pub fn get(&self, key: &str) -> Option<serde_json::Value> {
        self.db
            .with(|c| {
                c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
            })
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str(&v).ok())
    }

    pub fn get_str(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| v.as_str().map(|s| s.to_string()))
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(|v| v.as_bool())
    }

    pub fn get_all(&self) -> serde_json::Map<String, serde_json::Value> {
        let rows: Vec<(String, String)> = self
            .db
            .with(|c| {
                let mut stmt = c.prepare("SELECT key, value FROM settings")?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect()
            })
            .unwrap_or_default();
        let mut out = serde_json::Map::new();
        for (k, v) in rows {
            if let Ok(val) = serde_json::from_str(&v) {
                out.insert(k, val);
            }
        }
        out
    }

    pub fn set(&self, key: &str, value: &serde_json::Value) {
        let _ = self.db.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, serde_json::to_string(value).unwrap_or_default()],
            )
        });
    }

    pub fn delete(&self, key: &str) -> bool {
        self.db
            .with(|c| c.execute("DELETE FROM settings WHERE key = ?1", [key]))
            .map(|n| n > 0)
            .unwrap_or(false)
    }
}

#[derive(Clone)]
pub struct AuditQueue {
    db: Db,
}

impl AuditQueue {
    /// 审计落库（失败不阻断主流程）
    pub fn append(&self, event_type: &str, meta: serde_json::Value) {
        let _ = self.db.with(|c| {
            c.execute(
                "INSERT INTO audit_log (ts, type, meta) VALUES (?1, ?2, ?3)",
                rusqlite::params![now_iso(), event_type, serde_json::to_string(&meta).unwrap_or_default()],
            )
        });
    }
}

/// 聚合句柄：克隆廉价，进程内共享同一 SQLite 连接
#[derive(Clone)]
pub struct Stores {
    pub hosts: HostStore,
    pub known: KnownHosts,
    pub settings: SettingsStore,
    pub audit: AuditQueue,
}

impl Stores {
    pub fn open(db: Db) -> Stores {
        Stores {
            hosts: HostStore { db: db.clone() },
            known: KnownHosts { db: db.clone() },
            settings: SettingsStore { db: db.clone() },
            audit: AuditQueue { db },
        }
    }
}
