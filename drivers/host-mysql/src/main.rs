//! mysql-host：MySQL 驱动宿主（stdio NDJSON-RPC，mysql_async 纯 Rust）。
//!
//! 值转换约定：数值 → JSON number（超 JSON 安全范围转 string）、字符串 → string、
//! 二进制(BINARY/BLOB) → base64 string、时间/日期 → 字符串、DECIMAL → string、
//! JSON → 嵌套 JSON 值。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use host_common::{
    bytes_to_json, clamp_max_rows, f64_to_json, i64_to_json, run_stdio, u64_to_json, BoxFut,
    DriverError, RequestHandler, MAX_ROWS_LIMIT,
};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, Row, Value};
use serde_json::{json, Value as J};

const DRIVER_VERSION: &str = "0.1.0";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

struct State {
    conns: HashMap<String, Conn>,
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        let _ = run_stdio(MySqlHandler {
            state: Arc::new(Mutex::new(State { conns: HashMap::new() })),
        })
        .await;
    });
}

struct MySqlHandler {
    state: Arc<Mutex<State>>,
}

impl RequestHandler for MySqlHandler {
    fn handle(&self, method: &str, params: J) -> BoxFut {
        let state = self.state.clone();
        let method = method.to_string();
        Box::pin(async move {
            match method.as_str() {
                "initialize" => initialize(),
                "connect" => connect(&state, params).await,
                "query" => query(&state, params).await,
                "queryMore" => query_more(&state, params).await,
                "execute" => execute(&state, params).await,
                "introspect" => introspect(&state, params).await,
                "cancel" => cancel(),
                "disconnect" => disconnect(&state, params),
                other => Err(DriverError::unsupported(format!("unknown method: {other}"))),
            }
        })
    }
}

fn initialize() -> Result<J, DriverError> {
    Ok(json!({
        "name": "mysql",
        "version": DRIVER_VERSION,
        "capabilities": { "cancel": true, "streaming": false, "multipleDatabases": true }
    }))
}

fn str_param(params: &J, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

async fn connect(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let host = str_param(&params, "host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = params.get("port").and_then(|v| v.as_u64()).unwrap_or(3306) as u16;
    let user = str_param(&params, "user");
    let password = str_param(&params, "password");
    let database = str_param(&params, "database");

    let opts: Opts = OptsBuilder::default()
        .ip_or_hostname(host)
        .tcp_port(port)
        .user(user)
        .pass(password)
        .db_name(database)
        // SSH 隧道本地端口：禁用 unix socket 优先，强制 TCP
        .prefer_socket(false)
        .into();

    let conn = tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(opts))
        .await
        .map_err(|_| DriverError::network("connect-timeout", "connection timeout", true))?
        .map_err(map_mysql_error)?;

    let (major, minor, patch) = conn.server_version();
    let server_version = format!("{major}.{minor}.{patch}");
    let default_database = conn.opts().db_name().map(|s| s.to_string());

    let conn_id = uuid::Uuid::new_v4().to_string();
    state.lock().unwrap().conns.insert(conn_id.clone(), conn);

    let mut result = json!({ "connId": conn_id, "serverVersion": server_version });
    if let Some(db) = default_database {
        result["defaultDatabase"] = J::String(db);
    }
    Ok(result)
}

fn take_conn(state: &Arc<Mutex<State>>, params: &J) -> Result<Conn, DriverError> {
    let conn_id = str_param(params, "connId")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing connId"))?;
    let mut st = state.lock().unwrap();
    st.conns
        .remove(&conn_id)
        .ok_or_else(|| DriverError::sql_error("unknown-conn", format!("unknown connId: {conn_id}")))
}

fn put_conn(state: &Arc<Mutex<State>>, conn_id: &str, conn: Conn) {
    state.lock().unwrap().conns.insert(conn_id.to_string(), conn);
}

fn column_to_meta(c: &mysql_async::Column) -> J {
    json!({
        "name": c.name_str(),
        "dataType": format!("{:?}", c.column_type()),
        "nullable": !c.flags().contains(ColumnFlags::NOT_NULL_FLAG),
    })
}

/// MySQL Value → JSON（见文件头转换约定）
fn mysql_value_to_json(v: &Value, ty: ColumnType) -> J {
    match v {
        Value::NULL => J::Null,
        Value::Int(i) => i64_to_json(*i),
        Value::UInt(u) => u64_to_json(*u),
        Value::Float(f) => f64_to_json(*f as f64),
        Value::Double(d) => f64_to_json(*d),
        Value::Bytes(b) => match ty {
            ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
                String::from_utf8_lossy(b).into()
            }
            _ => {
                if let Ok(s) = std::str::from_utf8(b) {
                    // 时间/日期/JSON 文本形态
                    if let Ok(jv) = serde_json::from_str::<J>(s) {
                        if ty == ColumnType::MYSQL_TYPE_JSON {
                            return jv;
                        }
                    }
                    s.into()
                } else {
                    bytes_to_json(b)
                }
            }
        },
        Value::Date(y, mo, d, h, mi, s, us) => {
            if *h == 0 && *mi == 0 && *s == 0 && *us == 0 {
                format!("{y:04}-{mo:02}-{d:02}").into()
            } else if *us == 0 {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}").into()
            } else {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}").into()
            }
        }
        Value::Time(neg, days, h, mi, s, us) => {
            let sign = if *neg { "-" } else { "" };
            let hours = days * 24 + *h as u32;
            if *us == 0 {
                format!("{sign}{hours:02}:{mi:02}:{s:02}").into()
            } else {
                format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}").into()
            }
        }
    }
}

async fn query(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let conn_id = str_param(&params, "connId")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing connId"))?;
    let sql = str_param(&params, "sql")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing sql"))?;
    let query_id = str_param(&params, "queryId").unwrap_or_default();
    let max_rows = clamp_max_rows(params.get("maxRows").and_then(|v| v.as_u64()));
    let timeout_ms = params.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(30_000).max(1);

    let mut conn = take_conn(state, &params)?;
    let started = Instant::now();
    let deadline = Duration::from_millis(timeout_ms);

    let result = tokio::time::timeout(deadline, async {
        let mut result_set = conn.query_iter(&sql).await?;
        let columns: Vec<J> = result_set.columns_ref().iter().map(column_to_meta).collect();
        let col_types: Vec<ColumnType> = result_set.columns_ref().iter().map(|c| c.column_type()).collect();
        let mut rows: Vec<J> = Vec::new();
        let mut total: u64 = 0;
        let mut truncated = false;
        while let Some(row) = result_set.next().await? {
            total += 1;
            if (rows.len() as u32) < max_rows {
                rows.push(mysql_row_to_json(&row, &col_types));
            } else {
                truncated = true;
            }
            if total >= MAX_ROWS_LIMIT as u64 && rows.len() as u32 >= max_rows {
                truncated = true;
                // 排空剩余结果集，保持连接可用
                while result_set.next().await?.is_some() {}
                break;
            }
        }
        Ok::<_, mysql_async::Error>((columns, rows, total, truncated))
    })
    .await;

    let duration_ms = started.elapsed().as_millis() as u64;
    put_conn(state, &conn_id, conn);

    let (columns, rows, total, truncated) = match result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(map_mysql_error(e)),
        Err(_) => {
            return Err(DriverError::network(
                "query-timeout",
                format!("query timeout after {}ms", timeout_ms),
                true,
            ));
        }
    };

    Ok(json!({
        "queryId": query_id,
        "columns": columns,
        "rows": rows,
        "rowCount": total,
        "truncated": truncated,
        "durationMs": duration_ms,
    }))
}

fn mysql_row_to_json(row: &Row, col_types: &[ColumnType]) -> J {
    let mut values = Vec::with_capacity(row.len());
    for i in 0..row.len() {
        let ty = col_types.get(i).copied().unwrap_or(ColumnType::MYSQL_TYPE_NULL);
        let v = row.as_ref(i).map(|v| mysql_value_to_json(v, ty)).unwrap_or(J::Null);
        values.push(v);
    }
    J::Array(values)
}

async fn query_more(_state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
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

async fn execute(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let conn_id = str_param(&params, "connId")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing connId"))?;
    let sql = str_param(&params, "sql")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing sql"))?;
    let timeout_ms = params.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(30_000).max(1);

    let mut conn = take_conn(state, &params)?;
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let rs = conn.query_iter(&sql).await?;
        // DDL/DML 无结果集；若有则丢弃
        rs.drop_result().await?;
        let affected = conn.affected_rows();
        Ok::<_, mysql_async::Error>(affected)
    })
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;
    put_conn(state, &conn_id, conn);

    let affected = match result {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return Err(map_mysql_error(e)),
        Err(_) => {
            return Err(DriverError::network(
                "query-timeout",
                format!("execute timeout after {}ms", timeout_ms),
                true,
            ))
        }
    };
    Ok(json!({ "affected": affected, "durationMs": duration_ms }))
}

async fn introspect(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let scope = str_param(&params, "scope")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing scope"))?;
    let schema = str_param(&params, "schema");
    let table = str_param(&params, "table");
    let conn_id = str_param(&params, "connId")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing connId"))?;

    let mut conn = take_conn(state, &params)?;
    // 先算后归还：任何错误都不得丢掉连接（对齐 query/execute 的 put_conn-先行模式）
    let out = async {
        Ok::<J, DriverError>(match scope.as_str() {
        "databases" => {
            let rows: Vec<mysql_async::Row> = conn
                .query("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME")
                .await
                .map_err(map_mysql_error)?;
            let dbs: Vec<J> = rows
                .iter()
                .map(|r| json!({ "name": row_str(r, 0) }))
                .collect();
            json!({ "databases": dbs })
        }
        "tables" => {
            let schema_v = schema.unwrap_or_default();
            let rows: Vec<mysql_async::Row> = conn.query_iter(format!(
                "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{}' ORDER BY TABLE_NAME",
                escape_sql_str(&schema_v)
            )).await.map_err(map_mysql_error)?.collect_and_drop().await.map_err(map_mysql_error)?;
            let tables: Vec<J> = rows
                .iter()
                .map(|r| {
                    let kind = if row_str(r, 1).contains("VIEW") { "view" } else { "table" };
                    let est = r.as_ref(2).map(|v| match v {
                        Value::Int(i) => *i,
                        Value::UInt(u) => *u as i64,
                        _ => 0,
                    });
                    let mut t = json!({ "name": row_str(r, 0), "kind": kind });
                    if let Some(e) = est {
                        if e > 0 {
                            t["rowsEstimate"] = json!(e);
                        }
                    }
                    t
                })
                .collect();
            json!({ "tables": tables })
        }
        "columns" => {
            let schema_v = schema.unwrap_or_default();
            let table_v = table.unwrap_or_default();
            let rows: Vec<mysql_async::Row> = conn.query_iter(format!(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}' ORDER BY ORDINAL_POSITION",
                escape_sql_str(&schema_v), escape_sql_str(&table_v)
            )).await.map_err(map_mysql_error)?.collect_and_drop().await.map_err(map_mysql_error)?;
            let cols: Vec<J> = rows
                .iter()
                .map(|r| {
                    let mut c = json!({
                        "name": row_str(r, 0),
                        "dataType": row_str(r, 1),
                        "pk": row_str(r, 4) == "PRI",
                        "nullable": row_str(r, 2) == "YES",
                    });
                    match r.as_ref(3) {
                        Some(Value::NULL) | None => c["default"] = J::Null,
                        Some(v) => c["default"] = J::String(mysql_value_to_json(v, ColumnType::MYSQL_TYPE_VAR_STRING).to_string().trim_matches('"').to_string()),
                    }
                    c
                })
                .collect();
            json!({ "columns": cols })
        }
        "indexes" => {
            let schema_v = schema.unwrap_or_default();
            let table_v = table.unwrap_or_default();
            let rows: Vec<mysql_async::Row> = conn.query_iter(format!(
                "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}' ORDER BY INDEX_NAME, SEQ_IN_INDEX",
                escape_sql_str(&schema_v), escape_sql_str(&table_v)
            )).await.map_err(map_mysql_error)?.collect_and_drop().await.map_err(map_mysql_error)?;
            // 聚合同名索引的列
            let mut ordered: Vec<String> = Vec::new();
            let mut map: HashMap<String, (bool, Vec<String>)> = HashMap::new();
            for r in &rows {
                let name = row_str(r, 0);
                let unique = row_str(r, 1) == "0";
                let col = row_str(r, 2);
                map.entry(name.clone())
                    .and_modify(|e| e.1.push(col.clone()))
                    .or_insert_with(|| {
                        ordered.push(name.clone());
                        (unique, vec![col])
                    });
            }
            let indexes: Vec<J> = ordered
                .iter()
                .map(|n| {
                    let (unique, cols) = &map[n];
                    json!({ "name": n, "unique": unique, "columns": cols })
                })
                .collect();
            json!({ "indexes": indexes })
        }
        other => {
            return Err(DriverError::unsupported(format!("unknown introspect scope: {other}")));
        }
        })
    }
    .await;
    put_conn(state, &conn_id, conn);
    Ok(out?)
}

fn row_str(row: &mysql_async::Row, idx: usize) -> String {
    match row.as_ref(idx) {
        Some(Value::Bytes(b)) => String::from_utf8_lossy(b).to_string(),
        Some(Value::Int(i)) => i.to_string(),
        Some(Value::UInt(u)) => u.to_string(),
        _ => String::new(),
    }
}

/// 单引号转义（introspect 元数据查询用）
fn escape_sql_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "''")
}

fn cancel() -> Result<J, DriverError> {
    // 尽力而为：查询是短时同步 await，首期仅确认收到
    Ok(json!({ "ok": true }))
}

fn disconnect(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    if let Some(conn_id) = str_param(&params, "connId") {
        state.lock().unwrap().conns.remove(&conn_id);
    }
    Ok(json!({ "ok": true }))
}

/// mysql_async 错误 → 协议错误对象
fn map_mysql_error(e: mysql_async::Error) -> DriverError {
    use mysql_async::Error as E;
    match &e {
        E::Server(se) => {
            let msg = format!("{} (code {})", se.message, se.code);
            if se.code == 1045 || se.code == 1044 || se.message.contains("Access denied") {
                DriverError::permission_denied("auth-failed", msg)
            } else {
                DriverError::sql_error("query-failed", msg)
            }
        }
        E::Io(ioe) => {
            let msg = ioe.to_string();
            if msg.contains("timeout") {
                DriverError::network("timeout", format!("connection timeout: {msg}"), true)
            } else {
                DriverError::network("network-error", msg, true)
            }
        }
        _ => DriverError::network("mysql-error", e.to_string(), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_value_conversions() {
        // 数值
        assert_eq!(mysql_value_to_json(&Value::Int(-7), ColumnType::MYSQL_TYPE_LONG), json!(-7));
        assert_eq!(
            mysql_value_to_json(&Value::UInt(u64::MAX), ColumnType::MYSQL_TYPE_LONGLONG),
            J::String(u64::MAX.to_string())
        );
        // NULL
        assert_eq!(mysql_value_to_json(&Value::NULL, ColumnType::MYSQL_TYPE_NULL), J::Null);
        // 二进制（非 UTF-8）→ base64；TEXT/BLOB wire 类型同源，按内容启发式区分
        assert_eq!(
            mysql_value_to_json(&Value::Bytes(vec![0xff, 0x00, 0x01]), ColumnType::MYSQL_TYPE_BLOB),
            J::String("/wAB".to_string())
        );
        // 字符串
        assert_eq!(
            mysql_value_to_json(&Value::Bytes(b"hi".to_vec()), ColumnType::MYSQL_TYPE_VAR_STRING),
            J::String("hi".to_string())
        );
        // DECIMAL → string
        assert_eq!(
            mysql_value_to_json(&Value::Bytes(b"12.30".to_vec()), ColumnType::MYSQL_TYPE_NEWDECIMAL),
            J::String("12.30".to_string())
        );
        // 时间 → 字符串
        assert_eq!(
            mysql_value_to_json(&Value::Date(2026, 10, 15, 0, 0, 0, 0), ColumnType::MYSQL_TYPE_DATE),
            J::String("2026-10-15".to_string())
        );
        assert_eq!(
            mysql_value_to_json(&Value::Date(2026, 10, 15, 8, 30, 0, 0), ColumnType::MYSQL_TYPE_DATETIME),
            J::String("2026-10-15 08:30:00".to_string())
        );
        assert_eq!(
            mysql_value_to_json(&Value::Time(true, 1, 2, 0, 0, 0), ColumnType::MYSQL_TYPE_TIME),
            J::String("-26:00:00".to_string())
        );
    }

    #[test]
    fn escape_sql_string() {
        assert_eq!(escape_sql_str("a'b\\c"), "a''b\\\\c");
    }
}
