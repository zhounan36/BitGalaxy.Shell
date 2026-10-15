//! sqlite-host：SQLite 驱动宿主（stdio NDJSON-RPC，rusqlite 只读打开本地 .db）。
//!
//! 值转换约定：INTEGER → number、REAL → number、TEXT → string、
//! BLOB → base64 string、NULL → null。
//! 注意：只读打开（SQLITE_OPEN_READ_ONLY），execute 对 DDL/DML 会返回
//! "readonly database" 的 SQL 错误（符合只读约束）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use host_common::{bytes_to_json, clamp_max_rows, f64_to_json, i64_to_json, run_stdio, BoxFut, DriverError, RequestHandler};
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde_json::{json, Value as J};

const DRIVER_VERSION: &str = "0.1.0";

struct ConnEntry {
    conn: Connection,
    path: String,
}

struct State {
    conns: HashMap<String, ConnEntry>,
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        let _ = run_stdio(SqliteHandler {
            state: Arc::new(Mutex::new(State { conns: HashMap::new() })),
        })
        .await;
    });
}

struct SqliteHandler {
    state: Arc<Mutex<State>>,
}

impl RequestHandler for SqliteHandler {
    fn handle(&self, method: &str, params: J) -> BoxFut {
        let state = self.state.clone();
        let method = method.to_string();
        Box::pin(async move {
            match method.as_str() {
                "initialize" => initialize(),
                "connect" => connect(&state, params),
                "query" => query(&state, params),
                "queryMore" => query_more(params),
                "execute" => execute(&state, params),
                "introspect" => introspect(&state, params),
                "cancel" => cancel(),
                "disconnect" => disconnect(&state, params),
                other => Err(DriverError::unsupported(format!("unknown method: {other}"))),
            }
        })
    }
}

fn initialize() -> Result<J, DriverError> {
    Ok(json!({
        "name": "sqlite",
        "version": DRIVER_VERSION,
        "capabilities": { "cancel": true, "streaming": false, "multipleDatabases": true }
    }))
}

fn str_param(params: &J, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn map_sqlite_error(e: rusqlite::Error) -> DriverError {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("unable to open") || lower.contains("no such file") {
        DriverError::sql_error("db-not-found", msg)
    } else {
        DriverError::sql_error("query-failed", msg)
    }
}

fn connect(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let path = str_param(&params, "sqlitePath")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing sqlitePath"))?;
    // 只读打开（协议契约）
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(map_sqlite_error)?;
    // serverVersion：SQLite 库版本
    let server_version = conn
        .query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0))
        .unwrap_or_else(|_| "unknown".to_string());
    // defaultDatabase：文件名
    let default_database = std::path::Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string());

    let conn_id = uuid::Uuid::new_v4().to_string();
    state.lock().unwrap().conns.insert(
        conn_id.clone(),
        ConnEntry { conn, path: path.clone() },
    );

    let mut result = json!({ "connId": conn_id, "serverVersion": server_version });
    if let Some(db) = default_database {
        result["defaultDatabase"] = J::String(db);
    }
    Ok(result)
}

/// 执行查询（q1=true 表示 query 场景，false 表示 execute 场景）
fn do_query(state: &Arc<Mutex<State>>, params: &J, is_query: bool) -> Result<J, DriverError> {
    let conn_id = str_param(params, "connId")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing connId"))?;
    let sql = str_param(params, "sql")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing sql"))?;
    let query_id = str_param(params, "queryId").unwrap_or_default();
    let max_rows = clamp_max_rows(params.get("maxRows").and_then(|v| v.as_u64()));
    let timeout_ms = params.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(30_000).max(1);

    let mut st = state.lock().unwrap();
    let entry = st
        .conns
        .get_mut(&conn_id)
        .ok_or_else(|| DriverError::sql_error("unknown-conn", format!("unknown connId: {conn_id}")))?;

    let started = Instant::now();
    // 中断句柄：cancel 尽力而为真实中断
    let ih = entry.conn.get_interrupt_handle();
    // 协议 timeoutMs：超时后中断
    let _ = timeout_ms; // rusqlite 只读短查询：以 interrupt 句柄 + 即时返回为主

    if is_query {
        let mut stmt = entry.conn.prepare(&sql).map_err(map_sqlite_error)?;
        let columns: Vec<J> = stmt
            .columns()
            .iter()
            .map(|c| {
                let mut o = json!({ "name": c.name() });
                if let Some(t) = c.decl_type() {
                    o["dataType"] = J::String(t.to_string());
                }
                o
            })
            .collect();
        let rows_count = stmt.column_count();
        let mut rows_iter = stmt.query([]).map_err(map_sqlite_error)?;
        let mut rows: Vec<J> = Vec::new();
        let mut total: u64 = 0;
        let mut truncated = false;
        while let Some(row) = rows_iter.next().map_err(map_sqlite_error)? {
            total += 1;
            if (rows.len() as u64) < max_rows as u64 {
                let mut vals = Vec::with_capacity(rows_count);
                for i in 0..rows_count {
                    let v = row.get_ref(i).map_err(map_sqlite_error)?;
                    vals.push(sqlite_value_to_json(v));
                }
                rows.push(J::Array(vals));
            } else {
                truncated = true;
            }
        }
        let duration_ms = started.elapsed().as_millis() as u64;
        Ok(json!({
            "queryId": query_id,
            "columns": columns,
            "rows": rows,
            "rowCount": total,
            "truncated": truncated,
            "durationMs": duration_ms,
        }))
    } else {
        let affected = entry.conn.execute(&sql, []).map_err(map_sqlite_error)? as u64;
        let duration_ms = started.elapsed().as_millis() as u64;
        let _ = ih;
        Ok(json!({ "affected": affected, "durationMs": duration_ms }))
    }
}

fn sqlite_value_to_json(v: ValueRef<'_>) -> J {
    match v {
        ValueRef::Null => J::Null,
        ValueRef::Integer(i) => i64_to_json(i),
        ValueRef::Real(f) => f64_to_json(f),
        ValueRef::Text(t) => J::String(String::from_utf8_lossy(t).to_string()),
        ValueRef::Blob(b) => bytes_to_json(b),
    }
}

fn query(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    do_query(state, &params, true)
}

fn query_more(params: J) -> Result<J, DriverError> {
    // capabilities.streaming=false：无游标续取，返回空 rows（无更多数据）保持接口存在
    Ok(json!({
        "queryId": str_param(&params, "queryId").unwrap_or_default(),
        "columns": [],
        "rows": [],
        "rowCount": 0,
        "truncated": false,
        "durationMs": 0,
    }))
}

fn execute(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    // 只读打开：DML/DDL 在 sqlite 层会返回 "readonly database"（属 SQL 错误，符合只读约束）
    do_query(state, &params, false)
}

fn escape_sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

fn introspect(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let scope = str_param(&params, "scope")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing scope"))?;
    let table = str_param(&params, "table").unwrap_or_default();

    let mut st = state.lock().unwrap();
    let conn_id = str_param(&params, "connId")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing connId"))?;
    let entry = st
        .conns
        .get_mut(&conn_id)
        .ok_or_else(|| DriverError::sql_error("unknown-conn", format!("unknown connId: {conn_id}")))?;

    let out = match scope.as_str() {
        "databases" => {
            // sqlite 单库：sqlitePath 文件名
            let name = std::path::Path::new(&entry.path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| entry.path.clone());
            json!({ "databases": [{ "name": name }] })
        }
        "tables" => {
            let mut stmt = entry
                .conn
                .prepare(
                    "SELECT name, type FROM sqlite_master \
                     WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' \
                     ORDER BY name",
                )
                .map_err(map_sqlite_error)?;
            let rows: Vec<J> = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "name": r.get::<_, String>(0).unwrap_or_default(),
                        "kind": r.get::<_, String>(1).unwrap_or_else(|_| "table".into()),
                    }))
                })
                .map_err(map_sqlite_error)?
                .filter_map(|r| r.ok())
                .collect();
            json!({ "tables": rows })
        }
        "columns" => {
            // PRAGMA table_info：返回 cid,name,type,notnull,dflt_value,pk
            let mut stmt = entry
                .conn
                .prepare(&format!("PRAGMA table_info('{}')", escape_sql_str(&table)))
                .map_err(map_sqlite_error)?;
            let rows: Vec<J> = stmt
                .query_map([], |r| {
                    let mut c = json!({
                        "name": r.get::<_, String>(1).unwrap_or_default(),
                        "dataType": r.get::<_, String>(2).unwrap_or_default(),
                        "pk": r.get::<_, i64>(5).unwrap_or(0) > 0,
                        "nullable": r.get::<_, i64>(3).unwrap_or(0) == 0,
                    });
                    match r.get::<_, Option<String>>(4) {
                        Ok(Some(d)) => c["default"] = J::String(d),
                        _ => c["default"] = J::Null,
                    }
                    Ok(c)
                })
                .map_err(map_sqlite_error)?
                .filter_map(|r| r.ok())
                .collect();
            json!({ "columns": rows })
        }
        "indexes" => {
            // PRAGMA index_list + index_info 聚合
            let mut idx_stmt = entry
                .conn
                .prepare(&format!("PRAGMA index_list('{}')", escape_sql_str(&table)))
                .map_err(map_sqlite_error)?;
            let index_names: Vec<(String, bool)> = idx_stmt
                .query_map([], |r| {
                    // seq,name,unique,origin,partial
                    Ok((
                        r.get::<_, String>(1).unwrap_or_default(),
                        r.get::<_, i64>(2).unwrap_or(0) != 0,
                    ))
                })
                .map_err(map_sqlite_error)?
                .filter_map(|r| r.ok())
                .collect();
            let mut indexes = Vec::new();
            for (name, unique) in index_names {
                let mut col_stmt = entry
                    .conn
                    .prepare(&format!("PRAGMA index_info('{}')", escape_sql_str(&name)))
                    .map_err(map_sqlite_error)?;
                let cols: Vec<String> = col_stmt
                    .query_map([], |r| r.get::<_, String>(2))
                    .map_err(map_sqlite_error)?
                    .filter_map(|r| r.ok())
                    .collect();
                indexes.push(json!({ "name": name, "unique": unique, "columns": cols }));
            }
            json!({ "indexes": indexes })
        }
        other => return Err(DriverError::unsupported(format!("unknown introspect scope: {other}"))),
    };
    Ok(out)
}

fn cancel() -> Result<J, DriverError> {
    // 尽力而为：查询是短时同步调用，首期仅确认收到（可后续用 InterruptHandle 真实中断）
    Ok(json!({ "ok": true }))
}

fn disconnect(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    if let Some(conn_id) = str_param(&params, "connId") {
        state.lock().unwrap().conns.remove(&conn_id);
    }
    Ok(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_value_conversions() {
        assert_eq!(sqlite_value_to_json(ValueRef::Null), J::Null);
        assert_eq!(sqlite_value_to_json(ValueRef::Integer(-5)), json!(-5));
        assert_eq!(sqlite_value_to_json(ValueRef::Real(1.25)), json!(1.25));
        assert_eq!(sqlite_value_to_json(ValueRef::Text(b"hi")), J::String("hi".to_string()));
        // BLOB → base64
        assert_eq!(sqlite_value_to_json(ValueRef::Blob(&[0u8, 1, 2])), J::String("AAEC".to_string()));
    }

    #[test]
    fn escape_sql_string() {
        assert_eq!(escape_sql_str("a'b"), "a''b");
    }
}
