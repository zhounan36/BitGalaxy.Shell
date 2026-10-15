//! postgres-host：PostgreSQL 驱动宿主（stdio NDJSON-RPC，tokio-postgres 纯 Rust）。
//!
//! 值转换约定：数值 → JSON number（numeric/超范围 → string）、字符串 → string、
//! 二进制(bytea/bit) → base64 string、时间/日期 → 字符串（timestamptz 用 RFC3339，
//! timestamp/date/time 用 "YYYY-MM-DD HH:MM:SS[.ffffff]" 形态）、json/jsonb → 嵌套 JSON 值。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Duration as ChronoDuration, NaiveDate, NaiveDateTime};
use host_common::{
    bytes_to_json, clamp_max_rows, f64_to_json, run_stdio, BoxFut, DriverError,
    RequestHandler,
};
use postgres_types::{FromSql, Type};
use serde_json::{json, Value as J};
use tokio_postgres::{Client, Config, Error as PgError, NoTls, Row, SimpleQueryMessage};

const DRIVER_VERSION: &str = "0.1.0";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

struct State {
    /// connId → (客户端, 后台连接任务句柄)；Client 不可 Clone，共享用 Arc
    conns: HashMap<String, (Arc<Client>, tokio::task::JoinHandle<()>)>,
    /// queryId → 取消令牌（cancel 真实中断用）
    queries: HashMap<String, tokio_postgres::CancelToken>,
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        let _ = run_stdio(PgHandler {
            state: Arc::new(Mutex::new(State {
                conns: HashMap::new(),
                queries: HashMap::new(),
            })),
        })
        .await;
    });
}

struct PgHandler {
    state: Arc<Mutex<State>>,
}

impl RequestHandler for PgHandler {
    fn handle(&self, method: &str, params: J) -> BoxFut {
        let state = self.state.clone();
        let method = method.to_string();
        Box::pin(async move {
            match method.as_str() {
                "initialize" => initialize(),
                "connect" => connect(&state, params).await,
                "query" => query(&state, params).await,
                "queryMore" => query_more(params),
                "execute" => execute(&state, params).await,
                "introspect" => introspect(&state, params).await,
                "cancel" => cancel(&state, params),
                "disconnect" => disconnect(&state, params),
                other => Err(DriverError::unsupported(format!("unknown method: {other}"))),
            }
        })
    }
}

fn initialize() -> Result<J, DriverError> {
    Ok(json!({
        "name": "postgres",
        "version": DRIVER_VERSION,
        "capabilities": { "cancel": true, "streaming": false, "multipleDatabases": true }
    }))
}

fn str_param(params: &J, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

async fn connect(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let host = str_param(&params, "host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = params.get("port").and_then(|v| v.as_u64()).unwrap_or(5432) as u16;
    let user = str_param(&params, "user").unwrap_or_default();
    let password = str_param(&params, "password").unwrap_or_default();
    let database = str_param(&params, "database");

    let mut cfg = Config::new();
    cfg.host(host).port(port).user(user).password(password).connect_timeout(CONNECT_TIMEOUT);
    if let Some(db) = database {
        cfg.dbname(db);
    }

    let connect_fut = async {
        let (client, connection) = cfg.connect(NoTls).await?;
        let task = tokio::spawn(async move {
            let _ = connection.await; // 后台驱动连接，Client 断开后自然结束
        });
        Ok::<_, PgError>((client, task))
    };
    let (client, task) = tokio::time::timeout(CONNECT_TIMEOUT, connect_fut)
        .await
        .map_err(|_| DriverError::network("connect-timeout", "connection timeout", true))?
        .map_err(map_pg_error)?;

    // tokio-postgres 无 parameter() API：版本/当前库走真实查询（失败回退默认值）
    let server_version = match client.query_one("SHOW server_version", &[]).await {
        Ok(row) => row.try_get::<_, String>(0).unwrap_or_else(|_| "unknown".into()),
        Err(_) => "unknown".to_string(),
    };
    let default_database = match client.query_one("SELECT current_database()", &[]).await {
        Ok(row) => row.try_get::<_, String>(0).ok(),
        Err(_) => None,
    }
    .or_else(|| cfg.get_dbname().map(|s| s.to_string()));

    let conn_id = uuid::Uuid::new_v4().to_string();
    state
        .lock()
        .unwrap()
        .conns
        .insert(conn_id.clone(), (Arc::new(client), task));

    let mut result = json!({ "connId": conn_id, "serverVersion": server_version });
    if let Some(db) = default_database {
        result["defaultDatabase"] = J::String(db);
    }
    Ok(result)
}

fn get_client(state: &Arc<Mutex<State>>, params: &J) -> Result<Arc<Client>, DriverError> {
    let conn_id = str_param(params, "connId")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing connId"))?;
    state
        .lock()
        .unwrap()
        .conns
        .get(&conn_id)
        .map(|(c, _)| Arc::clone(c))
        .ok_or_else(|| DriverError::sql_error("unknown-conn", format!("unknown connId: {conn_id}")))
}

async fn query(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let sql = str_param(&params, "sql")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing sql"))?;
    let query_id = str_param(&params, "queryId").unwrap_or_default();
    let max_rows = clamp_max_rows(params.get("maxRows").and_then(|v| v.as_u64()));
    let timeout_ms = params.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(30_000).max(1);

    let client = get_client(state, &params)?;
    let cancel_token = client.cancel_token();
    state.lock().unwrap().queries.insert(query_id.clone(), cancel_token.clone());

    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), run_query(&client, &sql, max_rows)).await;
    state.lock().unwrap().queries.remove(&query_id);
    let duration_ms = started.elapsed().as_millis() as u64;

    let (columns, rows, total) = match result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(map_pg_error(e)),
        Err(_) => {
            // 尽力而为：真实中断后端查询
            let token = cancel_token.clone();
            tokio::spawn(async move {
                let _ = token.cancel_query(NoTls).await;
            });
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
        "truncated": (rows.len() as u64) < total,
        "durationMs": duration_ms,
    }))
}

/// 执行查询：优先扩展协议（类型化值），prepare 失败（多语句/非常规语句）回退 simple_query
async fn run_query(client: &Client, sql: &str, max_rows: u32) -> Result<(Vec<J>, Vec<J>, u64), PgError> {
    match client.prepare(sql).await {
        Ok(stmt) => {
            let col_types: Vec<Type> = stmt.columns().iter().map(|c| c.type_().clone()).collect();
            let columns: Vec<J> = stmt
                .columns()
                .iter()
                .map(|c| json!({ "name": c.name(), "dataType": c.type_().name() }))
                .collect();
            let raw_rows = client.query(&stmt, &[]).await?;
            let total = raw_rows.len() as u64;
            let rows = raw_rows
                .iter()
                .take(max_rows as usize)
                .map(pg_row_to_json)
                .collect::<Result<Vec<J>, PgError>>()?;
            let _ = col_types;
            Ok((columns, rows, total))
        }
        Err(_) => run_query_simple(client, sql, max_rows).await,
    }
}

/// simple_query 回退：值全部为文本形态
async fn run_query_simple(client: &Client, sql: &str, max_rows: u32) -> Result<(Vec<J>, Vec<J>, u64), PgError> {
    let msgs = client.simple_query(sql).await?;
    let mut columns: Vec<J> = Vec::new();
    let mut rows: Vec<J> = Vec::new();
    let mut total: u64 = 0;
    for msg in &msgs {
        if let SimpleQueryMessage::Row(row) = msg {
            if columns.is_empty() {
                columns = row.columns().iter().map(|c| json!({ "name": c.name() })).collect();
            }
            total += 1;
            if (rows.len() as u64) < max_rows as u64 {
                let vals: Vec<J> = (0..row.len())
                    .map(|i| match row.get(i) {
                        Some(s) => J::String(s.to_string()),
                        None => J::Null,
                    })
                    .collect();
                rows.push(J::Array(vals));
            }
        }
    }
    Ok((columns, rows, total))
}

fn pg_row_to_json(row: &Row) -> Result<J, PgError> {
    let mut vals = Vec::with_capacity(row.len());
    for i in 0..row.len() {
        let raw: RawValue = row.try_get(i)?;
        vals.push(pg_value_to_json(&raw));
    }
    Ok(J::Array(vals))
}

/// PG 原始值（保留 wire 字节 + 类型），用于自由转换 JSON
struct RawValue {
    ty: Type,
    data: Option<Vec<u8>>,
}

impl<'a> FromSql<'a> for RawValue {
    fn from_sql(ty: &Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawValue { ty: ty.clone(), data: Some(raw.to_vec()) })
    }
    fn from_sql_null(ty: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawValue { ty: ty.clone(), data: None })
    }
    fn accepts(_: &Type) -> bool {
        true
    }
}

/// PG 值 → JSON（转换约定见文件头）
fn pg_value_to_json(v: &RawValue) -> J {
    let Some(raw) = v.data.as_deref() else { return J::Null };
    let name = v.ty.name();
    match name {
        "bool" => J::Bool(raw.first().map(|b| *b != 0).unwrap_or(false)),
        "int2" => postgres_protocol::types::int2_from_sql(raw).map(|x| json!(x)).unwrap_or(J::Null),
        "int4" => postgres_protocol::types::int4_from_sql(raw).map(|x| json!(x)).unwrap_or(J::Null),
        "int8" => postgres_protocol::types::int8_from_sql(raw).map(|x| json!(x)).unwrap_or(J::Null),
        "oid" => postgres_protocol::types::oid_from_sql(raw).map(|x| json!(x)).unwrap_or(J::Null),
        "float4" => postgres_protocol::types::float4_from_sql(raw)
            .map(|x| f64_to_json(x as f64))
            .unwrap_or(J::Null),
        "float8" => postgres_protocol::types::float8_from_sql(raw)
            .map(f64_to_json)
            .unwrap_or(J::Null),
        "numeric" => match pg_numeric_to_string(raw) {
            Ok(s) => J::String(s),
            Err(_) => J::Null,
        },
        "text" | "varchar" | "bpchar" | "char" | "name" | "unknown" | "cstring" => {
            match postgres_protocol::types::text_from_sql(raw) {
                Ok(s) => J::String(s.to_string()),
                Err(_) => J::String(String::from_utf8_lossy(raw).to_string()),
            }
        }
        "bytea" => bytes_to_json(postgres_protocol::types::bytea_from_sql(raw)),
        "json" => serde_json::from_slice::<J>(raw).unwrap_or_else(|_| J::String(String::from_utf8_lossy(raw).to_string())),
        "jsonb" => {
            // jsonb wire 首字节是版本号 0x01
            let body = if raw.len() > 1 && raw[0] == 1 { &raw[1..] } else { raw };
            serde_json::from_slice::<J>(body).unwrap_or_else(|_| J::String(String::from_utf8_lossy(raw).to_string()))
        }
        "date" => match postgres_protocol::types::date_from_sql(raw) {
            Ok(days) => J::String(fmt_pg_date(days)),
            Err(_) => J::Null,
        },
        "time" => match postgres_protocol::types::time_from_sql(raw) {
            Ok(us) => J::String(fmt_pg_time(us)),
            Err(_) => J::Null,
        },
        "timestamp" => match postgres_protocol::types::timestamp_from_sql(raw) {
            Ok(us) => J::String(fmt_pg_timestamp(us)),
            Err(_) => J::Null,
        },
        "timestamptz" => match postgres_protocol::types::timestamp_from_sql(raw) {
            Ok(us) => J::String(fmt_pg_timestamp(us)),
            Err(_) => J::Null,
        },
        "uuid" => match postgres_protocol::types::uuid_from_sql(raw) {
            Ok(bytes) => J::String(uuid::Uuid::from_bytes(bytes).to_string()),
            Err(_) => bytes_to_json(raw),
        },
        // bit/varbit/inet/macaddr/数组/区间等未知形态 → base64
        _ => bytes_to_json(raw),
    }
}

/// PG numeric wire → 十进制字符串（保留 dscale）
fn pg_numeric_to_string(raw: &[u8]) -> Result<String, &'static str> {
    if raw.len() < 8 {
        return Err("numeric too short");
    }
    let ndigits = u16::from_be_bytes([raw[0], raw[1]]) as usize;
    let weight = i16::from_be_bytes([raw[2], raw[3]]) as isize;
    let sign = u16::from_be_bytes([raw[4], raw[5]]);
    let dscale = i16::from_be_bytes([raw[6], raw[7]]);
    match sign {
        0xC000 => return Ok("NaN".to_string()),
        0xD000 => return Ok("Infinity".to_string()),
        0xF000 => return Ok("-Infinity".to_string()),
        _ => {}
    }
    if raw.len() < 8 + ndigits * 2 {
        return Err("numeric digits truncated");
    }
    let digits: Vec<u16> = (0..ndigits)
        .map(|i| u16::from_be_bytes([raw[8 + i * 2], raw[8 + i * 2 + 1]]))
        .collect();
    let negative = sign == 0x4000;

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        for i in 0..=(weight as usize) {
            let d = digits.get(i).copied().unwrap_or(0);
            if i == 0 {
                out.push_str(&d.to_string());
            } else {
                out.push_str(&format!("{d:04}"));
            }
        }
    }
    if dscale > 0 {
        out.push('.');
        let mut frac = String::new();
        let mut i = weight + 1;
        while (frac.len() as i16) < dscale {
            let d = if i < 0 { 0 } else { digits.get(i as usize).copied().unwrap_or(0) };
            frac.push_str(&format!("{d:04}"));
            i += 1;
        }
        out.push_str(&frac[..dscale as usize]);
    }
    Ok(out)
}

fn pg_base_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2000, 1, 1).expect("pg epoch")
}

fn pg_base() -> NaiveDateTime {
    pg_base_date().and_hms_opt(0, 0, 0).expect("pg epoch")
}

fn fmt_pg_date(days: i32) -> String {
    match pg_base_date().checked_add_signed(ChronoDuration::days(days as i64)) {
        Some(d) => d.format("%Y-%m-%d").to_string(),
        None => format!("day {days}"),
    }
}

fn fmt_pg_time(micros: i64) -> String {
    let us = micros.rem_euclid(1_000_000);
    let secs = micros.div_euclid(1_000_000);
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if us == 0 {
        format!("{h:02}:{mi:02}:{s:02}")
    } else {
        format!("{h:02}:{mi:02}:{s:02}.{us:06}")
    }
}

fn fmt_pg_timestamp(micros: i64) -> String {
    match pg_base().checked_add_signed(ChronoDuration::microseconds(micros)) {
        Some(dt) => {
            let us = micros.rem_euclid(1_000_000);
            if us == 0 {
                dt.format("%Y-%m-%d %H:%M:%S").to_string()
            } else {
                format!("{}.{us:06}", dt.format("%Y-%m-%d %H:%M:%S"))
            }
        }
        None => format!("timestamp {micros}us"),
    }
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

async fn execute(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let sql = str_param(&params, "sql")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing sql"))?;
    let timeout_ms = params.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(30_000).max(1);
    let client = get_client(state, &params)?;

    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), client.simple_query(&sql)).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    let affected: u64 = match result {
        Ok(Ok(msgs)) => msgs
            .iter()
            .map(|m| match m {
                SimpleQueryMessage::CommandComplete(n) => *n,
                _ => 0,
            })
            .sum(),
        Ok(Err(e)) => return Err(map_pg_error(e)),
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

/// 元数据 SQL 字符串字面量转义
fn escape_sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

async fn introspect(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    let scope = str_param(&params, "scope")
        .ok_or_else(|| DriverError::sql_error("bad-params", "missing scope"))?;
    let schema = str_param(&params, "schema").unwrap_or_else(|| "public".to_string());
    let table = str_param(&params, "table").unwrap_or_default();
    let client = get_client(state, &params)?;

    let out = match scope.as_str() {
        "databases" => {
            let rows = client
                .query("SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY datname", &[])
                .await
                .map_err(map_pg_error)?;
            let dbs: Vec<J> = rows
                .iter()
                .map(|r| json!({ "name": raw_str(r, 0) }))
                .collect();
            json!({ "databases": dbs })
        }
        "tables" => {
            let rows = client
                .query(
                    &format!(
                        "SELECT c.relname, c.relkind, GREATEST(c.reltuples, 0) \
                         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = '{}' AND c.relkind IN ('r','v','m','p') ORDER BY c.relname",
                        escape_sql_str(&schema)
                    ),
                    &[],
                )
                .await
                .map_err(map_pg_error)?;
            let tables: Vec<J> = rows
                .iter()
                .map(|r| {
                    let kind = if raw_str(r, 1) == "v" || raw_str(r, 1) == "m" { "view" } else { "table" };
                    let mut t = json!({ "name": raw_str(r, 0), "kind": kind });
                    if let Ok(est) = r.try_get::<_, i64>(2) {
                        if est > 0 {
                            t["rowsEstimate"] = json!(est);
                        }
                    }
                    t
                })
                .collect();
            json!({ "tables": tables })
        }
        "columns" => {
            let rows = client
                .query(
                    &format!(
                        "SELECT column_name, data_type, is_nullable, column_default \
                         FROM information_schema.columns \
                         WHERE table_schema = '{}' AND table_name = '{}' ORDER BY ordinal_position",
                        escape_sql_str(&schema),
                        escape_sql_str(&table)
                    ),
                    &[],
                )
                .await
                .map_err(map_pg_error)?;
            // 主键列集合
            let pk_rows = client
                .query(
                    &format!(
                        "SELECT kcu.column_name FROM information_schema.table_constraints tc \
                         JOIN information_schema.key_column_usage kcu \
                           ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema \
                         WHERE tc.table_schema = '{}' AND tc.table_name = '{}' AND tc.constraint_type = 'PRIMARY KEY'",
                        escape_sql_str(&schema),
                        escape_sql_str(&table)
                    ),
                    &[],
                )
                .await
                .map_err(map_pg_error)?;
            let pk_set: std::collections::HashSet<String> =
                pk_rows.iter().map(|r| raw_str(r, 0)).collect();
            let cols: Vec<J> = rows
                .iter()
                .map(|r| {
                    let name = raw_str(r, 0);
                    let mut c = json!({
                        "name": name,
                        "dataType": raw_str(r, 1),
                        "pk": pk_set.contains(&name),
                        "nullable": raw_str(r, 2) == "YES",
                    });
                    match r.try_get::<_, Option<String>>(3) {
                        Ok(Some(d)) => c["default"] = J::String(d),
                        _ => c["default"] = J::Null,
                    }
                    c
                })
                .collect();
            json!({ "columns": cols })
        }
        "indexes" => {
            let rows = client
                .query(
                    &format!(
                        "SELECT i.relname, ix.indisunique, a.attname \
                         FROM pg_class t \
                         JOIN pg_index ix ON t.oid = ix.indrelid \
                         JOIN pg_class i ON i.oid = ix.indexrelid \
                         JOIN pg_namespace n ON n.oid = t.relnamespace \
                         JOIN LATERAL unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) ON TRUE \
                         JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
                         WHERE n.nspname = '{}' AND t.relname = '{}' \
                         ORDER BY i.relname, k.ord",
                        escape_sql_str(&schema),
                        escape_sql_str(&table)
                    ),
                    &[],
                )
                .await
                .map_err(map_pg_error)?;
            let mut ordered: Vec<String> = Vec::new();
            let mut map: HashMap<String, (bool, Vec<String>)> = HashMap::new();
            for r in &rows {
                let name = raw_str(r, 0);
                let unique = r.try_get::<_, bool>(1).unwrap_or(false);
                let col = raw_str(r, 2);
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
        other => return Err(DriverError::unsupported(format!("unknown introspect scope: {other}"))),
    };
    Ok(out)
}

/// 元数据行取字符串（RawValue 展示形态）
fn raw_str(row: &Row, idx: usize) -> String {
    match row.try_get::<_, RawValue>(idx) {
        Ok(v) => match pg_value_to_json(&v) {
            J::String(s) => s,
            J::Null => String::new(),
            other => other.to_string(),
        },
        Err(_) => String::new(),
    }
}

fn cancel(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    if let Some(query_id) = str_param(&params, "queryId") {
        let token = state.lock().unwrap().queries.get(&query_id).cloned();
        if let Some(token) = token {
            // 尽力而为：真实中断后端查询
            tokio::spawn(async move {
                let _ = token.cancel_query(NoTls).await;
            });
        }
    }
    Ok(json!({ "ok": true }))
}

fn disconnect(state: &Arc<Mutex<State>>, params: J) -> Result<J, DriverError> {
    if let Some(conn_id) = str_param(&params, "connId") {
        if let Some((_, task)) = state.lock().unwrap().conns.remove(&conn_id) {
            task.abort();
        }
    }
    Ok(json!({ "ok": true }))
}

/// tokio-postgres 错误 → 协议错误对象
fn map_pg_error(e: PgError) -> DriverError {
    if let Some(db) = e.as_db_error() {
        let msg = format!("{} (SQLSTATE {})", db.message(), db.code().code());
        return match db.code().code() {
            "28P01" | "28000" => DriverError::permission_denied("auth-failed", msg),
            _ => DriverError::sql_error("query-failed", msg),
        };
    }
    let msg = e.to_string();
    if msg.contains("authentication failed") {
        DriverError::permission_denied("auth-failed", msg)
    } else if e.is_closed() {
        DriverError::network("connection-closed", format!("connection closed: {msg}"), true)
    } else {
        DriverError::network("network-error", msg, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_decoding() {
        // 12.30 → digits [12, 3000], weight 0, dscale 2
        let raw = [
            0u8, 2, // ndigits
            0, 0, // weight
            0, 0, // sign: positive
            0, 2, // dscale
            0, 12, // digit 12
            0x0b, 0xb8, // digit 3000
        ];
        assert_eq!(pg_numeric_to_string(&raw).unwrap(), "12.30");

        // -1.5 → digits [1, 5000], weight 0, dscale 1, negative
        let raw = [0u8, 2, 0, 0, 0x40, 0, 0, 1, 0, 1, 0x13, 0x88];
        assert_eq!(pg_numeric_to_string(&raw).unwrap(), "-1.5");

        // 0.007 → digits [70]（weight=-1 时首位基数 1/10000），dscale 3
        let raw = [0u8, 1, 0xff, 0xff, 0, 0, 0, 3, 0, 70];
        assert_eq!(pg_numeric_to_string(&raw).unwrap(), "0.007");

        // 10000 → digits [1], weight 1, dscale 0
        let raw = [0u8, 1, 0, 1, 0, 0, 0, 0, 0, 1];
        assert_eq!(pg_numeric_to_string(&raw).unwrap(), "10000");

        // NaN
        let raw = [0u8, 0, 0, 0, 0xc0, 0, 0, 0];
        assert_eq!(pg_numeric_to_string(&raw).unwrap(), "NaN");
    }

    #[test]
    fn value_conversions() {
        let mk = |ty: &Type| RawValue { ty: ty.clone(), data: None };
        // NULL
        let mut v = mk(&Type::TEXT);
        assert_eq!(pg_value_to_json(&v), J::Null);
        // int8
        v = mk(&Type::INT8);
        v.data = Some((-42i64).to_be_bytes().to_vec());
        assert_eq!(pg_value_to_json(&v), json!(-42));
        // text
        v = mk(&Type::TEXT);
        v.data = Some(b"hi".to_vec());
        assert_eq!(pg_value_to_json(&v), J::String("hi".to_string()));
        // bytea → base64
        v = mk(&Type::BYTEA);
        v.data = Some(vec![0u8, 1, 2]);
        assert_eq!(pg_value_to_json(&v), J::String("AAEC".to_string()));
        // bool
        v = mk(&Type::BOOL);
        v.data = Some(vec![1]);
        assert_eq!(pg_value_to_json(&v), J::Bool(true));
    }

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_pg_date(0), "2000-01-01");
        assert_eq!(fmt_pg_time(3_661_500_000), "01:01:01.500000");
        assert_eq!(fmt_pg_time(3_600_000_000), "01:00:00");
        assert_eq!(fmt_pg_timestamp(0), "2000-01-01 00:00:00");
    }
}
