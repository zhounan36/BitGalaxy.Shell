//! bg-sql：SQL 数据库引擎 crate（直连版）。驱动宿主进程（安装包内置）+
//! 会话式 op 分发（仿 sftp.rs 的 rid 模式）。
//!
//! 会话生命周期：connect → （驱动进程 + 驱动侧 connId）→ query/introspect/… → disconnect。
//! 连接方式：MySQL/PostgreSQL 由本进程直连目标库（用户名/密码，Navicat 习惯）；
//! SQLite 直接打开本地 .db 文件（只读，会话关闭不删用户文件）。
//! 历史：P1 曾有 SSH direct-tcpip 隧道与远端 SQLite 下载（fetch-sqlite），
//! P2 四 EXE 拆分时改为直连（tunnel.rs 已删除）。
pub mod driver;
pub mod driver_mgr;
pub mod error;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64 as StdAtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use bg_core::protocol::{
    SqlColumnM, SqlConnsResultM, SqlConnRecord, SqlDatabaseM, SqlDriverInfoM, SqlIndexM,
    SqlColumnInfoM, SqlOpM, SqlResultM, SqlTableM, ServerFrame,
};
use bg_core::stores::SqlConnStore;
use bg_core::wire::Outbound;

use driver::DriverHost;
use driver_mgr::DriverManager;
use error::classify_sql_error;

const CONNECT_DEADLINE: Duration = Duration::from_secs(15);
const INTROSPECT_DEADLINE: Duration = Duration::from_secs(10);
const QUERY_DEADLINE_DEFAULT: Duration = Duration::from_secs(30);
const QUERY_DEADLINE_MAX: Duration = Duration::from_secs(600);
/// 结果集序列化后单帧上限（超过则截断行并提示分页/加 WHERE）
const RESULT_FRAME_LIMIT: usize = 1024 * 1024;
const DEFAULT_MAX_ROWS: u32 = 500;
const HARD_MAX_ROWS: u32 = 2000;

/// 一条 SQL 连接会话（驱动进程 + 驱动侧 connId）
struct SqlSession {
    /// 归属的 WS 连接（进程级单例被多路共享，断开只清本连接的会话）
    ws: u64,
    driver: Arc<DriverHost>,
    conn_id: String,
}

pub struct SqlHandler {
    mgr: DriverManager,
    /// 连接档案（sql-connections.db 独立域库）
    conns: SqlConnStore,
    sessions: Arc<StdMutex<HashMap<String, HashMap<String, SqlSession>>>>,
    /// WS 连接代际计数器 + 活跃连接表：new_conn 插入 / close_all 移除；
    /// 迟到的 connect 完成时发现 ws 已不在表内即拒收（防孤儿注册）
    next_ws: StdAtomicU64,
    live_ws: Arc<StdMutex<HashSet<u64>>>,
    /// 已关闭 tab 的 sid（close 帧清理后，迟到完成的 connect 按 tombstone 拒收）
    dead_sids: Arc<StdMutex<HashSet<String>>>,
}

impl SqlHandler {
    pub fn new(data_dir: &std::path::Path) -> SqlHandler {
        SqlHandler {
            mgr: DriverManager::new(data_dir),
            conns: SqlConnStore::open(data_dir),
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            next_ws: StdAtomicU64::new(1),
            live_ws: Arc::new(StdMutex::new(HashSet::new())),
            dead_sids: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    /// 每条 WS 连接接入时领取的代际 id（server.rs 调用；断开时 close_all(ws)）
    pub fn new_conn(&self) -> u64 {
        let ws = self.next_ws.fetch_add(1, Ordering::SeqCst);
        self.live_ws.lock().unwrap().insert(ws);
        ws
    }

    /// 入口：按 op 分发到独立任务（不阻塞 WS 帧分发循环），结果回 sql-result 帧
    pub fn handle_op(&self, ws: u64, sid: &str, m: &SqlOpM, out: &Outbound) {
        let mgr_root = self.mgr.root.clone();
        let sessions = self.sessions.clone();
        let live_ws = self.live_ws.clone();
        let dead_sids = self.dead_sids.clone();
        let sid = sid.to_string();
        let m = m.clone();
        let out = out.clone();
        tokio::spawn(async move {
            let mgr = DriverManager::new(&mgr_root);
            let handler = SqlHandlerInner {
                ws,
                mgr,
                sessions,
                live_ws,
                dead_sids,
                out: out.clone(),
            };
            handler.dispatch(&sid, &m).await;
        });
    }

    /// sql-conns 帧：连接档案 CRUD（sql-connections.db）
    pub fn handle_conns(&self, m: &serde_json::Value, out: &Outbound) {
        let op = m.get("op").and_then(|v| v.as_str()).unwrap_or_default();
        let store = self.conns.clone();
        let m = m.clone();
        let out = out.clone();
        // SQLite 快操作（毫秒级），同步执行即可
        let reply = |ok: bool, conns: Option<Vec<SqlConnRecord>>, conn: Option<SqlConnRecord>, message: Option<String>| {
            out.send_frame(&ServerFrame::SqlConnsResult {
                m: SqlConnsResultM { op: op.to_string(), ok, conns, conn, message },
            });
        };
        match op {
            "list" => reply(true, Some(store.list()), None, None),
            "create" => {
                let Ok(rec) = serde_json::from_value::<SqlConnRecord>(m.get("conn").cloned().unwrap_or(serde_json::Value::Null)) else {
                    reply(false, None, None, Some("缺少连接参数".into()));
                    return;
                };
                let now = bg_core::stores::now_iso();
                let rec = SqlConnRecord {
                    id: uuid::Uuid::new_v4().to_string(),
                    created_at: now.clone(),
                    updated_at: now,
                    ..rec
                };
                match store.create(&rec) {
                    Ok(()) => reply(true, Some(store.list()), Some(rec), None),
                    Err(e) => reply(false, None, None, Some(e.to_string())),
                }
            }
            "update" => {
                let Ok(mut rec) = serde_json::from_value::<SqlConnRecord>(m.get("conn").cloned().unwrap_or(serde_json::Value::Null)) else {
                    reply(false, None, None, Some("缺少连接参数".into()));
                    return;
                };
                if rec.id.is_empty() {
                    reply(false, None, None, Some("缺少连接 id".into()));
                    return;
                }
                rec.updated_at = bg_core::stores::now_iso();
                match store.update(&rec) {
                    Ok(true) => reply(true, Some(store.list()), Some(rec), None),
                    Ok(false) => reply(false, None, None, Some("连接不存在".into())),
                    Err(e) => reply(false, None, None, Some(e.to_string())),
                }
            }
            "delete" => {
                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                if id.is_empty() {
                    reply(false, None, None, Some("缺少连接 id".into()));
                    return;
                }
                if store.delete(id) {
                    reply(true, Some(store.list()), None, None);
                } else {
                    reply(false, None, None, Some("连接不存在".into()));
                }
            }
            _ => reply(false, None, None, Some("未知操作".into())),
        }
    }

    /// WS 连接断开：只关闭**该连接**的 SQL 会话（单例被工作台/桌宠共享，
    /// 不能 drain 全部——桌宠重连不得杀掉工作台的连接）
    pub fn close_all(&self, ws: u64) {
        self.live_ws.lock().unwrap().remove(&ws);
        let mut map = self.sessions.lock().unwrap();
        // 两段式：retain 闭包借 &mut 取不出所有权；先收集键再逐个 remove
        let targets: Vec<(String, String)> = map
            .iter()
            .flat_map(|(sid, conns)| {
                conns
                    .iter()
                    .filter(|(_, s)| s.ws == ws)
                    .map(move |(cid, _)| (sid.clone(), cid.clone()))
            })
            .collect();
        let mut removed = Vec::new();
        for (sid, cid) in targets {
            if let Some(s) = map.get_mut(&sid).and_then(|c| c.remove(&cid)) {
                removed.push(s);
            }
        }
        map.retain(|_, conns| !conns.is_empty());
        drop(map);
        for s in removed {
            spawn_cleanup(s);
        }
        let _ = self.compact_dead_sids();
    }

    /// SQL tab 关闭（close 帧）：清理该会话的全部数据库连接
    pub fn close_session(&self, sid: &str) {
        let conns = self.sessions.lock().unwrap().remove(sid);
        if let Some(conns) = conns {
            for (_, s) in conns {
                spawn_cleanup(s);
            }
        }
        // tab 关闭后再发 connect（前端重开同 sid？sid 为 uuid 不会复用）——
        // 防进行中的 connect 完成后注册成孤儿
        self.dead_sids.lock().unwrap().insert(sid.to_string());
        let _ = self.compact_dead_sids();
    }

    /// dead_sids 上限保护：极端增长时整体清空（最坏漏防一个迟到 connect）
    fn compact_dead_sids(&self) -> bool {
        let mut set = self.dead_sids.lock().unwrap();
        if set.len() > 4096 {
            set.clear();
            true
        } else {
            false
        }
    }
}

/// 单次 op 处理的内部上下文（每次 dispatch 新建）
struct SqlHandlerInner {
    /// 本次 op 所在 WS 连接代际
    ws: u64,
    mgr: DriverManager,
    sessions: Arc<StdMutex<HashMap<String, HashMap<String, SqlSession>>>>,
    live_ws: Arc<StdMutex<HashSet<u64>>>,
    dead_sids: Arc<StdMutex<HashSet<String>>>,
    out: Outbound,
}

impl SqlHandlerInner {
    fn send(&self, sid: &str, m: SqlResultM) {
        self.out.send_frame(&ServerFrame::SqlResult { s: sid.to_string(), m });
    }

    fn fail(&self, sid: &str, op: &str, rid: Option<String>, msg: &str) {
        let kind = classify_sql_error(msg).unwrap_or_else(|| "unknown".to_string());
        let m = SqlResultM {
            op: op.to_string(),
            ok: false,
            rid,
            message: Some(msg.to_string()),
            error_kind: Some(kind),
            ..empty_result()
        };
        self.send(sid, m);
    }

    async fn dispatch(&self, sid: &str, m: &SqlOpM) {
        let op = m.op.as_str();
        let rid = m.rid.clone();
        match op {
            "drivers-list" => self.op_drivers_list(sid, rid).await,
            "connect" => self.op_connect(sid, m).await,
            "disconnect" => self.op_disconnect(sid, m).await,
            "query" | "query-more" => self.op_query(sid, m).await,
            "execute" => self.op_execute(sid, m).await,
            "cancel" => self.op_cancel(sid, m).await,
            "introspect" => self.op_introspect(sid, m).await,
            _ => {
                self.fail(sid, op, rid, &format!("未知 SQL 操作: {op}"));
            }
        }
    }

    async fn op_drivers_list(&self, sid: &str, rid: Option<String>) {
        let drivers: Vec<SqlDriverInfoM> = self
            .mgr
            .list()
            .into_iter()
            .map(|(name, version, installed, description)| SqlDriverInfoM {
                name,
                version,
                state: if installed { "installed" } else { "not-installed" }.into(),
                description,
            })
            .collect();
        self.send(sid, SqlResultM {
            op: "drivers-list".into(),
            ok: true,
            rid,
            drivers: Some(drivers),
            ..empty_result()
        });
    }

    async fn op_connect(&self, sid: &str, m: &SqlOpM) {
        let rid = m.rid.clone();
        let Some(spec) = m.conn.clone() else {
            self.fail(sid, "connect", rid, "缺少连接参数 conn");
            return;
        };
        let kind = match spec.kind.as_str() {
            "mysql" | "mariadb" => "mysql",
            "postgres" | "postgresql" => "postgres",
            "sqlite" => "sqlite",
            other => {
                self.fail(sid, "connect", rid, &format!("不支持的数据库类型: {other}"));
                return;
            }
        };

        // 1. 确保驱动可用（安装包内置，无下载）
        let exe = match self.mgr.ensure(kind) {
            Ok(p) => p,
            Err(e) => {
                self.fail(sid, "connect", rid, &e);
                return;
            }
        };

        let driver = match DriverHost::spawn(&exe, "0.1.0").await {
            Ok(d) => Arc::new(d),
            Err(e) => {
                self.fail(sid, "connect", rid, &e.to_string());
                return;
            }
        };

        // 2. 直连目标：MySQL/PG 直连远端库；SQLite 打开本地文件
        let (connect_host, connect_port, sqlite_path) = if kind == "sqlite" {
            let Some(local) = spec.sqlite_path.clone().or(spec.database.clone()) else {
                self.fail(sid, "connect", rid, "sqlite 连接缺少 sqlitePath（本地 .db 文件路径）");
                return;
            };
            if !std::path::Path::new(&local).is_file() {
                self.fail(sid, "connect", rid, &format!("本地数据库文件不存在: {local}"));
                return;
            }
            (String::new(), 0u16, Some(local))
        } else {
            (
                spec.host.clone().unwrap_or_default(),
                spec.port.unwrap_or(if kind == "mysql" { 3306 } else { 5432 }),
                None,
            )
        };

        // 3. 驱动 connect（密码只在此处传入，走 stdin JSON，不进命令行/日志）
        let mut params = serde_json::json!({
            "host": connect_host,
            "port": connect_port,
            "user": spec.user.clone().unwrap_or_default(),
            "password": spec.password.clone().unwrap_or_default(),
        });
        if let Some(db) = &spec.database {
            params["database"] = serde_json::Value::String(db.clone());
        }
        if let Some(p) = &sqlite_path {
            params["sqlitePath"] = serde_json::Value::String(p.clone());
        }

        match driver.call("connect", params, CONNECT_DEADLINE).await {
            Ok(result) => {
                let conn_id = result
                    .get("connId")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if conn_id.is_empty() {
                    self.fail(sid, "connect", rid, "驱动未返回 connId");
                    driver.shutdown().await;
                    return;
                }
                let server_version =
                    result.get("serverVersion").and_then(|x| x.as_str()).map(|s| s.to_string());
                if !self.register_session(sid, SqlSession {
                    ws: self.ws,
                    driver,
                    conn_id: conn_id.clone(),
                }) {
                    // WS/tab 已在连接过程中关闭：会话已被就地清理
                    return;
                }
                self.send(sid, SqlResultM {
                    op: "connect".into(),
                    ok: true,
                    rid,
                    conn_id: Some(conn_id),
                    server_version,
                    ..empty_result()
                });
            }
            Err(e) => {
                self.fail(sid, "connect", rid, &e.to_string());
                driver.shutdown().await;
            }
        }
    }

    /// 注册会话；返回 false = 请求所属的 WS/tab 已在连接期间关闭（迟到完成），
    /// 会话就地清理，不注册（否则变成无人认领的孤儿）
    fn register_session(&self, sid: &str, s: SqlSession) -> bool {
        if !self.live_ws.lock().unwrap().contains(&self.ws)
            || self.dead_sids.lock().unwrap().contains(sid)
        {
            spawn_cleanup(s);
            return false;
        }
        let mut map = self.sessions.lock().unwrap();
        let conns = map.entry(sid.to_string()).or_default();
        // connId 由驱动生成，通常唯一；极端撞车时显式清理旧会话防泄漏
        if let Some(old) = conns.insert(s.conn_id.clone(), s) {
            drop(map);
            spawn_cleanup(old);
        }
        true
    }

    /// 取会话（未连接返回 None）
    fn session(&self, sid: &str, conn_id: &str) -> Option<Arc<DriverHost>> {
        let map = self.sessions.lock().unwrap();
        map.get(sid)
            .and_then(|c| c.get(conn_id))
            .map(|s| s.driver.clone())
    }

    fn query_deadline(&self, m: &SqlOpM) -> Duration {
        m.timeout_ms
            .map(|ms| Duration::from_millis(ms.min(QUERY_DEADLINE_MAX.as_millis() as u64)))
            .unwrap_or(QUERY_DEADLINE_DEFAULT)
    }

    async fn op_query(&self, sid: &str, m: &SqlOpM) {
        let rid = m.rid.clone();
        let op = m.op.clone();
        let Some(conn_id) = m.conn_id_ref() else {
            self.fail(sid, &op, rid, "缺少 connId");
            return;
        };
        let Some(driver) = self.session(sid, &conn_id) else {
            self.fail(sid, &op, rid, "连接不存在或已断开");
            return;
        };
        let max_rows = m.max_rows.unwrap_or(DEFAULT_MAX_ROWS).min(HARD_MAX_ROWS);
        let mut params = serde_json::json!({
            "connId": conn_id,
            "maxRows": max_rows,
            "timeoutMs": self.query_deadline(m).as_millis() as u64,
        });
        if let Some(sql) = &m.sql {
            params["sql"] = serde_json::Value::String(sql.clone());
        }
        if let Some(qid) = &m.query_id {
            params["queryId"] = serde_json::Value::String(qid.clone());
        }
        let method = if op == "query-more" { "queryMore" } else { "query" };
        let deadline = self.query_deadline(m) + Duration::from_secs(5);
        match driver.call(method, params, deadline).await {
            Ok(result) => {
                let mut out = empty_result();
                out.op = op;
                out.ok = true;
                out.rid = rid;
                out.query_id = result.get("queryId").and_then(|x| x.as_str()).map(|s| s.to_string());
                out.duration_ms = result.get("durationMs").and_then(|x| x.as_u64());
                out.truncated = result.get("truncated").and_then(|x| x.as_bool());
                out.row_count = result.get("rowCount").and_then(|x| x.as_u64());
                out.columns = result
                    .get("columns")
                    .and_then(|x| x.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|c| SqlColumnM {
                                name: c.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                                data_type: c.get("dataType").and_then(|x| x.as_str()).map(|s| s.to_string()),
                                nullable: c.get("nullable").and_then(|x| x.as_bool()),
                            })
                            .collect()
                    });
                let mut rows: Vec<Vec<serde_json::Value>> = result
                    .get("rows")
                    .and_then(|x| x.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|r| r.as_array().map(|r| r.clone()).unwrap_or_default())
                            .collect()
                    })
                    .unwrap_or_default();
                // 结果集体积防护：超限截断行数（保列头），提示分页
                let (rows2, truncated2) = clamp_rows(rows, RESULT_FRAME_LIMIT);
                rows = rows2;
                out.rows = Some(rows);
                if truncated2 {
                    out.truncated = Some(true);
                    out.message = Some("结果集过大已截断，请加 WHERE/LIMIT 分页查看".into());
                }
                self.send(sid, out);
            }
            Err(e) => {
                self.fail(sid, &op, rid, &e.to_string());
            }
        }
    }

    async fn op_execute(&self, sid: &str, m: &SqlOpM) {
        let rid = m.rid.clone();
        let Some(conn_id) = m.conn_id_ref() else {
            self.fail(sid, "execute", rid, "缺少 connId");
            return;
        };
        let Some(driver) = self.session(sid, &conn_id) else {
            self.fail(sid, "execute", rid, "连接不存在或已断开");
            return;
        };
        let Some(sql) = m.sql.clone() else {
            self.fail(sid, "execute", rid, "缺少 SQL");
            return;
        };
        let params = serde_json::json!({
            "connId": conn_id,
            "sql": sql,
            "timeoutMs": self.query_deadline(m).as_millis() as u64,
        });
        let deadline = self.query_deadline(m) + Duration::from_secs(5);
        match driver.call("execute", params, deadline).await {
            Ok(result) => {
                self.send(sid, SqlResultM {
                    op: "execute".into(),
                    ok: true,
                    rid,
                    affected: result.get("affected").and_then(|x| x.as_u64()),
                    duration_ms: result.get("durationMs").and_then(|x| x.as_u64()),
                    ..empty_result()
                });
            }
            Err(e) => self.fail(sid, "execute", rid, &e.to_string()),
        }
    }

    async fn op_cancel(&self, sid: &str, m: &SqlOpM) {
        let rid = m.rid.clone();
        let Some(query_id) = m.query_id.clone() else {
            self.fail(sid, "cancel", rid, "缺少 queryId");
            return;
        };
        // cancel 需定位会话：任一持有该 queryId 的会话（驱动侧按 queryId 取消）
        let found = {
            let map = self.sessions.lock().unwrap();
            map.get(sid)
                .and_then(|c| c.values().next())
                .map(|s| s.driver.clone())
        };
        let Some(driver) = found else {
            self.fail(sid, "cancel", rid, "无活动连接");
            return;
        };
        match driver.call("cancel", serde_json::json!({ "queryId": query_id }), Duration::from_secs(3)).await {
            Ok(_) => self.send(sid, SqlResultM {
                op: "cancel".into(),
                ok: true,
                rid,
                ..empty_result()
            }),
            Err(e) => self.fail(sid, "cancel", rid, &e.to_string()),
        }
    }

    async fn op_introspect(&self, sid: &str, m: &SqlOpM) {
        let rid = m.rid.clone();
        let Some(conn_id) = m.conn_id_ref() else {
            self.fail(sid, "introspect", rid, "缺少 connId");
            return;
        };
        let Some(driver) = self.session(sid, &conn_id) else {
            self.fail(sid, "introspect", rid, "连接不存在或已断开");
            return;
        };
        let scope = m.scope.clone().unwrap_or_else(|| "databases".into());
        let mut params = serde_json::json!({ "connId": conn_id, "scope": scope });
        if let Some(s) = &m.schema {
            params["schema"] = serde_json::Value::String(s.clone());
        }
        if let Some(t) = &m.table {
            params["table"] = serde_json::Value::String(t.clone());
        }
        match driver.call("introspect", params, INTROSPECT_DEADLINE).await {
            Ok(result) => {
                let mut out = empty_result();
                out.op = "introspect".into();
                out.ok = true;
                out.rid = rid;
                if let Some(arr) = result.get("databases").and_then(|x| x.as_array()) {
                    out.databases = Some(
                        arr.iter()
                            .map(|d| SqlDatabaseM {
                                name: d.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                            })
                            .collect(),
                    );
                }
                if let Some(arr) = result.get("tables").and_then(|x| x.as_array()) {
                    out.tables = Some(
                        arr.iter()
                            .map(|t| SqlTableM {
                                name: t.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                                kind: t.get("kind").and_then(|x| x.as_str()).map(|s| s.to_string()),
                                rows_estimate: t.get("rowsEstimate").and_then(|x| x.as_i64()),
                            })
                            .collect(),
                    );
                }
                if let Some(arr) = result.get("columns").and_then(|x| x.as_array()) {
                    out.fields = Some(
                        arr.iter()
                            .map(|c| SqlColumnInfoM {
                                name: c.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                                data_type: c.get("dataType").and_then(|x| x.as_str()).map(|s| s.to_string()),
                                pk: c.get("pk").and_then(|x| x.as_bool()).unwrap_or(false),
                                nullable: c.get("nullable").and_then(|x| x.as_bool()).unwrap_or(true),
                                default: c.get("default").and_then(|x| x.as_str()).map(|s| s.to_string()),
                            })
                            .collect(),
                    );
                }
                if let Some(arr) = result.get("indexes").and_then(|x| x.as_array()) {
                    out.indexes = Some(
                        arr.iter()
                            .map(|i| SqlIndexM {
                                name: i.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                                unique: i.get("unique").and_then(|x| x.as_bool()).unwrap_or(false),
                                columns: i
                                    .get("columns")
                                    .and_then(|x| x.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .map(|c| c.as_str().unwrap_or("").to_string())
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            })
                            .collect(),
                    );
                }
                self.send(sid, out);
            }
            Err(e) => self.fail(sid, "introspect", rid, &e.to_string()),
        }
    }

    async fn op_disconnect(&self, sid: &str, m: &SqlOpM) {
        let rid = m.rid.clone();
        let Some(conn_id) = m.conn_id_ref() else {
            self.fail(sid, "disconnect", rid, "缺少 connId");
            return;
        };
        let session = {
            let mut map = self.sessions.lock().unwrap();
            map.get_mut(sid).and_then(|c| c.remove(&conn_id))
        };
        if let Some(s) = session {
            let _ = s.driver.call("disconnect", serde_json::json!({ "connId": conn_id }), Duration::from_secs(2)).await;
            s.driver.shutdown().await;
        }
        self.send(sid, SqlResultM {
            op: "disconnect".into(),
            ok: true,
            rid,
            ..empty_result()
        });
    }
}

/// 异步清理一个会话：disconnect + 关驱动进程（不阻塞调用方）
fn spawn_cleanup(s: SqlSession) {
    tokio::spawn(async move {
        let _ = s
            .driver
            .call("disconnect", serde_json::json!({ "connId": s.conn_id }), Duration::from_secs(2))
            .await;
        s.driver.shutdown().await;
    });
}

fn empty_result() -> SqlResultM {
    SqlResultM {
        op: String::new(),
        ok: false,
        rid: None,
        message: None,
        error_kind: None,
        conn_id: None,
        server_version: None,
        query_id: None,
        columns: None,
        rows: None,
        row_count: None,
        truncated: None,
        affected: None,
        duration_ms: None,
        databases: None,
        tables: None,
        fields: None,
        indexes: None,
        drivers: None,
        sqlite_ref: None,
    }
}

/// 结果集体积防护：序列化超限则从尾部截行，直到低于 limit
fn clamp_rows(
    rows: Vec<Vec<serde_json::Value>>,
    limit: usize,
) -> (Vec<Vec<serde_json::Value>>, bool) {
    if rows.is_empty() {
        return (rows, false);
    }
    let size = serde_json::to_string(&rows).map(|s| s.len()).unwrap_or(0);
    if size <= limit {
        return (rows, false);
    }
    // 二分估算可保留行数（按平均行大小）
    let avg = (size / rows.len().max(1)).max(1);
    let keep = (limit / avg).max(1).min(rows.len());
    let mut out = rows;
    out.truncate(keep);
    (out, true)
}

/// SqlOpM 取 connId 的便捷方法
trait SqlOpConnId {
    fn conn_id_ref(&self) -> Option<String>;
}

impl SqlOpConnId for SqlOpM {
    fn conn_id_ref(&self) -> Option<String> {
        self.conn_id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_rows_truncates_oversized() {
        let rows: Vec<Vec<serde_json::Value>> = (0..1000)
            .map(|i| vec![serde_json::Value::String("x".repeat(200)), serde_json::json!(i)])
            .collect();
        let (out, truncated) = clamp_rows(rows, 10_000);
        assert!(truncated);
        assert!(out.len() < 1000);
    }

    #[test]
    fn clamp_rows_keeps_small() {
        let rows = vec![vec![serde_json::json!(1), serde_json::json!("a")]];
        let (out, truncated) = clamp_rows(rows, 10_000);
        assert!(!truncated);
        assert_eq!(out.len(), 1);
    }
}
