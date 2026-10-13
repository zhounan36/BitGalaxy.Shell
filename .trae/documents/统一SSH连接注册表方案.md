# 统一 SSH 连接注册表（ConnRegistry）

## Context

项目当前有两套独立的 SSH 连接管理：

- **SessionManager**（[session.rs](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/session.rs)）：工作台 shell/sftp/metrics 会话用。每个 WS 连接一个实例，每次 `open()` 独立建 TCP 连接，交互式 TOFU（confirm/auth slot 经会话 Outbound 弹窗），关会话即关连接。
- **HostPool**（[toolcore.rs](file:///d:/sorce3/BitGalaxy.Shell/apps/desktop/src-tauri/src/toolcore.rs) L67-235）：AI/MCP 工具用。按 host_id 复用 + 空闲回收（默认 300s，来自 policy），fail-closed TOFU。`AiHandler::new`（每 WS）与 MCP stdio 子进程（独立进程）各建一个实例。

用户需求：SSH Pane、Pet、AI 使用同一套连接管理。已确认方案：**同主机共享一条 TCP 连接**（shell PTY / SFTP / exec / metrics 全部多通道复用），无引用时按空闲超时回收。Pet 经工作台/AI 间接受益，前端协议不变、零改动。

## 设计要点

1. **新模块 pool.rs：`ConnRegistry`**（由 HostPool 进化，进程级单例，MCP 子进程各建实例）
   - 条目：`{ conn: Arc<SshConnection>, sftp: Option<Arc<SftpSession>>, holders: HashSet<String>, idle_task }`，按 host_id 键控
   - 两种获取：`acquire_session`（持久持有者，工作台会话）/ `acquire_transient`（AI 临时用，Drop 自动释放）
   - **空闲回收仅在 holders 为空时触发**（修复现状"空闲即关、不管有人用"的问题）
   - 保留 host_lock（per-host tokio::Mutex）并发建连去重；acquire 入口做 `is_alive` 检查，死条目先移除再重建（吸收 `run_sftp_transfer` 的 evict 重连逻辑）
2. **PTY 从连接对象拆出**（关键前提）：`SshConnection.pty/shell_reader` 是单 Option 字段，一条连接只支持一个 shell。共享后 PTY 必须独立：新增 `PtyShell { read, write }`，由 SessionEntry 持有，每会话各自 `open_pty_shell()`
3. **TOFU 策略按发起方**：工作台 open → `HostKeyAsk::Interactive`（携带会话的 confirm/auth slot + Outbound）；AI/MCP → `FailClosed`（保留"先在工作台人工确认一次"的语义）。同主机首个建连方决定确认方式，后续复用
4. **SFTP 底层关闭唯一归口 registry**：`SftpController::close()` 拆为 `close_ops()`（清上传/下载/传输任务）与底层关闭，避免会话关闭误杀共享的 SftpSession
5. **`AiHandler::dispose` 不再 close_all**（现状会误杀工作台连接），只撤审批；WS 断开 = 逐会话 release 本 WS 的持有者

## 与统一传输服务的接轨（本方案后行）

另一个改造流「统一上传/下载传输服务」（见 [统一传输服务实施方案.md](file:///d:/sorce3/BitGalaxy.Shell/.trae/documents/统一传输服务实施方案.md)）**先行实施**，本方案等其完成后再动工。两方案正交：传输服务管**传输语义层**（preflight 预检、确认弹窗、`.part` 续传、error_kind 错误分类），本方案管**连接生命周期层**（建连、同主机复用、持有者计数、空闲回收）。

**重叠文件与接轨点**：

- `toolcore.rs`：传输方案重写 `run_sftp_transfer`（复用 sftp.rs 的 remote_preflight / xfer_* 引擎 + overwrite/force 参数）。本方案实施时将其连接获取点（原 `pool.acquire_sftp`）迁移到 `registry.acquire_sftp`——同名同语义（返回池缓存的 SftpSession），迁移点集中且机械。
- `session.rs`：传输方案加 `preflight` op 分支与 `fresh` 透传；本方案只改 open/close 的连接来源与 handle_sftp 惰性 SFTP 的取用路径。op 分发结构与「每会话一个 SftpController」形态不变，正交。
- `sftp.rs`：传输方案大改（remote_preflight / classify_sftp_error / fresh / XferState noop）；本方案只拆 `SftpController::close()` → `close_ops()`。XferState 归会话层，SftpSession 由 registry 以 Arc 共享，互不干扰。
- `protocol.rs` / 前端 / Cargo.toml：本方案零改动，无冲突。
- **相互增强**：传输方案的 AI 续传下载与 preflight 复用 registry 缓存的 SftpSession——工作台 SFTP 会话与 AI 传输共享同一条 SSH 连接；`error_kind`（传输层）与 ToolError 映射（连接层）正交，两层并存。

**实施前 rebase 检查清单**（传输方案合并后逐项确认）：

1. 重读 toolcore.rs：确认重写后的 `run_sftp_transfer` 连接获取点数量与形态，步骤 3 工具清单按实际代码调整（原"多次 acquire_sftp + 失败 evict"已被 preflight + 引擎取代）
2. 确认 sftp.rs 新增的 remote_preflight / xfer_* 方法不依赖"每会话独占 SftpSession"；若 XferState 生命周期绑会话，Arc 共享仍成立
3. session.rs 的 `preflight` 分支与 `fresh` 透传保持不动，只迁移连接来源
4. `err_sftp` 的 classify 注入（传输层）与本方案 ToolError 映射（连接层）合并时两层都保留

## 实施步骤（按序，每步可独立编译）

### 步骤 1｜ssh.rs：PTY 出连接
- `connect()` 的 Shell 分支（PTY+shell 建立，15s 超时）抽为 `SshConnection::open_pty_shell(&self) -> Result<PtyShell, SshError>`
- `SshConnection` 删除 `pty`/`shell_reader` 字段及 `write_input`/`resize`（迁入 PtyShell，保留 5s 写超时）
- `SshKind` 简化：池内连接一律 `Exec` 建连（无 PTY），按需开通道

### 步骤 2｜新建 pool.rs：ConnRegistry
```rust
pub struct ConnRegistry { /* stores, idle_close_sec, entries: Mutex<HashMap<host_id, Entry>>, host_locks */ }
pub struct CreateCtx { ask: HostKeyAsk, mismatch: Arc<AtomicBool> }  // Interactive 携 slot+Outbound；默认 FailClosed
impl ConnRegistry {
    pub fn new(stores: Stores, idle_close_sec: u64) -> Self;
    pub async fn acquire_session(&self, host_id, holder_id, create: CreateCtx) -> Result<Arc<SshConnection>, ToolError>;
    pub async fn acquire_transient(&self, host_id) -> Result<TransientLease, ToolError>;  // Drop → release
    pub async fn acquire_sftp(&self, host_id) -> Result<Arc<SftpSession>, ToolError>;     // 池内缓存
    pub fn release(&self, host_id, holder_id);  // holders 空则重启 idle 计时
    pub fn evict(&self, host_id);               // 关 conn+sftp，仅 registry 归口
    pub fn close_all(&self);
}
```
- 错误映射（ERR_HOSTKEY_UNKNOWN/ERR_AUTH_FAILED/ERR_CONNECT_FAILED…）与审计事件（`connection.opened/closed` 带 initiator：workbench/ai/mcp + reason）从 toolcore 迁入

### 步骤 3｜toolcore.rs 切换
- 删 HostPool；`ToolCtx.pool` → `registry: &ConnRegistry`
- 工具函数换 transient 获取：`run_get_metrics`/`run_exec_command`（acquire）→ acquire_transient；`run_read_file`/`run_write_file`（acquire_sftp）；`run_list_dir`/`run_make_dir`/`run_rename_path`/`run_delete_path`（acquire_sftp_ctrl，基于池缓存 sftp + `SftpController::from_shared`）
- `run_sftp_transfer`：**传输方案已重写为 preflight + xfer_* 引擎形态**（含 overwrite/force 参数），本步骤只迁移其连接获取点到 `registry.acquire_sftp`，不动预检/确认/传输逻辑；原手动 `pool.evict` 调用删除（registry 死条目自动重建）。具体迁移点以 rebase 检查清单第 1 项确认为准

### 步骤 4｜ai.rs / mcp.rs
- `AiHandler::new(stores, out, policy, registry: Arc<ConnRegistry>)`；`dispose()` 只撤审批
- mcp.rs `run_stdio_async` 自建 `ConnRegistry` 实例（独立进程，代码统一）

### 步骤 5｜session.rs 切换
- `SessionManager::new(stores, out, registry: Arc<ConnRegistry>)`；`SessionEntry` 增加 `host_id` 与 `pty: Option<PtyShell>`（stdin 任务改写 PtyShell）
- `open()`：组装 Interactive CreateCtx → `acquire_session`；shell → `conn.open_pty_shell()` + reader 任务；sftp/metrics 直接复用池连接
- `close`/`handle_closed`：`registry.release(host_id, session_id)` 取代 `conn.close()`；SFTP 惰性路径（shell 拖拽上传）改走 registry 的 sftp 缓存 + from_shared
- is_alive 看门狗保留（共享连接死亡 → 各持有者各自收到 session-closed）
- 顺手修复：`handle_hosts_op` 的 delete/update 增加 `registry.evict(host_id)`（主机改密/删除后旧连接不再残留）

### 步骤 6｜server.rs 装配
- `start_server` 创建一次 `Arc<ConnRegistry>` → 传入 `handle_conn` → 注入 SessionManager 与 AiHandler

## 边界情况

- **并发建连**：host_lock 串行化 + 锁内 double-check；TOFU 弹窗挂起时后来者在锁上等待（等用户，可接受）
- **TOFU/密码挂起中取消会话**：close() 先向 slot 发 false/None → connect 失败路径不得注册 holder；`acquire_session` 成功后会话已删（现有 L217 None 分支）必须 release
- **连接死亡**：shell 由 reader、sftp/metrics 由看门狗、AI 由操作报错各自 release；新请求经 is_alive 检查重建
- **idle 与持有者互斥**：到期 holders 非空跳过；release 后 holders 空才重启计时
- **同主机多 shell**：各自 PtyShell 通道，互不干扰（服务端 MaxSessions 默认 10 通道，够用）

## 验证

1. `cargo check` + `cargo clippy`（apps/desktop/src-tauri）；前端零改动无需构建
2. 手工场景：
   - 同主机开 shell+sftp+metrics 三会话 → 服务器 `ss -tn | grep :22` 仅一条 ESTABLISHED
   - 同主机两个 shell tab 独立交互、resize 互不影响
   - shell 打开时 AI 执行 exec_command/list_dir → 复用同连接（审计 connection.opened 仅一条）
   - 关全部会话 + 等 idle（临时调小 idle_close_sec）→ 审计出现 connection.closed
   - AI 访问未 TOFU 主机 → hostkey-unknown 提示；工作台连一次后 AI 可用
   - 断网/远端重启 → shell 报 connection-lost；AI 下次操作自动重连
   - 密码错误 → auth-retry 弹窗，取消后无 holder 泄漏（再开新会话正常）
   - 删除主机档案 → 池内连接被 evict
   - `--mcp-stdio` 子进程 MCP 调用正常
