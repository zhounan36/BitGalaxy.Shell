# SQL 数据库管理模块设计方案

## Context

BitGalaxy.Shell 目前是 SSH 工作台（终端 / SFTP / Metrics / AI 对话），用户需要在其中直接管理远端服务器上的数据库。目标是新增 SQL 模块：完整管理工作台（库表结构树 + SQL 编辑器 + 结果网格 + 表数据查看），支持 MySQL/MariaDB、PostgreSQL，以及远端 SQLite 文件。

已确认的产品决策（用户拍板）：

1. **连接通道**：复用已有 SSH 连接做 direct-tcpip 隧道转发（远端 127.0.0.1:3306/5432 → Agent 内驱动），Navicat/DataGrip 同款方式，不依赖远端装客户端。
2. **驱动按需动态加载**：每种数据库驱动是**独立可执行程序（驱动宿主进程）**，Agent 主程序不含任何数据库协议实现；首次连接对应数据库类型时从远程源自动下载（内置清单 + SHA256 校验）到本地缓存，之后离线可用；Agent 与驱动经 stdio NDJSON-RPC 通信（类似 MCP 架构），驱动崩溃不连累主进程。开发期支持本地手动放置驱动包。
3. **UI**：完整管理工作台（Navicat 精简版）：左侧库/表/字段树 + 多标签 SQL 编辑器（语法高亮、执行选中）+ 结果网格（排序/分页/CSV 导出）+ 表数据查看。

## 架构总览

```
SqlPane(React) ── WS sql 帧(rid) ──> server.rs dispatch ──> sql/ 模块
                                              │
                    ┌─────────────────────────┼──────────────────────┐
                    ▼                         ▼                      ▼
              TunnelBridge            DriverManager            Stores.secrets
         (ConnRegistry + direct-tcpip)  (spawn 驱动宿主进程)      (凭据内存缓存)
                    │                         │
                    │ 本地 127.0.0.1:port      │ stdio NDJSON-RPC
                    ▼                         ▼
              SSH 远端 3306/5432          drivers/mysql-host.exe (mysql_async)
```

## 1. 驱动宿主协议（Agent ↔ 驱动进程，stdio NDJSON-RPC）

每行一条 JSON：

- 请求 `{"id": u64, "method": "...", "params": {...}}`
- 响应 `{"id": u64, "result": {...}}` 或 `{"id": u64, "error": {"code", "kind", "message", "retryable"}}`
- 通知 `{"method": "log"/"progress", "params": {...}}`（无 id，进 tracing 日志）

| method | params 关键字段 | result |
|---|---|---|
| `initialize` | protocolVersion: 1, agentVersion | name, version, capabilities{cancel, streaming, multipleDatabases} |
| `connect` | host(=127.0.0.1), port(=隧道本地端口), user, password, database?, sqlitePath? | connId, serverVersion |
| `query` | connId, sql, maxRows(≤2000), timeoutMs, queryId | columns[{name,dataType,nullable}], rows[][], rowCount, truncated, durationMs |
| `queryMore` | connId, queryId, maxRows | 同 query（游标续取） |
| `execute` | connId, sql, timeoutMs | affected, durationMs（DDL/DML） |
| `introspect` | connId, scope: databases\|tables\|columns\|indexes, schema?, table? | 结构化数组（库/表/列{type,pk,nullable,default}/索引） |
| `cancel` | queryId | ok |
| `disconnect` | connId | ok |

要点：

- 驱动只感知 `127.0.0.1:隧道端口`，**不感知 SSH**；密码经 stdin 传递，禁止出现在命令行参数。
- SQLite：`connect` 传本地临时文件路径 `sqlitePath`，驱动只读打开。
- 首期驱动实现直接用现成 crate 打包在宿主进程里（mysql_async / tokio-postgres / rusqlite），按平台编译发布——外置的意义在于主程序不带驱动、按需下载、崩溃隔离。

生命周期（`sql/driver.rs`）：按驱动惰性 spawn（tokio::process，stderr 逐行捕获）；initialize 5s 超时；空闲 30min 回收；监视退出 → 全部 pending 请求 fail-fast（`errorKind:"driver_crashed"`）；无法 spawn → `driver_unavailable`。

## 2. 驱动管理器（`sql/driver_mgr.rs`）

清单（编译期内置 `drivers.json`，可选远程覆盖）：

```json
{ "name": "mysql", "version": "0.1.0", "protocolVersion": 1,
  "artifacts": { "windows-x86_64": { "url": "https://.../mysql-host-0.1.0-win-x64.zip", "sha256": "..." } } }
```

- 缓存目录：`{app_data_dir}/drivers/{name}/{version}/{platform}/host.exe` + `manifest.json`（记录校验状态）。
- 下载：reqwest → `.part` → sha2 校验 → 原子改名 → 写 manifest；失败重试 2 次报 `driver_download_failed`。
- 手动安装：host.exe + manifest.json 放入缓存目录即被识别（开发期用）；Settings 可后续加"驱动管理"页。
- 清单 version 变更即下载新版，旧版惰性清理。Windows 优先，目录布局留 Linux/macOS。

## 3. SSH 隧道桥（`sql/tunnel.rs`）

- `SshConnection`（ssh.rs）新增 `open_direct_tcpip(host, port)`（russh `channel_open_direct_tcpip`），10s 打开超时（仿 exec_with_stdin 的 timeout 约定）。
- Agent 起本地 `TcpListener` **仅绑定 127.0.0.1:0**；驱动每次 TCP 连接 → 新开一条 direct-tcpip 通道 → 双向 pump（各方向 300s 空闲超时）。多连接驱动天然支持；多 SQL 会话共享 SSH 连接时各持独立通道（与 PTY/SFTP 复用模式一致）。
- 生命周期：`ConnRegistry::acquire_session(host_id, sql_session_id)` 持有者计数防回收；会话关闭 → 关 listener/通道/kill 驱动；SSH 断线 → 通道 EOF → 查询以 `errorKind:"network"` 失败 + 池驱逐（复用现有 evict 约定）。
- SQLite：`fetch-sqlite` op 走 SFTP 下载到 `{temp_dir}/sql/{uuid}.db`（复用 sftp.rs 下载/deadline 机制），只读查询，会话关闭即删。

## 4. 后端模块与协议帧

新目录 `apps/desktop/src-tauri/src/sql/`：`mod.rs`（SqlHandler：会话状态机 + op 分发）、`driver.rs`、`driver_mgr.rs`、`tunnel.rs`、`error.rs`（`classify_sql_error`，仿 `classify_sftp_error`）。

- [protocol.rs](file:///d:\sorce3\BitGalaxy.Shell\apps\desktop\src-tauri\src\protocol.rs)：仿 [SftpOpM](file:///d:\sorce3\BitGalaxy.Shell\apps\desktop\src-tauri\src\protocol.rs#L358) 新增 `SqlOpM{op, rid, conn?, sql?, query_id?, max_rows?, scope?...}`、`SqlResultM{op, rid, ok, error_kind?, message?, rows?, columns?, truncated?, affected?, duration_ms?, tree?}`，`ClientFrame::Sql` / `ServerFrame::SqlResult` / `SqlProgress`。
- op 集合：`connect / disconnect / query / query-more / execute / cancel / introspect / fetch-sqlite / drivers-list`。
- [server.rs](file:///d:\sorce3\BitGalaxy.Shell\apps\desktop\src-tauri\src\server.rs#L188-L264) dispatch 加 `ClientFrame::Sql` 分支（tokio::spawn 内执行）。
- 凭据：密码存 `Stores.secrets`（key=`sql:{uuid}`，仿 sudo-pw 模式），不落盘不进日志，会话关闭即清。
- 超时预算（后端 deadline 严格先于前端 20s）：connect 15s、introspect 10s、query 30s（可提，上限 10min）、initialize 5s。
- 错误分类 errorKind：`sql_error / network / permission_denied / timeout / driver_crashed / driver_unavailable / driver_download_failed / tunnel_failed / too_large`。
- 结果集防护：maxRows 默认 500 上限 2000，超限 `truncated`；单帧 ≤1MB，超限 `too_large` 提示加 WHERE；CSV 导出放前端（结果在内存，Blob 下载）。

## 5. 前端 SqlPane

- [protocol/index.ts](file:///d:\sorce3\BitGalaxy.Shell\packages\protocol\src\index.ts#L16)：`SessionKind` 加 `'sql'`；[App.tsx](file:///d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\App.tsx) 的 `KIND_LABEL/KIND_ICON` 注册（antd `DatabaseOutlined`），tab 渲染分发挂 `SqlPane`。SQL tab 不占 SSH 会话，首次 connect 才建隧道。
- 组件拆分（`packages/workbench-ui/src/components/`）：`SqlPane.tsx`（布局 + `sqlRequest` rid 封装，仿 [SftpPane](file:///d:\sorce3\BitGalaxy.Shell\packages\workbench-ui\src\components\SftpPane.tsx#L120-L157) 的 pending Map + 20s 超时）、`ConnBar`（连接表单/驱动下载进度）、`SchemaTree`（antd Tree 懒加载 introspect）、`SqlEditor`（多标签）、`ResultGrid`（antd Table + virtual 虚拟滚动）、`HistoryPanel`（localStorage）。
- 编辑器：**CodeMirror 6** + `@codemirror/lang-sql`（懒加载）；结果网格用 antd Table virtual，不引 ag-grid。
- 交互：`agentSocket.sendControl({t:'sql', m:{op, rid, ...}})`，`SqlResult` 按 rid resolve，长查询 `SqlProgress` + `cancel`。
- 项目约定：TS 块注释禁止出现 `*/` 字面量；错误文案复用 `describeTransferError` 式的 errorKind → 中文映射。

## 6. 安全

1. 密码只进内存 + 驱动 stdin，不落盘、不进 tracing/命令行。
2. 隧道 listener 仅绑定 127.0.0.1 随机端口；远端端口来自连接配置而非任意输入。
3. 驱动下载强制 SHA256 + HTTPS；手动放置也需 manifest 校验。
4. 全程结构化通道（无 PTY/exec）；SQLite 临时文件只读 + 会话销毁即删；execute（DDL/DML）与 query 分开按钮 + 二次确认。

## 7. 实施步骤

| 阶段 | 内容 | 验证 |
|---|---|---|
| P1 | sql/error.rs + protocol.rs 帧 + driver.rs RPC 骨架（mock echo 驱动） | cargo test（RPC 编解码、错误分类） |
| P2 | driver_mgr.rs 下载/校验/缓存/手动安装识别 | cargo test + 手动放置包识别 |
| P3 | tunnel.rs direct-tcpip + ConnRegistry 接入 | 实机 SSH 打通远端 3306 |
| P4 | mysql-host 驱动（mysql_async 包装）connect/query 跑通 | 驱动宿主独立 cargo test + 实机连 MySQL |
| P5 | SqlPane UI（ConnBar/Tree/Editor/ResultGrid） | tsc --noEmit + vite build + 端到端 |
| P6 | postgres-host + fetch-sqlite（SQLite） | 同上 |
| P7 | 打磨：取消、queryMore、CSV 导出、空闲回收、错误文案 | 回归 + 断线测试 |

## 关键文件

- `apps/desktop/src-tauri/src/protocol.rs` — SqlOpM/SqlResultM/帧类型
- `apps/desktop/src-tauri/src/server.rs` — dispatch 接入
- `apps/desktop/src-tauri/src/ssh.rs` + `pool.rs` — open_direct_tcpip / 持有者生命周期
- `apps/desktop/src-tauri/src/sql/*` — 新模块（mod/driver/driver_mgr/tunnel/error）
- `apps/desktop/src-tauri/src/sftp.rs` — op/rid/deadline/classify 参照，fetch-sqlite 复用
- `packages/protocol/src/index.ts` + `packages/workbench-ui/src/App.tsx` — SessionKind 接入
- `packages/workbench-ui/src/components/SqlPane*` — 新 UI 组件
- `apps/desktop/src-tauri/Cargo.toml` — 新增 sha2/reqwest（如未有）；驱动宿主为独立 crate

## 验证方式

- 每阶段 `cargo test`（agent 与驱动宿主各自）+ `tsc --noEmit` + `vite build`。
- 端到端：SSH 连测试机 → SQL tab 新建 MySQL 连接（观察驱动下载进度）→ 展开表树 → 执行 SELECT 看结果网格/分页/CSV 导出 → 执行 DDL 看确认流程 → 杀掉驱动进程验证 driver_crashed 兜底 → 断开 SSH 验证 network 错误分类与重连。
